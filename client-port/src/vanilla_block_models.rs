// SPDX-License-Identifier: LGPL-3.0-or-later

//! Bounded loader and lazy state-id cache for Java blockstate/model JSON.
//!
//! Layers are ordered from lowest to highest priority. Parsing and geometry
//! resolution are delegated to `model_baker`; this module maps vanilla asset
//! paths to those existing parsers and caches the final result per state id.

use std::collections::{BTreeMap, HashMap, HashSet};

use crate::model_baker::blockstate::{self, BlockStateDef};
use crate::model_baker::{
    BakedModelQuad, ModelError, RawModelJson, ResourcePackModelStack,
};

/// Maximum UTF-8 JSON document size accepted from one layer entry.
pub const MAX_BLOCK_MODEL_JSON_BYTES: usize = 1_048_576;
/// Maximum model documents parsed across all layers.
pub const MAX_BLOCK_MODEL_DOCUMENTS: usize = 16_384;
/// Maximum blockstate documents parsed across all layers.
pub const MAX_BLOCKSTATE_DOCUMENTS: usize = 8_192;

/// Parsed namespace, block name, and property pairs.
pub type ParsedBlockStateName = (String, String, Vec<(String, String)>);

/// Parses a canonical block state identifier into namespace, block path, and
/// ordered property pairs. An omitted namespace means `minecraft`.
pub fn parse_block_state_name(state: &str) -> Result<ParsedBlockStateName, ModelError> {
    let (identifier, properties) = match state.find('[') {
        Some(open) => {
            if !state.ends_with(']') {
                return Err(ModelError::BlockState("unterminated state properties".into()));
            }
            (&state[..open], Some(&state[open + 1..state.len() - 1]))
        }
        None => {
            if state.contains(']') {
                return Err(ModelError::BlockState("unexpected closing bracket".into()));
            }
            (state, None)
        }
    };
    let mut id_parts = identifier.split(':');
    let first = id_parts.next().unwrap_or_default();
    let (namespace, name) = match id_parts.next() {
        Some(name) if !name.is_empty() && id_parts.next().is_none() => (first, name),
        Some(_) => return Err(ModelError::BlockState("invalid block identifier".into())),
        None => ("minecraft", first),
    };
    if !valid_resource_part(namespace) || !valid_resource_part(name) {
        return Err(ModelError::BlockState("invalid block identifier".into()));
    }
    let mut parsed = Vec::new();
    let mut seen = HashSet::new();
    if let Some(properties) = properties {
        if properties.is_empty() {
            return Err(ModelError::BlockState("empty state properties".into()));
        }
        for property in properties.split(',') {
            let Some((key, value)) = property.split_once('=') else {
                return Err(ModelError::BlockState("property must be key=value".into()));
            };
            if key.is_empty() || value.is_empty() || !seen.insert(key) {
                return Err(ModelError::BlockState(
                    "empty or duplicate state property".into(),
                ));
            }
            parsed.push((key.to_owned(), value.to_owned()));
        }
    }
    Ok((namespace.to_owned(), name.to_owned(), parsed))
}

fn valid_resource_part(part: &str) -> bool {
    !part.is_empty()
        && part.bytes().all(|byte| {
            byte.is_ascii_lowercase()
                || byte.is_ascii_digit()
                || matches!(byte, b'_' | b'-' | b'.' | b'/')
        })
}

/// One highest-priority blockstate definition and the layered model stack.
#[derive(Clone, Debug, Default)]
pub struct VanillaBlockModelCatalog {
    blockstates: BTreeMap<String, BlockStateDef>,
    models: ResourcePackModelStack,
    loaded_blockstates: usize,
    loaded_models: usize,
}

impl VanillaBlockModelCatalog {
    /// Loads raw `(entry_path, utf8_json)` entries in low-to-high priority
    /// order. Invalid documents and entries beyond the hard limits are
    /// skipped; callers receive `None` for states that cannot be baked.
    pub fn new(layers: Vec<Vec<(String, String)>>) -> Self {
        let mut catalog = Self::default();
        let mut total_models = 0;
        let mut total_blockstates = 0;
        for layer in layers {
            let mut parsed_models = BTreeMap::new();
            for (entry, text) in layer {
                let Some((kind, id)) = asset_entry_id(&entry) else {
                    continue;
                };
                if text.len() > MAX_BLOCK_MODEL_JSON_BYTES {
                    continue;
                }
                match kind {
                    AssetKind::Model if total_models < MAX_BLOCK_MODEL_DOCUMENTS => {
                        total_models += 1;
                        if let Ok(model) = RawModelJson::parse(&text) {
                            parsed_models.insert(id, model);
                            catalog.loaded_models += 1;
                        }
                    }
                    AssetKind::Blockstate if total_blockstates < MAX_BLOCKSTATE_DOCUMENTS => {
                        total_blockstates += 1;
                        if let Ok(definition) = BlockStateDef::parse(&text) {
                            catalog.blockstates.insert(id, definition);
                        }
                    }
                    _ => {}
                }
            }
            catalog.models.push_pack(parsed_models);
        }
        catalog.loaded_blockstates = catalog.blockstates.len();
        catalog
    }

    /// Number of valid, currently selected blockstate definitions.
    pub fn loaded_blockstates(&self) -> usize {
        self.loaded_blockstates
    }

    /// Number of valid model documents accepted across all layers.
    pub fn loaded_models(&self) -> usize {
        self.loaded_models
    }

