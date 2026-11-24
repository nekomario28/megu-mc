// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later

//! Self-contained `.mmod` / dual-loader `.jar` packager.
//!
//! A pure-Rust **stored** (uncompressed, method 0) ZIP emitter with a fixed
//! 1980-01-01 DOS timestamp — byte-deterministic output with zero clock
//! reads — using the canonical [`megu_core::hash::crc32_ieee`] for entry
//! CRCs and [`megu_core::zip_archive::validate_entry_name`] for member-name
//! validation. [`verify_stored_zip`] walks the emitted bytes back through
//! EOCD → Central Directory → Local File Headers and re-checks every CRC,
//! and the test-suite additionally proves the bytes index and read through
//! [`megu_core::zip_archive::LazyZipIndex`], the loader's canonical reader.
//!
//! Layout ([`ModProject::export_mmod_bytes`]):
//!
//! ```text
//! megu-mod.toml                      native manifest (megu-mod/1)
//! megu-mod-bundle.json               strict megu-mod-bundle/1 document
//! data/<mod>/megu/registry.json      megu-mod-registry/1 extension records
//! data/<ns>/recipe/<id>.json         recipes
//! data/<ns>/tags/{item,block}/<id>.json   declared + membership tag files
//! data/<ns>/dimension_type/<id>.json dimension declarations
//! assets/<mod>/lang/<locale>.json    translations
//! assets/<mod>/blockstates/<id>.json block state definitions
//! ```
//!
//! [`ModProject::export_compat_jar_bytes`] additionally writes
//! `META-INF/neoforge.mods.toml` and `fabric.mod.json` — the explicit opt-in
//! for authors shipping one artifact to both Java loaders. Native `.mmod`
//! intentionally carries none of that.

use std::collections::BTreeMap;

use megu_core::hash::crc32_ieee;
use megu_core::zip_archive::validate_entry_name;

use crate::builders::{
    ItemResult, ModProject, RecipeKind, RecipeSpec, TagKind,
};
use crate::{json, toml_key, toml_string, SdkError};
use crate::{MAX_ARCHIVE_BYTES, MAX_ARCHIVE_ENTRIES};

/// The native `.mmod` manifest entry.
pub const MMOD_MANIFEST: &str = "megu-mod.toml";
/// The strict `megu-mod-bundle/1` document entry.
pub const BUNDLE_ENTRY: &str = "megu-mod-bundle.json";
/// The extended registry entry (`data/<mod>/megu/registry.json`).
pub const REGISTRY_ENTRY_SUFFIX: &str = "megu/registry.json";

// ---------------------------------------------------------------------------
// Stored-ZIP writer (deterministic, method 0, fixed DOS timestamp)
// ---------------------------------------------------------------------------

const SIG_LFH: u32 = 0x0403_4b50;
const SIG_CDE: u32 = 0x0201_4b50;
const SIG_EOCD: u32 = 0x0605_4b50;
const LFH_FIXED: usize = 30;
const CDE_FIXED: usize = 46;
const EOCD_LEN: usize = 22;
/// 1980-01-01 00:00:00 as (date, time) — deterministic bytes, no clock.
const DOS_DATE: u16 = (1 << 5) | 1;
const DOS_TIME: u16 = 0;
const METHOD_STORED: u16 = 0;

fn push_u16(out: &mut Vec<u8>, v: u16) {
    out.extend_from_slice(&v.to_le_bytes());
}

fn push_u32(out: &mut Vec<u8>, v: u32) {
    out.extend_from_slice(&v.to_le_bytes());
}

