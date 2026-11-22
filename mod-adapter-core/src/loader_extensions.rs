// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later

//! Generic, data-driven loader extensions closing the remaining NeoForge
//! 1.21.1 & Fabric/Quilt 1.21.1 compatibility surface:
//!
//! - [`parse_quilt_mod_json`][]: `quilt.mod.json` manifest parsing
//!   (`schema_version`, `quilt_loader.id`/`version`/`depends`,
//!   `minecraft.environment`) alongside `fabric.mod.json` and
//!   `META-INF/*.mods.toml`, feeding the same [`ModJarDescriptor`] table in
//!   [`crate::ecosystem_compat`].
//! - [`inspect_jar_extensions`][]: bounded Jar-in-Jar inspection — parses
//!   `META-INF/jarjar/metadata.json` and recursively walks declared nested
//!   JARs under strict fail-closed depth/count/byte bounds
//!   ([`MAX_JAR_IN_JAR_DEPTH`], [`MAX_JAR_IN_JAR_NESTED_JARS`],
//!   [`MAX_JAR_IN_JAR_TOTAL_BYTES`]) with path-traversal and non-`.jar`
//!   rejection.
//! - [`parse_access_transformer_cfg`][]: NeoForge/Forge
//!   `META-INF/accesstransformer.cfg` directives (`public`/`protected`/
//!   `default`/`private`, `+f`/`-f` final modifiers, class/field/method
//!   targets) classified by whether they need JVM-bridge delegation.
//! - [`encode_fabric_registry_sync_direct`] /
//!   [`decode_fabric_registry_sync_direct`][]: clean binary codec for the
//!   Fabric 1.21.1 `fabric:registry/sync/direct` channel — a deterministic
//!   `registry_id -> Vec<(entry_id, raw_id)>` map wire format with bounded
//!   counts and string lengths, reusing [`crate::neoforge_wire`]'s
//!   Protocol 767 VarInt/string primitives.
//! - [`parse_tag_file_json`] + [`FluidTagRegistry`][]: generic tag-file
//!   parsing (`replace`, `values`, `#tag` nesting, `required: false`
//!   optional entries) applied to `data/<ns>/tags/fluid/*.json` and legacy
//!   `data/<ns>/tags/fluids/*.json` layouts.
//! - [`parse_data_map`] + [`DataMapRegistry`][]: NeoForge 1.21.1 data maps
//!   from `data/<ns>/data_maps/<registry>/<map>.json` and the legacy
//!   `data/<ns>/neoforge/data_maps/<registry>/<map>.json` layout —
//!   `replace`, `values`, `remove`, the eight built-in map names
//!   (`compostables`, `furnace_fuels`, `oxidizables`, `waxables`,
//!   `raid_hero_gifts`, `vibration_frequencies`, `monster_room_mobs`,
//!   `parrot_imitations`) and arbitrary custom maps.
//! - [`parse_biome_modifier`][]: `data/<ns>/neoforge/biome_modifier/*.json`
//!   loading for `neoforge:add_features`, `neoforge:remove_features`,
//!   `neoforge:add_spawns`, `neoforge:remove_spawns`, and `neoforge:none`
//!   with biome id/`#tag`/list selectors.
//! - [`ConventionalTagRegistry`][]: all ten 1.21.1 `c:` conventional tag
//!   families (`c:ores`, `c:ingots`, `c:raw_materials`, `c:crops`,
//!   `c:foods`, `c:tools`, `c:stones`, `c:dyes`, `c:gems`,
//!   `c:storage_blocks`) seeded with the Vanilla 1.21.1 baseline member
//!   sets, legacy `forge:*`/`neoforge:*` alias resolution, and datapack
//!   tag-file merging.

use std::collections::{BTreeMap, BTreeSet};

use crate::data_loader::{parse_json, JsonValue};
use crate::ecosystem_compat::{ModDependency, ModEnvironment};
use crate::modpack::{self, ZipMemberEntry};
use crate::neoforge_wire::{
    read_mc_string, read_varint, write_mc_string, write_varint, MAX_REGISTRY_DATA_NAME_LEN,
};

/// Error produced by every loader-extension parser/scanner. Single-variant
/// like [`crate::ecosystem_compat::JarScanError`]'s `Manifest` arm: a
/// human-readable `reason` that is always fail-closed — callers reject the
/// artifact rather than partially trusting it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LoaderExtensionError {
    reason: String,
}

impl LoaderExtensionError {
    pub fn reason(&self) -> &str {
        &self.reason
    }
}

impl std::fmt::Display for LoaderExtensionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.reason)
    }
}

impl std::error::Error for LoaderExtensionError {}

fn err(msg: impl Into<String>) -> LoaderExtensionError {
    LoaderExtensionError {
        reason: msg.into(),
    }
}

fn req_str<'a>(
    obj: &'a BTreeMap<String, JsonValue>,
    key: &str,
    ctx: &str,
) -> Result<&'a str, LoaderExtensionError> {
    obj.get(key)
        .and_then(JsonValue::as_str)
        .ok_or_else(|| err(format!("{ctx}: missing string '{key}'")))
}

// ===========================================================================
// 1. quilt.mod.json (Quilt Loader metadata, Fabric-family ecosystem)
// ===========================================================================

/// Canonical manifest member name for Quilt Loader mods.
pub const QUILT_MOD_JSON: &str = "quilt.mod.json";

/// Maven coordinate providing the `quilt_loader` runtime dependency. The
/// `org.quiltmc` toolchain layers — like `net.fabricmc:fabric-loader` — are
/// implemented natively by megu, so a dependency on them never needs a
/// JVM-side artifact.
pub const QUILT_LOADER_ARTIFACT: &str = "org.quiltmc:quilt-loader";

/// Runtime artifacts that satisfy loader/game dependency ids — Quilt and
/// Fabric loader layers megu implements natively, the game itself, and
/// the JRE. Used by dependency planning to map a `depends` entry onto the
/// artifact coordinate that provides it.
pub fn provided_loader_artifact(mod_id: &str) -> Option<&'static str> {
    Some(match mod_id {
        "quilt_loader" | "quilted_fabric_loader" | "quilt" => QUILT_LOADER_ARTIFACT,
        "fabricloader" | "fabric-loader" | "fabric" | "fabric-api" | "fabric_api" => {
            "net.fabricmc:fabric-loader"
        }
        "minecraft" => "net.minecraft:minecraft",
        "java" => "java:jre",
        "neoforge" => "net.neoforged:neoforge",
        "forge" => "net.minecraftforge:forge",
        _ => return None,
    })
}

/// Parsed `quilt.mod.json` (`schema_version` 1) — the Quilt counterpart of
/// `fabric.mod.json`. Quilt is a Fabric-fork toolchain, so the descriptor
/// table treats Quilt-only JARs as the Fabric-family ecosystem.
#[derive(Clone, Debug, PartialEq)]
pub struct QuiltMeta {
    /// `quilt_loader.id`.
    pub mod_id: String,
    /// `quilt_loader.group` (Maven group the mod publishes under; empty
    /// when undeclared).
    pub group: String,
    /// `quilt_loader.version` (string form; `{value: ".."}` objects also
    /// normalize to their `value`).
    pub version: String,
    /// `minecraft.environment` (`"*"`/`"both"` → `Both`, `"client"` →
    /// `Client`, `"server"`/`"dedicated_server"` → `Server`).
    pub environment: ModEnvironment,
    /// Minecraft dependency range from `quilt_loader.depends`, if present.
    pub mc_range: Option<String>,
    /// Mixin config member names declared by `quilt_loader.mixins` (all
    /// string/array/object-map forms flattened).
    pub mixin_configs: Vec<String>,
    /// All `quilt_loader.entrypoints` arrays flattened (strings and
    /// `{adapter, value}` objects contribute their `value`).
    pub entrypoints: Vec<String>,
    /// `quilt_loader.depends` (array and object-map forms) normalized onto
    /// [`ModDependency`]; `optional: true` and `only_if`/`unless`/`any`
    /// alternative containers become `required = false`.
    pub dependencies: Vec<ModDependency>,
}

fn valid_quilt_mod_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .bytes()
            .all(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'_' | b'-'))
}

fn quilt_versions_to_string(val: &JsonValue) -> Option<String> {
    if let Some(s) = val.as_str() {
        return Some(s.to_string());
    }
    if let Some(arr) = val.as_array() {
        let parts: Vec<&str> = arr.iter().filter_map(JsonValue::as_str).collect();
        if parts.is_empty() {
            return None;
        }
        return Some(parts.join(" || "));
    }
    let obj = val.as_object()?;
    let mut parts = Vec::new();
    for key in ["versions", "version", "all", "any"] {
        if let Some(inner) = obj.get(key) {
            if let Some(s) = quilt_versions_to_string(inner) {
                parts.push(s);
            }
        }
    }
    (!parts.is_empty()).then(|| parts.join(" || "))
}

/// One `quilt_loader.depends` entry: object form
/// (`{"id": "...", "versions": "...", "optional": bool}`) or bare string.
/// Alternative/nested containers (`any`, `all`, `only_if`, `unless`)
/// recurse, marking members non-required unless the container is `all`.
fn collect_quilt_dependency(
    value: &JsonValue,
    required: bool,
    ctx: &str,
    out: &mut Vec<ModDependency>,
) -> Result<(), LoaderExtensionError> {
    if let Some(id) = value.as_str() {
        out.push(ModDependency {
            mod_id: id.to_string(),
            version_range: None,
            required,
        });
        return Ok(());
    }
    let obj = value
        .as_object()
        .ok_or_else(|| err(format!("{ctx}: depends entry must be a string or object")))?;
    for (key, sub_required) in [
        ("any", false),
        ("unless", false),
        ("only_if", false),
        ("all", true),
    ] {
        if let Some(inner) = obj.get(key) {
            let items: &[JsonValue] = inner.as_array().map_or(std::slice::from_ref(inner), |a| a);
            for item in items {
                collect_quilt_dependency(item, required && sub_required, ctx, out)?;
            }
        }
    }
    let Some(id) = obj.get("id").and_then(JsonValue::as_str) else {
        // A pure container entry (only any/all/only_if/unless) is fine.
        if obj.keys().all(|k| {
            matches!(
                k.as_str(),
                "any" | "all" | "only_if" | "unless" | "reason" | "optional"
            )
        }) {
            return Ok(());
        }
        return Err(err(format!("{ctx}: depends entry is missing 'id'")));
    };
    let optional = obj.get("optional").and_then(JsonValue::as_bool) == Some(true);
    let version_range = obj
        .get("versions")
        .or_else(|| obj.get("version"))
        .and_then(quilt_versions_to_string);
    out.push(ModDependency {
        mod_id: id.to_string(),
        version_range,
        required: required && !optional,
    });
    Ok(())
}

