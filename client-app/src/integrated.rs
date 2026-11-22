// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
//! Real `--quickPlaySingleplayer <world>` support: resolve the `megu`
//! server binary, launch it on an ephemeral loopback port with the world save
//! rooted at `<gameDir>/saves/<world>`, wait for TCP readiness, and always
//! terminate + reap the child via a `Drop` guard.

use std::env;
use std::fs;
use std::io;
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use crate::cli::CliConfig;
use crate::settings::{ClientSettings, HostMapFairPlaySettings};

/// Upper bound for the integrated server to start listening.
const READY_TIMEOUT: Duration = Duration::from_secs(15);
const READY_POLL: Duration = Duration::from_millis(50);

/// A running integrated `megu` server child. Dropping kills and reaps it.
pub struct SingleplayerServer {
    child: Child,
    pub port: u16,
    #[allow(dead_code)]
    world_dir: PathBuf,
}

impl SingleplayerServer {
    pub fn addr(&self) -> SocketAddr {
        SocketAddr::from(([127, 0, 0, 1], self.port))
    }
}

impl Drop for SingleplayerServer {
    fn drop(&mut self) {
        // Prefer a graceful SIGTERM so the server can run its save_world()
        // shutdown path; fall back to kill() if it does not exit promptly.
        #[cfg(unix)]
        {
            let _ = Command::new("kill")
                .arg("-TERM")
                .arg(self.child.id().to_string())
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
            let deadline = Instant::now() + Duration::from_secs(3);
            loop {
                match self.child.try_wait() {
                    Ok(Some(_)) | Err(_) => return,
                    Ok(None) if Instant::now() >= deadline => break,
                    Ok(None) => thread::sleep(Duration::from_millis(25)),
                }
            }
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Fail-closed world-name validation: the name becomes a single path
/// component under `<gameDir>/saves/`, so separators, traversal segments,
/// dot names, empty strings and control characters are all rejected.
pub fn validate_world_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("empty --quickPlaySingleplayer world name".to_string());
    }
    if name == "." || name == ".." {
        return Err(format!(
            "invalid --quickPlaySingleplayer world name '{name}'"
        ));
    }
    if name.contains('/') || name.contains('\\') || name.contains('\0') {
        return Err(format!(
            "invalid --quickPlaySingleplayer world name '{name}': path separators are not allowed"
        ));
    }
    if name.chars().any(|c| c.is_control()) {
        return Err(format!(
            "invalid --quickPlaySingleplayer world name '{name}': control characters are not allowed"
        ));
    }
    Ok(())
}

/// Enqueue the platform server-binary names inside `dir`: `megu`
/// everywhere, plus `megu.exe` — the name the Windows toolchain
/// actually emits — tried first there.
#[cfg(windows)]
fn push_server_bin_candidates(candidates: &mut Vec<PathBuf>, dir: &Path) {
    candidates.push(dir.join("megu.exe"));
    candidates.push(dir.join("megu"));
}

#[cfg(not(windows))]
fn push_server_bin_candidates(candidates: &mut Vec<PathBuf>, dir: &Path) {
    candidates.push(dir.join("megu"));
}

fn is_executable_file(path: &Path) -> bool {
    path.is_file()
}

/// Resolve the `megu` server binary (`megu.exe` on Windows).
/// Order: `--server-bin`, `MEGU_SERVER_BIN`, a `megu` sibling of
/// this executable, then `target/{debug,release}/megu` relative to
/// the working directory and to ancestors of the working directory and
/// this executable.
pub fn resolve_server_bin(explicit: Option<&Path>) -> Result<PathBuf, String> {
    if let Some(path) = explicit {
        let path = path.to_path_buf();
        if is_executable_file(&path) {
            return Ok(path);
        }
        return Err(format!(
            "--server-bin '{}' does not exist or is not a file",
            path.display()
        ));
    }
    if let Ok(env_path) = env::var("MEGU_SERVER_BIN") {
        if !env_path.is_empty() {
            let path = PathBuf::from(&env_path);
            if is_executable_file(&path) {
                return Ok(path);
            }
            return Err(format!(
                "MEGU_SERVER_BIN '{}' does not exist or is not a file",
                path.display()
            ));
        }
    }

    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Ok(exe) = env::current_exe() {
        if let Some(dir) = exe.parent() {
            push_server_bin_candidates(&mut candidates, dir);
        }
    }
    let mut bases: Vec<PathBuf> = vec![env::current_dir().unwrap_or_else(|_| PathBuf::from("."))];
    if let Ok(exe) = env::current_exe() {
        bases.extend(exe.ancestors().skip(1).take(6).map(Path::to_path_buf));
    }
    for base in bases {
        for profile in ["debug", "release"] {
            push_server_bin_candidates(&mut candidates, &base.join("target").join(profile));
        }
        push_server_bin_candidates(&mut candidates, &base);
    }
    for candidate in candidates {
        if is_executable_file(&candidate) {
            return Ok(candidate);
        }
    }
    Err(
        "could not locate the megu server binary (set --server-bin <path> or MEGU_SERVER_BIN)"
            .to_string(),
    )
}

/// Locate the server runtime root: a directory containing `config_packets.bin`,
/// `datapacks/` and `assets/block_states_1211.json`, searched from the server
/// binary's ancestors and the working directory upwards.
fn find_runtime_root(server_bin: &Path) -> Option<PathBuf> {
    let mut bases: Vec<PathBuf> = Vec::new();
    if let Some(dir) = server_bin.parent() {
        bases.extend(dir.ancestors().take(8).map(Path::to_path_buf));
    }
    if let Ok(cwd) = env::current_dir() {
        bases.extend(cwd.ancestors().take(8).map(Path::to_path_buf));
    }
    bases.sort();
    bases.dedup();
    bases.into_iter().find(|dir| {
        dir.join("config_packets.bin").is_file()
            && dir.join("datapacks").is_dir()
            && dir.join("assets/block_states_1211.json").is_file()
    })
}

#[cfg(unix)]
fn link_or_copy(src: &Path, dst: &Path) -> io::Result<()> {
    if dst.symlink_metadata().is_ok() {
        return Ok(());
    }
    match std::os::unix::fs::symlink(src, dst) {
        Ok(()) => Ok(()),
        Err(_) if src.is_dir() => copy_dir_all(src, dst),
        Err(_) => fs::copy(src, dst).map(|_| ()),
    }
}

#[cfg(not(unix))]
fn link_or_copy(src: &Path, dst: &Path) -> io::Result<()> {
    if dst.symlink_metadata().is_ok() {
        return Ok(());
    }
    if src.is_dir() {
        copy_dir_all(src, dst)
    } else {
        fs::copy(src, dst).map(|_| ())
    }
}

fn copy_dir_all(src: &Path, dst: &Path) -> io::Result<()> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let target = dst.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir_all(&entry.path(), &target)?;
        } else {
            fs::copy(entry.path(), &target)?;
        }
    }
    Ok(())
}

