// SPDX-License-Identifier: LGPL-3.0-or-later

//! Create-typed recipe view layered over [`mod_adapter_core::data_loader`].
//!
//! The JAR/ZIP entry scan is done locally so real production mod JARs —
//! where ~100% of entries are Deflate-compressed (`method == 8`) — load
//! natively: Stored entries pass through verbatim and Deflate entries are
//! inflated with [`megu_core::zip_archive::inflate_raw_rfc1951`], then
//! the decoded payloads are repacked as a stored-only archive and ingested
//! by [`mod_adapter_core::data_loader::load_from_stored_zip`] so recipes,
//! tags and blockstates share one parse path.

use std::collections::BTreeMap;
use std::path::Path;

use megu_core::zip_archive::{inflate_raw_rfc1951, validate_entry_name};
use mod_adapter_core::data_loader::{
    load_from_directory as load_mod_directory, load_from_stored_zip as load_mod_zip,
    parse_json, parse_mod_recipe_json, JsonValue, LoadedModData, ParsedModRecipe,
};
pub use mod_adapter_core::data_loader::{DataLoadError, ItemFluidIdTable};
use mod_offload::recipe_processor::{Recipe, Resource, ResourceKind, Stack};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum CreateRecipeKind {
    Pressing,
    Mixing,
    Milling,
    Crushing,
    Cutting,
    Compacting,
    Splashing,
    Haunting,
    Filling,
    Emptying,
    Deploying,
    ItemApplication,
    SandpaperPolishing,
    MechanicalCrafting,
    SequencedAssembly,
    ToolboxDyeing,
    ItemCopying,
}

