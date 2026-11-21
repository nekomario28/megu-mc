// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later

//! `mc-spine-mod-check` — zero-dependency modpack pre-flight checker and
//! mod-JAR mixin scanner.
//!
//! `--modpack` points [`modpack::ModpackManifest::from_path`] and
//! [`analyze_modpack`] at a Modrinth `.mrpack` archive, a standalone
//! `modrinth.index.json`, or a CurseForge `manifest.json` / pack `.zip`
//! and reports — in plain language or deterministic JSON — whether the
//! pack's Minecraft version matches this server, which redundant
//! performance mods are skipped automatically, which files run natively,
//! and which Java JARs still need a bytecode inspection.
//!
//! `--scan-jar` runs [`class_mixin_scanner::scan_jar_path_with_options`]
//! on one mod JAR: every `*.mixins.json` config is read, the registered
//! mixin classes' bytecode annotations are reduced to injection sites,
//! and [`mixin_lifter::lift_mixin_sites`] either lifts each site onto a
//! standard mc-spine event hook (Tier 2, bounded JVM shim) or reports it
//! unliftable with the reason it stays at Tier 3.
//!
//! Exit codes mirror `tools/mod_launcher.py --modpack`: `0` the input is
//! fully usable on this server, `2` a version mismatch, unliftable mixin
//! sites, or invalid arguments, `1` an unreadable or unsafe input.

use std::collections::BTreeSet;
use std::env;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use mod_adapter_core::class_mixin_scanner::{
    scan_jar_path_with_options, JarMixinScanReport, JarScanOptions,
};
use mod_adapter_core::mixin_lifter::{
    LiftedBinding, MixinLiftVerdict, MixinSite, UnliftableReason,
};
use mod_adapter_core::modpack::{analyze_modpack, ModpackManifest, ModpackReadiness};
use mod_adapter_core::version_compat::McVersion;

const USAGE: &str = "\
mc-spine-mod-check — check whether a modpack can run on this mc-spine server,
or scan one mod JAR's mixin bytecode for Tier 2 liftability

Usage: mc-spine-mod-check --modpack <path> [options]
       mc-spine-mod-check --scan-jar <mod.jar> [--include-client-mixins] [--json]

--modpack accepts a Modrinth .mrpack archive, a standalone modrinth.index.json,
or a CurseForge manifest.json / pack .zip, and reports — before anything is
downloaded or launched — which files run natively, which redundant
performance mods are skipped automatically, and which Java mods still need
a compatibility inspection.

--scan-jar parses one mod JAR's .class bytecode directly, with no JVM: every
*.mixins.json config is read, only the classes it registers are parsed, and
each mixin injection site is either lifted onto a standard mc-spine event
hook (Tier 2, bounded JVM shim) or reported unliftable with the reason it
stays at Tier 3.

Options:
  --modpack <path>        modpack file to check
  --scan-jar <path>       mod JAR to bytecode-scan for mixin sites
                          (mutually exclusive with --modpack)
  --mc-version <version>  Minecraft version this server runs: 1.21.1 or 26.3
                          (default: 1.21.1; --modpack only)
  --flagship <ids>        comma-separated mod ids handled by a flagship
                          native adapter (default: create; --modpack only)
  --profiles <ids>        comma-separated mod ids covered by declarative
                          offload profiles (default: none; --modpack only)
  --include-client-mixins scan `client`-array mixin classes too (default:
                          skip them — this is a dedicated server)
  --json                  print the machine-readable report instead of the
                          human summary
  -h, --help              show this help and exit

Exit status: for --modpack, 0 the pack can run on this server (native, with
automatic skips, or pending a bytecode inspection), 2 the pack targets a
different Minecraft version or the arguments were invalid, 1 the pack could
not be read or is unsafe. For --scan-jar, 0 every mixin site lifted to a
standard event hook (Tier 2 eligible), 2 at least one site stays at Tier 3
or the arguments were invalid, 1 the JAR or a .class member could not be
read or parsed.";

struct Cli {
    modpack: PathBuf,
    target: McVersion,
    flagship: BTreeSet<String>,
    profiles: BTreeSet<String>,
    json: bool,
}

