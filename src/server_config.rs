// `server.toml`: the one optional settings file.
//
// The server runs without it. On first boot a short, commented file is written next to
// the world so an operator can see the few choices there are; every key in it is
// optional. Environment variables (`MC_SPINE_*`) keep working and win over the file, so
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
const SERVER_CONFIG_DEFAULT_FILE: &str = r#"# mc-spine settings. Every key is optional; delete this file to get it back.
# Environment variables (MC_SPINE_*) override anything set here.

[server]
# port = 25565
# motd = "mc-spine"
# max_players = 20

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
    ("server-port", "MC_SPINE_PORT"),
    ("server-ip", "MC_SPINE_BIND_ADDR"),
    ("motd", "MC_SPINE_MOTD"),
    ("max-players", "MC_SPINE_MAX_PLAYERS"),
    ("level-seed", "MC_SPINE_SEED"),
    ("network-compression-threshold", "MC_SPINE_COMPRESSION_THRESHOLD"),
    ("enforce-whitelist", "MC_SPINE_WHITELIST_ENABLED"),
    ("white-list", "MC_SPINE_WHITELIST_ENABLED"),
    ("difficulty", "MC_SPINE_DIFFICULTY"),
    ("gamemode", "MC_SPINE_DEFAULT_GAMEMODE"),
    ("view-distance", "MC_SPINE_VIEW_DISTANCE"),
    ("simulation-distance", "MC_SPINE_SIMULATION_DISTANCE"),
    ("rcon.port", "MC_SPINE_RCON_PORT"),
    ("rcon.password", "MC_SPINE_RCON_PASSWORD"),
    // The listener actually reads `MC_SPINE_RCON_PASS`; the properties layer feeds
    // both so a Vanilla file's `rcon.password` takes effect, not just parses.
    ("rcon.password", "MC_SPINE_RCON_PASS"),
    ("player-idle-timeout", "MC_SPINE_IDLE_TIMEOUT_SECS"),
    ("op-permission-level", "MC_SPINE_OP_PERMISSION_LEVEL"),
    ("pvp", "MC_SPINE_PVP"),
    ("allow-flight", "MC_SPINE_ALLOW_FLIGHT"),
    ("online-mode", "MC_SPINE_ONLINE_MODE"),
];

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
/// for the default `server.toml`, the custom file's directory under `MC_SPINE_CONFIG`).
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
/// `MC_SPINE_IDLE_TIMEOUT_SECS` is seconds); `op-permission-level` is clamped to
/// Vanilla's `1..=4` range. Empty values are skipped: Vanilla writes e.g.
/// `server-ip=` to mean "unset".
fn server_properties_env(
    props: &BTreeMap<String, String>,
    is_set: impl Fn(&str) -> bool,
) -> Vec<(&'static str, String)> {
    let mut out = Vec::new();
    let mut emitted = HashSet::new();
    for &(key, env) in SERVER_PROPERTIES_KEYS {
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

/// `[server]` keys and the environment variable each one stands for. The file only
/// fills a variable that is not already set, so the environment keeps priority.
const SERVER_CONFIG_KEYS: &[(&str, &str)] = &[
    ("port", "MC_SPINE_PORT"),
    ("bind_addr", "MC_SPINE_BIND_ADDR"),
    ("motd", "MC_SPINE_MOTD"),
    ("max_players", "MC_SPINE_MAX_PLAYERS"),
    ("seed", "MC_SPINE_SEED"),
    ("compression_threshold", "MC_SPINE_COMPRESSION_THRESHOLD"),
    ("allowlist", "MC_SPINE_ALLOWLIST"),
    ("idle_timeout_secs", "MC_SPINE_IDLE_TIMEOUT_SECS"),
    ("login_timeout_secs", "MC_SPINE_LOGIN_TIMEOUT_SECS"),
    ("backup_interval_secs", "MC_SPINE_BACKUP_INTERVAL_SECS"),
    ("max_conn_per_ip", "MC_SPINE_MAX_CONN_PER_IP"),
    ("resource_pack_url", "MC_SPINE_RESOURCE_PACK_URL"),
    ("resource_pack_required", "MC_SPINE_RESOURCE_PACK_REQUIRED"),
    ("rcon_port", "MC_SPINE_RCON_PORT"),
    // Reliability / ops / multi-version keys read by `RuntimeOpsConfig`. Keeping
    // them in the shared table means the boot export reaches the consumers that
    // already read the variable directly (`MC_SPINE_VERSION_DIR` in
    // block_registry, `MC_SPINE_MC_VERSION` in save_v2).
    ("online_mode", "MC_SPINE_ONLINE_MODE"),
    ("velocity_forwarding", "MC_SPINE_VELOCITY_FORWARDING"),
    ("velocity_forwarding_secret", "MC_SPINE_VELOCITY_SECRET"),
    ("autosave_interval_secs", "MC_SPINE_AUTOSAVE_SECS"),
    ("backup_max_keep", "MC_SPINE_BACKUP_MAX_KEEP"),
    ("lan_broadcast", "MC_SPINE_LAN_BROADCAST"),
    ("auto_load_balance", "MC_SPINE_AUTO_LOAD_BALANCE"),
    ("target_mc_version", "MC_SPINE_MC_VERSION"),
    ("version_pack_dir", "MC_SPINE_VERSION_DIR"),
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

/// The file in use: `MC_SPINE_CONFIG`, else `server.toml` in the working directory
/// (where the world files live).
fn server_config_path() -> std::path::PathBuf {
    std::env::var_os("MC_SPINE_CONFIG")
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
    }
    // The environment now carries every file layer, so resolving the ops knobs
    // sees the same values the env-reading consumers do. Reported once, like
    // the unknown-key warnings above.
    let ops = RuntimeOpsConfig::from_doc(&doc, |name| std::env::var(name).ok());
    for note in &ops.notes {
        eprintln!("[config] {note}");
    }
    let _ = RUNTIME_OPS.set(ops);
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

// ---------- production ops knobs ----------
//
// The typed view of the `[server]` keys the reliability, operator-UX and
// multi-version lanes read. Resolution is deliberately small: the file fills
// what the environment leaves unset and `MC_SPINE_*` always wins, exactly like
// `AcConfig::from_doc`. A dropped-in vanilla `server.properties` reaches the
// same variables through `server_config_boot` (`online-mode` is the only one
// of these that has a vanilla key).

/// The MC release the bundled data pack targets; `MC_SPINE_MC_VERSION` and the
/// `[server] target_mc_version` key move it together.
pub(crate) const RUNTIME_OPS_DEFAULT_MC_VERSION: &str = "1.21.1";
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
    /// `MC_SPINE_ONLINE_MODE`). Off keeps the loopback test behaviour: no
    /// encryption request, offline UUIDs.
    pub(crate) online_mode: bool,
    /// Velocity modern forwarding (`MC_SPINE_VELOCITY_FORWARDING`): the login
    /// handshake trusts the signed `velocity:player_info` payload instead of a
    /// Mojang session lookup. Meaningful only with a secret.
    pub(crate) velocity_forwarding: bool,
    /// Shared secret Velocity signs forwarding data with
    /// (`MC_SPINE_VELOCITY_SECRET`). Never printed: `Debug` and `summary()`
    /// show `[REDACTED]`; the raw value leaves the struct only through
    /// `velocity_forwarding_secret()`.
    velocity_forwarding_secret: Option<String>,
    /// Autosave cadence for the background save worker
    /// (`MC_SPINE_AUTOSAVE_SECS`), clamped to
    /// `AUTOSAVE_SECS_MIN..=AUTOSAVE_SECS_MAX`.
    pub(crate) autosave_interval_secs: u64,
    /// Generations `rotate_world_backups` keeps (`MC_SPINE_BACKUP_MAX_KEEP`).
    pub(crate) backup_max_keep: usize,
    /// Vanilla-style LAN broadcast announcements (`MC_SPINE_LAN_BROADCAST`) so
    /// the server appears in the client multiplayer list.
    pub(crate) lan_broadcast: bool,
    /// `AutoLoadBalancer` tick smoothing (`MC_SPINE_AUTO_LOAD_BALANCE`). On
    /// unless an operator explicitly turns it off.
    pub(crate) auto_load_balance: bool,
    /// The MC release the active version pack describes
    /// (`MC_SPINE_MC_VERSION`).
    pub(crate) target_mc_version: String,
    /// Explicit version-pack directory (`MC_SPINE_VERSION_DIR`); unset means
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
            target_mc_version: RUNTIME_OPS_DEFAULT_MC_VERSION.to_owned(),
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
            .field("target_mc_version", &self.target_mc_version)
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

impl RuntimeOpsConfig {
    /// `[server]` from the parsed file, then the environment on top:
    /// `MC_SPINE_ONLINE_MODE`, `MC_SPINE_VELOCITY_FORWARDING`,
    /// `MC_SPINE_VELOCITY_SECRET`, `MC_SPINE_AUTOSAVE_SECS`,
    /// `MC_SPINE_BACKUP_MAX_KEEP`, `MC_SPINE_LAN_BROADCAST`,
    /// `MC_SPINE_AUTO_LOAD_BALANCE`, `MC_SPINE_MC_VERSION`,
    /// `MC_SPINE_VERSION_DIR`.
    pub(crate) fn from_doc(
        doc: &ServerConfigDoc,
        env: impl Fn(&str) -> Option<String>,
    ) -> RuntimeOpsConfig {
        let table = doc.get("server");
        let get = |k: &str| table.and_then(|t| t.get(k));
        let mut notes = Vec::new();
        let mut c = RuntimeOpsConfig {
            online_mode: ops_bool(&mut notes, "online_mode", get("online_mode"), env("MC_SPINE_ONLINE_MODE"), false),
            velocity_forwarding: ops_bool(&mut notes, "velocity_forwarding", get("velocity_forwarding"), env("MC_SPINE_VELOCITY_FORWARDING"), false),
            velocity_forwarding_secret: ops_str(&mut notes, "velocity_forwarding_secret", get("velocity_forwarding_secret"), env("MC_SPINE_VELOCITY_SECRET")),
            autosave_interval_secs: {
                let raw = ops_u64(&mut notes, "autosave_interval_secs", get("autosave_interval_secs"), env("MC_SPINE_AUTOSAVE_SECS"), AUTOSAVE_SECS_DEFAULT);
                let clamped = raw.clamp(AUTOSAVE_SECS_MIN, AUTOSAVE_SECS_MAX);
                if raw != clamped {
                    notes.push(format!(
                        "autosave_interval_secs {raw} outside {AUTOSAVE_SECS_MIN}..={AUTOSAVE_SECS_MAX}; clamped to {clamped}"
                    ));
                }
                clamped
            },
            backup_max_keep: {
                let raw = ops_u64(&mut notes, "backup_max_keep", get("backup_max_keep"), env("MC_SPINE_BACKUP_MAX_KEEP"), BACKUP_MAX_KEEP_DEFAULT as u64);
                usize::try_from(raw).unwrap_or_else(|_| {
                    notes.push(format!("backup_max_keep {raw} does not fit; using {BACKUP_MAX_KEEP_DEFAULT}"));
                    BACKUP_MAX_KEEP_DEFAULT
                })
            },
            lan_broadcast: ops_bool(&mut notes, "lan_broadcast", get("lan_broadcast"), env("MC_SPINE_LAN_BROADCAST"), false),
            auto_load_balance: ops_bool(&mut notes, "auto_load_balance", get("auto_load_balance"), env("MC_SPINE_AUTO_LOAD_BALANCE"), true),
            target_mc_version: ops_str(&mut notes, "target_mc_version", get("target_mc_version"), env("MC_SPINE_MC_VERSION"))
                .unwrap_or_else(|| RUNTIME_OPS_DEFAULT_MC_VERSION.to_owned()),
            version_pack_dir: ops_str(&mut notes, "version_pack_dir", get("version_pack_dir"), env("MC_SPINE_VERSION_DIR")),
            notes: Vec::new(),
        };
        if c.velocity_forwarding && c.velocity_forwarding_secret.is_none() {
            notes.push(
                "velocity_forwarding is on but no secret is set (velocity_forwarding_secret / MC_SPINE_VELOCITY_SECRET); forwarding data cannot be verified"
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
    #[allow(dead_code)]
    pub(crate) fn summary(&self) -> String {
        format!(
            "ops: online_mode={} velocity_forwarding={} (secret {}) autosave={}s backups={} lan={} load_balance={} mc_version={} pack_dir={}",
            self.online_mode,
            self.velocity_forwarding,
            if self.velocity_forwarding_secret.is_some() { "[REDACTED]" } else { "unset" },
            self.autosave_interval_secs,
            self.backup_max_keep,
            self.lan_broadcast,
            self.auto_load_balance,
            self.target_mc_version,
            self.version_pack_dir.as_deref().unwrap_or("<bundled>"),
        )
    }
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
        let env = server_config_server_env(&doc, |name| name == "MC_SPINE_PORT");
        assert_eq!(
            env,
            vec![
                ("MC_SPINE_MOTD", "hi".to_owned()),
                ("MC_SPINE_ALLOWLIST", "A,B".to_owned())
            ]
        );
    }

    #[test]
    fn server_properties_parses_vanilla_keys_and_respects_env_and_toml_precedence() {
        let props = server_properties_parse(
            "# vanilla comment\n\
             ! also a comment\n\
             server-port=25599\n\
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
        assert_eq!(props["server-port"], "25599");
        assert_eq!(props["motd"], "A: escaped server\nline two");
        assert_eq!(props["max-players"], "5");
        assert_eq!(props["escaped=key"], "v");
        assert_eq!(props["unicode"], "é");
        // Empty values parse but are never exported: Vanilla writes `server-ip=` for unset.
        assert_eq!(props["server-ip"], "");

        // `is_set` stands for "a higher layer already decided this variable":
        // the environment wins over everything, `server.toml` over the properties.
        let decided: HashSet<&str> = ["MC_SPINE_PORT", "MC_SPINE_MOTD"].into_iter().collect();
        let env = server_properties_env(&props, |name| decided.contains(name));
        let got: BTreeMap<&str, String> = env.into_iter().collect();
        assert!(!got.contains_key("MC_SPINE_PORT")); // server.toml won
        assert!(!got.contains_key("MC_SPINE_MOTD")); // environment won
        assert!(!got.contains_key("MC_SPINE_BIND_ADDR")); // `server-ip=` skipped
        assert_eq!(got["MC_SPINE_MAX_PLAYERS"], "5");
        assert_eq!(got["MC_SPINE_IDLE_TIMEOUT_SECS"], "600"); // minutes × 60
        assert_eq!(got["MC_SPINE_OP_PERMISSION_LEVEL"], "4"); // clamped to 1..=4
        // `enforce-whitelist` wins over `white-list` for the shared variable.
        assert_eq!(got["MC_SPINE_WHITELIST_ENABLED"], "false");
    }

    #[test]
    fn missing_file_is_created_once_and_never_overwritten() {
        // Under the build tree, not /tmp.
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join(format!("mc-spine-cfg-{}", std::process::id()));
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
        assert_eq!(c.target_mc_version, "1.21.1");
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
             target_mc_version = \"1.21.4\"\n\
             version_pack_dir = \"packs/1214\"\n",
        );
        assert!(warnings.is_empty(), "{warnings:?}");
        assert!(server_config_unknown_keys(&doc).is_empty());
        let env = |name: &str| match name {
            "MC_SPINE_ONLINE_MODE" => Some("0".to_owned()),
            "MC_SPINE_AUTOSAVE_SECS" => Some("45".to_owned()),
            "MC_SPINE_BACKUP_MAX_KEEP" => Some("8".to_owned()),
            "MC_SPINE_MC_VERSION" => Some("1.21.5".to_owned()),
            _ => None,
        };
        let c = RuntimeOpsConfig::from_doc(&doc, env);
        // The environment wins over the file on every knob it sets.
        assert!(!c.online_mode);
        assert_eq!(c.autosave_interval_secs, 45);
        assert_eq!(c.backup_max_keep, 8);
        assert_eq!(c.target_mc_version, "1.21.5");
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
        // `MC_SPINE_*` variable its env-reading consumers already watch.
        let (doc, _) = server_config_parse(
            "[server]\nonline_mode = true\ntarget_mc_version = \"1.21.4\"\nversion_pack_dir = \"p\"\n",
        );
        let env = server_config_server_env(&doc, |name| name == "MC_SPINE_ONLINE_MODE");
        let got: BTreeMap<&str, String> = env.into_iter().collect();
        assert!(!got.contains_key("MC_SPINE_ONLINE_MODE")); // env already won
        assert_eq!(got["MC_SPINE_MC_VERSION"], "1.21.4");
        assert_eq!(got["MC_SPINE_VERSION_DIR"], "p");
        // And resolving the file sees the value the env layer would have set.
        let c = RuntimeOpsConfig::from_doc(&doc, |_| None);
        assert!(c.online_mode);
    }

    #[test]
    fn online_mode_maps_from_vanilla_server_properties() {
        let props = server_properties_parse("online-mode=true\nmotd=x\n");
        let env = server_properties_env(&props, |_| false);
        let got: BTreeMap<&str, String> = env.into_iter().collect();
        assert_eq!(got["MC_SPINE_ONLINE_MODE"], "true");
        // A higher layer that already decided the variable keeps it.
        let env = server_properties_env(&props, |n| n == "MC_SPINE_ONLINE_MODE");
        assert!(env.iter().all(|(name, _)| *name != "MC_SPINE_ONLINE_MODE"));
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
            "MC_SPINE_AUTOSAVE_SECS" => Some("notanum".to_owned()),
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
            "MC_SPINE_ONLINE_MODE" => Some("TRUE".to_owned()),
            "MC_SPINE_LAN_BROADCAST" => Some(" yes ".to_owned()),
            "MC_SPINE_AUTO_LOAD_BALANCE" => Some("".to_owned()),
            _ => None,
        };
        let c = RuntimeOpsConfig::from_doc(&doc, env);
        assert!(c.online_mode);
        assert!(c.lan_broadcast);
        // An empty env var is unset, so the default still applies.
        assert!(c.auto_load_balance);
        assert!(c.notes.is_empty(), "{:?}", c.notes);
    }
}
