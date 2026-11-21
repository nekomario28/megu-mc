// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
//! Standalone, dependency-free bootstrapper for a server directory.

use mc_spine_core::version::TargetVersion;
use mc_spine_server_files::json::Json;
use mc_spine_server_files::{
    check_eula, write_atomic, write_default_eula, EulaStatus, OpsList, ServerDirectory, Whitelist,
    EULA_FILE, EULA_LINK, OPS_FILE, WHITELIST_FILE,
};
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

const HELP: &str = "mc-spine-init — create a Minecraft server directory\n\
\n\
USAGE:\n\
    mc-spine-init --dir <path> [--mc-version 1.21.1|26.3] [--port <u16>]\n\
                  [--accept-eula] [--force] [--dry-run] [--json]\n\
    mc-spine-init -h | --help\n\
\n\
OPTIONS:\n\
    --dir <path>        Server directory (default: .)\n\
    --mc-version <id>   Minecraft version (default: 1.21.1)\n\
    --port <u16>        Server port, 1..65535 (default: 25565)\n\
    --accept-eula       Explicitly agree to the Minecraft EULA\n\
    --force             Overwrite existing generated configuration files\n\
    --dry-run           Show planned actions without modifying files\n\
    --json              Emit deterministic JSON\n\
\n\
Existing administration subcommands (init, doctor, op, deop, ban, pardon,\n\
whitelist, claims) remain available.\n";

const DIRECTORIES: [&str; 4] = ["world", "mods", "datapacks", "backups"];
const CONFIG_FILES: [&str; 4] = ["server.toml", EULA_FILE, OPS_FILE, WHITELIST_FILE];

struct Options {
    dir: PathBuf,
    version: TargetVersion,
    port: u16,
    accept_eula: bool,
    force: bool,
    dry_run: bool,
    json: bool,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            dir: PathBuf::from("."),
            version: TargetVersion::V1_21_1,
            port: 25565,
            accept_eula: false,
            force: false,
            dry_run: false,
            json: false,
        }
    }
}

fn parse(args: &[String]) -> Result<Options, String> {
    let mut options = Options::default();
    let mut seen = std::collections::HashSet::new();
    let mut index = 0;
    while index < args.len() {
        let flag = args[index].as_str();
        if !seen.insert(flag) {
            return Err(format!("duplicate option {flag}"));
        }
        match flag {
            "--dir" | "--mc-version" | "--port" => {
                index += 1;
                let value = args
                    .get(index)
                    .ok_or_else(|| format!("{flag} requires a value"))?;
                match flag {
                    "--dir" => {
                        if value.is_empty() || value.contains('\0') || value.starts_with('-') {
                            return Err(
                                "--dir requires a nonempty path without NUL bytes or leading '-' (prefix such paths with ./)".into(),
                            );
                        }
                        options.dir = PathBuf::from(value);
                    }
                    "--mc-version" => {
                        options.version = TargetVersion::parse(value)
                            .ok_or_else(|| format!("unsupported Minecraft version {value:?}"))?;
                    }
                    "--port" => {
                        options.port = value
                            .parse::<u16>()
                            .map_err(|_| format!("invalid port {value:?} (expected 1..65535)"))?;
                        if options.port == 0 {
                            return Err("--port must be in 1..65535".into());
                        }
                    }
                    _ => unreachable!(),
                }
            }
            "--accept-eula" => options.accept_eula = true,
            "--force" => options.force = true,
            "--dry-run" => options.dry_run = true,
            "--json" => options.json = true,
            _ => return Err(format!("unrecognized argument {flag:?}")),
        }
        index += 1;
    }
    Ok(options)
}

#[derive(Clone, Copy)]
enum Action {
    Create,
    Keep,
    Overwrite,
}

impl Action {
    fn label(self) -> &'static str {
        match self {
            Self::Create => "create",
            Self::Keep => "keep",
            Self::Overwrite => "overwrite",
        }
    }
}

struct Planned {
    name: &'static str,
    is_dir: bool,
    action: Action,
}

