// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
//! Standard Minecraft 1.21.1 launcher CLI parser, credential redactor, and
//! player-settings source resolution.

use std::fmt;
use std::path::{Path, PathBuf};

use mc_spine_client_mesh::RenderBackendPreference;
use mc_spine_proto::friend_relay::InviteCode;

use crate::crash_reporter::parse_report_url;
use crate::hardware_profile::{probe_host_hardware, recommend_tuning};
use crate::settings::{
    sanitize_shader_selector, ClientSettings, CloudMode, CrashReportMode, DistantHorizon,
    HostMinimapMode, HostPlayerRadarScope, ParticleLevel, QualityPreset, MAX_BACKGROUND_FPS,
    MAX_FOV, MAX_MAX_FPS, MAX_VIEW_DISTANCE, MIN_FOV, MIN_MAX_FPS, MIN_VIEW_DISTANCE,
    SETTINGS_FILE_NAME,
};

pub const CLIENT_VERSION: &str = env!("CARGO_PKG_VERSION");
pub const MINECRAFT_VERSION: &str = "1.21.1";
pub const PROTOCOL_VERSION: u32 = mc_spine_proto::ProtocolVersion::V1_21_1.number() as u32;

/// Environment variable consulted for the kernel version pack directory when
/// `--version-dir` is not passed — the same name the server's
/// `VersionAssetPaths::resolve` honors.
pub const VERSION_DIR_ENV: &str = "MC_SPINE_VERSION_DIR";