struct JarCli {
    jar: PathBuf,
    /// [`JarScanOptions::server_only`] — `false` once
    /// `--include-client-mixins` is passed.
    server_only: bool,
    json: bool,
}

enum Action {
    Help,
    RunModpack(Cli),
    ScanJar(JarCli),
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
/// positional, bad `--mc-version`, or flag applied to the wrong mode is a
/// usage error (exit 2), never a silent default.
fn parse_args(argv: &[OsString]) -> Result<Action, String> {
    let mut modpack: Option<PathBuf> = None;
    let mut scan_jar: Option<PathBuf> = None;
    let mut mc_version: Option<String> = None;
    let mut flagship: Option<BTreeSet<String>> = None;
    let mut profiles: Option<BTreeSet<String>> = None;
    let mut include_client_mixins = false;
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
            "--include-client-mixins" => {
                if inline.is_some() {
                    return Err("--include-client-mixins does not take a value".to_string());
                }
                include_client_mixins = true;
            }
            "--modpack" => {
                let v = take_value(argv, &mut i, "--modpack", inline_os.as_ref())?;
                modpack = Some(PathBuf::from(v));
            }
            "--scan-jar" => {
                let v = take_value(argv, &mut i, "--scan-jar", inline_os.as_ref())?;
                scan_jar = Some(PathBuf::from(v));
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
    match (modpack, scan_jar) {
        (Some(_), Some(_)) => Err("--modpack and --scan-jar are mutually exclusive".to_string()),
        (None, None) => Err("one of --modpack or --scan-jar is required".to_string()),
        (None, Some(jar)) => {
            for (flag, present) in [
                ("--mc-version", mc_version.is_some()),
                ("--flagship", flagship.is_some()),
                ("--profiles", profiles.is_some()),
            ] {
                if present {
                    return Err(format!("{flag} only applies to --modpack"));
                }
            }
            Ok(Action::ScanJar(JarCli {
                jar,
                server_only: !include_client_mixins,
                json,
            }))
        }
        (Some(modpack), None) => {
            if include_client_mixins {
                return Err("--include-client-mixins only applies to --scan-jar".to_string());
            }
            let declared = mc_version.unwrap_or_else(|| "1.21.1".to_string());
            let target = McVersion::parse(declared.trim())
                .filter(|v| *v == McVersion::V1_21_1 || *v == McVersion::V26_3)
                .ok_or_else(|| {
                    format!("invalid --mc-version '{declared}' (expected 1.21.1 or 26.3)")
                })?;
            Ok(Action::RunModpack(Cli {
                modpack,
                target,
                flagship: flagship.unwrap_or_else(|| csv_ids(&OsString::from("create")).unwrap()),
                profiles: profiles.unwrap_or_default(),
                json,
            }))
        }
    }
}

// ---------------------------------------------------------------------------
// --scan-jar report
// ---------------------------------------------------------------------------

fn lifted_bindings(verdict: &MixinLiftVerdict) -> &[LiftedBinding] {
    match verdict {
        MixinLiftVerdict::FullyLiftable(bindings) => bindings,
        MixinLiftVerdict::RequiresFallback { lifted, .. } => lifted,
    }
}

fn unliftable_sites(verdict: &MixinLiftVerdict) -> &[(MixinSite, UnliftableReason)] {
    match verdict {
        MixinLiftVerdict::FullyLiftable(_) => &[],
        MixinLiftVerdict::RequiresFallback { unliftable, .. } => unliftable,
    }
}

/// One site on a report line, e.g.
/// `com/x/Mixin -> net/minecraft/.../LivingEntity#hurt @Inject @At(HEAD)`.
fn describe_site(site: &MixinSite) -> String {
    let at = site
        .at
        .as_ref()
        .map(|p| p.to_string())
        .unwrap_or_else(|| "<none>".to_string());
    format!(
        "{} -> {}#{} {} @At({})",
        site.mixin_class, site.target_class, site.target_method, site.annotation, at
    )
}

/// Human-readable scan report: scan counts, the sites lifted onto Tier 2
/// standard event hooks, the sites that stay at Tier 3 with their
/// reasons, and the overall verdict.
fn format_scan_human(path: &Path, report: &JarMixinScanReport) -> String {
    use std::fmt::Write;
    let mut out = String::new();
    let _ = writeln!(out, "JAR mixin scan report for \"{}\"", path.display());
    let _ = writeln!(
        out,
        "Found {} mixin config(s); scanned {} class(es); skipped {} client-only mixin class(es).",
        report.mixin_configs_found, report.classes_scanned, report.skipped_client_mixins
    );
    if report.sites.is_empty() {
        let _ = writeln!(out, "No mixin injection sites found.");
    }
    let lifted = lifted_bindings(&report.verdict);
    if !lifted.is_empty() {
        let _ = writeln!(
            out,
            "Lifted {} site(s) onto standard Tier 2 event hooks:",
            lifted.len()
        );
        for binding in lifted {
            let _ = writeln!(
                out,
                "  - {} => {}",
                describe_site(&binding.site),
                binding.hook
            );
        }
    }
    let unliftable = unliftable_sites(&report.verdict);
    if !unliftable.is_empty() {
        let _ = writeln!(
            out,
            "{} site(s) cannot lift and stay at Tier 3:",
            unliftable.len()
        );
        for (site, reason) in unliftable {
            let _ = writeln!(out, "  - {} ({})", describe_site(site), reason);
        }
    }
    let _ = write!(
        out,
        "Result: {}",
        match &report.verdict {
            MixinLiftVerdict::FullyLiftable(_) =>
                "fully liftable — every mixin site maps onto a standard mc-spine event hook; this JAR is Tier 2 eligible (bounded JVM shim).".to_string(),
            MixinLiftVerdict::RequiresFallback { unliftable, .. } => format!(
                "requires fallback — {} mixin site(s) cannot be proven safe; this JAR stays at Tier 3.",
                unliftable.len()
            ),
        }
    );
    out
}

/// `json.dumps`-compatible string escaping (ensure_ascii) — the same
/// shape `modpack`'s pre-flight JSON uses so both reports diff cleanly.
fn json_string(out: &mut String, s: &str) {
    use std::fmt::Write;
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0c}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (' '..='~').contains(&c) => out.push(c),
            c => {
                let mut buf = [0u16; 2];
                for u in c.encode_utf16(&mut buf) {
                    let _ = write!(out, "\\u{u:04x}");
                }
            }
        }
    }
    out.push('"');
}

