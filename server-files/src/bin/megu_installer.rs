// SPDX-License-Identifier: LGPL-3.0-or-later
//! `megu-installer`: the native installer — Official Launcher,
//! Prism Launcher and dedicated-server lanes.
//!
//! Pure `std` plus `megu-core`: the installer lanes share the crate's
//! zero-dependency contract, so profile/instance setup runs on a Windows box
//! with no Java and broken `.jar` associations exactly like the Fabric `.exe`
//! installer. Explicit integrated-singleplayer runtime-data generation
//! separately uses Python 3 plus Java 21 on the user's pinned official jar.
//!
//! Exit codes: `0` success, `2` bad command line, `1` a file-level
//! failure (unsafe or unreadable target).

#![deny(unsafe_code)]

use megu_core::version::TargetVersion;
use megu_server_files::installer::{
    self, ClientInstallOptions, InstallReport, ServerInstallOptions,
};
use megu_server_files::json::Json;
use std::io;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

const HELP: &str = "megu-installer — install Megu like the Fabric/NeoForge installers\n\
\n\
USAGE:\n\
    megu-installer [client|gui] [--minecraft-dir <dir>] [--mc-version 1.21.1|26.3]\n\
                   [--client-bin <path>] [--client-sha256 <hex>] [--bootstrap-jar <jar>]\n\
                   [--no-profile] [--json]\n\
    megu-installer prism [--instances-dir <dir>] [--mc-version 1.21.1|26.3]\n\
                   [--client-bin <path>] [--client-sha256 <hex>] [--bootstrap-jar <jar>] [--json]\n\
    megu-installer server --dir <dir> [--mc-version 1.21.1|26.3] [--server-bin <path>] [--json]\n\
    megu-installer uninstall [--minecraft-dir <dir>] [--mc-version 1.21.1|26.3] [--no-profile] [--json]\n\
    megu-installer --gui\n\
    megu-installer -h | --help\n\
    megu-installer --version\n\
\n\
COMMANDS:\n\
    client      Install into the Official Launcher (the default when no\n\
                command is given): writes versions/megu-<mc>/megu-<mc>.json,\n\
                stages the bootstrapper library, and registers a\n\
                \"Megu (<mc>)\" profile in launcher_profiles.json (plus the\n\
                Windows Store profile file when present, with a .bak backup).\n\
    gui         Open the native graphical installer dialog (Official Launcher,\n\
                Prism Launcher, Dedicated Server, or Uninstall).\n\
    prism       Create a Megu-<mc> Prism Launcher instance: instance.cfg,\n\
                mmc-pack.json, the standard .minecraft client folders, an\n\
                optional bootstrapper patch, and an optional native\n\
                WrapperCommand pin.\n\
    server      Initialize a dedicated server directory: standard layout,\n\
                eula.txt with eula=false (the EULA is never auto-accepted),\n\
                run.sh / run.bat launch scripts.\n\
    uninstall   Remove the megu-<mc> version directory and its launcher\n\
                profile entries; every other profile is preserved.\n\
\n\
INTEGRATED SINGLEPLAYER RUNTIME DATA:\n\
    config_packets.bin, the canonical physical block-state asset and the\n\
    vanilla datapack are required by default. If they are missing the\n\
    installer fails closed instead of creating a half-working profile.\n\
    Generation is opt-in through --accept-eula plus a user-supplied or\n\
    Mojang-downloaded pinned 1.21.1 server jar. It uses Python 3 and Java 21\n\
    locally; --no-runtime-data is the explicit multiplayer-only escape.\n\
\n\
OPTIONS:\n\
    --minecraft-dir <dir>  Minecraft directory (default: the OS-standard one)\n\
    --instances-dir <dir>  Prism instances directory (default: OS-standard)\n\
    --dir <dir>            Dedicated server directory (required for server)\n\
    --mc-version <id>      1.21.1 or 26.3 (default: 1.21.1)\n\
    --bootstrap-jar <jar>  Stage a megu-bootstrapper.jar under libraries/\n\
    --client-bin <path>    Pin a native client binary (SHA-256 is computed\n\
                           automatically and written into the profile)\n\
    --client-sha256 <hex>  Expected SHA-256 of --client-bin (64 hex digits)\n\
    --server-bin <path>    Stage a native server binary as megu/megu.exe\n\
    --vanilla-server-jar <path>\n\
                           Use your official 1.21.1 server jar to generate\n\
                           integrated-singleplayer runtime data\n\
    --download-vanilla-server\n\
                           Download that jar from Mojang's pinned object URL\n\
                           when no user-supplied/cached copy is available;\n\
                           also permits pinned CC0 mapping downloads needed\n\
                           for physical block-state generation\n\
    --accept-eula          You accept https://aka.ms/MinecraftEULA and allow\n\
                           one local offline vanilla-server run to capture\n\
                           registry data (never implied or automatic)\n\
    --no-runtime-data      Explicitly install without integrated-singleplayer\n\
                           runtime data (multiplayer-only; not the default)\n\
    --no-profile           Skip launcher_profiles.json edits\n\
    --json                 Print the report as JSON instead of text\n";

const VERSION: &str = concat!("megu-installer ", env!("CARGO_PKG_VERSION"));

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Command {
    Client,
    Prism,
    Server,
    Uninstall,
}

impl Command {
    fn name(self) -> &'static str {
        match self {
            Self::Client => "client",
            Self::Prism => "prism",
            Self::Server => "server",
            Self::Uninstall => "uninstall",
        }
    }
}

struct Cli {
    command: Command,
    minecraft_dir: Option<PathBuf>,
    instances_dir: Option<PathBuf>,
    dir: Option<PathBuf>,
    mc_version: TargetVersion,
    client_bin: Option<PathBuf>,
    client_sha256: Option<String>,
    bootstrap_jar: Option<PathBuf>,
    server_bin: Option<PathBuf>,
    vanilla_server_jar: Option<PathBuf>,
    download_vanilla_server: bool,
    accept_eula: bool,
    no_runtime_data: bool,
    no_profile: bool,
    json: bool,
}

fn path_value(flag: &str, value: &str) -> Result<PathBuf, String> {
    if value.is_empty() || value.contains('\0') || value.starts_with('-') {
        return Err(format!(
            "{flag} requires a nonempty path without NUL bytes or leading '-' (prefix such paths with ./)"
        ));
    }
    Ok(PathBuf::from(value))
}

fn parse(args: &[String]) -> Result<Cli, String> {
    let (command, rest) = match args.first().map(String::as_str) {
        Some("client") => (Command::Client, &args[1..]),
        Some("prism") => (Command::Prism, &args[1..]),
        Some("server") => (Command::Server, &args[1..]),
        Some("uninstall") => (Command::Uninstall, &args[1..]),
        _ => (Command::Client, args),
    };
    let mut cli = Cli {
        command,
        minecraft_dir: None,
        instances_dir: None,
        dir: None,
        mc_version: TargetVersion::V1_21_1,
        client_bin: None,
        client_sha256: None,
        bootstrap_jar: None,
        server_bin: None,
        vanilla_server_jar: None,
        download_vanilla_server: false,
        accept_eula: false,
        no_runtime_data: false,
        no_profile: false,
        json: false,
    };
    let mut seen = std::collections::HashSet::new();
    let mut index = 0;
    while index < rest.len() {
        let flag = rest[index].as_str();
        if !seen.insert(flag.to_owned()) {
            return Err(format!("duplicate option {flag}"));
        }
        let allowed = match flag {
            "--minecraft-dir" => matches!(command, Command::Client | Command::Uninstall),
            "--instances-dir" => command == Command::Prism,
            "--dir" => command == Command::Server,
            "--client-bin" | "--client-sha256" | "--bootstrap-jar" => {
                matches!(command, Command::Client | Command::Prism)
            }
            "--server-bin" => command == Command::Server,
            "--vanilla-server-jar"
            | "--download-vanilla-server"
            | "--accept-eula"
            | "--no-runtime-data" => {
                matches!(command, Command::Client | Command::Prism | Command::Server)
            }
            "--mc-version" | "--json" => true,
            "--no-profile" => matches!(command, Command::Client | Command::Uninstall),
            _ => false,
        };
        if !allowed {
            return Err(format!(
                "unrecognized argument {flag:?} for {}",
                command.name()
            ));
        }
        match flag {
            "--minecraft-dir"
            | "--instances-dir"
            | "--dir"
            | "--client-bin"
            | "--client-sha256"
            | "--bootstrap-jar"
            | "--server-bin"
            | "--vanilla-server-jar"
            | "--mc-version" => {
                index += 1;
                let value = rest
                    .get(index)
                    .ok_or_else(|| format!("{flag} requires a value"))?;
                match flag {
                    "--minecraft-dir" => cli.minecraft_dir = Some(path_value(flag, value)?),
                    "--instances-dir" => cli.instances_dir = Some(path_value(flag, value)?),
                    "--dir" => cli.dir = Some(path_value(flag, value)?),
                    "--client-bin" => cli.client_bin = Some(path_value(flag, value)?),
                    "--bootstrap-jar" => cli.bootstrap_jar = Some(path_value(flag, value)?),
                    "--server-bin" => cli.server_bin = Some(path_value(flag, value)?),
                    "--vanilla-server-jar" => {
                        cli.vanilla_server_jar = Some(path_value(flag, value)?)
                    }
                    "--client-sha256" => {
                        cli.client_sha256 = Some(
                            installer::normalize_sha256_hex(value)
                                .map_err(|err| err.to_string())?,
                        );
                    }
                    "--mc-version" => {
                        cli.mc_version = TargetVersion::parse(value)
                            .ok_or_else(|| format!("unsupported --mc-version: {value}"))?;
                    }
                    _ => unreachable!(),
                }
            }
            "--download-vanilla-server" => cli.download_vanilla_server = true,
            "--accept-eula" => cli.accept_eula = true,
            "--no-runtime-data" => cli.no_runtime_data = true,
            "--no-profile" => cli.no_profile = true,
            "--json" => cli.json = true,
            _ => unreachable!(),
        }
        index += 1;
    }
    if command == Command::Server && cli.dir.is_none() {
        return Err("server requires --dir <path>".into());
    }
    if cli.no_runtime_data
        && (cli.accept_eula || cli.download_vanilla_server || cli.vanilla_server_jar.is_some())
    {
        return Err(
            "--no-runtime-data conflicts with --accept-eula, --download-vanilla-server and --vanilla-server-jar"
                .into(),
        );
    }
    Ok(cli)
}

fn report_json(
    command: Command,
    dir: &str,
    mc_version: TargetVersion,
    report: &InstallReport,
) -> Json {
    let actions = report
        .actions
        .iter()
        .map(|entry| {
            Json::Object(vec![
                (
                    "path".into(),
                    Json::String(entry.path.display().to_string()),
                ),
                ("action".into(), Json::String(entry.action.into())),
            ])
        })
        .collect();
    Json::Object(vec![
        ("command".into(), Json::String(command.name().into())),
        ("dir".into(), Json::String(dir.to_owned())),
        ("mc_version".into(), Json::String(mc_version.id().into())),
        ("summary".into(), Json::String(report.summary.clone())),
        ("actions".into(), Json::Array(actions)),
    ])
}

fn emit(cli: &Cli, dir: &str, report: &InstallReport) {
    if cli.json {
        println!(
            "{}",
            report_json(cli.command, dir, cli.mc_version, report).to_json_string()
        );
    } else {
        println!("{}", report.summary);
        for entry in &report.actions {
            println!("  {} {}", entry.action, entry.path.display());
        }
    }
}

