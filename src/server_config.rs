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
];

/// `[anticheat]` keys: the three in the default file, then old ones that still work
/// (with a deprecation note from `anticheat::AcConfig::from_doc`).
const SERVER_CONFIG_ANTICHEAT_KEYS: &[&str] = &[
    "antixray",
    "antifullbright",
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

#[cfg(test)]
mod server_config_tests {
    use super::*;

    #[test]
    fn default_file_is_four_switches_with_one_line_comments() {
        let (doc, warnings) = server_config_parse(SERVER_CONFIG_DEFAULT_FILE);
        assert!(warnings.is_empty(), "{warnings:?}");
        assert!(server_config_unknown_keys(&doc).is_empty());
        let keys: Vec<&str> = doc["anticheat"].keys().map(String::as_str).collect();
        assert_eq!(keys, ["antifullbright", "antixray", "check_ops", "enabled"]);
        assert_eq!(doc["anticheat"]["enabled"], ServerConfigValue::Bool(true));
        assert_eq!(doc["anticheat"]["antixray"], ServerConfigValue::Bool(true));
        assert_eq!(doc["anticheat"]["antifullbright"], ServerConfigValue::Bool(true));
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
}
