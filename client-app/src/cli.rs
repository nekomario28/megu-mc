// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
//! Standard Minecraft 1.21.1 launcher CLI parser and credential redactor.

use std::fmt;
use std::path::PathBuf;

pub const CLIENT_VERSION: &str = env!("CARGO_PKG_VERSION");
pub const MINECRAFT_VERSION: &str = "1.21.1";
pub const PROTOCOL_VERSION: u32 = 767;

const TOKEN_FLAGS: &[&str] = &[
    "--accessToken",
    "--clientToken",
    "--authToken",
    "--token",
    "--xuid",
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScriptMode {
    Walk,
    Jump,
    Sprint,
    Idle,
    Survey,
}

impl ScriptMode {
    pub fn parse(value: &str) -> Result<Self, String> {
        match value.to_ascii_lowercase().as_str() {
            "walk" => Ok(Self::Walk),
            "jump" => Ok(Self::Jump),
            "sprint" => Ok(Self::Sprint),
            "idle" => Ok(Self::Idle),
            "survey" => Ok(Self::Survey),
            other => Err(format!(
                "invalid --script '{other}' (expected walk, jump, sprint, idle, or survey)"
            )),
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Walk => "walk",
            Self::Jump => "jump",
            Self::Sprint => "sprint",
            Self::Idle => "idle",
            Self::Survey => "survey",
        }
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct CliConfig {
    // Standard Minecraft 1.21.1 launcher arguments
    pub username: String,
    pub uuid: Option<String>,
    access_token: Option<String>,
    client_token: Option<String>,
    pub client_id: Option<String>,
    xuid: Option<String>,
    pub user_type: Option<String>,
    pub version: Option<String>,
    pub version_type: Option<String>,
    pub game_dir: Option<PathBuf>,
    pub assets_dir: Option<PathBuf>,
    pub asset_index: Option<String>,
    pub quick_play_multiplayer: Option<String>,
    pub quick_play_singleplayer: Option<String>,
    pub server: Option<String>,
    pub port: u16,
    pub server_bin: Option<PathBuf>,
    pub seed: Option<i64>,

    // Native mc-spine-client flags
    pub headless: bool,
    pub ticks: u32,
    pub view_distance: i8,
    pub assets_json: Option<PathBuf>,
    pub script: ScriptMode,
    pub json_summary: bool,
    pub render_png: Option<PathBuf>,
    pub render_width: u32,
    pub render_height: u32,
    pub show_help: bool,
    pub show_version_info: bool,

    // All secret values collected from argv for sanitization of any diagnostic output.
    secrets: Vec<String>,
}

impl Default for CliConfig {
    fn default() -> Self {
        Self {
            username: "SpinePlayer".to_string(),
            uuid: None,
            access_token: None,
            client_token: None,
            client_id: None,
            xuid: None,
            user_type: None,
            version: None,
            version_type: None,
            game_dir: None,
            assets_dir: None,
            asset_index: None,
            quick_play_multiplayer: None,
            quick_play_singleplayer: None,
            server: None,
            port: 25565,
            server_bin: None,
            seed: None,
            headless: false,
            ticks: 40,
            view_distance: 2,
            assets_json: None,
            script: ScriptMode::Idle,
            json_summary: false,
            render_png: None,
            render_width: 320,
            render_height: 180,
            show_help: false,
            show_version_info: false,
            secrets: Vec::new(),
        }
    }
}

impl fmt::Debug for CliConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CliConfig")
            .field("username", &self.username)
            .field("uuid", &self.uuid)
            .field(
                "access_token",
                &self.access_token.as_ref().map(|_| "<REDACTED>"),
            )
            .field(
                "client_token",
                &self.client_token.as_ref().map(|_| "<REDACTED>"),
            )
            .field("client_id", &self.client_id)
            .field("xuid", &self.xuid.as_ref().map(|_| "<REDACTED>"))
            .field("user_type", &self.user_type)
            .field("version", &self.version)
            .field("version_type", &self.version_type)
            .field("game_dir", &self.game_dir)
            .field("assets_dir", &self.assets_dir)
            .field("asset_index", &self.asset_index)
            .field("quick_play_multiplayer", &self.quick_play_multiplayer)
            .field("quick_play_singleplayer", &self.quick_play_singleplayer)
            .field("server", &self.server)
            .field("port", &self.port)
            .field("server_bin", &self.server_bin)
            .field("seed", &self.seed)
            .field("headless", &self.headless)
            .field("ticks", &self.ticks)
            .field("view_distance", &self.view_distance)
            .field("assets_json", &self.assets_json)
            .field("script", &self.script)
            .field("json_summary", &self.json_summary)
            .field("render_png", &self.render_png)
            .field("render_width", &self.render_width)
            .field("render_height", &self.render_height)
            .finish()
    }
}

impl CliConfig {
    pub fn has_access_token(&self) -> bool {
        self.access_token.is_some()
    }

    pub fn has_xuid(&self) -> bool {
        self.xuid.is_some()
    }

    /// Resolve the target multiplayer endpoint `(host, port)`, if configured.
    pub fn target_endpoint(&self) -> Result<Option<(String, u16)>, String> {
        if let Some(ref endpoint) = self.quick_play_multiplayer {
            return parse_host_port(endpoint, self.port).map(Some);
        }
        if let Some(ref host) = self.server {
            return parse_host_port(host, self.port).map(Some);
        }
        Ok(None)
    }

    /// Redact any known secret tokens from an arbitrary diagnostic message.
    pub fn redact(&self, message: &str) -> String {
        redact_with_secrets(message, &self.secrets)
    }
}

pub fn parse_host_port(input: &str, default_port: u16) -> Result<(String, u16), String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err("empty server address".to_string());
    }
    if let Some(rest) = trimmed.strip_prefix('[') {
        let Some((host, tail)) = rest.split_once(']') else {
            return Err(format!("unclosed IPv6 bracket in '{trimmed}'"));
        };
        if host.is_empty() {
            return Err("empty IPv6 host".to_string());
        }
        if tail.is_empty() {
            return Ok((host.to_string(), default_port));
        }
        let port_str = tail
            .strip_prefix(':')
            .ok_or_else(|| format!("invalid port separator in '{trimmed}'"))?;
        let port = port_str
            .parse::<u16>()
            .map_err(|_| format!("invalid port '{port_str}'"))?;
        return Ok((host.to_string(), port));
    }
    if let Some((host, port_str)) = trimmed.rsplit_once(':') {
        if !host.contains(':') {
            if host.is_empty() {
                return Err("empty server host".to_string());
            }
            let port = port_str
                .parse::<u16>()
                .map_err(|_| format!("invalid port '{port_str}'"))?;
            return Ok((host.to_string(), port));
        }
    }
    Ok((trimmed.to_string(), default_port))
}