/// Expose the server runtime assets (`assets/`, `datapacks/`,
/// `config_packets.bin`) inside the per-world run directory, which doubles as
/// the server's working directory and save location.
fn prepare_world_dir(world_dir: &Path, runtime_root: &Path) -> Result<(), String> {
    fs::create_dir_all(world_dir)
        .map_err(|e| format!("cannot create world dir {}: {e}", world_dir.display()))?;
    for name in ["assets", "datapacks", "config_packets.bin"] {
        link_or_copy(&runtime_root.join(name), &world_dir.join(name))
            .map_err(|e| format!("cannot link {} into {}: {e}", name, world_dir.display()))?;
    }
    Ok(())
}

/// The integrated server should stream at least what the client intends to
/// draw: the resolved view distance when a settings source or an explicit
/// `--view-distance` supplied one, otherwise `None` (keep the server
/// default).
pub fn server_view_distance(cfg: &CliConfig) -> Option<i8> {
    (cfg.view_distance_explicit || cfg.settings_source_present).then_some(cfg.view_distance)
}

/// Environment variable carrying the map fair-play master switch.
pub const FAIR_PLAY_ENV_ENABLED: &str = "MEGU_FAIR_PLAY";
/// Environment variable carrying the cave-map permission.
pub const FAIR_PLAY_ENV_ALLOW_CAVE_MAP: &str = "MEGU_FAIR_PLAY_ALLOW_CAVE_MAP";
/// Environment variable carrying the entity-radar permission.
pub const FAIR_PLAY_ENV_ALLOW_ENTITY_RADAR: &str = "MEGU_FAIR_PLAY_ALLOW_ENTITY_RADAR";
/// Environment variable carrying the waypoint-teleport permission.
pub const FAIR_PLAY_ENV_ALLOW_WAYPOINT_TELEPORT: &str =
    "MEGU_FAIR_PLAY_ALLOW_WAYPOINT_TELEPORT";