/// Emit `entries` (name → bytes, emitted in BTreeMap order) as a stored ZIP.
/// Fail-closed on entry-name violations, the entry/byte caps, and names or
/// offsets that would overflow the 16/32-bit ZIP fields.
pub(crate) fn pack_zip_stored(entries: &BTreeMap<String, Vec<u8>>) -> Result<Vec<u8>, SdkError> {
    if entries.len() > MAX_ARCHIVE_ENTRIES {
        return Err(SdkError::Archive(format!(
            "{} entries exceeds the {MAX_ARCHIVE_ENTRIES} member cap",
            entries.len()
        )));
    }
    let total_payload: usize = entries.values().map(Vec::len).sum();
    if total_payload > MAX_ARCHIVE_BYTES {
        return Err(SdkError::Archive(format!(
            "{total_payload} payload bytes exceeds the {MAX_ARCHIVE_BYTES} cap"
        )));
    }
    let mut out = Vec::with_capacity(total_payload + entries.len() * 128);
    let mut central = Vec::with_capacity(entries.len() * (CDE_FIXED + 48));
    for (name, data) in entries {
        validate_entry_name(name)
            .map_err(|e| SdkError::Archive(format!("entry {name:?}: {e}")))?;
        if name.len() > u16::MAX as usize {
            return Err(SdkError::Archive(format!("entry name {name:?} too long")));
        }
        if data.len() > u32::MAX as usize || out.len() > u32::MAX as usize {
            return Err(SdkError::Archive("archive exceeds the 4 GiB ZIP32 limit".to_string()));
        }
        let crc = crc32_ieee(data);
        let size = data.len() as u32;
        let local_off = out.len() as u32;

        // Local File Header.
        push_u32(&mut out, SIG_LFH);
        push_u16(&mut out, 20); // version needed
        push_u16(&mut out, 0); // flags
        push_u16(&mut out, METHOD_STORED);
        push_u16(&mut out, DOS_TIME);
        push_u16(&mut out, DOS_DATE);
        push_u32(&mut out, crc);
        push_u32(&mut out, size);
        push_u32(&mut out, size);
        push_u16(&mut out, name.len() as u16);
        push_u16(&mut out, 0); // extra len
        out.extend_from_slice(name.as_bytes());
        out.extend_from_slice(data);

        // Central Directory record.
        push_u32(&mut central, SIG_CDE);
        push_u16(&mut central, 20); // version made by
        push_u16(&mut central, 20); // version needed
        push_u16(&mut central, 0); // flags
        push_u16(&mut central, METHOD_STORED);
        push_u16(&mut central, DOS_TIME);
        push_u16(&mut central, DOS_DATE);
        push_u32(&mut central, crc);
        push_u32(&mut central, size);
        push_u32(&mut central, size);
        push_u16(&mut central, name.len() as u16);
        push_u16(&mut central, 0); // extra len
        push_u16(&mut central, 0); // comment len
        push_u16(&mut central, 0); // disk start
        push_u16(&mut central, 0); // internal attrs
        push_u32(&mut central, 0); // external attrs
        push_u32(&mut central, local_off);
        central.extend_from_slice(name.as_bytes());
    }
    let cd_offset = out.len() as u32;
    let cd_size = central.len() as u32;
    out.extend_from_slice(&central);
    push_u32(&mut out, SIG_EOCD);
    push_u16(&mut out, 0); // disk
    push_u16(&mut out, 0); // cd disk
    push_u16(&mut out, entries.len() as u16);
    push_u16(&mut out, entries.len() as u16);
    push_u32(&mut out, cd_size);
    push_u32(&mut out, cd_offset);
    push_u16(&mut out, 0); // comment len
    Ok(out)
}

/// One verified archive member returned by [`verify_stored_zip`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VerifiedEntry {
    /// Member name.
    pub name: String,
    /// CRC-32 (IEEE) declared in the Central Directory — verified against
    /// the recomputed payload checksum before being reported.
    pub crc32: u32,
    /// Uncompressed size.
    pub size: u32,
    /// Absolute offset of the member's payload bytes.
    pub data_offset: usize,
}

fn u16le(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}

fn u32le(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}