/// Parse `quilt.mod.json` bytes. Fail-closed: a malformed root, missing
/// `quilt_loader.id`/`version`, an invalid mod id, or an unknown
/// `minecraft.environment` value rejects the manifest.
pub fn parse_quilt_mod_json(bytes: &[u8]) -> Result<QuiltMeta, LoaderExtensionError> {
    let ctx = QUILT_MOD_JSON;
    let root = parse_json(bytes)
        .map_err(|e| err(format!("{ctx}: {}", match e {
            crate::data_loader::DataLoadError::Json(m)
            | crate::data_loader::DataLoadError::Io(m)
            | crate::data_loader::DataLoadError::Zip(m) => m,
        })))?;
    let obj = root
        .as_object()
        .ok_or_else(|| err(format!("{ctx}: root must be an object")))?;

    let schema = obj
        .get("schema_version")
        .or_else(|| obj.get("schemaVersion"))
        .ok_or_else(|| err(format!("{ctx}: missing 'schema_version'")))?;
    match schema.as_u64() {
        Some(1) => {}
        Some(v) => {
            return Err(err(format!(
                "{ctx}: unsupported schema_version {v} (expected 1)"
            )))
        }
        None => return Err(err(format!("{ctx}: schema_version must be 1"))),
    }

    let loader = obj
        .get("quilt_loader")
        .and_then(JsonValue::as_object)
        .ok_or_else(|| err(format!("{ctx}: missing object 'quilt_loader'")))?;
    let mod_id = req_str(loader, "id", ctx)?.to_string();
    if !valid_quilt_mod_id(&mod_id) {
        return Err(err(format!("{ctx}: invalid mod id '{mod_id}'")));
    }
    let version = match loader.get("version") {
        Some(JsonValue::String(s)) => s.clone(),
        Some(v) => v
            .as_object()
            .and_then(|o| o.get("value"))
            .and_then(JsonValue::as_str)
            .ok_or_else(|| err(format!("{ctx}: 'version' must be a string or {{value}}")))?
            .to_string(),
        None => return Err(err(format!("{ctx}: missing string 'version'"))),
    };
    let group = loader
        .get("group")
        .and_then(JsonValue::as_str)
        .unwrap_or_default()
        .to_string();

    // minecraft.environment ("*" / "client" / "server" /
    // "dedicated_server"); a root-level "environment" string is accepted
    // as a fallback used by some early drafts.
    let environment = match obj
        .get("minecraft")
        .and_then(JsonValue::as_object)
        .and_then(|m| m.get("environment"))
        .or_else(|| obj.get("environment"))
        .and_then(JsonValue::as_str)
    {
        None | Some("*") | Some("both") => ModEnvironment::Both,
        Some("client") => ModEnvironment::Client,
        Some("server") | Some("dedicated_server") => ModEnvironment::Server,
        Some(other) => {
            return Err(err(format!(
                "{ctx}: unknown minecraft.environment '{other}'"
            )))
        }
    };

    let mut dependencies = Vec::new();
    let mut mc_range = None;
    match loader.get("depends") {
        None => {}
        Some(JsonValue::Array(items)) => {
            for item in items {
                collect_quilt_dependency(item, true, ctx, &mut dependencies)?;
            }
        }
        Some(JsonValue::Object(map)) => {
            for (dep_id, dep_val) in map {
                let version_range = quilt_versions_to_string(dep_val);
                let optional = dep_val
                    .as_object()
                    .and_then(|o| o.get("optional"))
                    .and_then(JsonValue::as_bool)
                    == Some(true);
                dependencies.push(ModDependency {
                    mod_id: dep_id.clone(),
                    version_range,
                    required: !optional,
                });
            }
        }
        Some(_) => {
            return Err(err(format!(
                "{ctx}: 'depends' must be an array or object"
            )))
        }
    }
    for dep in &dependencies {
        if dep.mod_id == "minecraft" {
            mc_range = dep.version_range.clone();
        }
    }

    let mut entrypoints = Vec::new();
    if let Some(ep) = loader.get("entrypoints") {
        let Some(ep_obj) = ep.as_object() else {
            return Err(err(format!(
                "{ctx}: 'entrypoints' must be an object"
            )));
        };
        for (ep_name, list) in ep_obj {
            let Some(items) = list.as_array() else {
                return Err(err(format!(
                    "{ctx}: entrypoints.{ep_name} must be an array"
                )));
            };
            for item in items {
                if let Some(s) = item.as_str() {
                    entrypoints.push(s.to_string());
                } else if let Some(v) = item
                    .as_object()
                    .and_then(|o| o.get("value"))
                    .and_then(JsonValue::as_str)
                {
                    entrypoints.push(v.to_string());
                } else {
                    return Err(err(format!(
                        "{ctx}: bad entrypoint entry under '{ep_name}'"
                    )));
                }
            }
        }
    }

    let mut mixin_configs = Vec::new();
    let mut collect_mixin = |val: &JsonValue| -> Result<(), LoaderExtensionError> {
        match val {
            JsonValue::String(s) => mixin_configs.push(s.clone()),
            JsonValue::Array(items) => {
                for item in items {
                    if let Some(s) = item.as_str() {
                        mixin_configs.push(s.to_string());
                    } else if let Some(cfg) = item
                        .as_object()
                        .and_then(|o| o.get("config").or_else(|| o.get("file")))
                        .and_then(JsonValue::as_str)
                    {
                        mixin_configs.push(cfg.to_string());
                    } else {
                        return Err(err(format!(
                            "{ctx}: bad 'mixins' entry (expected string or {{config}})"
                        )));
                    }
                }
            }
            JsonValue::Object(map) => {
                // Object form maps side ("client"/"server") to a string or
                // array of config names.
                for side in map.values() {
                    match side {
                        JsonValue::String(s) => mixin_configs.push(s.clone()),
                        JsonValue::Array(items) => {
                            for item in items {
                                let Some(s) = item.as_str() else {
                                    return Err(err(format!(
                                        "{ctx}: bad 'mixins' side entry"
                                    )));
                                };
                                mixin_configs.push(s.to_string());
                            }
                        }
                        _ => {
                            return Err(err(format!(
                                "{ctx}: bad 'mixins' side value"
                            )))
                        }
                    }
                }
            }
            _ => {
                return Err(err(format!(
                    "{ctx}: 'mixins' must be a string, array, or object"
                )))
            }
        }
        Ok(())
    };
    if let Some(m) = loader.get("mixins") {
        collect_mixin(m)?;
    }

    Ok(QuiltMeta {
        mod_id,
        group,
        version,
        environment,
        mc_range,
        mixin_configs,
        entrypoints,
        dependencies,
    })
}

// ===========================================================================
// 2. Jar-in-Jar — META-INF/jarjar/metadata.json + bounded nested scan
// ===========================================================================

/// Forge/NeoForge JarJar (Jar-in-Jar) metadata member.
pub const JAR_IN_JAR_METADATA_PATH: &str = "META-INF/jarjar/metadata.json";

/// Archive member prefix every declared embedded JAR must live under.
pub const JAR_IN_JAR_MEMBER_PREFIX: &str = "META-INF/jarjar/";

/// Deepest nesting level inspected: the outer JAR is depth 0 and nested
/// archives may declare their own JarJar metadata down to depth 3.
/// Metadata found deeper than this fails closed — an unbounded chain of
/// nested archives is a classic decompression-bomb vector.
pub const MAX_JAR_IN_JAR_DEPTH: usize = 3;

/// Total embedded JARs (across all levels) a single outer archive may
/// declare. Exceeding this fails closed.
pub const MAX_JAR_IN_JAR_NESTED_JARS: usize = 64;

/// Total decompressed bytes of embedded-JAR members read across all
/// nesting levels — the aggregate decompression-bomb budget on top of
/// [`crate::modpack`]'s per-member cap.
pub const MAX_JAR_IN_JAR_TOTAL_BYTES: u64 = 64 * 1024 * 1024;

/// One `jars[]` record from `META-INF/jarjar/metadata.json`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EmbeddedJarDesc {
    /// `identifier` normalized to `group:artifact`.
    pub identifier: String,
    /// `identifier.group`.
    pub group: String,
    /// `identifier.artifact`.
    pub artifact: String,
    /// `version.artifactVersion` (or string form).
    pub version: String,
    /// `version.range` Maven-style range, when declared.
    pub version_range: Option<String>,
    /// Archive member path (`META-INF/jarjar/<name>.jar`), already
    /// validated by [`validate_jar_in_jar_path`].
    pub path: String,
}

/// Validate an embedded-JAR member path: must live under
/// `META-INF/jarjar/`, end in `.jar`, and must not contain a leading `/`,
/// `\\`, `..` segments, `.` segments, or NUL bytes — fail closed on every
/// path-traversal or non-JAR shape.
pub fn validate_jar_in_jar_path(path: &str) -> Result<(), LoaderExtensionError> {
    if path.is_empty() || path.len() > 512 {
        return Err(err("jarjar: invalid embedded path length"));
    }
    if path.bytes().any(|b| b == 0) {
        return Err(err("jarjar: NUL byte in embedded path"));
    }
    if path.starts_with('/') || path.contains('\\') {
        return Err(err(format!(
            "jarjar: path traversal rejected in '{path}'"
        )));
    }
    if !path.starts_with(JAR_IN_JAR_MEMBER_PREFIX) {
        return Err(err(format!(
            "jarjar: embedded path '{path}' is outside {JAR_IN_JAR_MEMBER_PREFIX}"
        )));
    }
    for seg in path.split('/') {
        if seg.is_empty() || seg == "." || seg == ".." {
            return Err(err(format!(
                "jarjar: path traversal rejected in '{path}'"
            )));
        }
    }
    if !path.ends_with(".jar") {
        return Err(err(format!(
            "jarjar: embedded path '{path}' is not a .jar member"
        )));
    }
    Ok(())
}