/// Collect secret token values from raw CLI arguments so errors can always be sanitized.
pub fn collect_secrets<S: AsRef<str>>(args: &[S]) -> Vec<String> {
    let mut secrets = Vec::new();
    let mut i = 0;
    while i < args.len() {
        let arg = args[i].as_ref();
        if let Some((key, value)) = arg.split_once('=') {
            if TOKEN_FLAGS.contains(&key) && !value.is_empty() {
                secrets.push(value.to_string());
            }
        } else if TOKEN_FLAGS.contains(&arg) {
            if let Some(next) = args.get(i + 1) {
                let value = next.as_ref();
                if !value.is_empty() {
                    secrets.push(value.to_string());
                }
            }
            i += 1;
        }
        i += 1;
    }
    secrets.sort_by_key(|s| std::cmp::Reverse(s.len()));
    secrets.dedup();
    secrets
}

/// Redact explicit secret values and any `Bearer <token>` substrings.
pub fn redact_with_secrets(message: &str, secrets: &[String]) -> String {
    let mut out = message.to_string();
    for secret in secrets {
        if !secret.is_empty() {
            out = out.replace(secret, "<REDACTED>");
        }
    }
    redact_bearer_tokens(&out)
}

pub fn redact_from_argv<S: AsRef<str>>(message: &str, argv: &[S]) -> String {
    let secrets = collect_secrets(argv);
    redact_with_secrets(message, &secrets)
}

fn redact_bearer_tokens(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = String::with_capacity(input.len());
    let mut i = 0;
    while i < bytes.len() {
        let at_word_boundary = i == 0 || !bytes[i - 1].is_ascii_alphanumeric();
        if at_word_boundary
            && i + 6 < bytes.len()
            && bytes[i..i + 6].eq_ignore_ascii_case(b"bearer")
            && bytes[i + 6].is_ascii_whitespace()
        {
            out.push_str("Bearer ");
            i += 6;
            while i < bytes.len() && bytes[i].is_ascii_whitespace() {
                i += 1;
            }
            while i < bytes.len()
                && !bytes[i].is_ascii_whitespace()
                && !matches!(bytes[i], b',' | b';' | b'"' | b'\'')
            {
                i += 1;
            }
            out.push_str("<REDACTED>");
        } else {
            let ch = input[i..].chars().next().unwrap();
            out.push(ch);
            i += ch.len_utf8();
        }
    }
    out
}

