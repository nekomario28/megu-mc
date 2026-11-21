// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later

//! Modpack pre-flight analyzer for the two manifest formats real players and
//! server operators actually install: Modrinth `.mrpack`
//! (`modrinth.index.json`, `formatVersion` 1) and CurseForge
//! (`manifest.json`, `manifestType` `minecraftModpack`, `manifestVersion` 1).
//!
//! The analyzer inspects the manifest *before* anything is downloaded,
//! extracted, or launched and answers three operator questions in plain
//! language:
//! 1. Does the pack's Minecraft version match this server's target
//!    ([`McVersion::V1_21_1`] or [`McVersion::V26_3`])?
//! 2. Which redundant performance mods (`sodium`, `lithium`, `modernfix`,
//!    `distanthorizons`, `ferritecore`, …) will be skipped automatically
//!    because [`BUILTIN_SUPERSEDED_PERF_MODS`] already covers them?
//! 3. Which entries run natively (flagship adapter or declarative profile /
//!    pure datapack), which are client-only, and which are still unknown
//!    Java JARs needing a ShimCheck bytecode inspection?
//!
//! Both parsers are fail-closed: malformed JSON, wrong
//! `formatVersion`/`manifestType`/`game`, and path-traversal entries
//! (`..`, absolute paths, Windows drive letters, backslash separators, NUL
//! bytes) are hard errors, never silent skips.
//!
//! Clean-room note: matching file names and mod ids against
//! [`BUILTIN_SUPERSEDED_PERF_MODS`] (or flagship/profile sets) is strictly
//! factual compatibility matching — it only decides whether a user-supplied
//! JAR is loaded or skipped. No code, assets, or data from any third-party
//! mod is copied, linked, bundled, or derived from; every built-in
//! optimization is an original clean-room Rust implementation.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use crate::data_loader::{parse_json, DataLoadError, JsonValue};
use crate::fallback::BUILTIN_SUPERSEDED_PERF_MODS;
use crate::version_compat::{check_mod_mc_version, McVersion, VersionCompatVerdict};

/// Loader ids recognized inside Modrinth `dependencies` (everything besides
/// the required `minecraft` pin).
const MODRINTH_LOADER_DEPENDENCIES: &[&str] = &[
    "neoforge",
    "forge",
    "fabric-loader",
    "quilt-loader",
    "fabric-api",
];

/// Which launcher ecosystem produced the manifest.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModpackFormat {
    /// Modrinth `.mrpack` / `modrinth.index.json`.
    Modrinth,
    /// CurseForge `manifest.json`.
    CurseForge,
}

/// Error produced by [`ModpackManifest::from_modrinth_json`] and
/// [`ModpackManifest::from_curseforge_json`]. Fail-closed: any malformed,
/// unsupported, or unsafe input yields this error rather than a partial
/// manifest.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModpackError(String);

impl ModpackError {
    pub fn reason(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ModpackError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ModpackError {}

fn err(msg: impl Into<String>) -> ModpackError {
    ModpackError(msg.into())
}

fn json_err(e: DataLoadError) -> ModpackError {
    let msg = match e {
        DataLoadError::Io(m) | DataLoadError::Json(m) | DataLoadError::Zip(m) => m,
    };
    ModpackError(msg)
}

/// Environment-side support flag from a Modrinth `env` entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EnvSupport {
    /// The file is installed on that side.
    Required,
    /// The file may be installed on that side.
    Optional,
    /// The file must not be installed on that side (e.g. a shader UI or
    /// minimap mod on a dedicated server).
    Unsupported,
}

fn parse_env_support(value: Option<&JsonValue>, ctx: &str) -> Result<EnvSupport, ModpackError> {
    let Some(v) = value else {
        return Ok(EnvSupport::Required);
    };
    match v.as_str() {
        Some("required") => Ok(EnvSupport::Required),
        Some("optional") => Ok(EnvSupport::Optional),
        Some("unsupported") => Ok(EnvSupport::Unsupported),
        Some(other) => Err(err(format!(
            "{ctx}: unknown env value '{other}' (expected required/optional/unsupported)"
        ))),
        None => Err(err(format!("{ctx}: env value must be a string"))),
    }
}

/// One declared modloader dependency (Modrinth `dependencies` or a
/// CurseForge `minecraft.modLoaders[]` entry).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModpackLoader {
    /// Normalized loader id (`neoforge`, `forge`, `fabric-loader`,
    /// `quilt-loader`, `fabric-api`).
    pub id: String,
    /// Declared loader version (`21.1.93`, `0.16.9`, …).
    pub version: String,
    /// CurseForge `primary` flag; `true` for every Modrinth loader
    /// dependency (the format marks no primary).
    pub primary: bool,
}

/// One file entry inside a modpack manifest.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModpackFile {
    /// Manifest-relative path (`mods/create-1.21.1-6.0.6.jar`). For
    /// CurseForge entries without a `fileName` this is a synthesized
    /// descriptive path under `mods/`.
    pub path: String,
    /// Best-effort normalized mod id inferred from the JAR file name (or
    /// the CurseForge `slug`), with loader and version suffixes stripped:
    /// `sodium-neoforge-0.6.13+mc1.21.1.jar` -> `sodium`. `None` when no
    /// usable id could be inferred — such JARs always classify
    /// [`ModpackEntryClass::NeedsInspection`] (fail closed).
    pub candidate_mod_id: Option<String>,
    /// Client-side support (Modrinth `env.client`; [`EnvSupport::Required`]
    /// for CurseForge entries, which carry no env flags).
    pub client_support: EnvSupport,
    /// Server-side support (Modrinth `env.server`; [`EnvSupport::Required`]
    /// for CurseForge entries). [`EnvSupport::Unsupported`] marks
    /// client-only mods a dedicated server never installs.
    pub server_support: EnvSupport,
    /// CurseForge `required` flag (`true` for every Modrinth entry).
    pub required: bool,
    /// Modrinth `fileSize`, when present.
    pub file_size: Option<u64>,
    /// Modrinth `hashes.sha1`, when present.
    pub sha1: Option<String>,
    /// Modrinth `hashes.sha512`, when present.
    pub sha512: Option<String>,
    /// CurseForge `projectID`, when present.
    pub project_id: Option<u64>,
    /// CurseForge `fileID`, when present.
    pub file_id: Option<u64>,
}

/// A parsed modpack manifest ready for [`analyze_modpack`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModpackManifest {
    pub format: ModpackFormat,
    /// Pack display name (`name`).
    pub name: String,
    /// Pack version (`versionId` on Modrinth, `version` on CurseForge).
    pub version: String,
    /// Declared Minecraft version/range (`dependencies.minecraft` or
    /// `minecraft.version`). Kept as the declared string so
    /// [`check_mod_mc_version`] judges it against the server target.
    pub mc_version: String,
    /// Modloader dependencies declared by the pack.
    pub loaders: Vec<ModpackLoader>,
    /// File entries, in manifest order.
    pub files: Vec<ModpackFile>,
}

