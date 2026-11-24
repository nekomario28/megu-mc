// SPDX-License-Identifier: LGPL-3.0-or-later
//! `megu-world-migrate`: inspect `STV1` world snapshots and migrate world
//! data between Minecraft `1.21.1` and `26.3`, plus the pure-offline operator
//! subcommands `audit-query` / `rollback` over `AUD1` audit logs and
//! `pregen-plan` / `pregen-run` for spiral pregeneration sizing and execution.
//!
//! Exit codes: `0` success, `2` a refused downgrade, a refused over-budget
//! rollback or bad command line, `1` anything wrong with the files themselves
//! (unreadable, corrupt, CRC mismatch, malformed JSON).

use megu_core::version::TargetVersion;
use megu_state_store::{
    decode_snapshot, encode_snapshot, inspect_anvil_world_dir, inspect_snapshot,
    migrate_anvil_level_dat, migrate_json_document, migrate_snapshot, to_data_json,
    write_file_atomic, AnvilInspectError, AuditAction, AuditLog, AuditQuery, AuditRecord,
    DataValue, MigrationError, PregenJob, PregenRunConfig, PregenShape, ServerContext,
    SnapshotSummary,
};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

const HELP: &str = "\
megu-world-migrate — inspect and migrate megu world data
between Minecraft 1.21.1 (DataVersion 3955) and 26.3 (DataVersion 5023)