fn auto_discover_sibling_in(
    explicit: &Option<PathBuf>,
    stem: &str,
    search_dirs: &[PathBuf],
) -> Option<PathBuf> {
    if explicit.is_some() {
        return explicit.clone();
    }
    let names: Vec<String> = if stem.ends_with(".jar") {
        vec![stem.to_owned()]
    } else if cfg!(windows) {
        vec![format!("{stem}.exe"), stem.to_owned()]
    } else {
        vec![stem.to_owned(), format!("{stem}.exe")]
    };
    for dir in search_dirs {
        for name in &names {
            let candidate = dir.join(name);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

fn auto_discover_sibling(explicit: &Option<PathBuf>, stem: &str) -> Option<PathBuf> {
    let mut dirs = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        if let Some(parent) = exe.parent() {
            dirs.push(parent.to_path_buf());
        }
    }
    if let Ok(cwd) = std::env::current_dir() {
        if !dirs.iter().any(|d| d == &cwd) {
            dirs.push(cwd);
        }
    }
    auto_discover_sibling_in(explicit, stem, &dirs)
}

const DEFAULT_RELEASE_TAG: &str = "v0.1.0-alpha.5";
const DEFAULT_GITHUB_REPO: &str = "nekomario28/megu-mc";

fn host_release_target_for(
    os: &str,
    arch: &str,
    env: &str,
) -> Option<(&'static str, &'static str)> {
    match (os, arch, env) {
        ("windows", "x86_64", _) => Some(("windows-x86_64", ".exe")),
        ("windows", "aarch64", _) => Some(("windows-arm64", ".exe")),
        ("linux", "x86_64", "musl") => Some(("linux-x86_64-musl", "")),
        ("linux", "aarch64", "musl") => Some(("linux-arm64-musl", "")),
        ("linux", "x86_64", _) => Some(("linux-x86_64", "")),
        ("linux", "aarch64", _) => Some(("linux-arm64", "")),
        ("macos", "aarch64", _) => Some(("macos-arm64", "")),
        ("macos", "x86_64", _) => Some(("macos-x86_64", "")),
        _ => None,
    }
}

fn host_release_target() -> Option<(&'static str, &'static str)> {
    // `target_env` is "musl" only for `*-linux-musl` builds; every other
    // target reports a non-musl value ("" or "gnu") and takes the glibc lane.
    let env = if cfg!(target_env = "musl") {
        "musl"
    } else {
        ""
    };
    host_release_target_for(std::env::consts::OS, std::env::consts::ARCH, env)
}

fn parse_expected_sha256(sums_text: &str, filename: &str) -> Option<String> {
    for line in sums_text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let mut parts = trimmed.split_whitespace();
        let digest = parts.next()?;
        let name = parts.next()?.trim_start_matches('*');
        if name == filename {
            return installer::normalize_sha256_hex(digest).ok();
        }
    }
    None
}

/// Download `url` to `dest`, preferring curl and falling back to wget so
/// minimal distros (Alpine, Void musl) that ship only BusyBox wget still
/// work. On Windows, `curl` may resolve nothing even though the OS-bundled
/// `curl.exe` exists in System32 (Windows 10 1803+), and PowerShell's
/// `Invoke-WebRequest` is the always-present last resort — both are tried
/// before wget so Windows users are never blocked by a missing PATH entry.
fn download_release_file(url: &str, dest: &std::path::Path) -> Option<()> {
    let curl_ok = std::process::Command::new("curl")
        .args(["-fsSL", url, "-o"])
        .arg(dest)
        .status()
        .map(|status| status.success())
        .unwrap_or(false);
    if curl_ok {
        return Some(());
    }
    #[cfg(windows)]
    {
        // The bundled curl.exe ships in System32 even when it is not on
        // PATH (e.g. older images or stripped environments).
        let curl_exe = std::env::var_os("SystemRoot")
            .map(|root| {
                std::path::Path::new(&root)
                    .join("System32")
                    .join("curl.exe")
            })
            .filter(|path| path.is_file())
            .unwrap_or_else(|| std::path::PathBuf::from(r"C:\Windows\System32\curl.exe"));
        if curl_exe.is_file() {
            let ok = std::process::Command::new(&curl_exe)
                .args(["-fsSL", url, "-o"])
                .arg(dest)
                .status()
                .map(|status| status.success())
                .unwrap_or(false);
            if ok {
                return Some(());
            }
        }
        // PowerShell 5 `Invoke-WebRequest` is present on every supported
        // Windows release; single-quoted literals with ''-escaped quotes
        // keep URL/path interpolation injection-free.
        let script = format!(
            "Invoke-WebRequest -UseBasicParsing -Uri '{}' -OutFile '{}'",
            url.replace('\'', "''"),
            dest.display().to_string().replace('\'', "''")
        );
        for shell in ["powershell", "pwsh"] {
            let ok = std::process::Command::new(shell)
                .args(["-NoProfile", "-NonInteractive", "-Command", &script])
                .status()
                .map(|status| status.success())
                .unwrap_or(false);
            if ok && dest.is_file() {
                return Some(());
            }
        }
    }
    let wget_ok = std::process::Command::new("wget")
        .arg("-q")
        .arg("-O")
        .arg(dest)
        .arg(url)
        .status()
        .map(|status| status.success())
        .unwrap_or(false);
    wget_ok.then_some(())
}

/// When `megu-installer` is run as a standalone downloaded executable (no local
/// sibling `megu-client-gpu` / `megu-bootstrapper.jar` next to it) against the
/// real OS default directory, automatically fetch and verify the release bundle
/// from GitHub Releases into `<base_dir>/megu-runtime/`.
fn auto_fetch_release_bundle(base_dir: &std::path::Path) -> Option<PathBuf> {
    if std::env::var_os("MEGU_NO_AUTO_FETCH").is_some() {
        return None;
    }
    let Some((target, suffix)) = host_release_target() else {
        eprintln!(
            "[megu-installer] Auto-download skipped: no GitHub Release bundle for {}/{}",
            std::env::consts::OS,
            std::env::consts::ARCH
        );
        return None;
    };
    let tag = std::env::var("MEGU_RELEASE_TAG").unwrap_or_else(|_| DEFAULT_RELEASE_TAG.to_owned());
    let repo = std::env::var("MEGU_GITHUB_REPO").unwrap_or_else(|_| DEFAULT_GITHUB_REPO.to_owned());
    let runtime_dir = base_dir.join("megu-runtime");
    let bundle_dir = runtime_dir.join(format!("megu-{tag}-{target}"));
    let client_name = format!("megu-client-gpu{suffix}");
    let jar_path = bundle_dir.join("megu-bootstrapper.jar");
    // The musl bundles ship only the server and this installer (no
    // megu-client-gpu), so a complete musl fetch is recognised by the jar alone.
    let server_only = target.ends_with("-musl");
    let bundle_complete = bundle_dir.is_dir()
        && jar_path.is_file()
        && (server_only || bundle_dir.join(&client_name).is_file());
    if bundle_complete {
        return Some(bundle_dir);
    }
    if let Err(err) = std::fs::create_dir_all(&runtime_dir) {
        eprintln!(
            "[megu-installer] Auto-download skipped: cannot create {}: {err}",
            runtime_dir.display()
        );
        return None;
    }
    let base_url = format!("https://github.com/{repo}/releases/download/{tag}");
    let archive_name = format!("megu-{tag}-{target}.tar.gz");
    let sums_path = runtime_dir.join("SHA256SUMS-all.txt");
    let archive_path = runtime_dir.join(&archive_name);
    let dl_jar_path = runtime_dir.join("megu-bootstrapper.jar");

    eprintln!("[megu-installer] Downloading Megu {tag} ({target}) from GitHub Releases...");
    for (remote, local) in [
        ("SHA256SUMS-all.txt", &sums_path),
        (archive_name.as_str(), &archive_path),
        ("megu-bootstrapper.jar", &dl_jar_path),
    ] {
        let url = format!("{base_url}/{remote}");
        if download_release_file(&url, local).is_none() {
            eprintln!(
                "[megu-installer] Auto-download skipped: failed to download {url} \
                 (install curl or wget and check network access, or set \
                 MEGU_NO_AUTO_FETCH=1 to disable auto-download)"
            );
            return None;
        }
    }
    let sums_text = match std::fs::read_to_string(&sums_path) {
        Ok(text) => text,
        Err(err) => {
            eprintln!(
                "[megu-installer] Auto-download skipped: cannot read {}: {err}",
                sums_path.display()
            );
            return None;
        }
    };
    for (fname, path) in [
        (archive_name.as_str(), &archive_path),
        ("megu-bootstrapper.jar", &dl_jar_path),
    ] {
        let Some(expected) = parse_expected_sha256(&sums_text, fname) else {
            eprintln!(
                "[megu-installer] Auto-download skipped: SHA256SUMS-all.txt has no entry for {fname}"
            );
            return None;
        };
        let actual = match installer::sha256_file(path) {
            Ok(digest) => digest,
            Err(err) => {
                eprintln!(
                    "[megu-installer] Auto-download skipped: cannot hash {}: {err}",
                    path.display()
                );
                return None;
            }
        };
        if actual != expected {
            eprintln!("[megu-installer] SHA-256 mismatch for {fname}");
            let _ = std::fs::remove_file(path);
            return None;
        }
    }
    let status = match std::process::Command::new("tar")
        .arg("-xzf")
        .arg(&archive_path)
        .arg("-C")
        .arg(&runtime_dir)
        .status()
    {
        Ok(status) => status,
        Err(err) => {
            eprintln!(
                "[megu-installer] Auto-download skipped: cannot run tar: {err} \
                 (install tar or set MEGU_NO_AUTO_FETCH=1 to disable auto-download)"
            );
            return None;
        }
    };
    if !status.success() || !bundle_dir.is_dir() {
        eprintln!(
            "[megu-installer] Auto-download skipped: tar failed to extract {archive_name} into {}",
            runtime_dir.display()
        );
        return None;
    }
    let _ = std::fs::copy(&dl_jar_path, &jar_path);
    let _ = std::fs::remove_file(&archive_path);
    Some(bundle_dir)
}

/// Stage `megu-client-gpu(.exe)` and sibling `megu(.exe)` into `<mc_dir>/bin/`
/// so the Official Launcher (including Flatpak on Linux, macOS Dock, and
/// Windows Store Launcher) and integrated singleplayer always find them inside
/// `--gameDir/bin/` even after the user cleans their `Downloads` folder.
fn stage_into_mc_bin_dir(mc_dir: &std::path::Path, client_bin: &PathBuf) -> Option<PathBuf> {
    let bin_dir = mc_dir.join("bin");
    std::fs::create_dir_all(&bin_dir).ok()?;
    let file_name = client_bin.file_name()?;
    let dest_client = bin_dir.join(file_name);
    if client_bin != &dest_client {
        if std::fs::symlink_metadata(&dest_client)
            .map(|m| m.file_type().is_symlink())
            .unwrap_or(false)
        {
            return None;
        }
        std::fs::copy(client_bin, &dest_client).ok()?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if let Ok(meta) = std::fs::metadata(&dest_client) {
                let mut perms = meta.permissions();
                perms.set_mode(0o755);
                let _ = std::fs::set_permissions(&dest_client, perms);
            }
        }
    }
    if let Some(parent) = client_bin.parent() {
        let srv_name = if cfg!(windows) { "megu.exe" } else { "megu" };
        let sibling_srv = parent.join(srv_name);
        let dest_srv = bin_dir.join(srv_name);
        if sibling_srv.is_file()
            && sibling_srv != dest_srv
            && !std::fs::symlink_metadata(&dest_srv)
                .map(|m| m.file_type().is_symlink())
                .unwrap_or(false)
        {
            let _ = std::fs::copy(&sibling_srv, &dest_srv);
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                if let Ok(meta) = std::fs::metadata(&dest_srv) {
                    let mut perms = meta.permissions();
                    perms.set_mode(0o755);
                    let _ = std::fs::set_permissions(&dest_srv, perms);
                }
            }
        }
    }
    Some(dest_client)
}

// ---------------------------------------------------------------------------
// Integrated-singleplayer runtime data (fail closed, clean-room local inputs)
// ---------------------------------------------------------------------------

const VANILLA_SERVER_SHA1: &str = "59353fb40c36d304f2035d51e7d6e6baa98dc05c";
const VANILLA_SERVER_URL: &str =
    "https://piston-data.mojang.com/v1/objects/59353fb40c36d304f2035d51e7d6e6baa98dc05c/server.jar";
const BLOCKS_JSON_SHA256: &str =
    "7d1d95c4e0255f9f269293e8077c7923c9ec0ff902a9a475ed131da8b26bc654";
const BLOCKS_JSON_URL: &str = "https://raw.githubusercontent.com/PrismarineJS/minecraft-data/d56346a4e32defcd28d9ba6750957c12001e78d6/data/pc/1.21.1/blocks.json";

// These are Megu's own clean-room generator/checker sources, embedded by
// reference (not copied) so the installer remains usable from a bare release
// binary. They contain no Mojang assets; at runtime they read only the user's
// pinned official server jar and write generated data into that user's game
// directory.
const GEN_VANILLA_DATA_PY: &str = include_str!("../../../tools/vanilla_data/gen_vanilla_data.py");
const VERSION_PACK_PY: &str = include_str!("../../../tools/vanilla_data/version_pack.py");
const CAPTURE_CONFIG_PACKETS_PY: &str =
    include_str!("../../../tools/vanilla_data/capture_config_packets.py");
const BUILD_VANILLA_DATA_PY: &str =
    include_str!("../../../tools/vanilla_data/build_vanilla_data.py");
const VERIFY_PACK_PY: &str = include_str!("../../../tools/vanilla_data/verify_pack.py");
const VANILLA_JAVA_PY: &str = include_str!("../../../tools/vanilla_java.py");
const VANILLA_STRUCTURE_ASSETS_PY: &str = include_str!("../../../tools/vanilla_structure_assets.py");
const BIOME_TREE_FROM_JAR_PY: &str = include_str!("../../../tools/biome_tree_from_jar.py");
const BIOME_TREE_EXPORT_JAVA: &str = include_str!("../../../tools/biome-tree/ExportBiomeTree.java");
const EXTRACT_JAVA: &str = include_str!("../../../tools/vanilla_data/Extract.java");
const EXPORT_BLOCK_REGISTRY_ASSETS_PY: &str =
    include_str!("../../../tools/vanilla_data/export_block_registry_assets.py");
const BLOCK_PREDICATE_ORACLE_PY: &str = include_str!("../../../tools/block_predicate_oracle.py");
const BLOCK_STATE_FACTS_FROM_JAR_PY: &str = include_str!("../../../tools/block_state_facts_from_jar.py");
const STRUCTURE_TRANSFORM_ORACLE_PY: &str = include_str!("../../../tools/structure_transform_oracle.py");
const BLOCK_PREDICATE_ORACLE_JAVA: &str =
    include_str!("../../../tools/block-predicate-oracle/BlockPredicateOracle.java");
const BLOCK_STATE_FACTS_ORACLE_JAVA: &str =
    include_str!("../../../tools/block-state-facts-oracle/BlockStateFactsOracle.java");