impl CreateRecipeKind {
    pub fn from_type_str(value: &str) -> Option<Self> {
        let (namespace, path) = value.split_once(':')?;
        // Create's standard processing serializers are reused by third-party
        // addons under their own namespaces. Only the serializer path is
        // fixed; a valid namespace must still be present.
        if namespace.is_empty()
            || !namespace
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"_.-".contains(&b))
        {
            return None;
        }
        match path {
            "pressing" => Some(Self::Pressing),
            "mixing" => Some(Self::Mixing),
            "milling" => Some(Self::Milling),
            "crushing" => Some(Self::Crushing),
            "cutting" => Some(Self::Cutting),
            "compacting" => Some(Self::Compacting),
            "splashing" => Some(Self::Splashing),
            "haunting" => Some(Self::Haunting),
            "filling" => Some(Self::Filling),
            "emptying" => Some(Self::Emptying),
            "deploying" => Some(Self::Deploying),
            "item_application" => Some(Self::ItemApplication),
            "sandpaper_polishing" => Some(Self::SandpaperPolishing),
            "mechanical_crafting" => Some(Self::MechanicalCrafting),
            "sequenced_assembly" => Some(Self::SequencedAssembly),
            "toolbox_dyeing" => Some(Self::ToolboxDyeing),
            "item_copying" => Some(Self::ItemCopying),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParsedCreateRecipe {
    pub name: String,
    pub kind: CreateRecipeKind,
    pub recipe: Recipe,
}

#[derive(Clone, Debug, Default)]
pub struct LoadedCreateData {
    pub recipes: Vec<ParsedCreateRecipe>,
    pub tags: BTreeMap<String, Vec<String>>,
    /// Declared blockstate property tables per block id (see
    /// [`mod_adapter_core::data_loader::LoadedModData::blockstates`]).
    pub blockstates: BTreeMap<String, Vec<mod_adapter_core::blockstates::PropertyDef>>,
    pub skipped_compressed_zip_entries: usize,
}

/// Hard bounds for the raw-entry JAR scan backing [`load_from_stored_zip`]
/// and [`crate::addon_profile::discover_addon_from_jar`]: at most this many
/// local-file entries walked or matched, this many total *uncompressed*
/// payload bytes held at once, and this many uncompressed bytes per entry
/// (the JSON parser's own 4 MiB cap applies again later). Sized so a real
/// production modpack member like `create-1.21.1-6.0.10.jar` (12,536
/// Deflate entries, ~40 MiB uncompressed) loads with zero skips while ZIP
/// bombs still fail closed.
pub(crate) const MAX_JAR_ENTRIES: usize = 32_768;
pub(crate) const MAX_JAR_TOTAL_UNCOMPRESSED: usize = 128 * 1024 * 1024;
pub(crate) const MAX_JAR_ENTRY_BYTES: usize = 8 * 1024 * 1024;

const ZIP_LOCAL_HEADER_SIG: u32 = 0x0403_4b50;
const ZIP_METHOD_STORED: u16 = 0;
const ZIP_METHOD_DEFLATE: u16 = 8;

/// Decoded JAR members collected by [`scan_jar_entries`], sorted by name.
#[derive(Clone, Debug)]
pub(crate) struct ScannedJarEntries {
    /// `(entry name, uncompressed payload)` for every member matching the
    /// scan filter, in sorted-path order.
    pub entries: Vec<(String, Vec<u8>)>,
    /// Matching entries whose compression method is neither Stored nor
    /// Deflate and therefore could not be decoded (soft warning, never a
    /// silently ignored decode failure).
    pub skipped_compressed: usize,
}

/// Whether a JAR member name is mod datapack/asset JSON the universal
/// loader ingests (`data/**` or `assets/**`, `.json` suffix) — the same
/// filter [`mod_adapter_core::data_loader::load_from_stored_zip`] applies.
pub(crate) fn is_mod_data_entry(name: &str) -> bool {
    name.ends_with(".json") && (name.starts_with("data/") || name.starts_with("assets/"))
}

/// Bounded local-header JAR scan decoding Stored and Deflate entries whose
/// names pass `wanted`. Mirrors the fail-closed contract of
/// [`mod_adapter_core::data_loader::load_from_stored_zip`]: data-descriptor
/// streaming entries, truncated records, non-UTF8 or traversal-suspicious
/// names, unsupported methods on *matching* entries are counted (other
/// methods are skipped), and entry/payload budgets are hard errors.
pub(crate) fn scan_jar_entries(
    jar: &[u8],
    wanted: impl Fn(&str) -> bool,
) -> Result<ScannedJarEntries, DataLoadError> {
    let mut entries: Vec<(String, Vec<u8>)> = Vec::new();
    let mut skipped_compressed = 0usize;
    let mut payload_bytes = 0usize;
    let mut total_entries = 0usize;
    let mut cursor = 0usize;

    while cursor + 4 <= jar.len() {
        let sig = u32::from_le_bytes([
            jar[cursor],
            jar[cursor + 1],
            jar[cursor + 2],
            jar[cursor + 3],
        ]);
        if sig != ZIP_LOCAL_HEADER_SIG {
            break;
        }
        total_entries += 1;
        if total_entries > MAX_JAR_ENTRIES {
            return Err(DataLoadError::Zip(format!(
                "JAR declares more than {MAX_JAR_ENTRIES} entries"
            )));
        }
        if cursor + 30 > jar.len() {
            return Err(DataLoadError::Zip("truncated local file header".into()));
        }
        let flags = u16::from_le_bytes([jar[cursor + 6], jar[cursor + 7]]);
        if flags & 0x08 != 0 {
            return Err(DataLoadError::Zip(
                "data-descriptor streaming ZIP entries are unsupported".into(),
            ));
        }
        let method = u16::from_le_bytes([jar[cursor + 8], jar[cursor + 9]]);
        let comp_size = u32::from_le_bytes([
            jar[cursor + 18],
            jar[cursor + 19],
            jar[cursor + 20],
            jar[cursor + 21],
        ]) as usize;
        let uncomp_size = u32::from_le_bytes([
            jar[cursor + 22],
            jar[cursor + 23],
            jar[cursor + 24],
            jar[cursor + 25],
        ]) as usize;
        let name_len = u16::from_le_bytes([jar[cursor + 26], jar[cursor + 27]]) as usize;
        let extra_len = u16::from_le_bytes([jar[cursor + 28], jar[cursor + 29]]) as usize;

        let name_start = cursor + 30;
        let data_start = name_start
            .checked_add(name_len)
            .and_then(|v| v.checked_add(extra_len))
            .ok_or_else(|| DataLoadError::Zip("header length overflow".into()))?;
        let data_end = data_start
            .checked_add(comp_size)
            .ok_or_else(|| DataLoadError::Zip("entry payload overflow".into()))?;
        if data_end > jar.len() {
            return Err(DataLoadError::Zip("truncated ZIP entry payload".into()));
        }
        let name = std::str::from_utf8(&jar[name_start..name_start + name_len])
            .map_err(|_| DataLoadError::Zip("non-UTF8 ZIP entry path".into()))?;
        if wanted(name) {
            validate_entry_name(name)
                .map_err(|e| DataLoadError::Zip(format!("invalid ZIP entry path: {e}")))?;
            let payload = match method {
                ZIP_METHOD_STORED => {
                    if comp_size > MAX_JAR_ENTRY_BYTES {
                        return Err(DataLoadError::Zip(format!(
                            "{name}: entry exceeds {MAX_JAR_ENTRY_BYTES} byte scan limit"
                        )));
                    }
                    jar[data_start..data_end].to_vec()
                }
                ZIP_METHOD_DEFLATE => {
                    if uncomp_size > MAX_JAR_ENTRY_BYTES {
                        return Err(DataLoadError::Zip(format!(
                            "{name}: deflated entry exceeds {MAX_JAR_ENTRY_BYTES} byte scan limit"
                        )));
                    }
                    let inflated =
                        inflate_raw_rfc1951(&jar[data_start..data_end], MAX_JAR_ENTRY_BYTES)
                            .map_err(|e| {
                                DataLoadError::Zip(format!("{name}: deflate decode failed: {e}"))
                            })?;
                    if inflated.len() != uncomp_size {
                        return Err(DataLoadError::Zip(format!(
                            "{name}: inflated size {} != declared {uncomp_size}",
                            inflated.len()
                        )));
                    }
                    inflated
                }
                _ => {
                    skipped_compressed += 1;
                    cursor = data_end;
                    continue;
                }
            };
            if entries.len() >= MAX_JAR_ENTRIES
                || payload_bytes + payload.len() > MAX_JAR_TOTAL_UNCOMPRESSED
            {
                return Err(DataLoadError::Zip(
                    "JAR scan exceeds entry/uncompressed-byte budget".into(),
                ));
            }
            payload_bytes += payload.len();
            entries.push((name.to_string(), payload));
        }
        cursor = data_end;
    }
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(ScannedJarEntries {
        entries,
        skipped_compressed,
    })
}

/// Repack decoded `(name, payload)` members as a stored-method archive so
/// [`load_mod_zip`]'s unchanged scanner can ingest them.
fn repack_as_stored_zip(entries: &[(String, Vec<u8>)]) -> Vec<u8> {
    let mut out = Vec::new();
    for (name, data) in entries {
        out.extend_from_slice(&ZIP_LOCAL_HEADER_SIG.to_le_bytes());
        out.extend_from_slice(&20u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(&(data.len() as u32).to_le_bytes());
        out.extend_from_slice(&(data.len() as u32).to_le_bytes());
        out.extend_from_slice(&(name.len() as u16).to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(name.as_bytes());
        out.extend_from_slice(data);
    }
    out
}

/// Decode a Stored- or Deflate-compressed JAR into the universal
/// [`LoadedModData`] view (any namespace's recipes, tags, blockstates).
/// Returns the already-scanned member list alongside so callers needing
/// raw payloads (recipe/kinetics discovery) do not scan twice.
pub(crate) fn load_mod_data_from_zip(
    jar: &[u8],
    ids: &mut ItemFluidIdTable,
) -> Result<(LoadedModData, ScannedJarEntries), DataLoadError> {
    let scanned = scan_jar_entries(jar, is_mod_data_entry)?;
    let mut data = load_mod_zip(&repack_as_stored_zip(&scanned.entries), ids)?;
    data.skipped_compressed_zip_entries = scanned.skipped_compressed;
    recover_grid_recipes(&scanned.entries, &mut data, ids)?;
    recover_curiosity_recipes(&scanned.entries, &mut data)?;
    Ok((data, scanned))
}

/// `<namespace>:mechanical_crafting` recipes declare `key`/`pattern`/`result`
/// instead of `ingredients`, so the universal [`parse_mod_recipe_json`]
/// shape check drops them. Recover them here by folding pattern-symbol
/// usage into input stacks, mirroring the generic parser's item/tag
/// semantics and its 100-tick default.
fn recover_grid_recipes(
    entries: &[(String, Vec<u8>)],
    data: &mut LoadedModData,
    ids: &mut ItemFluidIdTable,
) -> Result<(), DataLoadError> {
    for (entry_name, payload) in entries {
        let parts: Vec<&str> = entry_name.split('/').collect();
        if parts.len() < 4
            || parts[0] != "data"
            || (parts[2] != "recipe" && parts[2] != "recipes")
        {
            continue;
        }
        let stem = parts[3..].join("/");
        let stem = stem.strip_suffix(".json").unwrap_or(&stem);
        let name = format!("{}:{stem}", parts[1]);
        if data.recipes.iter().any(|r| r.name == name) {
            continue;
        }
        let root = parse_json(payload)?;
        let Some(obj) = root.as_object() else {
            continue;
        };
        let Some(recipe_type) = obj.get("type").and_then(JsonValue::as_str) else {
            continue;
        };
        if !matches!(
            CreateRecipeKind::from_type_str(recipe_type),
            Some(CreateRecipeKind::MechanicalCrafting)
        ) {
            continue;
        }
        let Some(recipe) = parse_grid_recipe(obj, ids) else {
            continue;
        };
        let next_id = data
            .recipes
            .iter()
            .map(|r| r.recipe.id)
            .max()
            .map_or(0, |id| id + 1);
        data.recipes.push(ParsedModRecipe {
            name,
            recipe_type: recipe_type.into(),
            recipe: Recipe { id: next_id, ..recipe },
        });
    }
    Ok(())
}

/// `create:toolbox_dyeing` and `create:item_copying` are Create "curiosity"
/// recipes: they declare only a `type` (plus `category`) with no
/// `ingredients`/`results`, so [`parse_mod_recipe_json`]'s non-empty-IO
/// shape check drops them before the recipe `type` is ever inspected.
/// Recover them as recognized-kind recipes with empty IO so a real Create
/// JAR loses zero recipe entries.
fn recover_curiosity_recipes(
    entries: &[(String, Vec<u8>)],
    data: &mut LoadedModData,
) -> Result<(), DataLoadError> {
    for (entry_name, payload) in entries {
        let parts: Vec<&str> = entry_name.split('/').collect();
        if parts.len() < 4
            || parts[0] != "data"
            || (parts[2] != "recipe" && parts[2] != "recipes")
        {
            continue;
        }
        let stem = parts[3..].join("/");
        let stem = stem.strip_suffix(".json").unwrap_or(&stem);
        let name = format!("{}:{stem}", parts[1]);
        if data.recipes.iter().any(|r| r.name == name) {
            continue;
        }
        let root = parse_json(payload)?;
        let Some(obj) = root.as_object() else {
            continue;
        };
        let Some(recipe_type) = obj.get("type").and_then(JsonValue::as_str) else {
            continue;
        };
        if !matches!(
            CreateRecipeKind::from_type_str(recipe_type),
            Some(CreateRecipeKind::ToolboxDyeing | CreateRecipeKind::ItemCopying)
        ) {
            continue;
        }
        let next_id = data
            .recipes
            .iter()
            .map(|r| r.recipe.id)
            .max()
            .map_or(0, |id| id + 1);
        data.recipes.push(ParsedModRecipe {
            name,
            recipe_type: recipe_type.into(),
            recipe: Recipe {
                id: next_id,
                base_ticks: 1,
                inputs: Vec::new(),
                outputs: Vec::new(),
            },
        });
    }
    Ok(())
}

/// Fold a `key`/`pattern`/`result` grid recipe into a flat [`Recipe`];
/// `None` when the grid declares no resolvable input or output.
fn parse_grid_recipe(
    obj: &std::collections::BTreeMap<String, JsonValue>,
    ids: &mut ItemFluidIdTable,
) -> Option<Recipe> {
    let key = obj.get("key").and_then(JsonValue::as_object)?;
    let pattern = obj.get("pattern").and_then(JsonValue::as_array)?;
    let mut usage = std::collections::BTreeMap::<String, u32>::new();
    for row in pattern {
        let JsonValue::String(row) = row else {
            continue;
        };
        for symbol in row.chars() {
            if symbol != ' ' {
                *usage.entry(symbol.to_string()).or_default() += 1;
            }
        }
    }
    let mut inputs = Vec::new();
    for (symbol, times) in usage {
        let operand = key.get(&symbol)?;
        let item_id = operand
            .as_str()
            .or_else(|| {
                operand.as_object().and_then(|o| {
                    o.get("item")
                        .or_else(|| o.get("id"))
                        .or_else(|| o.get("tag"))
                        .and_then(JsonValue::as_str)
                })
            })?;
        inputs.push(Stack {
            resource: Resource {
                kind: ResourceKind::Item,
                id: ids.intern_item(item_id),
            },
            amount: times,
        });
    }
    let mut outputs = Vec::new();
    let output_values: Vec<&JsonValue> = match obj.get("results").and_then(JsonValue::as_array) {
        Some(arr) => arr.iter().collect(),
        None => obj.get("result").into_iter().collect(),
    };
    for value in output_values {
        let item_id = value.as_str().or_else(|| {
            value.as_object().and_then(|o| {
                o.get("id")
                    .or_else(|| o.get("item"))
                    .and_then(JsonValue::as_str)
            })
        })?;
        let count = value
            .as_object()
            .and_then(|o| o.get("count"))
            .and_then(JsonValue::as_u64)
            .and_then(|v| u32::try_from(v).ok())
            .filter(|&v| v > 0)
            .unwrap_or(1);
        outputs.push(Stack {
            resource: Resource {
                kind: ResourceKind::Item,
                id: ids.intern_item(item_id),
            },
            amount: count,
        });
    }
    if inputs.is_empty() || outputs.is_empty() {
        return None;
    }
    let base_ticks = obj
        .get("processing_time")
        .or_else(|| obj.get("processingTime"))
        .and_then(JsonValue::as_u64)
        .and_then(|v| u32::try_from(v).ok())
        .filter(|&v| v > 0)
        .unwrap_or(100);
    Some(Recipe {
        id: 0,
        base_ticks,
        inputs,
        outputs,
    })
}

pub fn load_from_directory(
    root: &Path,
    ids: &mut ItemFluidIdTable,
) -> Result<LoadedCreateData, DataLoadError> {
    let mut data = load_mod_directory(root, ids)?;
    let recipe_files = collect_recipe_files(root);
    recover_grid_recipes(&recipe_files, &mut data, ids)?;
    recover_curiosity_recipes(&recipe_files, &mut data)?;
    Ok(filter_create_data(data))
}

/// Read `data/<namespace>/recipe(s)/**` payloads from a datapack directory so
/// [`recover_grid_recipes`] can apply the same JAR-parity fixups.
fn collect_recipe_files(root: &Path) -> Vec<(String, Vec<u8>)> {
    let mut out = Vec::new();
    if let Ok(namespaces) = std::fs::read_dir(root.join("data")) {
        for entry in namespaces.flatten() {
            if !entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                continue;
            }
            let namespace = entry.file_name().to_string_lossy().into_owned();
            for category in ["recipe", "recipes"] {
                let dir = entry.path().join(category);
                let prefix = format!("data/{namespace}/{category}");
                collect_recipe_dir(&dir, &dir, &prefix, &mut out);
            }
        }
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

fn collect_recipe_dir(dir: &Path, base: &Path, prefix: &str, out: &mut Vec<(String, Vec<u8>)>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        if kind.is_dir() {
            collect_recipe_dir(&path, base, prefix, out);
        } else if kind.is_file() && path.extension().is_some_and(|ext| ext == "json") {
            if let (Ok(rel), Ok(payload)) = (path.strip_prefix(base), std::fs::read(&path)) {
                out.push((
                    format!("{prefix}/{}", rel.to_string_lossy().replace('\\', "/")),
                    payload,
                ));
            }
        }
    }
}

/// Load Create recipes/tags/blockstates from a Stored *or* Deflate JAR.
/// Entries using other compression methods are counted in
/// [`LoadedCreateData::skipped_compressed_zip_entries`] — including zero
/// for every conforming production JAR.
pub fn load_from_stored_zip(
    bytes: &[u8],
    ids: &mut ItemFluidIdTable,
) -> Result<LoadedCreateData, DataLoadError> {
    let (data, _) = load_mod_data_from_zip(bytes, ids)?;
    Ok(filter_create_data(data))
}

pub fn parse_create_recipe_json(
    recipe_name: &str,
    recipe_id: u32,
    json_bytes: &[u8],
    ids: &mut ItemFluidIdTable,
) -> Result<Option<ParsedCreateRecipe>, DataLoadError> {
    let Some(parsed) = parse_mod_recipe_json(recipe_name, recipe_id, json_bytes, ids)? else {
        return Ok(None);
    };
    let Some(kind) = CreateRecipeKind::from_type_str(&parsed.recipe_type) else {
        return Ok(None);
    };
    Ok(Some(ParsedCreateRecipe {
        name: parsed.name,
        kind,
        recipe: parsed.recipe,
    }))
}

fn filter_create_data(raw: LoadedModData) -> LoadedCreateData {
    let recipes = raw
        .recipes
        .into_iter()
        .filter_map(|r| {
            let kind = CreateRecipeKind::from_type_str(&r.recipe_type)?;
            Some(ParsedCreateRecipe {
                name: r.name,
                kind,
                recipe: r.recipe,
            })
        })
        .collect();
    LoadedCreateData {
        recipes,
        tags: raw.tags,
        blockstates: raw.blockstates,
        skipped_compressed_zip_entries: raw.skipped_compressed_zip_entries,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build_stored_zip(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut out = Vec::new();
        for (name, data) in entries {
            out.extend_from_slice(&0x0403_4b50u32.to_le_bytes());
            out.extend_from_slice(&20u16.to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes());
            out.extend_from_slice(&0u32.to_le_bytes());
            out.extend_from_slice(&(data.len() as u32).to_le_bytes());
            out.extend_from_slice(&(data.len() as u32).to_le_bytes());
            out.extend_from_slice(&(name.len() as u16).to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes());
            out.extend_from_slice(name.as_bytes());
            out.extend_from_slice(data);
        }
        out
    }

    #[test]
    fn loads_pressing_and_filling_recipes_from_stored_jar() {
        let pressing = br#"{
          "type": "create:pressing",
          "ingredients": [{"item": "minecraft:iron_ingot"}],
          "results": [{"id": "create:iron_sheet", "count": 1}],
          "processing_time": 40
        }"#;
        let filling = br#"{
          "type": "create:filling",
          "ingredients": [
            {"item": "minecraft:bucket"},
            {"fluid": "minecraft:water", "amount": 1000}
          ],
          "results": [{"id": "minecraft:water_bucket", "count": 1}],
          "processing_time": 20
        }"#;
        let jar = build_stored_zip(&[
            ("data/create/recipe/pressing/iron_sheet.json", pressing),
            ("data/create/recipe/filling/water_bucket.json", filling),
        ]);
        let mut ids = ItemFluidIdTable::new();
        let loaded = load_from_stored_zip(&jar, &mut ids).unwrap();
        assert_eq!(loaded.recipes.len(), 2);
        assert_eq!(loaded.recipes[0].kind, CreateRecipeKind::Filling);
        assert_eq!(loaded.recipes[1].kind, CreateRecipeKind::Pressing);
    }
}