USAGE:
    megu-world-migrate inspect <file.stv1> [--json]
    megu-world-migrate inspect-anvil <world_dir> [--json]
    megu-world-migrate migrate --in <in.stv1> --out <out.stv1> --to <1.21.1|26.3> [--allow-downgrade] [--json]
    megu-world-migrate migrate-json --in <in.json> --out <out.json> --to <1.21.1|26.3> [--allow-downgrade] [--json]
    megu-world-migrate migrate-level --in <in_level.dat> --out <out_level.dat> --to <1.21.1|26.3> [--allow-downgrade] [--backup-old] [--json]
    megu-world-migrate audit-query --log <audit.aud1> [--actor <name>] [--dim <dim>] [--pos <x,y,z> --radius <r>] [--since-tick <t>] [--limit <n>] [--json]
    megu-world-migrate rollback --log <audit.aud1> --in <world.stv1> --out <restored.stv1> [--actor <name>] [--dim <dim>] [--pos <x,y,z> --radius <r>] [--since-tick <t>] [--max-blocks <n>] [--json]
    megu-world-migrate pregen-plan --radius <chunks> [--shape square|circle] [--center <cx,cz>] [--batch-size <n>] [--json]
    megu-world-migrate pregen-run --radius <chunks> --budget-per-tick <n> [--shape square|circle] [--center <cx,cz>] [--max-resident <n>] [--json]
    megu-world-migrate --help

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
    audit-query    Query an AUD1 audit log fully offline: every intact
                   CRC32-framed record is recovered (a torn or corrupt tail
                   is reported and skipped), then filtered by actor,
                   dimension, --pos/--radius sphere and --since-tick, and
                   printed in chronological seq order.
    rollback       Replay the matching audit-log block mutations against an
                   STV1 snapshot: every filtered block mutation is undone in
                   reverse seq order (each position converges to its state
                   before the earliest matched edit), the result is
                   re-encoded under the input's exact target version,
                   verified decodable, and written atomically. Refuses with
                   exit 2 when the matched block count exceeds --max-blocks;
                   a refusal never touches --out.
    pregen-plan    Size a spiral pregen without generating anything: exact
                   chunk total, ring count, first/last chunk coordinates and
                   the tick count a --batch-size budget implies, for a
                   square or circular radius around --center.
    pregen-run     Actually execute the spiral pregen: each tick generates at
                   most --budget-per-tick chunks in a fresh overworld context
                   and the ticket-LRU unloader then evicts unticketed
                   residents down to --max-resident, so the resident set —
                   and RAM — stays bounded no matter how large the radius.
                   Reports the deterministic counters: chunks generated,
                   ticks run, peak/evicted/resident chunk counts.

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
    --log <path>         audit-query/rollback: the AUD1 audit log to read.
    --actor <name>       Only records by this actor (player name or a
                         pseudo-actor like #tnt / #creeper / #fire).
    --dim <dimension>    Only records in this dimension id.
    --pos <x,y,z>        Center of the spatial filter; must be given
                         together with --radius.
    --radius <r>         audit-query/rollback: Euclidean radius in blocks
                         around --pos. pregen-plan/pregen-run: spiral radius
                         in chunks (required; capped at 2000000, past the
                         vanilla world border's ~1875000 chunks).
    --since-tick <t>     Only records at tick >= t (inclusive).
    --limit <n>          audit-query: print at most n matching rows.
    --max-blocks <n>     rollback: refuse when more than n block mutations
                         would be rewritten (default 10000, hard cap
                         100000).
    --shape <s>          pregen footprint: \"square\" (default) covers
                         (2r+1)^2 chunks; \"circle\" covers dx^2+dz^2 <= r^2.
    --center <cx,cz>     pregen spiral center chunk (default 0,0).
    --batch-size <n>     pregen-plan chunks per tick used for the tick-count
                         estimate (default 64).
    --budget-per-tick <n>  pregen-run chunks generated per tick (required,
                         at least 1).
    --max-resident <n>   pregen-run unticketed resident-chunk cap enforced
                         by the LRU unloader after every tick (default
                         1024; 0 keeps nothing resident).
    --json               Print the report as JSON instead of text.
    -h, --help           Show this help.

EXIT CODES:
    0  success
    1  the file could not be used: unreadable, corrupt, CRC mismatch,
       malformed JSON, or a DataVersion this tool does not know
    2  refused downgrade without --allow-downgrade, a rollback whose
       matched block count exceeds --max-blocks, or bad command line
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
    AuditQuery {
        log: PathBuf,
        query: AuditQuery,
        limit: Option<usize>,
        json: bool,
    },
    Rollback {
        log: PathBuf,
        input: PathBuf,
        output: PathBuf,
        query: AuditQuery,
        max_blocks: u64,
        json: bool,
    },
    PregenPlan {
        radius: u32,
        shape: PregenShape,
        center: (i32, i32),
        batch_size: usize,
        json: bool,
    },
    PregenRun {
        radius: u32,
        shape: PregenShape,
        center: (i32, i32),
        budget_per_tick: usize,
        max_resident: usize,
        json: bool,
    },
}

/// `rollback`'s safety bound: the matched-block count it tolerates without an
/// explicit `--max-blocks`, and the hard cap no invocation may cross.
const DEFAULT_MAX_BLOCKS: u64 = 10_000;
const MAX_BLOCKS_CAP: u64 = 100_000;
/// `pregen-plan`'s default per-tick budget and its radius ceiling. The cap
/// sits past the vanilla world border (~30M blocks = ~1.875M chunks), so any
/// real plan fits while a typo cannot spin the ring arithmetic for hours.
const DEFAULT_BATCH_SIZE: usize = 64;
const MAX_PREGEN_RADIUS: u32 = 2_000_000;
/// `pregen-run`'s resident-chunk ceiling when `--max-resident` is not given:
/// the unloader keeps at most this many unticketed chunks in memory, so the
/// default run is already bounded.
const DEFAULT_MAX_RESIDENT_CHUNKS: usize = 1024;

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

/// `--pos x,y,z` / `--center cx,cz` argument split on commas. A wrong
/// component count or a non-integer component is a usage error, never a
/// clamp or a silent drop.
fn parse_i32_list(flag: &str, value: &str, count: usize) -> Result<Vec<i32>, CliError> {
    let parts: Vec<&str> = value.split(',').collect();
    if parts.len() != count {
        return Err(usage(format!(
            "{flag} expects {count} comma-separated integers, got {value:?}"
        )));
    }
    parts
        .iter()
        .map(|p| {
            p.trim()
                .parse::<i32>()
                .map_err(|_| usage(format!("{flag} expects integers, got {value:?}")))
        })
        .collect()
}

fn parse_pos(flag: &str, value: &str) -> Result<[i32; 3], CliError> {
    let v = parse_i32_list(flag, value, 3)?;
    Ok([v[0], v[1], v[2]])
}

fn parse_center(flag: &str, value: &str) -> Result<(i32, i32), CliError> {
    let v = parse_i32_list(flag, value, 2)?;
    Ok((v[0], v[1]))
}

fn parse_u64(flag: &str, value: &str) -> Result<u64, CliError> {
    value.parse::<u64>().map_err(|_| {
        usage(format!(
            "{flag} expects a non-negative integer, got {value:?}"
        ))
    })
}

/// The shared flag set of `audit-query` and `rollback`: `--log` plus the
/// `AuditQuery` filters. `--in`/`--out`/`--max-blocks`/`--limit` are also
/// collected so each command can reject the other's flags with a precise
/// message instead of a bare "unknown option".
struct AuditFlags {
    log: Option<PathBuf>,
    input: Option<PathBuf>,
    output: Option<PathBuf>,
    actor: Option<String>,
    dim: Option<String>,
    pos: Option<[i32; 3]>,
    radius: Option<i32>,
    since_tick: Option<u64>,
    limit: Option<u64>,
    max_blocks: Option<u64>,
    json: bool,
}

fn parse_audit_flags(cmd: &str, args: &[String]) -> Result<AuditFlags, CliError> {
    let mut flags = AuditFlags {
        log: None,
        input: None,
        output: None,
        actor: None,
        dim: None,
        pos: None,
        radius: None,
        since_tick: None,
        limit: None,
        max_blocks: None,
        json: false,
    };
    let mut i = 0;
    while i < args.len() {
        let arg = args[i].as_str();
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
            "--log" => flags.log = Some(PathBuf::from(take_value("--log")?)),
            "--in" => flags.input = Some(PathBuf::from(take_value("--in")?)),
            "--out" => flags.output = Some(PathBuf::from(take_value("--out")?)),
            "--actor" => flags.actor = Some(take_value("--actor")?),
            "--dim" => flags.dim = Some(take_value("--dim")?),
            "--pos" => flags.pos = Some(parse_pos("--pos", &take_value("--pos")?)?),
            "--radius" => {
                let v = take_value("--radius")?;
                flags.radius = Some(
                    v.parse::<i32>()
                        .map_err(|_| usage(format!("--radius expects an integer, got {v:?}")))?,
                );
            }
            "--since-tick" => {
                flags.since_tick = Some(parse_u64("--since-tick", &take_value("--since-tick")?)?)
            }
            "--limit" => flags.limit = Some(parse_u64("--limit", &take_value("--limit")?)?),
            "--max-blocks" => {
                flags.max_blocks = Some(parse_u64("--max-blocks", &take_value("--max-blocks")?)?)
            }
            "--json" => flags.json = true,
            other if other.starts_with('-') => {
                return Err(usage(format!("{cmd}: unknown option {other:?}")))
            }
            other => return Err(usage(format!("{cmd}: unexpected argument {other:?}"))),
        }
        i += 1;
    }
    // --pos and --radius are one spatial filter: giving only half of it is a
    // bad command line, not an implied infinite or zero radius.
    match (flags.pos, flags.radius) {
        (Some(_), None) => return Err(usage(format!("{cmd}: --pos requires --radius"))),
        (None, Some(_)) => return Err(usage(format!("{cmd}: --radius requires --pos"))),
        _ => {}
    }
    Ok(flags)
}