/// Append `"key": "escaped"` (indent `level` x 2 spaces) plus a trailing
/// comma when `comma` — the `json.dumps(indent=2)` line shape.
fn json_field_str(out: &mut String, level: usize, key: &str, value: &str, comma: bool) {
    use std::fmt::Write;
    let _ = write!(out, "{:width$}\"{key}\": ", "", width = level * 2);
    json_string(out, value);
    let _ = writeln!(out, "{}", if comma { "," } else { "" });
}

/// Same line shape for a `string | null` field.
fn json_field_opt_str(out: &mut String, level: usize, key: &str, value: Option<&str>, comma: bool) {
    use std::fmt::Write;
    let _ = write!(out, "{:width$}\"{key}\": ", "", width = level * 2);
    match value {
        Some(v) => json_string(out, v),
        None => out.push_str("null"),
    }
    let _ = writeln!(out, "{}", if comma { "," } else { "" });
}

/// The `mixin_class`/`target_*`/`annotation`/`at` fields shared by
/// `lifted_hooks` and `unliftable` entries (all comma-terminated; the
/// caller appends the entry's final field).
fn json_site_fields(out: &mut String, site: &MixinSite) {
    json_field_str(out, 3, "mixin_class", &site.mixin_class, true);
    json_field_str(out, 3, "target_class", &site.target_class, true);
    json_field_str(out, 3, "target_method", &site.target_method, true);
    json_field_str(out, 3, "annotation", site.annotation.as_str(), true);
    json_field_opt_str(
        out,
        3,
        "at",
        site.at.as_ref().map(|p| p.to_string()).as_deref(),
        true,
    );
}