/// Environment variable carrying the Xaero `§`-coded control signal the
/// hosted server broadcasts to joining map-mod clients.
pub const FAIR_PLAY_ENV_XAERO_SIGNAL: &str = "MEGU_FAIR_PLAY_XAERO_SIGNAL";
/// Environment variable carrying the granular minimap mode
/// (`unrestricted`/`fair_play`/`disabled`), mirroring the server's
/// `minimap-mode` key (OPS-4).
pub const FAIR_PLAY_ENV_MINIMAP_MODE: &str = "MEGU_MINIMAP_MODE";
/// Environment variable carrying the mob/entity-radar permission under
/// the server's `allow-mob-radar` spelling (OPS-4).
pub const FAIR_PLAY_ENV_ALLOW_MOB_RADAR: &str = "MEGU_ALLOW_MOB_RADAR";
/// Environment variable carrying the player-radar scope
/// (`all`/`party_only`/`none`), mirroring the server's
/// `player-radar-scope` key (OPS-4).
pub const FAIR_PLAY_ENV_PLAYER_RADAR_SCOPE: &str = "MEGU_PLAYER_RADAR_SCOPE";

/// Launch policy carried by a world this client hosts — integrated
/// (`--quickPlaySingleplayer`), LAN share, or a `MEGU-XXXX-XXXX` friend
/// relay. Built once from the resolved [`ClientSettings`] so the spawned
/// server receives the exact same map-mod fair-play policy the player's
/// TOML or CLI flags expressed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IntegratedServerConfig {
    /// The resolved map fair-play policy.
    pub map_fair_play: HostMapFairPlaySettings,
    /// The `§`-coded Xaero control signal derived from `map_fair_play`:
    /// `"§f§a§i§r§x§a§e§r§o"` under full fair play, `"§r§e§s§e§t§x§a§e§r§o"`
    /// when the policy is off, or the reset signal plus selective lock
    /// codes for a partial lock (see
    /// [`HostMapFairPlaySettings::xaero_control_string`]).
    pub xaero_control: String,
}

impl IntegratedServerConfig {
    /// Snapshot the fair-play policy from resolved client settings.
    pub fn from_settings(settings: &ClientSettings) -> Self {
        Self {
            map_fair_play: settings.map_fair_play,
            xaero_control: settings.map_fair_play.xaero_control_string(),
        }
    }

    /// Deterministic `(name, value)` env pairs for the spawned server,
    /// always emitted in this fixed order: the master switch, the three
    /// `allow_*` permissions as `true`/`false`, the Xaero control signal,
    /// then the three OPS-4 granular switches (`minimap_mode`,
    /// `allow_mob_radar`, `player_radar_scope`).
    pub fn env_pairs(&self) -> Vec<(&'static str, String)> {
        let fp = &self.map_fair_play;
        vec![
            (FAIR_PLAY_ENV_ENABLED, fp.minimap_fair_play.to_string()),
            (
                FAIR_PLAY_ENV_ALLOW_CAVE_MAP,
                fp.allow_cave_map.to_string(),
            ),
            (
                FAIR_PLAY_ENV_ALLOW_ENTITY_RADAR,
                fp.allow_entity_radar.to_string(),
            ),
            (
                FAIR_PLAY_ENV_ALLOW_WAYPOINT_TELEPORT,
                fp.allow_waypoint_teleport.to_string(),
            ),
            (FAIR_PLAY_ENV_XAERO_SIGNAL, self.xaero_control.clone()),
            (
                FAIR_PLAY_ENV_MINIMAP_MODE,
                fp.minimap_mode.as_str().to_string(),
            ),
            (
                FAIR_PLAY_ENV_ALLOW_MOB_RADAR,
                fp.allow_mob_radar.to_string(),
            ),
            (
                FAIR_PLAY_ENV_PLAYER_RADAR_SCOPE,
                fp.player_radar_scope.as_str().to_string(),
            ),
        ]
    }

