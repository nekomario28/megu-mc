// Vanilla server-operator state: `ops.json` permission levels 0-4, persistent
// player/IP bans and the whitelist, the login-admission check that reads them, the
// admin command surface (`/ban` … `/whitelist`), and the interactive stdin console.
//
// Everything here is data plus std I/O so wiring it into `commands_core` /
// `play_protocol` stays a drop-in: the dispatcher returns the reply text and the
// player names to kick, and the console loop runs on any `BufRead`. The JSON files
// keep the exact Vanilla schemas so saves are interchangeable with a real server.

/// `banned-players.json` / `banned-ips.json` / `whitelist.json` file names
/// (`ops.json` is `OPS_FILE` in `commands_core`, which names the same file).
#[allow(dead_code)]
pub(crate) const BANNED_PLAYERS_FILE: &str = "banned-players.json";
#[allow(dead_code)]
pub(crate) const BANNED_IPS_FILE: &str = "banned-ips.json";
#[allow(dead_code)]
pub(crate) const WHITELIST_FILE: &str = "whitelist.json";

/// Temp-file-then-rename write, same discipline as `set_operator`: a crash leaves
/// the old or the new file, never a truncated one.
#[allow(dead_code)]
fn ops_json_write_atomic(path: &std::path::Path, body: &str) -> std::io::Result<()> {
    static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);
    let temp = path.with_extension(format!(
        "json.{}.{}.tmp",
        std::process::id(),
        NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
    ));
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temp)?;
    let written = file
        .write_all(body.as_bytes())
        .and_then(|_| file.sync_all());
    drop(file);
    if let Err(error) = written.and_then(|_| std::fs::rename(&temp, path)) {
        let _ = std::fs::remove_file(&temp);
        return Err(error);
    }
    Ok(())
}

/// A string field of a parsed JSON object; non-string values do not pass for one.
#[allow(dead_code)]
fn ops_json_str(j: &J, key: &str) -> Option<String> {
    match json::js(j, key) {
        Some(J::Str(s)) => Some(s.clone()),
        _ => None,
    }
}

#[allow(dead_code)]
fn ops_json_invalid(path: &std::path::Path) -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        format!("could not parse {}", path.display()),
    )
}

/// The offline-mode UUID `login_identity` derives (`OfflinePlayer:` + name, MD5
/// with version-3/variant bits), rendered dashed the way the Vanilla files write it.
#[allow(dead_code)]
fn offline_uuid_string(name: &str) -> String {
    let mut uuid = generation_math::md5(format!("OfflinePlayer:{name}").as_bytes());
    uuid[6] = (uuid[6] & 0x0f) | 0x30;
    uuid[8] = (uuid[8] & 0x3f) | 0x80;
    let hex: String = uuid.iter().map(|b| format!("{b:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..]
    )
}

// ------------------------------------------------------------------ ops.json

/// One Vanilla `ops.json` object.
#[derive(Clone, Debug, PartialEq)]
#[allow(dead_code)]
pub(crate) struct OpEntry {
    pub uuid: String,
    pub name: String,
    /// Vanilla permission level, clamped to `0..=4` on read and on write.
    pub level: u8,
    pub bypasses_player_limit: bool,
}

/// The operator list: Vanilla `ops.json` objects, plus the legacy flat name array
/// (`["alice","bob"]`) which reads as level-4 operators like the old schema meant.
#[derive(Clone, Default)]
#[allow(dead_code)]
pub(crate) struct OpsRegistry {
    pub entries: Vec<OpEntry>,
}

#[allow(dead_code)]
impl OpsRegistry {
    /// Missing file is an empty registry; malformed JSON is an error for the caller
    /// to report (a corrupt operator file must not silently de-op everyone).
    pub(crate) fn load(path: &std::path::Path) -> std::io::Result<Self> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(e) => return Err(e),
        };
        ops_registry_parse(&text).ok_or_else(|| ops_json_invalid(path))
    }

    pub(crate) fn save(&self, path: &std::path::Path) -> std::io::Result<()> {
        ops_json_write_atomic(path, &ops_registry_json(self))
    }

    fn find(&self, name: &str) -> Option<&OpEntry> {
        self.entries
            .iter()
            .find(|e| e.name.eq_ignore_ascii_case(name))
    }

    /// `0` when the name is not an operator, else its `1..=4` level.
    pub(crate) fn op_level_of(&self, name: &str) -> u8 {
        self.find(name).map(|e| e.level).unwrap_or(0)
    }

    /// Presence in the file — even a level-0 operator counts, as Vanilla's
    /// `opList.contains(profile)` does for the whitelist bypass.
    pub(crate) fn is_op(&self, name: &str) -> bool {
        self.find(name).is_some()
    }

    pub(crate) fn bypasses_player_limit(&self, name: &str) -> bool {
        self.find(name)
            .map(|e| e.bypasses_player_limit)
            .unwrap_or(false)
    }

    /// Insert or update an operator; `true` when anything changed.
    pub(crate) fn set(&mut self, name: &str, level: u8, bypasses_player_limit: bool) -> bool {
        let level = level.min(4);
        if let Some(entry) = self
            .entries
            .iter_mut()
            .find(|e| e.name.eq_ignore_ascii_case(name))
        {
            let changed =
                entry.level != level || entry.bypasses_player_limit != bypasses_player_limit;
            entry.level = level;
            entry.bypasses_player_limit = bypasses_player_limit;
            return changed;
        }
        self.entries.push(OpEntry {
            uuid: offline_uuid_string(name),
            name: name.to_owned(),
            level,
            bypasses_player_limit,
        });
        true
    }

    /// `true` when an operator was removed.
    pub(crate) fn remove(&mut self, name: &str) -> bool {
        let before = self.entries.len();
        self.entries
            .retain(|e| !e.name.eq_ignore_ascii_case(name));
        self.entries.len() != before
    }

    pub(crate) fn names(&self) -> Vec<&str> {
        self.entries.iter().map(|e| e.name.as_str()).collect()
    }
}

