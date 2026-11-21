// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
//! Standard Minecraft 1.21.1 launcher CLI parser, credential redactor, and
//! player-settings source resolution.

use std::fmt;
use std::path::{Path, PathBuf};

use crate::settings::{
    ClientSettings, CloudMode, DistantHorizon, ParticleLevel, QualityPreset,
    MAX_FOV, MAX_MAX_FPS, MAX_VIEW_DISTANCE, MIN_FOV, MIN_MAX_FPS, MIN_VIEW_DISTANCE,
    SETTINGS_FILE_NAME,
};

pub const CLIENT_VERSION: &str = env!("CARGO_PKG_VERSION");
pub const MINECRAFT_VERSION: &str = "1.21.1";
pub const PROTOCOL_VERSION: u32 = mc_spine_proto::ProtocolVersion::V1_21_1.number() as u32;

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

    // Player graphics settings: file/preset/import sources and write-out paths
    pub settings_path: Option<PathBuf>,
    pub preset: Option<QualityPreset>,
    pub import_vanilla_options: Option<PathBuf>,
    pub write_settings: Option<PathBuf>,
    pub write_default_settings: Option<PathBuf>,

    // Explicit per-setting overrides; `None` means the flag was not given.
    pub fov: Option<u8>,
    pub distant_horizon: Option<DistantHorizon>,
    pub clouds: Option<CloudMode>,
    pub particles: Option<ParticleLevel>,
    pub smooth_lighting: Option<bool>,
    pub entity_shadows: Option<bool>,
    pub vsync: Option<bool>,
    /// `Some(None)` records `--max-fps unlimited`.
    pub max_fps: Option<Option<u32>>,
    /// Whether `--view-distance` was given on the command line.
    pub view_distance_explicit: bool,

    /// Resolved player settings; populated by
    /// [`CliConfig::resolve_client_settings`].
    pub client_settings: ClientSettings,
    /// Whether any settings source (file, preset, or options.txt import)
    /// contributed values during resolution.
    pub settings_source_present: bool,

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
            settings_path: None,
            preset: None,
            import_vanilla_options: None,
            write_settings: None,
            write_default_settings: None,
            fov: None,
            distant_horizon: None,
            clouds: None,
            particles: None,
            smooth_lighting: None,
            entity_shadows: None,
            vsync: None,
            max_fps: None,
            view_distance_explicit: false,
            client_settings: ClientSettings::default(),
            settings_source_present: false,
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
            .field("settings_path", &self.settings_path)
            .field("preset", &self.preset)
            .field("import_vanilla_options", &self.import_vanilla_options)
            .field("write_settings", &self.write_settings)
            .field("write_default_settings", &self.write_default_settings)
            .field("fov", &self.fov)
            .field("distant_horizon", &self.distant_horizon)
            .field("clouds", &self.clouds)
            .field("particles", &self.particles)
            .field("smooth_lighting", &self.smooth_lighting)
            .field("entity_shadows", &self.entity_shadows)
            .field("vsync", &self.vsync)
            .field("max_fps", &self.max_fps)
            .field("view_distance_explicit", &self.view_distance_explicit)
            .field("client_settings", &self.client_settings)
            .field("settings_source_present", &self.settings_source_present)
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

    /// Whether the run requests a server or render action (as opposed to a
    /// settings-only invocation such as `--write-settings`).
    pub fn wants_session_action(&self) -> bool {
        self.quick_play_multiplayer.is_some()
            || self.quick_play_singleplayer.is_some()
            || self.server.is_some()
            || self.render_png.is_some()
    }

    /// Merge the configured settings sources into
    /// [`CliConfig::client_settings`] and fold the resolved view distance
    /// back into [`CliConfig::view_distance`] when `--view-distance` was not
    /// given explicitly.
    ///
    /// Precedence, lowest to highest: `--preset` base, `--settings` file (or
    /// `mc-spine-client.toml` in the working directory) keys,
    /// `--import-vanilla-options` keys, explicit per-flag overrides. Returns
    /// one plain-language warning per recoverable file problem; an
    /// unreadable explicit `--settings` or `--import-vanilla-options` path
    /// is a hard error.
    pub fn resolve_client_settings(&mut self) -> Result<Vec<String>, String> {
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        self.resolve_client_settings_from(&cwd)
    }

    /// [`CliConfig::resolve_client_settings`] with an explicit directory for
    /// the `mc-spine-client.toml` auto-load, kept separate so tests can
    /// point it at a scratch directory.
    pub fn resolve_client_settings_from(
        &mut self,
        auto_dir: &Path,
    ) -> Result<Vec<String>, String> {
        let mut warnings = Vec::new();
        let mut source_present = self.preset.is_some();
        let mut settings = self
            .preset
            .map(ClientSettings::from_preset)
            .unwrap_or_default();

        let file_path = self.settings_path.clone().or_else(|| {
            let auto = auto_dir.join(SETTINGS_FILE_NAME);
            auto.is_file().then_some(auto)
        });
        if let Some(path) = file_path {
            match std::fs::read_to_string(&path) {
                Ok(text) => {
                    for warning in settings.apply_toml(&text) {
                        warnings.push(format!("{}: {warning}", path.display()));
                    }
                    source_present = true;
                }
                Err(err) if self.settings_path.is_some() => {
                    return Err(format!("cannot read {}: {err}", path.display()));
                }
                Err(err) => {
                    warnings.push(format!("{}: cannot read file ({err})", path.display()));
                }
            }
        }

        if let Some(path) = self.import_vanilla_options.clone() {
            let text = std::fs::read_to_string(&path)
                .map_err(|err| format!("cannot read {}: {err}", path.display()))?;
            settings.apply_vanilla_options_txt(&text);
            source_present = true;
        }

        // Explicit flags always win over every file, preset, or import.
        let mut tuned = false;
        if self.view_distance_explicit {
            settings.view_distance = i64::from(self.view_distance)
                .clamp(i64::from(MIN_VIEW_DISTANCE), i64::from(MAX_VIEW_DISTANCE))
                as u8;
            tuned = true;
        }
        if let Some(value) = self.fov {
            settings.fov = value;
            tuned = true;
        }
        if let Some(value) = self.distant_horizon {
            settings.distant_horizon = value;
            tuned = true;
        }
        if let Some(value) = self.clouds {
            settings.clouds = value;
            tuned = true;
        }
        if let Some(value) = self.particles {
            settings.particles = value;
            tuned = true;
        }
        if let Some(value) = self.smooth_lighting {
            settings.smooth_lighting = value;
            tuned = true;
        }
        if let Some(value) = self.entity_shadows {
            settings.entity_shadows = value;
            tuned = true;
        }
        if let Some(value) = self.vsync {
            settings.vsync = value;
            tuned = true;
        }
        if let Some(value) = self.max_fps {
            settings.max_fps = value;
            tuned = true;
        }
        if tuned {
            settings.preset = QualityPreset::Custom;
        }

        self.settings_source_present = source_present;
        if source_present && !self.view_distance_explicit {
            self.view_distance = settings.view_distance as i8;
        }
        self.client_settings = settings;
        Ok(warnings)
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

/// Player-facing on/off flag value: accepts on/true/yes/1 and
/// off/false/no/0, anything else is an error naming the flag.
fn parse_on_off(value: &str, flag: &str) -> Result<bool, String> {
    if matches!(
        value.to_ascii_lowercase().as_str(),
        "on" | "true" | "yes" | "1"
    ) {
        Ok(true)
    } else if matches!(
        value.to_ascii_lowercase().as_str(),
        "off" | "false" | "no" | "0"
    ) {
        Ok(false)
    } else {
        Err(format!("{flag} must be 'on' or 'off' (got '{value}')"))
    }
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
                cfg.view_distance_explicit = true;
            }
            "--settings" => cfg.settings_path = Some(PathBuf::from(take_val(flag, &mut i)?)),
            "--preset" => {
                let raw_preset = take_val(flag, &mut i)?;
                cfg.preset = Some(QualityPreset::parse(&raw_preset).ok_or_else(|| {
                    format!(
                        "invalid --preset '{raw_preset}' (expected fast, balanced, high, or ultra)"
                    )
                })?);
            }
            "--import-vanilla-options" => {
                cfg.import_vanilla_options = Some(PathBuf::from(take_val(flag, &mut i)?));
            }
            "--write-settings" => {
                cfg.write_settings = Some(PathBuf::from(take_val(flag, &mut i)?));
            }
            "--write-default-settings" => {
                cfg.write_default_settings = Some(PathBuf::from(take_val(flag, &mut i)?));
            }
            "--fov" => {
                let raw_fov = take_val(flag, &mut i)?;
                let parsed = raw_fov
                    .parse::<i64>()
                    .map_err(|_| format!("invalid --fov '{raw_fov}'"))?;
                if !(i64::from(MIN_FOV)..=i64::from(MAX_FOV)).contains(&parsed) {
                    return Err(format!(
                        "--fov must be in {MIN_FOV}..={MAX_FOV} (got {parsed})"
                    ));
                }
                cfg.fov = Some(parsed as u8);
            }
            "--distant-horizon" => {
                let raw = take_val(flag, &mut i)?;
                cfg.distant_horizon = Some(DistantHorizon::parse(&raw).ok_or_else(|| {
                    format!("invalid --distant-horizon '{raw}' (expected off, medium, or far)")
                })?);
            }
            "--clouds" => {
                let raw = take_val(flag, &mut i)?;
                cfg.clouds = Some(CloudMode::parse(&raw).ok_or_else(|| {
                    format!("invalid --clouds '{raw}' (expected off, fast, or fancy)")
                })?);
            }
            "--particles" => {
                let raw = take_val(flag, &mut i)?;
                cfg.particles = Some(ParticleLevel::parse(&raw).ok_or_else(|| {
                    format!("invalid --particles '{raw}' (expected minimal, decreased, or all)")
                })?);
            }
            "--smooth-lighting" => {
                cfg.smooth_lighting = Some(parse_on_off(&take_val(flag, &mut i)?, flag)?);
            }
            "--entity-shadows" => {
                cfg.entity_shadows = Some(parse_on_off(&take_val(flag, &mut i)?, flag)?);
            }
            "--vsync" => {
                cfg.vsync = Some(parse_on_off(&take_val(flag, &mut i)?, flag)?);
            }
            "--max-fps" => {
                let raw = take_val(flag, &mut i)?;
                if matches!(raw.to_ascii_lowercase().as_str(), "unlimited" | "none" | "off") {
                    cfg.max_fps = Some(None);
                } else {
                    let parsed = raw
                        .parse::<i64>()
                        .map_err(|_| format!("invalid --max-fps '{raw}'"))?;
                    if !(i64::from(MIN_MAX_FPS)..=i64::from(MAX_MAX_FPS)).contains(&parsed) {
                        return Err(format!(
                            "--max-fps must be 'unlimited' or in {MIN_MAX_FPS}..={MAX_MAX_FPS} (got {parsed})"
                        ));
                    }
                    cfg.max_fps = Some(Some(parsed as u32));
                }
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

PLAYER SETTINGS:
  Settings resolve in this order: --preset first, then the settings file,
  then --import-vanilla-options, then any explicit flags below.
  --settings <path>                Load settings from a TOML file
                                   (default: ./mc-spine-client.toml when present)
  --preset <name>                  Start from a preset: fast|balanced|high|ultra
  --import-vanilla-options <path>  Import a Vanilla options.txt into the settings
  --write-settings <path>          Save the resolved settings to a TOML file
                                   (runs as usual when a server is also given)
  --write-default-settings <path>  Save the default settings to a TOML file
  --fov <deg>                      Field of view, 30..=110
  --distant-horizon <mode>         Far terrain beyond view distance: off|medium|far
  --clouds <mode>                  Clouds: off|fast|fancy
  --particles <level>              Particles: minimal|decreased|all
  --smooth-lighting <on|off>       Smooth lighting
  --entity-shadows <on|off>        Entity shadows
  --vsync <on|off>                 Vertical sync
  --max-fps <N|unlimited>          Frame rate cap, 30..=260
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::MAX_VIEW_DISTANCE;

    fn scratch_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "mc-spine-cli-test-{tag}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_file(dir: &Path, name: &str, text: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, text).unwrap();
        path
    }

    #[test]
    fn preset_then_file_then_cli_precedence() {
        let dir = scratch_dir("precedence");
        let file = write_file(
            &dir,
            "custom.toml",
            "view_distance = 20\nclouds = \"fancy\"\n",
        );
        let mut cfg = parse_args(&[
            "--preset",
            "fast",
            "--settings",
            file.to_str().unwrap(),
            "--fov",
            "95",
        ])
        .unwrap();
        let warnings = cfg.resolve_client_settings_from(&dir).unwrap();
        assert!(warnings.is_empty(), "unexpected warnings: {warnings:?}");

        let s = &cfg.client_settings;
        // The file beat the preset for the keys it defines.
        assert_eq!(s.view_distance, 20);
        assert_eq!(s.clouds, CloudMode::Fancy);
        // The preset still fills keys the file does not define.
        assert_eq!(s.particles, ParticleLevel::Decreased);
        assert!(!s.smooth_lighting);
        assert!(!s.entity_shadows);
        // The explicit flag beats everything and marks the preset Custom.
        assert_eq!(s.fov, 95);
        assert_eq!(s.preset, QualityPreset::Custom);
        // No explicit --view-distance: the resolved value drives the
        // session's requested chunk window.
        assert_eq!(cfg.view_distance, 20);
        assert!(cfg.settings_source_present);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn explicit_view_distance_beats_file_and_preset() {
        let dir = scratch_dir("vd-override");
        let file = write_file(&dir, "custom.toml", "view_distance = 30\n");
        let mut cfg = parse_args(&[
            "--preset",
            "high",
            "--settings",
            file.to_str().unwrap(),
            "--view-distance",
            "9",
        ])
        .unwrap();
        cfg.resolve_client_settings_from(&dir).unwrap();
        assert_eq!(cfg.view_distance, 9);
        assert_eq!(cfg.client_settings.view_distance, 9);
        // High's far-terrain tier survives the view-distance override.
        assert_eq!(cfg.client_settings.distant_horizon, DistantHorizon::Medium);
        assert_eq!(cfg.client_settings.preset, QualityPreset::Custom);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn auto_loads_mc_spine_client_toml_and_warns_on_bad_values() {
        let dir = scratch_dir("auto-load");
        write_file(
            &dir,
            SETTINGS_FILE_NAME,
            "view_distance = 99\nfov = 0\nbogus_key = 1\nmax_fps = \"unlimited\"\n",
        );
        let mut cfg = parse_args::<&str>(&[]).unwrap();
        let warnings = cfg.resolve_client_settings_from(&dir).unwrap();
        let s = &cfg.client_settings;
        assert_eq!(s.view_distance, MAX_VIEW_DISTANCE);
        assert_eq!(s.fov, MIN_FOV);
        assert_eq!(s.max_fps, None);
        assert_eq!(cfg.view_distance, i8::try_from(MAX_VIEW_DISTANCE).unwrap());
        assert!(cfg.settings_source_present);
        assert!(warnings.iter().any(|w| w.contains("clamped")));
        assert!(warnings.iter().any(|w| w.contains("bogus_key")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_settings_file_defaults_silently() {
        let dir = scratch_dir("no-file");
        let mut cfg = parse_args::<&str>(&[]).unwrap();
        let warnings = cfg.resolve_client_settings_from(&dir).unwrap();
        assert!(warnings.is_empty());
        assert_eq!(cfg.client_settings, ClientSettings::default());
        // No source => the session default view distance is untouched.
        assert_eq!(cfg.view_distance, 2);
        assert!(!cfg.settings_source_present);

        // An explicitly named file that does not exist is a hard error.
        let mut cfg = parse_args(&["--settings", "definitely-missing.toml"]).unwrap();
        let err = cfg.resolve_client_settings_from(&dir).unwrap_err();
        assert!(err.contains("cannot read"), "unexpected error: {err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn vanilla_options_import_then_toml_roundtrip() {
        let dir = scratch_dir("vanilla-import");
        let options = write_file(
            &dir,
            "options.txt",
            "version:3465\nao:2\nfov:0.5\nenableVsync:false\nmaxFps:144\n\
             renderClouds:\"fast\"\ngraphicsMode:1\nparticles:1\nentityShadows:false\n\
             renderDistance:16\n",
        );
        let out = dir.join("imported.toml");
        let mut cfg = parse_args(&[
            "--import-vanilla-options",
            options.to_str().unwrap(),
            "--write-settings",
            out.to_str().unwrap(),
        ])
        .unwrap();
        cfg.resolve_client_settings_from(&dir).unwrap();

        let s = &cfg.client_settings;
        assert_eq!(s.preset, QualityPreset::Custom);
        assert_eq!(s.view_distance, 16);
        assert_eq!(s.fov, 70);
        assert!(!s.vsync);
        assert_eq!(s.max_fps, Some(144));
        assert_eq!(s.clouds, CloudMode::Fast);
        assert_eq!(s.particles, ParticleLevel::Decreased);
        assert!(s.smooth_lighting);
        assert!(!s.entity_shadows);
        assert_eq!(cfg.view_distance, 16);
        // The write-out path is a settings-only action, not a session.
        assert!(!cfg.wants_session_action());

        s.write_toml(&out).unwrap();
        let (back, warnings) = ClientSettings::from_toml(&std::fs::read_to_string(&out).unwrap());
        assert_eq!(&back, s);
        assert!(warnings.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn graphics_flags_parse_and_override() {
        let mut cfg = parse_args(&[
            "--fov",
            "103",
            "--distant-horizon",
            "far",
            "--clouds",
            "off",
            "--particles",
            "minimal",
            "--smooth-lighting",
            "off",
            "--entity-shadows",
            "0",
            "--vsync",
            "no",
            "--max-fps",
            "120",
        ])
        .unwrap();
        cfg.resolve_client_settings_from(Path::new(".")).unwrap();
        let s = &cfg.client_settings;
        assert_eq!(s.fov, 103);
        assert_eq!(s.distant_horizon, DistantHorizon::Far);
        assert_eq!(s.clouds, CloudMode::Off);
        assert_eq!(s.particles, ParticleLevel::Minimal);
        assert!(!s.smooth_lighting);
        assert!(!s.entity_shadows);
        assert!(!s.vsync);
        assert_eq!(s.max_fps, Some(120));
        assert_eq!(s.preset, QualityPreset::Custom);

        let mut cfg = parse_args(&["--max-fps", "unlimited", "--preset", "ultra"]).unwrap();
        cfg.resolve_client_settings_from(Path::new(".")).unwrap();
        assert_eq!(cfg.client_settings.max_fps, None);
        assert_eq!(cfg.client_settings.view_distance, MAX_VIEW_DISTANCE);
    }

    #[test]
    fn invalid_flag_values_error_out() {
        for argv in [
            vec!["--preset", "ludicrous"],
            vec!["--fov", "abc"],
            vec!["--fov", "10"],
            vec!["--distant-horizon", "near"],
            vec!["--clouds", "stormy"],
            vec!["--particles", "lots"],
            vec!["--vsync", "maybe"],
            vec!["--smooth-lighting", "2"],
            vec!["--max-fps", "12"],
            vec!["--max-fps", "fast"],
        ] {
            assert!(parse_args(&argv).is_err(), "{argv:?} must fail");
        }
    }
}