const MINESHAFT_SUPPORT_ORACLE_JAVA: &str =
    include_str!("../../../tools/structure-oracle/MineshaftSupportOracle.java");
const NATIVE_HARNESS_JAVA: &str = include_str!("../../../tools/structure-oracle/NativeHarness.java");
const STRUCTURE_TEMPLATE_ORACLE_JAVA: &str =
    include_str!("../../../tools/structure-oracle/StructureTemplateOracle.java");
const STRUCTURE_TRANSFORM_ORACLE_JAVA: &str =
    include_str!("../../../tools/structure-oracle/StructureTransformOracle.java");
const FEATURE_ORDER_ORACLE_JAVA: &str =
    include_str!("../../../tools/structure-oracle/FeatureOrderOracle.java");

const RUNTIME_REQUIRED: [&str; 22] = [
    "config_packets.bin",
    "assets/block_states_1211.json",
    "assets/blocks_1211.json",
    "assets/block_predicates_1211.bin",
    "assets/block_state_facts_1211.bin",
    "assets/collision_shape_full_1211.bin",
    "assets/mineshaft_support_1211.bin",
    "assets/multiface_support_1211.bin",
    "assets/structure_state_transforms_1211.bin",
    "assets/template_solid_1211.bin",
    "assets/feature_order_1211.json",
    "datapacks/vanilla/pack.mcmeta",
    "datapacks/vanilla/multi_noise_biome_tree.json",
    "datapacks/vanilla/structure-provenance.json",
    "datapacks/vanilla/data",
    "datapacks/vanilla/data/minecraft",
    "datapacks/vanilla/data/minecraft/worldgen",
    "datapacks/vanilla/data/minecraft/worldgen/biome",
    "datapacks/vanilla/data/minecraft/dimension_type",
    "datapacks/vanilla/data/minecraft/tags/block",
    "datapacks/vanilla/data/minecraft/tags/worldgen",
    "datapacks/vanilla/data/minecraft/structure",
];

fn invalid_runtime(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.into())
}

const BLOCK_COUNT: usize = 1_060;
const BLOCK_STATE_COUNT: usize = 26_684;

fn json_u64(value: &Json) -> Option<u64> {
    let n = value.as_f64()?;
    (n.is_finite() && n.fract() == 0.0 && n >= 0.0 && n <= u64::MAX as f64)
        .then_some(n as u64)
}

fn json_number(value: &Json) -> Option<f64> {
    value.as_f64().filter(|n| n.is_finite())
}

fn valid_minecraft_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 128
        && name.chars().all(|c| {
            c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '_' | '-' | '.')
        })
}

fn valid_namespaced_name(name: &str) -> bool {
    name.strip_prefix("minecraft:").is_some_and(valid_minecraft_name)
}

fn canonical_block_state_asset_valid(path: &Path) -> bool {
    let Ok(text) = std::fs::read_to_string(path) else {
        return false;
    };
    let limits = megu_core::json::JsonLimits {
        max_nodes: 1_000_000,
        max_bytes: 64 * 1024 * 1024,
        ..Default::default()
    };
    let Ok(asset) = megu_core::json::parse_json_with_limits(&text, limits, false) else {
        return false;
    };
    // The report-only generator index has `blocks` but none of the physical-facts
    // columns. The server registry consumes those columns for lighting, collision,
    // occlusion and state ordering, so they are part of the install contract.
    let numeric_column = |key| {
        asset
            .get(key)
            .and_then(Json::as_array)
            .filter(|values| !values.is_empty() && values.iter().all(|v| v.as_f64().is_some()))
    };
    let flags = numeric_column("flags");
    let luminance = numeric_column("luminance");
    let light_block = numeric_column("light_block");
    let full_sides = numeric_column("full_sides");
    let collision_shapes = asset.get("collision_shapes").and_then(Json::as_array);
    let shapes_valid = asset
        .get("shapes")
        .and_then(Json::as_array)
        .is_some_and(|shapes| {
            !shapes.is_empty()
                && shapes.iter().all(|shape| {
                    shape
                        .get("min")
                        .and_then(Json::as_array)
                        .is_some_and(|v| v.len() == 3)
                        && shape
                            .get("max")
                            .and_then(Json::as_array)
                            .is_some_and(|v| v.len() == 3)
                })
        });
    let blocks_valid = asset
        .get("blocks")
        .and_then(Json::as_array)
        .is_some_and(|blocks| {
            !blocks.is_empty()
                && blocks.iter().all(|row| {
                    row.as_array().is_some_and(|fields| {
                        fields.len() == 3
                            && fields[0].as_str().is_some()
                            && fields[1].as_f64().is_some()
                            && fields[2].as_f64().is_some()
                    })
                })
        });
    let column_len = flags.map(|v| v.len());
    blocks_valid
        && shapes_valid
        && column_len.is_some()
        && luminance.is_some_and(|v| Some(v.len()) == column_len)
        && light_block.is_some_and(|v| Some(v.len()) == column_len)
        && full_sides.is_some_and(|v| Some(v.len()) == column_len)
        && collision_shapes.is_some_and(|v| {
            Some(v.len()) == column_len
                && v.iter().all(|entry| {
                    entry
                        .as_array()
                        .is_some_and(|indexes| indexes.iter().all(|index| index.as_f64().is_some()))
                })
        })
}

fn pack_mcmeta_valid(path: &Path) -> bool {
    let Ok(text) = std::fs::read_to_string(path) else {
        return false;
    };
    let Ok(meta) = megu_server_files::json::parse(&text) else {
        return false;
    };
    meta.get("pack")
        .and_then(|pack| pack.get("pack_format"))
        .and_then(Json::as_f64)
        .is_some_and(|format| format == 48.0)
}

fn sha256_file(path: &Path) -> io::Result<String> {
    use std::io::Read;
    let mut file = std::fs::File::open(path)?;
    let mut hasher = megu_core::crypto::Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher.finalize_hex())
}

fn canonical_blocks_asset_semantics_valid(blocks: &[Json]) -> bool {
    if blocks.len() != BLOCK_COUNT {
        return false;
    }
    let mut expected_state = 0u64;
    let mut names = std::collections::HashSet::new();
    for (index, block) in blocks.iter().enumerate() {
        let required = [
            "id",
            "name",
            "hardness",
            "resistance",
            "stackSize",
            "defaultState",
            "minStateId",
            "maxStateId",
            "states",
            "drops",
        ];
        if required.iter().any(|key| block.get(key).is_none()) {
            return false;
        }
        if block.get("id").and_then(json_u64) != Some(index as u64) {
            return false;
        }
        let Some(name) = block.get("name").and_then(Json::as_str) else {
            return false;
        };
        if !valid_minecraft_name(name) || !names.insert(name.to_owned()) {
            return false;
        }
        if block.get("hardness").and_then(json_number).is_none()
            || block.get("resistance").and_then(json_number).is_none()
            || !matches!(block.get("stackSize").and_then(json_u64), Some(1 | 16 | 64))
        {
            return false;
        }
        let (Some(lo), Some(hi), Some(default)) = (
            block.get("minStateId").and_then(json_u64),
            block.get("maxStateId").and_then(json_u64),
            block.get("defaultState").and_then(json_u64),
        ) else {
            return false;
        };
        if lo != expected_state || hi < lo || default < lo || default > hi {
            return false;
        }
        let range_len = hi - lo + 1;
        let Some(states) = block.get("states").and_then(Json::as_array) else {
            return false;
        };
        let mut cardinality = 1u64;
        for property in states {
            let Some(prop_name) = property.get("name").and_then(Json::as_str) else {
                return false;
            };
            if !valid_minecraft_name(prop_name) {
                return false;
            }
            let values_len = match property.get("values").and_then(Json::as_array) {
                Some(values) => {
                    if values.is_empty()
                        || values.len() > 64
                        || !values.iter().all(|v| {
                            v.as_str().is_some_and(valid_minecraft_name)
                        })
                    {
                        return false;
                    }
                    values.len() as u64
                }
                None => 2,
            };
            if let Some(num_values) = property.get("num_values").and_then(json_u64) {
                if num_values != values_len {
                    return false;
                }
            }
            cardinality = cardinality.saturating_mul(values_len);
        }
        if cardinality != range_len {
            return false;
        }
        if !block.get("drops").and_then(Json::as_array).is_some_and(|drops| {
            drops
                .iter()
                .all(|drop| json_u64(drop).is_some())
        }) {
            return false;
        }
        expected_state = hi + 1;
    }
    if expected_state != BLOCK_STATE_COUNT as u64 {
        return false;
    }
    // Registry anchors reject the vacuous synthetic block0..block1059 shape
    // even when its ranges and property arithmetic are internally consistent.
    blocks
        .first()
        .and_then(|block| block.get("name"))
        .and_then(Json::as_str)
        == Some("air")
        && blocks
            .get(1)
            .and_then(|block| block.get("name"))
            .and_then(Json::as_str)
            == Some("stone")
        && blocks
            .last()
            .and_then(|block| block.get("name"))
            .and_then(Json::as_str)
            == Some("heavy_core")
}

fn canonical_blocks_asset_valid(path: &Path) -> bool {
    let Ok(text) = std::fs::read_to_string(path) else {
        return false;
    };
    let Ok(blocks) = megu_server_files::json::parse(&text) else {
        return false;
    };
    let Some(blocks) = blocks.as_array() else {
        return false;
    };
    canonical_blocks_asset_semantics_valid(blocks)
}

fn feature_order_asset_valid(path: &Path) -> bool {
    let Ok(text) = std::fs::read_to_string(path) else {
        return false;
    };
    let Ok(order) = megu_server_files::json::parse(&text) else {
        return false;
    };
    for (dimension, biome_count, step_count, feature_count) in [
        ("overworld", 53usize, 11usize, 151usize),
        ("nether", 5, 10, 37),
        ("end", 5, 11, 5),
    ] {
        let Some(dim) = order.get(dimension) else {
            return false;
        };
        let Some(biomes) = dim.get("biomes").and_then(Json::as_array) else {
            return false;
        };
        if biomes.len() != biome_count
            || !biomes.iter().all(|biome| {
                biome.as_str().is_some_and(valid_namespaced_name)
            })
        {
            return false;
        }
        let mut unique_biomes = std::collections::HashSet::new();
        if !biomes.iter().all(|biome| {
            unique_biomes.insert(biome.as_str().unwrap_or_default())
        }) {
            return false;
        }
        let Some(steps) = dim.get("steps").and_then(Json::as_array) else {
            return false;
        };
        if steps.len() != step_count {
            return false;
        }
        let mut features = std::collections::HashSet::new();
        for step in steps {
            let Some(list) = step.as_array() else {
                return false;
            };
            for feature in list {
                let Some(name) = feature.as_str() else {
                    return false;
                };
                if !valid_namespaced_name(name) || !features.insert(name) {
                    return false;
                }
            }
        }
        if features.len() != feature_count {
            return false;
        }
        if dimension == "overworld"
            && biomes.first().and_then(Json::as_str) != Some("minecraft:mushroom_fields")
        {
            return false;
        }
        if dimension == "end"
            && !features.contains("minecraft:end_island_decorated")
        {
            return false;
        }
    }
    true
}

fn binary_runtime_asset_valid(path: &Path, rel: &str) -> bool {
    let Ok(data) = std::fs::read(path) else {
        return false;
    };
    let expected = match rel {
        "assets/block_predicates_1211.bin"
        | "assets/collision_shape_full_1211.bin"
        | "assets/mineshaft_support_1211.bin"
        | "assets/multiface_support_1211.bin"
        | "assets/template_solid_1211.bin" => BLOCK_STATE_COUNT,
        "assets/block_state_facts_1211.bin" => BLOCK_STATE_COUNT * 6,
        "assets/structure_state_transforms_1211.bin" => BLOCK_STATE_COUNT * 12 * 4,
        _ => return false,
    };
    if data.len() != expected || data.len() < 2 {
        return false;
    }
    let distinct = data.iter().copied().collect::<std::collections::BTreeSet<_>>().len();
    match rel {
        "assets/block_predicates_1211.bin" => {
            data[0] == 0 && data[1] == 31 && distinct > 2
        }
        "assets/multiface_support_1211.bin" => {
            data.iter().all(|byte| *byte < 64) && data[0] == 0 && data[1] == 63 && distinct > 1
        }
        "assets/mineshaft_support_1211.bin" => data[0] == 0 && data[1] == 127 && distinct > 2,
        "assets/collision_shape_full_1211.bin" | "assets/template_solid_1211.bin" => {
            data.iter().all(|byte| matches!(*byte, 0 | 1))
                && data[0] == 0
                && data[1] == 1
                && distinct == 2
        }
        "assets/block_state_facts_1211.bin" => {
            data[0..6] == [161, 0, 0, 0, 0, 0]
                && data[6..12] == [220, 15, 255, 255, 3, 0]
                && distinct > 8
        }
        "assets/structure_state_transforms_1211.bin" => {
            data[..48].iter().all(|byte| *byte == 0)
                && data[48..96]
                    .as_chunks::<4>().0
                    .iter()
                    .all(|word| *word == [0, 0, 0, 1])
                && distinct > 8
        }
        _ => false,
    }
}

fn runtime_asset_file_valid(path: &Path, rel: &str) -> bool {
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return false;
    };
    if !meta.is_file() || meta.file_type().is_symlink() {
        return false;
    }
    match rel {
        "assets/blocks_1211.json" => canonical_blocks_asset_valid(path),
        "assets/feature_order_1211.json" => feature_order_asset_valid(path),
        _ => binary_runtime_asset_valid(path, rel),
    }
}