#[allow(dead_code)]
fn ops_registry_parse(text: &str) -> Option<OpsRegistry> {
    let J::Arr(items) = jparse(text.as_bytes())? else {
        return None;
    };
    let mut entries = Vec::new();
    for item in items {
        match item {
            // Legacy flat name array: old `ops.json` meant "operator", which maps to
            // Vanilla's default `op-permission-level` of 4.
            J::Str(name) if valid_player_name(&name) => entries.push(OpEntry {
                uuid: offline_uuid_string(&name),
                name,
                level: 4,
                bypasses_player_limit: false,
            }),
            J::Str(_) => return None,
            obj => {
                let name = ops_json_str(&obj, "name")?;
                entries.push(OpEntry {
                    uuid: ops_json_str(&obj, "uuid")
                        .unwrap_or_else(|| offline_uuid_string(&name)),
                    name,
                    level: json::jint(&obj, "level", 4).clamp(0, 4) as u8,
                    bypasses_player_limit: json::jbool(&obj, "bypassesPlayerLimit", false),
                });
            }
        }
    }
    Some(OpsRegistry { entries })
}

#[allow(dead_code)]
fn ops_registry_json(registry: &OpsRegistry) -> String {
    let mut out = String::from("[\n");
    for (i, e) in registry.entries.iter().enumerate() {
        out.push_str("  {\n");
        out.push_str(&format!("    \"uuid\": {},\n", json_text(&e.uuid)));
        out.push_str(&format!("    \"name\": {},\n", json_text(&e.name)));
        out.push_str(&format!("    \"level\": {},\n", e.level));
        out.push_str(&format!(
            "    \"bypassesPlayerLimit\": {}\n",
            e.bypasses_player_limit
        ));
        out.push_str(if i + 1 == registry.entries.len() {
            "  }\n"
        } else {
            "  },\n"
        });
    }
    out.push_str("]\n");
    out
}

// --------------------------------------------------------- permission levels

/// Vanilla 1.21.1 permission levels (the `hasPermission` gate on each literal in
/// `Commands`): `0` everyone, `2` game-master (cheats), `3` moderator, `4` owner.
/// Unlisted commands default to `2` — Vanilla's most common gate, and the safe
/// level for a command this server adds before it is mapped here.
#[allow(dead_code)]
pub(crate) fn required_permission_level(cmd: &str) -> u8 {
    match cmd {
        "help" | "list" | "me" | "msg" | "tell" | "w" | "teammsg" | "tm" | "trigger" => 0,
        "kick" | "ban" | "ban-ip" | "banlist" | "pardon" | "pardon-ip" | "op" | "deop"
        | "whitelist" | "setidletimeout" | "tick" => 3,
        "stop" | "save-all" | "save" | "save-on" | "save-off" | "debug" | "jfr" | "perf"
        | "publish" | "reload" => 4,
        _ => 2,
    }
}

/// `true` when a source at `level` may run `cmd` under the Vanilla levels above.
#[allow(dead_code)]
pub(crate) fn command_allowed_at_level(cmd: &str, level: u8) -> bool {
    level >= required_permission_level(cmd)
}

// --------------------------------------------------------------- ban files

/// One `banned-players.json` object.
#[derive(Clone, Debug, PartialEq)]
#[allow(dead_code)]
pub(crate) struct BanEntry {
    pub uuid: String,
    pub name: String,
    pub created: String,
    pub source: String,
    /// `"forever"` or an ISO date, as Vanilla writes it.
    pub expires: String,
    pub reason: String,
}

/// One `banned-ips.json` object.
#[derive(Clone, Debug, PartialEq)]
#[allow(dead_code)]
pub(crate) struct IpBanEntry {
    pub ip: String,
    pub created: String,
    pub source: String,
    pub expires: String,
    pub reason: String,
}

/// The two persistent ban files together; `load`/`save` take the directory both
/// Vanilla-named files live in (the server working directory).
#[derive(Clone, Default)]
#[allow(dead_code)]
pub(crate) struct BanRegistry {
    pub players: Vec<BanEntry>,
    pub ips: Vec<IpBanEntry>,
}

/// Canonical IP form for comparison: an `IpAddr` round-trip collapses IPv6
/// spelling and leading zeros; a stray leading `/` (Java `InetAddress` form) is
/// dropped; anything else compares case-insensitively, covering Vanilla hostname
/// bans.
#[allow(dead_code)]
fn normalize_ban_ip(ip: &str) -> String {
    let ip = ip.trim().trim_start_matches('/');
    ip.parse::<std::net::IpAddr>()
        .map(|a| a.to_string())
        .unwrap_or_else(|_| ip.to_ascii_lowercase())
}