/// Validate a manifest member path fail-closed: rejects `..` traversal,
/// absolute paths, Windows drive letters, backslash separators, empty
/// segments, and NUL bytes — everything a hostile or corrupt manifest could
/// use to escape the extraction root.
fn validate_member_path(path: &str, ctx: &str) -> Result<(), ModpackError> {
    if path.is_empty() {
        return Err(err(format!("{ctx}: empty path")));
    }
    if path.contains('\0') {
        return Err(err(format!("{ctx}: NUL byte in path '{path:?}'")));
    }
    if path.contains('\\') {
        return Err(err(format!(
            "{ctx}: backslash separator not allowed in path '{path}'"
        )));
    }
    if path.starts_with('/') {
        return Err(err(format!("{ctx}: absolute path '{path}'")));
    }
    let b = path.as_bytes();
    if b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':' {
        return Err(err(format!("{ctx}: Windows drive-letter path '{path}'")));
    }
    for segment in path.split('/') {
        match segment {
            "" => return Err(err(format!("{ctx}: empty path segment in '{path}'"))),
            "." => return Err(err(format!("{ctx}: '.' path segment in '{path}'"))),
            ".." => return Err(err(format!("{ctx}: '..' traversal segment in '{path}'"))),
            _ => {}
        }
    }
    Ok(())
}

/// Whether a filename token is a version segment worth stripping:
/// pure digits/dots (`1.21.1`, `6.0.6`, `19.21.0.247`), prefixed versions
/// (`mc1.21.1`, `v2`, `build42`, `rc1`, `b3`), or a lone pre-release marker
/// (`b`, `beta`, `alpha`, `rc`, `release`) wedged inside a version run such
/// as `2.3.2-b-1.21.1`.
fn is_versionish_token(tok: &str) -> bool {
    if tok.is_empty() {
        return false;
    }
    if tok.bytes().all(|b| b.is_ascii_digit() || b == b'.')
        && tok.bytes().any(|b| b.is_ascii_digit())
    {
        return true;
    }
    for prefix in [
        "snapshot", "release", "build", "beta", "alpha", "rev", "pre", "mc", "rc", "v", "b", "r",
    ] {
        if let Some(rest) = tok.strip_prefix(prefix) {
            if !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit() || b == b'.') {
                return true;
            }
        }
    }
    matches!(
        tok,
        "b" | "beta" | "alpha" | "rc" | "pre" | "preview" | "release" | "final" | "snapshot"
    )
}

/// Whether a filename token is a loader/edition suffix worth stripping when
/// it trails a JAR name (`-neoforge`, `-fabric`, `-forge`, `-quilt`,
/// `-universal`, `-all`, …). Kept deliberately narrow: under-stripping only
/// degrades an entry to [`ModpackEntryClass::NeedsInspection`] (fail
/// closed), while over-stripping could produce a wrong mod id.
fn is_loaderish_token(tok: &str) -> bool {
    matches!(
        tok,
        "fabric"
            | "forge"
            | "neoforge"
            | "quilt"
            | "liteloader"
            | "rift"
            | "common"
            | "universal"
            | "multiloader"
            | "all"
    )
}

/// Case-insensitive `.jar` suffix check shared by id inference and the
/// bytecode/data split — an uppercase `MOD.JAR` is still a Java JAR and must
/// never slip through as a "data file" (fail closed).
fn is_jar_name(name: &str) -> bool {
    name.len() > 4 && name[name.len() - 4..].eq_ignore_ascii_case(".jar")
}

/// Infer a normalized candidate mod id from a JAR path's basename:
/// lowercase, `_`/`+` folded to `-`, trailing version and loader tokens
/// stripped.
///
/// `mods/sodium-fabric-0.6.13+mc1.21.1.jar` -> `sodium`,
/// `mods/ferritecore-7.0.2-neoforge.jar` -> `ferritecore`,
/// `mods/DistantHorizons-2.3.2-b-1.21.1-fabric-neoforge.jar` ->
/// `distanthorizons`, `mods/fabric-api-0.102.0+1.21.1.jar` -> `fabric-api`.
///
/// Returns `None` for non-`.jar` paths or stems that normalize to nothing.
/// The `.jar` suffix is matched case-insensitively (see [`is_jar_name`]).
pub fn infer_candidate_mod_id(path: &str) -> Option<String> {
    let name = path.rsplit('/').next().unwrap_or(path);
    if !is_jar_name(name) {
        return None;
    }
    let stem = &name[..name.len() - 4];
    let normalized = stem.to_ascii_lowercase().replace(['_', '+'], "-");
    let mut tokens: Vec<&str> = normalized.split('-').filter(|t| !t.is_empty()).collect();
    while let Some(last) = tokens.last() {
        if is_versionish_token(last) || is_loaderish_token(last) {
            tokens.pop();
        } else {
            break;
        }
    }
    if tokens.is_empty() {
        None
    } else {
        Some(tokens.join("-"))
    }
}

/// Comparison key for mod ids: case-folded with `-`/`_` removed, so
/// `embeddium_extra`, `Embeddium-Extra`, and `embeddiumextra` all match the
/// same catalog entry and filename-normalized ids compare equal to
/// underscore-style mod ids.
fn normalized_mod_key(id: &str) -> String {
    id.chars()
        .filter(|c| !matches!(c, '-' | '_'))
        .flat_map(char::to_lowercase)
        .collect()
}

fn req_obj<'a>(
    obj: &'a BTreeMap<String, JsonValue>,
    key: &str,
    ctx: &str,
) -> Result<&'a BTreeMap<String, JsonValue>, ModpackError> {
    obj.get(key)
        .and_then(JsonValue::as_object)
        .ok_or_else(|| err(format!("{ctx}: missing object '{key}'")))
}

fn req_str<'a>(
    obj: &'a BTreeMap<String, JsonValue>,
    key: &str,
    ctx: &str,
) -> Result<&'a str, ModpackError> {
    obj.get(key)
        .and_then(JsonValue::as_str)
        .ok_or_else(|| err(format!("{ctx}: missing string '{key}'")))
}

fn req_u64(obj: &BTreeMap<String, JsonValue>, key: &str, ctx: &str) -> Result<u64, ModpackError> {
    obj.get(key)
        .and_then(JsonValue::as_u64)
        .ok_or_else(|| err(format!("{ctx}: missing number '{key}'")))
}

fn req_array<'a>(
    obj: &'a BTreeMap<String, JsonValue>,
    key: &str,
    ctx: &str,
) -> Result<&'a [JsonValue], ModpackError> {
    obj.get(key)
        .and_then(JsonValue::as_array)
        .ok_or_else(|| err(format!("{ctx}: missing array '{key}'")))
}