/// In-memory structural verification of a stored ZIP emitted by
/// [`pack_zip_stored`]: locates the EOCD, walks every Central Directory
/// record, re-checks each Local File Header signature and name, and
/// recomputes `crc32_ieee` over every payload. Returns the verified member
/// table on success — the slice-index role `megu-mod-bundle.json`
/// consumers play when reading an `.mmod` straight from memory.
///
/// # Errors
///
/// `SdkError::Archive` on any signature, size, method, CRC, or bound
/// violation — the verifier never yields partially-trusted data.
pub fn verify_stored_zip(bytes: &[u8]) -> Result<Vec<VerifiedEntry>, SdkError> {
    let err = |m: &str| SdkError::Archive(m.to_string());
    if bytes.len() < EOCD_LEN {
        return Err(err("archive too small for an EOCD record"));
    }
    // Scan the tail window for the EOCD signature (allowing a comment).
    let window = bytes.len().min(22 + u16::MAX as usize);
    let tail = &bytes[bytes.len() - window..];
    let mut eocd_pos = None;
    for pos in (0..=tail.len() - EOCD_LEN).rev() {
        if u32le(tail, pos) == SIG_EOCD
            && pos + EOCD_LEN + u16le(tail, pos + 20) as usize == tail.len()
        {
            eocd_pos = Some(bytes.len() - window + pos);
            break;
        }
    }
    let eocd = eocd_pos.ok_or_else(|| err("no EOCD record found"))?;
    let rec = &bytes[eocd..];
    if u16le(rec, 4) != 0 || u16le(rec, 6) != 0 || u16le(rec, 8) != u16le(rec, 10) {
        return Err(err("multi-disk archives are not supported"));
    }
    let count = u16le(rec, 10) as usize;
    let cd_size = u32le(rec, 12) as usize;
    let cd_off = u32le(rec, 16) as usize;
    if cd_off.checked_add(cd_size).is_none_or(|end| end > bytes.len()) {
        return Err(err("central directory outside archive"));
    }
    let mut entries = Vec::with_capacity(count);
    let mut seen = std::collections::BTreeSet::new();
    let mut p = cd_off;
    for _ in 0..count {
        if p + CDE_FIXED > bytes.len() || u32le(bytes, p) != SIG_CDE {
            return Err(err("bad central directory record"));
        }
        let method = u16le(bytes, p + 10);
        if method != METHOD_STORED {
            return Err(err("only stored (uncompressed) members are emitted"));
        }
        if u16le(bytes, p + 8) != 0 {
            return Err(err("flagged members are not supported"));
        }
        let crc = u32le(bytes, p + 16);
        let comp = u32le(bytes, p + 20) as usize;
        let uncomp = u32le(bytes, p + 24) as usize;
        if comp != uncomp {
            return Err(err("stored member size mismatch"));
        }
        let name_len = u16le(bytes, p + 28) as usize;
        let extra_len = u16le(bytes, p + 30) as usize;
        let comment_len = u16le(bytes, p + 32) as usize;
        let local_off = u32le(bytes, p + 42) as usize;
        let name_start = p + CDE_FIXED;
        let name_end = name_start
            .checked_add(name_len)
            .ok_or_else(|| err("member name overflows"))?;
        let name_bytes = bytes
            .get(name_start..name_end)
            .ok_or_else(|| err("member name truncated"))?;
        let name = std::str::from_utf8(name_bytes)
            .map_err(|_| err("member name is not UTF-8"))?
            .to_string();
        validate_entry_name(&name).map_err(|e| {
            SdkError::Archive(format!("entry {name:?} failed name validation: {e}"))
        })?;
        if !seen.insert(name.clone()) {
            return Err(err(&format!("duplicate member {name:?}")));
        }
        // Local File Header cross-check.
        if local_off + LFH_FIXED > bytes.len() || u32le(bytes, local_off) != SIG_LFH {
            return Err(err("bad local file header"));
        }
        let lname_len = u16le(bytes, local_off + 26) as usize;
        let lextra_len = u16le(bytes, local_off + 28) as usize;
        let data_off = local_off + LFH_FIXED + lname_len + lextra_len;
        let local_name = bytes
            .get(local_off + LFH_FIXED..local_off + LFH_FIXED + lname_len)
            .ok_or_else(|| err("local name truncated"))?;
        if local_name != name_bytes {
            return Err(err("local/central member name mismatch"));
        }
        let payload = bytes
            .get(data_off..data_off.checked_add(comp).ok_or_else(|| err("overflow"))?)
            .ok_or_else(|| err("member payload truncated"))?;
        if crc32_ieee(payload) != crc {
            return Err(err(&format!("CRC mismatch in member {name:?}")));
        }
        entries.push(VerifiedEntry {
            name,
            crc32: crc,
            size: uncomp as u32,
            data_offset: data_off,
        });
        p = name_end + extra_len + comment_len;
    }
    if p != cd_off + cd_size {
        return Err(err("central directory size mismatch"));
    }
    Ok(entries)
}