#[allow(dead_code)]
impl BanRegistry {
    pub(crate) fn load(dir: &std::path::Path) -> std::io::Result<Self> {
        let players_path = dir.join(BANNED_PLAYERS_FILE);
        let ips_path = dir.join(BANNED_IPS_FILE);
        Ok(Self {
            players: match std::fs::read_to_string(&players_path) {
                Ok(text) => {
                    ban_players_parse(&text).ok_or_else(|| ops_json_invalid(&players_path))?
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
                Err(e) => return Err(e),
            },
            ips: match std::fs::read_to_string(&ips_path) {
                Ok(text) => {
                    ban_ips_parse(&text).ok_or_else(|| ops_json_invalid(&ips_path))?
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
                Err(e) => return Err(e),
            },
        })
    }

    /// Both files, each atomic; a failure reports after the write that failed.
    pub(crate) fn save(&self, dir: &std::path::Path) -> std::io::Result<()> {
        ops_json_write_atomic(&dir.join(BANNED_PLAYERS_FILE), &ban_players_json(self))?;
        ops_json_write_atomic(&dir.join(BANNED_IPS_FILE), &ban_ips_json(self))
    }

    /// Player-name lookup is case-insensitive, like the login reservation.
    pub(crate) fn player_ban(&self, name: &str) -> Option<&BanEntry> {
        self.players
            .iter()
            .find(|e| e.name.eq_ignore_ascii_case(name))
    }

    pub(crate) fn ip_ban(&self, ip: &str) -> Option<&IpBanEntry> {
        let ip = normalize_ban_ip(ip);
        self.ips.iter().find(|e| e.ip == ip)
    }

    /// `true` when the ban was added (`false` = already banned).
    pub(crate) fn ban_player(&mut self, name: &str, reason: &str, source: &str, now: &str) -> bool {
        if self.player_ban(name).is_some() {
            return false;
        }
        self.players.push(BanEntry {
            uuid: offline_uuid_string(name),
            name: name.to_owned(),
            created: now.to_owned(),
            source: source.to_owned(),
            expires: "forever".to_owned(),
            reason: reason.to_owned(),
        });
        true
    }

    /// `true` when an entry was removed.
    pub(crate) fn pardon_player(&mut self, name: &str) -> bool {
        let before = self.players.len();
        self.players
            .retain(|e| !e.name.eq_ignore_ascii_case(name));
        self.players.len() != before
    }

    pub(crate) fn ban_ip(&mut self, ip: &str, reason: &str, source: &str, now: &str) -> bool {
        let ip = normalize_ban_ip(ip);
        if self.ip_ban(&ip).is_some() {
            return false;
        }
        self.ips.push(IpBanEntry {
            ip,
            created: now.to_owned(),
            source: source.to_owned(),
            expires: "forever".to_owned(),
            reason: reason.to_owned(),
        });
        true
    }

    pub(crate) fn pardon_ip(&mut self, ip: &str) -> bool {
        let ip = normalize_ban_ip(ip);
        let before = self.ips.len();
        self.ips.retain(|e| e.ip != ip);
        self.ips.len() != before
    }
}

#[allow(dead_code)]
fn ban_players_parse(text: &str) -> Option<Vec<BanEntry>> {
    let J::Arr(items) = jparse(text.as_bytes())? else {
        return None;
    };
    items
        .iter()
        .map(|item| {
            Some(BanEntry {
                uuid: ops_json_str(item, "uuid").unwrap_or_default(),
                name: ops_json_str(item, "name")?,
                created: ops_json_str(item, "created").unwrap_or_default(),
                source: ops_json_str(item, "source").unwrap_or_else(|| "Server".into()),
                expires: ops_json_str(item, "expires").unwrap_or_else(|| "forever".into()),
                reason: ops_json_str(item, "reason").unwrap_or_default(),
            })
        })
        .collect()
}

#[allow(dead_code)]
fn ban_ips_parse(text: &str) -> Option<Vec<IpBanEntry>> {
    let J::Arr(items) = jparse(text.as_bytes())? else {
        return None;
    };
    items
        .iter()
        .map(|item| {
            Some(IpBanEntry {
                ip: ops_json_str(item, "ip").map(|ip| normalize_ban_ip(&ip))?,
                created: ops_json_str(item, "created").unwrap_or_default(),
                source: ops_json_str(item, "source").unwrap_or_else(|| "Server".into()),
                expires: ops_json_str(item, "expires").unwrap_or_else(|| "forever".into()),
                reason: ops_json_str(item, "reason").unwrap_or_default(),
            })
        })
        .collect()
}

#[allow(dead_code)]
fn ban_entry_json(e: &BanEntry) -> String {
    format!(
        "  {{\n    \"uuid\": {},\n    \"name\": {},\n    \"created\": {},\n    \"source\": {},\n    \"expires\": {},\n    \"reason\": {}\n  }}",
        json_text(&e.uuid),
        json_text(&e.name),
        json_text(&e.created),
        json_text(&e.source),
        json_text(&e.expires),
        json_text(&e.reason)
    )
}

#[allow(dead_code)]
fn ban_players_json(registry: &BanRegistry) -> String {
    let mut out = String::from("[\n");
    for (i, e) in registry.players.iter().enumerate() {
        out.push_str(&ban_entry_json(e));
        out.push_str(if i + 1 == registry.players.len() {
            "\n"
        } else {
            ",\n"
        });
    }
    out.push_str("]\n");
    out
}

#[allow(dead_code)]
fn ban_ips_json(registry: &BanRegistry) -> String {
    let mut out = String::from("[\n");
    for (i, e) in registry.ips.iter().enumerate() {
        out.push_str(&format!(
            "  {{\n    \"ip\": {},\n    \"created\": {},\n    \"source\": {},\n    \"expires\": {},\n    \"reason\": {}\n  }}",
            json_text(&e.ip),
            json_text(&e.created),
            json_text(&e.source),
            json_text(&e.expires),
            json_text(&e.reason)
        ));
        out.push_str(if i + 1 == registry.ips.len() { "\n" } else { ",\n" });
    }
    out.push_str("]\n");
    out
}

// ---------------------------------------------------------------- whitelist

/// One `whitelist.json` object.
#[derive(Clone, Debug, PartialEq)]
#[allow(dead_code)]
pub(crate) struct WhitelistEntry {
    pub uuid: String,
    pub name: String,
}

/// The whitelist file plus its enforcement switch. `enabled` is run time only —
/// Vanilla keeps it in `server.properties` (`white-list=`), not in the JSON file,
/// so the file schema stays exactly `[{"uuid":…,"name":…}]`.
#[derive(Clone, Default)]
#[allow(dead_code)]
pub(crate) struct WhitelistRegistry {
    pub enabled: bool,
    pub entries: Vec<WhitelistEntry>,
}

#[allow(dead_code)]
impl WhitelistRegistry {
    pub(crate) fn load(path: &std::path::Path) -> std::io::Result<Self> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(e) => return Err(e),
        };
        whitelist_parse(&text)
            .map(|entries| Self {
                enabled: false,
                entries,
            })
            .ok_or_else(|| ops_json_invalid(path))
    }