fn config_packets_valid(path: &Path) -> bool {
    std::fs::metadata(path)
        .map(|meta| meta.is_file() && meta.len() >= 1024)
        .unwrap_or(false)
}

fn runtime_path_present(root: &Path, rel: &str) -> bool {
    let path = root.join(rel);
    match std::fs::symlink_metadata(&path) {
        Ok(meta) if meta.file_type().is_symlink() => false,
        Ok(meta) if rel == "assets/block_states_1211.json" => {
            meta.is_file() && canonical_block_state_asset_valid(&path)
        }
        Ok(_) if rel == "config_packets.bin" => config_packets_valid(&path),
        Ok(meta) if rel == "datapacks/vanilla/pack.mcmeta" => {
            meta.is_file() && pack_mcmeta_valid(&path)
        }
        Ok(meta) if rel.starts_with("assets/") && (rel.ends_with(".json") || rel.ends_with(".bin")) => {
            meta.is_file() && runtime_asset_file_valid(&path, rel)
        }
        Ok(meta) if rel.ends_with(".json") || rel.ends_with(".bin") => meta.is_file(),
        Ok(meta) => meta.is_dir(),
        Err(_) => false,
    }
}

fn missing_runtime_data(root: &Path) -> Vec<&'static str> {
    RUNTIME_REQUIRED
        .iter()
        .copied()
        .filter(|rel| !runtime_path_present(root, rel))
        .collect()
}

fn runtime_data_partially_present(root: &Path) -> bool {
    RUNTIME_REQUIRED
        .iter()
        .any(|rel| std::fs::symlink_metadata(root.join(rel)).is_ok())
}

fn prepare_runtime_root(root: &Path) -> io::Result<()> {
    installer::reject_traversal(root)?;
    for ancestor in root.ancestors() {
        match std::fs::symlink_metadata(ancestor) {
            Ok(meta) if meta.file_type().is_symlink() || !meta.is_dir() => {
                return Err(invalid_runtime(format!(
                    "{} has an unsafe or incompatible file type",
                    ancestor.display()
                )));
            }
            Ok(_) => {}
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(err) => return Err(err),
        }
    }
    std::fs::create_dir_all(root)?;
    ensure_real_directory(root)
}

fn ensure_real_directory(path: &Path) -> io::Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() || !meta.is_dir() => Err(invalid_runtime(
            format!("{} has an unsafe or incompatible file type", path.display()),
        )),
        Ok(_) => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            std::fs::create_dir_all(path)?;
            ensure_real_directory(path)
        }
        Err(err) => Err(err),
    }
}

fn sha1_file(path: &Path) -> io::Result<String> {
    use std::io::Read;
    match std::fs::symlink_metadata(path)? {
        meta if meta.file_type().is_symlink() || !meta.is_file() => {
            return Err(invalid_runtime(format!(
                "{} has an unsafe or incompatible file type",
                path.display()
            )));
        }
        _ => {}
    }
    let mut file = std::fs::File::open(path)?;
    let mut hasher = megu_core::crypto::Sha1::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher.finalize_hex())
}

fn verified_vanilla_server_jar(path: &Path) -> io::Result<PathBuf> {
    let digest = sha1_file(path)?;
    if digest != VANILLA_SERVER_SHA1 {
        return Err(invalid_runtime(format!(
            "{} is not the pinned official 1.21.1 server jar (sha1 {}, expected {})",
            path.display(),
            digest,
            VANILLA_SERVER_SHA1
        )));
    }
    Ok(path.to_path_buf())
}

fn acquire_vanilla_server_jar(cli: &Cli, runtime_cache: &Path) -> io::Result<PathBuf> {
    if let Some(path) = cli
        .vanilla_server_jar
        .clone()
        .or_else(|| std::env::var_os("MEGU_VANILLA_SERVER_JAR").map(PathBuf::from))
    {
        return verified_vanilla_server_jar(&path);
    }

    let cache_jar = runtime_cache.join("server-1.21.1.jar");
    if cache_jar.is_file() {
        return verified_vanilla_server_jar(&cache_jar);
    }
    if !cli.download_vanilla_server {
        return Err(invalid_runtime(
            "integrated-singleplayer runtime data is missing. Provide the official 1.21.1 server jar with --vanilla-server-jar <path> (or MEGU_VANILLA_SERVER_JAR), or explicitly opt in with --download-vanilla-server; use --no-runtime-data only for a deliberate multiplayer-only install",
        ));
    }

    ensure_real_directory(runtime_cache)?;
    let part = runtime_cache.join(format!("server-1.21.1.jar.part-{}", std::process::id()));
    if download_release_file(VANILLA_SERVER_URL, &part).is_none() {
        let _ = std::fs::remove_file(&part);
        return Err(invalid_runtime(
            "failed to download the official Mojang 1.21.1 server jar",
        ));
    }
    match verified_vanilla_server_jar(&part) {
        Ok(_) => {
            if cache_jar.exists() {
                return Err(invalid_runtime(format!(
                    "refusing to replace existing cached jar {}",
                    cache_jar.display()
                )));
            }
            std::fs::rename(&part, &cache_jar)?;
            Ok(cache_jar)
        }
        Err(err) => {
            let _ = std::fs::remove_file(&part);
            Err(err)
        }
    }
}

fn acquire_blocks_json(cli: &Cli, runtime_cache: &Path) -> io::Result<PathBuf> {
    if let Some(path) = std::env::var_os("MEGU_BLOCKS_JSON").map(PathBuf::from) {
        let digest = sha256_file(&path)?;
        if digest != BLOCKS_JSON_SHA256 {
            return Err(invalid_runtime(format!(
                "{} is not the pinned minecraft-data 1.21.1 blocks.json (sha256 {}, expected {})",
                path.display(), digest, BLOCKS_JSON_SHA256
            )));
        }
        return Ok(path);
    }
    let cache = runtime_cache.join("blocks-1.21.1.json");
    if cache.is_file() {
        if sha256_file(&cache)? == BLOCKS_JSON_SHA256 {
            return Ok(cache);
        }
        return Err(invalid_runtime(format!(
            "cached {} has the wrong sha256; remove/repair it explicitly",
            cache.display()
        )));
    }
    if !cli.download_vanilla_server {
        return Err(invalid_runtime(
            "pinned minecraft-data blocks.json is required for block-registry generation. Provide it with MEGU_BLOCKS_JSON or explicitly opt in with --download-vanilla-server",
        ));
    }
    let part = runtime_cache.join(format!("blocks-1.21.1.json.part-{}", std::process::id()));
    if download_release_file(BLOCKS_JSON_URL, &part).is_none() {
        let _ = std::fs::remove_file(&part);
        return Err(invalid_runtime("failed to download pinned minecraft-data blocks.json"));
    }
    let digest = sha256_file(&part)?;
    if digest != BLOCKS_JSON_SHA256 {
        let _ = std::fs::remove_file(&part);
        return Err(invalid_runtime(format!(
            "downloaded blocks.json sha256 {digest}, expected {BLOCKS_JSON_SHA256}"
        )));
    }
    std::fs::rename(&part, &cache)?;
    Ok(cache)
}

fn write_embedded_python_tools(dir: &Path) -> io::Result<PathBuf> {
    ensure_real_directory(dir)?;
    for (name, source) in [
        ("gen_vanilla_data.py", GEN_VANILLA_DATA_PY),
        ("version_pack.py", VERSION_PACK_PY),
        ("capture_config_packets.py", CAPTURE_CONFIG_PACKETS_PY),
        ("build_vanilla_data.py", BUILD_VANILLA_DATA_PY),
        ("verify_pack.py", VERIFY_PACK_PY),
        ("vanilla_java.py", VANILLA_JAVA_PY),
        ("vanilla_structure_assets.py", VANILLA_STRUCTURE_ASSETS_PY),
        ("biome_tree_from_jar.py", BIOME_TREE_FROM_JAR_PY),
        ("ExportBiomeTree.java", BIOME_TREE_EXPORT_JAVA),
        ("Extract.java", EXTRACT_JAVA),
        ("export_block_registry_assets.py", EXPORT_BLOCK_REGISTRY_ASSETS_PY),
        ("block_predicate_oracle.py", BLOCK_PREDICATE_ORACLE_PY),
        ("block_state_facts_from_jar.py", BLOCK_STATE_FACTS_FROM_JAR_PY),
        ("structure_transform_oracle.py", STRUCTURE_TRANSFORM_ORACLE_PY),
        ("BlockPredicateOracle.java", BLOCK_PREDICATE_ORACLE_JAVA),
        ("BlockStateFactsOracle.java", BLOCK_STATE_FACTS_ORACLE_JAVA),
        ("MineshaftSupportOracle.java", MINESHAFT_SUPPORT_ORACLE_JAVA),
        ("NativeHarness.java", NATIVE_HARNESS_JAVA),
        ("StructureTemplateOracle.java", STRUCTURE_TEMPLATE_ORACLE_JAVA),
        ("StructureTransformOracle.java", STRUCTURE_TRANSFORM_ORACLE_JAVA),
        ("FeatureOrderOracle.java", FEATURE_ORDER_ORACLE_JAVA),
    ] {
        let path = dir.join(name);
        if let Ok(meta) = std::fs::symlink_metadata(&path) {
            if meta.file_type().is_symlink() || !meta.is_file() {
                return Err(invalid_runtime(format!(
                    "{} has an unsafe or incompatible file type",
                    path.display()
                )));
            }
        }
        let current = std::fs::read(&path).unwrap_or_default();
        if current != source.as_bytes() {
            std::fs::write(&path, source)?;
        }
    }
    Ok(dir.join("gen_vanilla_data.py"))
}

fn command_output_lossy(output: &std::process::Output) -> String {
    let mut text = String::new();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    if !stdout.trim().is_empty() {
        text.push_str(stdout.trim());
    }
    if !stderr.trim().is_empty() {
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str(stderr.trim());
    }
    if text.len() > 1200 {
        text.truncate(1200);
        text.push_str("...");
    }
    text
}

fn run_python_script(script: &Path, args: &[&str]) -> io::Result<()> {
    let candidates: Vec<(&str, Vec<&str>)> = if cfg!(windows) {
        vec![("py", vec!["-3"]), ("python", vec![]), ("python3", vec![])]
    } else {
        vec![("python3", vec![]), ("python", vec![])]
    };
    let mut not_found = Vec::new();
    for (program, prefix) in candidates {
        let mut command = std::process::Command::new(program);
        command.args(prefix).arg(script).args(args);
        match command.output() {
            Ok(output) if output.status.success() => return Ok(()),
            Ok(output) => {
                return Err(invalid_runtime(format!(
                    "{} failed while generating runtime data: {}",
                    script.display(),
                    command_output_lossy(&output)
                )));
            }
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                not_found.push(program.to_owned());
            }
            Err(err) => return Err(err),
        }
    }
    Err(invalid_runtime(format!(
        "Python 3 is required for first-run vanilla-data generation but none of [{}] was found",
        not_found.join(", ")
    )))
}

const RUNTIME_OUTPUT_MAPPINGS: [(&str, &str); 12] = [
    ("datapacks/vanilla", "datapacks/vanilla"),
    (
        "assets/block_states_1211.json",
        "assets/block_states_1211.json",
    ),
    ("config_packets.bin", "config_packets.bin"),
    ("assets/blocks_1211.json", "assets/blocks_1211.json"),
    ("assets/block_predicates_1211.bin", "assets/block_predicates_1211.bin"),
    ("assets/block_state_facts_1211.bin", "assets/block_state_facts_1211.bin"),
    ("assets/collision_shape_full_1211.bin", "assets/collision_shape_full_1211.bin"),
    ("assets/mineshaft_support_1211.bin", "assets/mineshaft_support_1211.bin"),
    ("assets/multiface_support_1211.bin", "assets/multiface_support_1211.bin"),
    (
        "assets/structure_state_transforms_1211.bin",
        "assets/structure_state_transforms_1211.bin",
    ),
    ("assets/template_solid_1211.bin", "assets/template_solid_1211.bin"),
    ("assets/feature_order_1211.json", "assets/feature_order_1211.json"),
];

fn unsafe_runtime_path(path: &Path) -> io::Error {
    invalid_runtime(format!(
        "{} has an unsafe or incompatible file type",
        path.display()
    ))
}

/// Validate every existing component below the runtime root with lstat
/// semantics. In particular, a pre-existing `datapacks -> outside` symlink is
/// rejected before generation starts and again before any output is moved.
fn validate_existing_components_below_root(root: &Path, rel: &str) -> io::Result<()> {
    let parts = Path::new(rel)
        .components()
        .map(|component| match component {
            std::path::Component::Normal(part) => Ok(part.to_owned()),
            _ => Err(invalid_runtime(format!(
                "runtime-data path {} is not a safe relative path",
                rel
            ))),
        })
        .collect::<io::Result<Vec<_>>>()?;
    let mut current = root.to_path_buf();
    for (index, part) in parts.iter().enumerate() {
        current.push(part);
        match std::fs::symlink_metadata(&current) {
            Ok(meta) if meta.file_type().is_symlink() => return Err(unsafe_runtime_path(&current)),
            Ok(meta) if index + 1 < parts.len() && !meta.is_dir() => {
                return Err(unsafe_runtime_path(&current));
            }
            Ok(_) => {}
            Err(err) if err.kind() == io::ErrorKind::NotFound => break,
            Err(err) => return Err(err),
        }
    }
    Ok(())
}