// ---------------------------------------------------------------------------
// Project → archive file set
// ---------------------------------------------------------------------------

fn ingredient_json(reference: &str) -> json::J {
    match reference.strip_prefix('#') {
        Some(tag) => json::obj(vec![("tag", json::s(tag))]),
        None => json::obj(vec![("item", json::s(reference))]),
    }
}

fn result_json(result: &ItemResult) -> json::J {
    let mut pairs = vec![("id", json::s(&result.item))];
    if result.count != 1 {
        pairs.push(("count", json::u(u64::from(result.count))));
    }
    json::obj(pairs)
}

fn recipe_json(recipe: &RecipeSpec, mod_ns: &str) -> String {
    let mut pairs: Vec<(&str, json::J)> = vec![("type", json::s(&recipe.recipe_type(mod_ns)))];
    match &recipe.kind {
        RecipeKind::Shaped {
            pattern,
            keys,
            result,
        } => {
            pairs.push((
                "pattern",
                json::arr(pattern.iter().map(|r| json::s(r)).collect()),
            ));
            pairs.push((
                "key",
                json::J::Obj(
                    keys.iter()
                        .map(|(c, v)| (c.to_string(), ingredient_json(v)))
                        .collect(),
                ),
            ));
            pairs.push(("result", result_json(result)));
        }
        RecipeKind::Shapeless {
            ingredients,
            result,
        } => {
            pairs.push((
                "ingredients",
                json::arr(ingredients.iter().map(|i| ingredient_json(i)).collect()),
            ));
            pairs.push(("result", result_json(result)));
        }
        RecipeKind::Smelting {
            input,
            result,
            experience,
            cook_ticks,
        } => {
            pairs.push(("ingredient", ingredient_json(input)));
            pairs.push(("result", result_json(result)));
            pairs.push(("experience", json::n(*experience)));
            pairs.push(("cookingtime", json::u(u64::from(*cook_ticks))));
        }
        RecipeKind::Pressing { input, result } => {
            pairs.push((
                "ingredients",
                json::arr(vec![ingredient_json(input)]),
            ));
            pairs.push(("results", json::arr(vec![result_json(result)])));
        }
        RecipeKind::AlloyForge {
            base,
            catalyst,
            element,
            heat_cost,
            result,
        } => {
            pairs.push((
                "ingredients",
                json::arr(vec![ingredient_json(base), ingredient_json(catalyst)]),
            ));
            pairs.push(("element", json::s(element)));
            pairs.push(("heat_cost", json::u(u64::from(*heat_cost))));
            pairs.push(("results", json::arr(vec![result_json(result)])));
        }
    }
    json::render(&json::obj(pairs))
}

fn tag_json(values: &[String], replace: bool) -> String {
    let mut pairs = vec![(
        "values",
        json::arr(values.iter().map(|v| json::s(v)).collect()),
    )];
    if replace {
        pairs.push(("replace", json::b(true)));
    }
    json::render(&json::obj(pairs))
}

fn dimension_json(spec: &crate::builders::DimensionSpec) -> String {
    json::render(&json::obj(vec![
        ("schema", json::s("megu-dimension/1")),
        ("min_y", json::i(i64::from(spec.min_y))),
        ("height", json::u(u64::from(spec.height))),
        ("has_skylight", json::b(spec.has_skylight)),
        ("coordinate_scale", json::n(spec.coordinate_scale)),
        ("ambient_light", json::n(spec.ambient_light)),
    ]))
}