fn build_audit_query(flags: &AuditFlags) -> AuditQuery {
    let mut query = AuditQuery::new();
    if let Some(actor) = &flags.actor {
        query = query.actor(actor.clone());
    }
    if let Some(dim) = &flags.dim {
        query = query.dimension(dim.clone());
    }
    if let (Some(pos), Some(radius)) = (flags.pos, flags.radius) {
        query = query.within(pos, radius);
    }
    if let Some(tick) = flags.since_tick {
        query = query.since_tick(tick);
    }
    query
}

/// `--shape` for either pregen subcommand; anything but the two documented
/// footprints is a usage error, never a silent default.
fn parse_pregen_shape(cmd: &str, value: &str) -> Result<PregenShape, CliError> {
    match value {
        "square" => Ok(PregenShape::Square),
        "circle" => Ok(PregenShape::Circle),
        _ => Err(usage(format!(
            "{cmd}: unknown shape {value:?}; expected \"square\" or \"circle\""
        ))),
    }
}

fn parse_pregen_flags(args: &[String]) -> Result<Command, CliError> {
    let mut radius = None;
    let mut shape = PregenShape::Square;
    let mut center = (0, 0);
    let mut batch_size = DEFAULT_BATCH_SIZE;
    let mut json = false;
    let mut i = 0;
    while i < args.len() {
        let arg = args[i].as_str();
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
            "--radius" => {
                let v = take_value("--radius")?;
                let r = parse_u64("--radius", &v)?;
                radius = Some(u32::try_from(r).map_err(|_| {
                    usage(format!(
                        "pregen-plan: --radius {r} exceeds {MAX_PREGEN_RADIUS}"
                    ))
                })?);
            }
            "--shape" => {
                shape = parse_pregen_shape("pregen-plan", &take_value("--shape")?)?
            }
            "--center" => center = parse_center("--center", &take_value("--center")?)?,
            "--batch-size" => {
                let v = take_value("--batch-size")?;
                let n = parse_u64("--batch-size", &v)?;
                batch_size = usize::try_from(n).ok().filter(|&n| n >= 1).ok_or_else(|| {
                    usage(format!(
                        "pregen-plan: --batch-size must be at least 1, got {v:?}"
                    ))
                })?;
            }
            "--json" => json = true,
            other if other.starts_with('-') => {
                return Err(usage(format!("pregen-plan: unknown option {other:?}")))
            }
            other => return Err(usage(format!("pregen-plan: unexpected argument {other:?}"))),
        }
        i += 1;
    }
    let radius = require("--radius", radius)?;
    if radius > MAX_PREGEN_RADIUS {
        return Err(usage(format!(
            "pregen-plan: --radius {radius} exceeds the {MAX_PREGEN_RADIUS}-chunk cap"
        )));
    }
    Ok(Command::PregenPlan {
        radius,
        shape,
        center,
        batch_size,
        json,
    })
}