    /// Forward every fair-play environment variable onto `command`.
    pub fn apply_env(&self, command: &mut Command) {
        for (name, value) in self.env_pairs() {
            command.env(name, value);
        }
    }
}

/// Launch the integrated server for `--quickPlaySingleplayer`. On success the
/// returned guard keeps the child alive until dropped.
pub fn launch(cfg: &CliConfig) -> Result<SingleplayerServer, String> {
    let world = cfg
        .quick_play_singleplayer
        .as_deref()
        .ok_or_else(|| "no --quickPlaySingleplayer world configured".to_string())?;
    validate_world_name(world)?;

    let server_bin = resolve_server_bin(cfg.server_bin.as_deref())?;
    let runtime_root = find_runtime_root(&server_bin).ok_or_else(|| {
        format!(
            "could not find megu runtime assets (config_packets.bin, datapacks/, assets/) near {}",
            server_bin.display()
        )
    })?;

    let game_dir = cfg.game_dir.clone().unwrap_or_else(|| PathBuf::from("."));
    let world_dir = game_dir.join("saves").join(world);
    prepare_world_dir(&world_dir, &runtime_root)?;

    // Ephemeral loopback port: bind :0, read the assigned port, release.
    let port = {
        let listener = TcpListener::bind(("127.0.0.1", 0))
            .map_err(|e| format!("cannot allocate loopback port: {e}"))?;
        listener
            .local_addr()
            .map_err(|e| format!("cannot read loopback port: {e}"))?
            .port()
    };

    let mut command = Command::new(&server_bin);
    command
        .current_dir(&world_dir)
        .env("MEGU_BIND_ADDR", "127.0.0.1")
        .env("MEGU_PORT", port.to_string())
        .env("MEGU_WORLD_DIR", &world_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit());
    if let Some(view) = server_view_distance(cfg) {
        command.env("MEGU_VIEW_DISTANCE", view.to_string());
    }
    if let Some(seed) = cfg.seed {
        command.env("MEGU_SEED", seed.to_string());
    }
    // The hosted world inherits the resolved map fair-play policy:
    // LAN-opened and friend-relay worlds share this spawn path, so the
    // same MEGU_FAIR_PLAY* env vars and Xaero signal apply to all of
    // them.
    IntegratedServerConfig::from_settings(&cfg.client_settings).apply_env(&mut command);
    let mut child = command.spawn().map_err(|e| {
        format!(
            "failed to spawn megu server '{}': {e}",
            server_bin.display()
        )
    })?;

    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let deadline = Instant::now() + READY_TIMEOUT;
    loop {
        if let Ok(Some(status)) = child.try_wait() {
            return Err(format!(
                "megu server exited during startup (status {status}) for world '{world}'"
            ));
        }
        if TcpStream::connect_timeout(&addr, Duration::from_millis(250)).is_ok() {
            return Ok(SingleplayerServer {
                child,
                port,
                world_dir,
            });
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!(
                "timed out after {}s waiting for megu server on {addr}",
                READY_TIMEOUT.as_secs()
            ));
        }
        thread::sleep(READY_POLL);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_view_distance_follows_resolved_settings() {
        // No settings source and no explicit flag: keep the server default.
        let mut cfg = CliConfig::default();
        assert_eq!(server_view_distance(&cfg), None);

        // A resolved settings source forwards its view distance.
        cfg.view_distance = 16;
        cfg.settings_source_present = true;
        assert_eq!(server_view_distance(&cfg), Some(16));

        // An explicit --view-distance forwards too, even alone.
        let mut cfg = CliConfig::default();
        cfg.view_distance = 9;
        cfg.view_distance_explicit = true;
        assert_eq!(server_view_distance(&cfg), Some(9));
    }
}
