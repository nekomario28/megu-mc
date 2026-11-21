// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
//! `mc-spine-world-migrate`: inspect `STV1` world snapshots and migrate world
//! data between Minecraft `1.21.1` and `26.3`.
//!
//! Exit codes: `0` success, `2` a refused downgrade or bad command line, `1`
//! anything wrong with the files themselves (unreadable, corrupt, CRC
//! mismatch, malformed JSON).

use mc_spine_core::version::TargetVersion;
use mc_spine_state_store::{
    inspect_anvil_world_dir, inspect_snapshot, migrate_anvil_level_dat, migrate_json_document,
    migrate_snapshot, to_data_json, write_file_atomic, AnvilInspectError, DataValue,
    MigrationError, SnapshotSummary,
};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

const HELP: &str = "\
mc-spine-world-migrate — inspect and migrate mc-spine world data
between Minecraft 1.21.1 (DataVersion 3955) and 26.3 (DataVersion 5023)

USAGE:
    mc-spine-world-migrate inspect <file.stv1> [--json]
    mc-spine-world-migrate inspect-anvil <world_dir> [--json]
    mc-spine-world-migrate migrate --in <in.stv1> --out <out.stv1> --to <1.21.1|26.3> [--allow-downgrade] [--json]
    mc-spine-world-migrate migrate-json --in <in.json> --out <out.json> --to <1.21.1|26.3> [--allow-downgrade] [--json]
    mc-spine-world-migrate migrate-level --in <in_level.dat> --out <out_level.dat> --to <1.21.1|26.3> [--allow-downgrade] [--backup-old] [--json]
    mc-spine-world-migrate --help

