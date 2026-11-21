// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
//! `mc-spine-init`: bootstrap a Vanilla-compatible server directory, run a
//! pre-flight health check (`doctor`), and administer ops/bans/whitelist
//! offline while the server is stopped.
//!
//! Pure `std` plus `mc-spine-core`: the binary shares the crate's
//! zero-dependency contract, so every file it writes goes through the same
//! atomic persistence [`mc_spine_server_files::ServerDirectory`] uses.
//!
//! Exit codes: `0` success (doctor: directory is ready to boot), `2` a bad
//! command line or a doctor verdict of "EULA not accepted", `1` a file-level
//! failure (unreadable or corrupt server files).
//!
//! `rcon.password` is a secret: neither the text output nor `--json` ever
//! prints it — reports carry `rcon_password_set` / `[REDACTED]` instead.

use mc_spine_core::version::TargetVersion;
use mc_spine_server_files::json::Json;
use mc_spine_server_files::{
    check_eula, valid_player_name, BannedIps, BannedPlayers, ClaimChunkKey, ClaimOwner,
    ClaimsWorld, ChunkClaim, EulaStatus, OpsList, Party, ServerDirectory, ServerProperties,
    SubClaimId, UserCache, Whitelist, BANNED_IPS_FILE, BANNED_PLAYERS_FILE, EULA_FILE, EULA_LINK,
    OPS_FILE, PROPERTIES_FILE, USERCACHE_FILE, WHITELIST_FILE,
};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

const HELP: &str = "\
mc-spine-init — bootstrap and administer a Vanilla-compatible server directory