fn inspect(path: &Path, is_dir: bool, force: bool) -> io::Result<Action> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink()
                || (is_dir && !metadata.is_dir())
                || (!is_dir && !metadata.is_file())
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("{} has an unsafe or incompatible file type", path.display()),
                ));
            }
            Ok(if force && !is_dir {
                Action::Overwrite
            } else {
                Action::Keep
            })
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(Action::Create),
        Err(err) => Err(err),
    }
}

fn plan(options: &Options) -> io::Result<Vec<Planned>> {
    // An ancestor symlink could redirect a missing target outside the
    // requested directory even when the target itself is not a symlink.
    for ancestor in options.dir.ancestors() {
        match fs::symlink_metadata(ancestor) {
            Ok(metadata) if !metadata.is_dir() || metadata.file_type().is_symlink() => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("{} is not a real directory", ancestor.display()),
                ));
            }
            Ok(_) => {}
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(err) => return Err(err),
        }
    }
    inspect(&options.dir, true, false)?;
    DIRECTORIES
        .iter()
        .map(|name| (*name, true))
        .chain(CONFIG_FILES.iter().map(|name| (*name, false)))
        .map(|(name, is_dir)| {
            Ok(Planned {
                name,
                is_dir,
                action: inspect(&options.dir.join(name), is_dir, options.force)?,
            })
        })
        .collect()
}

// create_new prevents overwriting a file that appeared after the plan was made.
fn write_new(path: &Path, contents: &[u8]) -> io::Result<()> {
    let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
    if let Err(err) = file.write_all(contents).and_then(|()| file.sync_all()) {
        drop(file);
        let _ = fs::remove_file(path);
        return Err(err);
    }
    Ok(())
}

fn store(path: &Path, contents: &[u8], action: Action) -> io::Result<()> {
    match action {
        Action::Create => write_new(path, contents),
        Action::Overwrite => write_atomic(path, contents),
        Action::Keep => Ok(()),
    }
}

fn eula_content(accepted: bool) -> String {
    format!(
        "#By changing the setting below to TRUE you are indicating your agreement to our EULA ({EULA_LINK}).\n\
         eula={}\n",
        if accepted { "true" } else { "false" }
    )
}

fn initialize(options: &Options, actions: &[Planned]) -> io::Result<()> {
    fs::create_dir_all(&options.dir)?;
    for entry in actions.iter().filter(|entry| entry.is_dir) {
        if matches!(entry.action, Action::Create) {
            fs::create_dir(options.dir.join(entry.name))?;
        }
    }

    // Create the EULA exclusively before loading the shared directory facade.
    // The facade's default writer uses atomic replacement, which could
    // otherwise overwrite an EULA created concurrently after our plan.
    let eula_action = actions
        .iter()
        .find(|entry| entry.name == EULA_FILE)
        .expect("EULA is always in the plan")
        .action;
    if matches!(eula_action, Action::Overwrite) && !options.accept_eula {
        write_default_eula(&options.dir)?;
    } else {
        store(
            &options.dir.join(EULA_FILE),
            eula_content(options.accept_eula).as_bytes(),
            eula_action,
        )?;
    }

    // Existing documents are parsed by the common directory facade.
    let directory = if options.force {
        None
    } else {
        Some(ServerDirectory::load_or_init(
            &options.dir,
            options.version,
        )?)
    };

    for entry in actions
        .iter()
        .filter(|entry| !entry.is_dir && entry.name != EULA_FILE)
    {
        let path = options.dir.join(entry.name);
        match entry.name {
            "server.toml" => store(
                &path,
                format!(
                    "# mc-spine server configuration\nmc_version = \"{}\"\nport = {}\n",
                    options.version.id(),
                    options.port
                )
                .as_bytes(),
                entry.action,
            )?,
            OPS_FILE => {
                let contents = directory
                    .as_ref()
                    .map(|dir| dir.ops.to_json())
                    .unwrap_or_else(|| OpsList::default().to_json());
                store(&path, contents.as_bytes(), entry.action)?;
            }
            WHITELIST_FILE => {
                let contents = directory
                    .as_ref()
                    .map(|dir| dir.whitelist.to_json())
                    .unwrap_or_else(|| Whitelist::default().to_json());
                store(&path, contents.as_bytes(), entry.action)?;
            }
            _ => unreachable!(),
        }
    }
    Ok(())
}