/// `pregen-run` shares `pregen-plan`'s footprint flags but drives a real
/// sweep, so it takes `--budget-per-tick` (required, at least 1 — a zero
/// budget would spin forever reporting nothing) and `--max-resident` for the
/// unloader cap instead of `--batch-size`.
fn parse_pregen_run_flags(args: &[String]) -> Result<Command, CliError> {
    let mut radius = None;
    let mut shape = PregenShape::Square;
    let mut center = (0, 0);
    let mut budget_per_tick = None;
    let mut max_resident = DEFAULT_MAX_RESIDENT_CHUNKS;
    let mut json = false;
    let mut i = 0;
    while i < args.len() {
        let arg = args[i].as_str();
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
            "--radius" => {
                let v = take_value("--radius")?;
                let r = parse_u64("--radius", &v)?;
                radius = Some(u32::try_from(r).map_err(|_| {
                    usage(format!(
                        "pregen-run: --radius {r} exceeds {MAX_PREGEN_RADIUS}"
                    ))
                })?);
            }
            "--shape" => {
                shape = parse_pregen_shape("pregen-run", &take_value("--shape")?)?
            }
            "--center" => center = parse_center("--center", &take_value("--center")?)?,
            "--budget-per-tick" => {
                let v = take_value("--budget-per-tick")?;
                let n = parse_u64("--budget-per-tick", &v)?;
                budget_per_tick =
                    Some(usize::try_from(n).ok().filter(|&n| n >= 1).ok_or_else(
                        || {
                            usage(format!(
                                "pregen-run: --budget-per-tick must be at least 1, got {v:?}"
                            ))
                        },
                    )?);
            }
            "--max-resident" => {
                let v = take_value("--max-resident")?;
                let n = parse_u64("--max-resident", &v)?;
                max_resident = usize::try_from(n).map_err(|_| {
                    usage(format!("pregen-run: --max-resident {n} is too large"))
                })?;
            }
            "--json" => json = true,
            other if other.starts_with('-') => {
                return Err(usage(format!("pregen-run: unknown option {other:?}")))
            }
            other => {
                return Err(usage(format!(
                    "pregen-run: unexpected argument {other:?}"
                )))
            }
        }
        i += 1;
    }
    let radius = require("--radius", radius)?;
    if radius > MAX_PREGEN_RADIUS {
        return Err(usage(format!(
            "pregen-run: --radius {radius} exceeds the {MAX_PREGEN_RADIUS}-chunk cap"
        )));
    }
    Ok(Command::PregenRun {
        radius,
        shape,
        center,
        budget_per_tick: require("--budget-per-tick", budget_per_tick)?,
        max_resident,
        json,
    })
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
        "audit-query" => {
            let flags = parse_audit_flags("audit-query", &args[1..])?;
            for (present, name) in [
                (flags.input.is_some(), "--in"),
                (flags.output.is_some(), "--out"),
                (flags.max_blocks.is_some(), "--max-blocks"),
            ] {
                if present {
                    return Err(usage(format!(
                        "audit-query: {name} only applies to rollback"
                    )));
                }
            }
            let query = build_audit_query(&flags);
            Ok(Command::AuditQuery {
                log: require("--log", flags.log)?,
                limit: flags
                    .limit
                    .map(|n| usize::try_from(n).unwrap_or(usize::MAX)),
                query,
                json: flags.json,
            })
        }
        "rollback" => {
            let flags = parse_audit_flags("rollback", &args[1..])?;
            if flags.limit.is_some() {
                return Err(usage("rollback: --limit only applies to audit-query"));
            }
            let max_blocks = flags.max_blocks.unwrap_or(DEFAULT_MAX_BLOCKS);
            if max_blocks > MAX_BLOCKS_CAP {
                return Err(usage(format!(
                    "rollback: --max-blocks {max_blocks} exceeds the hard cap {MAX_BLOCKS_CAP}"
                )));
            }
            let query = build_audit_query(&flags);
            Ok(Command::Rollback {
                log: require("--log", flags.log)?,
                input: require("--in", flags.input)?,
                output: require("--out", flags.output)?,
                query,
                max_blocks,
                json: flags.json,
            })
        }
        "pregen-plan" => parse_pregen_flags(&args[1..]),
        "pregen-run" => parse_pregen_run_flags(&args[1..]),
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

