// SPDX-License-Identifier: LGPL-3.0-or-later
//! Real `--quickPlaySingleplayer <world>` support: resolve the `megu`
//! server binary, launch it on an ephemeral loopback port with the world save
//! rooted at `<gameDir>/saves/<world>`, wait for TCP readiness, and always
//! terminate + reap the child via a `Drop` guard.

use std::env;
use std::fs;
use std::io;
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use crate::cli::CliConfig;
use crate::settings::{ClientSettings, HostMapFairPlaySettings};

/// Upper bound for the integrated server to start listening.
const READY_TIMEOUT: Duration = Duration::from_secs(15);
const READY_POLL: Duration = Duration::from_millis(50);

/// The integrated host binds loopback only — never `0.0.0.0`, so a
/// singleplayer or friend-hosted world is unreachable from the LAN even
/// when a friend relay exposes it to remote guests.
pub const INTEGRATED_BIND_IP: Ipv4Addr = Ipv4Addr::LOCALHOST;

/// The dedicated `megu` server's conventional port. Integrated
/// singleplayer/friend-hosted worlds must never claim it: a player
/// running a dedicated `megu` server on `25565` while playing
/// singleplayer on the same PC would otherwise collide.
pub const DEDICATED_SERVER_PORT: u16 = 25565;

/// Allocate the integrated host's OS-assigned ephemeral port on loopback.
/// Binding port `0` — never a fixed port — is what guarantees an
/// integrated world can never collide with a dedicated `megu` server
/// running on the same machine.
fn allocate_loopback_port() -> Result<u16, String> {
    let listener = TcpListener::bind((INTEGRATED_BIND_IP, 0))
        .map_err(|e| format!("cannot allocate loopback port: {e}"))?;
    let port = listener
        .local_addr()
        .map_err(|e| format!("cannot read loopback port: {e}"))?
        .port();
    // Ephemeral allocations never land on 25565 on any platform, but the
    // dedicated-server collision guarantee is worth a fail-closed check.
    if port == DEDICATED_SERVER_PORT {
        return Err(
            "OS assigned the dedicated-server port to the integrated host".to_string(),
        );
    }
    Ok(port)
}

/// The only location an integrated or friend-hosted world may live:
/// `<gameDir>/saves/<world>` after [`validate_world_name`] rejects every
/// separator, traversal segment, dot name and control character — so a
/// world name can never write outside `saves/`.
pub fn world_save_dir(game_dir: &Path, world_name: &str) -> Result<PathBuf, String> {
    validate_world_name(world_name)?;
    Ok(game_dir.join("saves").join(world_name))
}

/// A running integrated `megu` server child. Dropping kills and reaps it.
pub struct SingleplayerServer {
    child: Child,
    pub port: u16,
    #[allow(dead_code)]
    world_dir: PathBuf,
}

impl SingleplayerServer {
    pub fn addr(&self) -> SocketAddr {
        SocketAddr::from((INTEGRATED_BIND_IP, self.port))
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
            // Give the server up to 10s to finish `save_world()` and exit
            // cleanly on SIGTERM before falling back to SIGKILL (fast exits
            // still return on the first 25 ms poll with zero extra delay).
            let deadline = Instant::now() + Duration::from_secs(10);
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

fn canonicalize_best_effort(path: PathBuf) -> PathBuf {
    fs::canonicalize(&path).unwrap_or(path)
}

fn absolute_path_best_effort(path: &Path) -> PathBuf {
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else if let Ok(cwd) = env::current_dir() {
        cwd.join(path)
    } else {
        path.to_path_buf()
    };
    canonicalize_best_effort(path)
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
            return Ok(canonicalize_best_effort(path));
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
                return Ok(canonicalize_best_effort(path));
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
            return Ok(canonicalize_best_effort(candidate));
        }
    }
    Err(
        "could not locate the megu server binary (set --server-bin <path> or MEGU_SERVER_BIN)"
            .to_string(),
    )
}