/// Parse `META-INF/jarjar/metadata.json` into ordered [`EmbeddedJarDesc`]
/// records, validating every declared path.
pub fn parse_jarjar_metadata(
    bytes: &[u8],
) -> Result<Vec<EmbeddedJarDesc>, LoaderExtensionError> {
    let ctx = "jarjar/metadata.json";
    let root = parse_json(bytes).map_err(|e| {
        err(format!(
            "{ctx}: {}",
            match e {
                crate::data_loader::DataLoadError::Json(m)
                | crate::data_loader::DataLoadError::Io(m)
                | crate::data_loader::DataLoadError::Zip(m) => m,
            }
        ))
    })?;
    let obj = root
        .as_object()
        .ok_or_else(|| err(format!("{ctx}: root must be an object")))?;
    let jars = obj
        .get("jars")
        .and_then(JsonValue::as_array)
        .ok_or_else(|| err(format!("{ctx}: missing array 'jars'")))?;

    let mut out = Vec::with_capacity(jars.len().min(MAX_JAR_IN_JAR_NESTED_JARS));
    for (i, jar) in jars.iter().enumerate() {
        let entry = jar
            .as_object()
            .ok_or_else(|| err(format!("{ctx}: jars[{i}] must be an object")))?;
        let ectx = format!("{ctx} jars[{i}]");
        let (group, artifact) = match entry.get("identifier") {
            Some(JsonValue::String(s)) => {
                let (g, a) = s.split_once(':').ok_or_else(|| {
                    err(format!("{ectx}: identifier must be 'group:artifact'"))
                })?;
                (g.to_string(), a.to_string())
            }
            Some(v) => {
                let id_obj = v.as_object().ok_or_else(|| {
                    err(format!("{ectx}: 'identifier' must be an object or 'g:a'"))
                })?;
                (
                    req_str(id_obj, "group", &ectx)?.to_string(),
                    req_str(id_obj, "artifact", &ectx)?.to_string(),
                )
            }
            None => return Err(err(format!("{ectx}: missing 'identifier'"))),
        };
        if group.is_empty() || artifact.is_empty() {
            return Err(err(format!("{ectx}: empty identifier group/artifact")));
        }
        let (version, version_range) = match entry.get("version") {
            Some(JsonValue::String(s)) => (s.clone(), None),
            Some(v) => {
                let ver_obj = v.as_object().ok_or_else(|| {
                    err(format!("{ectx}: 'version' must be an object or string"))
                })?;
                let artifact_version = ver_obj
                    .get("artifactVersion")
                    .or_else(|| ver_obj.get("version"))
                    .and_then(JsonValue::as_str)
                    .unwrap_or_default()
                    .to_string();
                let range = ver_obj
                    .get("range")
                    .and_then(JsonValue::as_str)
                    .map(str::to_string);
                (artifact_version, range)
            }
            None => return Err(err(format!("{ectx}: missing 'version'"))),
        };
        let path = req_str(entry, "path", &ectx)?.to_string();
        validate_jar_in_jar_path(&path)
            .map_err(|e| err(format!("{ectx}: {}", e.reason())))?;
        out.push(EmbeddedJarDesc {
            identifier: format!("{group}:{artifact}"),
            group,
            artifact,
            version,
            version_range,
            path,
        });
    }
    Ok(out)
}

/// Result of the bounded recursive Jar-in-Jar inspection.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct JarInJarScan {
    /// Every metadata-declared embedded JAR, in declaration order across
    /// all levels (parents before children).
    pub declared: Vec<EmbeddedJarDesc>,
    /// Nested-JAR members actually opened, decompressed, and verified.
    pub extracted_members: usize,
    /// Sum of decompressed bytes of all opened embedded-JAR members.
    pub total_member_bytes: u64,
    /// Deepest nesting level at which JarJar metadata was found.
    pub max_depth_seen: usize,
}

impl JarInJarScan {
    /// `true` when the archive declares no embedded JARs.
    pub fn is_empty(&self) -> bool {
        self.declared.is_empty()
    }
}

struct JarInJarBudget {
    count: usize,
    bytes: u64,
}

fn scan_jij_level(
    entries: &[ZipMemberEntry],
    jar_bytes: &[u8],
    display: &str,
    depth: usize,
    budget: &mut JarInJarBudget,
    scan: &mut JarInJarScan,
) -> Result<(), LoaderExtensionError> {
    let Some(meta_entry) = entries
        .iter()
        .find(|e| e.name == JAR_IN_JAR_METADATA_PATH)
    else {
        return Ok(());
    };
    if depth > MAX_JAR_IN_JAR_DEPTH {
        return Err(err(format!(
            "{display}: jarjar metadata nested deeper than {MAX_JAR_IN_JAR_DEPTH} levels"
        )));
    }
    scan.max_depth_seen = scan.max_depth_seen.max(depth);
    let meta_bytes = modpack::read_zip_member(jar_bytes, meta_entry, display)
        .map_err(|e| err(format!("{display}: {}", e.reason())))?;
    let declared = parse_jarjar_metadata(&meta_bytes)
        .map_err(|e| err(format!("{display}: {}", e.reason())))?;

    for desc in declared {
        let member = entries
            .iter()
            .find(|e| e.name == desc.path)
            .ok_or_else(|| {
                err(format!(
                    "{display}: jarjar declares {} but no such member exists",
                    desc.path
                ))
            })?;
        let bytes = modpack::read_zip_member(jar_bytes, member, display)
            .map_err(|e| err(format!("{display}: {}", e.reason())))?;
        budget.count += 1;
        if budget.count > MAX_JAR_IN_JAR_NESTED_JARS {
            return Err(err(format!(
                "{display}: more than {MAX_JAR_IN_JAR_NESTED_JARS} nested JARs declared"
            )));
        }
        budget.bytes = budget.bytes.saturating_add(bytes.len() as u64);
        if budget.bytes > MAX_JAR_IN_JAR_TOTAL_BYTES {
            return Err(err(format!(
                "{display}: nested JAR members exceed the {MAX_JAR_IN_JAR_TOTAL_BYTES}-byte total budget (decompression bomb guard)"
            )));
        }
        scan.extracted_members += 1;
        scan.total_member_bytes = budget.bytes;
        // Nested JARs may carry their own jarjar metadata — descend under
        // the same global budget.
        let nested_entries = modpack::zip_member_list(&bytes, display).map_err(|e| {
            err(format!(
                "{display}: nested JAR {}: {}",
                desc.path,
                e.reason()
            ))
        })?;
        scan.declared.push(desc);
        scan_jij_level(&nested_entries, &bytes, display, depth + 1, budget, scan)?;
    }
    Ok(())
}

/// Bounded recursive Jar-in-Jar inspection of one JAR's bytes: reads the
/// outer `META-INF/jarjar/metadata.json`, validates every declared member
/// path, extracts each embedded JAR (all through
/// [`crate::modpack`]'s fail-closed ZIP reader), and recurses into nested
/// metadata under the global [`MAX_JAR_IN_JAR_DEPTH`] /
/// [`MAX_JAR_IN_JAR_NESTED_JARS`] / [`MAX_JAR_IN_JAR_TOTAL_BYTES`] bounds.
/// An archive with no JarJar metadata returns an empty scan; any bound or
/// structural violation is a hard error.
pub fn inspect_jar_in_jar(
    jar_bytes: &[u8],
    display: &str,
) -> Result<JarInJarScan, LoaderExtensionError> {
    let mut scan = JarInJarScan::default();
    let mut budget = JarInJarBudget { count: 0, bytes: 0 };
    let entries = modpack::zip_member_list(jar_bytes, display)
        .map_err(|e| err(format!("{display}: {}", e.reason())))?;
    scan_jij_level(&entries, jar_bytes, display, 0, &mut budget, &mut scan)?;
    Ok(scan)
}

// ===========================================================================
// 3. META-INF/accesstransformer.cfg (NeoForge/Forge Access Transformer)
// ===========================================================================

/// Member paths where an Access Transformer config may live inside a mod
/// JAR (`META-INF/accesstransformer.cfg` is the canonical NeoForge/FML
/// location; a bare root file is also accepted).
pub const ACCESS_TRANSFORMER_PATHS: &[&str] = &[
    "META-INF/accesstransformer.cfg",
    "accesstransformer.cfg",
];

/// Largest accepted `accesstransformer.cfg`: 1 MiB / 65536 lines.
pub const MAX_ACCESS_TRANSFORMER_BYTES: usize = 1024 * 1024;
pub const MAX_ACCESS_TRANSFORMER_LINES: usize = 65_536;

/// Target access level an AT directive assigns.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AtAccess {
    Public,
    Protected,
    /// `default` (package-private).
    Default,
    Private,
}

impl AtAccess {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Public => "public",
            Self::Protected => "protected",
            Self::Default => "default",
            Self::Private => "private",
        }
    }
}

/// `+f` / `-f` final-flag modifier on an AT directive.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AtFinalModifier {
    /// No `f` suffix — leave `final` untouched.
    Keep,
    /// `-f`: remove `final` from the target.
    Remove,
    /// `+f`: add `final` to the target.
    Add,
}

/// What an AT directive's target names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AtTargetKind {
    /// A bare class name (`com.example.Foo`).
    Class,
    /// `com.example.Foo fieldName`.
    Field,
    /// `com.example.Foo methodName(descriptor)returnType`.
    Method,
}

/// One parsed Access Transformer directive.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AccessTransformerEntry {
    /// 1-based line number in the `.cfg` source.
    pub line_no: usize,
    pub access: AtAccess,
    pub final_modifier: AtFinalModifier,
    pub target_kind: AtTargetKind,
    /// Owning class in dotted source form (`com.example.Foo`).
    pub owner_class: String,
    /// Field or method name; `None` for class targets.
    pub member_name: Option<String>,
    /// JVM method descriptor (`(I)V`, `()Ljava/lang/String;`); `None` for
    /// class/field targets.
    pub descriptor: Option<String>,
}