fn validate_runtime_output_components(root: &Path) -> io::Result<()> {
    match std::fs::symlink_metadata(root) {
        Ok(meta) if meta.file_type().is_symlink() || !meta.is_dir() => {
            return Err(unsafe_runtime_path(root));
        }
        Ok(_) => {}
        Err(err) => return Err(err),
    }
    for (_, rel) in RUNTIME_OUTPUT_MAPPINGS {
        validate_existing_components_below_root(root, rel)?;
    }
    Ok(())
}

/// Create `rel` below `root` one component at a time without ever using
/// create_dir_all through an unchecked intermediate. Concurrent creation of the
/// same real directory is fine; a symlink or non-directory never is.
fn ensure_real_directory_below_root(root: &Path, rel: &Path) -> io::Result<()> {
    let mut current = root.to_path_buf();
    for component in rel.components() {
        let std::path::Component::Normal(part) = component else {
            return Err(invalid_runtime(format!(
                "runtime-data directory {} is not a safe relative path",
                rel.display()
            )));
        };
        current.push(part);
        loop {
            match std::fs::symlink_metadata(&current) {
                Ok(meta) if meta.file_type().is_symlink() || !meta.is_dir() => {
                    return Err(unsafe_runtime_path(&current));
                }
                Ok(_) => break,
                Err(err) if err.kind() == io::ErrorKind::NotFound => {
                    match std::fs::create_dir(&current) {
                        Ok(()) => break,
                        Err(err) if err.kind() == io::ErrorKind::AlreadyExists => continue,
                        Err(err) => return Err(err),
                    }
                }
                Err(err) => return Err(err),
            }
        }
    }
    Ok(())
}

struct RuntimeOutputLock {
    path: PathBuf,
    _file: std::fs::File,
}

impl RuntimeOutputLock {
    fn acquire(runtime_cache: &Path) -> io::Result<Self> {
        let path = runtime_cache.join("runtime-output.lock");
        let mut file = match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(file) => file,
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {
                return Err(invalid_runtime(format!(
                    "another runtime-data move holds {}; refusing to race it",
                    path.display()
                )));
            }
            Err(err) => return Err(err),
        };
        use std::io::Write;
        let _ = writeln!(file, "pid {}", std::process::id());
        Ok(Self { path, _file: file })
    }
}

impl Drop for RuntimeOutputLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

fn move_file_no_replace(from: &Path, to: &Path) -> io::Result<()> {
    match std::fs::symlink_metadata(from)? {
        meta if meta.file_type().is_symlink() || !meta.is_file() => {
            return Err(unsafe_runtime_path(from));
        }
        _ => {}
    }
    match std::fs::hard_link(from, to) {
        Ok(()) => return std::fs::remove_file(from),
        Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {
            return Err(invalid_runtime(format!(
                "refusing to overwrite existing runtime-data path {}",
                to.display()
            )));
        }
        Err(_) => {}
    }
    // Some target filesystems do not support hard links. Exclusive creation
    // still provides the required no-replace kernel check; the source remains
    // in the installer-owned stage if a partial copy fails closed.
    let mut input = std::fs::File::open(from)?;
    let mut output = match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(to)
    {
        Ok(file) => file,
        Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {
            return Err(invalid_runtime(format!(
                "refusing to overwrite existing runtime-data path {}",
                to.display()
            )));
        }
        Err(err) => return Err(err),
    };
    io::copy(&mut input, &mut output)?;
    drop(output);
    std::fs::remove_file(from)
}

fn move_directory_no_replace(source: &Path, target: &Path) -> io::Result<()> {
    match std::fs::symlink_metadata(source)? {
        meta if meta.file_type().is_symlink() || !meta.is_dir() => {
            return Err(unsafe_runtime_path(source));
        }
        _ => {}
    }
    match std::fs::create_dir(target) {
        Ok(()) => {}
        Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {
            return Err(invalid_runtime(format!(
                "refusing to overwrite existing runtime-data path {}",
                target.display()
            )));
        }
        Err(err) => return Err(err),
    }
    let mut pending = vec![(source.to_path_buf(), target.to_path_buf())];
    while let Some((src_dir, dst_dir)) = pending.pop() {
        let mut entries = std::fs::read_dir(&src_dir)?.collect::<Result<Vec<_>, _>>()?;
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            let src = entry.path();
            let dst = dst_dir.join(entry.file_name());
            let meta = std::fs::symlink_metadata(&src)?;
            if meta.file_type().is_symlink() {
                return Err(unsafe_runtime_path(&src));
            }
            if meta.is_dir() {
                match std::fs::create_dir(&dst) {
                    Ok(()) => {}
                    Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {
                        return Err(invalid_runtime(format!(
                            "refusing to overwrite existing runtime-data path {}",
                            dst.display()
                        )));
                    }
                    Err(err) => return Err(err),
                }
                pending.push((src, dst));
            } else if meta.is_file() {
                move_file_no_replace(&src, &dst)?;
            } else {
                return Err(unsafe_runtime_path(&src));
            }
        }
    }
    std::fs::remove_dir_all(source)
}

fn move_runtime_output(stage: &Path, root: &Path) -> io::Result<()> {
    let runtime_cache = root.join("megu-runtime");
    let _lock = RuntimeOutputLock::acquire(&runtime_cache)?;
    validate_runtime_output_components(root)?;
    for (from_rel, to_rel) in RUNTIME_OUTPUT_MAPPINGS {
        let from = stage.join(from_rel);
        let to = root.join(to_rel);
        if std::fs::symlink_metadata(&from).is_err() {
            return Err(invalid_runtime(format!(
                "generator did not produce required runtime-data path {}",
                from_rel
            )));
        }
        if std::fs::symlink_metadata(&to).is_ok() {
            return Err(invalid_runtime(format!(
                "refusing to overwrite existing runtime-data path {}",
                to.display()
            )));
        }
        if let Some(parent) = to.parent() {
            let parent_rel = parent.strip_prefix(root).map_err(|_| {
                invalid_runtime(format!(
                    "runtime-data parent {} escapes {}",
                    parent.display(),
                    root.display()
                ))
            })?;
            ensure_real_directory_below_root(root, parent_rel)?;
        }
        // Parent creation can be concurrent with another writer even when that
        // writer does not cooperate with the lock, so re-lstat this output path
        // immediately before the no-replace creation.
        validate_existing_components_below_root(root, to_rel)?;
        let meta = std::fs::symlink_metadata(&from)?;
        if meta.is_dir() {
            move_directory_no_replace(&from, &to)?;
        } else if meta.is_file() {
            move_file_no_replace(&from, &to)?;
        } else {
            return Err(unsafe_runtime_path(&from));
        }
    }
    Ok(())
}

fn generate_runtime_data(cli: &Cli, root: &Path) -> io::Result<()> {
    prepare_runtime_root(root)?;
    let runtime_cache = root.join("megu-runtime");
    ensure_real_directory(&runtime_cache)?;
    validate_runtime_output_components(root)?;
    let tools_dir = runtime_cache.join("generator");
    let stage = runtime_cache.join(format!("data-stage-{}", std::process::id()));
    if stage.exists() || std::fs::symlink_metadata(&stage).is_ok() {
        return Err(invalid_runtime(format!(
            "refusing to reuse existing runtime-data staging directory {}",
            stage.display()
        )));
    }
    std::fs::create_dir_all(&stage)?;

    let jar = acquire_vanilla_server_jar(cli, &runtime_cache)?;
    let generator = write_embedded_python_tools(&tools_dir)?;
    let stage_string = stage.to_string_lossy().into_owned();
    let jar_string = jar.to_string_lossy().into_owned();
    let mappings_dir = std::env::var_os("MEGU_FABRIC_MAPPINGS")
        .map(PathBuf::from)
        .unwrap_or_else(|| runtime_cache.join("mappings"));
    if mappings_dir.exists() {
        ensure_real_directory(&mappings_dir)?;
    }
    let mappings_string = mappings_dir.to_string_lossy().into_owned();
    let mut generator_args = vec![
        "--jar".to_owned(),
        jar_string.clone(),
        "--out-dir".to_owned(),
        stage_string,
        "--full-runtime".to_owned(),
        "--mappings-dir".to_owned(),
        mappings_string,
    ];
    if cli.download_vanilla_server {
        generator_args.push("--fetch-mappings".to_owned());
    }
    let generator_arg_refs = generator_args
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>();
    run_python_script(&generator, &generator_arg_refs)?;

    let blocks_json = acquire_blocks_json(cli, &runtime_cache)?;
    let block_exporter = tools_dir.join("export_block_registry_assets.py");
    let stage_assets = stage.join("assets");
    let stage_assets_string = stage_assets.to_string_lossy().into_owned();
    let blocks_string = blocks_json.to_string_lossy().into_owned();
    run_python_script(
        &block_exporter,
        &[
            "--jar",
            &jar_string,
            "--blocks-json",
            &blocks_string,
            "--out-dir",
            &stage_assets_string,
        ],
    )?;
    let missing_before_capture = missing_runtime_data(&stage)
        .into_iter()
        .filter(|rel| *rel != "config_packets.bin")
        .collect::<Vec<_>>();
    if !missing_before_capture.is_empty() {
        return Err(invalid_runtime(format!(
            "generated runtime data is incomplete before packet capture; missing {}",
            missing_before_capture.join(", ")
        )));
    }

    let capture = tools_dir.join("capture_config_packets.py");
    let config_out = stage.join("config_packets.bin");
    let config_string = config_out.to_string_lossy().into_owned();
    run_python_script(&capture, &[&jar_string, &config_string, "--accept-eula"])?;

    let missing = missing_runtime_data(&stage);
    if !missing.is_empty() {
        return Err(invalid_runtime(format!(
            "generated runtime data is incomplete; missing {}",
            missing.join(", ")
        )));
    }
    move_runtime_output(&stage, root)?;
    // The stage is installer-owned scratch space. Keep it on failure for
    // diagnostics, but do not leave duplicate reports/test vectors behind after
    // the validated outputs have been moved into the runtime root.
    let _ = std::fs::remove_dir_all(&stage);
    Ok(())
}

fn ensure_runtime_data(cli: &Cli, root: &Path) -> io::Result<bool> {
    let missing = missing_runtime_data(root);
    if missing.is_empty() {
        return Ok(false);
    }
    if cli.no_runtime_data {
        eprintln!(
            "megu-installer: warning: installing without integrated-singleplayer runtime data because --no-runtime-data was explicit (missing {})",
            missing.join(", ")
        );
        return Ok(false);
    }
    if runtime_data_partially_present(root) {
        return Err(invalid_runtime(format!(
            "integrated-singleplayer runtime data at {} is incomplete (missing {}). Refusing to overwrite or silently continue from a half-install; remove/repair that data explicitly or rerun with --no-runtime-data for multiplayer-only",
            root.display(),
            missing.join(", ")
        )));
    }
    if !cli.accept_eula {
        return Err(invalid_runtime(format!(
            "integrated-singleplayer runtime data is missing ({}). Refusing a silent half-install. Rerun with --accept-eula plus --vanilla-server-jar <official-1.21.1-server.jar> (or --download-vanilla-server), or explicitly choose --no-runtime-data for multiplayer-only",
            missing.join(", ")
        )));
    }
    generate_runtime_data(cli, root)?;
    let remaining = missing_runtime_data(root);
    if remaining.is_empty() {
        Ok(true)
    } else {
        Err(invalid_runtime(format!(
            "runtime-data generation finished but validation still misses {}",
            remaining.join(", ")
        )))
    }
}