/// Deterministic `mc-spine-jar-mixin-scan/1` JSON form of a
/// [`JarMixinScanReport`].
fn scan_report_json(path: &Path, report: &JarMixinScanReport) -> String {
    use std::fmt::Write;
    let mut out = String::with_capacity(512 + report.sites.len() * 256);
    out.push_str("{\n");
    json_field_str(&mut out, 1, "schema", "mc-spine-jar-mixin-scan/1", true);
    json_field_str(&mut out, 1, "path", &path.display().to_string(), true);
    let _ = writeln!(
        out,
        "  \"mixin_configs_found\": {},",
        report.mixin_configs_found
    );
    let _ = writeln!(out, "  \"classes_scanned\": {},", report.classes_scanned);
    let _ = writeln!(
        out,
        "  \"skipped_client_mixins\": {},",
        report.skipped_client_mixins
    );
    let _ = writeln!(out, "  \"sites_found\": {},", report.sites.len());
    json_field_str(
        &mut out,
        1,
        "verdict",
        match &report.verdict {
            MixinLiftVerdict::FullyLiftable(_) => "fully_liftable",
            MixinLiftVerdict::RequiresFallback { .. } => "requires_fallback",
        },
        true,
    );

    let lifted = lifted_bindings(&report.verdict);
    if lifted.is_empty() {
        let _ = writeln!(out, "  \"lifted_hooks\": [],");
    } else {
        let _ = writeln!(out, "  \"lifted_hooks\": [");
        for (i, binding) in lifted.iter().enumerate() {
            let last = i + 1 == lifted.len();
            let _ = writeln!(out, "    {{");
            json_field_str(&mut out, 3, "hook", binding.hook.as_str(), true);
            json_site_fields(&mut out, &binding.site);
            let _ = writeln!(out, "      \"cancellable\": {}", binding.cancellable);
            let _ = writeln!(out, "    }}{}", if last { "" } else { "," });
        }
        let _ = writeln!(out, "  ],");
    }

    let unliftable = unliftable_sites(&report.verdict);
    if unliftable.is_empty() {
        let _ = writeln!(out, "  \"unliftable\": []");
    } else {
        let _ = writeln!(out, "  \"unliftable\": [");
        for (i, (site, reason)) in unliftable.iter().enumerate() {
            let last = i + 1 == unliftable.len();
            let _ = writeln!(out, "    {{");
            json_site_fields(&mut out, site);
            json_field_str(&mut out, 3, "reason", reason.as_str(), false);
            let _ = writeln!(out, "    }}{}", if last { "" } else { "," });
        }
        let _ = writeln!(out, "  ]");
    }
    out.push_str("}\n");
    out
}

// ---------------------------------------------------------------------------

fn run_modpack(cli: Cli) -> ExitCode {
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

fn run_scan_jar(cli: &JarCli) -> ExitCode {
    let opts = JarScanOptions {
        server_only: cli.server_only,
    };
    let report = match scan_jar_path_with_options(&cli.jar, opts) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("mc-spine-mod-check: {e}");
            return ExitCode::FAILURE;
        }
    };
    if cli.json {
        print!("{}", scan_report_json(&cli.jar, &report));
    } else {
        println!("{}", format_scan_human(&cli.jar, &report));
    }
    match report.verdict {
        MixinLiftVerdict::FullyLiftable(_) => ExitCode::SUCCESS,
        MixinLiftVerdict::RequiresFallback { .. } => ExitCode::from(2),
    }
}

fn main() -> ExitCode {
    let argv: Vec<OsString> = env::args_os().skip(1).collect();
    match parse_args(&argv) {
        Ok(Action::Help) => {
            println!("{USAGE}");
            ExitCode::SUCCESS
        }
        Ok(Action::RunModpack(cli)) => run_modpack(cli),
        Ok(Action::ScanJar(cli)) => run_scan_jar(&cli),
        Err(msg) => {
            eprintln!("mc-spine-mod-check: {msg}");
            eprintln!("Try 'mc-spine-mod-check --help' for usage.");
            ExitCode::from(2)
        }
    }
}