impl AccessTransformerEntry {
    /// Whether satisfying this AT requires JVM-bridge delegation (real
    /// bytecode rewriting) rather than the bounded shim's member-dispatch
    /// table.
    ///
    /// Member-level directives — widening a field/method's access or
    /// stripping its `final` flag — are expressible in the shim's dispatch
    /// table: callers route through the adapter and the member is simply
    /// marked reachable/mutable, so they do **not** need a bridge.
    /// Class-level directives rewrite the class's own access/`final`
    /// header, and `+f` directives alter subclassability guarantees for
    /// downstream code; both are structural bytecode mutations a shim
    /// cannot represent, so they require JVM-bridge delegation.
    pub fn requires_jvm_bridge(&self) -> bool {
        self.target_kind == AtTargetKind::Class
            || self.final_modifier == AtFinalModifier::Add
    }
}

fn parse_at_directive(token: &str, ctx: &str) -> Result<(AtAccess, AtFinalModifier), LoaderExtensionError> {
    let (access_str, final_modifier) = match token.rsplit(['-', '+']).next() {
        Some("f") if token.len() > 1 => {
            let (head, suffix) = token.split_at(token.len() - 2);
            let modifier = match suffix {
                "-f" => AtFinalModifier::Remove,
                "+f" => AtFinalModifier::Add,
                _ => unreachable!(),
            };
            (head, modifier)
        }
        _ => (token, AtFinalModifier::Keep),
    };
    let access = match access_str {
        "public" => AtAccess::Public,
        "protected" => AtAccess::Protected,
        "default" => AtAccess::Default,
        "private" => AtAccess::Private,
        _ => {
            return Err(err(format!(
                "{ctx}: unknown access directive '{token}'"
            )))
        }
    };
    Ok((access, final_modifier))
}

/// Parse a NeoForge/Forge `accesstransformer.cfg`. Lines are
/// `directive target [member]` — `#` comments and blank lines are
/// skipped, and every malformed line is a hard error (fail closed: a
/// half-applied AT set silently weakens encapsulation guarantees).
pub fn parse_access_transformer_cfg(
    bytes: &[u8],
) -> Result<Vec<AccessTransformerEntry>, LoaderExtensionError> {
    if bytes.len() > MAX_ACCESS_TRANSFORMER_BYTES {
        return Err(err("accesstransformer.cfg exceeds the 1 MiB limit"));
    }
    let text = std::str::from_utf8(bytes)
        .map_err(|_| err("accesstransformer.cfg is not UTF-8"))?;

    let mut entries = Vec::new();
    for (idx, raw_line) in text.lines().enumerate() {
        let line_no = idx + 1;
        if line_no > MAX_ACCESS_TRANSFORMER_LINES {
            return Err(err("accesstransformer.cfg exceeds 65536 lines"));
        }
        // '#' begins a comment anywhere on the line.
        let line = raw_line.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        let ctx = format!("accesstransformer.cfg:{line_no}");
        let tokens: Vec<&str> = line.split_whitespace().collect();
        if tokens.len() < 2 {
            return Err(err(format!("{ctx}: expected 'directive target [member]'")));
        }
        let (access, final_modifier) = parse_at_directive(tokens[0], &ctx)?;
        let owner_class = tokens[1].to_string();
        if owner_class.is_empty() || owner_class.contains('/') || owner_class.contains('(') {
            return Err(err(format!("{ctx}: invalid class name '{owner_class}'")));
        }
        match tokens.len() {
            2 => entries.push(AccessTransformerEntry {
                line_no,
                access,
                final_modifier,
                target_kind: AtTargetKind::Class,
                owner_class,
                member_name: None,
                descriptor: None,
            }),
            3 => {
                let member = tokens[2];
                if let Some(paren) = member.find('(') {
                    let name = &member[..paren];
                    let descriptor = &member[paren..];
                    // JVM method descriptors are `(params)return`.
                    if name.is_empty()
                        || !descriptor.starts_with('(')
                        || !descriptor.contains(')')
                    {
                        return Err(err(format!(
                            "{ctx}: invalid method target '{member}'"
                        )));
                    }
                    entries.push(AccessTransformerEntry {
                        line_no,
                        access,
                        final_modifier,
                        target_kind: AtTargetKind::Method,
                        owner_class,
                        member_name: Some(name.to_string()),
                        descriptor: Some(descriptor.to_string()),
                    });
                } else {
                    if member.is_empty() {
                        return Err(err(format!("{ctx}: empty member name")));
                    }
                    entries.push(AccessTransformerEntry {
                        line_no,
                        access,
                        final_modifier,
                        target_kind: AtTargetKind::Field,
                        owner_class,
                        member_name: Some(member.to_string()),
                        descriptor: None,
                    });
                }
            }
            _ => {
                return Err(err(format!(
                    "{ctx}: too many tokens (expected 'directive target [member]')"
                )))
            }
        }
    }
    Ok(entries)
}

// ===========================================================================
// 4. Fabric 1.21.1 `fabric:registry/sync/direct` wire channel
// ===========================================================================

/// Fabric API 1.21.1 direct registry-sync channel: a compact
/// `registry_id -> Vec<(entry_id, raw_id)>` map exchanged during
/// configuration, counterpart to NeoForge's `neoforge:frozen_registry*`
/// frames.
pub const CHANNEL_FABRIC_REGISTRY_SYNC_DIRECT: &str = "fabric:registry/sync/direct";

/// Largest accepted `fabric:registry/sync/direct` payload (16 MiB).
pub const MAX_FABRIC_REGISTRY_SYNC_BODY_BYTES: usize = 16 * 1024 * 1024;
/// Most registries one payload may carry (vanilla + modded is ≪ 100).
pub const MAX_FABRIC_REGISTRY_SYNC_REGISTRIES: usize = 1024;
/// Most `(entry_id, raw_id)` pairs per registry.
pub const MAX_FABRIC_REGISTRY_SYNC_ENTRIES: usize = 1 << 20;

/// Deterministic `fabric:registry/sync/direct` registry map: registries
/// are iterated in [`BTreeMap`] order and each entry list preserves the
/// sender's declared order.
pub type FabricRegistrySyncMap = BTreeMap<String, Vec<(String, u32)>>;

/// Encode a [`FabricRegistrySyncMap`] onto the wire: VarInt registry
/// count, then per registry a VarInt-length-prefixed UTF-8 registry id, a
/// VarInt entry count, and per entry the VarInt-length-prefixed entry id
/// followed by its VarInt `raw_id`. Big-endian throughout — Protocol 767
/// field order and VarInt packing are byte-exact deterministic.
pub fn encode_fabric_registry_sync_direct(
    map: &FabricRegistrySyncMap,
) -> Result<Vec<u8>, &'static str> {
    if map.len() > MAX_FABRIC_REGISTRY_SYNC_REGISTRIES {
        return Err("too many fabric:registry/sync/direct registries");
    }
    let mut out = Vec::with_capacity(map.len() * 64);
    write_varint(&mut out, map.len() as u32);
    for (registry_id, entries) in map {
        if registry_id.len() > MAX_REGISTRY_DATA_NAME_LEN {
            return Err("oversized fabric:registry/sync/direct registry id");
        }
        if entries.len() > MAX_FABRIC_REGISTRY_SYNC_ENTRIES {
            return Err("too many fabric:registry/sync/direct entries");
        }
        write_mc_string(&mut out, registry_id);
        write_varint(&mut out, entries.len() as u32);
        for (entry_id, raw_id) in entries {
            if entry_id.len() > MAX_REGISTRY_DATA_NAME_LEN {
                return Err("oversized fabric:registry/sync/direct entry id");
            }
            write_mc_string(&mut out, entry_id);
            write_varint(&mut out, *raw_id);
        }
    }
    if out.len() > MAX_FABRIC_REGISTRY_SYNC_BODY_BYTES {
        return Err("fabric:registry/sync/direct body exceeds 16 MiB");
    }
    Ok(out)
}

/// Decode a body produced by [`encode_fabric_registry_sync_direct`].
/// Fails closed (`None`) on any truncation, oversized count, oversized
/// string, or trailing garbage.
pub fn decode_fabric_registry_sync_direct(body: &[u8]) -> Option<FabricRegistrySyncMap> {
    if body.is_empty() || body.len() > MAX_FABRIC_REGISTRY_SYNC_BODY_BYTES {
        return None;
    }
    let mut cursor = 0usize;
    let registry_count = read_varint(body, &mut cursor)? as usize;
    if registry_count > MAX_FABRIC_REGISTRY_SYNC_REGISTRIES
        || registry_count > (body.len() - cursor) / 3
    {
        // Each registry needs at minimum a length byte + id byte + count
        // byte — a count beyond a third of the remaining body is provably
        // malformed.
        return None;
    }
    let mut map = BTreeMap::new();
    for _ in 0..registry_count {
        let registry_id = read_mc_string(body, &mut cursor, MAX_REGISTRY_DATA_NAME_LEN)?;
        let count = read_varint(body, &mut cursor)? as usize;
        if count > MAX_FABRIC_REGISTRY_SYNC_ENTRIES || count > (body.len() - cursor) / 2 {
            // Minimum per entry is a length byte plus the raw_id byte.
            return None;
        }
        let mut entries = Vec::with_capacity(count.min(4096));
        for _ in 0..count {
            let entry_id = read_mc_string(body, &mut cursor, MAX_REGISTRY_DATA_NAME_LEN)?;
            let raw_id = read_varint(body, &mut cursor)?;
            entries.push((entry_id, raw_id));
        }
        map.insert(registry_id, entries);
    }
    (cursor == body.len()).then_some(map)
}

// ===========================================================================
// 5. Generic tag files — data/<ns>/tags/fluid (and legacy tags/fluids)
// ===========================================================================

/// Tag directory inside `data/<ns>/` for 1.21+ fluid tags.
pub const FLUID_TAG_DIR: &str = "tags/fluid";
/// Pre-1.21 singular layout still shipped by older packs.
pub const LEGACY_FLUID_TAG_DIR: &str = "tags/fluids";

/// One `values` entry in a vanilla-style tag file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TagFileEntry {
    /// A required member — resolution fails when the target is absent.
    Required(TagRef),
    /// `"required": false` — silently skipped when the target is absent.
    Optional(TagRef),
}