impl ModpackManifest {
    /// Parse a Modrinth `.mrpack` `modrinth.index.json` document.
    ///
    /// Requires `formatVersion == 1`, `game == "minecraft"`, `name`,
    /// `versionId`, `dependencies.minecraft`, and `files[]`; optional loader
    /// dependencies (`neoforge`, `forge`, `fabric-loader`, `quilt-loader`)
    /// are collected into [`ModpackManifest::loaders`]. Every `files[].path`
    /// is validated against traversal/absolute/drive/NUL injection.
    pub fn from_modrinth_json(text: &str) -> Result<Self, ModpackError> {
        let ctx = "modrinth.index.json";
        let root = parse_json(text.as_bytes()).map_err(json_err)?;
        let obj = root
            .as_object()
            .ok_or_else(|| err(format!("{ctx}: root must be an object")))?;

        let format_version = req_u64(obj, "formatVersion", ctx)?;
        if format_version != 1 {
            return Err(err(format!(
                "{ctx}: unsupported formatVersion {format_version} (expected 1)"
            )));
        }
        match req_str(obj, "game", ctx)? {
            "minecraft" => {}
            other => {
                return Err(err(format!(
                    "{ctx}: unsupported game '{other}' (expected 'minecraft')"
                )))
            }
        }
        let name = req_str(obj, "name", ctx)?.to_string();
        let version = req_str(obj, "versionId", ctx)?.to_string();

        let deps = req_obj(obj, "dependencies", ctx)?;
        let mc_version = req_str(deps, "minecraft", ctx)?.to_string();
        let mut loaders = Vec::new();
        for (dep_id, dep_val) in deps {
            if dep_id == "minecraft" || !MODRINTH_LOADER_DEPENDENCIES.contains(&dep_id.as_str()) {
                continue;
            }
            let Some(dep_version) = dep_val.as_str() else {
                return Err(err(format!(
                    "{ctx}: dependencies.{dep_id} must be a version string"
                )));
            };
            loaders.push(ModpackLoader {
                id: dep_id.clone(),
                version: dep_version.to_string(),
                primary: true,
            });
        }

        let mut files = Vec::new();
        for (i, entry) in req_array(obj, "files", ctx)?.iter().enumerate() {
            let fctx = format!("{ctx}: files[{i}]");
            let fobj = entry
                .as_object()
                .ok_or_else(|| err(format!("{fctx}: must be an object")))?;
            let path = req_str(fobj, "path", &fctx)?.to_string();
            validate_member_path(&path, &fctx)?;

            let (client_support, server_support) = match fobj.get("env") {
                Some(JsonValue::Object(env_obj)) => (
                    parse_env_support(env_obj.get("client"), &fctx)?,
                    parse_env_support(env_obj.get("server"), &fctx)?,
                ),
                Some(_) => return Err(err(format!("{fctx}: env must be an object"))),
                None => (EnvSupport::Required, EnvSupport::Required),
            };

            let hashes = fobj.get("hashes").and_then(JsonValue::as_object);
            files.push(ModpackFile {
                candidate_mod_id: infer_candidate_mod_id(&path),
                client_support,
                server_support,
                required: true,
                file_size: fobj.get("fileSize").and_then(JsonValue::as_u64),
                sha1: hashes
                    .and_then(|h| h.get("sha1"))
                    .and_then(JsonValue::as_str)
                    .map(str::to_string),
                sha512: hashes
                    .and_then(|h| h.get("sha512"))
                    .and_then(JsonValue::as_str)
                    .map(str::to_string),
                project_id: None,
                file_id: None,
                path,
            });
        }

        Ok(Self {
            format: ModpackFormat::Modrinth,
            name,
            version,
            mc_version,
            loaders,
            files,
        })
    }

    /// Parse a CurseForge `manifest.json` document.
    ///
    /// Requires `manifestType == "minecraftModpack"`, `manifestVersion == 1`,
    /// `name`, `version`, `minecraft.version`, and `files[]` with numeric
    /// `projectID`/`fileID`. Optional per-file `slug`/`fileName` (carried by
    /// exported manifests that keep resolved names) drive candidate mod id
    /// inference; entries without either fail closed to
    /// [`ModpackEntryClass::NeedsInspection`].
    pub fn from_curseforge_json(text: &str) -> Result<Self, ModpackError> {
        let ctx = "manifest.json";
        let root = parse_json(text.as_bytes()).map_err(json_err)?;
        let obj = root
            .as_object()
            .ok_or_else(|| err(format!("{ctx}: root must be an object")))?;

        match req_str(obj, "manifestType", ctx)? {
            "minecraftModpack" => {}
            other => {
                return Err(err(format!(
                    "{ctx}: unsupported manifestType '{other}' (expected 'minecraftModpack')"
                )))
            }
        }
        let manifest_version = req_u64(obj, "manifestVersion", ctx)?;
        if manifest_version != 1 {
            return Err(err(format!(
                "{ctx}: unsupported manifestVersion {manifest_version} (expected 1)"
            )));
        }
        let name = req_str(obj, "name", ctx)?.to_string();
        let version = req_str(obj, "version", ctx)?.to_string();

        let mc = req_obj(obj, "minecraft", ctx)?;
        let mc_version = req_str(mc, "version", ctx)?.to_string();
        let mut loaders = Vec::new();
        if let Some(list) = mc.get("modLoaders").and_then(JsonValue::as_array) {
            for (i, entry) in list.iter().enumerate() {
                let lctx = format!("{ctx}: minecraft.modLoaders[{i}]");
                let lobj = entry
                    .as_object()
                    .ok_or_else(|| err(format!("{lctx}: must be an object")))?;
                let id_field = req_str(lobj, "id", &lctx)?;
                // Ids look like "neoforge-21.1.93": loader + '-' + version.
                let (id, ver) = id_field
                    .split_once('-')
                    .map(|(l, v)| (l.to_string(), v.to_string()))
                    .unwrap_or_else(|| (id_field.to_string(), String::new()));
                loaders.push(ModpackLoader {
                    id,
                    version: ver,
                    primary: lobj
                        .get("primary")
                        .and_then(JsonValue::as_bool)
                        .unwrap_or(false),
                });
            }
        }

        let mut files = Vec::new();
        for (i, entry) in req_array(obj, "files", ctx)?.iter().enumerate() {
            let fctx = format!("{ctx}: files[{i}]");
            let fobj = entry
                .as_object()
                .ok_or_else(|| err(format!("{fctx}: must be an object")))?;
            let project_id = req_u64(fobj, "projectID", &fctx)?;
            let file_id = req_u64(fobj, "fileID", &fctx)?;
            let required = fobj
                .get("required")
                .and_then(JsonValue::as_bool)
                .unwrap_or(true);

            let slug = fobj.get("slug").and_then(JsonValue::as_str);
            let file_name = fobj.get("fileName").and_then(JsonValue::as_str);
            for (kind, value) in [("slug", slug), ("fileName", file_name)] {
                if let Some(v) = value {
                    if v.is_empty()
                        || v.contains('/')
                        || v.contains('\\')
                        || v.contains("..")
                        || v.contains('\0')
                    {
                        return Err(err(format!(
                            "{fctx}: unsafe {kind} '{v}' (must be a bare name)"
                        )));
                    }
                }
            }
            let (path, candidate_mod_id) = match (file_name, slug) {
                (Some(file_name), _) => (
                    format!("mods/{file_name}"),
                    slug.map(normalized_slug_id)
                        .or_else(|| infer_candidate_mod_id(file_name)),
                ),
                (None, Some(slug)) => (format!("mods/{slug}.jar"), Some(normalized_slug_id(slug))),
                (None, None) => (
                    format!("mods/curseforge-project-{project_id}-file-{file_id}.jar"),
                    None,
                ),
            };
            files.push(ModpackFile {
                path,
                candidate_mod_id,
                client_support: EnvSupport::Required,
                server_support: EnvSupport::Required,
                required,
                file_size: None,
                sha1: None,
                sha512: None,
                project_id: Some(project_id),
                file_id: Some(file_id),
            });
        }

        Ok(Self {
            format: ModpackFormat::CurseForge,
            name,
            version,
            mc_version,
            loaders,
            files,
        })
    }
}