fn blockstate_json(mod_id: &str, path: &str) -> String {
    json::render(&json::obj(vec![(
        "variants",
        json::J::Obj(
            [(
                String::new(),
                json::obj(vec![("model", json::s(&format!("{mod_id}:block/{path}")))]),
            )]
            .into_iter()
            .collect(),
        ),
    )]))
}

fn lang_json(entries: &BTreeMap<String, String>) -> String {
    json::render(&json::J::Obj(
        entries
            .iter()
            .map(|(k, v)| (k.clone(), json::s(v)))
            .collect(),
    ))
}

fn mmod_manifest(project: &ModProject) -> String {
    let mut out = String::with_capacity(256);
    out.push_str("schema = \"megu-mod/1\"\n");
    out.push_str(&format!("mod_id = {}\n", toml_string(&project.mod_id)));
    out.push_str(&format!("version = {}\n", toml_string(&project.version)));
    out.push_str(&format!(
        "display_name = {}\n",
        toml_string(&project.display_name)
    ));
    out.push_str(&format!(
        "mc_version_range = {}\n",
        toml_string(&project.mc_version_range)
    ));
    for dep in &project.dependencies {
        out.push_str("\n[[dependencies]]\n");
        out.push_str(&format!("mod_id = {}\n", toml_string(&dep.mod_id)));
        out.push_str(&format!(
            "version_range = {}\n",
            toml_string(&dep.version_range)
        ));
        out.push_str(&format!("required = {}\n", dep.required));
    }
    out
}

fn neoforge_mods_toml(project: &ModProject) -> String {
    let mut out = String::with_capacity(512);
    out.push_str("modLoader = \"javafml\"\n");
    out.push_str("loaderVersion = \"[1,)\"\n");
    out.push_str("license = \"LGPL-3.0-or-later\"\n\n");
    out.push_str("[[mods]]\n");
    out.push_str(&format!("modId = {}\n", toml_string(&project.mod_id)));
    out.push_str(&format!("version = {}\n", toml_string(&project.version)));
    out.push_str(&format!(
        "displayName = {}\n\n",
        toml_string(&project.display_name)
    ));
    let owner = toml_key(&project.mod_id);
    out.push_str(&format!("[[dependencies.{owner}]]\n"));
    out.push_str("modId = \"neoforge\"\n");
    out.push_str("type = \"required\"\n");
    out.push_str("versionRange = \"[21.1,)\"\n");
    out.push_str("ordering = \"NONE\"\n");
    out.push_str("side = \"BOTH\"\n\n");
    out.push_str(&format!("[[dependencies.{owner}]]\n"));
    out.push_str("modId = \"minecraft\"\n");
    out.push_str("type = \"required\"\n");
    out.push_str(&format!(
        "versionRange = {}\n",
        toml_string(&project.mc_version_range)
    ));
    out.push_str("ordering = \"NONE\"\n");
    out.push_str("side = \"BOTH\"\n");
    for dep in &project.dependencies {
        out.push_str(&format!("\n[[dependencies.{owner}]]\n"));
        out.push_str(&format!("modId = {}\n", toml_string(&dep.mod_id)));
        out.push_str(&format!(
            "type = {}\n",
            toml_string(if dep.required { "required" } else { "optional" })
        ));
        out.push_str(&format!("mandatory = {}\n", dep.required));
        out.push_str(&format!(
            "versionRange = {}\n",
            toml_string(&dep.version_range)
        ));
        out.push_str("ordering = \"NONE\"\n");
        out.push_str("side = \"BOTH\"\n");
    }
    out
}