/// What a tag `values` entry points at.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum TagRef {
    /// A concrete registry object id (`ns:id`).
    Id(String),
    /// A nested tag reference (`#ns:tag`, stored without the `#`).
    NestedTag(String),
}

/// A parsed vanilla `data/<ns>/tags/<kind>/<path>.json` file.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TagFile {
    /// `replace: true` discards earlier data for this tag (load order).
    pub replace: bool,
    pub entries: Vec<TagFileEntry>,
}

fn parse_tag_ref(text: &str) -> TagRef {
    match text.strip_prefix('#') {
        Some(nested) => TagRef::NestedTag(nested.to_string()),
        None => TagRef::Id(text.to_string()),
    }
}

/// Parse a vanilla-style tag file (`replace`, `values` of string or
/// `{"id": ..., "required": bool}` entries). Shared by fluid tags and
/// [`ConventionalTagRegistry`] item-tag merging.
pub fn parse_tag_file_json(
    bytes: &[u8],
    ctx: &str,
) -> Result<TagFile, LoaderExtensionError> {
    let root = parse_json(bytes).map_err(|e| {
        err(format!(
            "{ctx}: {}",
            match e {
                crate::data_loader::DataLoadError::Json(m)
                | crate::data_loader::DataLoadError::Io(m)
                | crate::data_loader::DataLoadError::Zip(m) => m,
            }
        ))
    })?;
    let obj = root
        .as_object()
        .ok_or_else(|| err(format!("{ctx}: root must be an object")))?;
    let replace = obj.get("replace").and_then(JsonValue::as_bool) == Some(true);
    let mut file = TagFile {
        replace,
        entries: Vec::new(),
    };
    if let Some(values) = obj.get("values") {
        let Some(items) = values.as_array() else {
            return Err(err(format!("{ctx}: 'values' must be an array")));
        };
        for item in items {
            match item {
                JsonValue::String(s) => {
                    file.entries.push(TagFileEntry::Required(parse_tag_ref(s)));
                }
                JsonValue::Object(o) => {
                    let Some(id) = o.get("id").and_then(JsonValue::as_str) else {
                        return Err(err(format!(
                            "{ctx}: values entry object is missing 'id'"
                        )));
                    };
                    let required = o
                        .get("required")
                        .and_then(JsonValue::as_bool)
                        .unwrap_or(true);
                    let target = parse_tag_ref(id);
                    file.entries.push(if required {
                        TagFileEntry::Required(target)
                    } else {
                        TagFileEntry::Optional(target)
                    });
                }
                _ => return Err(err(format!("{ctx}: bad values entry"))),
            }
        }
    }
    Ok(file)
}

/// Deepest `#tag` reference chain expanded during resolution.
pub const MAX_TAG_REF_DEPTH: usize = 64;

/// Tag-id (`ns:path`, path may contain `/` subdirectories) carried by a
/// `data/<ns>/tags/<dir>/<path>.json` member, or `None` when the member is
/// not a fluid tag file for either supported layout.
pub fn fluid_tag_id_for_path(rel_path: &str) -> Option<String> {
    let rest = rel_path.strip_prefix("data/")?;
    let (ns, rest) = rest.split_once('/')?;
    // Strip with the trailing '/' — "tags/fluid" alone is a prefix of the
    // legacy "tags/fluids" directory name.
    let stem = rest
        .strip_prefix("tags/fluid/")
        .or_else(|| rest.strip_prefix("tags/fluids/"))?
        .strip_suffix(".json")?;
    if ns.is_empty() || stem.is_empty() {
        return None;
    }
    Some(format!("{ns}:{stem}"))
}

/// Merged, resolvable `tags/fluid` registry across every loaded
/// datapack/JAR. Entries are keyed by tag id; `replace` files clobber
/// earlier data and `#tag` references resolve against the merged table
/// with a strict depth bound — fail closed on unresolved *required*
/// references, silently skipping optional ones (vanilla semantics).
#[derive(Clone, Debug, Default)]
pub struct FluidTagRegistry {
    files: BTreeMap<String, TagFile>,
}

impl FluidTagRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Load and merge one tag file under `tag_id` (`ns:path`).
    pub fn merge_tag_file(
        &mut self,
        tag_id: &str,
        file: TagFile,
    ) {
        let entry = self.files.entry(tag_id.to_string()).or_default();
        if file.replace {
            entry.entries.clear();
            entry.replace = true;
        }
        entry.entries.extend(file.entries);
    }

    /// Classify `rel_path` and, when it names a fluid tag member
    /// (`data/<ns>/tags/fluid/*.json` or legacy `tags/fluids/*.json`),
    /// parse + merge it. Returns `Ok(true)` when the member was consumed.
    pub fn ingest_member(
        &mut self,
        rel_path: &str,
        bytes: &[u8],
    ) -> Result<bool, LoaderExtensionError> {
        let Some(tag_id) = fluid_tag_id_for_path(rel_path) else {
            return Ok(false);
        };
        let file = parse_tag_file_json(bytes, rel_path)?;
        self.merge_tag_file(&tag_id, file);
        Ok(true)
    }

    /// Resolved member set for `tag_id` — literal ids plus transitively
    /// expanded `#tag` references. Required references to absent tags fail
    /// closed; optional ones skip silently. `required: false` *literal* ids
    /// are only resolvable against a real fluid registry, which this table
    /// does not carry — they are conservatively skipped rather than
    /// hallucinated; use [`resolve_with`](Self::resolve_with) to supply a
    /// presence predicate from the live registry.
    pub fn resolve(&self, tag_id: &str) -> Result<BTreeSet<String>, LoaderExtensionError> {
        self.resolve_with(tag_id, &|_| false)
    }

    /// [`resolve`](Self::resolve) consulting `known(id)` for
    /// `required: false` literal ids — an optional member resolves exactly
    /// when the live fluid registry reports it present.
    pub fn resolve_with(
        &self,
        tag_id: &str,
        known: &dyn Fn(&str) -> bool,
    ) -> Result<BTreeSet<String>, LoaderExtensionError> {
        let mut out = BTreeSet::new();
        let mut visiting = BTreeSet::new();
        self.resolve_into(tag_id, 0, &mut visiting, &mut out, known)
            .map(|_| out)
    }

    fn resolve_into(
        &self,
        tag_id: &str,
        depth: usize,
        visiting: &mut BTreeSet<String>,
        out: &mut BTreeSet<String>,
        known: &dyn Fn(&str) -> bool,
    ) -> Result<(), LoaderExtensionError> {
        if depth > MAX_TAG_REF_DEPTH {
            return Err(err(format!(
                "fluid tag '#{tag_id}': nested reference depth exceeds {MAX_TAG_REF_DEPTH}"
            )));
        }
        if !visiting.insert(tag_id.to_string()) {
            return Err(err(format!(
                "fluid tag '#{tag_id}': cyclic nested tag reference"
            )));
        }
        let Some(file) = self.files.get(tag_id) else {
            return Err(err(format!("fluid tag '#{tag_id}' is not defined")));
        };
        for entry in &file.entries {
            match entry {
                TagFileEntry::Required(TagRef::Id(id)) => {
                    out.insert(id.clone());
                }
                TagFileEntry::Optional(TagRef::Id(id)) => {
                    if known(id) {
                        out.insert(id.clone());
                    }
                }
                TagFileEntry::Required(TagRef::NestedTag(nested)) => {
                    if !self.files.contains_key(nested) {
                        return Err(err(format!(
                            "fluid tag '#{tag_id}': required nested tag '#{nested}' is absent"
                        )));
                    }
                    self.resolve_into(nested, depth + 1, visiting, out, known)?;
                }
                TagFileEntry::Optional(TagRef::NestedTag(nested)) => {
                    if self.files.contains_key(nested) {
                        self.resolve_into(nested, depth + 1, visiting, out, known)?;
                    }
                }
            }
        }
        visiting.remove(tag_id);
        Ok(())
    }

    /// Every resolved tag (`tag_id -> members`), failing on the first
    /// unresolvable required reference.
    pub fn resolve_all(&self) -> Result<BTreeMap<String, BTreeSet<String>>, LoaderExtensionError> {
        self.resolve_all_with(&|_| false)
    }

    /// [`resolve_all`](Self::resolve_all) with a presence predicate for
    /// optional literal ids — see [`resolve_with`](Self::resolve_with).
    pub fn resolve_all_with(
        &self,
        known: &dyn Fn(&str) -> bool,
    ) -> Result<BTreeMap<String, BTreeSet<String>>, LoaderExtensionError> {
        let mut out = BTreeMap::new();
        for tag_id in self.files.keys() {
            out.insert(tag_id.clone(), self.resolve_with(tag_id, known)?);
        }
        Ok(out)
    }

    /// Loaded tag ids in sorted order.
    pub fn tag_ids(&self) -> Vec<&str> {
        self.files.keys().map(String::as_str).collect()
    }
}

// ===========================================================================
// 6. NeoForge 1.21.1 data maps — data/<ns>/(neoforge/)data_maps/<reg>/<map>
// ===========================================================================

/// NeoForge's built-in 1.21.1 data map names (registry-relative paths
/// under `data_maps/<registry>/`). Custom maps with arbitrary names load
/// through the same generic path — this list exists so callers can tell
/// well-known maps from mod-defined ones.
pub const NEOFORGE_BUILTIN_DATA_MAPS: &[&str] = &[
    "compostables",
    "furnace_fuels",
    "oxidizables",
    "waxables",
    "raid_hero_gifts",
    "vibration_frequencies",
    "monster_room_mobs",
    "parrot_imitations",
];

/// `true` for the eight built-in 1.21.1 data map names.
pub fn is_builtin_data_map(map_name: &str) -> bool {
    NEOFORGE_BUILTIN_DATA_MAPS.contains(&map_name)
}