// ------------------------------------------------------------- audit ops

fn action_name(action: AuditAction) -> &'static str {
    match action {
        AuditAction::BlockPlace => "block_place",
        AuditAction::BlockBreak => "block_break",
        AuditAction::Explosion => "explosion",
        AuditAction::ContainerInsert => "container_insert",
        AuditAction::ContainerTake => "container_take",
        AuditAction::EntityKill => "entity_kill",
    }
}

/// One audit record as a deterministic JSON object.
fn record_value(r: &AuditRecord) -> DataValue {
    map(vec![
        ("action", DataValue::Str(action_name(r.action).to_string())),
        ("actor", DataValue::Str(r.actor.clone())),
        ("dimension", DataValue::Str(r.dimension.clone())),
        ("item_count", DataValue::Int(i64::from(r.item_count))),
        (
            "item_id",
            r.item_id
                .as_ref()
                .map_or(DataValue::Null, |s| DataValue::Str(s.clone())),
        ),
        ("new_state", DataValue::Int(i64::from(r.new_state))),
        ("old_state", DataValue::Int(i64::from(r.old_state))),
        (
            "pos",
            DataValue::List(
                r.pos
                    .iter()
                    .map(|&c| DataValue::Int(i64::from(c)))
                    .collect(),
            ),
        ),
        ("rolled_back", DataValue::Bool(r.rolled_back)),
        ("seq", DataValue::Int(r.seq as i64)),
        ("tick", DataValue::Int(r.tick as i64)),
    ])
}