    pub(crate) fn save(&self, path: &std::path::Path) -> std::io::Result<()> {
        ops_json_write_atomic(path, &whitelist_json(self))
    }

    pub(crate) fn contains(&self, name: &str) -> bool {
        self.entries
            .iter()
            .any(|e| e.name.eq_ignore_ascii_case(name))
    }

    /// `true` when the player was added (`false` = already whitelisted).
    pub(crate) fn add(&mut self, name: &str) -> bool {
        if self.contains(name) {
            return false;
        }
        self.entries.push(WhitelistEntry {
            uuid: offline_uuid_string(name),
            name: name.to_owned(),
        });
        true
    }

    pub(crate) fn remove(&mut self, name: &str) -> bool {
        let before = self.entries.len();
        self.entries
            .retain(|e| !e.name.eq_ignore_ascii_case(name));
        self.entries.len() != before
    }

    pub(crate) fn names(&self) -> Vec<&str> {
        self.entries.iter().map(|e| e.name.as_str()).collect()
    }
}

#[allow(dead_code)]
fn whitelist_parse(text: &str) -> Option<Vec<WhitelistEntry>> {
    let J::Arr(items) = jparse(text.as_bytes())? else {
        return None;
    };
    items
        .iter()
        .map(|item| {
            Some(WhitelistEntry {
                uuid: ops_json_str(item, "uuid").unwrap_or_default(),
                name: ops_json_str(item, "name")?,
            })
        })
        .collect()
}

#[allow(dead_code)]
fn whitelist_json(registry: &WhitelistRegistry) -> String {
    let mut out = String::from("[\n");
    for (i, e) in registry.entries.iter().enumerate() {
        out.push_str(&format!(
            "  {{\n    \"uuid\": {},\n    \"name\": {}\n  }}",
            json_text(&e.uuid),
            json_text(&e.name)
        ));
        out.push_str(if i + 1 == registry.entries.len() {
            "\n"
        } else {
            ",\n"
        });
    }
    out.push_str("]\n");
    out
}

// ------------------------------------------------------------ login admission

/// Vanilla `PlayerList.canPlayerLogin` order — IP ban first, then the player ban,
/// the whitelist, and the player cap last — with Vanilla's disconnect texts.
#[allow(dead_code)]
pub(crate) fn check_login_admission(
    name: &str,
    ip: &str,
    online_count: usize,
    max_players: usize,
    ops: &OpsRegistry,
    bans: &BanRegistry,
    whitelist: &WhitelistRegistry,
) -> Result<(), String> {
    if let Some(ban) = bans.ip_ban(ip) {
        return Err(format!(
            "Your IP address is banned from this server.\nReason: {}",
            ban.reason
        ));
    }
    if let Some(ban) = bans.player_ban(name) {
        return Err(format!(
            "You are banned from this server.\nReason: {}",
            ban.reason
        ));
    }
    if whitelist.enabled && !whitelist.contains(name) && !ops.is_op(name) {
        return Err("You are not white-listed on this server!".to_owned());
    }
    if online_count >= max_players && !ops.bypasses_player_limit(name) {
        return Err("The server is full!".to_owned());
    }
    Ok(())
}

// ------------------------------------------------------------ admin commands