/// `(registry_id, map_name)` carried by a data-map member path:
/// `data/<ns>/data_maps/<registry-path>/<map>.json` and the legacy
/// `data/<ns>/neoforge/data_maps/<registry-path>/<map>.json` layouts. A
/// one-segment registry path maps onto `minecraft:` (`item` →
/// `minecraft:item`); two or more segments take the first as the registry
/// namespace (`mymod/item` → `mymod:item`, or `worldgen/biome` →
/// `worldgen:biome` for namespaced registries).
pub fn data_map_key_for_path(rel_path: &str) -> Option<(String, String)> {
    let rest = rel_path.strip_prefix("data/")?;
    let (_ns, rest) = rest.split_once('/')?;
    let sub = rest
        .strip_prefix("neoforge/data_maps/")
        .or_else(|| rest.strip_prefix("data_maps/"))?;
    let stem = sub.strip_suffix(".json")?;
    let mut segs: Vec<&str> = stem.split('/').collect();
    if segs.len() < 2 {
        return None;
    }
    let map = segs.pop()?;
    if map.is_empty() {
        return None;
    }
    let registry = if segs.len() == 1 {
        format!("minecraft:{}", segs[0])
    } else {
        let ns = segs.remove(0);
        format!("{ns}:{}", segs.join("/"))
    };
    Some((registry, map.to_string()))
}

/// A parsed `data_maps` document: `replace`, `values` (an object keyed by
/// entry id or `#tag` reference; a `?`-suffixed key marks an optional
/// entry) and `remove`.
#[derive(Clone, Debug, Default)]
pub struct DataMapFile {
    /// Target registry id (`minecraft:item`, `minecraft:game_event`, …).
    pub registry: String,
    /// Map name (`compostables`, `mymod:custom_map`, …).
    pub map_id: String,
    pub replace: bool,
    /// Entry ids to delete before `values` apply.
    pub remove: Vec<String>,
    /// `values` normalized to `entry_key -> arbitrary JSON payload`;
    /// `?`-suffixed (optional) keys are stored stripped.
    pub values: BTreeMap<String, JsonValue>,
    /// Keys in `values` that were `?`-suffixed optional entries.
    pub optional_keys: BTreeSet<String>,
}

/// Parse one data-map JSON document. `registry`/`map_id` come from
/// [`data_map_key_for_path`] — the document itself does not carry them.
pub fn parse_data_map(
    bytes: &[u8],
    registry: &str,
    map_id: &str,
    ctx: &str,
) -> Result<DataMapFile, LoaderExtensionError> {
    let root = parse_json(bytes).map_err(|e| {
        err(format!(
            "{ctx}: {}",
            match e {
                crate::data_loader::DataLoadError::Json(m)
                | crate::data_loader::DataLoadError::Io(m)
                | crate::data_loader::DataLoadError::Zip(m) => m,
            }
        ))
    })?;
    let obj = root
        .as_object()
        .ok_or_else(|| err(format!("{ctx}: root must be an object")))?;
    let mut file = DataMapFile {
        registry: registry.to_string(),
        map_id: map_id.to_string(),
        replace: obj.get("replace").and_then(JsonValue::as_bool) == Some(true),
        remove: Vec::new(),
        values: BTreeMap::new(),
        optional_keys: BTreeSet::new(),
    };
    if let Some(remove) = obj.get("remove") {
        let Some(items) = remove.as_array() else {
            return Err(err(format!("{ctx}: 'remove' must be an array")));
        };
        for item in items {
            let Some(id) = item.as_str() else {
                return Err(err(format!("{ctx}: 'remove' entries must be strings")));
            };
            file.remove.push(id.to_string());
        }
    }
    if let Some(values) = obj.get("values") {
        match values {
            JsonValue::Object(map) => {
                for (key, payload) in map {
                    let (key, optional) = match key.strip_suffix('?') {
                        Some(k) => (k.to_string(), true),
                        None => (key.clone(), false),
                    };
                    if optional {
                        file.optional_keys.insert(key.clone());
                    }
                    file.values.insert(key, payload.clone());
                }
            }
            // Array-of-records form also seen in the wild:
            // [{"key": "id", "value": {...}}, ...]
            JsonValue::Array(items) => {
                for item in items {
                    let rec = item.as_object().ok_or_else(|| {
                        err(format!("{ctx}: bad 'values' record"))
                    })?;
                    let raw_key = rec
                        .get("key")
                        .or_else(|| rec.get("id"))
                        .and_then(JsonValue::as_str)
                        .ok_or_else(|| {
                            err(format!("{ctx}: 'values' record is missing 'key'"))
                        })?;
                    let (key, optional) = match raw_key.strip_suffix('?') {
                        Some(k) => (k.to_string(), true),
                        None => (raw_key.to_string(), false),
                    };
                    let payload = rec
                        .get("value")
                        .or_else(|| rec.get("data"))
                        .cloned()
                        .unwrap_or(JsonValue::Null);
                    if optional {
                        file.optional_keys.insert(key.clone());
                    }
                    file.values.insert(key, payload);
                }
            }
            _ => return Err(err(format!("{ctx}: 'values' must be an object or array"))),
        }
    }
    Ok(file)
}

/// Merged data-map table keyed by `(registry, map_id)`; `replace`
/// clears prior content, `remove` deletes, `values` upserts — the same
/// fold NeoForge applies across datapack load order.
#[derive(Clone, Debug, Default)]
pub struct DataMapRegistry {
    maps: BTreeMap<(String, String), BTreeMap<String, JsonValue>>,
}

impl DataMapRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Apply one parsed document.
    pub fn apply(&mut self, file: &DataMapFile) {
        let key = (file.registry.clone(), file.map_id.clone());
        let map = self.maps.entry(key).or_default();
        if file.replace {
            map.clear();
        }
        for removed in &file.remove {
            map.remove(removed);
        }
        for (k, v) in &file.values {
            map.insert(k.clone(), v.clone());
        }
    }

    /// Classify `rel_path`, parse + apply when it names a data-map member.
    /// Returns `Ok(true)` when the member was consumed.
    pub fn ingest_member(
        &mut self,
        rel_path: &str,
        bytes: &[u8],
    ) -> Result<bool, LoaderExtensionError> {
        let Some((registry, map_id)) = data_map_key_for_path(rel_path) else {
            return Ok(false);
        };
        let file = parse_data_map(bytes, &registry, &map_id, rel_path)?;
        self.apply(&file);
        Ok(true)
    }

    /// Merged contents of `(registry, map_id)`, if any document loaded.
    pub fn get(&self, registry: &str, map_id: &str) -> Option<&BTreeMap<String, JsonValue>> {
        self.maps
            .get(&(registry.to_string(), map_id.to_string()))
    }

    /// Every `(registry, map_id)` with content, in sorted order.
    pub fn keys(&self) -> Vec<&(String, String)> {
        self.maps.keys().collect()
    }
}

// ===========================================================================
// 7. NeoForge 1.21.1 biome modifiers — data/<ns>/neoforge/biome_modifier
// ===========================================================================

/// `type` of a `neoforge/biome_modifier` document.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BiomeModifierKind {
    /// `neoforge:add_features` — `features` + `step` required.
    AddFeatures,
    /// `neoforge:remove_features` — `features` (`step` optional = all).
    RemoveFeatures,
    /// `neoforge:add_spawns` — `spawners` object or array required.
    AddSpawns,
    /// `neoforge:remove_spawns` — `spawners` entity id or list required.
    RemoveSpawns,
    /// `neoforge:none` — marker, no fields required.
    None,
    /// Any other (mod-defined) `type` — kept generic, not rejected.
    Other(String),
}

/// `biomes` selector: one id, a `#tag` reference, or a list of either.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BiomeSelector {
    /// Selector terms — plain biome ids or `#`-prefixed tag ids (the `#`
    /// is preserved so consumers can tell the two apart).
    pub terms: Vec<String>,
}

/// A parsed `data/<ns>/neoforge/biome_modifier/<name>.json` document.
#[derive(Clone, Debug)]
pub struct BiomeModifier {
    /// `ns:name` derived from the member path.
    pub id: String,
    pub kind: BiomeModifierKind,
    pub biomes: BiomeSelector,
    /// Feature id list for `add_features`/`remove_features` (list-of-lists
    /// per-step form flattened into one ordered list).
    pub features: Vec<String>,
    /// `step` (`underground_ores`, `vegetal_decoration`, …) for feature ops.
    pub step: Option<String>,
    /// Raw `spawners` payloads — objects (`{"type": "ns:entity", ...}`)
    /// for `add_spawns`, entity ids for `remove_spawns`.
    pub spawners: Vec<JsonValue>,
}

fn parse_string_list(
    val: &JsonValue,
    ctx: &str,
    key: &str,
) -> Result<Vec<String>, LoaderExtensionError> {
    match val {
        JsonValue::String(s) => Ok(vec![s.clone()]),
        JsonValue::Array(items) => {
            let mut out = Vec::new();
            for item in items {
                // Feature lists may nest one level (per-step lists).
                match item {
                    JsonValue::String(s) => out.push(s.clone()),
                    JsonValue::Array(inner) => {
                        for s2 in inner {
                            let Some(s2) = s2.as_str() else {
                                return Err(err(format!(
                                    "{ctx}: '{key}' entries must be strings"
                                )));
                            };
                            out.push(s2.to_string());
                        }
                    }
                    _ => {
                        return Err(err(format!(
                            "{ctx}: '{key}' entries must be strings"
                        )))
                    }
                }
            }
            Ok(out)
        }
        _ => Err(err(format!("{ctx}: '{key}' must be a string or array"))),
    }
}

/// `ns:name` a `data/<ns>/neoforge/biome_modifier/<name>.json` member
/// carries (nested subdirectories map onto `ns:dir/name`), or `None`
/// when the member is not a biome-modifier document.
pub fn biome_modifier_id_for_path(rel_path: &str) -> Option<String> {
    let rest = rel_path.strip_prefix("data/")?;
    let (ns, rest) = rest.split_once('/')?;
    let sub = rest.strip_prefix("neoforge/biome_modifier/")?;
    let stem = sub.strip_suffix(".json")?;
    if ns.is_empty() || stem.is_empty() {
        return None;
    }
    Some(format!("{ns}:{stem}"))
}