/// Normalize a CurseForge slug to the same id space
/// [`infer_candidate_mod_id`] produces (lowercase, `_` -> `-`).
fn normalized_slug_id(slug: &str) -> String {
    slug.to_ascii_lowercase().replace('_', "-")
}

/// Per-entry classification inside a [`ModpackPreflightReport`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModpackEntryClass {
    /// Redundant performance mod already covered by
    /// [`BUILTIN_SUPERSEDED_PERF_MODS`] — always skipped automatically,
    /// never downloaded for the server, never counted against it.
    SupersededPerfMod,
    /// `env.server == "unsupported"`: a client-only mod (shader UI,
    /// minimap, …) a dedicated server never installs.
    ClientOnlySkipped,
    /// Mod id claimed by a flagship dedicated Rust adapter (Tier 0, e.g.
    /// `create`, `aether`).
    NativeFlagship,
    /// Covered by a declarative offload profile, or a pure-data file
    /// (datapack under `datapacks/`, resourcepack, config) carrying no Java
    /// bytecode (Tier 1).
    UniversalNative,
    /// A Java JAR whose bytecode has not been ShimCheck'd yet — *not* a
    /// confirmed Tier 3 fallback, just unknown until inspected.
    NeedsInspection,
}

/// One analyzed manifest entry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModpackEntry {
    /// Manifest-relative path (same as [`ModpackFile::path`]).
    pub path: String,
    /// Inferred mod id, when one could be derived.
    pub candidate_mod_id: Option<String>,
    pub class: ModpackEntryClass,
    /// Plain-language reason for the classification.
    pub note: String,
}

/// Overall pre-flight verdict for the pack.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModpackReadiness {
    /// Every file runs natively on `mc-spine` — nothing to skip, nothing to
    /// inspect.
    ReadyNative,
    /// Ready once redundant perf mods and client-only files are
    /// automatically skipped; everything that remains is native.
    ReadyWithAutoSkips,
    /// Version matches, but >=1 JAR is still unclassified until its
    /// bytecode passes a ShimCheck inspection.
    NeedsJarInspection,
    /// The pack declares a Minecraft version/range incompatible with the
    /// server target — nothing else matters until that is fixed.
    VersionMismatch,
}

/// Full pre-flight report produced by [`analyze_modpack`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModpackPreflightReport {
    pub pack_name: String,
    pub pack_version: String,
    pub format: ModpackFormat,
    /// The pack's declared Minecraft version/range.
    pub mc_version_declared: String,
    /// The Minecraft version the server targets.
    pub target: McVersion,
    /// Verdict from [`check_mod_mc_version`] on the declared range.
    pub version_verdict: VersionCompatVerdict,
    pub entries: Vec<ModpackEntry>,
    pub readiness: ModpackReadiness,
}

impl ModpackPreflightReport {
    /// Entries classified `class`, in manifest order.
    pub fn entries_of_class(&self, class: ModpackEntryClass) -> Vec<&ModpackEntry> {
        self.entries.iter().filter(|e| e.class == class).collect()
    }

    /// Sorted display names (inferred mod id, else file basename) of
    /// redundant perf mods skipped automatically.
    pub fn skipped_perf_mod_names(&self) -> Vec<String> {
        self.sorted_names(ModpackEntryClass::SupersededPerfMod)
    }

    /// Sorted display names of client-only entries skipped on a dedicated
    /// server.
    pub fn client_only_names(&self) -> Vec<String> {
        self.sorted_names(ModpackEntryClass::ClientOnlySkipped)
    }

    /// Sorted display names of entries running natively (flagship +
    /// universal/profile/datapack).
    pub fn native_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .entries
            .iter()
            .filter(|e| {
                matches!(
                    e.class,
                    ModpackEntryClass::NativeFlagship | ModpackEntryClass::UniversalNative
                )
            })
            .map(entry_display_name)
            .collect();
        names.sort();
        names
    }

    /// Sorted display names of uninspected Java JARs.
    pub fn needs_inspection_names(&self) -> Vec<String> {
        self.sorted_names(ModpackEntryClass::NeedsInspection)
    }

    fn sorted_names(&self, class: ModpackEntryClass) -> Vec<String> {
        let mut names: Vec<String> = self
            .entries_of_class(class)
            .into_iter()
            .map(entry_display_name)
            .collect();
        names.sort();
        names
    }

    /// Human-readable, jargon-free summary an operator can paste into a
    /// ticket or read top to bottom.
    pub fn format_human_summary(&self) -> String {
        use fmt::Write;
        let mut out = String::new();
        let format_name = match self.format {
            ModpackFormat::Modrinth => "Modrinth pack",
            ModpackFormat::CurseForge => "CurseForge pack",
        };
        let _ = writeln!(
            out,
            "Pre-flight report for \"{}\" {} ({format_name})",
            self.pack_name, self.pack_version
        );

        match &self.version_verdict {
            VersionCompatVerdict::Compatible => {
                let _ = writeln!(
                    out,
                    "The pack targets Minecraft {}, which matches this server.",
                    self.mc_version_declared
                );
            }
            VersionCompatVerdict::MinorMismatch { .. }
            | VersionCompatVerdict::MajorMismatch { .. } => {
                let _ = writeln!(
                    out,
                    "The pack targets Minecraft {} but this server runs {} — the versions do not match.",
                    self.mc_version_declared, self.target
                );
            }
            VersionCompatVerdict::InvalidRange { declared, .. } => {
                let _ = writeln!(
                    out,
                    "The pack declares an unreadable Minecraft version '{declared}', so it cannot be confirmed against this server ({}).",
                    self.target
                );
            }
        }

        let _ = writeln!(out, "Checked {} pack file(s):", self.entries.len());
        let skipped = self.skipped_perf_mod_names();
        if !skipped.is_empty() {
            let _ = writeln!(
                out,
                "  - {} performance mod(s) skipped automatically — mc-spine already has the same optimizations built in (clean-room): {}",
                skipped.len(),
                skipped.join(", ")
            );
        }
        let client_only = self.client_only_names();
        if !client_only.is_empty() {
            let _ = writeln!(
                out,
                "  - {} client-side mod(s) a dedicated server does not install: {}",
                client_only.len(),
                client_only.join(", ")
            );
        }
        let native = self.native_names();
        if !native.is_empty() {
            let _ = writeln!(
                out,
                "  - {} mod(s) run natively with no Java needed: {}",
                native.len(),
                native.join(", ")
            );
        }
        let inspect = self.needs_inspection_names();
        if !inspect.is_empty() {
            let _ = writeln!(
                out,
                "  - {} Java mod(s) still need a compatibility inspection before launch: {}",
                inspect.len(),
                inspect.join(", ")
            );
        }

        let _ = write!(
            out,
            "Result: {}",
            match self.readiness {
                ModpackReadiness::ReadyNative =>
                    "ready — every file in this pack runs natively on mc-spine.".to_string(),
                ModpackReadiness::ReadyWithAutoSkips =>
                    "ready — redundant and client-only files are skipped automatically; the rest runs natively.".to_string(),
                ModpackReadiness::NeedsJarInspection =>
                    "almost ready — the Java mod(s) listed above still need a compatibility check before launch.".to_string(),
                ModpackReadiness::VersionMismatch =>
                    "not compatible — this pack was built for a different Minecraft version.".to_string(),
            }
        );
        out
    }
}

