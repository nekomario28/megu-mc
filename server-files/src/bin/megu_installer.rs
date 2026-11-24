// SPDX-License-Identifier: LGPL-3.0-or-later
//! `megu-installer`: the zero-Java native installer — Official Launcher,
//! Prism Launcher and dedicated-server lanes.
//!
//! Pure `std` plus `megu-core`: the binary shares the crate's
//! zero-dependency contract, so it runs on a Windows box with no Java and
//! broken `.jar` associations exactly like the Fabric `.exe` installer.
//!
//! Exit codes: `0` success, `2` bad command line, `1` a file-level
//! failure (unsafe or unreadable target).

#![deny(unsafe_code)]

use megu_core::version::TargetVersion;
use megu_server_files::installer::{
    self, ClientInstallOptions, InstallReport, ServerInstallOptions,
};
use megu_server_files::json::Json;
use std::path::PathBuf;
use std::process::ExitCode;

const HELP: &str = "megu-installer — install Megu like the Fabric/NeoForge installers, no Java needed\n\
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
            "--mc-version" | "--json" => true,
            "--no-profile" => matches!(command, Command::Client | Command::Uninstall),
            _ => false,
        };
        if !allowed {
            return Err(format!("unrecognized argument {flag:?} for {}", command.name()));
        }
        match flag {
            "--minecraft-dir" | "--instances-dir" | "--dir" | "--client-bin"
            | "--client-sha256" | "--bootstrap-jar" | "--server-bin" | "--mc-version" => {
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
                    "--client-sha256" => {
                        cli.client_sha256 = Some(installer::normalize_sha256_hex(value).map_err(
                            |err| err.to_string(),
                        )?);
                    }
                    "--mc-version" => {
                        cli.mc_version = TargetVersion::parse(value)
                            .ok_or_else(|| format!("unsupported --mc-version: {value}"))?;
                    }
                    _ => unreachable!(),
                }
            }
            "--no-profile" => cli.no_profile = true,
            "--json" => cli.json = true,
            _ => unreachable!(),
        }
        index += 1;
    }
    if command == Command::Server && cli.dir.is_none() {
        return Err("server requires --dir <path>".into());
    }
    Ok(cli)
}

fn report_json(command: Command, dir: &str, mc_version: TargetVersion, report: &InstallReport) -> Json {
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
        (
            "dir".into(),
            Json::String(dir.to_owned()),
        ),
        (
            "mc_version".into(),
            Json::String(mc_version.id().into()),
        ),
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

const DEFAULT_RELEASE_TAG: &str = "v0.1.0-alpha.2";
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
            .map(|root| std::path::Path::new(&root).join("System32").join("curl.exe"))
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
fn auto_fetch_release_bundle(base_dir: & std::path::Path) -> Option<PathBuf> {
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

fn run(cli: &Cli) -> std::io::Result<()> {
    match cli.command {
        Command::Client => {
            let mc_dir = match &cli.minecraft_dir {
                Some(dir) => dir.clone(),
                None => installer::detect_minecraft_dir()?,
            };
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
                        client_bin = auto_discover_sibling_in(
                            &cli.client_bin,
                            "megu-client-gpu",
                            &dirs,
                        );
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
                        client_bin = auto_discover_sibling_in(
                            &cli.client_bin,
                            "megu-client-gpu",
                            &dirs,
                        );
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
    let mut parts = trimmed.splitn(3, '|');
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

result = ["CANCEL"]
def do_ok():
    result[0] = f"{mode_var.get()}|{ver_var.get()}|{dir_var.get()}"
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
$form.Size = New-Object System.Drawing.Size(500, 330)
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

$btnOk = New-Object System.Windows.Forms.Button
$btnOk.Text = "Install"
$btnOk.Location = New-Object System.Drawing.Point(300, 245)
$btnOk.DialogResult = [System.Windows.Forms.DialogResult]::OK
$form.AcceptButton = $btnOk
$form.Controls.Add($btnOk)

$btnCancel = New-Object System.Windows.Forms.Button
$btnCancel.Text = "Cancel"
$btnCancel.Location = New-Object System.Drawing.Point(388, 245)
$btnCancel.DialogResult = [System.Windows.Forms.DialogResult]::Cancel
$form.CancelButton = $btnCancel
$form.Controls.Add($btnCancel)

if ($form.ShowDialog() -eq [System.Windows.Forms.DialogResult]::OK) {
    $m = if ($rbPrism.Checked) { "prism" } elseif ($rbServer.Checked) { "server" } elseif ($rbUninstall.Checked) { "uninstall" } else { "client" }
    Write-Output "$m|$($cmbVer.SelectedItem)|$($txtDir.Text)"
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
            .args(["-c", py_script, title, message, if is_error { "1" } else { "0" }])
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
        let g = parse_gui_selection("client|1.21.1|/tmp/mc").unwrap().unwrap();
        assert_eq!(g.command, Command::Client);
        assert_eq!(g.mc_version, TargetVersion::V1_21_1);
        assert_eq!(g.minecraft_dir, Some(PathBuf::from("/tmp/mc")));
        let g2 = parse_gui_selection("server|26.3|/tmp/srv").unwrap().unwrap();
        assert_eq!(g2.command, Command::Server);
        assert_eq!(g2.mc_version, TargetVersion::V26_3);
        assert_eq!(g2.dir, Some(PathBuf::from("/tmp/srv")));
    }

    #[test]
    fn auto_discovers_sibling_binaries_and_respects_explicit_override() {
        let dir = std::env::temp_dir().join(format!(
            "megu-installer-discover-{}",
            std::process::id()
        ));
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
}