/// Parse one `neoforge/biome_modifier` document, `id` derived from the
/// member path by [`biome_modifier_id_for_path`]. Required fields per
/// `type` are enforced fail-closed; unknown `type`s parse as
/// [`BiomeModifierKind::Other`] rather than being rejected (mods may
/// register custom modifier serializers).
pub fn parse_biome_modifier(
    id: &str,
    bytes: &[u8],
    ctx: &str,
) -> Result<BiomeModifier, LoaderExtensionError> {
    let root = parse_json(bytes).map_err(|e| {
        err(format!(
            "{ctx}: {}",
            match e {
                crate::data_loader::DataLoadError::Json(m)
                | crate::data_loader::DataLoadError::Io(m)
                | crate::data_loader::DataLoadError::Zip(m) => m,
            }
        ))
    })?;
    let obj = root
        .as_object()
        .ok_or_else(|| err(format!("{ctx}: root must be an object")))?;
    let kind = match obj.get("type").and_then(JsonValue::as_str) {
        Some("neoforge:add_features") => BiomeModifierKind::AddFeatures,
        Some("neoforge:remove_features") => BiomeModifierKind::RemoveFeatures,
        Some("neoforge:add_spawns") => BiomeModifierKind::AddSpawns,
        Some("neoforge:remove_spawns") => BiomeModifierKind::RemoveSpawns,
        Some("neoforge:none") | None => BiomeModifierKind::None,
        Some(other) => BiomeModifierKind::Other(other.to_string()),
    };

    let biomes = match obj.get("biomes") {
        Some(v) => BiomeSelector {
            terms: parse_string_list(v, ctx, "biomes")?,
        },
        None => BiomeSelector::default(),
    };
    let features = match obj.get("features") {
        Some(v) => parse_string_list(v, ctx, "features")?,
        None => Vec::new(),
    };
    let step = obj
        .get("step")
        .and_then(JsonValue::as_str)
        .map(str::to_string);
    let spawners = match obj.get("spawners") {
        Some(JsonValue::Array(items)) => items.clone(),
        Some(v) => vec![v.clone()],
        None => Vec::new(),
    };

    match kind {
        BiomeModifierKind::AddFeatures => {
            if features.is_empty() || step.is_none() {
                return Err(err(format!(
                    "{ctx}: neoforge:add_features requires 'features' and 'step'"
                )));
            }
        }
        BiomeModifierKind::RemoveFeatures => {
            if features.is_empty() {
                return Err(err(format!(
                    "{ctx}: neoforge:remove_features requires 'features'"
                )));
            }
        }
        BiomeModifierKind::AddSpawns | BiomeModifierKind::RemoveSpawns => {
            if spawners.is_empty() {
                return Err(err(format!(
                    "{ctx}: {} requires 'spawners'",
                    if kind == BiomeModifierKind::AddSpawns {
                        "neoforge:add_spawns"
                    } else {
                        "neoforge:remove_spawns"
                    }
                )));
            }
        }
        BiomeModifierKind::None | BiomeModifierKind::Other(_) => {}
    }
    if !matches!(kind, BiomeModifierKind::None) && biomes.terms.is_empty() {
        return Err(err(format!("{ctx}: 'biomes' selector is required")));
    }

    Ok(BiomeModifier {
        id: id.to_string(),
        kind,
        biomes,
        features,
        step,
        spawners,
    })
}

// ===========================================================================
// 8. Conventional `c:` tag registry — all ten 1.21.1 families
// ===========================================================================

/// The ten conventional common-tag families every 1.21.1 NeoForge and
/// Fabric mod expects under the `c:` namespace.
pub const CONVENTIONAL_TAG_FAMILIES: [&str; 10] = [
    "ores",
    "ingots",
    "raw_materials",
    "crops",
    "foods",
    "tools",
    "stones",
    "dyes",
    "gems",
    "storage_blocks",
];

/// Vanilla 1.21.1 baseline members for each [`CONVENTIONAL_TAG_FAMILIES`]
/// family — the item/block ids the vanilla game itself contributes. Mods
/// and datapacks merge on top of this baseline.
const VANILLA_BASELINE: [(&str, &[&str]); 10] = [
    (
        "c:ores",
        &[
            "minecraft:coal_ore",
            "minecraft:deepslate_coal_ore",
            "minecraft:iron_ore",
            "minecraft:deepslate_iron_ore",
            "minecraft:copper_ore",
            "minecraft:deepslate_copper_ore",
            "minecraft:gold_ore",
            "minecraft:deepslate_gold_ore",
            "minecraft:redstone_ore",
            "minecraft:deepslate_redstone_ore",
            "minecraft:emerald_ore",
            "minecraft:deepslate_emerald_ore",
            "minecraft:lapis_ore",
            "minecraft:deepslate_lapis_ore",
            "minecraft:diamond_ore",
            "minecraft:deepslate_diamond_ore",
            "minecraft:nether_gold_ore",
            "minecraft:nether_quartz_ore",
            "minecraft:ancient_debris",
        ],
    ),
    (
        "c:ingots",
        &[
            "minecraft:iron_ingot",
            "minecraft:copper_ingot",
            "minecraft:gold_ingot",
            "minecraft:netherite_ingot",
        ],
    ),
    (
        "c:raw_materials",
        &[
            "minecraft:raw_iron",
            "minecraft:raw_copper",
            "minecraft:raw_gold",
        ],
    ),
    (
        "c:crops",
        &[
            "minecraft:wheat",
            "minecraft:carrot",
            "minecraft:potato",
            "minecraft:beetroot",
            "minecraft:melon_slice",
            "minecraft:pumpkin",
            "minecraft:cocoa_beans",
            "minecraft:nether_wart",
            "minecraft:sugar_cane",
            "minecraft:cactus",
        ],
    ),
    (
        "c:foods",
        &[
            "minecraft:apple",
            "minecraft:baked_potato",
            "minecraft:beetroot",
            "minecraft:beetroot_soup",
            "minecraft:bread",
            "minecraft:carrot",
            "minecraft:chorus_fruit",
            "minecraft:cooked_beef",
            "minecraft:cooked_chicken",
            "minecraft:cooked_cod",
            "minecraft:cooked_mutton",
            "minecraft:cooked_porkchop",
            "minecraft:cooked_rabbit",
            "minecraft:cooked_salmon",
            "minecraft:cookie",
            "minecraft:dried_kelp",
            "minecraft:glow_berries",
            "minecraft:golden_apple",
            "minecraft:golden_carrot",
            "minecraft:melon_slice",
            "minecraft:mushroom_stew",
            "minecraft:potato",
            "minecraft:pumpkin_pie",
            "minecraft:rabbit_stew",
            "minecraft:sweet_berries",
        ],
    ),
    (
        "c:tools",
        &[
            "minecraft:wooden_sword",
            "minecraft:stone_sword",
            "minecraft:iron_sword",
            "minecraft:golden_sword",
            "minecraft:diamond_sword",
            "minecraft:netherite_sword",
            "minecraft:wooden_pickaxe",
            "minecraft:stone_pickaxe",
            "minecraft:iron_pickaxe",
            "minecraft:golden_pickaxe",
            "minecraft:diamond_pickaxe",
            "minecraft:netherite_pickaxe",
            "minecraft:wooden_axe",
            "minecraft:stone_axe",
            "minecraft:iron_axe",
            "minecraft:golden_axe",
            "minecraft:diamond_axe",
            "minecraft:netherite_axe",
            "minecraft:wooden_shovel",
            "minecraft:stone_shovel",
            "minecraft:iron_shovel",
            "minecraft:golden_shovel",
            "minecraft:diamond_shovel",
            "minecraft:netherite_shovel",
            "minecraft:wooden_hoe",
            "minecraft:stone_hoe",
            "minecraft:iron_hoe",
            "minecraft:golden_hoe",
            "minecraft:diamond_hoe",
            "minecraft:netherite_hoe",
            "minecraft:bow",
            "minecraft:crossbow",
            "minecraft:trident",
            "minecraft:mace",
            "minecraft:shears",
            "minecraft:flint_and_steel",
            "minecraft:fishing_rod",
            "minecraft:brush",
            "minecraft:shield",
        ],
    ),
    (
        "c:stones",
        &[
            "minecraft:stone",
            "minecraft:smooth_stone",
            "minecraft:cobblestone",
            "minecraft:mossy_cobblestone",
            "minecraft:stone_bricks",
            "minecraft:granite",
            "minecraft:diorite",
            "minecraft:andesite",
            "minecraft:deepslate",
            "minecraft:cobbled_deepslate",
            "minecraft:tuff",
            "minecraft:blackstone",
            "minecraft:basalt",
            "minecraft:smooth_basalt",
            "minecraft:netherrack",
            "minecraft:end_stone",
            "minecraft:sandstone",
            "minecraft:red_sandstone",
            "minecraft:prismarine",
            "minecraft:dripstone_block",
            "minecraft:calcite",
        ],
    ),
    (
        "c:dyes",
        &[
            "minecraft:white_dye",
            "minecraft:light_gray_dye",
            "minecraft:gray_dye",
            "minecraft:black_dye",
            "minecraft:brown_dye",
            "minecraft:red_dye",
            "minecraft:orange_dye",
            "minecraft:yellow_dye",
            "minecraft:lime_dye",
            "minecraft:green_dye",
            "minecraft:cyan_dye",
            "minecraft:light_blue_dye",
            "minecraft:blue_dye",
            "minecraft:purple_dye",
            "minecraft:magenta_dye",
            "minecraft:pink_dye",
        ],
    ),
    (
        "c:gems",
        &[
            "minecraft:diamond",
            "minecraft:emerald",
            "minecraft:lapis_lazuli",
            "minecraft:quartz",
            "minecraft:amethyst_shard",
            "minecraft:prismarine_shard",
            "minecraft:prismarine_crystals",
        ],
    ),
    (
        "c:storage_blocks",
        &[
            "minecraft:coal_block",
            "minecraft:iron_block",
            "minecraft:copper_block",
            "minecraft:gold_block",
            "minecraft:redstone_block",
            "minecraft:emerald_block",
            "minecraft:lapis_block",
            "minecraft:diamond_block",
            "minecraft:netherite_block",
            "minecraft:amethyst_block",
            "minecraft:quartz_block",
            "minecraft:raw_iron_block",
            "minecraft:raw_copper_block",
            "minecraft:raw_gold_block",
            "minecraft:hay_block",
            "minecraft:dried_kelp_block",
            "minecraft:nether_wart_block",
        ],
    ),
];