fn run(cli: &Cli) -> std::io::Result<()> {
    match cli.command {
        Command::Client => {
            let mc_dir = match &cli.minecraft_dir {
                Some(dir) => dir.clone(),
                None => installer::detect_minecraft_dir()?,
            };
            ensure_runtime_data(cli, &mc_dir)?;
            let mut bootstrap_jar =
                auto_discover_sibling(&cli.bootstrap_jar, "megu-bootstrapper.jar");
            let mut client_bin = auto_discover_sibling(&cli.client_bin, "megu-client-gpu");
            if cli.minecraft_dir.is_none()
                && !cli.no_profile
                && (bootstrap_jar.is_none() || client_bin.is_none())
            {
                if let Some(fetched_dir) = auto_fetch_release_bundle(&mc_dir) {
                    let dirs = vec![fetched_dir];
                    if bootstrap_jar.is_none() {
                        bootstrap_jar = auto_discover_sibling_in(
                            &cli.bootstrap_jar,
                            "megu-bootstrapper.jar",
                            &dirs,
                        );
                    }
                    if client_bin.is_none() {
                        client_bin =
                            auto_discover_sibling_in(&cli.client_bin, "megu-client-gpu", &dirs);
                    }
                }
            }
            if cli.minecraft_dir.is_none() && !cli.no_profile {
                if let Some(ref src_client) = client_bin {
                    if let Some(staged) = stage_into_mc_bin_dir(&mc_dir, src_client) {
                        client_bin = Some(staged);
                    }
                }
            }
            let options = ClientInstallOptions {
                mc_version: cli.mc_version,
                bootstrap_jar,
                client_bin,
                client_sha256: cli.client_sha256.clone(),
            };
            let report = installer::install_client(
                &mc_dir,
                &options,
                !cli.no_profile,
                &installer::iso_utc_now(),
            )?;
            emit(cli, &mc_dir.display().to_string(), &report);
            Ok(())
        }
        Command::Prism => {
            let instances = match &cli.instances_dir {
                Some(dir) => dir.clone(),
                None => installer::detect_prism_instances_dir()?,
            };
            let runtime_root = instances
                .join(installer::prism_instance_name(cli.mc_version))
                .join(".minecraft");
            ensure_runtime_data(cli, &runtime_root)?;
            let mut bootstrap_jar =
                auto_discover_sibling(&cli.bootstrap_jar, "megu-bootstrapper.jar");
            let mut client_bin = auto_discover_sibling(&cli.client_bin, "megu-client-gpu");
            if cli.instances_dir.is_none() && (bootstrap_jar.is_none() || client_bin.is_none()) {
                if let Some(fetched_dir) = auto_fetch_release_bundle(&instances) {
                    let dirs = vec![fetched_dir];
                    if bootstrap_jar.is_none() {
                        bootstrap_jar = auto_discover_sibling_in(
                            &cli.bootstrap_jar,
                            "megu-bootstrapper.jar",
                            &dirs,
                        );
                    }
                    if client_bin.is_none() {
                        client_bin =
                            auto_discover_sibling_in(&cli.client_bin, "megu-client-gpu", &dirs);
                    }
                }
            }
            let options = ClientInstallOptions {
                mc_version: cli.mc_version,
                bootstrap_jar,
                client_bin,
                client_sha256: cli.client_sha256.clone(),
            };
            let report = installer::install_prism(&instances, &options)?;
            emit(cli, &instances.display().to_string(), &report);
            Ok(())
        }
        Command::Server => {
            let dir = cli.dir.clone().unwrap_or_default();
            ensure_runtime_data(cli, &dir)?;
            let options = ServerInstallOptions {
                mc_version: cli.mc_version,
                server_bin: auto_discover_sibling(&cli.server_bin, "megu"),
            };
            let report = installer::install_server(&dir, &options)?;
            emit(cli, &dir.display().to_string(), &report);
            Ok(())
        }
        Command::Uninstall => {
            let mc_dir = match &cli.minecraft_dir {
                Some(dir) => dir.clone(),
                None => installer::detect_minecraft_dir()?,
            };
            let report = installer::uninstall_client(
                &mc_dir,
                cli.mc_version,
                !cli.no_profile,
                &installer::iso_utc_now(),
            )?;
            emit(cli, &mc_dir.display().to_string(), &report);
            Ok(())
        }
    }
}

fn parse_gui_selection(raw: &str) -> Result<Option<Cli>, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed == "CANCEL" {
        return Ok(None);
    }
    let (base, accept_eula) = match trimmed.rsplit_once('|') {
        Some((head, "0")) => (head, false),
        Some((head, "1")) => (head, true),
        _ => (trimmed, false),
    };
    let mut parts = base.splitn(3, '|');
    let mode = parts.next().unwrap_or("client").trim();
    let ver_str = parts.next().unwrap_or("1.21.1").trim();
    let target_dir = parts.next().unwrap_or("").trim();
    let mc_version = TargetVersion::parse(ver_str)
        .ok_or_else(|| format!("unsupported --mc-version: {ver_str}"))?;
    let path_opt = if target_dir.is_empty() {
        None
    } else {
        Some(PathBuf::from(target_dir))
    };
    let cli = match mode {
        "client" => Cli {
            command: Command::Client,
            minecraft_dir: path_opt,
            instances_dir: None,
            dir: None,
            mc_version,
            client_bin: None,
            client_sha256: None,
            bootstrap_jar: None,
            server_bin: None,
            vanilla_server_jar: None,
            download_vanilla_server: accept_eula,
            accept_eula,
            no_runtime_data: false,
            no_profile: false,
            json: false,
        },
        "prism" => Cli {
            command: Command::Prism,
            minecraft_dir: None,
            instances_dir: path_opt,
            dir: None,
            mc_version,
            client_bin: None,
            client_sha256: None,
            bootstrap_jar: None,
            server_bin: None,
            vanilla_server_jar: None,
            download_vanilla_server: accept_eula,
            accept_eula,
            no_runtime_data: false,
            no_profile: false,
            json: false,
        },
        "server" => Cli {
            command: Command::Server,
            minecraft_dir: None,
            instances_dir: None,
            dir: Some(path_opt.unwrap_or_else(|| PathBuf::from("./megu-server"))),
            mc_version,
            client_bin: None,
            client_sha256: None,
            bootstrap_jar: None,
            server_bin: None,
            vanilla_server_jar: None,
            download_vanilla_server: accept_eula,
            accept_eula,
            no_runtime_data: false,
            no_profile: false,
            json: false,
        },
        "uninstall" => Cli {
            command: Command::Uninstall,
            minecraft_dir: path_opt,
            instances_dir: None,
            dir: None,
            mc_version,
            client_bin: None,
            client_sha256: None,
            bootstrap_jar: None,
            server_bin: None,
            vanilla_server_jar: None,
            download_vanilla_server: accept_eula,
            accept_eula,
            no_runtime_data: false,
            no_profile: false,
            json: false,
        },
        other => return Err(format!("unknown GUI mode: {other}")),
    };
    Ok(Some(cli))
}

const TKINTER_GUI_SCRIPT: &str = r#"
import sys, tkinter as tk
from tkinter import ttk, filedialog

mc_dir, prism_dir, srv_dir = sys.argv[1], sys.argv[2], sys.argv[3]
root = tk.Tk()
root.title("Megu Installer")
root.geometry("500x320")
root.resizable(False, False)

pad = {"padx": 14, "pady": 6}
ttk.Label(root, text="Megu — Native Rust + wgpu Installer", font=("Sans", 12, "bold")).pack(anchor="w", padx=14, pady=(14, 4))

mode_var = tk.StringVar(value="client")
ver_var = tk.StringVar(value="1.21.1")
dir_var = tk.StringVar(value=mc_dir)

def on_mode_change():
    m = mode_var.get()
    if m in ("client", "uninstall"):
        dir_var.set(mc_dir)
    elif m == "prism":
        dir_var.set(prism_dir)
    elif m == "server":
        dir_var.set(srv_dir)

modes = [
    ("Official Launcher — Client Profile (Recommended)", "client"),
    ("Prism Launcher — Create Instance", "prism"),
    ("Dedicated Server — Initialize Server Folder", "server"),
    ("Uninstall from Official Launcher", "uninstall"),
]
frame_m = ttk.Frame(root)
frame_m.pack(fill="x", **pad)
for label, val in modes:
    ttk.Radiobutton(frame_m, text=label, variable=mode_var, value=val, command=on_mode_change).pack(anchor="w", pady=2)

frame_v = ttk.Frame(root)
frame_v.pack(fill="x", **pad)
ttk.Label(frame_v, text="Minecraft version:").pack(side="left")
ttk.Combobox(frame_v, textvariable=ver_var, values=("1.21.1", "26.3"), state="readonly", width=10).pack(side="left", padx=8)

frame_d = ttk.Frame(root)
frame_d.pack(fill="x", **pad)
ttk.Entry(frame_d, textvariable=dir_var, width=46).pack(side="left", fill="x", expand=True)
def browse():
    p = filedialog.askdirectory(initialdir=dir_var.get() or ".")
    if p:
        dir_var.set(p)
ttk.Button(frame_d, text="Browse...", command=browse).pack(side="left", padx=(6, 0))

eula_var = tk.BooleanVar(value=False)
ttk.Checkbutton(
    root,
    text="I accept the Minecraft EULA and allow Megu to download the official 1.21.1 server, pinned CC0 mappings and pinned MIT minecraft-data block metadata, then run the server locally once for runtime data",
    variable=eula_var,
    wraplength=460,
    justify="left",
).pack(anchor="w", padx=14, pady=(2, 0))

result = ["CANCEL"]
def do_ok():
    result[0] = f"{mode_var.get()}|{ver_var.get()}|{dir_var.get()}|{1 if eula_var.get() else 0}"
    root.destroy()
def do_cancel():
    root.destroy()

frame_b = ttk.Frame(root)
frame_b.pack(fill="x", padx=14, pady=12)
ttk.Button(frame_b, text="Cancel", command=do_cancel).pack(side="right", padx=(6, 0))
ttk.Button(frame_b, text="Install", command=do_ok).pack(side="right")
root.mainloop()
print(result[0])
"#;

const POWERSHELL_GUI_SCRIPT: &str = r#"
Add-Type -AssemblyName System.Windows.Forms
Add-Type -AssemblyName System.Drawing
$mcDir = $args[0]; $prismDir = $args[1]; $srvDir = $args[2]
$form = New-Object System.Windows.Forms.Form
$form.Text = "Megu Installer"
$form.Size = New-Object System.Drawing.Size(500, 365)
$form.StartPosition = "CenterScreen"
$form.FormBorderStyle = "FixedDialog"
$form.MaximizeBox = $false

$lbl = New-Object System.Windows.Forms.Label
$lbl.Text = "Megu — Native Rust + wgpu Installer"
$lbl.Font = New-Object System.Drawing.Font("Segoe UI", 11, [System.Drawing.FontStyle]::Bold)
$lbl.Location = New-Object System.Drawing.Point(16, 14)
$lbl.AutoSize = $true
$form.Controls.Add($lbl)

$rbClient = New-Object System.Windows.Forms.RadioButton
$rbClient.Text = "Official Launcher — Client Profile (Recommended)"
$rbClient.Location = New-Object System.Drawing.Point(20, 46)
$rbClient.AutoSize = $true; $rbClient.Checked = $true
$form.Controls.Add($rbClient)

$rbPrism = New-Object System.Windows.Forms.RadioButton
$rbPrism.Text = "Prism Launcher — Create Instance"
$rbPrism.Location = New-Object System.Drawing.Point(20, 72)
$rbPrism.AutoSize = $true
$form.Controls.Add($rbPrism)

$rbServer = New-Object System.Windows.Forms.RadioButton
$rbServer.Text = "Dedicated Server — Initialize Server Folder"
$rbServer.Location = New-Object System.Drawing.Point(20, 98)
$rbServer.AutoSize = $true
$form.Controls.Add($rbServer)

$rbUninstall = New-Object System.Windows.Forms.RadioButton
$rbUninstall.Text = "Uninstall from Official Launcher"
$rbUninstall.Location = New-Object System.Drawing.Point(20, 124)
$rbUninstall.AutoSize = $true
$form.Controls.Add($rbUninstall)

$lblVer = New-Object System.Windows.Forms.Label
$lblVer.Text = "Minecraft version:"
$lblVer.Location = New-Object System.Drawing.Point(20, 160)
$lblVer.AutoSize = $true
$form.Controls.Add($lblVer)

$cmbVer = New-Object System.Windows.Forms.ComboBox
$cmbVer.Location = New-Object System.Drawing.Point(145, 156)
$cmbVer.Width = 100
$cmbVer.DropDownStyle = "DropDownList"
[void]$cmbVer.Items.Add("1.21.1")
[void]$cmbVer.Items.Add("26.3")
$cmbVer.SelectedIndex = 0
$form.Controls.Add($cmbVer)

$txtDir = New-Object System.Windows.Forms.TextBox
$txtDir.Location = New-Object System.Drawing.Point(20, 196)
$txtDir.Width = 360
$txtDir.Text = $mcDir
$form.Controls.Add($txtDir)

$rbClient.Add_CheckedChanged({ if ($rbClient.Checked) { $txtDir.Text = $mcDir } })
$rbUninstall.Add_CheckedChanged({ if ($rbUninstall.Checked) { $txtDir.Text = $mcDir } })
$rbPrism.Add_CheckedChanged({ if ($rbPrism.Checked) { $txtDir.Text = $prismDir } })
$rbServer.Add_CheckedChanged({ if ($rbServer.Checked) { $txtDir.Text = $srvDir } })

$btnBrowse = New-Object System.Windows.Forms.Button
$btnBrowse.Text = "Browse..."
$btnBrowse.Location = New-Object System.Drawing.Point(388, 194)
$btnBrowse.Width = 80
$btnBrowse.Add_Click({
    $fbd = New-Object System.Windows.Forms.FolderBrowserDialog
    $fbd.SelectedPath = $txtDir.Text
    if ($fbd.ShowDialog() -eq "OK") { $txtDir.Text = $fbd.SelectedPath }
})
$form.Controls.Add($btnBrowse)

$chkEula = New-Object System.Windows.Forms.CheckBox
$chkEula.Text = "I accept the Minecraft EULA and allow Megu to download the official 1.21.1 server, pinned CC0 mappings and pinned MIT minecraft-data block metadata, then run the server locally once for runtime data"
$chkEula.Location = New-Object System.Drawing.Point(20, 230)
$chkEula.Size = New-Object System.Drawing.Size(450, 42)
$chkEula.Checked = $false
$form.Controls.Add($chkEula)

$btnOk = New-Object System.Windows.Forms.Button
$btnOk.Text = "Install"
$btnOk.Location = New-Object System.Drawing.Point(300, 282)
$btnOk.DialogResult = [System.Windows.Forms.DialogResult]::OK
$form.AcceptButton = $btnOk
$form.Controls.Add($btnOk)

