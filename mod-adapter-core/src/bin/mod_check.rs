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
//! `--scan-mods-dir` points the same engines at an entire `mods/` folder:
//! JARs whose filenames infer a [`fallback::BUILTIN_SUPERSEDED_PERF_MODS`]
//! id are skipped without reading a byte, every other JAR gets both the
//! bytecode mixin scan *and* [`culinary_adapter::CulinaryRegistry`]'s
//! generic culinary discovery, and each lands in one of five classes —
//! auto-skipped, native culinary, clean data, Tier 2 lifted, or Tier 3
//! requiring fallback.
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
    scan_jar_mixins_with_options, scan_jar_path_with_options, JarMixinScanReport, JarScanOptions,
};
use mod_adapter_core::culinary_adapter::{CulinaryDiscoveryStats, CulinaryRegistry};
use mod_adapter_core::fallback::BUILTIN_SUPERSEDED_PERF_MODS;
use mod_adapter_core::mixin_lifter::{
    LiftedBinding, MixinLiftVerdict, MixinSite, UnliftableReason,
};
use mod_adapter_core::modpack::{
    analyze_modpack, infer_candidate_mod_id, ModpackManifest, ModpackReadiness,
};
use mod_adapter_core::version_compat::McVersion;

const USAGE: &str = "\
mc-spine-mod-check — check whether a modpack can run on this mc-spine server,
scan one mod JAR's mixin bytecode for Tier 2 liftability, or inspect a whole
mods/ directory with zero JVM

Usage: mc-spine-mod-check --modpack <path> [options]
       mc-spine-mod-check --scan-jar <mod.jar> [--include-client-mixins] [--json]
       mc-spine-mod-check --scan-mods-dir <dir> [--include-client-mixins] [--json]

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

--scan-mods-dir inspects every *.jar inside a mods/ folder, still with no
JVM: JARs named like a built-in superseded performance mod (Sodium,
Lithium, Distant Horizons, ...) are skipped without reading a byte, every
other JAR gets both the bytecode mixin scan and the generic culinary data
discovery (cooking/cutting recipes, item/block tags, loot modifiers), and
each one is classified as auto-skipped, native culinary, clean data,
Tier 2 lifted, or requiring a Tier 3 fallback.

Options:
  --modpack <path>        modpack file to check
  --scan-jar <path>       mod JAR to bytecode-scan for mixin sites
                          (mutually exclusive with --modpack/--scan-mods-dir)
  --scan-mods-dir <dir>   directory of mod JARs to inspect together
                          (mutually exclusive with --modpack/--scan-jar)
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
read or parsed. For --scan-mods-dir, 0 no JAR requires a Tier 3 fallback,
2 at least one does (or the arguments were invalid), 1 the directory is
missing/unreadable or a JAR could not be read or parsed.";

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

struct ModsDirCli {
    dir: PathBuf,
    /// [`JarScanOptions::server_only`] — `false` once
    /// `--include-client-mixins` is passed.
    server_only: bool,
    json: bool,
}