/// Parse a `--mc-version` value into the protocol the client should speak.
/// Accepts the Minecraft release name or its numeric protocol; anything else
/// fails closed instead of guessing a table.
pub fn parse_mc_version(value: &str) -> Result<mc_spine_proto::ProtocolVersion, String> {
    match value.trim() {
        "1.21.1" | "767" => Ok(mc_spine_proto::ProtocolVersion::V1_21_1),
        "26.3" | "777" => Ok(mc_spine_proto::ProtocolVersion::V26_3),
        other => Err(format!(
            "invalid --mc-version '{other}' (expected 1.21.1, 26.3, 767, or 777)"
        )),
    }
}

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
    pub join_code: Option<InviteCode>,
    pub relay: Option<String>,
    pub server_bin: Option<PathBuf>,
    pub seed: Option<i64>,

    // Native mc-spine-client flags
    /// Protocol the client handshakes with (`--mc-version`); defaults to
    /// 1.21.1/767.
    pub mc_version: mc_spine_proto::ProtocolVersion,
    /// `--ping <host[:port]>`: run a server-list status query and exit.
    pub ping: Option<String>,
    /// Kernel version pack directory (`--version-dir` or
    /// `MC_SPINE_VERSION_DIR`) for `Registry::from_version_dir`.
    pub version_dir: Option<PathBuf>,
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
    /// `--background-fps <0..=240>`: unfocused-window frame cap; `0`
    /// disables background throttling.
    pub background_fps: Option<u16>,
    /// `--gpu-backend`: explicit GPU pipeline selection; overrides the
    /// TOML `gpu_backend` key when given.
    pub gpu_backend: Option<RenderBackendPreference>,
    /// `--shader-pack <path>`: external OptiFine/Iris/DistantHorizons
    /// shaderpack (zip or directory) or Megu native `megu-shader.toml`
    /// WGSL pack; overrides the TOML `shader_pack` key when given.
    pub shader_pack: Option<String>,
    /// `--shader-profile <name>`: `profile.<NAME>` inside the pack's
    /// `shaders.properties`; overrides the TOML `shader_profile` key.
    pub shader_profile: Option<String>,
    /// `--auto-tune`: seed settings from `recommend_tuning` of the probed
    /// host hardware before TOML keys and explicit flags are overlaid.
    pub auto_tune: bool,
    /// `--fair-play`: host the world in map-mod fair-play mode —
    /// `minimap_mode` `fair_play` with cave map, mob/entity radar, player
    /// radar and waypoint teleport locked unless a granular flag
    /// re-enables one.
    pub fair_play: bool,
    /// `--minimap-mode <unrestricted|fair_play|disabled>`: explicit
    /// minimap-mode override applied on top of `--fair-play` or the TOML
    /// `minimap_mode` key.
    pub minimap_mode: Option<HostMinimapMode>,
    /// `--allow-cave-map <true|false>`: explicit cave-map override applied
    /// on top of `--fair-play` or the TOML `allow_cave_map` key.
    pub allow_cave_map: Option<bool>,
    /// `--allow-entity-radar <true|false>`: explicit entity-radar
    /// override — the pre-granular spelling of `--allow-mob-radar`.
    pub allow_entity_radar: Option<bool>,
    /// `--allow-mob-radar <true|false>`: explicit mob/entity-radar
    /// override; wins over `--allow-entity-radar` when both are given.
    pub allow_mob_radar: Option<bool>,
    /// `--player-radar <all|party_only|none>`: explicit player-radar
    /// scope override.
    pub player_radar: Option<HostPlayerRadarScope>,
    /// `--allow-waypoint-teleport <true|false>`: explicit
    /// waypoint-teleport override.
    pub allow_waypoint_teleport: Option<bool>,
    /// Whether `--view-distance` was given on the command line.
    pub view_distance_explicit: bool,

    /// `--crash-report-mode <ask|auto_crash_only|off>`: consent-policy
    /// override applied on top of the TOML `crash_report_mode` key.
    pub crash_report_mode: Option<CrashReportMode>,
    /// `--report-endpoint <url>`: report relay URL, validated through
    /// [`parse_report_url`] at parse time; overrides the TOML
    /// `report_endpoint` key. `None` = no network egress.
    pub report_endpoint: Option<String>,
    /// `--crash-reports-dir <path>`: crash-report directory override;
    /// defaults to `<gameDir>/crash-reports` or `./crash-reports`.
    pub crash_reports_dir: Option<PathBuf>,
    /// `--flush-crash-reports`: offline-safe recovery pass over the crash
    /// directory — summarizes queued `pending-*.json` payloads and sends
    /// only what the resolved consent policy allows.
    pub flush_crash_reports: bool,
    /// `--confirm-send`: explicit user consent for
    /// `--flush-crash-reports` under `ask` mode.
    pub confirm_send: bool,

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
            join_code: None,
            relay: None,
            server_bin: None,
            seed: None,
            mc_version: mc_spine_proto::ProtocolVersion::V1_21_1,
            ping: None,
            version_dir: None,
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
            background_fps: None,
            gpu_backend: None,
            shader_pack: None,
            shader_profile: None,
            auto_tune: false,
            fair_play: false,
            minimap_mode: None,
            allow_cave_map: None,
            allow_entity_radar: None,
            allow_mob_radar: None,
            player_radar: None,
            allow_waypoint_teleport: None,
            view_distance_explicit: false,
            crash_report_mode: None,
            report_endpoint: None,
            crash_reports_dir: None,
            flush_crash_reports: false,
            confirm_send: false,
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
            .field("join_code", &self.join_code)
            .field("relay", &self.relay)
            .field("server_bin", &self.server_bin)
            .field("seed", &self.seed)
            .field("mc_version", &self.mc_version)
            .field("ping", &self.ping)
            .field("version_dir", &self.version_dir)
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
            .field("background_fps", &self.background_fps)
            .field("gpu_backend", &self.gpu_backend)
            .field("shader_pack", &self.shader_pack)
            .field("shader_profile", &self.shader_profile)
            .field("auto_tune", &self.auto_tune)
            .field("fair_play", &self.fair_play)
            .field("minimap_mode", &self.minimap_mode)
            .field("allow_cave_map", &self.allow_cave_map)
            .field("allow_entity_radar", &self.allow_entity_radar)
            .field("allow_mob_radar", &self.allow_mob_radar)
            .field("player_radar", &self.player_radar)
            .field("allow_waypoint_teleport", &self.allow_waypoint_teleport)
            .field("view_distance_explicit", &self.view_distance_explicit)
            .field("crash_report_mode", &self.crash_report_mode)
            .field("report_endpoint", &self.report_endpoint)
            .field("crash_reports_dir", &self.crash_reports_dir)
            .field("flush_crash_reports", &self.flush_crash_reports)
            .field("confirm_send", &self.confirm_send)
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

    /// Crash-report directory: `--crash-reports-dir` when given, else
    /// `<gameDir>/crash-reports` when `--gameDir` is set, else
    /// `./crash-reports` relative to the working directory.
    pub fn resolved_crash_reports_dir(&self) -> PathBuf {
        if let Some(dir) = &self.crash_reports_dir {
            return dir.clone();
        }
        if let Some(game_dir) = &self.game_dir {
            return game_dir.join("crash-reports");
        }
        PathBuf::from("crash-reports")
    }

    /// Whether the run requests a server or render action (as opposed to a
    /// settings-only invocation such as `--write-settings`).
    pub fn wants_session_action(&self) -> bool {
        self.quick_play_multiplayer.is_some()
            || self.quick_play_singleplayer.is_some()
            || self.server.is_some()
            || self.join_code.is_some()
            || self.render_png.is_some()
            || self.ping.is_some()
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

        // `--auto-tune` seeds the base from the probed host hardware BEFORE
        // the settings file, the options.txt import, and explicit flags are
        // overlaid, so every manual value always wins over the
        // recommendation.
        if self.auto_tune {
            recommend_tuning(&probe_host_hardware()).apply_to(&mut settings);
            source_present = true;
        }

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
        if let Some(value) = self.background_fps {
            settings.background_fps_limit = value;
            tuned = true;
        }
        if let Some(value) = self.gpu_backend {
            settings.gpu_backend = value;
            tuned = true;
        }
        if let Some(value) = &self.shader_pack {
            settings.shader_pack = Some(value.clone());
            tuned = true;
        }
        if let Some(value) = &self.shader_profile {
            settings.shader_profile = Some(value.clone());
            tuned = true;
        }
        // Crash-report policy flags: consent and egress configuration,
        // not quality-preset dimensions, so they do not mark `Custom`.
        if let Some(mode) = self.crash_report_mode {
            settings.crash_report_mode = mode;
        }
        if let Some(endpoint) = &self.report_endpoint {
            settings.report_endpoint = Some(endpoint.clone());
        }
        // Host fair-play policy: `--fair-play` switches the minimap into
        // fair-play mode and locks every granular switch, then each
        // `--minimap-mode`/`--allow-*`/`--player-radar` flag refines its
        // own dimension. These are host-policy flags, not quality-preset
        // dimensions, so they do not mark the preset `Custom`. The legacy
        // mirrors stay in sync: `minimap_fair_play` is `minimap_mode !=
        // Unrestricted` and `allow_entity_radar` is `allow_mob_radar`.
        if self.fair_play {
            settings.map_fair_play.minimap_fair_play = true;
            settings.map_fair_play.minimap_mode = HostMinimapMode::FairPlay;
            settings.map_fair_play.allow_cave_map = false;
            settings.map_fair_play.allow_entity_radar = false;
            settings.map_fair_play.allow_mob_radar = false;
            settings.map_fair_play.player_radar_scope = HostPlayerRadarScope::None;
            settings.map_fair_play.allow_waypoint_teleport = false;
        }
        if let Some(mode) = self.minimap_mode {
            settings.map_fair_play.minimap_mode = mode;
            settings.map_fair_play.minimap_fair_play = mode != HostMinimapMode::Unrestricted;
        }
        if let Some(value) = self.allow_cave_map {
            settings.map_fair_play.allow_cave_map = value;
        }
        if let Some(value) = self.allow_entity_radar {
            settings.map_fair_play.allow_entity_radar = value;
            settings.map_fair_play.allow_mob_radar = value;
        }
        if let Some(value) = self.allow_mob_radar {
            settings.map_fair_play.allow_entity_radar = value;
            settings.map_fair_play.allow_mob_radar = value;
        }
        if let Some(scope) = self.player_radar {
            settings.map_fair_play.player_radar_scope = scope;
        }
        if let Some(value) = self.allow_waypoint_teleport {
            settings.map_fair_play.allow_waypoint_teleport = value;
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
            "--json" | "--json-summary" => cfg.json_summary = true,
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
            "--join-code" => {
                let raw = take_val(flag, &mut i)?;
                cfg.join_code = Some(
                    InviteCode::parse(&raw)
                        .map_err(|e| format!("invalid --join-code '{raw}': {e}"))?,
                );
            }
            "--relay" => cfg.relay = Some(take_val(flag, &mut i)?),
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
                if matches!(
                    raw.to_ascii_lowercase().as_str(),
                    "unlimited" | "none" | "off"
                ) {
                    cfg.max_fps = Some(None);
                } else {
                    let parsed = raw
                        .parse::<i64>()
                        .map_err(|_| format!("invalid --max-fps '{raw}'"))?;
                    if parsed == 0 {
                        cfg.max_fps = Some(None);
                    } else {
                        if !(i64::from(MIN_MAX_FPS)..=i64::from(MAX_MAX_FPS)).contains(&parsed) {
                            return Err(format!(
                                "--max-fps must be 0, 'unlimited', or in {MIN_MAX_FPS}..={MAX_MAX_FPS} (got {parsed})"
                            ));
                        }
                        cfg.max_fps = Some(Some(parsed as u32));
                    }
                }
            }
            "--background-fps" => {
                let raw = take_val(flag, &mut i)?;
                let parsed = raw
                    .parse::<i64>()
                    .map_err(|_| format!("invalid --background-fps '{raw}'"))?;
                if !(0..=i64::from(MAX_BACKGROUND_FPS)).contains(&parsed) {
                    return Err(format!(
                        "--background-fps must be in 0..={MAX_BACKGROUND_FPS} (got {parsed})"
                    ));
                }
                cfg.background_fps = Some(parsed as u16);
            }
            "--gpu-backend" => {
                let raw = take_val(flag, &mut i)?;
                cfg.gpu_backend = Some(RenderBackendPreference::parse(&raw).ok_or_else(|| {
                    format!(
                        "invalid --gpu-backend '{raw}' (expected auto, mesh-shader, \
                         compute-indirect, classic-low-spec, or classic)"
                    )
                })?);
            }
            "--shader-pack" => {
                let raw = take_val(flag, &mut i)?;
                let Some(value) = sanitize_shader_selector(&raw) else {
                    return Err(format!(
                        "invalid --shader-pack '{raw}' (empty or contains '..' path traversal)"
                    ));
                };
                cfg.shader_pack = Some(value);
            }
            "--shader-profile" => {
                let raw = take_val(flag, &mut i)?;
                let Some(value) = sanitize_shader_selector(&raw) else {
                    return Err(format!(
                        "invalid --shader-profile '{raw}' (empty or contains '..' path traversal)"
                    ));
                };
                cfg.shader_profile = Some(value);
            }
            "--auto-tune" => cfg.auto_tune = true,
            "--fair-play" => cfg.fair_play = true,
            "--minimap-mode" => {
                let raw = take_val(flag, &mut i)?;
                cfg.minimap_mode = Some(HostMinimapMode::parse(&raw).ok_or_else(|| {
                    format!(
                        "invalid --minimap-mode '{raw}' (expected unrestricted, fair_play, or disabled)"
                    )
                })?);
            }
            "--allow-cave-map" => {
                cfg.allow_cave_map = Some(parse_on_off(&take_val(flag, &mut i)?, flag)?);
            }
            "--allow-entity-radar" => {
                cfg.allow_entity_radar = Some(parse_on_off(&take_val(flag, &mut i)?, flag)?);
            }
            "--allow-mob-radar" => {
                cfg.allow_mob_radar = Some(parse_on_off(&take_val(flag, &mut i)?, flag)?);
            }
            "--player-radar" => {
                let raw = take_val(flag, &mut i)?;
                cfg.player_radar = Some(HostPlayerRadarScope::parse(&raw).ok_or_else(|| {
                    format!("invalid --player-radar '{raw}' (expected all, party_only, or none)")
                })?);
            }
            "--allow-waypoint-teleport" => {
                cfg.allow_waypoint_teleport =
                    Some(parse_on_off(&take_val(flag, &mut i)?, flag)?);
            }
            "--mc-version" => {
                let raw = take_val(flag, &mut i)?;
                cfg.mc_version = parse_mc_version(&raw)?;
            }
            "--ping" => cfg.ping = Some(take_val(flag, &mut i)?),
            "--version-dir" => {
                cfg.version_dir = Some(PathBuf::from(take_val(flag, &mut i)?));
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
            "--crash-report-mode" => {
                let raw = take_val(flag, &mut i)?;
                cfg.crash_report_mode = Some(CrashReportMode::parse(&raw).ok_or_else(|| {
                    format!(
                        "invalid --crash-report-mode '{raw}' (expected ask, auto_crash_only, or off)"
                    )
                })?);
            }
            "--report-endpoint" => {
                let raw = take_val(flag, &mut i)?;
                parse_report_url(&raw)
                    .map_err(|err| format!("invalid --report-endpoint '{raw}': {err}"))?;
                cfg.report_endpoint = Some(raw);
            }
            "--crash-reports-dir" => {
                cfg.crash_reports_dir = Some(PathBuf::from(take_val(flag, &mut i)?));
            }
            "--flush-crash-reports" => cfg.flush_crash_reports = true,
            "--confirm-send" => cfg.confirm_send = true,
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

    // `--version-dir` wins over the environment; an empty value disables it.
    if cfg.version_dir.is_none() {
        if let Some(dir) = std::env::var_os(VERSION_DIR_ENV) {
            if !dir.is_empty() {
                cfg.version_dir = Some(PathBuf::from(dir));
            }
        }
    }

    if cfg.join_code.is_some() {
        let relay = cfg
            .relay
            .as_deref()
            .ok_or_else(|| "--join-code requires --relay <host:port>".to_string())?;
        let (_, port) = parse_host_port(relay, 0)?;
        if !relay.contains(':') || port == 0 {
            return Err("--relay must include a nonzero port (<host:port>)".to_string());
        }
        if cfg.server.is_some()
            || cfg.quick_play_multiplayer.is_some()
            || cfg.quick_play_singleplayer.is_some()
            || cfg.ping.is_some()
        {
            return Err(
                "--join-code cannot be combined with another server target or --ping".to_string(),
            );
        }
    }

    if cfg.confirm_send && !cfg.flush_crash_reports {
        return Err("--confirm-send only applies to --flush-crash-reports".to_string());
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
  --join-code <MEGU-XXXX-XXXX>       Join a shared world using a friend invite
  --relay <host:port>                Relay address (required with --join-code)
  --port <port>                      Server port (default: 25565)
  --server-bin <path>                mc-spine server binary for --quickPlaySingleplayer
  --seed <seed>                      World seed for a new --quickPlaySingleplayer world

NATIVE CLIENT FLAGS:
  --mc-version <ver>                 Client protocol: 1.21.1|26.3 or 767|777 (default: 1.21.1)
  --ping <host[:port]>               Query a server's status (version, players,
                                     MOTD, latency, icon) and exit; honors
                                     --mc-version and --json/--json-summary
  --version-dir <path>               Kernel version pack directory for the block
                                     registry (env: MC_SPINE_VERSION_DIR)
  --headless                         Run headless protocol + meshing + physics session
  --ticks <N>                        Number of simulation ticks to run (default: 40)
  --view-distance <N>                Client view distance in chunks, 2..=32 (default: 2)
  --assets-json <path>               Path to assets/block_states_1211.json
  --script <mode>                    Scripted input: walk|jump|sprint|idle|survey
  --json-summary, --json             Emit machine-readable JSON on stdout
                                     (session summary or --ping status)
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
  --max-fps <N|0|unlimited|none>   Frame rate cap, 10..=2000
                                   (0, none, or unlimited disables the cap)
  --background-fps <N>             Frame rate cap while unfocused, 0..=240
                                   (0 disables background throttling,
                                   default 15)
  --gpu-backend <backend>          GPU pipeline: auto|mesh-shader|
                                   compute-indirect|classic-low-spec|classic
                                   (classic-low-spec targets legacy Intel
                                   HD/UHD iGPUs: GL 3.3 / Vulkan 1.1 only)
  --auto-tune                      Seed settings from probed host hardware;
                                   the settings file, options.txt import, and
                                   explicit flags still override

SHADER PACKS:
  --shader-pack <path>             External OptiFine/Iris shaderpack (zip or
                                   directory), DistantHorizons LOD pack, or a
                                   Megu native WGSL pack (megu-shader.toml);
                                   packs requiring GPU features this device
                                   lacks fall back to built-in rendering
                                   with a notice instead of crashing
  --shader-profile <name>          Optional profile.<NAME> inside the pack's
                                   shaders.properties to activate on load

HOST WORLD MAP FAIR PLAY:
  Applied when this client hosts a world (--quickPlaySingleplayer, LAN
  share, or a MEGU-XXXX-XXXX friend relay) and forwarded to the spawned
  mc-spine server as MC_SPINE_FAIR_PLAY*, MC_SPINE_MINIMAP_MODE,
  MC_SPINE_ALLOW_MOB_RADAR and MC_SPINE_PLAYER_RADAR_SCOPE environment
  variables plus the Xaero §-coded control signal. The five granular
  switches mirror the server's minimap-mode / allow-cave-map /
  allow-mob-radar / player-radar-scope / allow-waypoint-teleport keys.
  --fair-play                      Enable map-mod fair play: minimap mode
                                   fair_play with cave map, mob radar,
                                   player radar, and waypoint teleport all
                                   OFF; re-enable individually below
  --minimap-mode <mode>            Minimap policy: unrestricted|fair_play|
                                   disabled (default: unrestricted)
  --allow-cave-map <true|false>    Allow Xaero cave-map mode (default off
                                   under --fair-play, on otherwise)
  --allow-mob-radar <true|false>   Allow mob/entity radar (same defaulting)
  --allow-entity-radar <t|f>       Legacy spelling of --allow-mob-radar
  --player-radar <scope>           Player radar scope: all|party_only|none
                                   (default none under --fair-play, all
                                   otherwise)
  --allow-waypoint-teleport <t|f>  Allow world-map waypoint teleport (same)

CRASH REPORTS:
  Panics are always written as redacted crash-<fingerprint>.md files under
  the crash-reports directory; nothing is transmitted unless a relay
  endpoint is configured AND the consent mode allows it.
  --crash-report-mode <mode>       Consent policy: ask|auto_crash_only|off
                                   (default: ask; TOML crash_report_mode)
  --report-endpoint <url>          Report relay URL: https://<host> or a
                                   loopback http://127.0.0.1:<port>/api/report;
                                   unset means no network egress
  --crash-reports-dir <path>       Crash report directory (default:
                                   <gameDir>/crash-reports or ./crash-reports)
  --flush-crash-reports            Summarize queued reports and send the
                                   pending ones the policy allows, then exit;
                                   honors --json/--json-summary
  --confirm-send                   Confirm sending queued reports for
                                   --flush-crash-reports under 'ask' mode
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
    fn gpu_backend_flag_parses_overrides_and_fails_closed() {
        // The flag alone applies to the resolved settings and marks the
        // preset Custom like every other explicit override.
        let mut cfg = parse_args(&["--gpu-backend", "classic-low-spec"]).unwrap();
        cfg.resolve_client_settings_from(Path::new(".")).unwrap();
        assert_eq!(
            cfg.client_settings.gpu_backend,
            RenderBackendPreference::ClassicLowSpec
        );
        assert_eq!(cfg.client_settings.preset, QualityPreset::Custom);
        assert_eq!(cfg.gpu_backend, Some(RenderBackendPreference::ClassicLowSpec));

        // Every documented token parses.
        for (token, want) in [
            ("auto", RenderBackendPreference::Auto),
            ("mesh-shader", RenderBackendPreference::MeshShader),
            ("compute-indirect", RenderBackendPreference::ComputeIndirect),
            ("classic-low-spec", RenderBackendPreference::ClassicLowSpec),
            ("classic", RenderBackendPreference::Classic),
        ] {
            let cfg = parse_args(&["--gpu-backend", token]).unwrap();
            assert_eq!(cfg.gpu_backend, Some(want), "{token}");
        }
        // `--gpu-backend=...` inline form works too.
        let cfg = parse_args(&["--gpu-backend=classic-low-spec"]).unwrap();
        assert_eq!(
            cfg.gpu_backend,
            Some(RenderBackendPreference::ClassicLowSpec)
        );

        // The flag overrides the TOML key cleanly.
        let dir = scratch_dir("gpu-backend");
        let file = write_file(&dir, "custom.toml", "gpu_backend = \"mesh-shader\"\n");
        let mut cfg = parse_args(&[
            "--settings",
            file.to_str().unwrap(),
            "--gpu-backend",
            "classic",
        ])
        .unwrap();
        cfg.resolve_client_settings_from(&dir).unwrap();
        assert_eq!(cfg.client_settings.gpu_backend, RenderBackendPreference::Classic);
        // Without the flag the TOML value applies on its own.
        let mut cfg = parse_args(&["--settings", file.to_str().unwrap()]).unwrap();
        cfg.resolve_client_settings_from(&dir).unwrap();
        assert_eq!(
            cfg.client_settings.gpu_backend,
            RenderBackendPreference::MeshShader
        );
        let _ = std::fs::remove_dir_all(&dir);

        // Unknown values fail closed with a useful message.
        let err = parse_args(&["--gpu-backend", "vulkan"]).unwrap_err();
        assert!(err.contains("invalid --gpu-backend"), "unexpected: {err}");
        let err = parse_args(&["--gpu-backend", "low-spec"]).unwrap_err();
        assert!(err.contains("invalid --gpu-backend"), "unexpected: {err}");
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
            vec!["--max-fps", "9"],
            vec!["--max-fps", "2001"],
            vec!["--max-fps", "fast"],
            vec!["--gpu-backend", "vulkan"],
            vec!["--gpu-backend"],
        ] {
            assert!(parse_args(&argv).is_err(), "{argv:?} must fail");
        }
    }
}