$btnCancel = New-Object System.Windows.Forms.Button
$btnCancel.Text = "Cancel"
$btnCancel.Location = New-Object System.Drawing.Point(388, 282)
$btnCancel.DialogResult = [System.Windows.Forms.DialogResult]::Cancel
$form.CancelButton = $btnCancel
$form.Controls.Add($btnCancel)

if ($form.ShowDialog() -eq [System.Windows.Forms.DialogResult]::OK) {
    $m = if ($rbPrism.Checked) { "prism" } elseif ($rbServer.Checked) { "server" } elseif ($rbUninstall.Checked) { "uninstall" } else { "client" }
    $consent = if ($chkEula.Checked) { "1" } else { "0" }
    Write-Output "$m|$($cmbVer.SelectedItem)|$($txtDir.Text)|$consent"
} else {
    Write-Output "CANCEL"
}
"#;

fn launch_gui_dialog() -> Result<Option<Cli>, String> {
    let mc_dir = installer::detect_minecraft_dir()
        .unwrap_or_else(|_| PathBuf::from(".minecraft"))
        .display()
        .to_string();
    let prism_dir = installer::detect_prism_instances_dir()
        .unwrap_or_else(|_| PathBuf::from("instances"))
        .display()
        .to_string();
    let srv_dir = std::env::current_dir()
        .unwrap_or_else(|_| PathBuf::from("."))
        .join("megu-server")
        .display()
        .to_string();

    if cfg!(windows) {
        if let Ok(out) = std::process::Command::new("powershell")
            .args([
                "-NoProfile",
                "-STA",
                "-Command",
                POWERSHELL_GUI_SCRIPT,
                &mc_dir,
                &prism_dir,
                &srv_dir,
            ])
            .output()
        {
            if out.status.success() {
                return parse_gui_selection(&String::from_utf8_lossy(&out.stdout));
            }
        }
    }

    for py in ["python3", "python"] {
        if let Ok(out) = std::process::Command::new(py)
            .args(["-c", TKINTER_GUI_SCRIPT, &mc_dir, &prism_dir, &srv_dir])
            .output()
        {
            if out.status.success() {
                return parse_gui_selection(&String::from_utf8_lossy(&out.stdout));
            }
        }
    }

    Err("no GUI toolkit available (use CLI flags such as `megu-installer client`)".into())
}

fn should_autolaunch_gui() -> bool {
    if std::env::var_os("MEGU_HEADLESS").is_some() || std::env::var_os("CI").is_some() {
        return false;
    }
    if std::env::var_os("MEGU_GUI").is_some() {
        return true;
    }
    if cfg!(windows) || cfg!(target_os = "macos") {
        return true;
    }
    std::env::var_os("DISPLAY").is_some() || std::env::var_os("WAYLAND_DISPLAY").is_some()
}