fn fabric_mod_json(project: &ModProject) -> String {
    let mut depends: BTreeMap<String, json::J> = BTreeMap::new();
    depends.insert(
        "minecraft".to_string(),
        json::s(&project.mc_version_range),
    );
    for dep in &project.dependencies {
        if dep.required {
            depends.insert(dep.mod_id.clone(), json::s(&dep.version_range));
        }
    }
    let mut recommends: BTreeMap<String, json::J> = BTreeMap::new();
    for dep in &project.dependencies {
        if !dep.required {
            recommends.insert(dep.mod_id.clone(), json::s(&dep.version_range));
        }
    }
    let mut pairs: Vec<(&str, json::J)> = vec![
        ("schemaVersion", json::u(1)),
        ("id", json::s(&project.mod_id)),
        ("version", json::s(&project.version)),
        ("name", json::s(&project.display_name)),
        ("environment", json::s("*")),
        ("depends", json::J::Obj(depends)),
        (
            "custom",
            json::obj(vec![("megu:bundle", json::s(BUNDLE_ENTRY))]),
        ),
    ];
    if !recommends.is_empty() {
        pairs.push(("recommends", json::J::Obj(recommends)));
    }
    json::render(&json::obj(pairs))
}

/// Render the complete archive member table (name → bytes), merging
/// block/item tag memberships into the tag files and grouping every
/// data file under its declared namespace.
fn archive_files(project: &ModProject, compat: bool) -> Result<BTreeMap<String, Vec<u8>>, SdkError> {
    let mut files: BTreeMap<String, Vec<u8>> = BTreeMap::new();
    let mut put = |name: String, body: String| {
        files.insert(name, body.into_bytes());
    };

    put(MMOD_MANIFEST.to_string(), mmod_manifest(project));
    put(
        BUNDLE_ENTRY.to_string(),
        project.export_megu_bundle_json()?,
    );
    put(
        format!("data/{}/{REGISTRY_ENTRY_SUFFIX}", project.mod_id),
        json::render(&project.registry_ast()),
    );

    for recipe in &project.recipes {
        let (ns, path) = recipe
            .id
            .split_once(':')
            .ok_or_else(|| SdkError::Archive(format!("recipe id {:?} unqualified", recipe.id)))?;
        put(
            format!("data/{ns}/recipe/{path}.json"),
            recipe_json(recipe, &project.mod_id),
        );
    }

    // Merge declared tag files and membership tags (blocks → block tags,
    // items → item tags) keyed by (kind-dir, tag id).
    let mut tag_values: BTreeMap<(TagKind, String), Vec<String>> = BTreeMap::new();
    for spec in &project.tags {
        tag_values
            .entry((spec.kind, spec.id.clone()))
            .or_default()
            .extend(spec.values.iter().cloned());
    }
    for block in &project.blocks {
        for tag in &block.tags {
            tag_values
                .entry((TagKind::Block, tag.clone()))
                .or_default()
                .push(block.id.clone());
        }
    }
    for item in &project.items {
        for tag in &item.tags {
            tag_values
                .entry((TagKind::Item, tag.clone()))
                .or_default()
                .push(item.id.clone());
        }
    }
    let declared_replace: BTreeMap<(TagKind, String), bool> = project
        .tags
        .iter()
        .map(|t| ((t.kind, t.id.clone()), t.replace))
        .collect();
    for ((kind, tag_id), mut values) in tag_values {
        values.sort();
        values.dedup();
        let (ns, path) = tag_id
            .split_once(':')
            .ok_or_else(|| SdkError::Archive(format!("tag id {tag_id:?} unqualified")))?;
        let replace = declared_replace.get(&(kind, tag_id.clone())).copied().unwrap_or(false);
        put(
            format!("data/{ns}/tags/{}/{path}.json", kind.dir()),
            tag_json(&values, replace),
        );
    }

    for dim in &project.dimensions {
        let (ns, path) = dim
            .id
            .split_once(':')
            .ok_or_else(|| SdkError::Archive(format!("dimension id {:?} unqualified", dim.id)))?;
        put(
            format!("data/{ns}/dimension_type/{path}.json"),
            dimension_json(dim),
        );
    }

    for locale in &project.locales {
        put(
            format!("assets/{}/lang/{}.json", project.mod_id, locale.locale),
            lang_json(&locale.entries),
        );
    }

    for block in &project.blocks {
        let (ns, path) = block
            .id
            .split_once(':')
            .ok_or_else(|| SdkError::Archive(format!("block id {:?} unqualified", block.id)))?;
        if ns == project.mod_id {
            put(
                format!("assets/{ns}/blockstates/{path}.json"),
                blockstate_json(ns, path),
            );
        }
    }

    if compat {
        put(
            "META-INF/neoforge.mods.toml".to_string(),
            neoforge_mods_toml(project),
        );
        put("fabric.mod.json".to_string(), fabric_mod_json(project));
    }
    Ok(files)
}