/// Return the runtime payload pieces missing from `dir`, in the fixed order
/// used by user-facing diagnostics.
fn missing_runtime_root_parts(dir: &Path) -> Vec<&'static str> {
    let mut missing = Vec::new();
    if !dir.join("config_packets.bin").is_file() {
        missing.push("config_packets.bin");
    }
    if !dir.join("datapacks").is_dir() {
        missing.push("datapacks/");
    }
    if !dir.join("assets/block_states_1211.json").is_file() {
        missing.push("assets/block_states_1211.json");
    }
    missing
}

fn push_runtime_root_candidate(candidates: &mut Vec<PathBuf>, candidate: PathBuf) {
    if !candidates.iter().any(|existing| existing == &candidate) {
        candidates.push(candidate);
    }
}

/// Runtime-root candidates in their deterministic search order: the resolved
/// server binary's ancestors nearest-first, then `<gameDir>/bin`, `<gameDir>`,
/// and the lexicographically sorted direct bundle children of
/// `<gameDir>/megu-runtime`.
fn runtime_root_candidates(server_bin: &Path, game_dir: &Path) -> Vec<PathBuf> {
    let resolved_bin = absolute_path_best_effort(server_bin);
    let game_dir = absolute_path_best_effort(game_dir);
    let mut candidates = Vec::new();
    if let Some(dir) = resolved_bin.parent() {
        for ancestor in dir.ancestors().filter(|path| !path.as_os_str().is_empty()) {
            push_runtime_root_candidate(&mut candidates, ancestor.to_path_buf());
        }
    }
    push_runtime_root_candidate(&mut candidates, game_dir.join("bin"));
    push_runtime_root_candidate(&mut candidates, game_dir.clone());

    let runtime_dir = game_dir.join("megu-runtime");
    if let Ok(entries) = fs::read_dir(&runtime_dir) {
        let mut bundles: Vec<PathBuf> = entries
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.path())
            .filter(|path| path.is_dir())
            .collect();
        bundles.sort();
        for bundle in bundles {
            push_runtime_root_candidate(&mut candidates, bundle);
        }
    }
    candidates
}

/// Locate the server runtime root: a complete directory containing
/// `config_packets.bin`, `datapacks/` and `assets/block_states_1211.json`.
fn find_runtime_root(server_bin: &Path, game_dir: &Path) -> Option<PathBuf> {
    runtime_root_candidates(server_bin, game_dir)
        .into_iter()
        .find(|dir| missing_runtime_root_parts(dir).is_empty())
}

fn runtime_root_error(server_bin: &Path, game_dir: &Path) -> String {
    let mut error = format!(
        "could not find a complete megu runtime root for server '{}' in game dir '{}'; required: config_packets.bin, datapacks/, assets/block_states_1211.json",
        server_bin.display(),
        game_dir.display()
    );
    error.push_str("\nsearched in order:");
    for candidate in runtime_root_candidates(server_bin, game_dir) {
        let missing = missing_runtime_root_parts(&candidate);
        if missing.is_empty() {
            error.push_str(&format!("\n  - {} (complete)", candidate.display()));
        } else {
            error.push_str(&format!(
                "\n  - {} (missing {})",
                candidate.display(),
                missing.join(", ")
            ));
        }
    }
    let bundle_dir = absolute_path_best_effort(game_dir).join("megu-runtime");
    if let Err(err) = fs::read_dir(&bundle_dir) {
        error.push_str(&format!(
            "\ncould not scan release bundle dir {}: {err}",
            bundle_dir.display()
        ));
    }
    error.push_str(&format!(
        "\nstage the generated runtime data in one of these locations or in a direct child of {}; incomplete roots are skipped",
        bundle_dir.display()
    ));
    error
}