fn show_gui_message(title: &str, message: &str, is_error: bool) {
    if cfg!(windows) {
        let icon = if is_error { "Error" } else { "Information" };
        let _ = std::process::Command::new("powershell")
            .args([
                "-NoProfile",
                "-STA",
                "-Command",
                "Add-Type -AssemblyName System.Windows.Forms; [System.Windows.Forms.MessageBox]::Show($args[1], $args[0], 'OK', $args[2]) | Out-Null",
                title,
                message,
                icon,
            ])
            .status();
        return;
    }
    let py_script = "import sys, tkinter as tk; from tkinter import messagebox; r=tk.Tk(); r.withdraw(); (messagebox.showerror if sys.argv[3]=='1' else messagebox.showinfo)(sys.argv[1], sys.argv[2]); r.destroy()";
    for py in ["python3", "python"] {
        if let Ok(st) = std::process::Command::new(py)
            .args([
                "-c",
                py_script,
                title,
                message,
                if is_error { "1" } else { "0" },
            ])
            .status()
        {
            if st.success() {
                return;
            }
        }
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if matches!(args.as_slice(), [flag] if flag == "--help" || flag == "-h") {
        print!("{HELP}");
        return ExitCode::SUCCESS;
    }
    if matches!(args.as_slice(), [flag] if flag == "--version" || flag == "-V") {
        println!("{VERSION}");
        return ExitCode::SUCCESS;
    }
    if matches!(args.as_slice(), [flag] if flag == "gui" || flag == "--gui") {
        match launch_gui_dialog() {
            Ok(Some(cli)) => match run(&cli) {
                Ok(()) => {
                    show_gui_message(
                        "Megu Installer",
                        &format!(
                            "Successfully executed '{}' for Minecraft {}.",
                            cli.command.name(),
                            cli.mc_version.id()
                        ),
                        false,
                    );
                    return ExitCode::SUCCESS;
                }
                Err(err) => {
                    let msg = format!("Installation failed: {err}");
                    show_gui_message("Megu Installer — Error", &msg, true);
                    eprintln!("megu-installer: {err}");
                    return ExitCode::FAILURE;
                }
            },
            Ok(None) => return ExitCode::SUCCESS,
            Err(err) => {
                eprintln!("megu-installer: {err}");
                return ExitCode::FAILURE;
            }
        }
    }
    if args.is_empty() && should_autolaunch_gui() {
        match launch_gui_dialog() {
            Ok(Some(cli)) => match run(&cli) {
                Ok(()) => {
                    show_gui_message(
                        "Megu Installer",
                        &format!(
                            "Successfully executed '{}' for Minecraft {}.",
                            cli.command.name(),
                            cli.mc_version.id()
                        ),
                        false,
                    );
                    return ExitCode::SUCCESS;
                }
                Err(err) => {
                    let msg = format!("Installation failed: {err}");
                    show_gui_message("Megu Installer — Error", &msg, true);
                    eprintln!("megu-installer: {err}");
                    return ExitCode::FAILURE;
                }
            },
            Ok(None) => return ExitCode::SUCCESS,
            Err(_) => {
                // Fall back seamlessly to CLI client install when no GUI toolkit is available.
            }
        }
    }
    let cli = match parse(&args) {
        Ok(cli) => cli,
        Err(err) => {
            eprintln!("megu-installer: {err}");
            return ExitCode::from(2);
        }
    };
    match run(&cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("megu-installer: {err}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_release_target_maps_supported_platforms() {
        assert_eq!(
            host_release_target_for("windows", "x86_64", ""),
            Some(("windows-x86_64", ".exe"))
        );
        assert_eq!(
            host_release_target_for("linux", "x86_64", "gnu"),
            Some(("linux-x86_64", ""))
        );
        assert_eq!(
            host_release_target_for("linux", "x86_64", "musl"),
            Some(("linux-x86_64-musl", ""))
        );
        assert_eq!(
            host_release_target_for("linux", "aarch64", "gnu"),
            Some(("linux-arm64", ""))
        );
        assert_eq!(
            host_release_target_for("linux", "aarch64", "musl"),
            Some(("linux-arm64-musl", ""))
        );
        assert_eq!(
            host_release_target_for("macos", "aarch64", ""),
            Some(("macos-arm64", ""))
        );
        assert_eq!(
            host_release_target_for("macos", "x86_64", ""),
            Some(("macos-x86_64", ""))
        );
        assert_eq!(
            host_release_target_for("windows", "aarch64", ""),
            Some(("windows-arm64", ".exe"))
        );
        assert_eq!(host_release_target_for("linux", "riscv64", "gnu"), None);
        assert_eq!(host_release_target_for("linux", "riscv64", "musl"), None);
    }

    #[test]
    fn parser_rejects_bad_input() {
        let args = |items: &[&str]| items.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert!(parse(&args(&["server"])).is_err()); // missing --dir
        assert!(parse(&args(&["server", "--dir", "x", "--client-bin", "b"])).is_err());
        assert!(parse(&args(&["--mc-version", "1.20.4"])).is_err());
        assert!(parse(&args(&["client", "--no-profile", "--no-profile"])).is_err());
        assert!(parse(&args(&["--client-sha256", "nothex"])).is_err());
        assert!(parse(&args(&["uninstall", "--instances-dir", "x"])).is_err());
        let cli = parse(&args(&["prism", "--mc-version", "26.3", "--json"])).unwrap();
        assert_eq!(cli.command, Command::Prism);
        assert_eq!(cli.mc_version, TargetVersion::V26_3);
        assert!(cli.json);
        // No subcommand at all means the Official Launcher lane.
        let cli = parse(&args(&["--json"])).unwrap();
        assert_eq!(cli.command, Command::Client);
        // Bare `megu-installer` installs for the Official Launcher too.
        let cli = parse(&args(&[])).unwrap();
        assert_eq!(cli.command, Command::Client);

        assert!(parse_gui_selection("CANCEL").unwrap().is_none());
        let g = parse_gui_selection("client|1.21.1|/tmp/mc")
            .unwrap()
            .unwrap();
        assert_eq!(g.command, Command::Client);
        assert_eq!(g.mc_version, TargetVersion::V1_21_1);
        assert_eq!(g.minecraft_dir, Some(PathBuf::from("/tmp/mc")));
        let g2 = parse_gui_selection("server|26.3|/tmp/srv")
            .unwrap()
            .unwrap();
        assert_eq!(g2.command, Command::Server);
        assert_eq!(g2.mc_version, TargetVersion::V26_3);
        assert_eq!(g2.dir, Some(PathBuf::from("/tmp/srv")));
    }

    #[test]
    fn auto_discovers_sibling_binaries_and_respects_explicit_override() {
        let dir =
            std::env::temp_dir().join(format!("megu-installer-discover-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let gpu = dir.join("megu-client-gpu");
        let jar = dir.join("megu-bootstrapper.jar");
        std::fs::write(&gpu, b"fake").unwrap();
        std::fs::write(&jar, b"fake").unwrap();

        let search = vec![dir.clone()];
        assert_eq!(
            auto_discover_sibling_in(&None, "megu-client-gpu", &search),
            Some(gpu.clone())
        );
        assert_eq!(
            auto_discover_sibling_in(&None, "megu-bootstrapper.jar", &search),
            Some(jar.clone())
        );
        let explicit = Some(PathBuf::from("/custom/path"));
        assert_eq!(
            auto_discover_sibling_in(&explicit, "megu-client-gpu", &search),
            explicit
        );
        let _ = std::fs::remove_file(&gpu);
        let _ = std::fs::remove_file(&jar);
        let _ = std::fs::remove_dir(&dir);
    }

    fn parse_cli(items: &[&str]) -> Cli {
        parse(&items.iter().map(|s| s.to_string()).collect::<Vec<_>>()).unwrap()
    }

    fn scratch_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "megu-installer-runtime-{name}-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn values_json(prefix: &str, count: usize) -> String {
        format!(
            "[{}]",
            (0..count)
                .map(|index| format!("\"{prefix}{index}\""))
                .collect::<Vec<_>>()
                .join(",")
        )
    }

    fn runtime_blocks_json(synthetic_names: bool) -> String {
        let mut blocks = String::from("[");
        let mut first = 0u32;
        for id in 0..1_060u32 {
            let count = if id == 0 { 25_625 } else { 1 };
            if id != 0 {
                blocks.push(',');
            }
            let name = if synthetic_names {
                format!("block{id}")
            } else {
                match id {
                    0 => "air".to_owned(),
                    1 => "stone".to_owned(),
                    1059 => "heavy_core".to_owned(),
                    _ => format!("block{id}"),
                }
            };
            let states = if id == 0 {
                format!(
                    "[{{\"name\":\"p41\",\"type\":\"enum\",\"num_values\":41,\"values\":{p41}}},{{\"name\":\"p5a\",\"type\":\"enum\",\"num_values\":5,\"values\":{p5}}},{{\"name\":\"p5b\",\"type\":\"enum\",\"num_values\":5,\"values\":{p5}}},{{\"name\":\"p5c\",\"type\":\"enum\",\"num_values\":5,\"values\":{p5}}},{{\"name\":\"p5d\",\"type\":\"enum\",\"num_values\":5,\"values\":{p5}}}]",
                    p41 = values_json("v", 41),
                    p5 = values_json("w", 5),
                )
            } else {
                "[]".to_owned()
            };
            blocks.push_str(&format!(
                "{{\"id\":{id},\"name\":\"{name}\",\"hardness\":1.0,\"resistance\":1.0,\"stackSize\":64,\"defaultState\":{first},\"minStateId\":{first},\"maxStateId\":{},\"states\":{states},\"drops\":[]}}",
                first + count - 1
            ));
            first += count;
        }
        blocks.push(']');
        blocks
    }

    fn write_block_states_asset(path: &Path) {
        let mut rows = String::new();
        let mut start = 0u32;
        for id in 0..1_060u32 {
            let count = match id {
                0 | 1 => 1,
                2 => 25_624,
                1_059 => 2,
                _ => 1,
            };
            if id != 0 {
                rows.push(',');
            }
            let name = match id {
                0 => "minecraft:air".to_owned(),
                1 => "minecraft:stone".to_owned(),
                1_059 => "minecraft:heavy_core".to_owned(),
                _ => format!("minecraft:block{id}"),
            };
            rows.push_str(&format!("[\"{name}\",{start},{count}]"));
            start += count;
        }
        assert_eq!(start, 26_684);
        let mut flags = vec![8u32; 26_684];
        flags[0] = 3;
        let mut luminance = vec![0u32; 26_684];
        luminance[1] = 1;
        let mut light_block = vec![0u32; 26_684];
        light_block[1] = 1;
        let full_sides = vec![63u32; 26_684];
        let mut collision = vec!["[0]".to_owned(); 26_684];
        collision[0] = "[]".to_owned();
        let join = |values: &[u32]| {
            values
                .iter()
                .map(u32::to_string)
                .collect::<Vec<_>>()
                .join(",")
        };
        let asset = format!(
            "{{\"format\":1,\"minecraft\":\"1.21.1\",\"jar_sha1\":\"{VANILLA_SERVER_SHA1}\",\"blocks\":[{rows}],\"luminance\":[{}],\"light_block\":[{}],\"flags\":[{}],\"full_sides\":[{}],\"shapes\":[{{\"min\":[0.0,0.0,0.0],\"max\":[1.0,1.0,1.0]}}],\"collision_shapes\":[{}]}}",
            join(&luminance),
            join(&light_block),
            join(&flags),
            join(&full_sides),
            collision.join(",")
        );
        std::fs::write(path, asset).unwrap();
    }

    fn feature_order_json() -> String {
        let mut out = String::from("{");
        for (dim_index, (dimension, biome_count, step_count, feature_count)) in [
            ("overworld", 53usize, 11usize, 151usize),
            ("nether", 5, 10, 37),
            ("end", 5, 11, 5),
        ]
        .iter()
        .enumerate()
        {
            if dim_index != 0 {
                out.push(',');
            }
            let mut biomes = Vec::new();
            for index in 0..*biome_count {
                let name = if dimension == &"overworld" && index == 0 {
                    "minecraft:mushroom_fields".to_owned()
                } else {
                    format!("minecraft:{dimension}_biome{index}")
                };
                biomes.push(format!("\"{name}\""));
            }
            let mut features = Vec::new();
            for index in 0..*feature_count {
                let name = if dimension == &"end" && index == 0 {
                    "minecraft:end_island_decorated".to_owned()
                } else {
                    format!("minecraft:{dimension}_feature{index}")
                };
                features.push(format!("\"{name}\""));
            }
            let mut steps = vec![Vec::new(); *step_count];
            for (index, feature) in features.iter().enumerate() {
                steps[index % *step_count].push(feature.clone());
            }
            let steps_json = steps
                .iter()
                .map(|step| format!("[{}]", step.join(",")))
                .collect::<Vec<_>>()
                .join(",");
            out.push_str(&format!(
                "\"{dimension}\":{{\"biomes\":[{}],\"steps\":[{steps_json}]}}",
                biomes.join(",")
            ));
        }
        out.push('}');
        out
    }

    fn write_valid_binary_assets(root: &Path) {
        let assets = root.join("assets");
        let mut predicates = vec![31u8; 26_684];
        predicates[0] = 0;
        predicates[2] = 30;
        std::fs::write(assets.join("block_predicates_1211.bin"), predicates).unwrap();

        let mut facts = vec![0u8; 26_684 * 6];
        for (index, byte) in facts.iter_mut().enumerate() {
            *byte = (index % 251) as u8;
        }
        facts[0..6].copy_from_slice(&[161, 0, 0, 0, 0, 0]);
        facts[6..12].copy_from_slice(&[220, 15, 255, 255, 3, 0]);
        std::fs::write(assets.join("block_state_facts_1211.bin"), facts).unwrap();

        let mut collision = vec![1u8; 26_684];
        collision[0] = 0;
        std::fs::write(assets.join("collision_shape_full_1211.bin"), collision).unwrap();
        let mut mineshaft = vec![127u8; 26_684];
        mineshaft[0] = 0;
        mineshaft[2] = 1;
        std::fs::write(assets.join("mineshaft_support_1211.bin"), mineshaft).unwrap();
        let mut multiface = vec![63u8; 26_684];
        multiface[0] = 0;
        multiface[2] = 1;
        std::fs::write(assets.join("multiface_support_1211.bin"), multiface).unwrap();
        let mut transforms = vec![0u8; 26_684 * 12 * 4];
        for (index, byte) in transforms.iter_mut().enumerate().skip(96) {
            *byte = (index % 251) as u8;
        }
        for offset in (48..96).step_by(4) {
            transforms[offset..offset + 4].copy_from_slice(&[0, 0, 0, 1]);
        }
        std::fs::write(
            assets.join("structure_state_transforms_1211.bin"),
            transforms,
        )
        .unwrap();
        let mut template = vec![1u8; 26_684];
        template[0] = 0;
        std::fs::write(assets.join("template_solid_1211.bin"), template).unwrap();
    }

    fn write_minimal_runtime_tree(root: &Path) {
        std::fs::create_dir_all(root.join("assets")).unwrap();
        std::fs::create_dir_all(root.join("datapacks/vanilla/data/minecraft/worldgen/biome"))
            .unwrap();
        std::fs::create_dir_all(root.join("datapacks/vanilla/data/minecraft/dimension_type"))
            .unwrap();
        std::fs::create_dir_all(root.join("datapacks/vanilla/data/minecraft/tags/block")).unwrap();
        std::fs::create_dir_all(root.join("datapacks/vanilla/data/minecraft/tags/worldgen"))
            .unwrap();
        std::fs::create_dir_all(root.join("datapacks/vanilla/data/minecraft/structure")).unwrap();
        std::fs::write(root.join("config_packets.bin"), vec![1u8; 1024]).unwrap();
        std::fs::write(
            root.join("datapacks/vanilla/multi_noise_biome_tree.json"),
            br#"{"overworld":{"_type":"leaf","biome":"minecraft:plains","parameters":[]},"nether":{"_type":"leaf","biome":"minecraft:nether_wastes","parameters":[]}}"#,
        )
        .unwrap();
        std::fs::write(
            root.join("datapacks/vanilla/structure-provenance.json"),
            br#"{"minecraft":"1.21.1"}"#,
        )
        .unwrap();
        write_block_states_asset(&root.join("assets/block_states_1211.json"));
        std::fs::write(
            root.join("assets/blocks_1211.json"),
            runtime_blocks_json(false),
        )
        .unwrap();
        write_valid_binary_assets(root);
        std::fs::write(
            root.join("assets/feature_order_1211.json"),
            feature_order_json(),
        )
        .unwrap();
        std::fs::write(
            root.join("datapacks/vanilla/pack.mcmeta"),
            br#"{"pack":{"pack_format":48,"description":"test"}}"#,
        )
        .unwrap();
    }

    #[test]
    fn runtime_data_detection_requires_the_integrated_singleplayer_layout() {
        let root = scratch_dir("detect");
        assert_eq!(missing_runtime_data(&root), RUNTIME_REQUIRED.to_vec());
        write_minimal_runtime_tree(&root);
        assert!(missing_runtime_data(&root).is_empty(), "missing {:?}", missing_runtime_data(&root));

        // A directory with the right file name is not a usable packet capture or asset.
        std::fs::remove_file(root.join("config_packets.bin")).unwrap();
        std::fs::create_dir(root.join("config_packets.bin")).unwrap();
        assert_eq!(missing_runtime_data(&root), vec!["config_packets.bin"]);
    }

    #[test]
    fn report_only_block_state_index_is_not_accepted_as_runtime_data() {
        let root = scratch_dir("schema");
        write_minimal_runtime_tree(&root);
        assert!(missing_runtime_data(&root).is_empty(), "missing {:?}", missing_runtime_data(&root));
        std::fs::write(
            root.join("assets/block_states_1211.json"),
            br#"{
              "format":1,
              "source_reports":{"blocks.json":"deadbeef"},
              "blocks":[["minecraft:stone",1,1]],
              "block_index":{"minecraft:stone":{"first_state_id":1,"state_count":1}}
            }"#,
        )
        .unwrap();
        assert_eq!(
            missing_runtime_data(&root),
            vec!["assets/block_states_1211.json"]
        );
    }

    #[test]
    fn runtime_data_detection_rejects_zeroed_tables_and_shapeless_feature_order() {
        let root = scratch_dir("semantic");
        write_minimal_runtime_tree(&root);
        assert!(missing_runtime_data(&root).is_empty(), "missing {:?}", missing_runtime_data(&root));

        let table = root.join("assets/block_predicates_1211.bin");
        std::fs::write(&table, vec![0u8; 26_684]).unwrap();
        assert_eq!(
            missing_runtime_data(&root),
            vec!["assets/block_predicates_1211.bin"]
        );

        write_minimal_runtime_tree(&root);
        std::fs::write(root.join("assets/feature_order_1211.json"), br#"{"overworld":{}}"#).unwrap();
        assert_eq!(
            missing_runtime_data(&root),
            vec!["assets/feature_order_1211.json"]
        );
    }

    #[test]
    fn runtime_data_detection_rejects_synthetic_or_inconsistent_block_registry() {
        let root = scratch_dir("block-semantics");
        write_minimal_runtime_tree(&root);
        assert!(
            missing_runtime_data(&root).is_empty(),
            "missing {:?}",
            missing_runtime_data(&root)
        );

        std::fs::write(
            root.join("assets/blocks_1211.json"),
            runtime_blocks_json(true),
        )
        .unwrap();
        assert_eq!(
            missing_runtime_data(&root),
            vec!["assets/blocks_1211.json"]
        );

        let inconsistent = runtime_blocks_json(false).replacen(
            "\"num_values\":5",
            "\"num_values\":4",
            1,
        );
        std::fs::write(root.join("assets/blocks_1211.json"), inconsistent).unwrap();
        assert_eq!(
            missing_runtime_data(&root),
            vec!["assets/blocks_1211.json"]
        );
    }

    #[test]
    fn runtime_data_defaults_fail_closed_and_explicit_opt_out_is_distinct() {
        let root = scratch_dir("fail-closed").join("fresh-mc");
        let cli = parse_cli(&["client", "--minecraft-dir", root.to_str().unwrap()]);
        let err = ensure_runtime_data(&cli, &root).expect_err("fresh install must fail closed");
        assert!(err.to_string().contains("--accept-eula"), "{err}");
        assert!(err.to_string().contains("--no-runtime-data"), "{err}");
        assert!(!root.join("versions").exists());

        let cli = parse_cli(&[
            "client",
            "--minecraft-dir",
            root.to_str().unwrap(),
            "--no-runtime-data",
        ]);
        assert!(!ensure_runtime_data(&cli, &root).unwrap());

        assert!(
            parse(&[
                "client".to_owned(),
                "--no-runtime-data".to_owned(),
                "--accept-eula".to_owned(),
            ])
            .is_err()
        );
    }

    #[test]
    fn runtime_data_partial_install_is_not_overwritten_or_silently_reused() {
        let root = scratch_dir("partial");
        std::fs::create_dir_all(root.join("datapacks/vanilla/data/minecraft")).unwrap();
        let cli = parse_cli(&[
            "client",
            "--minecraft-dir",
            root.to_str().unwrap(),
            "--accept-eula",
        ]);
        let err = ensure_runtime_data(&cli, &root).expect_err("partial data must fail closed");
        assert!(err.to_string().contains("incomplete"), "{err}");
        assert!(!root.join("megu-runtime").exists());
    }

    #[test]
    fn runtime_output_move_is_ordered_and_refuses_existing_targets() {
        let base = scratch_dir("move");
        let stage = base.join("stage");
        let root = base.join("root");
        write_minimal_runtime_tree(&stage);
        std::fs::create_dir_all(root.join("megu-runtime")).unwrap();
        move_runtime_output(&stage, &root).unwrap();
        assert!(root.join("config_packets.bin").is_file());
        assert!(root.join("assets/block_states_1211.json").is_file());
        assert!(root.join("datapacks/vanilla/data/minecraft").is_dir());
        assert!(!stage.join("datapacks/vanilla").exists());

        write_minimal_runtime_tree(&stage);
        let err = move_runtime_output(&stage, &root).expect_err("existing target must win");
        assert!(err.to_string().contains("refusing to overwrite"), "{err}");
    }

    #[cfg(unix)]
    #[test]
    fn runtime_output_move_rejects_symlinked_intermediate_parent() {
        let base = scratch_dir("move-symlink");
        let stage = base.join("stage");
        let root = base.join("root");
        let outside = base.join("outside");
        write_minimal_runtime_tree(&stage);
        std::fs::create_dir_all(root.join("megu-runtime")).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, root.join("assets")).unwrap();

        let err = move_runtime_output(&stage, &root).expect_err("symlinked parent must fail");
        assert!(err.to_string().contains("unsafe or incompatible"), "{err}");
        assert!(!outside.join("blocks_1211.json").exists());
    }

    #[test]
    fn vanilla_server_jar_pin_rejects_other_bytes() {
        let root = scratch_dir("jar-pin");
        let jar = root.join("server.jar");
        std::fs::write(&jar, b"not the official server").unwrap();
        let err = verified_vanilla_server_jar(&jar).expect_err("wrong jar must fail");
        assert!(
            err.to_string()
                .contains("not the pinned official 1.21.1 server jar"),
            "{err}"
        );
    }

    #[test]
    fn gui_selection_consent_suffix_is_explicit_and_backward_compatible() {
        let old = parse_gui_selection("client|1.21.1|/tmp/mc")
            .unwrap()
            .unwrap();
        assert!(!old.accept_eula);
        assert!(!old.download_vanilla_server);
        let yes = parse_gui_selection("client|1.21.1|/tmp/mc|1")
            .unwrap()
            .unwrap();
        assert!(yes.accept_eula);
        assert!(yes.download_vanilla_server);
        let no = parse_gui_selection("client|1.21.1|/tmp/mc|0")
            .unwrap()
            .unwrap();
        assert!(!no.accept_eula);
        assert!(!no.download_vanilla_server);
    }
}