pub fn parse_args<S: AsRef<str>>(args: &[S]) -> Result<CliConfig, String> {
    let secrets = collect_secrets(args);
    parse_args_inner(args, secrets.clone()).map_err(|err| redact_with_secrets(&err, &secrets))
}

fn parse_args_inner<S: AsRef<str>>(args: &[S], secrets: Vec<String>) -> Result<CliConfig, String> {
    let mut cfg = CliConfig {
        secrets,
        ..CliConfig::default()
    };

    let mut i = 0;
    while i < args.len() {
        let raw = args[i].as_ref();
        let (flag, inline_val) = match raw.split_once('=') {
            Some((k, v)) if k.starts_with('-') => (k, Some(v)),
            _ => (raw, None),
        };

        let take_val = |flag_name: &str, idx: &mut usize| -> Result<String, String> {
            if let Some(v) = inline_val {
                return Ok(v.to_string());
            }
            *idx += 1;
            args.get(*idx)
                .map(|s| s.as_ref().to_string())
                .ok_or_else(|| format!("missing value for {flag_name}"))
        };

        match flag {
            "-h" | "--help" => cfg.show_help = true,
            "--version-info" => cfg.show_version_info = true,
            "--headless" => cfg.headless = true,
            "--json-summary" => cfg.json_summary = true,
            "--username" => cfg.username = take_val(flag, &mut i)?,
            "--uuid" => cfg.uuid = Some(take_val(flag, &mut i)?),
            "--accessToken" => cfg.access_token = Some(take_val(flag, &mut i)?),
            "--clientToken" | "--authToken" | "--token" => {
                cfg.client_token = Some(take_val(flag, &mut i)?);
            }
            "--clientId" => cfg.client_id = Some(take_val(flag, &mut i)?),
            "--xuid" => cfg.xuid = Some(take_val(flag, &mut i)?),
            "--userType" => cfg.user_type = Some(take_val(flag, &mut i)?),
            "--version" => cfg.version = Some(take_val(flag, &mut i)?),
            "--versionType" => cfg.version_type = Some(take_val(flag, &mut i)?),
            "--gameDir" => cfg.game_dir = Some(PathBuf::from(take_val(flag, &mut i)?)),
            "--assetsDir" => cfg.assets_dir = Some(PathBuf::from(take_val(flag, &mut i)?)),
            "--assetIndex" => cfg.asset_index = Some(take_val(flag, &mut i)?),
            "--quickPlayMultiplayer" => {
                cfg.quick_play_multiplayer = Some(take_val(flag, &mut i)?);
            }
            "--quickPlaySingleplayer" => {
                cfg.quick_play_singleplayer = Some(take_val(flag, &mut i)?);
            }
            "--server" => cfg.server = Some(take_val(flag, &mut i)?),
            "--server-bin" => cfg.server_bin = Some(PathBuf::from(take_val(flag, &mut i)?)),
            "--seed" => {
                let raw_seed = take_val(flag, &mut i)?;
                cfg.seed = Some(
                    raw_seed
                        .parse::<i64>()
                        .map_err(|_| format!("invalid --seed '{raw_seed}'"))?,
                );
            }
            "--port" => {
                let raw_port = take_val(flag, &mut i)?;
                cfg.port = raw_port
                    .parse::<u16>()
                    .map_err(|_| format!("invalid --port '{raw_port}'"))?;
            }
            "--ticks" => {
                let raw_ticks = take_val(flag, &mut i)?;
                cfg.ticks = raw_ticks
                    .parse::<u32>()
                    .map_err(|_| format!("invalid --ticks '{raw_ticks}'"))?;
            }
            "--view-distance" => {
                let raw_vd = take_val(flag, &mut i)?;
                let parsed = raw_vd
                    .parse::<i32>()
                    .map_err(|_| format!("invalid --view-distance '{raw_vd}'"))?;
                if !(2..=32).contains(&parsed) {
                    return Err(format!("--view-distance must be in 2..=32 (got {parsed})"));
                }
                cfg.view_distance = parsed as i8;
            }
            "--assets-json" => cfg.assets_json = Some(PathBuf::from(take_val(flag, &mut i)?)),
            "--script" => {
                let raw_script = take_val(flag, &mut i)?;
                cfg.script = ScriptMode::parse(&raw_script)?;
            }
            "--render-png" => cfg.render_png = Some(PathBuf::from(take_val(flag, &mut i)?)),
            "--width" => {
                let raw_w = take_val(flag, &mut i)?;
                let parsed = raw_w
                    .parse::<u32>()
                    .map_err(|_| format!("invalid --width '{raw_w}'"))?;
                cfg.render_width = parsed.clamp(64, 1920);
            }
            "--height" => {
                let raw_h = take_val(flag, &mut i)?;
                let parsed = raw_h
                    .parse::<u32>()
                    .map_err(|_| format!("invalid --height '{raw_h}'"))?;
                cfg.render_height = parsed.clamp(64, 1080);
            }
            // Standard optional Minecraft launcher flags that take no value.
            "--demo" | "--fullscreen" | "--disableMultiplayer" | "--disableChat" => {}
            // Standard optional Minecraft launcher flags that take a single value.
            "--userProperties"
            | "--profileProperties"
            | "--quickPlayPath"
            | "--quickPlayRealms"
            | "--resourcePackDir"
            | "--proxyHost"
            | "--proxyPort"
            | "--proxyUser"
            | "--proxyPass" => {
                let _ = take_val(flag, &mut i)?;
            }
            other => {
                return Err(format!("unrecognized argument '{other}'"));
            }
        }
        i += 1;
    }

    Ok(cfg)
}