#[cfg(unix)]
fn link_or_copy(src: &Path, dst: &Path) -> io::Result<()> {
    if let Ok(meta) = dst.symlink_metadata() {
        // Heal broken symlinks (e.g. from an earlier relative-path bug) so the
        // world recovers cleanly on next launch.
        if meta.file_type().is_symlink() && !dst.exists() {
            let _ = fs::remove_file(dst);
        } else {
            return Ok(());
        }
    }
    let abs_src = canonicalize_best_effort(src.to_path_buf());
    match std::os::unix::fs::symlink(&abs_src, dst) {
        Ok(()) => Ok(()),
        Err(_) if abs_src.is_dir() => copy_dir_all(&abs_src, dst),
        Err(_) => fs::copy(&abs_src, dst).map(|_| ()),
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

/// Expose the server runtime assets (`assets/`, `datapacks/vanilla`,
/// `config_packets.bin`) inside the per-world run directory, which doubles as
/// the server's working directory and save location.
///
/// Only `datapacks/vanilla` is linked when present so repository test datapacks
/// (such as `modtest`/`testpack`) are not loaded into player singleplayer worlds.
fn prepare_world_dir(world_dir: &Path, runtime_root: &Path) -> Result<(), String> {
    let runtime_root = canonicalize_best_effort(runtime_root.to_path_buf());
    fs::create_dir_all(world_dir)
        .map_err(|e| format!("cannot create world dir {}: {e}", world_dir.display()))?;
    for name in ["assets", "config_packets.bin"] {
        link_or_copy(&runtime_root.join(name), &world_dir.join(name))
            .map_err(|e| format!("cannot link {} into {}: {e}", name, world_dir.display()))?;
    }
    let vanilla_dp = runtime_root.join("datapacks").join("vanilla");
    let world_dp = world_dir.join("datapacks");
    if vanilla_dp.is_dir() {
        // If a previous launch created a broken symlink at `world_dir/datapacks`,
        // clear it before creating the real `datapacks/` directory.
        if let Ok(meta) = world_dp.symlink_metadata() {
            if meta.file_type().is_symlink() && !world_dp.exists() {
                let _ = fs::remove_file(&world_dp);
            }
        }
        let is_symlink = world_dp
            .symlink_metadata()
            .is_ok_and(|m| m.file_type().is_symlink());
        if !is_symlink && (world_dp.symlink_metadata().is_err() || world_dp.is_dir()) {
            fs::create_dir_all(&world_dp).map_err(|e| {
                format!("cannot create datapacks dir {}: {e}", world_dp.display())
            })?;
            link_or_copy(&vanilla_dp, &world_dp.join("vanilla")).map_err(|e| {
                format!(
                    "cannot link datapacks/vanilla into {}: {e}",
                    world_dir.display()
                )
            })?;
        }
    } else {
        link_or_copy(&runtime_root.join("datapacks"), &world_dp)
            .map_err(|e| format!("cannot link datapacks into {}: {e}", world_dir.display()))?;
    }
    Ok(())
}

/// Resolve `<gameDir>/mods` for the integrated singleplayer server when that
/// directory exists, canonicalized so the child sees a valid absolute path
/// regardless of its `current_dir(&world_dir)`.
pub fn resolve_mods_dir(game_dir: &Path) -> Option<PathBuf> {
    let mods = game_dir.join("mods");
    mods.is_dir().then(|| canonicalize_best_effort(mods))
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

    let game_dir = cfg.game_dir.clone().unwrap_or_else(|| PathBuf::from("."));
    let server_bin = resolve_server_bin(cfg.server_bin.as_deref())?;
    let runtime_root = find_runtime_root(&server_bin, &game_dir)
        .ok_or_else(|| runtime_root_error(&server_bin, &game_dir))?;

    let world_dir = world_save_dir(&game_dir, world)?;
    prepare_world_dir(&world_dir, &runtime_root)?;

    // Ephemeral loopback port: bind :0, read the assigned port, release.
    let port = allocate_loopback_port()?;

    let mut command = Command::new(&server_bin);
    command
        .current_dir(&world_dir)
        .env("MEGU_BIND_ADDR", INTEGRATED_BIND_IP.to_string())
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
    if env::var_os("MEGU_MODS_DIR").is_none() {
        if let Some(mods_dir) = resolve_mods_dir(&game_dir) {
            command.env("MEGU_MODS_DIR", mods_dir);
        }
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

    let addr = SocketAddr::from((INTEGRATED_BIND_IP, port));
    let deadline = Instant::now() + READY_TIMEOUT;
    loop {
        if let Ok(Some(status)) = child.try_wait() {
            return Err(format!(
                "megu server exited during startup (status {status}) for world '{world}'"
            ));
        }
        if TcpStream::connect_timeout(&addr, Duration::from_millis(250)).is_ok() {
            // Best-effort instant-resume record: the last world the
            // integrated server actually reached is what a later
            // `resolve_resume` offers — a failed store write must never
            // block a running server.
            let _ = crate::seamless_runtime::InstantResumeStore::new(
                game_dir.join("megu-resume.json"),
            )
            .save(
                crate::seamless_runtime::ResumeTarget::SingleplayerWorld {
                    world_dir: world_dir.clone(),
                    display_name: world.to_string(),
                },
                None,
            );
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

    fn synthetic_root(name: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "megu-integrated-{name}-{}-{nanos}",
            std::process::id()
        ))
    }

    fn write_complete_runtime(root: &Path) {
        fs::create_dir_all(root.join("assets")).unwrap();
        fs::write(root.join("assets").join("block_states_1211.json"), b"{}").unwrap();
        fs::write(root.join("config_packets.bin"), b"cfg").unwrap();
        fs::create_dir_all(root.join("datapacks").join("vanilla")).unwrap();
        fs::write(
            root.join("datapacks").join("vanilla").join("pack.mcmeta"),
            b"{}",
        )
        .unwrap();
    }

    #[test]
    fn runtime_root_discovers_bundle_staged_under_game_dir() {
        let root = synthetic_root("bundle");
        let game_dir = root.join("game");
        let bin_dir = game_dir.join("bin");
        fs::create_dir_all(&bin_dir).unwrap();
        let server_bin = bin_dir.join("megu");
        fs::write(&server_bin, b"stub").unwrap();
        let bundle = game_dir.join("megu-runtime").join("megu-0.1-test");
        write_complete_runtime(&bundle);

        let found = find_runtime_root(&server_bin, &game_dir).expect("bundle runtime root");
        assert_eq!(found, canonicalize_best_effort(bundle));
    }

    #[test]
    fn runtime_root_discovers_bin_staged_runtime() {
        let root = synthetic_root("bin");
        let game_dir = root.join("game");
        let bin_dir = game_dir.join("bin");
        fs::create_dir_all(&bin_dir).unwrap();
        let server_bin = bin_dir.join("megu");
        fs::write(&server_bin, b"stub").unwrap();
        write_complete_runtime(&bin_dir);

        let found = find_runtime_root(&server_bin, &game_dir).expect("bin runtime root");
        assert_eq!(found, canonicalize_best_effort(bin_dir));
    }

    #[test]
    fn runtime_root_discovers_game_dir_staged_runtime() {
        let root = synthetic_root("game-dir");
        let game_dir = root.join("game");
        let bin_dir = game_dir.join("bin");
        fs::create_dir_all(&bin_dir).unwrap();
        let server_bin = bin_dir.join("megu");
        fs::write(&server_bin, b"stub").unwrap();
        write_complete_runtime(&game_dir);

        let found = find_runtime_root(&server_bin, &game_dir).expect("game dir runtime root");
        assert_eq!(found, canonicalize_best_effort(game_dir));
    }

    #[test]
    fn runtime_root_skips_incomplete_roots_and_does_not_recurse_bundles() {
        let root = synthetic_root("incomplete");
        let game_dir = root.join("game");
        let bin_dir = game_dir.join("bin");
        fs::create_dir_all(&bin_dir).unwrap();
        let server_bin = bin_dir.join("megu");
        fs::write(&server_bin, b"stub").unwrap();

        fs::write(bin_dir.join("config_packets.bin"), b"cfg").unwrap();
        fs::create_dir_all(game_dir.join("assets")).unwrap();
        fs::write(
            game_dir.join("assets").join("block_states_1211.json"),
            b"{}",
        )
        .unwrap();

        let runtime_dir = game_dir.join("megu-runtime");
        let incomplete_bundle = runtime_dir.join("a-incomplete");
        fs::create_dir_all(incomplete_bundle.join("assets")).unwrap();
        fs::write(
            incomplete_bundle
                .join("assets")
                .join("block_states_1211.json"),
            b"{}",
        )
        .unwrap();
        fs::write(incomplete_bundle.join("config_packets.bin"), b"cfg").unwrap();
        write_complete_runtime(&incomplete_bundle.join("nested-complete"));

        let complete_bundle = runtime_dir.join("b-complete");
        write_complete_runtime(&complete_bundle);

        let found = find_runtime_root(&server_bin, &game_dir).expect("complete bundle root");
        assert_eq!(found, canonicalize_best_effort(complete_bundle));
    }

    #[test]
    fn runtime_root_failure_is_closed_and_actionable() {
        let root = synthetic_root("error");
        let game_dir = root.join("game");
        let bin_dir = game_dir.join("bin");
        fs::create_dir_all(&bin_dir).unwrap();
        let server_bin = bin_dir.join("megu");
        fs::write(&server_bin, b"stub").unwrap();
        fs::write(bin_dir.join("config_packets.bin"), b"cfg").unwrap();

        assert!(find_runtime_root(&server_bin, &game_dir).is_none());
        let error = runtime_root_error(&server_bin, &game_dir);
        assert!(error.contains("could not find a complete megu runtime root"));
        assert!(error.contains("config_packets.bin"));
        assert!(error.contains("datapacks/"));
        assert!(error.contains("assets/block_states_1211.json"));
        assert!(error.contains(&bin_dir.display().to_string()));
        assert!(error.contains("megu-runtime"));
        assert!(error.contains("missing datapacks/, assets/block_states_1211.json"));
        assert!(error.contains("incomplete roots are skipped"));
    }

    #[test]
    fn launch_fails_closed_when_runtime_root_is_incomplete() {
        let root = synthetic_root("launch-error");
        let game_dir = root.join("game");
        let bin_dir = game_dir.join("bin");
        fs::create_dir_all(&bin_dir).unwrap();
        let server_bin = bin_dir.join("megu");
        fs::write(&server_bin, b"stub").unwrap();
        fs::write(bin_dir.join("config_packets.bin"), b"cfg").unwrap();

        let mut cfg = CliConfig::default();
        cfg.quick_play_singleplayer = Some("world".to_string());
        cfg.game_dir = Some(game_dir.clone());
        cfg.server_bin = Some(server_bin);
        let error = match launch(&cfg) {
            Ok(_) => panic!("launch must fail closed without a complete runtime root"),
            Err(error) => error,
        };
        assert!(error.contains("could not find a complete megu runtime root"));
        assert!(error.contains("missing datapacks/, assets/block_states_1211.json"));
        assert!(
            !game_dir.join("saves").exists(),
            "launch must fail before creating or spawning in the world directory"
        );
    }

    #[test]
    fn runtime_root_candidate_order_is_deterministic() {
        let root = synthetic_root("order");
        let external_root = root.join("external-runtime");
        let external_bin = external_root.join("bin");
        fs::create_dir_all(&external_bin).unwrap();
        let server_bin = external_bin.join("megu");
        fs::write(&server_bin, b"stub").unwrap();

        let game_dir = root.join("game");
        let game_bin = game_dir.join("bin");
        fs::create_dir_all(&game_bin).unwrap();
        let a_bundle = game_dir.join("megu-runtime").join("a-bundle");
        let z_bundle = game_dir.join("megu-runtime").join("z-bundle");
        fs::create_dir_all(&z_bundle).unwrap();
        fs::create_dir_all(&a_bundle).unwrap();

        let candidates = runtime_root_candidates(&server_bin, &game_dir);
        let position = |path: &Path| {
            let path = canonicalize_best_effort(path.to_path_buf());
            candidates
                .iter()
                .position(|candidate| candidate == &path)
                .unwrap_or_else(|| panic!("{} missing from candidates", path.display()))
        };
        assert!(position(&external_bin) < position(&external_root));
        assert!(position(&external_root) < position(&game_bin));
        assert!(position(&game_bin) < position(&game_dir));
        assert!(position(&game_dir) < position(&a_bundle));
        assert!(position(&a_bundle) < position(&z_bundle));
    }

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

    #[test]
    fn resolve_server_bin_canonicalizes_and_prepare_world_dir_links_only_vanilla_datapack() {
        let root = std::env::temp_dir().join(format!(
            "megu-integrated-test-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        let runtime = root.join("runtime");
        let bin = runtime.join("target").join("release").join("megu");
        fs::create_dir_all(bin.parent().unwrap()).unwrap();
        fs::write(&bin, b"stub").unwrap();
        fs::create_dir_all(runtime.join("assets")).unwrap();
        fs::write(runtime.join("assets").join("block_states_1211.json"), b"{}").unwrap();
        fs::write(runtime.join("config_packets.bin"), b"cfg").unwrap();
        fs::create_dir_all(runtime.join("datapacks").join("vanilla")).unwrap();
        fs::write(
            runtime.join("datapacks").join("vanilla").join("pack.mcmeta"),
            b"{}",
        )
        .unwrap();
        fs::create_dir_all(runtime.join("datapacks").join("testpack")).unwrap();
        fs::write(
            runtime.join("datapacks").join("testpack").join("pack.mcmeta"),
            b"{}",
        )
        .unwrap();

        let resolved = resolve_server_bin(Some(&bin)).unwrap();
        assert!(resolved.is_absolute());
        let found = find_runtime_root(&resolved, &root).expect("runtime root should be found");
        assert_eq!(found, canonicalize_best_effort(runtime.clone()));

        let world_dir = root.join("saves").join("TestWorld");
        prepare_world_dir(&world_dir, &found).unwrap();
        assert!(world_dir.join("assets").join("block_states_1211.json").is_file());
        assert!(world_dir.join("config_packets.bin").is_file());
        assert!(world_dir.join("datapacks").join("vanilla").join("pack.mcmeta").is_file());
        assert!(
            !world_dir.join("datapacks").join("testpack").exists(),
            "repository test datapacks must not be linked into singleplayer worlds"
        );
        assert_eq!(resolve_mods_dir(&root), None);
        fs::create_dir_all(root.join("mods")).unwrap();
        assert_eq!(
            resolve_mods_dir(&root),
            Some(canonicalize_best_effort(root.join("mods")))
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn integrated_host_never_claims_dedicated_port_or_escapes_saves() {
        // Loopback only — never the wildcard — and always an OS-assigned
        // ephemeral port, so an integrated world can never collide with a
        // dedicated `megu` server on 25565 on the same machine.
        assert!(INTEGRATED_BIND_IP.is_loopback());
        assert!(!INTEGRATED_BIND_IP.is_unspecified());
        for _ in 0..16 {
            let port = allocate_loopback_port().unwrap();
            assert_ne!(port, DEDICATED_SERVER_PORT);
        }

        // Worlds live strictly under <gameDir>/saves/<world>; every
        // separator, traversal segment and dot/control name is rejected
        // before the path is ever joined.
        let game_dir = Path::new("megu-game-dir");
        let world_dir = world_save_dir(game_dir, "My World").unwrap();
        assert_eq!(world_dir, game_dir.join("saves").join("My World"));
        assert!(world_dir.starts_with(game_dir.join("saves")));
        for bad in [
            "..",
            ".",
            "",
            "../escape",
            "a/b",
            "a\\b",
            "saves/../outside",
            "nul\0byte",
            "ctrl\u{0007}char",
        ] {
            assert!(
                world_save_dir(game_dir, bad).is_err(),
                "world name {bad:?} must be rejected"
            );
        }
    }
}