/// Run one `/ban`, `/ban-ip`, `/banlist`, `/pardon`, `/pardon-ip` or `/whitelist`
/// line against the registries, persisting changes under `dir` (the directory the
/// Vanilla-named files live in). Returns `(reply, kick_targets)`: the reply text
/// for the source and the names of players the caller must disconnect — a freshly
/// banned player is dropped right away. IP bans return no names because the caller
/// owns the connection table and knows which online players match. `source` and
/// `now` are the `source`/`created` fields written on new entries.
#[allow(dead_code)]
pub(crate) fn exec_server_ops_command(
    input: &str,
    bans: &mut BanRegistry,
    whitelist: &mut WhitelistRegistry,
    dir: &std::path::Path,
    source: &str,
    now: &str,
) -> (String, Vec<String>) {
    let input = input.trim().trim_start_matches('/');
    let mut words = input.split_whitespace();
    let Some(cmd) = words.next() else {
        return (String::new(), Vec::new());
    };
    let args: Vec<&str> = words.collect();
    let reason_of = |args: &[&str]| {
        let reason = args.join(" ");
        if reason.is_empty() {
            "Banned by an operator.".to_owned()
        } else {
            reason
        }
    };
    match cmd {
        "ban" => {
            let Some(&name) = args.first() else {
                return ("usage: /ban <target> [reason]".into(), Vec::new());
            };
            if !valid_player_name(name) {
                return (format!("Invalid player name '{name}'"), Vec::new());
            }
            if bans.player_ban(name).is_some() {
                return ("Nothing changed. The player is already banned".into(), Vec::new());
            }
            let reason = reason_of(&args[1..]);
            bans.ban_player(name, &reason, source, now);
            match bans.save(dir) {
                Ok(()) => (format!("Banned {name}: {reason}"), vec![name.to_owned()]),
                Err(e) => (format!("Could not save {BANNED_PLAYERS_FILE}: {e}"), Vec::new()),
            }
        }
        "ban-ip" => {
            let Some(&target) = args.first() else {
                return ("usage: /ban-ip <ip> [reason]".into(), Vec::new());
            };
            let Ok(addr) = target.parse::<std::net::IpAddr>() else {
                return (format!("Invalid IP address '{target}'"), Vec::new());
            };
            let ip = addr.to_string();
            if bans.ip_ban(&ip).is_some() {
                return (
                    "Nothing changed. That IP address is already banned".into(),
                    Vec::new(),
                );
            }
            let reason = reason_of(&args[1..]);
            bans.ban_ip(&ip, &reason, source, now);
            match bans.save(dir) {
                Ok(()) => (format!("Banned IP address {ip}: {reason}"), Vec::new()),
                Err(e) => (format!("Could not save {BANNED_IPS_FILE}: {e}"), Vec::new()),
            }
        }
        "banlist" => {
            let (kind, lines): (&str, Vec<String>) = match args.first().copied() {
                Some("ips") => (
                    "IP bans",
                    bans.ips
                        .iter()
                        .map(|e| format!("{} banned by {}: {}", e.ip, e.source, e.reason))
                        .collect(),
                ),
                Some("players") | None => (
                    "bans",
                    bans.players
                        .iter()
                        .map(|e| format!("{} banned by {}: {}", e.name, e.source, e.reason))
                        .collect(),
                ),
                _ => return ("usage: /banlist [ips|players]".into(), Vec::new()),
            };
            let reply = if lines.is_empty() {
                format!("There are no {kind}")
            } else {
                format!("There are {} {}:\n{}", lines.len(), kind, lines.join("\n"))
            };
            (reply, Vec::new())
        }
        "pardon" => {
            let Some(&name) = args.first() else {
                return ("usage: /pardon <target>".into(), Vec::new());
            };
            if !bans.pardon_player(name) {
                return ("Nothing changed. The player is not banned".into(), Vec::new());
            }
            match bans.save(dir) {
                Ok(()) => (format!("Unbanned {name}"), Vec::new()),
                Err(e) => (format!("Could not save {BANNED_PLAYERS_FILE}: {e}"), Vec::new()),
            }
        }
        "pardon-ip" => {
            let Some(&ip) = args.first() else {
                return ("usage: /pardon-ip <ip>".into(), Vec::new());
            };
            if !bans.pardon_ip(ip) {
                return ("Nothing changed. That IP is not banned".into(), Vec::new());
            }
            match bans.save(dir) {
                Ok(()) => (format!("Unbanned IP address {}", normalize_ban_ip(ip)), Vec::new()),
                Err(e) => (format!("Could not save {BANNED_IPS_FILE}: {e}"), Vec::new()),
            }
        }
        "whitelist" => match args.first().copied() {
            Some("on") => {
                if whitelist.enabled {
                    ("Whitelist is already turned on".into(), Vec::new())
                } else {
                    whitelist.enabled = true;
                    ("Whitelist is now turned on".into(), Vec::new())
                }
            }
            Some("off") => {
                if whitelist.enabled {
                    whitelist.enabled = false;
                    ("Whitelist is now turned off".into(), Vec::new())
                } else {
                    ("Whitelist is already turned off".into(), Vec::new())
                }
            }
            Some("list") => {
                let names = whitelist.names();
                if names.is_empty() {
                    ("There are no whitelisted players".into(), Vec::new())
                } else {
                    (
                        format!(
                            "There are {} whitelisted players: {}",
                            names.len(),
                            names.join(", ")
                        ),
                        Vec::new(),
                    )
                }
            }
            Some("add") => {
                let Some(&name) = args.get(1) else {
                    return ("usage: /whitelist add <player>".into(), Vec::new());
                };
                if !valid_player_name(name) {
                    return (format!("Invalid player name '{name}'"), Vec::new());
                }
                if !whitelist.add(name) {
                    return ("Player is already whitelisted".into(), Vec::new());
                }
                match whitelist.save(&dir.join(WHITELIST_FILE)) {
                    Ok(()) => (format!("Added {name} to the whitelist"), Vec::new()),
                    Err(e) => (format!("Could not save {WHITELIST_FILE}: {e}"), Vec::new()),
                }
            }
            Some("remove") => {
                let Some(&name) = args.get(1) else {
                    return ("usage: /whitelist remove <player>".into(), Vec::new());
                };
                if !whitelist.remove(name) {
                    return ("Player is not whitelisted".into(), Vec::new());
                }
                match whitelist.save(&dir.join(WHITELIST_FILE)) {
                    Ok(()) => (format!("Removed {name} from the whitelist"), Vec::new()),
                    Err(e) => (format!("Could not save {WHITELIST_FILE}: {e}"), Vec::new()),
                }
            }
            // Vanilla reloads the file; the enabled switch is run-time state and
            // survives the reload.
            Some("reload") => match WhitelistRegistry::load(&dir.join(WHITELIST_FILE)) {
                Ok(mut reloaded) => {
                    reloaded.enabled = whitelist.enabled;
                    *whitelist = reloaded;
                    ("Reloaded the whitelist".into(), Vec::new())
                }
                Err(e) => (format!("Could not reload {WHITELIST_FILE}: {e}"), Vec::new()),
            },
            _ => (
                "usage: /whitelist <on|off|list|add <player>|remove <player>|reload>".into(),
                Vec::new(),
            ),
        },
        _ => (format!("Unknown server operator command '{cmd}'"), Vec::new()),
    }
}