enum Action {
    Help,
    RunModpack(Cli),
    ScanJar(JarCli),
    ScanModsDir(ModsDirCli),
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
    let mut mods_dir: Option<PathBuf> = None;
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
            "--scan-mods-dir" => {
                let v = take_value(argv, &mut i, "--scan-mods-dir", inline_os.as_ref())?;
                mods_dir = Some(PathBuf::from(v));
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
    let modes = modpack.is_some() as usize
        + scan_jar.is_some() as usize
        + mods_dir.is_some() as usize;
    match modes {
        0 => return Err("one of --modpack, --scan-jar or --scan-mods-dir is required".to_string()),
        1 => {}
        _ => {
            return Err(
                "--modpack, --scan-jar and --scan-mods-dir are mutually exclusive".to_string(),
            )
        }
    }
    let reject_modpack_only = |mode: &str| -> Result<(), String> {
        for (flag, present) in [
            ("--mc-version", mc_version.is_some()),
            ("--flagship", flagship.is_some()),
            ("--profiles", profiles.is_some()),
        ] {
            if present {
                return Err(format!("{flag} only applies to --modpack, not {mode}"));
            }
        }
        Ok(())
    };
    match (modpack, scan_jar, mods_dir) {
        (None, None, Some(dir)) => {
            reject_modpack_only("--scan-mods-dir")?;
            Ok(Action::ScanModsDir(ModsDirCli {
                dir,
                server_only: !include_client_mixins,
                json,
            }))
        }
        (None, Some(jar), None) => {
            reject_modpack_only("--scan-jar")?;
            Ok(Action::ScanJar(JarCli {
                jar,
                server_only: !include_client_mixins,
                json,
            }))
        }
        (Some(modpack), None, None) => {
            if include_client_mixins {
                return Err(
                    "--include-client-mixins only applies to --scan-jar or --scan-mods-dir"
                        .to_string(),
                );
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
        // More than one mode flag was rejected above.
        _ => unreachable!("mutually exclusive modes rejected before dispatch"),
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

/// Append `"key": <u64>` (indent `level` x 2 spaces) plus a trailing
/// comma when `comma`.
fn json_field_u64(out: &mut String, level: usize, key: &str, value: u64, comma: bool) {
    use std::fmt::Write;
    let _ = writeln!(
        out,
        "{:width$}\"{key}\": {value}{}",
        "",
        if comma { "," } else { "" },
        width = level * 2
    );
}

/// Same line shape for a `bool` field.
fn json_field_bool(out: &mut String, level: usize, key: &str, value: bool, comma: bool) {
    use std::fmt::Write;
    let _ = writeln!(
        out,
        "{:width$}\"{key}\": {value}{}",
        "",
        if comma { "," } else { "" },
        width = level * 2
    );
}

/// The `mixin_class`/`target_*`/`annotation`/`at` fields shared by
/// `lifted_hooks` and `unliftable` entries (all comma-terminated; the
/// caller appends the entry's final field).
fn json_site_fields(out: &mut String, level: usize, site: &MixinSite) {
    json_field_str(out, level, "mixin_class", &site.mixin_class, true);
    json_field_str(out, level, "target_class", &site.target_class, true);
    json_field_str(out, level, "target_method", &site.target_method, true);
    json_field_str(out, level, "annotation", site.annotation.as_str(), true);
    json_field_opt_str(
        out,
        level,
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
            json_site_fields(&mut out, 3, &binding.site);
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
            json_site_fields(&mut out, 3, site);
            json_field_str(&mut out, 3, "reason", reason.as_str(), false);
            let _ = writeln!(out, "    }}{}", if last { "" } else { "," });
        }
        let _ = writeln!(out, "  ]");
    }
    out.push_str("}\n");
    out
}

// ---------------------------------------------------------------------------
// --scan-mods-dir report
// ---------------------------------------------------------------------------

/// Per-JAR outcome of a `--scan-mods-dir` pass.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ModsDirClass {
    /// Filename infers a [`BUILTIN_SUPERSEDED_PERF_MODS`] id — skipped
    /// without reading a byte.
    SupersededPerfMod,
    /// Zero mixin sites and >=1 discovered cooking/cutting recipe or loot
    /// modifier — a Farmer's Delight-family mod or addon (Tier 1 native).
    NativeCulinary,
    /// Zero mixin sites and no culinary data — plain data/clean JAR.
    NativeDataOrClean,
    /// Mixin sites present, every one lifted onto a standard event hook.
    LiftedTier2,
    /// At least one mixin site could not be lifted — stays at Tier 3.
    RequiresFallback,
}

impl ModsDirClass {
    fn as_str(self) -> &'static str {
        match self {
            Self::SupersededPerfMod => "superseded_perf_mod",
            Self::NativeCulinary => "native_culinary",
            Self::NativeDataOrClean => "native_data_or_clean",
            Self::LiftedTier2 => "lifted_tier2",
            Self::RequiresFallback => "requires_fallback",
        }
    }
}

/// One `*.jar` entry in a directory scan report.
struct ModsDirJar {
    /// File name (the `jars` array and human lines key off this).
    name: String,
    /// [`infer_candidate_mod_id`] result over the file name.
    mod_id: Option<String>,
    class: ModsDirClass,
    /// Bytecode scan — `None` only for [`ModsDirClass::SupersededPerfMod`].
    scan: Option<JarMixinScanReport>,
    /// Per-JAR culinary discovery stats (merged totals live on the report).
    culinary: Option<CulinaryDiscoveryStats>,
}

/// Merged culinary counters across every scanned JAR.
#[derive(Default)]
struct ModsDirCulinaryTotals {
    jar_members: u64,
    data_files: u64,
    cooking_recipes: u64,
    cutting_recipes: u64,
    skipped_recipes: u64,
    tag_files: u64,
    tag_values: u64,
    loot_modifiers: u64,
    food_specs: u64,
}

impl ModsDirCulinaryTotals {
    fn add(&mut self, s: &CulinaryDiscoveryStats) {
        self.jar_members += s.jar_members as u64;
        self.data_files += s.data_files as u64;
        self.cooking_recipes += s.cooking_recipes as u64;
        self.cutting_recipes += s.cutting_recipes as u64;
        self.skipped_recipes += s.skipped_recipes as u64;
        self.tag_files += s.tag_files as u64;
        self.tag_values += s.tag_values as u64;
        self.loot_modifiers += s.loot_modifiers as u64;
        self.food_specs += s.food_specs as u64;
    }
}

/// The case/separator-insensitive comparison `modpack` applies to
/// [`BUILTIN_SUPERSEDED_PERF_MODS`]: `-`/`_` removed, lowercased, so an
/// inferred `embeddium-extra` still matches the catalog's `embeddium_extra`.
fn normalized_mod_key(id: &str) -> String {
    id.chars()
        .filter(|c| !matches!(c, '-' | '_'))
        .flat_map(char::to_lowercase)
        .collect()
}

/// Run the directory scan, returning per-JAR reports in deterministic
/// filename order, the ignored non-JAR entry count, and the merged
/// culinary totals. Any unreadable directory entry, unreadable JAR, or
/// fail-closed scanner error aborts the pass (exit 1 upstream) — a
/// silently skipped or half-scanned JAR would look identical to a clean
/// one, which is exactly what this tool exists to distinguish.
fn scan_mods_dir(
    cli: &ModsDirCli,
) -> Result<(Vec<ModsDirJar>, usize, ModsDirCulinaryTotals), String> {
    if !cli.dir.is_dir() {
        return Err(format!(
            "'{}': not an existing directory",
            cli.dir.display()
        ));
    }
    let entries = std::fs::read_dir(&cli.dir).map_err(|e| {
        format!("'{}': cannot read mods directory: {e}", cli.dir.display())
    })?;
    let mut names = Vec::new();
    let mut non_jar = 0usize;
    for entry in entries {
        let entry = entry.map_err(|e| {
            format!(
                "'{}': cannot list directory entry: {e}",
                cli.dir.display()
            )
        })?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.to_ascii_lowercase().ends_with(".jar") {
            names.push(name);
        } else {
            non_jar += 1;
        }
    }
    // Deterministic order: case-insensitive filename, byte order as the
    // tiebreak — the report never depends on readdir order.
    names.sort_by(|a, b| {
        a.to_lowercase()
            .cmp(&b.to_lowercase())
            .then_with(|| a.cmp(b))
    });

    let superseded: BTreeSet<String> = BUILTIN_SUPERSEDED_PERF_MODS
        .iter()
        .map(|id| normalized_mod_key(id))
        .collect();
    let opts = JarScanOptions {
        server_only: cli.server_only,
    };
    let mut registry = CulinaryRegistry::new();
    let mut culinary_totals = ModsDirCulinaryTotals::default();
    let mut jars = Vec::with_capacity(names.len());

    for name in names {
        let mod_id = infer_candidate_mod_id(&name);
        if mod_id
            .as_deref()
            .map(|id| superseded.contains(&normalized_mod_key(id)))
            .unwrap_or(false)
        {
            jars.push(ModsDirJar {
                name,
                mod_id,
                class: ModsDirClass::SupersededPerfMod,
                scan: None,
                culinary: None,
            });
            continue;
        }
        let path = cli.dir.join(&name);
        let bytes = std::fs::read(&path)
            .map_err(|e| format!("'{name}': cannot read JAR: {e}"))?;
        let scan = scan_jar_mixins_with_options(&bytes, opts)
            .map_err(|e| format!("'{name}': mixin scan failed: {e}"))?;
        let culinary = registry
            .load_from_jar_bytes(&bytes)
            .map_err(|e| format!("'{name}': culinary discovery failed: {e}"))?;
        let class = if !unliftable_sites(&scan.verdict).is_empty() {
            ModsDirClass::RequiresFallback
        } else if !scan.sites.is_empty() {
            ModsDirClass::LiftedTier2
        } else if culinary.cooking_recipes + culinary.cutting_recipes + culinary.loot_modifiers > 0
        {
            ModsDirClass::NativeCulinary
        } else {
            ModsDirClass::NativeDataOrClean
        };
        culinary_totals.add(&culinary);
        jars.push(ModsDirJar {
            name,
            mod_id,
            class,
            scan: Some(scan),
            culinary: Some(culinary),
        });
    }
    Ok((jars, non_jar, culinary_totals))
}

fn mods_dir_count(jars: &[ModsDirJar], class: ModsDirClass) -> usize {
    jars.iter().filter(|j| j.class == class).count()
}

/// Crisp operator summary: one line per JAR (culinary counts, lifted-site
/// count, or the exact unliftable sites for Tier 3 JARs), the merged
/// culinary totals, then a plain-language verdict line.
fn format_mods_dir_human(
    dir: &Path,
    jars: &[ModsDirJar],
    non_jar: usize,
    culinary: &ModsDirCulinaryTotals,
) -> String {
    use std::fmt::Write;
    let mut out = String::new();
    let _ = writeln!(
        out,
        "Mods directory scan of \"{}\"",
        dir.display()
    );
    let _ = writeln!(
        out,
        "Found {} JAR(s){}; {} built-in performance mod(s) skipped automatically.",
        jars.len(),
        if non_jar > 0 {
            format!(" ({non_jar} non-JAR entr{} ignored)", if non_jar == 1 { "y" } else { "ies" })
        } else {
            String::new()
        },
        mods_dir_count(jars, ModsDirClass::SupersededPerfMod),
    );
    for jar in jars {
        let id = jar.mod_id.as_deref().unwrap_or("<unknown id>");
        match jar.class {
            ModsDirClass::SupersededPerfMod => {
                let _ = writeln!(
                    out,
                    "  {} [{id}]: skipped — already covered by mc-spine built-in optimizations.",
                    jar.name
                );
            }
            ModsDirClass::NativeCulinary => {
                let c = jar.culinary.as_ref().expect("scanned jar has stats");
                let _ = writeln!(
                    out,
                    "  {} [{id}]: native culinary — discovered {} cooking, {} cutting, {} loot modifier(s); runs natively (Tier 1).",
                    jar.name, c.cooking_recipes, c.cutting_recipes, c.loot_modifiers
                );
            }
            ModsDirClass::NativeDataOrClean => {
                let _ = writeln!(
                    out,
                    "  {} [{id}]: clean — no mixin sites, nothing to lift; runs natively.",
                    jar.name
                );
            }
            ModsDirClass::LiftedTier2 => {
                let scan = jar.scan.as_ref().expect("scanned jar has report");
                let _ = writeln!(
                    out,
                    "  {} [{id}]: Tier 2 — all {} mixin site(s) lifted onto standard event hooks (bounded JVM shim).",
                    jar.name,
                    lifted_bindings(&scan.verdict).len()
                );
            }
            ModsDirClass::RequiresFallback => {
                let scan = jar.scan.as_ref().expect("scanned jar has report");
                let unliftable = unliftable_sites(&scan.verdict);
                let _ = writeln!(
                    out,
                    "  {} [{id}]: requires fallback — {} mixin site(s) stay at Tier 3:",
                    jar.name,
                    unliftable.len()
                );
                for (site, reason) in unliftable {
                    let _ = writeln!(out, "      {} ({})", describe_site(site), reason);
                }
            }
        }
    }
    let _ = writeln!(
        out,
        "Culinary discoveries: {} cooking, {} cutting, {} loot modifier(s), {} tag file(s), {} food spec(s).",
        culinary.cooking_recipes,
        culinary.cutting_recipes,
        culinary.loot_modifiers,
        culinary.tag_files,
        culinary.food_specs,
    );
    let fallback = mods_dir_count(jars, ModsDirClass::RequiresFallback);
    if fallback == 0 {
        let _ = write!(
            out,
            "Result: clean — every JAR runs natively or on a bounded Tier 2 shim; no fallback needed."
        );
    } else {
        let _ = write!(
            out,
            "Result: requires fallback — {fallback} JAR(s) have unliftable mixin sites and stay at Tier 3 (quarantine or NeoForge fallback)."
        );
    }
    out
}

/// Per-JAR `jars[]` entry of the `mc-spine-mods-dir-scan/1` schema: file
/// name, inferred mod id, class, then — for scanned JARs — the same
/// `lifted_hooks`/`unliftable` site detail `--scan-jar` emits plus the
/// per-JAR culinary counters.
fn mods_dir_jar_json(out: &mut String, jar: &ModsDirJar, last: bool) {
    use std::fmt::Write;
    let _ = writeln!(out, "    {{");
    json_field_str(out, 3, "jar", &jar.name, true);
    json_field_opt_str(out, 3, "mod_id", jar.mod_id.as_deref(), true);
    json_field_str(out, 3, "class", jar.class.as_str(), true);
    match &jar.scan {
        None => json_field_bool(out, 3, "scanned", false, false),
        Some(scan) => {
            json_field_bool(out, 3, "scanned", true, true);
            json_field_u64(
                out,
                3,
                "mixin_configs_found",
                scan.mixin_configs_found as u64,
                true,
            );
            json_field_u64(out, 3, "classes_scanned", scan.classes_scanned as u64, true);
            json_field_u64(
                out,
                3,
                "skipped_client_mixins",
                scan.skipped_client_mixins as u64,
                true,
            );
            json_field_u64(out, 3, "sites_found", scan.sites.len() as u64, true);

            let lifted = lifted_bindings(&scan.verdict);
            if lifted.is_empty() {
                let _ = writeln!(out, "      \"lifted_hooks\": [],");
            } else {
                let _ = writeln!(out, "      \"lifted_hooks\": [");
                for (i, binding) in lifted.iter().enumerate() {
                    let last = i + 1 == lifted.len();
                    let _ = writeln!(out, "        {{");
                    json_field_str(out, 5, "hook", binding.hook.as_str(), true);
                    json_site_fields(out, 5, &binding.site);
                    let _ = writeln!(out, "          \"cancellable\": {}", binding.cancellable);
                    let _ = writeln!(out, "        }}{}", if last { "" } else { "," });
                }
                let _ = writeln!(out, "      ],");
            }

            let unliftable = unliftable_sites(&scan.verdict);
            if unliftable.is_empty() {
                let _ = writeln!(out, "      \"unliftable\": [],");
            } else {
                let _ = writeln!(out, "      \"unliftable\": [");
                for (i, (site, reason)) in unliftable.iter().enumerate() {
                    let last = i + 1 == unliftable.len();
                    let _ = writeln!(out, "        {{");
                    json_site_fields(out, 5, site);
                    json_field_str(out, 5, "reason", reason.as_str(), false);
                    let _ = writeln!(out, "        }}{}", if last { "" } else { "," });
                }
                let _ = writeln!(out, "      ],");
            }

            let c = jar.culinary.as_ref().expect("scanned jar has stats");
            let _ = writeln!(out, "      \"culinary\": {{");
            json_field_u64(out, 4, "jar_members", c.jar_members as u64, true);
            json_field_u64(out, 4, "data_files", c.data_files as u64, true);
            json_field_u64(out, 4, "cooking_recipes", c.cooking_recipes as u64, true);
            json_field_u64(out, 4, "cutting_recipes", c.cutting_recipes as u64, true);
            json_field_u64(out, 4, "skipped_recipes", c.skipped_recipes as u64, true);
            json_field_u64(out, 4, "tag_files", c.tag_files as u64, true);
            json_field_u64(out, 4, "tag_values", c.tag_values as u64, true);
            json_field_u64(out, 4, "loot_modifiers", c.loot_modifiers as u64, true);
            json_field_u64(out, 4, "food_specs", c.food_specs as u64, false);
            let _ = writeln!(out, "      }}");
        }
    }
    let _ = writeln!(out, "    }}{}", if last { "" } else { "," });
}

/// Deterministic `mc-spine-mods-dir-scan/1` JSON form of a directory
/// scan: totals by class, merged culinary counters, then one entry per
/// JAR in the same filename order as the human report.
fn mods_dir_json(
    dir: &Path,
    jars: &[ModsDirJar],
    non_jar: usize,
    culinary: &ModsDirCulinaryTotals,
) -> String {
    use std::fmt::Write;
    let mut out = String::with_capacity(1024 + jars.len() * 384);
    out.push_str("{\n");
    json_field_str(&mut out, 1, "schema", "mc-spine-mods-dir-scan/1", true);
    json_field_str(&mut out, 1, "dir", &dir.display().to_string(), true);
    json_field_u64(&mut out, 1, "jars_found", jars.len() as u64, true);
    json_field_u64(&mut out, 1, "non_jar_entries", non_jar as u64, true);

    let _ = writeln!(out, "  \"totals\": {{");
    json_field_u64(
        &mut out,
        2,
        "superseded_perf_mod",
        mods_dir_count(jars, ModsDirClass::SupersededPerfMod) as u64,
        true,
    );
    json_field_u64(
        &mut out,
        2,
        "native_culinary",
        mods_dir_count(jars, ModsDirClass::NativeCulinary) as u64,
        true,
    );
    json_field_u64(
        &mut out,
        2,
        "native_data_or_clean",
        mods_dir_count(jars, ModsDirClass::NativeDataOrClean) as u64,
        true,
    );
    json_field_u64(
        &mut out,
        2,
        "lifted_tier2",
        mods_dir_count(jars, ModsDirClass::LiftedTier2) as u64,
        true,
    );
    json_field_u64(
        &mut out,
        2,
        "requires_fallback",
        mods_dir_count(jars, ModsDirClass::RequiresFallback) as u64,
        false,
    );
    let _ = writeln!(out, "  }},");

    let _ = writeln!(out, "  \"culinary\": {{");
    json_field_u64(&mut out, 2, "jar_members", culinary.jar_members, true);
    json_field_u64(&mut out, 2, "data_files", culinary.data_files, true);
    json_field_u64(&mut out, 2, "cooking_recipes", culinary.cooking_recipes, true);
    json_field_u64(&mut out, 2, "cutting_recipes", culinary.cutting_recipes, true);
    json_field_u64(&mut out, 2, "skipped_recipes", culinary.skipped_recipes, true);
    json_field_u64(&mut out, 2, "tag_files", culinary.tag_files, true);
    json_field_u64(&mut out, 2, "tag_values", culinary.tag_values, true);
    json_field_u64(&mut out, 2, "loot_modifiers", culinary.loot_modifiers, true);
    json_field_u64(&mut out, 2, "food_specs", culinary.food_specs, false);
    let _ = writeln!(out, "  }},");

    json_field_str(
        &mut out,
        1,
        "verdict",
        if mods_dir_count(jars, ModsDirClass::RequiresFallback) == 0 {
            "clean"
        } else {
            "requires_fallback"
        },
        true,
    );

    if jars.is_empty() {
        let _ = writeln!(out, "  \"jars\": []");
    } else {
        let _ = writeln!(out, "  \"jars\": [");
        for (i, jar) in jars.iter().enumerate() {
            mods_dir_jar_json(&mut out, jar, i + 1 == jars.len());
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

fn run_scan_mods_dir(cli: &ModsDirCli) -> ExitCode {
    let (jars, non_jar, culinary) = match scan_mods_dir(cli) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("mc-spine-mod-check: {e}");
            return ExitCode::FAILURE;
        }
    };
    if cli.json {
        print!("{}", mods_dir_json(&cli.dir, &jars, non_jar, &culinary));
    } else {
        println!(
            "{}",
            format_mods_dir_human(&cli.dir, &jars, non_jar, &culinary)
        );
    }
    if mods_dir_count(&jars, ModsDirClass::RequiresFallback) == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(2)
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
        Ok(Action::ScanModsDir(cli)) => run_scan_mods_dir(&cli),
        Err(msg) => {
            eprintln!("mc-spine-mod-check: {msg}");
            eprintln!("Try 'mc-spine-mod-check --help' for usage.");
            ExitCode::from(2)
        }
    }
}