USAGE:
    mc-spine-init init --dir <dir> [--mc-version 1.21.1|26.3] [--port <u16>]
                       [--motd <text>] [--online-mode true|false]
                       [--max-players <u32>] [--json]
    mc-spine-init doctor --dir <dir> [--mc-version 1.21.1|26.3] [--json]
    mc-spine-init op --dir <dir> <player> [--level 1..4] [--bypass-limit] [--json]
    mc-spine-init deop --dir <dir> <player> [--json]
    mc-spine-init ban --dir <dir> <player> [--reason \"<text>\"]
                      [--created <stamp>] [--source <name>] [--json]
    mc-spine-init pardon --dir <dir> <player> [--json]
    mc-spine-init whitelist --dir <dir> <on|off|add|remove|list> [player] [--json]
    mc-spine-init claims --dir <dir> list [--dimension <dim>] [--json]
    mc-spine-init claims --dir <dir> parties [--json]
    mc-spine-init claims --dir <dir> server-claim <dim> <cx> <cz> [--forceload] [--json]
    mc-spine-init claims --dir <dir> unclaim <dim> <cx> <cz> [--json]
    mc-spine-init --help

COMMANDS:
    init        Create the directory when missing, write eula.txt (eula=false —
                the EULA is never auto-accepted) and any missing
                server.properties / ops.json / whitelist.json /
                banned-players.json / banned-ips.json / usercache.json.
                Existing files are never clobbered; only the explicitly passed
                flags are applied on top of an existing server.properties.
    doctor      Read-only pre-flight check: eula.txt acceptance,
                server.properties (rcon.password always redacted), the admin
                JSON lists and usercache.json. Exits 0 when the directory is
                ready, 2 with the action to take when the EULA is missing or
                not accepted, 1 on corrupt files.
    op          Grant operator status (default level: the directory's
                op-permission-level, else 4). --bypass-limit sets
                bypassesPlayerLimit; on an existing operator an omitted
                --level/--bypass-limit keeps the stored value.
    deop        Revoke operator status.
    ban         Ban a player by name (offline UUID). --reason defaults to
                \"Banned by an operator.\"; --created defaults to the epoch
                because this tool keeps no clock; --source defaults to
                \"mc-spine-init\".
    pardon      Lift a player ban.
    whitelist   on/off toggle white-list in server.properties; add/remove edit
                whitelist.json; list prints its entries.
    claims      Offline OpenPAC/FTB-Chunks administration of the claims store
                (openpac_claims.json / openpac_parties.json). list prints
                claims (optionally filtered to one dimension) plus the
                forceload count; parties prints parties with members and
                allies; server-claim claims a chunk for the server (spawn
                protection), optionally force-loaded; unclaim drops any
                claim as admin. Only the two claims files are read and
                written — eula.txt is never touched.

OPTIONS:
    --dir <dir>          The server working directory (required).
    --mc-version <v>     Pinned target: \"1.21.1\" or \"26.3\"
                         (default: 26.3, the latest vanilla).
    --json               Print the report as JSON instead of text.
    -h, --help           Show this help.

EXIT CODES:
    0  success; for doctor the directory is ready to boot
    1  a server file could not be used: unreadable or corrupt JSON/properties
    2  bad command line; for doctor the EULA is missing or not yet accepted
";

/// The stamps a CLI-written ban entry carries when the operator gives none:
/// the file schema wants `created`/`source` fields and this crate keeps no
/// clock, so `created` defaults to the epoch and `source` names the tool.
const DEFAULT_BAN_REASON: &str = "Banned by an operator.";
const DEFAULT_BAN_CREATED: &str = "1970-01-01T00:00:00Z";
const DEFAULT_BAN_SOURCE: &str = "mc-spine-init";

/// The seven files a server directory is made of, in report order.
const ALL_FILES: [&str; 7] = [
    EULA_FILE,
    PROPERTIES_FILE,
    OPS_FILE,
    WHITELIST_FILE,
    BANNED_PLAYERS_FILE,
    BANNED_IPS_FILE,
    USERCACHE_FILE,
];

/// A fatal-to-this-invocation condition paired with its exit code.
struct CliError {
    code: u8,
    message: String,
}

/// Bad command line: exit 2.
fn usage(message: impl Into<String>) -> CliError {
    CliError {
        code: 2,
        message: message.into(),
    }
}

/// File-level failure: exit 1.
fn fail(message: impl Into<String>) -> CliError {
    CliError {
        code: 1,
        message: message.into(),
    }
}

enum WhitelistAction {
    On,
    Off,
    Add(String),
    Remove(String),
    List,
}

enum ClaimsAction {
    /// `--dimension` is a normalized `namespace:path` filter.
    List { dimension: Option<String> },
    Parties,
    ServerClaim {
        dimension: String,
        cx: i32,
        cz: i32,
        forceload: bool,
    },
    Unclaim {
        dimension: String,
        cx: i32,
        cz: i32,
    },
}

enum Command {
    Help,
    Init {
        dir: PathBuf,
        target: TargetVersion,
        port: Option<u16>,
        motd: Option<String>,
        online_mode: Option<bool>,
        max_players: Option<u32>,
        json: bool,
    },
    Doctor {
        dir: PathBuf,
        target: TargetVersion,
        json: bool,
    },
    Op {
        dir: PathBuf,
        target: TargetVersion,
        player: String,
        level: Option<u8>,
        bypass_limit: bool,
        json: bool,
    },
    Deop {
        dir: PathBuf,
        target: TargetVersion,
        player: String,
        json: bool,
    },
    Ban {
        dir: PathBuf,
        target: TargetVersion,
        player: String,
        reason: String,
        created: String,
        source: String,
        json: bool,
    },
    Pardon {
        dir: PathBuf,
        target: TargetVersion,
        player: String,
        json: bool,
    },
    Whitelist {
        dir: PathBuf,
        target: TargetVersion,
        action: WhitelistAction,
        json: bool,
    },
    Claims {
        dir: PathBuf,
        action: ClaimsAction,
        json: bool,
    },
}

// ------------------------------------------------------------- arg parsing

/// The shared `--dir`/`--mc-version`/`--json` core every subcommand reads.
#[derive(Default)]
struct Common {
    dir: Option<PathBuf>,
    target: Option<TargetVersion>,
    json: bool,
    positionals: Vec<String>,
}

/// One pass over `args`: recognizes the common flags, hands every other
/// `--flag`/`--flag=value` to `take_flag` (returning `true` when consumed)
/// and collects positionals.
fn parse_args(
    cmd: &str,
    args: &[String],
    mut take_flag: impl FnMut(&str, Option<&str>, &mut ArgSource<'_, '_>) -> Result<bool, CliError>,
) -> Result<Common, CliError> {
    let mut common = Common::default();
    let mut i = 0;
    while i < args.len() {
        let arg = args[i].as_str();
        let (flag, inline) = match arg.split_once('=') {
            Some((f, v)) if f.starts_with("--") => (f, Some(v)),
            _ => (arg, None),
        };
        let mut source = ArgSource {
            args,
            i: &mut i,
            inline,
        };
        let consumed = match flag {
            "--dir" => {
                common.dir = Some(PathBuf::from(source.value("--dir")?));
                true
            }
            "--mc-version" => {
                let v = source.value("--mc-version")?;
                common.target = Some(TargetVersion::parse(&v).ok_or_else(|| {
                    usage(format!(
                        "unknown --mc-version {v:?}; expected \"1.21.1\" or \"26.3\""
                    ))
                })?);
                true
            }
            "--json" => {
                source.no_value("--json")?;
                common.json = true;
                true
            }
            // A `-`-prefixed word is an option — unless it starts with a
            // digit, in which case it is a negative number (claims chunk
            // coordinates legitimately go below zero), not a flag.
            other if other.starts_with('-')
                && !other
                    .as_bytes()
                    .get(1)
                    .is_some_and(u8::is_ascii_digit) =>
            {
                take_flag(other, inline, &mut source)?
            }
            _ => {
                common.positionals.push(arg.to_owned());
                true
            }
        };
        if !consumed {
            return Err(usage(format!("{cmd}: unknown option {flag:?}")));
        }
        i += 1;
    }
    Ok(common)
}

/// Where a flag's value can come from: the `--flag=value` inline part or the
/// next argv word.
struct ArgSource<'a, 'b> {
    args: &'a [String],
    i: &'b mut usize,
    inline: Option<&'a str>,
}

impl ArgSource<'_, '_> {
    fn value(&mut self, name: &str) -> Result<String, CliError> {
        match self.inline {
            Some(v) => Ok(v.to_owned()),
            None => {
                *self.i += 1;
                self.args
                    .get(*self.i)
                    .cloned()
                    .ok_or_else(|| usage(format!("{name} needs a value")))
            }
        }
    }

    fn no_value(&self, name: &str) -> Result<(), CliError> {
        if self.inline.is_some() {
            return Err(usage(format!("{name} takes no value")));
        }
        Ok(())
    }
}

fn required_dir(common: &Common) -> Result<PathBuf, CliError> {
    common
        .dir
        .clone()
        .ok_or_else(|| usage("missing required option --dir"))
}

fn target(common: &Common) -> TargetVersion {
    common.target.unwrap_or(TargetVersion::LATEST)
}

fn one_player(cmd: &str, common: &Common) -> Result<String, CliError> {
    match common.positionals.as_slice() {
        [player] => Ok(player.clone()),
        [] => Err(usage(format!("{cmd} needs a player name"))),
        _ => Err(usage(format!("{cmd} takes exactly one player name"))),
    }
}

fn check_player(cmd: &str, player: &str) -> Result<String, CliError> {
    if !valid_player_name(player) {
        return Err(usage(format!(
            "{cmd}: {player:?} is not a valid player name (1-16 of [A-Za-z0-9_])"
        )));
    }
    Ok(player.to_owned())
}

fn parse_bool(name: &str, raw: &str) -> Result<bool, CliError> {
    match raw {
        "true" => Ok(true),
        "false" => Ok(false),
        _ => Err(usage(format!("{name} expects true or false, got {raw:?}"))),
    }
}

fn parse_command(args: &[String]) -> Result<Command, CliError> {
    if args.is_empty() {
        return Err(usage("no command given; try --help"));
    }
    // `-h`/`--help` anywhere asks for the help text.
    if args.iter().any(|a| a == "--help" || a == "-h") || args[0] == "help" {
        return Ok(Command::Help);
    }
    let rest = &args[1..];
    match args[0].as_str() {
        "init" => {
            let mut port = None;
            let mut motd = None;
            let mut online_mode = None;
            let mut max_players = None;
            let common = parse_args("init", rest, |flag, _inline, source| {
                match flag {
                    "--port" => {
                        let v = source.value("--port")?;
                        port = Some(v.parse::<u16>().map_err(|_| {
                            usage(format!("--port expects 0-65535, got {v:?}"))
                        })?);
                    }
                    "--motd" => motd = Some(source.value("--motd")?),
                    "--online-mode" => {
                        online_mode =
                            Some(parse_bool("--online-mode", &source.value("--online-mode")?)?)
                    }
                    "--max-players" => {
                        let v = source.value("--max-players")?;
                        max_players = Some(v.parse::<u32>().map_err(|_| {
                            usage(format!("--max-players expects a non-negative integer, got {v:?}"))
                        })?);
                    }
                    _ => return Ok(false),
                }
                Ok(true)
            })?;
            if !common.positionals.is_empty() {
                return Err(usage(format!(
                    "init: unexpected argument {:?}",
                    common.positionals[0]
                )));
            }
            Ok(Command::Init {
                dir: required_dir(&common)?,
                target: target(&common),
                port,
                motd,
                online_mode,
                max_players,
                json: common.json,
            })
        }
        "doctor" => {
            let common = parse_args("doctor", rest, |_flag, _inline, _source| Ok(false))?;
            if !common.positionals.is_empty() {
                return Err(usage(format!(
                    "doctor: unexpected argument {:?}",
                    common.positionals[0]
                )));
            }
            Ok(Command::Doctor {
                dir: required_dir(&common)?,
                target: target(&common),
                json: common.json,
            })
        }
        "op" => {
            let mut level = None;
            let mut bypass_limit = false;
            let common = parse_args("op", rest, |flag, _inline, source| {
                match flag {
                    "--level" => {
                        let v = source.value("--level")?;
                        let n = v.parse::<u8>().ok().filter(|n| (1..=4).contains(n));
                        level = Some(n.ok_or_else(|| {
                            usage(format!("--level expects 1-4, got {v:?}"))
                        })?);
                    }
                    "--bypass-limit" => {
                        source.no_value("--bypass-limit")?;
                        bypass_limit = true;
                    }
                    _ => return Ok(false),
                }
                Ok(true)
            })?;
            let player = check_player("op", &one_player("op", &common)?)?;
            Ok(Command::Op {
                dir: required_dir(&common)?,
                target: target(&common),
                player,
                level,
                bypass_limit,
                json: common.json,
            })
        }
        "deop" | "pardon" => {
            let common = parse_args(args[0].as_str(), rest, |_flag, _inline, _source| Ok(false))?;
            let player = check_player(args[0].as_str(), &one_player(args[0].as_str(), &common)?)?;
            let (dir, target, json) = (required_dir(&common)?, target(&common), common.json);
            match args[0].as_str() {
                "deop" => Ok(Command::Deop {
                    dir,
                    target,
                    player,
                    json,
                }),
                _ => Ok(Command::Pardon {
                    dir,
                    target,
                    player,
                    json,
                }),
            }
        }
        "ban" => {
            let mut reason = DEFAULT_BAN_REASON.to_owned();
            let mut created = DEFAULT_BAN_CREATED.to_owned();
            let mut source_name = DEFAULT_BAN_SOURCE.to_owned();
            let common = parse_args("ban", rest, |flag, _inline, source| {
                match flag {
                    "--reason" => reason = source.value("--reason")?,
                    "--created" => created = source.value("--created")?,
                    "--source" => source_name = source.value("--source")?,
                    _ => return Ok(false),
                }
                Ok(true)
            })?;
            let player = check_player("ban", &one_player("ban", &common)?)?;
            Ok(Command::Ban {
                dir: required_dir(&common)?,
                target: target(&common),
                player,
                reason,
                created,
                source: source_name,
                json: common.json,
            })
        }
        "whitelist" => {
            let common = parse_args("whitelist", rest, |_flag, _inline, _source| Ok(false))?;
            let action = match common.positionals.as_slice() {
                [a] if a == "on" => WhitelistAction::On,
                [a] if a == "off" => WhitelistAction::Off,
                [a] if a == "list" => WhitelistAction::List,
                [a, p] if a == "add" => {
                    WhitelistAction::Add(check_player("whitelist add", p)?)
                }
                [a, p] if a == "remove" => {
                    WhitelistAction::Remove(check_player("whitelist remove", p)?)
                }
                [a] if a == "add" || a == "remove" => {
                    return Err(usage(format!("whitelist {a} needs a player name")))
                }
                [a, ..] if matches!(a.as_str(), "on" | "off" | "add" | "remove" | "list") => {
                    return Err(usage(format!(
                        "whitelist {a}: unexpected extra arguments"
                    )))
                }
                [a, ..] => {
                    return Err(usage(format!(
                        "whitelist: unknown action {a:?}; expected on|off|add|remove|list"
                    )))
                }
                [] => {
                    return Err(usage(
                        "whitelist needs an action: on|off|add|remove|list",
                    ))
                }
            };
            Ok(Command::Whitelist {
                dir: required_dir(&common)?,
                target: target(&common),
                action,
                json: common.json,
            })
        }
        "claims" => {
            let mut dimension = None;
            let mut forceload = false;
            let common = parse_args("claims", rest, |flag, _inline, source| {
                match flag {
                    "--dimension" => dimension = Some(source.value("--dimension")?),
                    "--forceload" => {
                        source.no_value("--forceload")?;
                        forceload = true;
                    }
                    _ => return Ok(false),
                }
                Ok(true)
            })?;
            // --dimension only makes sense on list, --forceload only on
            // server-claim; a misplaced flag is a usage error, not a
            // silently ignored one.
            let misplaced = |action: &str, flag: &str| -> CliError {
                usage(format!("claims {action}: {flag} does not apply to this action"))
            };
            let coord = |action: &str, name: &str, raw: &str| -> Result<i32, CliError> {
                raw.parse::<i32>().map_err(|_| {
                    usage(format!(
                        "claims {action}: {name} {raw:?} is not a 32-bit integer"
                    ))
                })
            };
            let dim_filter = |raw: &str| -> Result<String, CliError> {
                ClaimChunkKey::new(raw, 0, 0)
                    .map(|k| k.dimension)
                    .map_err(|e| usage(format!("claims list --dimension: {e}")))
            };
            let action = match common.positionals.as_slice() {
                [a] if a == "list" => {
                    if forceload {
                        return Err(misplaced("list", "--forceload"));
                    }
                    ClaimsAction::List {
                        dimension: dimension.as_deref().map(dim_filter).transpose()?,
                    }
                }
                [a] if a == "parties" => {
                    if dimension.is_some() {
                        return Err(misplaced("parties", "--dimension"));
                    }
                    if forceload {
                        return Err(misplaced("parties", "--forceload"));
                    }
                    ClaimsAction::Parties
                }
                [a, dim, cx, cz] if a == "server-claim" => {
                    if dimension.is_some() {
                        return Err(misplaced("server-claim", "--dimension"));
                    }
                    ClaimsAction::ServerClaim {
                        dimension: dim.clone(),
                        cx: coord("server-claim", "cx", cx)?,
                        cz: coord("server-claim", "cz", cz)?,
                        forceload,
                    }
                }
                [a, dim, cx, cz] if a == "unclaim" => {
                    if dimension.is_some() {
                        return Err(misplaced("unclaim", "--dimension"));
                    }
                    if forceload {
                        return Err(misplaced("unclaim", "--forceload"));
                    }
                    ClaimsAction::Unclaim {
                        dimension: dim.clone(),
                        cx: coord("unclaim", "cx", cx)?,
                        cz: coord("unclaim", "cz", cz)?,
                    }
                }
                [a, ..] if matches!(a.as_str(), "server-claim" | "unclaim") => {
                    return Err(usage(format!(
                        "claims {a} needs exactly <dim> <cx> <cz>"
                    )))
                }
                [a, ..] if matches!(a.as_str(), "list" | "parties") => {
                    return Err(usage(format!("claims {a}: unexpected extra arguments")))
                }
                [a, ..] => {
                    return Err(usage(format!(
                        "claims: unknown action {a:?}; expected list|parties|server-claim|unclaim"
                    )))
                }
                [] => {
                    return Err(usage(
                        "claims needs an action: list|parties|server-claim|unclaim",
                    ))
                }
            };
            Ok(Command::Claims {
                dir: required_dir(&common)?,
                action,
                json: common.json,
            })
        }
        other => Err(usage(format!("unknown command {other:?}; try --help"))),
    }
}

// ------------------------------------------------------------- JSON output

fn jstr(s: impl Into<String>) -> Json {
    Json::String(s.into())
}

fn jint(n: impl Into<i64>) -> Json {
    Json::Number(n.into() as f64)
}

fn jobj(pairs: Vec<(&str, Json)>) -> Json {
    Json::Object(
        pairs
            .into_iter()
            .map(|(k, v)| (k.to_owned(), v))
            .collect(),
    )
}

fn jlist(items: Vec<Json>) -> Json {
    Json::Array(items)
}

fn emit(doc: Json) {
    println!("{}", doc.to_json_string());
}

// -------------------------------------------------------------------- init

/// Which of [`ALL_FILES`] existed before [`ServerDirectory::load_or_init`]
/// ran (it may create `eula.txt` itself).
fn preexisting(dir: &Path) -> Vec<bool> {
    ALL_FILES
        .iter()
        .map(|name| dir.join(name).exists())
        .collect()
}

/// A missing `server.properties` materializes as the Vanilla defaults — the
/// same set the typed accessors report — so a fresh directory reads the way a
/// first-boot file does.
fn populate_default_properties(props: &mut ServerProperties) {
    props.set_server_port(25565);
    props.set_motd("A Minecraft Server");
    props.set_max_players(20);
    props.set_online_mode(true);
    props.set_white_list(false);
    props.set_enforce_whitelist(false);
    props.set_view_distance(10);
    props.set_simulation_distance(10);
    props.set_level_name("world");
    props.set_level_seed("");
    props.set_difficulty("easy");
    props.set_gamemode("survival");
    props.set_pvp(true);
    props.set_enable_rcon(false);
    props.set_rcon_port(25575);
    props.set_network_compression_threshold(256);
}

fn cmd_init(
    dir: &Path,
    target: TargetVersion,
    port: Option<u16>,
    motd: Option<&str>,
    online_mode: Option<bool>,
    max_players: Option<u32>,
    json: bool,
) -> Result<(), CliError> {
    let existed = preexisting(dir);
    let existed = |name: &str| existed[ALL_FILES.iter().position(|n| *n == name).unwrap()];

    let mut sd = ServerDirectory::load_or_init(dir, target).map_err(|e| {
        fail(format!("cannot initialize {}: {e}", dir.display()))
    })?;

    let props_missing = !existed(PROPERTIES_FILE);
    if props_missing {
        populate_default_properties(&mut sd.properties);
    }

    // Only the explicitly passed flags touch an existing document.
    let mut applied: Vec<(&str, String)> = Vec::new();
    if let Some(port) = port {
        sd.properties.set_server_port(port);
        applied.push(("server-port", port.to_string()));
    }
    if let Some(motd) = motd {
        sd.properties.set_motd(motd);
        applied.push(("motd", motd.to_owned()));
    }
    if let Some(online) = online_mode {
        sd.properties.set_online_mode(online);
        applied.push(("online-mode", online.to_string()));
    }
    if let Some(max) = max_players {
        sd.properties.set_max_players(i64::from(max));
        applied.push(("max-players", max.to_string()));
    }

    // Write only what was missing or what a flag actually addressed; every
    // write is atomic via the crate's sibling-temp rename.
    let write_err = |e: std::io::Error| {
        fail(format!("cannot write into {}: {e}", dir.display()))
    };
    if props_missing || !applied.is_empty() {
        sd.properties
            .save_to(&dir.join(PROPERTIES_FILE))
            .map_err(write_err)?;
    }
    if !existed(OPS_FILE) {
        sd.ops.save_to(&dir.join(OPS_FILE)).map_err(write_err)?;
    }
    if !existed(WHITELIST_FILE) {
        sd.whitelist
            .save_to(&dir.join(WHITELIST_FILE))
            .map_err(write_err)?;
    }
    if !existed(BANNED_PLAYERS_FILE) {
        sd.banned_players
            .save_to(&dir.join(BANNED_PLAYERS_FILE))
            .map_err(write_err)?;
    }
    if !existed(BANNED_IPS_FILE) {
        sd.banned_ips
            .save_to(&dir.join(BANNED_IPS_FILE))
            .map_err(write_err)?;
    }
    if !existed(USERCACHE_FILE) {
        sd.usercache
            .save_to(&dir.join(USERCACHE_FILE))
            .map_err(write_err)?;
    }

    let created: Vec<&str> = ALL_FILES
        .iter()
        .copied()
        .filter(|name| !existed(name))
        .collect();
    let kept: Vec<&str> = ALL_FILES
        .iter()
        .copied()
        .filter(|name| existed(name))
        .collect();
    let eula_accepted = sd.eula_status() == EulaStatus::Accepted;

    if json {
        emit(jobj(vec![
            ("action", jstr("init")),
            ("dir", jstr(dir.display().to_string())),
            ("mc_version", jstr(target.id())),
            (
                "created",
                jlist(created.iter().map(|n| jstr(*n)).collect()),
            ),
            ("kept", jlist(kept.iter().map(|n| jstr(*n)).collect())),
            (
                "applied",
                jobj(applied
                    .iter()
                    .map(|(k, v)| (*k, jstr(v.clone())))
                    .collect()),
            ),
            ("eula_accepted", Json::Bool(eula_accepted)),
        ]));
        return Ok(());
    }

    println!(
        "initialized server directory {} (target {})",
        dir.display(),
        target.id()
    );
    for name in &created {
        let note = match *name {
            EULA_FILE => " — eula=false; edit it to accept the EULA",
            _ => "",
        };
        println!("  created {name}{note}");
    }
    for name in &kept {
        let note = match *name {
            PROPERTIES_FILE if !applied.is_empty() => " (updated in place)",
            _ => " (kept)",
        };
        println!("  {name}{note}");
    }
    for (key, value) in &applied {
        println!("  applied {key} = {value}");
    }
    if !eula_accepted {
        println!(
            "next step: edit {} and set eula=true to agree to the Minecraft EULA ({EULA_LINK})",
            dir.join(EULA_FILE).display()
        );
    }
    Ok(())
}

// ------------------------------------------------------------------ doctor

/// One checked file's verdict, for both renderers.
enum Check {
    /// Loaded cleanly; the payload is a short human summary plus a JSON
    /// detail object.
    Ok { detail: String, json: Json },
    /// Absent — every loader treats it as the empty/default document.
    Missing,
    /// Unreadable or corrupt; this is what exit 1 reports.
    Broken(String),
}

fn check_json_list(
    dir: &Path,
    name: &str,
    count: impl FnOnce(&Path) -> Result<usize, std::io::Error>,
) -> Check {
    let path = dir.join(name);
    if !path.exists() {
        return Check::Missing;
    }
    match count(&path) {
        Ok(n) => Check::Ok {
            detail: format!("{n} entr{}", if n == 1 { "y" } else { "ies" }),
            json: jobj(vec![("status", jstr("ok")), ("entries", jint(n as i64))]),
        },
        Err(e) => Check::Broken(e.to_string()),
    }
}

fn eula_action(dir: &Path, status: EulaStatus) -> String {
    let file = dir.join(EULA_FILE);
    match status {
        EulaStatus::Accepted => String::new(),
        EulaStatus::NotAccepted => format!(
            "edit {} and set eula=true to agree to the Minecraft EULA ({EULA_LINK})",
            file.display()
        ),
        EulaStatus::Missing => format!(
            "run `mc-spine-init init --dir {}` to create eula.txt, then set eula=true to agree to the Minecraft EULA ({EULA_LINK})",
            dir.display()
        ),
    }
}

fn cmd_doctor(dir: &Path, target: TargetVersion, json: bool) -> Result<u8, CliError> {
    // Doctor is strictly read-only: no load_or_init, nothing is created.
    let eula = check_eula(dir);

    let props_path = dir.join(PROPERTIES_FILE);
    let props_present = props_path.exists();
    let mut loaded_props: Option<ServerProperties> = None;
    let props = match ServerProperties::load_from(&props_path) {
        Ok(p) if props_present => {
            let detail = format!(
                "port {}, motd {:?}, online-mode {}, max-players {}, white-list {}",
                p.server_port(),
                p.motd(),
                p.online_mode(),
                p.max_players(),
                p.white_list(),
            );
            let json = jobj(vec![
                ("status", jstr("ok")),
                ("server_port", jint(i64::from(p.server_port()))),
                ("motd", jstr(p.motd())),
                ("online_mode", Json::Bool(p.online_mode())),
                ("max_players", jint(p.max_players())),
                ("white_list", Json::Bool(p.white_list())),
                ("enforce_whitelist", Json::Bool(p.enforce_whitelist())),
                ("enable_rcon", Json::Bool(p.enable_rcon())),
                ("rcon_port", jint(i64::from(p.rcon_port()))),
                // The password itself never leaves the process.
                ("rcon_password_set", Json::Bool(!p.rcon_password().is_empty())),
            ]);
            loaded_props = Some(p);
            Check::Ok { detail, json }
        }
        Ok(_) => Check::Missing,
        Err(e) => Check::Broken(e.to_string()),
    };
    let props_rcon_note = loaded_props
        .map(|p| {
            format!(
                "rcon {} on port {}, password {}",
                if p.enable_rcon() { "enabled" } else { "disabled" },
                p.rcon_port(),
                if p.rcon_password().is_empty() {
                    "not set"
                } else {
                    "[REDACTED]"
                },
            )
        })
        .unwrap_or_default();

    let checks: Vec<(&str, Check)> = vec![
        (PROPERTIES_FILE, props),
        (
            OPS_FILE,
            check_json_list(dir, OPS_FILE, |p| {
                OpsList::load_from(p).map(|l| l.entries.len())
            }),
        ),
        (
            WHITELIST_FILE,
            check_json_list(dir, WHITELIST_FILE, |p| {
                Whitelist::load_from(p).map(|l| l.entries.len())
            }),
        ),
        (
            BANNED_PLAYERS_FILE,
            check_json_list(dir, BANNED_PLAYERS_FILE, |p| {
                BannedPlayers::load_from(p).map(|l| l.entries.len())
            }),
        ),
        (
            BANNED_IPS_FILE,
            check_json_list(dir, BANNED_IPS_FILE, |p| {
                BannedIps::load_from(p).map(|l| l.entries.len())
            }),
        ),
        (
            USERCACHE_FILE,
            check_json_list(dir, USERCACHE_FILE, |p| {
                UserCache::load_from(p).map(|c| c.len())
            }),
        ),
    ];

    let broken: Vec<&str> = checks
        .iter()
        .filter_map(|(name, check)| matches!(check, Check::Broken(_)).then_some(*name))
        .collect();
    // Corrupt files outrank the EULA verdict: exit 1 says "fix the files",
    // exit 2 says "accept the EULA", exit 0 says "ready".
    let code = if !broken.is_empty() {
        1u8
    } else if eula != EulaStatus::Accepted {
        2u8
    } else {
        0u8
    };

    if json {
        let files = checks
            .iter()
            .map(|(name, check)| {
                let body = match check {
                    Check::Ok { json, .. } => json.clone(),
                    Check::Missing => jobj(vec![
                        ("status", jstr("missing")),
                        ("note", jstr("absent; reads as the empty document")),
                    ]),
                    Check::Broken(e) => jobj(vec![
                        ("status", jstr("error")),
                        ("error", jstr(e.clone())),
                    ]),
                };
                (name.to_string(), body)
            })
            .collect();
        emit(jobj(vec![
            ("action", jstr("doctor")),
            ("dir", jstr(dir.display().to_string())),
            ("mc_version", jstr(target.id())),
            ("ready", Json::Bool(code == 0)),
            ("exit_code", jint(i64::from(code))),
            (
                "eula",
                jobj(vec![
                    ("status", jstr(match eula {
                        EulaStatus::Accepted => "accepted",
                        EulaStatus::NotAccepted => "not_accepted",
                        EulaStatus::Missing => "missing",
                    })),
                    ("accepted", Json::Bool(eula == EulaStatus::Accepted)),
                    ("action", jstr(eula_action(dir, eula))),
                ]),
            ),
            ("files", Json::Object(files)),
            (
                "errors",
                jlist(checks
                    .iter()
                    .filter_map(|(name, check)| match check {
                        Check::Broken(e) => Some(jstr(format!("{name}: {e}"))),
                        _ => None,
                    })
                    .collect()),
            ),
        ]));
        return Ok(code);
    }

    println!("doctor: {} (target {})", dir.display(), target.id());
    let eula_line = match eula {
        EulaStatus::Accepted => "accepted".to_owned(),
        EulaStatus::NotAccepted => {
            format!("NOT ACCEPTED — {}", eula_action(dir, eula))
        }
        EulaStatus::Missing => {
            format!("MISSING — {}", eula_action(dir, eula))
        }
    };
    println!("  {EULA_FILE:<19}: {eula_line}");
    for (name, check) in &checks {
        let line = match check {
            Check::Ok { detail, .. } if *name == PROPERTIES_FILE => {
                format!("ok — {detail}; {props_rcon_note}")
            }
            Check::Ok { detail, .. } => format!("ok — {detail}"),
            Check::Missing => "missing — reads as the empty document".to_owned(),
            Check::Broken(e) => format!("ERROR — {e}"),
        };
        println!("  {name:<19}: {line}");
    }
    match code {
        0 => println!("status: READY — eula accepted, all server files healthy"),
        1 => println!("status: BROKEN — fix the file errors above, then re-run doctor"),
        _ => println!("status: NOT READY — {}", eula_action(dir, eula)),
    }
    Ok(code)
}

// -------------------------------------------------------------- admin ops

fn load_directory(dir: &Path, target: TargetVersion) -> Result<ServerDirectory, CliError> {
    ServerDirectory::load_or_init(dir, target)
        .map_err(|e| fail(format!("cannot load {}: {e}", dir.display())))
}

fn save_directory(sd: &ServerDirectory) -> Result<(), CliError> {
    sd.save_all_atomic()
        .map_err(|e| fail(format!("cannot save {}: {e}", sd.root().display())))
}

/// `changed` gates the write: a no-op admin command leaves the files —
/// including an operator's hand formatting — untouched.
fn maybe_save(sd: &ServerDirectory, changed: bool) -> Result<(), CliError> {
    if changed {
        save_directory(sd)?;
    }
    Ok(())
}

fn report(json: bool, changed: bool, doc: Vec<(&str, Json)>, human: String) {
    if json {
        emit(jobj(doc));
    } else {
        let suffix = if changed { "" } else { " (no change)" };
        println!("{human}{suffix}");
    }
}

fn cmd_op(
    dir: &Path,
    target: TargetVersion,
    player: &str,
    level: Option<u8>,
    bypass_limit: bool,
    json: bool,
) -> Result<(), CliError> {
    let mut sd = load_directory(dir, target)?;
    // On an existing operator, omitted flags preserve the stored values —
    // only what was explicitly passed changes.
    let existing = sd
        .ops
        .entries
        .iter()
        .find(|e| e.name.eq_ignore_ascii_case(player));
    let level = match (level, existing) {
        (Some(level), _) => level,
        (None, Some(entry)) => entry.level,
        (None, None) => sd
            .properties
            .doc()
            .get("op-permission-level")
            .and_then(|v| v.trim().parse::<u8>().ok())
            .filter(|n| (1..=4).contains(n))
            .unwrap_or(4), // Vanilla's default op-permission-level
    };
    let bypass = bypass_limit || existing.map(|e| e.bypasses_player_limit).unwrap_or(false);
    let changed = sd.ops.set(player, level, bypass);
    maybe_save(&sd, changed)?;
    report(
        json,
        changed,
        vec![
            ("action", jstr("op")),
            ("dir", jstr(dir.display().to_string())),
            ("player", jstr(player)),
            ("level", jint(i64::from(level))),
            ("bypasses_player_limit", Json::Bool(bypass)),
            ("changed", Json::Bool(changed)),
        ],
        format!(
            "op: {player} is now an operator (level {level}, bypasses player limit: {bypass})"
        ),
    );
    Ok(())
}

fn cmd_deop(dir: &Path, target: TargetVersion, player: &str, json: bool) -> Result<(), CliError> {
    let mut sd = load_directory(dir, target)?;
    let changed = sd.ops.remove(player);
    maybe_save(&sd, changed)?;
    report(
        json,
        changed,
        vec![
            ("action", jstr("deop")),
            ("dir", jstr(dir.display().to_string())),
            ("player", jstr(player)),
            ("removed", Json::Bool(changed)),
        ],
        format!("deop: {player} is no longer an operator"),
    );
    Ok(())
}

fn cmd_ban(
    dir: &Path,
    target: TargetVersion,
    player: &str,
    reason: &str,
    created: &str,
    source: &str,
    json: bool,
) -> Result<(), CliError> {
    let mut sd = load_directory(dir, target)?;
    let changed = sd.banned_players.ban(player, reason, source, created);
    maybe_save(&sd, changed)?;
    report(
        json,
        changed,
        vec![
            ("action", jstr("ban")),
            ("dir", jstr(dir.display().to_string())),
            ("player", jstr(player)),
            ("reason", jstr(reason)),
            ("source", jstr(source)),
            ("created", jstr(created)),
            ("changed", Json::Bool(changed)),
        ],
        format!("ban: {player} is banned (reason: {reason})"),
    );
    Ok(())
}

fn cmd_pardon(dir: &Path, target: TargetVersion, player: &str, json: bool) -> Result<(), CliError> {
    let mut sd = load_directory(dir, target)?;
    let changed = sd.banned_players.pardon(player);
    maybe_save(&sd, changed)?;
    report(
        json,
        changed,
        vec![
            ("action", jstr("pardon")),
            ("dir", jstr(dir.display().to_string())),
            ("player", jstr(player)),
            ("removed", Json::Bool(changed)),
        ],
        format!("pardon: {player} is no longer banned"),
    );
    Ok(())
}

fn cmd_whitelist(
    dir: &Path,
    target: TargetVersion,
    action: &WhitelistAction,
    json: bool,
) -> Result<(), CliError> {
    let mut sd = load_directory(dir, target)?;
    match action {
        WhitelistAction::List => {
            if json {
                emit(jobj(vec![
                    ("action", jstr("whitelist")),
                    ("subaction", jstr("list")),
                    ("dir", jstr(dir.display().to_string())),
                    ("enabled", Json::Bool(sd.properties.white_list())),
                    (
                        "entries",
                        jlist(sd
                            .whitelist
                            .entries
                            .iter()
                            .map(|e| {
                                jobj(vec![
                                    ("uuid", jstr(e.uuid.clone())),
                                    ("name", jstr(e.name.clone())),
                                ])
                            })
                            .collect()),
                    ),
                ]));
            } else {
                println!(
                    "whitelist: {} ({} entr{})",
                    if sd.properties.white_list() {
                        "enabled"
                    } else {
                        "disabled"
                    },
                    sd.whitelist.entries.len(),
                    if sd.whitelist.entries.len() == 1 {
                        "y"
                    } else {
                        "ies"
                    },
                );
                for entry in &sd.whitelist.entries {
                    println!("  {}", entry.name);
                }
            }
            Ok(())
        }
        WhitelistAction::On | WhitelistAction::Off => {
            let on = matches!(action, WhitelistAction::On);
            // `enforce-whitelist` outranks `white-list` whenever the key is
            // present in the file, so the switch writes both — the gate
            // follows this command no matter which key the document carries.
            let changed = sd.properties.white_list() != on
                || sd.properties.enforce_whitelist() != on;
            if changed {
                sd.properties.set_white_list(on);
                sd.properties.set_enforce_whitelist(on);
            }
            maybe_save(&sd, changed)?;
            report(
                json,
                changed,
                vec![
                    ("action", jstr("whitelist")),
                    ("subaction", jstr(if on { "on" } else { "off" })),
                    ("dir", jstr(dir.display().to_string())),
                    ("white_list", Json::Bool(on)),
                    ("changed", Json::Bool(changed)),
                ],
                format!(
                    "whitelist: enforcement {}",
                    if on { "enabled" } else { "disabled" }
                ),
            );
            Ok(())
        }
        WhitelistAction::Add(player) | WhitelistAction::Remove(player) => {
            let add = matches!(action, WhitelistAction::Add(_));
            let changed = if add {
                sd.whitelist.add(player)
            } else {
                sd.whitelist.remove(player)
            };
            maybe_save(&sd, changed)?;
            report(
                json,
                changed,
                vec![
                    ("action", jstr("whitelist")),
                    ("subaction", jstr(if add { "add" } else { "remove" })),
                    ("dir", jstr(dir.display().to_string())),
                    ("player", jstr(player)),
                    ("changed", Json::Bool(changed)),
                    ("enabled", Json::Bool(sd.properties.white_list())),
                ],
                format!(
                    "whitelist: {player} {}",
                    if add {
                        "added"
                    } else {
                        "removed"
                    }
                ),
            );
            Ok(())
        }
    }
}

// ------------------------------------------------------------------ claims

/// The offline claims store lives beside the admin files but is *not* part
/// of [`ServerDirectory`]: these actions read and write only
/// `openpac_claims.json` / `openpac_parties.json`, so `eula.txt` and the
/// admin registries are never touched.
fn load_claims(dir: &Path) -> Result<ClaimsWorld, CliError> {
    ClaimsWorld::load_from_dir(dir)
        .map_err(|e| fail(format!("cannot load claims from {}: {e}", dir.display())))
}

fn save_claims(world: &ClaimsWorld, dir: &Path) -> Result<(), CliError> {
    world
        .save_to_dir(dir)
        .map_err(|e| fail(format!("cannot save claims into {}: {e}", dir.display())))
}

/// The `owner_key` spelling the persistence layer uses internally.
fn claims_owner_label(owner: &ClaimOwner) -> String {
    match owner {
        ClaimOwner::Player(u) => format!("player:{u}"),
        ClaimOwner::Server => "server".to_owned(),
        ClaimOwner::Expired(u) => format!("expired:{u}"),
    }
}

fn claim_key(action: &str, dim: &str, cx: i32, cz: i32) -> Result<ClaimChunkKey, CliError> {
    ClaimChunkKey::new(dim, cx, cz).map_err(|e| usage(format!("claims {action}: {e}")))
}

fn cmd_claims(dir: &Path, action: &ClaimsAction, json: bool) -> Result<(), CliError> {
    match action {
        ClaimsAction::List { dimension } => {
            let world = load_claims(dir)?;
            let rows: Vec<(&ClaimChunkKey, &ChunkClaim)> = world
                .claims()
                .iter()
                .filter(|(k, _)| {
                    dimension
                        .as_deref()
                        .is_none_or(|d| k.dimension.as_str() == d)
                })
                .collect();
            let forceloads = rows.iter().filter(|(_, c)| c.forceloaded).count();
            if json {
                emit(jobj(vec![
                    ("action", jstr("claims")),
                    ("subaction", jstr("list")),
                    ("dir", jstr(dir.display().to_string())),
                    (
                        "dimension",
                        dimension
                            .as_deref()
                            .map(jstr)
                            .unwrap_or(Json::Null),
                    ),
                    (
                        "claims",
                        jlist(rows
                            .iter()
                            .map(|(k, c)| {
                                jobj(vec![
                                    ("dimension", jstr(k.dimension.clone())),
                                    ("cx", jint(i64::from(k.cx))),
                                    ("cz", jint(i64::from(k.cz))),
                                    ("owner", jstr(claims_owner_label(&c.owner))),
                                    ("sub_claim", jstr(c.sub_claim.clone())),
                                    ("forceloaded", Json::Bool(c.forceloaded)),
                                    ("color", jstr(format!("#{:06x}", c.color_rgb))),
                                ])
                            })
                            .collect()),
                    ),
                    ("claim_count", jint(rows.len() as i64)),
                    ("forceload_count", jint(forceloads as i64)),
                ]));
            } else {
                let scope = dimension
                    .as_deref()
                    .map(|d| format!(" — dimension {d}"))
                    .unwrap_or_default();
                println!("claims: {}{scope}", dir.display());
                for (k, c) in &rows {
                    println!(
                        "  {k}: owner={} sub={} forceloaded={} color=#{:06x}",
                        claims_owner_label(&c.owner),
                        c.sub_claim,
                        c.forceloaded,
                        c.color_rgb,
                    );
                }
                println!(
                    "  {} claim{}, {} forceloaded",
                    rows.len(),
                    if rows.len() == 1 { "" } else { "s" },
                    forceloads,
                );
            }
            Ok(())
        }
        ClaimsAction::Parties => {
            let world = load_claims(dir)?;
            let parties: Vec<&Party> = world.parties().values().collect();
            if json {
                emit(jobj(vec![
                    ("action", jstr("claims")),
                    ("subaction", jstr("parties")),
                    ("dir", jstr(dir.display().to_string())),
                    (
                        "parties",
                        jlist(parties
                            .iter()
                            .map(|p| {
                                jobj(vec![
                                    ("id", jstr(p.id.clone())),
                                    ("name", jstr(p.name.clone())),
                                    ("owner", jstr(p.owner_uuid.clone())),
                                    (
                                        "members",
                                        jlist(p
                                            .members
                                            .iter()
                                            .map(|(u, r)| {
                                                jobj(vec![
                                                    ("uuid", jstr(u.clone())),
                                                    ("rank", jstr(r.as_str())),
                                                ])
                                            })
                                            .collect()),
                                    ),
                                    (
                                        "allies",
                                        jlist(p
                                            .allies
                                            .iter()
                                            .map(|a| jstr(a.clone()))
                                            .collect()),
                                    ),
                                    (
                                        "invites",
                                        jlist(p
                                            .pending_invites
                                            .iter()
                                            .map(|a| jstr(a.clone()))
                                            .collect()),
                                    ),
                                    ("color", jstr(format!("#{:06x}", p.color_rgb))),
                                ])
                            })
                            .collect()),
                    ),
                    ("party_count", jint(parties.len() as i64)),
                ]));
            } else {
                println!("parties: {}", dir.display());
                for p in &parties {
                    let members: Vec<String> = p
                        .members
                        .iter()
                        .map(|(u, r)| format!("{u}={}", r.as_str()))
                        .collect();
                    let allies: Vec<&str> = p.allies.iter().map(String::as_str).collect();
                    let invites: Vec<&str> =
                        p.pending_invites.iter().map(String::as_str).collect();
                    println!(
                        "  {} {:?} color=#{:06x}: members=[{}] allies=[{}] invites=[{}]",
                        p.id,
                        p.name,
                        p.color_rgb,
                        members.join(","),
                        allies.join(","),
                        invites.join(","),
                    );
                }
                println!(
                    "  {} part{}",
                    parties.len(),
                    if parties.len() == 1 { "y" } else { "ies" },
                );
            }
            Ok(())
        }
        ClaimsAction::ServerClaim {
            dimension,
            cx,
            cz,
            forceload,
        } => {
            let key = claim_key("server-claim", dimension, *cx, *cz)?;
            let mut world = load_claims(dir)?;
            world
                .claim_server_chunk(true, key.clone(), &SubClaimId::main())
                .map_err(|e| fail(format!("claims server-claim: {e}")))?;
            if *forceload {
                world
                    .set_forceload_as("", true, &key, true)
                    .map_err(|e| fail(format!("claims server-claim --forceload: {e}")))?;
            }
            save_claims(&world, dir)?;
            report(
                json,
                true,
                vec![
                    ("action", jstr("claims")),
                    ("subaction", jstr("server-claim")),
                    ("dir", jstr(dir.display().to_string())),
                    ("dimension", jstr(key.dimension.clone())),
                    ("cx", jint(i64::from(key.cx))),
                    ("cz", jint(i64::from(key.cz))),
                    ("forceloaded", Json::Bool(*forceload)),
                    ("changed", Json::Bool(true)),
                ],
                format!(
                    "claims: server-claimed {key}{}",
                    if *forceload { " (forceloaded)" } else { "" },
                ),
            );
            Ok(())
        }
        ClaimsAction::Unclaim { dimension, cx, cz } => {
            let key = claim_key("unclaim", dimension, *cx, *cz)?;
            let mut world = load_claims(dir)?;
            world
                .unclaim_chunk_as("", true, &key)
                .map_err(|e| fail(format!("claims unclaim: {e}")))?;
            save_claims(&world, dir)?;
            report(
                json,
                true,
                vec![
                    ("action", jstr("claims")),
                    ("subaction", jstr("unclaim")),
                    ("dir", jstr(dir.display().to_string())),
                    ("dimension", jstr(key.dimension.clone())),
                    ("cx", jint(i64::from(key.cx))),
                    ("cz", jint(i64::from(key.cz))),
                    ("removed", Json::Bool(true)),
                ],
                format!("claims: unclaimed {key}"),
            );
            Ok(())
        }
    }
}

// --------------------------------------------------------------------- main

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let command = match parse_command(&args) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("mc-spine-init: {}", e.message);
            return ExitCode::from(e.code);
        }
    };
    let result = match command {
        Command::Help => {
            print!("{HELP}");
            return ExitCode::SUCCESS;
        }
        Command::Init {
            dir,
            target,
            port,
            motd,
            online_mode,
            max_players,
            json,
        } => cmd_init(&dir, target, port, motd.as_deref(), online_mode, max_players, json)
            .map(|()| 0u8),
        Command::Doctor { dir, target, json } => cmd_doctor(&dir, target, json),
        Command::Op {
            dir,
            target,
            player,
            level,
            bypass_limit,
            json,
        } => cmd_op(&dir, target, &player, level, bypass_limit, json).map(|()| 0u8),
        Command::Deop {
            dir,
            target,
            player,
            json,
        } => cmd_deop(&dir, target, &player, json).map(|()| 0u8),
        Command::Ban {
            dir,
            target,
            player,
            reason,
            created,
            source,
            json,
        } => cmd_ban(&dir, target, &player, &reason, &created, &source, json).map(|()| 0u8),
        Command::Pardon {
            dir,
            target,
            player,
            json,
        } => cmd_pardon(&dir, target, &player, json).map(|()| 0u8),
        Command::Whitelist {
            dir,
            target,
            action,
            json,
        } => cmd_whitelist(&dir, target, &action, json).map(|()| 0u8),
        Command::Claims { dir, action, json } => cmd_claims(&dir, &action, json).map(|()| 0u8),
    };
    match result {
        Ok(code) => ExitCode::from(code),
        Err(e) => {
            eprintln!("mc-spine-init: {}", e.message);
            ExitCode::from(e.code)
        }
    }
}