// -------------------------------------------------------------- stdin console

/// How `run_stdin_console_loop` ended: `stop` was issued, or input ran out.
/// Headless runs (`/dev/null` stdin) take the `Eof` arm immediately — the blocked
/// read means zero CPU spin, not a polling loop that has to be throttled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub(crate) enum ConsoleExit {
    Stop,
    Eof,
}

/// Read command lines, strip an optional leading `/`, skip blanks, and hand each
/// line to `dispatch` (which owns the reply — printing it or forwarding it to a
/// player is the wiring's choice). `stop` is dispatched like any other command
/// and then ends the loop so the caller can shut the server down; EOF and read
/// errors end it silently.
#[allow(dead_code)]
pub(crate) fn run_stdin_console_loop(
    reader: impl std::io::BufRead,
    mut dispatch: impl FnMut(&str) -> String,
) -> ConsoleExit {
    for line in reader.lines() {
        let Ok(line) = line else {
            return ConsoleExit::Eof;
        };
        let line = line.trim().trim_start_matches('/').trim();
        if line.is_empty() {
            continue;
        }
        let stop = line.eq_ignore_ascii_case("stop");
        dispatch(line);
        if stop {
            return ConsoleExit::Stop;
        }
    }
    ConsoleExit::Eof
}

#[cfg(test)]
mod server_ops_tests {
    use super::*;