/// One audit record as a greppable `key=value` row.
fn record_row(r: &AuditRecord) -> String {
    let mut row = format!(
        "seq={} tick={} actor={} dim={} action={} pos=({},{},{}) old={} new={}",
        r.seq,
        r.tick,
        r.actor,
        r.dimension,
        action_name(r.action),
        r.pos[0],
        r.pos[1],
        r.pos[2],
        r.old_state,
        r.new_state,
    );
    if let Some(item) = &r.item_id {
        row.push_str(&format!(" item={item}"));
        if r.item_count > 0 {
            row.push_str(&format!(" x{}", r.item_count));
        }
    }
    if r.rolled_back {
        row.push_str(" rolled_back");
    }
    row
}

/// Recover `path` as an `AUD1` log and warn — on stderr, so stdout stays a
/// clean record stream — when bytes past the last intact frame were dropped.
/// Returns the log plus the dropped tail's byte count.
fn recover_audit_log(path: &Path) -> Result<(AuditLog, usize), CliError> {
    let (log, truncated) = AuditLog::recover_from_file(path)
        .map_err(|e| fail(format!("cannot read {}: {e}", path.display())))?;
    if truncated > 0 {
        eprintln!(
            "warning: {}: dropped {truncated} trailing byte(s) after the last intact frame",
            path.display()
        );
    }
    Ok((log, truncated))
}

fn cmd_audit_query(
    log_path: &Path,
    query: &AuditQuery,
    limit: Option<usize>,
    json: bool,
) -> Result<(), CliError> {
    let (log, truncated) = recover_audit_log(log_path)?;
    let mut records = log.query(query);
    let matched = records.len();
    if let Some(limit) = limit {
        records.truncate(limit);
    }
    let returned = records.len();

    if json {
        println!(
            "{}",
            to_data_json(&map(vec![
                ("log", DataValue::Str(log_path.display().to_string())),
                ("matched", int(matched)),
                (
                    "records",
                    DataValue::List(records.iter().map(|r| record_value(r)).collect()),
                ),
                ("returned", int(returned)),
                (
                    "schema",
                    DataValue::Str("megu-audit-query/1".to_string()),
                ),
                ("total_records", int(log.len())),
                ("truncated_tail_bytes", int(truncated)),
            ]))
        );
        return Ok(());
    }

    if returned < matched {
        println!(
            "{}: {} audit record(s), {matched} match the filter (showing {returned})",
            log_path.display(),
            log.len()
        );
    } else {
        println!(
            "{}: {} audit record(s), {matched} match the filter",
            log_path.display(),
            log.len()
        );
    }
    for r in records {
        println!("{}", record_row(r));
    }
    Ok(())
}

fn cmd_rollback(
    log_path: &Path,
    input: &Path,
    output: &Path,
    query: &AuditQuery,
    max_blocks: u64,
    json: bool,
) -> Result<(), CliError> {
    let (mut log, truncated) = recover_audit_log(log_path)?;
    let bytes = read_input(input)?;
    let (ctx, target) =
        decode_snapshot(&bytes).map_err(|e| fail(format!("{}: {e}", input.display())))?;

    // Everything below runs in memory first: a refused or failed rollback
    // leaves --out — and of course --in — exactly as it was.
    let deltas = log.rollback_blocks(query);
    if deltas.len() as u64 > max_blocks {
        return Err(usage(format!(
            "{}: rollback would rewrite {} block(s), exceeding --max-blocks {max_blocks}; {} left untouched",
            log_path.display(),
            deltas.len(),
            output.display()
        )));
    }
    log.apply_rollback_to_context(&ctx, &deltas);
    let out = encode_snapshot(&ctx, target);
    // Verify the artifact before it is written: the restored context must
    // re-seal as a valid STV1 under the exact target the input carried.
    let (_, verified) = decode_snapshot(&out)
        .map_err(|e| fail(format!("rollback output failed verification: {e}")))?;
    if verified != target {
        return Err(fail(
            "rollback output changed the snapshot's target version",
        ));
    }
    write_file_atomic(output, &out)
        .map_err(|e| fail(format!("cannot write {}: {e}", output.display())))?;

    if json {
        println!(
            "{}",
            to_data_json(&map(vec![
                ("input_bytes", int(bytes.len())),
                ("input_file", DataValue::Str(input.display().to_string())),
                ("log", DataValue::Str(log_path.display().to_string())),
                ("max_blocks", DataValue::Int(max_blocks as i64)),
                ("output_bytes", int(out.len())),
                ("output_file", DataValue::Str(output.display().to_string())),
                ("rolled_back_blocks", int(deltas.len())),
                ("schema", DataValue::Str("megu-rollback/1".to_string()),),
                ("target_version", DataValue::Str(target.id().to_string())),
                ("total_records", int(log.len())),
                ("truncated_tail_bytes", int(truncated)),
            ]))
        );
        return Ok(());
    }

    println!(
        "{}: {} audit record(s) recovered",
        log_path.display(),
        log.len()
    );
    println!(
        "{}: rolled back {} block change(s) (minecraft {target})",
        input.display(),
        deltas.len()
    );
    println!(
        "{}: restored snapshot written, {} bytes",
        output.display(),
        out.len()
    );
    Ok(())
}