fn report(options: &Options, actions: &[Planned]) {
    let eula_kept = actions
        .iter()
        .any(|entry| entry.name == EULA_FILE && matches!(entry.action, Action::Keep));
    let accepted = if eula_kept {
        check_eula(&options.dir) == EulaStatus::Accepted
    } else {
        options.accept_eula
    };
    let notice = if accepted {
        "Minecraft EULA accepted (eula=true)."
    } else {
        "Minecraft EULA not accepted (eula=false). Review https://aka.ms/MinecraftEULA and use --accept-eula (with --force if eula.txt exists) to accept."
    };
    if options.json {
        let actions_json = actions
            .iter()
            .map(|entry| {
                Json::Object(vec![
                    ("path".into(), Json::String(entry.name.into())),
                    ("action".into(), Json::String(entry.action.label().into())),
                ])
            })
            .collect();
        let output = Json::Object(vec![
            (
                "dir".into(),
                Json::String(options.dir.display().to_string()),
            ),
            (
                "mc_version".into(),
                Json::String(options.version.id().into()),
            ),
            ("port".into(), Json::Number(f64::from(options.port))),
            ("dry_run".into(), Json::Bool(options.dry_run)),
            ("eula_accepted".into(), Json::Bool(accepted)),
            ("eula_notice".into(), Json::String(notice.into())),
            ("actions".into(), Json::Array(actions_json)),
        ]);
        println!("{}", output.to_json_string());
    } else {
        println!(
            "{} {} (Minecraft {}, port {})",
            if options.dry_run {
                "plan for"
            } else {
                "initialized"
            },
            options.dir.display(),
            options.version.id(),
            options.port
        );
        for entry in actions {
            println!(
                "  {} {}{}",
                entry.action.label(),
                entry.name,
                if entry.is_dir { "/" } else { "" }
            );
        }
        println!("{notice}");
    }
}

fn legacy_command(args: &[String]) -> Option<ExitCode> {
    if !matches!(
        args.first()?.as_str(),
        "init" | "doctor" | "op" | "deop" | "ban" | "pardon" | "whitelist" | "claims" | "help"
    ) {
        return None;
    }
    let mut binary = std::env::current_exe().ok()?;
    binary.set_file_name(format!("mc-spine-admin{}", std::env::consts::EXE_SUFFIX));
    Some(match Command::new(&binary).args(args).status() {
        Ok(status) => ExitCode::from(status.code().unwrap_or(1) as u8),
        Err(err) => {
            eprintln!("mc-spine-init: could not run {}: {err}", binary.display());
            ExitCode::FAILURE
        }
    })
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if matches!(args.as_slice(), [flag] if flag == "--help" || flag == "-h") {
        print!("{HELP}");
        return ExitCode::SUCCESS;
    }
    if let Some(code) = legacy_command(&args) {
        return code;
    }
    if args.is_empty() {
        eprintln!("mc-spine-init: supply an option to initialize a directory (see --help)");
        return ExitCode::from(2);
    }
    let options = match parse(&args) {
        Ok(options) => options,
        Err(err) => {
            eprintln!("mc-spine-init: {err}");
            return ExitCode::from(2);
        }
    };
    let result = plan(&options).and_then(|actions| {
        if !options.dry_run {
            initialize(&options, &actions)?;
        }
        report(&options, &actions);
        Ok(())
    });
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("mc-spine-init: {err}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_nul_in_directory_is_rejected_by_parser() {
        let args = ["--dir".to_owned(), "bad\0path".to_owned()];
        assert!(parse(&args).is_err());
    }
}
