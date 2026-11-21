// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later

//! `mc-spine-mod-check` — zero-dependency modpack pre-flight checker.
//!
//! Points [`modpack::ModpackManifest::from_path`] and [`analyze_modpack`]
//! at a Modrinth `.mrpack` archive, a standalone `modrinth.index.json`,
//! or a CurseForge `manifest.json` / pack `.zip` and reports — in plain
//! language or deterministic JSON — whether the pack's Minecraft version
//! matches this server, which redundant performance mods are skipped
//! automatically, which files run natively, and which Java JARs still
//! need a bytecode inspection. Exit codes mirror
//! `tools/mod_launcher.py --modpack`: `0` the pack can run on this
//! server, `2` a version mismatch or invalid arguments, `1` an
//! unreadable or unsafe pack.

use std::collections::BTreeSet;
use std::env;
use std::ffi::OsString;
use std::path::PathBuf;
use std::process::ExitCode;

use mod_adapter_core::modpack::{analyze_modpack, ModpackManifest, ModpackReadiness};
use mod_adapter_core::version_compat::McVersion;

const USAGE: &str = "\
mc-spine-mod-check — check whether a modpack can run on this mc-spine server

Usage: mc-spine-mod-check --modpack <path> [options]

Accepts a Modrinth .mrpack archive, a standalone modrinth.index.json, or a
CurseForge manifest.json / pack .zip, and reports — before anything is
downloaded or launched — which files run natively, which redundant
performance mods are skipped automatically, and which Java mods still need
a compatibility inspection.

Options:
  --modpack <path>        modpack file to check (required)
  --mc-version <version>  Minecraft version this server runs: 1.21.1 or 26.3
                          (default: 1.21.1)
  --flagship <ids>        comma-separated mod ids handled by a flagship
                          native adapter (default: create)
  --profiles <ids>        comma-separated mod ids covered by declarative
                          offload profiles (default: none)
  --json                  print the machine-readable report instead of the
                          human summary
  -h, --help              show this help and exit

Exit status: 0 the pack can run on this server (native, with automatic
skips, or pending a bytecode inspection), 2 the pack targets a different
Minecraft version or the arguments were invalid, 1 the pack could not be
read or is unsafe.";

struct Cli {
    modpack: PathBuf,
    target: McVersion,
    flagship: BTreeSet<String>,
    profiles: BTreeSet<String>,
    json: bool,
}

enum Action {
    Help,
    Run(Cli),
}

fn csv_ids(value: &OsString) -> Result<BTreeSet<String>, String> {
    let text = value
        .to_str()
        .ok_or_else(|| format!("invalid non-UTF-8 id list '{value:?}'"))?;
    Ok(text
        .split(',')
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .map(str::to_string)
        .collect())
}

/// Fail-closed argument parser mirroring `tools/mod_launcher.py`'s
/// `--modpack` surface: every unknown flag, missing value, duplicate
/// positional, or bad `--mc-version` is a usage error (exit 2), never a
/// silent default.
fn parse_args(argv: &[OsString]) -> Result<Action, String> {
    let mut modpack: Option<PathBuf> = None;
    let mut mc_version: Option<String> = None;
    let mut flagship: Option<BTreeSet<String>> = None;
    let mut profiles: Option<BTreeSet<String>> = None;
    let mut json = false;
    let mut help = false;

    fn take_value<'a>(
        argv: &'a [OsString],
        i: &mut usize,
        flag: &str,
        inline: Option<&'a OsString>,
    ) -> Result<&'a OsString, String> {
        if let Some(v) = inline {
            return Ok(v);
        }
        *i += 1;
        match argv.get(*i) {
            Some(v) if !v.to_string_lossy().starts_with('-') => Ok(v),
            _ => Err(format!("{flag} expects a value")),
        }
    }

    let mut i = 0;
    while i < argv.len() {
        let arg = &argv[i];
        let text = arg
            .to_str()
            .ok_or_else(|| format!("unexpected non-UTF-8 argument '{arg:?}'"))?;
        let (flag, inline) = match text.split_once('=') {
            Some((f, v)) if f.starts_with("--") => (f, Some(v)),
            _ => (text, None),
        };
        let inline_os = inline.map(OsString::from);
        match flag {
            "-h" | "--help" => help = true,
            "--json" => {
                if inline.is_some() {
                    return Err("--json does not take a value".to_string());
                }
                json = true;
            }
            "--modpack" => {
                let v = take_value(argv, &mut i, "--modpack", inline_os.as_ref())?;
                modpack = Some(PathBuf::from(v));
            }
            "--mc-version" => {
                let v = take_value(argv, &mut i, "--mc-version", inline_os.as_ref())?;
                mc_version = Some(
                    v.to_str()
                        .ok_or_else(|| format!("invalid non-UTF-8 --mc-version '{v:?}'"))?
                        .to_string(),
                );
            }
            "--flagship" => {
                let v = take_value(argv, &mut i, "--flagship", inline_os.as_ref())?;
                flagship = Some(csv_ids(v)?);
            }
            "--profiles" => {
                let v = take_value(argv, &mut i, "--profiles", inline_os.as_ref())?;
                profiles = Some(csv_ids(v)?);
            }
            other if other.starts_with('-') => {
                return Err(format!("unknown option '{other}'"));
            }
            other => return Err(format!("unexpected argument '{other}'")),
        }
        i += 1;
    }

    if help {
        return Ok(Action::Help);
    }
    let modpack = modpack.ok_or_else(|| "--modpack is required".to_string())?;
    let declared = mc_version.unwrap_or_else(|| "1.21.1".to_string());
    let target = McVersion::parse(declared.trim())
        .filter(|v| *v == McVersion::V1_21_1 || *v == McVersion::V26_3)
        .ok_or_else(|| {
            format!("invalid --mc-version '{declared}' (expected 1.21.1 or 26.3)")
        })?;
    Ok(Action::Run(Cli {
        modpack,
        target,
        flagship: flagship.unwrap_or_else(|| csv_ids(&OsString::from("create")).unwrap()),
        profiles: profiles.unwrap_or_default(),
        json,
    }))
}

fn main() -> ExitCode {
    let argv: Vec<OsString> = env::args_os().skip(1).collect();
    let cli = match parse_args(&argv) {
        Ok(Action::Help) => {
            println!("{USAGE}");
            return ExitCode::SUCCESS;
        }
        Ok(Action::Run(cli)) => cli,
        Err(msg) => {
            eprintln!("mc-spine-mod-check: {msg}");
            eprintln!("Try 'mc-spine-mod-check --help' for usage.");
            return ExitCode::from(2);
        }
    };

    let manifest = match ModpackManifest::from_path(&cli.modpack) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("mc-spine-mod-check: {e}");
            return ExitCode::FAILURE;
        }
    };
    let report = analyze_modpack(&manifest, cli.target, &cli.flagship, &cli.profiles);
    if cli.json {
        print!("{}", report.to_json());
    } else {
        println!("{}", report.format_human_summary());
    }
    if report.readiness == ModpackReadiness::VersionMismatch {
        ExitCode::from(2)
    } else {
        ExitCode::SUCCESS
    }
}
