// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
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
    megu-installer [client] [--minecraft-dir <dir>] [--mc-version 1.21.1|26.3]\n\
                   [--client-bin <path>] [--client-sha256 <hex>] [--bootstrap-jar <jar>]\n\
                   [--no-profile] [--json]\n\
    megu-installer prism [--instances-dir <dir>] [--mc-version 1.21.1|26.3]\n\
                   [--client-bin <path>] [--client-sha256 <hex>] [--bootstrap-jar <jar>] [--json]\n\
    megu-installer server --dir <dir> [--mc-version 1.21.1|26.3] [--server-bin <path>] [--json]\n\
    megu-installer uninstall [--minecraft-dir <dir>] [--mc-version 1.21.1|26.3] [--no-profile] [--json]\n\
    megu-installer -h | --help\n\
    megu-installer --version\n\
\n\
COMMANDS:\n\
    client      Install into the Official Launcher (the default when no\n\
                command is given): writes versions/megu-<mc>/megu-<mc>.json,\n\
                stages the bootstrapper library, and registers a\n\
                \"Megu (<mc>)\" profile in launcher_profiles.json (plus the\n\
                Windows Store profile file when present, with a .bak backup).\n\
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

fn run(cli: &Cli) -> std::io::Result<()> {
    match cli.command {
        Command::Client => {
            let mc_dir = match &cli.minecraft_dir {
                Some(dir) => dir.clone(),
                None => installer::detect_minecraft_dir()?,
            };
            let options = ClientInstallOptions {
                mc_version: cli.mc_version,
                bootstrap_jar: cli.bootstrap_jar.clone(),
                client_bin: cli.client_bin.clone(),
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
            let options = ClientInstallOptions {
                mc_version: cli.mc_version,
                bootstrap_jar: cli.bootstrap_jar.clone(),
                client_bin: cli.client_bin.clone(),
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
                server_bin: cli.server_bin.clone(),
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
    }
}