/// Display label for an entry: the inferred mod id when known, else the
/// file's basename so reports never print a bare path tree.
fn entry_display_name(e: &ModpackEntry) -> String {
    e.candidate_mod_id
        .clone()
        .unwrap_or_else(|| e.path.rsplit('/').next().unwrap_or(&e.path).to_string())
}

/// Analyze a parsed modpack manifest against the server's target Minecraft
/// version, the registered flagship adapter ids, and the declarative
/// offload profile ids.
///
/// Every entry is classified exactly once: client-only files first, then
/// [`BUILTIN_SUPERSEDED_PERF_MODS`] auto-skips, then flagship/profile
/// natives and pure-data files, and finally unknown Java JARs fall to
/// [`ModpackEntryClass::NeedsInspection`]. All mod-id comparisons use the
/// normalized key (case-insensitive, `-`/`_` agnostic) so filename-derived
/// ids match catalog and caller-supplied ids reliably.
pub fn analyze_modpack(
    manifest: &ModpackManifest,
    target: McVersion,
    flagship_mods: &BTreeSet<String>,
    profile_mods: &BTreeSet<String>,
) -> ModpackPreflightReport {
    let version_verdict = check_mod_mc_version(target, &manifest.mc_version);

    let superseded: BTreeSet<String> = BUILTIN_SUPERSEDED_PERF_MODS
        .iter()
        .map(|id| normalized_mod_key(id))
        .collect();
    let flagship: BTreeSet<String> = flagship_mods
        .iter()
        .map(|s| normalized_mod_key(s))
        .collect();
    let profile: BTreeSet<String> = profile_mods.iter().map(|s| normalized_mod_key(s)).collect();

    let mut entries = Vec::with_capacity(manifest.files.len());
    for f in &manifest.files {
        let (class, note) = classify_entry(f, &superseded, &flagship, &profile);
        entries.push(ModpackEntry {
            path: f.path.clone(),
            candidate_mod_id: f.candidate_mod_id.clone(),
            class,
            note,
        });
    }

    let readiness = if !matches!(version_verdict, VersionCompatVerdict::Compatible) {
        ModpackReadiness::VersionMismatch
    } else if entries
        .iter()
        .any(|e| e.class == ModpackEntryClass::NeedsInspection)
    {
        ModpackReadiness::NeedsJarInspection
    } else if entries.iter().any(|e| {
        matches!(
            e.class,
            ModpackEntryClass::SupersededPerfMod | ModpackEntryClass::ClientOnlySkipped
        )
    }) {
        ModpackReadiness::ReadyWithAutoSkips
    } else {
        ModpackReadiness::ReadyNative
    };

    ModpackPreflightReport {
        pack_name: manifest.name.clone(),
        pack_version: manifest.version.clone(),
        format: manifest.format,
        mc_version_declared: manifest.mc_version.clone(),
        target,
        version_verdict,
        entries,
        readiness,
    }
}