    fn tmp_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join(format!("mc-spine-ops-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn ops_registry_supports_vanilla_levels_0_to_4_and_legacy_array_upgrade() {
        let dir = tmp_dir("ops");
        let path = dir.join(OPS_FILE);

        // Vanilla object schema: levels kept 0..=4, bypassesPlayerLimit honored.
        std::fs::write(
            &path,
            r#"[
              {"uuid":"00000000-0000-0000-0000-000000000001","name":"Root","level":4,"bypassesPlayerLimit":true},
              {"uuid":"00000000-0000-0000-0000-000000000002","name":"Mod","level":3,"bypassesPlayerLimit":false},
              {"uuid":"00000000-0000-0000-0000-000000000003","name":"Helper","level":1,"bypassesPlayerLimit":false},
              {"uuid":"00000000-0000-0000-0000-000000000004","name":"Honorary","level":0,"bypassesPlayerLimit":false}
            ]"#,
        )
        .unwrap();
        let ops = OpsRegistry::load(&path).unwrap();
        assert_eq!(ops.op_level_of("root"), 4); // case-insensitive
        assert_eq!(ops.op_level_of("MOD"), 3);
        assert_eq!(ops.op_level_of("Helper"), 1);
        assert_eq!(ops.op_level_of("Honorary"), 0);
        assert!(ops.is_op("honorary")); // level 0 is still an operator
        assert_eq!(ops.op_level_of("stranger"), 0);
        assert!(ops.bypasses_player_limit("ROOT"));
        assert!(!ops.bypasses_player_limit("Mod"));
        assert!(!ops.bypasses_player_limit("stranger"));

        // Levels beyond 4 are clamped on read.
        std::fs::write(&path, r#"[{"name":"Over","level":9,"bypassesPlayerLimit":false}]"#).unwrap();
        assert_eq!(OpsRegistry::load(&path).unwrap().op_level_of("over"), 4);

        // The legacy flat array upgrades to level-4 operators and saves back as
        // Vanilla objects.
        std::fs::write(&path, r#"["alice","bob"]"#).unwrap();
        let mut ops = OpsRegistry::load(&path).unwrap();
        assert_eq!(ops.op_level_of("alice"), 4);
        assert_eq!(ops.op_level_of("BOB"), 4);
        ops.set("carol", 2, true);
        assert!(ops.save(&path).is_ok());
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("\"level\": 4"), "{text}");
        assert!(text.contains("\"bypassesPlayerLimit\": true"), "{text}");
        let mut reread = OpsRegistry::load(&path).unwrap();
        assert_eq!(reread.op_level_of("carol"), 2);
        assert!(reread.bypasses_player_limit("carol"));
        assert!(reread.remove("alice") && !reread.remove("alice"));

        // A missing file is an empty registry; malformed JSON is an error.
        assert!(OpsRegistry::load(&dir.join("nope.json")).unwrap().entries.is_empty());
        std::fs::write(&path, "not json").unwrap();
        assert!(OpsRegistry::load(&path).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn required_permission_levels_match_vanilla() {
        for c in ["help", "list", "me", "msg", "tell", "w", "teammsg", "trigger"] {
            assert_eq!(required_permission_level(c), 0, "{c}");
        }
        for c in ["gamemode", "tp", "give", "fill", "clone", "setblock", "gamerule", "time", "weather", "summon", "effect"] {
            assert_eq!(required_permission_level(c), 2, "{c}");
        }
        for c in ["kick", "ban", "ban-ip", "banlist", "pardon", "pardon-ip", "op", "deop", "whitelist", "setidletimeout"] {
            assert_eq!(required_permission_level(c), 3, "{c}");
        }
        for c in ["stop", "save-all", "save-on", "save-off", "debug", "jfr", "perf", "publish"] {
            assert_eq!(required_permission_level(c), 4, "{c}");
        }
        assert!(command_allowed_at_level("list", 0));
        assert!(!command_allowed_at_level("ban", 2));
        assert!(command_allowed_at_level("ban", 3));
        assert!(!command_allowed_at_level("stop", 3));
        assert!(command_allowed_at_level("stop", 4));
    }

    #[test]
    fn ban_and_whitelist_registries_persist_atomically_and_gate_login_admission() {
        let dir = tmp_dir("bans");
        let ops = OpsRegistry::default();
        let mut bans = BanRegistry::default();
        let mut whitelist = WhitelistRegistry::default();

        bans.ban_player("Griefer", "griefing", "Server", "2026-11-21T00:00:00Z");
        bans.ban_ip("10.0.0.9", "scanning", "Server", "2026-11-21T00:00:00Z");
        whitelist.add("Friend");
        bans.save(&dir).unwrap();
        whitelist.save(&dir.join(WHITELIST_FILE)).unwrap();

        // The on-disk bytes are the Vanilla schemas.
        let text = std::fs::read_to_string(dir.join(BANNED_PLAYERS_FILE)).unwrap();
        assert!(text.contains("\"expires\": \"forever\""), "{text}");
        assert!(text.contains("\"reason\": \"griefing\""), "{text}");
        let text = std::fs::read_to_string(dir.join(BANNED_IPS_FILE)).unwrap();
        assert!(text.contains("\"ip\": \"10.0.0.9\""), "{text}");
        let text = std::fs::read_to_string(dir.join(WHITELIST_FILE)).unwrap();
        assert!(text.contains("\"name\": \"Friend\""), "{text}");

        // A fresh process reads the same decisions back.
        let bans = BanRegistry::load(&dir).unwrap();
        let whitelist = WhitelistRegistry::load(&dir.join(WHITELIST_FILE)).unwrap();
        assert!(bans.player_ban("griefer").is_some());
        assert!(bans.ip_ban("10.0.0.9").is_some());
        assert!(whitelist.contains("friend"));

        let admit = |name: &str, ip: &str, count: usize, ops: &OpsRegistry, wl: &WhitelistRegistry| {
            check_login_admission(name, ip, count, 2, ops, &bans, wl)
        };

        // Whitelist off: bans apply, cap applies.
        let mut wl = whitelist.clone();
        assert_eq!(
            admit("GRIEFER", "1.2.3.4", 0, &ops, &wl).unwrap_err(),
            "You are banned from this server.\nReason: griefing"
        );
        assert_eq!(
            admit("Newbie", "10.0.0.9", 0, &ops, &wl).unwrap_err(),
            "Your IP address is banned from this server.\nReason: scanning"
        );
        assert_eq!(
            admit("Newbie", "1.2.3.4", 2, &ops, &wl).unwrap_err(),
            "The server is full!"
        );
        assert!(admit("Newbie", "1.2.3.4", 1, &ops, &wl).is_ok());

        // Whitelist on: only listed players and operators get in.
        wl.enabled = true;
        assert_eq!(
            admit("Newbie", "1.2.3.4", 0, &ops, &wl).unwrap_err(),
            "You are not white-listed on this server!"
        );
        assert!(admit("Friend", "1.2.3.4", 0, &ops, &wl).is_ok());
        let mut ops = OpsRegistry::default();
        ops.set("Admin", 4, false);
        assert!(admit("Admin", "1.2.3.4", 0, &ops, &wl).is_ok()); // ops bypass the whitelist

        // Full server: only bypassesPlayerLimit operators get past the cap.
        ops.set("Vip", 4, true);
        assert_eq!(
            admit("Vip", "1.2.3.4", 2, &ops, &wl),
            Ok(())
        );
        assert_eq!(
            admit("Admin", "1.2.3.4", 2, &ops, &wl).unwrap_err(),
            "The server is full!"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ops_commands_ban_pardon_banlist_whitelist_work_end_to_end() {
        let dir = tmp_dir("cmds");
        let mut bans = BanRegistry::default();
        let mut whitelist = WhitelistRegistry::default();
        let run = |input: &str, bans: &mut BanRegistry, whitelist: &mut WhitelistRegistry| {
            exec_server_ops_command(input, bans, whitelist, &dir, "Server", "2026-11-21T00:00:00Z")
        };

        let (reply, kick) = run("ban Griefer greifed spawn", &mut bans, &mut whitelist);
        assert_eq!(reply, "Banned Griefer: greifed spawn");
        assert_eq!(kick, ["Griefer"]);
        assert!(dir.join(BANNED_PLAYERS_FILE).is_file());

        // Leading slash, default reason, duplicate ban.
        let (reply, _) = run("/ban Troll", &mut bans, &mut whitelist);
        assert_eq!(reply, "Banned Troll: Banned by an operator.");
        let (reply, kick) = run("ban griefer again", &mut bans, &mut whitelist);
        assert_eq!(reply, "Nothing changed. The player is already banned");
        assert!(kick.is_empty());

        let (reply, _) = run("ban-ip 10.0.0.9 scanning", &mut bans, &mut whitelist);
        assert_eq!(reply, "Banned IP address 10.0.0.9: scanning");
        let (reply, _) = run("ban-ip not-an-ip", &mut bans, &mut whitelist);
        assert_eq!(reply, "Invalid IP address 'not-an-ip'");

        let (reply, _) = run("banlist", &mut bans, &mut whitelist);
        assert!(reply.starts_with("There are 2 bans:"), "{reply}");
        assert!(reply.contains("Griefer"), "{reply}");
        let (reply, _) = run("banlist ips", &mut bans, &mut whitelist);
        assert!(reply.contains("10.0.0.9"), "{reply}");

        let (reply, _) = run("pardon Griefer", &mut bans, &mut whitelist);
        assert_eq!(reply, "Unbanned Griefer");
        assert!(bans.player_ban("griefer").is_none());
        let (reply, _) = run("pardon Griefer", &mut bans, &mut whitelist);
        assert_eq!(reply, "Nothing changed. The player is not banned");
        let (reply, _) = run("pardon-ip 10.0.0.9", &mut bans, &mut whitelist);
        assert_eq!(reply, "Unbanned IP address 10.0.0.9");
        let (reply, _) = run("pardon-ip 10.0.0.9", &mut bans, &mut whitelist);
        assert_eq!(reply, "Nothing changed. That IP is not banned");

        // Whitelist subcommands, persisted and reloadable.
        let (reply, _) = run("whitelist on", &mut bans, &mut whitelist);
        assert_eq!(reply, "Whitelist is now turned on");
        let (reply, _) = run("whitelist on", &mut bans, &mut whitelist);
        assert_eq!(reply, "Whitelist is already turned on");
        let (reply, _) = run("whitelist add Friend", &mut bans, &mut whitelist);
        assert_eq!(reply, "Added Friend to the whitelist");
        let (reply, _) = run("whitelist add Friend", &mut bans, &mut whitelist);
        assert_eq!(reply, "Player is already whitelisted");
        let (reply, _) = run("whitelist list", &mut bans, &mut whitelist);
        assert_eq!(reply, "There are 1 whitelisted players: Friend");

        // The file is what a reload reads; `enabled` is run-time and survives.
        whitelist.entries.clear();
        let (reply, _) = run("whitelist reload", &mut bans, &mut whitelist);
        assert_eq!(reply, "Reloaded the whitelist");
        assert!(whitelist.contains("Friend"));
        assert!(whitelist.enabled);
        let (reply, _) = run("whitelist remove Friend", &mut bans, &mut whitelist);
        assert_eq!(reply, "Removed Friend from the whitelist");
        let (reply, _) = run("whitelist off", &mut bans, &mut whitelist);
        assert_eq!(reply, "Whitelist is now turned off");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn stdin_console_loop_dispatches_commands_and_exits_cleanly_on_eof() {
        let mut seen: Vec<String> = Vec::new();
        let exit = run_stdin_console_loop(
            std::io::Cursor::new(b"\n  \n/list\nsay hi\n".as_slice()),
            |line| {
                seen.push(line.to_owned());
                format!("ran {line}")
            },
        );
        assert_eq!(exit, ConsoleExit::Eof); // input just ran out
        assert_eq!(seen, ["list", "say hi"]); // blanks skipped, `/` stripped

        // `stop` is dispatched and then ends the loop without waiting for EOF.
        let mut seen = Vec::new();
        let exit = run_stdin_console_loop(
            std::io::Cursor::new(b"save-all\nstop\nlist\n".as_slice()),
            |line| {
                seen.push(line.to_owned());
                String::new()
            },
        );
        assert_eq!(exit, ConsoleExit::Stop);
        assert_eq!(seen, ["save-all", "stop"]);

        // `/dev/null` — the headless case — returns Eof with no dispatch and no spin.
        let mut calls = 0;
        let exit = run_stdin_console_loop(
            std::io::BufReader::new(std::fs::File::open("/dev/null").unwrap()),
            |_| {
                calls += 1;
                String::new()
            },
        );
        assert_eq!(exit, ConsoleExit::Eof);
        assert_eq!(calls, 0);
    }
}