impl ModProject {
    /// Pack the project into a deterministic `.mmod` archive: the native
    /// `megu-mod.toml` manifest, the strict `megu-mod-bundle.json`
    /// document, and every datapack/asset file — **no** Java loader
    /// metadata.
    ///
    /// # Errors
    ///
    /// `SdkError::Diagnostics` when the project fails validation;
    /// `SdkError::Archive` on emission bound violations.
    pub fn export_mmod_bytes(&self) -> Result<Vec<u8>, SdkError> {
        pack_zip_stored(&archive_files(self, false)?)
    }

    /// Pack the project into a dual-loader `.jar`: everything `.mmod`
    /// carries plus `META-INF/neoforge.mods.toml` and `fabric.mod.json` —
    /// an explicit opt-in for authors targeting both Java loaders.
    ///
    /// # Errors
    ///
    /// Same contract as [`export_mmod_bytes`](Self::export_mmod_bytes).
    pub fn export_compat_jar_bytes(&self) -> Result<Vec<u8>, SdkError> {
        pack_zip_stored(&archive_files(self, true)?)
    }

    /// The archive member table an `.mmod`/compat jar will contain —
    /// exposed for diagnostics and tests.
    pub fn archive_manifest(&self) -> Result<Vec<String>, SdkError> {
        Ok(archive_files(self, true)?.into_keys().collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::builders::test_support::sample_project;

    #[test]
    fn mmod_contains_the_native_layout_and_verifies() {
        let project = sample_project();
        let bytes = project.export_mmod_bytes().expect("pack .mmod");
        let entries = verify_stored_zip(&bytes).expect("verify .mmod");
        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        for required in [
            MMOD_MANIFEST,
            BUNDLE_ENTRY,
            "data/horizon_demo/megu/registry.json",
            "data/horizon_demo/recipe/brass_block.json",
            "data/c/tags/item/ingots/brass.json",
            "data/minecraft/tags/block/mineable/pickaxe.json",
            "data/horizon_demo/dimension_type/sky_realm.json",
            "assets/horizon_demo/lang/en_us.json",
            "assets/horizon_demo/lang/ja_jp.json",
            "assets/horizon_demo/blockstates/brass_block.json",
        ] {
            assert!(names.contains(&required), "missing {required} in {names:?}");
        }
        assert!(!names.iter().any(|n| n.starts_with("META-INF")));
        assert!(!names.contains(&"fabric.mod.json"));
    }

    #[test]
    fn compat_jar_carries_dual_loader_manifests() {
        let project = sample_project();
        let bytes = project.export_compat_jar_bytes().expect("pack jar");
        let entries = verify_stored_zip(&bytes).expect("verify jar");
        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        assert!(names.contains(&"META-INF/neoforge.mods.toml"));
        assert!(names.contains(&"fabric.mod.json"));
        // The native manifest + strict bundle are still inside.
        assert!(names.contains(&MMOD_MANIFEST));
        assert!(names.contains(&BUNDLE_ENTRY));
    }

    #[test]
    fn packing_is_byte_deterministic() {
        let project = sample_project();
        assert_eq!(
            project.export_mmod_bytes().unwrap(),
            project.export_mmod_bytes().unwrap()
        );
    }

    #[test]
    fn verifier_rejects_corruption() {
        let project = sample_project();
        let mut bytes = project.export_mmod_bytes().unwrap();
        // Flip a payload byte: the CRC check must catch it.
        let mid = bytes.len() / 2;
        bytes[mid] ^= 0xff;
        assert!(verify_stored_zip(&bytes).is_err());
    }

    #[test]
    fn emitted_recipe_json_matches_data_loader_shape() {
        let project = sample_project();
        let files = archive_files(&project, false).unwrap();
        let shaped = std::str::from_utf8(
            &files["data/horizon_demo/recipe/brass_block.json"],
        )
        .unwrap()
        .to_string();
        assert!(shaped.contains("\"type\": \"minecraft:crafting_shaped\""));
        assert!(shaped.contains("\"pattern\""));
        assert!(shaped.contains("\"result\""));
        let smelt = std::str::from_utf8(
            &files["data/horizon_demo/recipe/brass_ingot_smelting.json"],
        )
        .unwrap()
        .to_string();
        assert!(smelt.contains("\"type\": \"minecraft:smelting\""));
        assert!(smelt.contains("\"tag\": \"c:ores/brass\""));
        assert!(smelt.contains("\"cookingtime\": 200"));
        let alloy = std::str::from_utf8(
            &files["data/horizon_demo/recipe/storm_alloy.json"],
        )
        .unwrap()
        .to_string();
        assert!(alloy.contains("\"type\": \"horizon_demo:alloy_forge\""));
        assert!(alloy.contains("\"element\": \"horizon_demo:lightning\""));
        let tag = std::str::from_utf8(
            &files["data/minecraft/tags/block/mineable/pickaxe.json"],
        )
        .unwrap()
        .to_string();
        assert!(tag.contains("\"horizon_demo:brass_block\""));
        assert!(tag.contains("\"horizon_demo:sky_forge\""));
    }

    #[test]
    fn neoforge_and_fabric_manifests_are_wellformed() {
        let project = sample_project();
        let files = archive_files(&project, true).unwrap();
        let toml = std::str::from_utf8(&files["META-INF/neoforge.mods.toml"]).unwrap();
        assert!(toml.contains("modLoader = \"javafml\""));
        assert!(toml.contains("modId = \"horizon_demo\""));
        assert!(toml.contains("modId = \"minecraft\""));
        assert!(toml.contains("versionRange = \"[1.21.1,1.22)\""));
        assert!(toml.contains("[[dependencies.horizon_demo]]"));
        assert!(toml.contains("modId = \"create\""));
        assert!(toml.contains("type = \"optional\""));
        let fabric = std::str::from_utf8(&files["fabric.mod.json"]).unwrap();
        assert!(fabric.contains("\"schemaVersion\": 1"));
        assert!(fabric.contains("\"id\": \"horizon_demo\""));
        assert!(fabric.contains("\"minecraft\": \"[1.21.1,1.22)\""));
        // Optional deps go under `recommends`, not `depends`.
        assert!(fabric.contains("\"recommends\""));
        assert!(fabric.contains("\"create\": \"[0.5.1,)\""));
    }

    #[test]
    fn mmod_roundtrips_through_the_canonical_lazy_index() {
        // LazyZipIndex is file-backed, so prove interop on a temp file.
        let project = sample_project();
        let bytes = project.export_mmod_bytes().unwrap();
        let path = std::env::temp_dir().join(format!(
            "megu_mod_sdk_{}_roundtrip.mmod",
            std::process::id()
        ));
        std::fs::write(&path, &bytes).expect("write temp mmod");
        let index = megu_core::zip_archive::LazyZipIndex::open_all(&path)
            .expect("canonical index opens the archive");
        let names: Vec<&str> = index.entries().iter().map(|e| e.name.as_str()).collect();
        for required in [
            MMOD_MANIFEST,
            BUNDLE_ENTRY,
            "data/horizon_demo/megu/registry.json",
        ] {
            assert!(names.contains(&required), "missing {required}");
        }
        let bundle = index
            .read_entry(BUNDLE_ENTRY, 1 << 20)
            .expect("read bundle entry");
        assert_eq!(
            bundle,
            project.export_megu_bundle_json().unwrap().into_bytes()
        );
        let _ = std::fs::remove_file(&path);
    }
}