fn classify_entry(
    f: &ModpackFile,
    superseded: &BTreeSet<String>,
    flagship: &BTreeSet<String>,
    profile: &BTreeSet<String>,
) -> (ModpackEntryClass, String) {
    if f.server_support == EnvSupport::Unsupported {
        return (
            ModpackEntryClass::ClientOnlySkipped,
            "client-side only; a dedicated server never installs it".to_string(),
        );
    }
    if let Some(id) = f.candidate_mod_id.as_deref() {
        let key = normalized_mod_key(id);
        if superseded.contains(&key) {
            return (
                ModpackEntryClass::SupersededPerfMod,
                "redundant performance mod — mc-spine already ships an equivalent clean-room optimization".to_string(),
            );
        }
        if flagship.contains(&key) {
            return (
                ModpackEntryClass::NativeFlagship,
                "handled by a flagship native Rust adapter (no Java)".to_string(),
            );
        }
        if profile.contains(&key) {
            return (
                ModpackEntryClass::UniversalNative,
                "covered by a declarative offload profile (no Java)".to_string(),
            );
        }
    }
    if f.path.starts_with("datapacks/") {
        return (
            ModpackEntryClass::UniversalNative,
            "datapack — pure data, runs natively".to_string(),
        );
    }
    if !is_jar_name(f.path.rsplit('/').next().unwrap_or(&f.path)) {
        return (
            ModpackEntryClass::UniversalNative,
            "data/resource file — no Java bytecode to adapt".to_string(),
        );
    }
    (
        ModpackEntryClass::NeedsInspection,
        "unrecognized Java mod — its bytecode still needs a compatibility inspection before launch"
            .to_string(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(items: &[&str]) -> BTreeSet<String> {
        items.iter().map(|s| (*s).to_string()).collect()
    }

    /// Real-shaped 1.21.1 NeoForge Modrinth index: Create + Aether content
    /// mods, five redundant perf mods, one client-only mod, one datapack,
    /// and one unknown JAR.
    const MODRINTH_1211: &str = r#"{
        "formatVersion": 1,
        "game": "minecraft",
        "versionId": "1.4.0",
        "name": "Create NeoForge Adventure",
        "summary": "Create, Aether, and a performance stack",
        "files": [
            {
                "path": "mods/create-1.21.1-6.0.6.jar",
                "hashes": {"sha1": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", "sha512": "b"},
                "env": {"client": "required", "server": "required"},
                "downloads": ["https://cdn.modrinth.com/data/abc/create.jar"],
                "fileSize": 16000000
            },
            {
                "path": "mods/aether-1.21.1-1.5.9-neoforge.jar",
                "hashes": {"sha512": "c"},
                "env": {"client": "required", "server": "required"},
                "downloads": ["https://cdn.modrinth.com/data/def/aether.jar"],
                "fileSize": 42000000
            },
            {
                "path": "mods/sodium-neoforge-0.6.13+mc1.21.1.jar",
                "hashes": {"sha512": "d"},
                "env": {"client": "required", "server": "optional"},
                "downloads": ["https://cdn.modrinth.com/data/ghi/sodium.jar"],
                "fileSize": 2000000
            },
            {
                "path": "mods/lithium-neoforge-0.15.0+mc1.21.1.jar",
                "hashes": {"sha512": "e"},
                "env": {"client": "required", "server": "required"},
                "downloads": ["https://cdn.modrinth.com/data/jkl/lithium.jar"],
                "fileSize": 1000000
            },
            {
                "path": "mods/modernfix-neoforge-5.20.2+mc1.21.1.jar",
                "hashes": {"sha512": "f"},
                "env": {"client": "required", "server": "required"},
                "downloads": ["https://cdn.modrinth.com/data/mno/modernfix.jar"],
                "fileSize": 3000000
            },
            {
                "path": "mods/DistantHorizons-2.3.2-b-1.21.1-fabric-neoforge.jar",
                "hashes": {"sha512": "g"},
                "env": {"client": "required", "server": "required"},
                "downloads": ["https://cdn.modrinth.com/data/pqr/dh.jar"],
                "fileSize": 25000000
            },
            {
                "path": "mods/ferritecore-7.0.2-neoforge.jar",
                "hashes": {"sha512": "h"},
                "env": {"client": "required", "server": "required"},
                "downloads": ["https://cdn.modrinth.com/data/stu/ferritecore.jar"],
                "fileSize": 300000
            },
            {
                "path": "mods/iris-neoforge-1.8.0+mc1.21.1.jar",
                "hashes": {"sha512": "i"},
                "env": {"client": "required", "server": "unsupported"},
                "downloads": ["https://cdn.modrinth.com/data/vwx/iris.jar"],
                "fileSize": 2500000
            },
            {
                "path": "datapacks/terralith-v2.5.8.zip",
                "hashes": {"sha512": "j"},
                "env": {"client": "required", "server": "required"},
                "downloads": ["https://cdn.modrinth.com/data/yz1/terralith.zip"],
                "fileSize": 800000
            },
            {
                "path": "mods/mysterious-factory-3.2.1.jar",
                "hashes": {"sha512": "k"},
                "env": {"client": "required", "server": "required"},
                "downloads": ["https://cdn.modrinth.com/data/234/mystery.jar"],
                "fileSize": 900000
            }
        ],
        "dependencies": {"minecraft": "1.21.1", "neoforge": "21.1.93"}
    }"#;

    #[test]
    fn modrinth_manifest_parses_real_shape() {
        let m = ModpackManifest::from_modrinth_json(MODRINTH_1211).unwrap();
        assert_eq!(m.format, ModpackFormat::Modrinth);
        assert_eq!(m.name, "Create NeoForge Adventure");
        assert_eq!(m.version, "1.4.0");
        assert_eq!(m.mc_version, "1.21.1");
        assert_eq!(
            m.loaders,
            vec![ModpackLoader {
                id: "neoforge".into(),
                version: "21.1.93".into(),
                primary: true,
            }]
        );
        assert_eq!(m.files.len(), 10);
        assert_eq!(
            m.files[0].candidate_mod_id.as_deref(),
            Some("create"),
            "create-1.21.1-6.0.6.jar should infer 'create'"
        );
        assert_eq!(
            m.files[2].candidate_mod_id.as_deref(),
            Some("sodium"),
            "sodium-neoforge-0.6.13+mc1.21.1.jar should infer 'sodium'"
        );
        assert_eq!(
            m.files[5].candidate_mod_id.as_deref(),
            Some("distanthorizons"),
            "DistantHorizons-2.3.2-b-1.21.1-fabric-neoforge.jar should infer 'distanthorizons'"
        );
        assert_eq!(m.files[7].server_support, EnvSupport::Unsupported);
        assert_eq!(m.files[0].file_size, Some(16000000));
        assert_eq!(m.files[0].sha512.as_deref(), Some("b"));
    }

    #[test]
    fn modrinth_preflight_classifies_every_entry() {
        let m = ModpackManifest::from_modrinth_json(MODRINTH_1211).unwrap();
        let report = analyze_modpack(
            &m,
            McVersion::V1_21_1,
            &set(&["create", "aether"]),
            &set(&[]),
        );
        assert_eq!(report.version_verdict, VersionCompatVerdict::Compatible);
        assert_eq!(report.readiness, ModpackReadiness::NeedsJarInspection);
        assert_eq!(report.entries.len(), 10);

        let class_of = |path: &str| {
            report
                .entries
                .iter()
                .find(|e| e.path == path)
                .unwrap()
                .class
        };
        assert_eq!(
            class_of("mods/create-1.21.1-6.0.6.jar"),
            ModpackEntryClass::NativeFlagship
        );
        assert_eq!(
            class_of("mods/aether-1.21.1-1.5.9-neoforge.jar"),
            ModpackEntryClass::NativeFlagship
        );
        for perf in [
            "mods/sodium-neoforge-0.6.13+mc1.21.1.jar",
            "mods/lithium-neoforge-0.15.0+mc1.21.1.jar",
            "mods/modernfix-neoforge-5.20.2+mc1.21.1.jar",
            "mods/DistantHorizons-2.3.2-b-1.21.1-fabric-neoforge.jar",
            "mods/ferritecore-7.0.2-neoforge.jar",
        ] {
            assert_eq!(
                class_of(perf),
                ModpackEntryClass::SupersededPerfMod,
                "{perf} must be auto-skipped"
            );
        }
        assert_eq!(
            class_of("mods/iris-neoforge-1.8.0+mc1.21.1.jar"),
            ModpackEntryClass::ClientOnlySkipped
        );
        assert_eq!(
            class_of("datapacks/terralith-v2.5.8.zip"),
            ModpackEntryClass::UniversalNative
        );
        assert_eq!(
            class_of("mods/mysterious-factory-3.2.1.jar"),
            ModpackEntryClass::NeedsInspection
        );

        assert_eq!(
            report.skipped_perf_mod_names(),
            vec![
                "distanthorizons".to_string(),
                "ferritecore".to_string(),
                "lithium".to_string(),
                "modernfix".to_string(),
                "sodium".to_string(),
            ]
        );
        assert_eq!(report.client_only_names(), vec!["iris".to_string()]);
        assert_eq!(
            report.needs_inspection_names(),
            vec!["mysterious-factory".to_string()]
        );
    }

    #[test]
    fn modrinth_fully_native_pack_reports_ready_with_auto_skips() {
        let text = r#"{
            "formatVersion": 1,
            "game": "minecraft",
            "versionId": "2.0",
            "name": "Native Only",
            "files": [
                {"path": "mods/create-1.21.1-6.0.6.jar", "hashes": {"sha512": "x"},
                 "env": {"client": "required", "server": "required"},
                 "downloads": [], "fileSize": 1},
                {"path": "mods/sodium-neoforge-0.6.13+mc1.21.1.jar", "hashes": {"sha512": "y"},
                 "env": {"client": "required", "server": "required"},
                 "downloads": [], "fileSize": 1}
            ],
            "dependencies": {"minecraft": "1.21.1", "neoforge": "21.1.93"}
        }"#;
        let m = ModpackManifest::from_modrinth_json(text).unwrap();
        let report = analyze_modpack(&m, McVersion::V1_21_1, &set(&["create"]), &set(&[]));
        assert_eq!(report.readiness, ModpackReadiness::ReadyWithAutoSkips);
    }

    #[test]
    fn modrinth_all_native_pack_reports_ready_native() {
        let text = r#"{
            "formatVersion": 1,
            "game": "minecraft",
            "versionId": "3.0",
            "name": "Pure Native",
            "files": [
                {"path": "mods/create-1.21.1-6.0.6.jar", "hashes": {"sha512": "x"},
                 "env": {"client": "required", "server": "required"},
                 "downloads": [], "fileSize": 1},
                {"path": "datapacks/custom-recipes.zip", "hashes": {"sha512": "y"},
                 "env": {"client": "required", "server": "required"},
                 "downloads": [], "fileSize": 1}
            ],
            "dependencies": {"minecraft": "1.21.1"}
        }"#;
        let m = ModpackManifest::from_modrinth_json(text).unwrap();
        let report = analyze_modpack(&m, McVersion::V1_21_1, &set(&["create"]), &set(&[]));
        assert_eq!(report.readiness, ModpackReadiness::ReadyNative);
    }

    #[test]
    fn modrinth_26_3_pack_matches_26_3_target() {
        let text = MODRINTH_1211
            .replace("\"minecraft\": \"1.21.1\"", "\"minecraft\": \"26.3\"")
            .replace("1.21.1-6.0.6.jar", "26.3-6.0.6.jar");
        let m = ModpackManifest::from_modrinth_json(&text).unwrap();
        assert_eq!(m.mc_version, "26.3");
        let report = analyze_modpack(&m, McVersion::V26_3, &set(&["create", "aether"]), &set(&[]));
        assert_eq!(report.version_verdict, VersionCompatVerdict::Compatible);
        // Same entries: still has one unknown JAR -> NeedsJarInspection.
        assert_eq!(report.readiness, ModpackReadiness::NeedsJarInspection);
    }

    #[test]
    fn modrinth_1211_pack_mismatches_26_3_target() {
        let m = ModpackManifest::from_modrinth_json(MODRINTH_1211).unwrap();
        let report = analyze_modpack(&m, McVersion::V26_3, &set(&["create", "aether"]), &set(&[]));
        assert!(matches!(
            report.version_verdict,
            VersionCompatVerdict::MajorMismatch { .. }
        ));
        assert_eq!(report.readiness, ModpackReadiness::VersionMismatch);
    }

    #[test]
    fn profile_and_datapack_entries_classify_universal_native() {
        let text = r#"{
            "formatVersion": 1,
            "game": "minecraft",
            "versionId": "1.0",
            "name": "Profile Pack",
            "files": [
                {"path": "mods/jei-1.21.1-neoforge-19.21.0.247.jar", "hashes": {"sha512": "x"},
                 "env": {"client": "required", "server": "required"},
                 "downloads": [], "fileSize": 1},
                {"path": "datapacks/dungeon-pack.zip", "hashes": {"sha512": "y"},
                 "env": {"client": "required", "server": "required"},
                 "downloads": [], "fileSize": 1}
            ],
            "dependencies": {"minecraft": "1.21.1", "neoforge": "21.1.93"}
        }"#;
        let m = ModpackManifest::from_modrinth_json(text).unwrap();
        let report = analyze_modpack(&m, McVersion::V1_21_1, &set(&[]), &set(&["jei"]));
        assert_eq!(
            report.entries[0].class,
            ModpackEntryClass::UniversalNative,
            "jei is covered by a declarative profile"
        );
        assert_eq!(report.entries[1].class, ModpackEntryClass::UniversalNative);
        assert_eq!(report.readiness, ModpackReadiness::ReadyNative);
    }

    // -- CurseForge ---------------------------------------------------------

    const CURSEFORGE_1211: &str = r#"{
        "minecraft": {
            "version": "1.21.1",
            "modLoaders": [
                {"id": "neoforge-21.1.93", "primary": true},
                {"id": "forge-52.0.0", "primary": false}
            ]
        },
        "manifestType": "minecraftModpack",
        "manifestVersion": 1,
        "name": "CF Adventure",
        "version": "7.1",
        "author": "someone",
        "files": [
            {"projectID": 123, "fileID": 456, "required": true,
             "slug": "create", "fileName": "create-1.21.1-6.0.6.jar"},
            {"projectID": 234, "fileID": 567, "required": true,
             "slug": "sodium", "fileName": "sodium-neoforge-0.6.13+mc1.21.1.jar"},
            {"projectID": 345, "fileID": 678, "required": true,
             "slug": "lithium"},
            {"projectID": 456, "fileID": 789, "required": false},
            {"projectID": 567, "fileID": 890, "required": true,
             "fileName": "mekanism-1.21.1-10.7.7.jar"}
        ],
        "overrides": "overrides"
    }"#;

    #[test]
    fn curseforge_manifest_parses_real_shape() {
        let m = ModpackManifest::from_curseforge_json(CURSEFORGE_1211).unwrap();
        assert_eq!(m.format, ModpackFormat::CurseForge);
        assert_eq!(m.name, "CF Adventure");
        assert_eq!(m.version, "7.1");
        assert_eq!(m.mc_version, "1.21.1");
        assert_eq!(m.loaders.len(), 2);
        assert_eq!(m.loaders[0].id, "neoforge");
        assert_eq!(m.loaders[0].version, "21.1.93");
        assert!(m.loaders[0].primary);
        assert!(!m.loaders[1].primary);
        assert_eq!(m.files.len(), 5);
        assert_eq!(m.files[0].project_id, Some(123));
        assert_eq!(m.files[0].candidate_mod_id.as_deref(), Some("create"));
        assert!(!m.files[3].required);
        assert_eq!(m.files[3].candidate_mod_id, None);
        assert_eq!(
            m.files[4].candidate_mod_id.as_deref(),
            Some("mekanism"),
            "fileName without slug should infer from the JAR name"
        );
    }

    #[test]
    fn curseforge_preflight_report() {
        let m = ModpackManifest::from_curseforge_json(CURSEFORGE_1211).unwrap();
        let report = analyze_modpack(&m, McVersion::V1_21_1, &set(&["create"]), &set(&[]));
        assert_eq!(report.version_verdict, VersionCompatVerdict::Compatible);
        // sodium + lithium superseded, create flagship, two unknown JARs.
        assert_eq!(
            report.skipped_perf_mod_names(),
            vec!["lithium".to_string(), "sodium".to_string()]
        );
        assert_eq!(report.native_names(), vec!["create".to_string()]);
        assert_eq!(
            report.needs_inspection_names(),
            vec![
                "curseforge-project-456-file-789.jar".to_string(),
                "mekanism".to_string()
            ]
        );
        assert_eq!(report.readiness, ModpackReadiness::NeedsJarInspection);
    }

    // -- Fail-closed security & validation ---------------------------------

    fn base_modrinth_with_path(path: &str) -> String {
        format!(
            r#"{{"formatVersion": 1, "game": "minecraft", "versionId": "1", "name": "x",
            "files": [{{"path": {path:?}, "hashes": {{"sha512": "a"}}, "downloads": [], "fileSize": 1}}],
            "dependencies": {{"minecraft": "1.21.1"}}}}"#
        )
    }

    #[test]
    fn modrinth_rejects_path_traversal_fail_closed() {
        for bad in [
            "../../etc/passwd",
            "mods/../../etc/passwd",
            "/abs/path.jar",
            "//server/share.jar",
            "mods\\..\\evil.jar",
            "mods\\evil.jar",
            "C:\\evil.jar",
            "C:/evil.jar",
            "mods//dup.jar",
            "mods/./weird.jar",
        ] {
            let text = base_modrinth_with_path(bad);
            assert!(
                ModpackManifest::from_modrinth_json(&text).is_err(),
                "path '{bad}' must be rejected"
            );
        }
    }

    #[test]
    fn modrinth_rejects_nul_byte_in_path() {
        let text = r#"{"formatVersion": 1, "game": "minecraft", "versionId": "1", "name": "x",
            "files": [{"path": "mods/evil.jar", "hashes": {"sha512": "a"}, "downloads": [], "fileSize": 1}],
            "dependencies": {"minecraft": "1.21.1"}}"#;
        let text = text.replace("mods/evil.jar", "mods/evil\\u0000.jar");
        assert!(ModpackManifest::from_modrinth_json(&text).is_err());
    }

    #[test]
    fn modrinth_rejects_wrong_game_and_format() {
        for bad in [
            r#"{"formatVersion": 2, "game": "minecraft", "versionId": "1", "name": "x",
               "files": [], "dependencies": {"minecraft": "1.21.1"}}"#,
            r#"{"formatVersion": 1, "game": "terraria", "versionId": "1", "name": "x",
               "files": [], "dependencies": {"minecraft": "1.21.1"}}"#,
            r#"{"formatVersion": 1, "game": "minecraft", "versionId": "1", "name": "x",
               "files": [], "dependencies": {}}"#,
        ] {
            assert!(
                ModpackManifest::from_modrinth_json(bad).is_err(),
                "input must be rejected: {bad}"
            );
        }
    }

    #[test]
    fn malformed_json_fails_closed() {
        for bad in [
            "",
            "not json",
            "{",
            r#"{"formatVersion": 1, "game": "minecraft", "versionId": "1", "name": "x", "files": [}"#,
            "[]",
            "42",
        ] {
            assert!(ModpackManifest::from_modrinth_json(bad).is_err());
            assert!(ModpackManifest::from_curseforge_json(bad).is_err());
        }
    }

    #[test]
    fn curseforge_rejects_wrong_manifest_type() {
        for bad in [
            r#"{"manifestType": "minecraftModpack", "manifestVersion": 2, "name": "x",
               "version": "1", "minecraft": {"version": "1.21.1", "modLoaders": []}, "files": []}"#,
            r#"{"manifestType": "other", "manifestVersion": 1, "name": "x",
               "version": "1", "minecraft": {"version": "1.21.1", "modLoaders": []}, "files": []}"#,
            r#"{"manifestType": "minecraftModpack", "manifestVersion": 1, "name": "x",
               "version": "1", "minecraft": {"version": "1.21.1", "modLoaders": []},
               "files": [{"projectID": "a", "fileID": 1, "required": true}]}"#,
        ] {
            assert!(
                ModpackManifest::from_curseforge_json(bad).is_err(),
                "input must be rejected: {bad}"
            );
        }
    }

    #[test]
    fn curseforge_rejects_unsafe_slug_and_filename() {
        for extra in [
            r#""slug": "../evil""#,
            r#""slug": "a/b""#,
            r#""fileName": "..\\evil.jar""#,
            r#""fileName": "sub/evil.jar""#,
        ] {
            let text = format!(
                r#"{{"manifestType": "minecraftModpack", "manifestVersion": 1, "name": "x",
                "version": "1", "minecraft": {{"version": "1.21.1", "modLoaders": []}},
                "files": [{{"projectID": 1, "fileID": 2, "required": true, {extra}}}]}}"#
            );
            assert!(
                ModpackManifest::from_curseforge_json(&text).is_err(),
                "input must be rejected: {text}"
            );
        }
    }

    // -- Filename -> mod id normalization -----------------------------------

    #[test]
    fn candidate_id_strips_loader_and_version_suffixes() {
        let cases = [
            ("mods/create-1.21.1-6.0.6.jar", "create"),
            ("mods/sodium-fabric-0.6.13+mc1.21.1.jar", "sodium"),
            ("mods/lithium-neoforge-0.15.0+mc1.21.1.jar", "lithium"),
            ("mods/modernfix-neoforge-5.20.2+mc1.21.1.jar", "modernfix"),
            (
                "mods/DistantHorizons-2.3.2-b-1.21.1-fabric-neoforge.jar",
                "distanthorizons",
            ),
            ("mods/ferritecore-7.0.2-neoforge.jar", "ferritecore"),
            ("mods/fabric-api-0.102.0+1.21.1.jar", "fabric-api"),
            ("mods/aether-1.21.1-1.5.9-neoforge.jar", "aether"),
            ("mods/jei-1.21.1-neoforge-19.21.0.247.jar", "jei"),
            ("mods/embeddium_extra-0.5.7+mc1.21.1.jar", "embeddium-extra"),
            ("mods/mekanism-1.21.1-10.7.7.jar", "mekanism"),
        ];
        for (path, expected) in cases {
            assert_eq!(
                infer_candidate_mod_id(path).as_deref(),
                Some(expected),
                "{path} should infer '{expected}'"
            );
        }
        assert_eq!(infer_candidate_mod_id("mods/1.21.1.jar"), None);
        assert_eq!(infer_candidate_mod_id("datapacks/x.zip"), None);
    }

    #[test]
    fn uppercase_jar_extension_still_needs_inspection() {
        let text = r#"{
            "formatVersion": 1,
            "game": "minecraft",
            "versionId": "1.0",
            "name": "Upper Jar",
            "files": [
                {"path": "mods/MYSTERY-1.0.JAR", "hashes": {"sha512": "x"},
                 "env": {"client": "required", "server": "required"},
                 "downloads": [], "fileSize": 1}
            ],
            "dependencies": {"minecraft": "1.21.1"}
        }"#;
        let m = ModpackManifest::from_modrinth_json(text).unwrap();
        let report = analyze_modpack(&m, McVersion::V1_21_1, &set(&[]), &set(&[]));
        assert_eq!(
            report.entries[0].class,
            ModpackEntryClass::NeedsInspection,
            "an uppercase .JAR is still a Java JAR and must not slip through as data"
        );
        assert_eq!(report.readiness, ModpackReadiness::NeedsJarInspection);
    }

    // -- Human summary ------------------------------------------------------

    #[test]
    fn human_summary_is_plain_language_and_names_skipped_mods() {
        let m = ModpackManifest::from_modrinth_json(MODRINTH_1211).unwrap();
        let report = analyze_modpack(
            &m,
            McVersion::V1_21_1,
            &set(&["create", "aether"]),
            &set(&[]),
        );
        let summary = report.format_human_summary();
        assert!(summary.contains("Create NeoForge Adventure"));
        assert!(summary.contains("Modrinth pack"));
        assert!(summary.contains("matches this server"));
        // Names the skipped perf mods explicitly with the built-in note.
        for name in [
            "distanthorizons",
            "ferritecore",
            "lithium",
            "modernfix",
            "sodium",
        ] {
            assert!(
                summary.contains(name),
                "summary must name skipped perf mod '{name}':\n{summary}"
            );
        }
        assert!(summary.contains("built in"));
        assert!(summary.contains("iris"));
        assert!(summary.contains("aether"));
        assert!(summary.contains("create"));
        assert!(summary.contains("mysterious-factory"));
        assert!(summary.contains("almost ready"));
    }

    #[test]
    fn human_summary_reports_version_mismatch_plainly() {
        let m = ModpackManifest::from_modrinth_json(MODRINTH_1211).unwrap();
        let report = analyze_modpack(&m, McVersion::V26_3, &set(&[]), &set(&[]));
        let summary = report.format_human_summary();
        assert!(summary.contains("versions do not match"));
        assert!(summary.contains("different Minecraft version"));
    }
}