// ------------------------------------------------------------- pregen-plan

fn shape_name(shape: PregenShape) -> &'static str {
    match shape {
        PregenShape::Square => "square",
        PregenShape::Circle => "circle",
    }
}

/// The last chunk the spiral emits. Ring `k` is walked clockwise from its
/// northwest corner and ends on its west edge: `(-k, -k + 1)` for `Square`
/// (every ring cell is in shape), `(-k, 0)` for `Circle` (`dz = 0` is the
/// only west-edge offset with `k^2 + dz^2 <= k^2`). Both collapse to the
/// center at radius 0. Derived rather than walked so `pregen-plan` stays
/// O(1) in memory at any radius.
fn last_plan_chunk(center: (i32, i32), radius: u32, shape: PregenShape) -> (i32, i32) {
    if radius == 0 {
        return center;
    }
    let r = radius as i32;
    let (dx, dz) = match shape {
        PregenShape::Square => (-r, -r + 1),
        PregenShape::Circle => (-r, 0),
    };
    (center.0.wrapping_add(dx), center.1.wrapping_add(dz))
}

fn chunk_value(coord: (i32, i32)) -> DataValue {
    DataValue::List(vec![
        DataValue::Int(i64::from(coord.0)),
        DataValue::Int(i64::from(coord.1)),
    ])
}

fn cmd_pregen_plan(
    radius: u32,
    shape: PregenShape,
    center: (i32, i32),
    batch_size: usize,
    json: bool,
) -> Result<(), CliError> {
    let mut job = PregenJob::new(center.0, center.1, radius, shape);
    let total = job.progress().total;
    // The job hands the first spiral cell out itself; both shapes always
    // cover the center, so the batch is never empty.
    let first = job.take_batch(1).into_iter().next().unwrap_or(center);
    let last = last_plan_chunk(center, radius, shape);
    // Ring 0 is the center plus one ring per radius step; every ring at
    // k <= radius emits at least the four axis cells, even for `Circle`.
    let rings = u64::from(radius) + 1;
    // batch_size >= 1 is enforced by the parser.
    let ticks = total.div_ceil(batch_size as u64);

    if json {
        println!(
            "{}",
            to_data_json(&map(vec![
                ("batch_size", int(batch_size)),
                ("center", chunk_value(center)),
                ("first_chunk", chunk_value(first)),
                ("last_chunk", chunk_value(last)),
                ("radius_chunks", DataValue::Int(i64::from(radius))),
                ("ring_count", DataValue::Int(rings as i64)),
                (
                    "schema",
                    DataValue::Str("megu-pregen-plan/1".to_string()),
                ),
                ("shape", DataValue::Str(shape_name(shape).to_string())),
                ("ticks", DataValue::Int(ticks as i64)),
                ("total_chunks", DataValue::Int(total as i64)),
            ]))
        );
        return Ok(());
    }

    println!(
        "pregen-plan: {} radius {radius} chunk(s) centered at chunk ({}, {})",
        shape_name(shape),
        center.0,
        center.1
    );
    println!("  {total} chunk(s) over {rings} ring(s) (rings 0..={radius})");
    println!(
        "  first chunk ({}, {}), last chunk ({}, {})",
        first.0, first.1, last.0, last.1
    );
    println!("  {ticks} tick(s) at {batch_size} chunk(s)/tick");
    Ok(())
}