    fn bake_state(&self, state: &str) -> Option<BakedBlockStateMesh> {
        let (namespace, name, parsed_props) = parse_block_state_name(state).ok()?;
        let id = format!("{namespace}:{name}");
        let definition = self.blockstates.get(&id)?;
        let props = parsed_props.into_iter().collect::<BTreeMap<_, _>>();
        let baked = blockstate::bake_blockstate(&self.models, definition, &props, [0; 3]).ok()?;
        if baked.quads.is_empty() {
            return None;
        }
        let uses_tint = baked.quads.iter().any(|quad| quad.tintindex >= 0);
        let textures = baked
            .quads
            .iter()
            .map(|quad| quad.texture.clone())
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect();
        let is_full_opaque_cube = is_full_unit_cube(&baked.quads) && !uses_tint;
        Some(BakedBlockStateMesh {
            quads: baked.quads,
            is_full_opaque_cube,
            uses_tint,
            textures,
        })
    }
}

#[derive(Clone, Copy)]
enum AssetKind {
    Blockstate,
    Model,
}

fn asset_entry_id(entry: &str) -> Option<(AssetKind, String)> {
    let rest = entry.strip_prefix("assets/")?;
    let (namespace, asset_path) = rest.split_once('/')?;
    if !valid_resource_part(namespace) || namespace.contains('/') {
        return None;
    }
    let (kind, path) = if let Some(path) = asset_path.strip_prefix("blockstates/") {
        (AssetKind::Blockstate, path)
    } else {
        let path = asset_path.strip_prefix("models/")?;
        (AssetKind::Model, path)
    };
    let path = path.strip_suffix(".json")?;
    if !valid_resource_part(path) {
        return None;
    }
    Some((kind, format!("{namespace}:{path}")))
}

/// Baked geometry and texture identifiers for one block state.
#[derive(Clone, Debug, PartialEq)]
pub struct BakedBlockStateMesh {
    pub quads: Vec<BakedModelQuad>,
    pub is_full_opaque_cube: bool,
    pub uses_tint: bool,
    pub textures: Vec<String>,
}

/// Counters for catalog loading and cache behavior.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BakedModelCacheStats {
    pub loaded_blockstates: usize,
    pub loaded_models: usize,
    /// Number of state-id lookups served from the cache.
    pub baked_hits: usize,
    /// Number of first-time state-id lookups that could not be baked.
    pub baked_misses: usize,
    /// Number of full-cube meshes baked and available to the greedy path.
    pub full_cube_fast_paths: usize,
}

/// Lazy state-id cache. Successful meshes and negative results are both
/// retained, so every state id is parsed and baked at most once.
pub struct BakedBlockStateCache {
    catalog: VanillaBlockModelCatalog,
    state_names: Vec<String>,
    baked: HashMap<u32, Option<BakedBlockStateMesh>>,
    stats: BakedModelCacheStats,
}

impl BakedBlockStateCache {
    pub fn new(catalog: VanillaBlockModelCatalog, state_names: Vec<String>) -> Self {
        let stats = BakedModelCacheStats {
            loaded_blockstates: catalog.loaded_blockstates(),
            loaded_models: catalog.loaded_models(),
            ..BakedModelCacheStats::default()
        };
        Self {
            catalog,
            state_names,
            baked: HashMap::new(),
            stats,
        }
    }

    /// Returns a cached mesh, baking the canonical state name on first access.
    pub fn get_or_bake(&mut self, state_id: u32) -> Option<&BakedBlockStateMesh> {
        if self.baked.contains_key(&state_id) {
            self.stats.baked_hits += 1;
            return self.baked.get(&state_id).and_then(Option::as_ref);
        }
        let result = usize::try_from(state_id)
            .ok()
            .and_then(|index| self.state_names.get(index))
            .and_then(|state| self.catalog.bake_state(state));
        if let Some(mesh) = &result {
            if mesh.is_full_opaque_cube {
                self.stats.full_cube_fast_paths += 1;
            }
        } else {
            self.stats.baked_misses += 1;
        }
        self.baked.insert(state_id, result);
        self.baked.get(&state_id).and_then(Option::as_ref)
    }

    /// True only when a successfully baked mesh is not a full opaque cube.
    pub fn is_non_cube_baked(&mut self, state_id: u32) -> bool {
        self.get_or_bake(state_id)
            .is_some_and(|mesh| !mesh.is_full_opaque_cube)
    }

    pub fn stats(&self) -> BakedModelCacheStats {
        self.stats
    }
}

fn is_full_unit_cube(quads: &[BakedModelQuad]) -> bool {
    if quads.len() != 6 {
        return false;
    }
    let mut faces = [false; 6];
    for quad in quads {
        let Some(axis) = quad
            .normal
            .iter()
            .position(|component| component.abs() > 0.9999)
        else {
            return false;
        };
        if quad.normal.iter().enumerate().any(|(i, component)| {
            i != axis && component.abs() > 0.0001
        }) {
            return false;
        }
        let sign = quad.normal[axis] > 0.0;
        let face = axis * 2 + usize::from(sign);
        if faces[face] {
            return false;
        }
        faces[face] = true;
        for position in quad.positions {
            if position
                .iter()
                .any(|coordinate| (*coordinate < -0.0001) || (*coordinate > 1.0001))
                || (position[axis] - if sign { 1.0 } else { 0.0 }).abs() > 0.0001
            {
                return false;
            }
        }
        for coordinate_axis in 0..3 {
            let min = quad
                .positions
                .iter()
                .map(|position| position[coordinate_axis])
                .fold(f32::INFINITY, f32::min);
            let max = quad
                .positions
                .iter()
                .map(|position| position[coordinate_axis])
                .fold(f32::NEG_INFINITY, f32::max);
            if coordinate_axis != axis && (min > 0.0001 || max < 0.9999) {
                return false;
            }
        }
    }
    faces.into_iter().all(|face| face)
}