COMMANDS:
    inspect        Validate an STV1 snapshot file and print what it carries:
                   the Minecraft/protocol/DataVersion it was saved under and
                   its dimension, chunk, block-override and block-entity counts.
    inspect-anvil  Inspect a vanilla Anvil world directory: reads level.dat
                   (gzip or raw) for the level name, saved-by version, seed
                   and data packs, classifies the world against the two
                   supported releases, and scans the region/, DIM-1/region/
                   and DIM1/region/ directories for region files, populated
                   chunks and compression methods.
    migrate        Re-seal an STV1 snapshot for another Minecraft version.
                   The output is written atomically and is verified decodable
                   before it is reported.
    migrate-json   Run a JSON world-metadata document (level.dat export,
                   chunk or datapack metadata) through the data migration
                   pipeline. Output is deterministic sorted-key JSON.
    migrate-level  Migrate a vanilla level.dat directly: decode gzip or raw
                   NBT, run the DataFixer pipeline, and write a verified
                   gzip level.dat atomically. With --backup-old and an
                   existing --out, the prior file is copied to <out>_old
                   (vanilla's level.dat_old convention) before the swap.

OPTIONS:
    --in <path>          Input file.
    --out <path>         Output file. Written atomically; the input is never
                         modified, even when --in and --out are the same path.
    --to <version>       Target Minecraft version: \"1.21.1\" or \"26.3\".
    --allow-downgrade    Permit migrating to an older version. Required for
                         any 26.3 -> 1.21.1 migration; downgrades can lose
                         data the older format cannot represent.
    --backup-old         migrate-level only: when --out already exists, copy
                         it to <out>_old before writing the migrated file —
                         matching vanilla's level.dat_old convention.
    --json               Print the report as JSON instead of text.
    -h, --help           Show this help.

EXIT CODES:
    0  success
    1  the file could not be used: unreadable, corrupt, CRC mismatch,
       malformed JSON, or a DataVersion this tool does not know
    2  refused downgrade without --allow-downgrade, or bad command line
";

/// A fatal-to-this-invocation condition paired with its exit code.
struct CliError {
    code: u8,
    message: String,
}

/// Bad command line or refused operation: exit 2.
fn usage(message: impl Into<String>) -> CliError {
    CliError {
        code: 2,
        message: message.into(),
    }
}

/// File-level failure: exit 1.
fn fail(message: impl Into<String>) -> CliError {
    CliError {
        code: 1,
        message: message.into(),
    }
}

enum Command {
    Help,
    Inspect {
        path: PathBuf,
        json: bool,
    },
    InspectAnvil {
        dir: PathBuf,
        json: bool,
    },
    Migrate {
        input: PathBuf,
        output: PathBuf,
        to: TargetVersion,
        allow_downgrade: bool,
        json: bool,
    },
    MigrateJson {
        input: PathBuf,
        output: PathBuf,
        to: TargetVersion,
        allow_downgrade: bool,
        json: bool,
    },
    MigrateLevel {
        input: PathBuf,
        output: PathBuf,
        to: TargetVersion,
        allow_downgrade: bool,
        backup_old: bool,
        json: bool,
    },
}

/// The shared `--in`/`--out`/`--to`/`--allow-downgrade`/`--backup-old`/
/// `--json` flag set of the three migrate subcommands (`--backup-old` is
/// only accepted on `migrate-level`).
struct MigrateFlags {
    input: Option<PathBuf>,
    output: Option<PathBuf>,
    to: Option<TargetVersion>,
    allow_downgrade: bool,
    backup_old: bool,
    json: bool,
}

fn parse_migrate_flags(cmd: &str, args: &[String]) -> Result<MigrateFlags, CliError> {
    let mut flags = MigrateFlags {
        input: None,
        output: None,
        to: None,
        allow_downgrade: false,
        backup_old: false,
        json: false,
    };
    let mut i = 0;
    while i < args.len() {
        let arg = args[i].as_str();
        // Split `--flag=value` into (flag, inline value).
        let (flag, inline) = match arg.split_once('=') {
            Some((f, v)) if f.starts_with("--") => (f, Some(v.to_string())),
            _ => (arg, None),
        };
        let mut take_value = |name: &str| -> Result<String, CliError> {
            match &inline {
                Some(v) => Ok(v.clone()),
                None => {
                    i += 1;
                    args.get(i)
                        .cloned()
                        .ok_or_else(|| usage(format!("{name} needs a value")))
                }
            }
        };
        match flag {
            "--in" => flags.input = Some(PathBuf::from(take_value("--in")?)),
            "--out" => flags.output = Some(PathBuf::from(take_value("--out")?)),
            "--to" => {
                let v = take_value("--to")?;
                flags.to = Some(TargetVersion::parse(&v).ok_or_else(|| {
                    usage(format!(
                        "unknown target version {v:?}; expected \"1.21.1\" or \"26.3\""
                    ))
                })?);
            }
            "--allow-downgrade" => flags.allow_downgrade = true,
            "--backup-old" => flags.backup_old = true,
            "--json" => flags.json = true,
            other if other.starts_with('-') => {
                return Err(usage(format!("{cmd}: unknown option {other:?}")))
            }
            other => return Err(usage(format!("{cmd}: unexpected argument {other:?}"))),
        }
        i += 1;
    }
    Ok(flags)
}

fn require<T>(name: &str, value: Option<T>) -> Result<T, CliError> {
    value.ok_or_else(|| usage(format!("missing required option {name}")))
}

fn parse_args(args: &[String]) -> Result<Command, CliError> {
    if args.is_empty() {
        return Err(usage("no command given; try --help"));
    }
    // `-h`/`--help` anywhere asks for the help text.
    if args.iter().any(|a| a == "--help" || a == "-h") || args[0] == "help" {
        return Ok(Command::Help);
    }
    match args[0].as_str() {
        "inspect" => {
            let mut path = None;
            let mut json = false;
            for arg in &args[1..] {
                match arg.as_str() {
                    "--json" => json = true,
                    other if other.starts_with('-') => {
                        return Err(usage(format!("inspect: unknown option {other:?}")))
                    }
                    _ => {
                        if path.is_some() {
                            return Err(usage("inspect takes exactly one file"));
                        }
                        path = Some(PathBuf::from(arg));
                    }
                }
            }
            Ok(Command::Inspect {
                path: path.ok_or_else(|| usage("inspect needs a file to read"))?,
                json,
            })
        }
        "inspect-anvil" => {
            let mut dir = None;
            let mut json = false;
            for arg in &args[1..] {
                match arg.as_str() {
                    "--json" => json = true,
                    other if other.starts_with('-') => {
                        return Err(usage(format!("inspect-anvil: unknown option {other:?}")))
                    }
                    _ => {
                        if dir.is_some() {
                            return Err(usage("inspect-anvil takes exactly one world directory"));
                        }
                        dir = Some(PathBuf::from(arg));
                    }
                }
            }
            Ok(Command::InspectAnvil {
                dir: dir.ok_or_else(|| usage("inspect-anvil needs a world directory"))?,
                json,
            })
        }
        cmd @ ("migrate" | "migrate-json" | "migrate-level") => {
            let flags = parse_migrate_flags(cmd, &args[1..])?;
            let parsed = (
                require("--in", flags.input)?,
                require("--out", flags.output)?,
                require("--to", flags.to)?,
            );
            if cmd != "migrate-level" && flags.backup_old {
                return Err(usage(format!(
                    "{cmd}: --backup-old only applies to migrate-level"
                )));
            }
            match cmd {
                "migrate" => Ok(Command::Migrate {
                    input: parsed.0,
                    output: parsed.1,
                    to: parsed.2,
                    allow_downgrade: flags.allow_downgrade,
                    json: flags.json,
                }),
                "migrate-json" => Ok(Command::MigrateJson {
                    input: parsed.0,
                    output: parsed.1,
                    to: parsed.2,
                    allow_downgrade: flags.allow_downgrade,
                    json: flags.json,
                }),
                _ => Ok(Command::MigrateLevel {
                    input: parsed.0,
                    output: parsed.1,
                    to: parsed.2,
                    allow_downgrade: flags.allow_downgrade,
                    backup_old: flags.backup_old,
                    json: flags.json,
                }),
            }
        }
        other => Err(usage(format!("unknown command {other:?}; try --help"))),
    }
}

// ------------------------------------------------------------ JSON building

fn map(pairs: Vec<(&str, DataValue)>) -> DataValue {
    DataValue::Map(
        pairs
            .into_iter()
            .map(|(k, v)| (k.to_string(), v))
            .collect::<BTreeMap<_, _>>(),
    )
}

fn int(v: usize) -> DataValue {
    DataValue::Int(v as i64)
}

fn summary_value(file: &Path, s: &SnapshotSummary) -> DataValue {
    map(vec![
        ("block_entity_count", int(s.block_entity_count)),
        ("block_override_count", int(s.block_override_count)),
        ("chunk_count", int(s.chunk_count)),
        ("data_version", DataValue::Int(s.data_version as i64)),
        ("dimension_count", int(s.dimension_count)),
        (
            "dimensions",
            DataValue::List(
                s.dimension_names
                    .iter()
                    .map(|n| DataValue::Str(n.clone()))
                    .collect(),
            ),
        ),
        ("file", DataValue::Str(file.display().to_string())),
        ("mc_version", DataValue::Str(s.mc_version.clone())),
        ("protocol_version", DataValue::Int(s.protocol_version as i64)),
    ])
}

fn str_list(values: &[String]) -> DataValue {
    DataValue::List(
        values
            .iter()
            .map(|s| DataValue::Str(s.clone()))
            .collect(),
    )
}

// ------------------------------------------------------------------ output

fn print_summary(file: &Path, s: &SnapshotSummary) {
    println!("{}: minecraft {} snapshot", file.display(), s.mc_version);
    println!(
        "  protocol {}, data version {}",
        s.protocol_version, s.data_version
    );
    println!(
        "  {} dimension{}, {} chunk{}, {} block override{}, {} block entit{}",
        s.dimension_count,
        if s.dimension_count == 1 { "" } else { "s" },
        s.chunk_count,
        if s.chunk_count == 1 { "" } else { "s" },
        s.block_override_count,
        if s.block_override_count == 1 { "" } else { "s" },
        s.block_entity_count,
        if s.block_entity_count == 1 { "y" } else { "ies" },
    );
    for name in &s.dimension_names {
        println!("  - {name}");
    }
}

fn read_input(path: &Path) -> Result<Vec<u8>, CliError> {
    std::fs::read(path)
        .map_err(|e| fail(format!("cannot read {}: {e}", path.display())))
}

fn map_migration_error(path: &Path, e: MigrationError) -> CliError {
    match e {
        MigrationError::DowngradeNotAllowed { .. } => usage(format!(
            "{}: {e} (pass --allow-downgrade to permit it)",
            path.display()
        )),
        other => fail(format!("{}: {other}", path.display())),
    }
}

fn cmd_inspect(path: &Path, json: bool) -> Result<(), CliError> {
    let bytes = read_input(path)?;
    let summary = inspect_snapshot(&bytes).map_err(|e| fail(format!("{}: {e}", path.display())))?;
    if json {
        println!("{}", to_data_json(&summary_value(path, &summary)));
    } else {
        print_summary(path, &summary);
    }
    Ok(())
}

fn cmd_inspect_anvil(dir: &Path, json: bool) -> Result<(), CliError> {
    let report = inspect_anvil_world_dir(dir)
        .map_err(|e| fail(format!("{}: {e}", dir.display())))?;
    if json {
        println!("{}", report.to_json());
    } else {
        print!("{}", report.to_human());
    }
    Ok(())
}

fn cmd_migrate(
    input: &Path,
    output: &Path,
    to: TargetVersion,
    allow_downgrade: bool,
    json: bool,
) -> Result<(), CliError> {
    let bytes = read_input(input)?;
    let (out, report) = migrate_snapshot(&bytes, to, allow_downgrade)
        .map_err(|e| map_migration_error(input, e))?;
    write_file_atomic(output, &out)
        .map_err(|e| fail(format!("cannot write {}: {e}", output.display())))?;

    if json {
        let mut value = match summary_value(output, &report.summary) {
            DataValue::Map(m) => m,
            _ => unreachable!(),
        };
        value.insert(
            "already_at_target".to_string(),
            DataValue::Bool(report.already_at_target),
        );
        value.insert(
            "from_version".to_string(),
            DataValue::Str(report.from.id().to_string()),
        );
        value.insert(
            "from_data_version".to_string(),
            DataValue::Int(report.from.data_version() as i64),
        );
        value.insert(
            "input_file".to_string(),
            DataValue::Str(input.display().to_string()),
        );
        value.insert("input_bytes".to_string(), int(report.input_len));
        value.insert("output_bytes".to_string(), int(report.output_len));
        println!("{}", to_data_json(&DataValue::Map(value)));
        return Ok(());
    }

    if report.already_at_target {
        println!(
            "{}: already at {}; wrote canonical snapshot to {}",
            input.display(),
            report.to,
            output.display()
        );
    } else {
        println!(
            "{}: migrated {} -> {} (data version {} -> {})",
            input.display(),
            report.from,
            report.to,
            report.from.data_version(),
            report.to.data_version(),
        );
    }
    println!(
        "  {} -> {} bytes; {} dimension{}, {} chunk{}, {} block override{}, {} block entit{}",
        report.input_len,
        report.output_len,
        report.summary.dimension_count,
        if report.summary.dimension_count == 1 { "" } else { "s" },
        report.summary.chunk_count,
        if report.summary.chunk_count == 1 { "" } else { "s" },
        report.summary.block_override_count,
        if report.summary.block_override_count == 1 { "" } else { "s" },
        report.summary.block_entity_count,
        if report.summary.block_entity_count == 1 { "y" } else { "ies" },
    );
    Ok(())
}

fn cmd_migrate_json(
    input: &Path,
    output: &Path,
    to: TargetVersion,
    allow_downgrade: bool,
    json: bool,
) -> Result<(), CliError> {
    let text = std::fs::read_to_string(input).map_err(|e| {
        fail(format!("cannot read {}: {e}", input.display()))
    })?;
    let (out, report) = migrate_json_document(&text, to, allow_downgrade)
        .map_err(|e| map_migration_error(input, e))?;
    write_file_atomic(output, out.as_bytes())
        .map_err(|e| fail(format!("cannot write {}: {e}", output.display())))?;

    for warning in &report.warnings {
        eprintln!("warning: {warning}");
    }

    if json {
        println!(
            "{}",
            to_data_json(&map(vec![
                ("applied_steps", str_list(&report.applied_steps)),
                ("final_data_version", DataValue::Int(report.final_dv as i64)),
                ("input_file", DataValue::Str(input.display().to_string())),
                ("input_bytes", int(text.len())),
                (
                    "original_data_version",
                    DataValue::Int(report.original_dv as i64),
                ),
                ("output_bytes", int(out.len())),
                ("output_file", DataValue::Str(output.display().to_string())),
                ("renamed_entries", int(report.renamed_entries)),
                ("target_version", DataValue::Str(to.id().to_string())),
                ("warnings", str_list(&report.warnings)),
            ]))
        );
        return Ok(());
    }

    println!(
        "{}: migrated data version {} -> {} to {}",
        input.display(),
        report.original_dv,
        report.final_dv,
        output.display()
    );
    if report.applied_steps.is_empty() {
        println!("  no migration steps needed");
    } else {
        println!("  applied steps: {}", report.applied_steps.join(", "));
    }
    println!("  renamed entries: {}", report.renamed_entries);
    Ok(())
}

/// `<out>_old` — the vanilla `level.dat_old` convention applied to whatever
/// name `--out` carries.
fn old_backup_path(output: &Path) -> PathBuf {
    let mut name = output.as_os_str().to_os_string();
    name.push("_old");
    PathBuf::from(name)
}

fn cmd_migrate_level(
    input: &Path,
    output: &Path,
    to: TargetVersion,
    allow_downgrade: bool,
    backup_old: bool,
    json: bool,
) -> Result<(), CliError> {
    let bytes = read_input(input)?;
    // The whole migration runs in memory and verifies its own output, so a
    // refused downgrade or a corrupt input leaves every file on disk — the
    // output included — exactly as it was.
    let (out, report) = migrate_anvil_level_dat(&bytes, to, allow_downgrade).map_err(|e| match e {
        AnvilInspectError::DowngradeNotAllowed { .. } => usage(format!(
            "{}: {e} (pass --allow-downgrade to permit it)",
            input.display()
        )),
        other => fail(format!("{}: {other}", input.display())),
    })?;

    // Only now is anything on disk touched: with --backup-old a pre-existing
    // output is copied to <out>_old first (vanilla's level.dat_old), then
    // the migrated bytes replace --out in one atomic rename.
    let mut backup_path = None;
    if backup_old && output.exists() {
        let backup = old_backup_path(output);
        let previous = std::fs::read(output).map_err(|e| {
            fail(format!(
                "cannot read existing {} for backup: {e}",
                output.display()
            ))
        })?;
        write_file_atomic(&backup, &previous).map_err(|e| {
            fail(format!("cannot write backup {}: {e}", backup.display()))
        })?;
        backup_path = Some(backup);
    }
    write_file_atomic(output, &out)
        .map_err(|e| fail(format!("cannot write {}: {e}", output.display())))?;

    for warning in &report.warnings {
        eprintln!("warning: {warning}");
    }

    let already_at_target = report.original_dv == to.data_version();

    if json {
        println!(
            "{}",
            to_data_json(&map(vec![
                ("already_at_target", DataValue::Bool(already_at_target)),
                ("applied_steps", str_list(&report.applied_steps)),
                (
                    "backup_file",
                    backup_path.as_ref().map_or(DataValue::Null, |p| {
                        DataValue::Str(p.display().to_string())
                    }),
                ),
                (
                    "final_data_version",
                    DataValue::Int(report.final_dv as i64),
                ),
                ("input_bytes", int(bytes.len())),
                ("input_file", DataValue::Str(input.display().to_string())),
                (
                    "original_data_version",
                    DataValue::Int(report.original_dv as i64),
                ),
                ("output_bytes", int(out.len())),
                ("output_file", DataValue::Str(output.display().to_string())),
                ("renamed_entries", int(report.renamed_entries)),
                ("target_version", DataValue::Str(to.id().to_string())),
                ("warnings", str_list(&report.warnings)),
            ]))
        );
        return Ok(());
    }

    if already_at_target {
        println!(
            "{}: already at {}; wrote canonical level.dat to {}",
            input.display(),
            to,
            output.display()
        );
    } else {
        println!(
            "{}: migrated data version {} -> {} to {}",
            input.display(),
            report.original_dv,
            report.final_dv,
            output.display()
        );
        if report.applied_steps.is_empty() {
            println!("  no migration steps needed");
        } else {
            println!("  applied steps: {}", report.applied_steps.join(", "));
        }
    }
    println!("  {} -> {} bytes", bytes.len(), out.len());
    if let Some(backup) = &backup_path {
        println!("  previous output backed up to {}", backup.display());
    }
    Ok(())
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let command = match parse_args(&args) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("mc-spine-world-migrate: {}", e.message);
            return ExitCode::from(e.code);
        }
    };
    let result = match command {
        Command::Help => {
            print!("{HELP}");
            Ok(())
        }
        Command::Inspect { path, json } => cmd_inspect(&path, json),
        Command::InspectAnvil { dir, json } => cmd_inspect_anvil(&dir, json),
        Command::Migrate {
            input,
            output,
            to,
            allow_downgrade,
            json,
        } => cmd_migrate(&input, &output, to, allow_downgrade, json),
        Command::MigrateJson {
            input,
            output,
            to,
            allow_downgrade,
            json,
        } => cmd_migrate_json(&input, &output, to, allow_downgrade, json),
        Command::MigrateLevel {
            input,
            output,
            to,
            allow_downgrade,
            backup_old,
            json,
        } => cmd_migrate_level(&input, &output, to, allow_downgrade, backup_old, json),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("mc-spine-world-migrate: {}", e.message);
            ExitCode::from(e.code)
        }
    }
}