/// Canonicalize a tag id: legacy `forge:<path>` and `neoforge:<path>` ids
/// whose path is a conventional family (or `family/…` subpath) map onto
/// the `c:` equivalent; anything else is returned unchanged.
pub fn canonical_conventional_tag_id(id: &str) -> String {
    let Some((ns, path)) = id.split_once(':') else {
        return id.to_string();
    };
    if ns != "forge" && ns != "neoforge" {
        return id.to_string();
    }
    let family = path.split('/').next().unwrap_or("");
    if CONVENTIONAL_TAG_FAMILIES.contains(&family) {
        format!("c:{path}")
    } else {
        id.to_string()
    }
}

/// `true` when `tag_id` names a `c:` conventional family or
/// `family/sub` subtag (after alias canonicalization).
pub fn is_conventional_family(tag_id: &str) -> bool {
    let canon = canonical_conventional_tag_id(tag_id);
    let Some((ns, path)) = canon.split_once(':') else {
        return false;
    };
    ns == "c" && CONVENTIONAL_TAG_FAMILIES.contains(&path.split('/').next().unwrap_or(""))
}

/// Merged `c:` conventional-tag table: Vanilla 1.21.1 baseline plus
/// whatever datapack/modpack `tags/item` files contribute. Literal ids
/// land in `members`; `#` references are kept per-tag with their
/// required/optional bit and expanded on [`resolve`](Self::resolve)
/// against the whole registry (also through `forge:`/`neoforge:`
/// aliases) under [`MAX_TAG_REF_DEPTH`].
#[derive(Clone, Debug, Default)]
pub struct ConventionalTagRegistry {
    members: BTreeMap<String, BTreeSet<String>>,
    refs: BTreeMap<String, BTreeMap<String, bool>>,
}

impl ConventionalTagRegistry {
    /// Empty registry (no baseline).
    pub fn new() -> Self {
        Self::default()
    }

    /// Registry seeded with the exact Vanilla 1.21.1 baseline member sets
    /// for all ten `c:` families.
    pub fn vanilla_1_21_1_baseline() -> Self {
        let mut reg = Self::new();
        for (tag, ids) in &VANILLA_BASELINE {
            reg.members.insert(
                (*tag).to_string(),
                ids.iter().map(|s| (*s).to_string()).collect(),
            );
        }
        reg
    }

    /// Merge a parsed tag file under `tag_id` (canonicalized through the
    /// `forge:`/`neoforge:` → `c:` alias). `replace` clears both the
    /// member set and pending references for that tag.
    pub fn merge_tag_file(&mut self, tag_id: &str, file: &TagFile) {
        let canon = canonical_conventional_tag_id(tag_id);
        let members = self.members.entry(canon.clone()).or_default();
        let refs = self.refs.entry(canon).or_default();
        if file.replace {
            members.clear();
            refs.clear();
        }
        for entry in &file.entries {
            match entry {
                TagFileEntry::Required(TagRef::Id(id))
                | TagFileEntry::Optional(TagRef::Id(id)) => {
                    members.insert(id.clone());
                }
                TagFileEntry::Required(TagRef::NestedTag(nested)) => {
                    refs.insert(canonical_conventional_tag_id(nested), true);
                }
                TagFileEntry::Optional(TagRef::NestedTag(nested)) => {
                    refs.entry(canonical_conventional_tag_id(nested))
                        .or_insert(false);
                }
            }
        }
    }

    /// Classify `rel_path` as `data/<ns>/tags/item/<rest>.json` and merge
    /// it when it names a `c:` conventional tag (or legacy
    /// `forge:`/`neoforge:` alias of one). Returns `Ok(true)` when
    /// consumed.
    pub fn ingest_item_tag_member(
        &mut self,
        rel_path: &str,
        bytes: &[u8],
    ) -> Result<bool, LoaderExtensionError> {
        let Some(rest) = rel_path.strip_prefix("data/") else {
            return Ok(false);
        };
        let Some((ns, rest)) = rest.split_once('/') else {
            return Ok(false);
        };
        let Some(sub) = rest
            .strip_prefix("tags/item/")
            .or_else(|| rest.strip_prefix("tags/items/"))
        else {
            return Ok(false);
        };
        let Some(stem) = sub.strip_suffix(".json") else {
            return Ok(false);
        };
        let tag_id = format!("{ns}:{stem}");
        if !is_conventional_family(&tag_id) {
            return Ok(false);
        }
        let file = parse_tag_file_json(bytes, rel_path)?;
        self.merge_tag_file(&tag_id, &file);
        Ok(true)
    }

    /// Direct member set for a tag (baseline + merged literals), alias
    /// canonicalized. `None` when the tag has no content at all.
    pub fn members(&self, tag_id: &str) -> Option<&BTreeSet<String>> {
        self.members.get(&canonical_conventional_tag_id(tag_id))
    }

    /// Fully resolved member set: direct members plus transitively
    /// expanded `#` references. A *required* reference to a tag with no
    /// content fails closed; optional references skip silently.
    pub fn resolve(&self, tag_id: &str) -> Result<BTreeSet<String>, LoaderExtensionError> {
        let mut out = BTreeSet::new();
        let mut visiting = BTreeSet::new();
        self.resolve_into(&canonical_conventional_tag_id(tag_id), 0, &mut visiting, &mut out)?;
        Ok(out)
    }

    fn resolve_into(
        &self,
        tag_id: &str,
        depth: usize,
        visiting: &mut BTreeSet<String>,
        out: &mut BTreeSet<String>,
    ) -> Result<(), LoaderExtensionError> {
        if depth > MAX_TAG_REF_DEPTH {
            return Err(err(format!(
                "conventional tag '#{tag_id}': nested reference depth exceeds {MAX_TAG_REF_DEPTH}"
            )));
        }
        if !visiting.insert(tag_id.to_string()) {
            return Err(err(format!(
                "conventional tag '#{tag_id}': cyclic nested tag reference"
            )));
        }
        if let Some(members) = self.members.get(tag_id) {
            out.extend(members.iter().cloned());
        }
        if let Some(refs) = self.refs.get(tag_id) {
            for (nested, required) in refs {
                let known = self.members.contains_key(nested) || self.refs.contains_key(nested);
                if !known {
                    if *required {
                        return Err(err(format!(
                            "conventional tag '#{tag_id}': required nested tag '#{nested}' is absent"
                        )));
                    }
                    continue;
                }
                self.resolve_into(nested, depth + 1, visiting, out)?;
            }
        }
        visiting.remove(tag_id);
        Ok(())
    }

    /// The ten conventional family ids.
    pub fn families(&self) -> impl Iterator<Item = String> + '_ {
        CONVENTIONAL_TAG_FAMILIES
            .iter()
            .map(|f| format!("c:{f}"))
    }
}

// ===========================================================================
// 9. Combined JAR extension scan (quilt + jarjar + accesstransformer)
// ===========================================================================

/// Everything [`crate::ecosystem_compat::scan_mod_jar`] collects beyond
/// the primary loader manifest — Quilt metadata, the bounded Jar-in-Jar
/// report, and any Access Transformer directives.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct JarExtensionReport {
    /// `quilt.mod.json` contents when the JAR is a Quilt mod. The primary
    /// `loader` field still reports the Fabric-family ecosystem — this
    /// field carries the Quilt-specific provenance (group, Quilt-style
    /// `depends` containers).
    pub quilt: Option<QuiltMeta>,
    /// Bounded recursive `META-INF/jarjar/metadata.json` scan — empty
    /// when the JAR ships no embedded JARs.
    pub jar_in_jar: JarInJarScan,
    /// `META-INF/accesstransformer.cfg` directives, in file order.
    pub access_transformers: Vec<AccessTransformerEntry>,
}

impl JarExtensionReport {
    /// `true` when any AT target needs JVM-bridge delegation (class-level
    /// directives or `+f` mutations) — the tier classifier treats the JAR
    /// as beyond the member-dispatch table's reach.
    pub fn requires_jvm_bridge(&self) -> bool {
        !self.jar_in_jar.declared.is_empty()
            || self
                .access_transformers
                .iter()
                .any(AccessTransformerEntry::requires_jvm_bridge)
    }
}

pub(crate) fn inspect_jar_extensions_with(
    jar_bytes: &[u8],
    display: &str,
    entries: &[ZipMemberEntry],
) -> Result<JarExtensionReport, LoaderExtensionError> {
    let mut report = JarExtensionReport::default();

    if let Some(entry) = entries.iter().find(|e| e.name == QUILT_MOD_JSON) {
        let bytes = modpack::read_zip_member(jar_bytes, entry, display)
            .map_err(|e| err(format!("{display}: {}", e.reason())))?;
        report.quilt = Some(
            parse_quilt_mod_json(&bytes)
                .map_err(|e| err(format!("{display}: {}", e.reason())))?,
        );
    }

    report.jar_in_jar =
        inspect_jar_in_jar(jar_bytes, display)?;

    for path in ACCESS_TRANSFORMER_PATHS {
        if let Some(entry) = entries.iter().find(|e| e.name == *path) {
            let bytes = modpack::read_zip_member(jar_bytes, entry, display)
                .map_err(|e| err(format!("{display}: {}", e.reason())))?;
            report.access_transformers = parse_access_transformer_cfg(&bytes)
                .map_err(|e| err(format!("{display}: {}", e.reason())))?;
            break;
        }
    }

    Ok(report)
}

/// Scan one JAR's extension surfaces: `quilt.mod.json`, the bounded
/// recursive `META-INF/jarjar/` tree, and `accesstransformer.cfg` — all
/// fail-closed. [`crate::ecosystem_compat::scan_mod_jar`] calls this on
/// every JAR and stores the report on the descriptor; it is also directly
/// usable when only the extension surface matters.
pub fn inspect_jar_extensions(
    jar_bytes: &[u8],
    display: &str,
) -> Result<JarExtensionReport, LoaderExtensionError> {
    let entries = modpack::zip_member_list(jar_bytes, display)
        .map_err(|e| err(format!("{display}: {}", e.reason())))?;
    inspect_jar_extensions_with(jar_bytes, display, &entries)
}