// ------------------------------------------------------------- pregen-run

/// `pregen-run` executes the same spiral `pregen-plan` sizes, against a fresh
/// `ServerContext`: each tick `drive_tick`s at most `budget_per_tick` chunks
/// into `minecraft:overworld` (the CLI generates no block payload — the
/// resident overlay and the counters are the product), then the ticket-LRU
/// unloader evicts unticketed residents down to `max_resident` with no grace
/// window, so the resident set — and the run's RAM — stays bounded at any
/// radius. Evicted overlays are empty and simply dropped; on a live server
/// the `flushed` sink is where they would be persisted.
fn cmd_pregen_run(
    radius: u32,
    shape: PregenShape,
    center: (i32, i32),
    budget_per_tick: usize,
    max_resident: usize,
    json: bool,
) -> Result<(), CliError> {
    let ctx = ServerContext::new();
    let mut job = PregenJob::new(center.0, center.1, radius, shape);
    let config = PregenRunConfig {
        dimension: "minecraft:overworld",
        budget_per_tick,
        max_resident: Some(max_resident),
        grace_ticks: 0,
    };
    let report = job.run(&ctx, &config, |_, _| {}, |_| {});

    if json {
        println!(
            "{}",
            to_data_json(&map(vec![
                ("budget_per_tick", int(budget_per_tick)),
                ("center", chunk_value(center)),
                ("dimension", DataValue::Str(config.dimension.to_string())),
                ("evicted_chunks", DataValue::Int(report.evicted_chunks as i64)),
                (
                    "generated_chunks",
                    DataValue::Int(report.generated_chunks as i64),
                ),
                ("max_resident", int(max_resident)),
                (
                    "peak_resident_chunks",
                    int(report.peak_resident_chunks),
                ),
                ("radius_chunks", DataValue::Int(i64::from(radius))),
                ("resident_chunks", int(report.resident_chunks)),
                (
                    "schema",
                    DataValue::Str("megu-pregen-run/1".to_string()),
                ),
                ("shape", DataValue::Str(shape_name(shape).to_string())),
                (
                    "state",
                    DataValue::Str(format!("{:?}", report.final_state).to_lowercase()),
                ),
                ("ticks_run", DataValue::Int(report.ticks_run as i64)),
            ]))
        );
        return Ok(());
    }

    println!(
        "pregen-run: {} radius {radius} chunk(s) centered at chunk ({}, {})",
        shape_name(shape),
        center.0,
        center.1
    );
    println!(
        "  {} chunk(s) generated in {} over {} tick(s) at {budget_per_tick} chunk(s)/tick",
        report.generated_chunks, config.dimension, report.ticks_run
    );
    println!(
        "  peak resident {} chunk(s); {} evicted by the LRU unloader, {} still resident",
        report.peak_resident_chunks, report.evicted_chunks, report.resident_chunks
    );
    Ok(())
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let command = match parse_args(&args) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("megu-world-migrate: {}", e.message);
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
        Command::AuditQuery {
            log,
            query,
            limit,
            json,
        } => cmd_audit_query(&log, &query, limit, json),
        Command::Rollback {
            log,
            input,
            output,
            query,
            max_blocks,
            json,
        } => cmd_rollback(&log, &input, &output, &query, max_blocks, json),
        Command::PregenPlan {
            radius,
            shape,
            center,
            batch_size,
            json,
        } => cmd_pregen_plan(radius, shape, center, batch_size, json),
        Command::PregenRun {
            radius,
            shape,
            center,
            budget_per_tick,
            max_resident,
            json,
        } => cmd_pregen_run(radius, shape, center, budget_per_tick, max_resident, json),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("megu-world-migrate: {}", e.message);
            ExitCode::from(e.code)
        }
    }
}