pub fn help_text() -> String {
    format!(
        "\
mc-spine-client {CLIENT_VERSION} (Minecraft {MINECRAFT_VERSION}, Protocol {PROTOCOL_VERSION})
Native zero-dependency Rust client combining mc-spine-kernel and mc-spine-client-mesh.

USAGE:
  mc-spine-client [LAUNCHER FLAGS] [NATIVE FLAGS]

LAUNCHER COMPATIBILITY FLAGS:
  --username <name>                  Player name (default: SpinePlayer)
  --uuid <uuid>                      Player UUID (defaults to offline v3 UUID)
  --accessToken <token>              Launcher auth token (never logged; redacted)
  --clientId <id>                    Launcher client ID
  --xuid <id>                        Xbox UID (never logged; redacted)
  --userType <type>                  Account type (mojang/msa/legacy)
  --version <ver>                    Minecraft version string
  --versionType <type>               Launcher version type
  --gameDir <path>                   Minecraft game directory
  --assetsDir <path>                 Minecraft assets directory
  --assetIndex <id>                  Asset index identifier
  --quickPlayMultiplayer <host[:p]>  Connect directly to multiplayer server
  --quickPlaySingleplayer <world>    Launch integrated mc-spine server for world
  --server <host>                    Server host (alternative to quickPlayMultiplayer)
  --port <port>                      Server port (default: 25565)
  --server-bin <path>                mc-spine server binary for --quickPlaySingleplayer
  --seed <seed>                      World seed for a new --quickPlaySingleplayer world

NATIVE CLIENT FLAGS:
  --headless                         Run headless protocol + meshing + physics session
  --ticks <N>                        Number of simulation ticks to run (default: 40)
  --view-distance <N>                Client view distance in chunks, 2..=32 (default: 2)
  --assets-json <path>               Path to assets/block_states_1211.json
  --script <mode>                    Scripted input: walk|jump|sprint|idle|survey
  --json-summary                     Emit machine-readable JSON summary on stdout
  --render-png <path>                Render final camera frame to a PNG screenshot
  --width <px>                       Render width, clamped 64..=1920 (default: 320)
  --height <px>                      Render height, clamped 64..=1080 (default: 180)
  --version-info                     Print version, protocol, and ABI info
  -h, --help                         Print this help message
"
    )
}

pub fn version_info_text() -> String {
    format!(
        "mc-spine-client {CLIENT_VERSION} (minecraft={MINECRAFT_VERSION}, protocol={PROTOCOL_VERSION}, kernel_abi={}, mesh_abi={})",
        mc_spine_kernel::ABI_VERSION,
        mc_spine_client_mesh::ffi::ABI_VERSION,
    )
}
