// SPDX-License-Identifier: LGPL-3.0-or-later

//! Bounded loader and lazy state-id cache for Java blockstate/model JSON.
//!
//! Layers are ordered from lowest to highest priority. Parsing and geometry
//! resolution are delegated to `model_baker`; this module maps vanilla asset
//! paths to those existing parsers and caches the final result per state id.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;

use crate::model_baker::blockstate::{self, BlockStateDef, VariantGroups};
use crate::model_baker::{
    BakedModel, BakedModelQuad, ModelError, RawModelJson, ResourcePackModelStack,
    bake_model_with_uvlock,
};
use crate::{ALL_FACES, Block, BlockShape, CUTOUT, Direction, OPAQUE, TINTED, TRANSLUCENT};

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
                return Err(ModelError::BlockState(
                    "unterminated state properties".into(),
                ));
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
        // Admission walks highest-priority layers first while still storing the
        // parsed packs in their original low-to-high stacking order. Otherwise
        // a large low-priority vanilla layer can consume the document budget and
        // crowd out the high-priority override that should win resolution.
        let mut parsed_layers = Vec::new();
        parsed_layers.resize_with(layers.len(), BTreeMap::new);
        for (layer_index, layer) in layers.iter().enumerate().rev() {
            for (entry, text) in layer {
                let Some((kind, id)) = asset_entry_id(entry) else {
                    continue;
                };
                if text.len() > MAX_BLOCK_MODEL_JSON_BYTES {
                    continue;
                }
                match kind {
                    AssetKind::Model if total_models < MAX_BLOCK_MODEL_DOCUMENTS => {
                        total_models += 1;
                        if let Ok(model) = RawModelJson::parse(text) {
                            parsed_layers[layer_index].insert(id, model);
                            catalog.loaded_models += 1;
                        }
                    }
                    AssetKind::Blockstate if total_blockstates < MAX_BLOCKSTATE_DOCUMENTS => {
                        total_blockstates += 1;
                        if let Ok(definition) = BlockStateDef::parse(text) {
                            catalog.blockstates.insert(id, definition);
                        }
                    }
                    _ => {}
                }
            }
        }
        for parsed_models in parsed_layers {
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

    fn bake_state(
        &self,
        state: &str,
        model_cache: &mut BakedVariants,
    ) -> Option<Vec<BakedVariantGroup>> {
        let (namespace, name, parsed_props) = parse_block_state_name(state).ok()?;
        let id = format!("{namespace}:{name}");
        let definition = self.blockstates.get(&id)?;
        let props = parsed_props.into_iter().collect();
        let groups = match blockstate::matching_variant_groups(definition, &props).ok()? {
            VariantGroups::Variant(variants) => vec![(0, variants)],
            VariantGroups::Multipart(groups) => groups,
        };
        let mut baked = Vec::with_capacity(groups.len());
        for (salt, variants) in groups {
            let mut total_weight = 0;
            let mut choices = Vec::with_capacity(variants.len());
            for variant in variants {
                let key = (
                    variant.model.clone(),
                    variant.x_deg,
                    variant.y_deg,
                    variant.uvlock,
                );
                let geometry = model_cache.entry(key).or_insert_with_key(|key| {
                    bake_model_with_uvlock(&self.models, &key.0, key.1, key.2, key.3)
                        .ok()
                        .map(Arc::new)
                });
                total_weight += variant.weight as u64;
                choices.push((total_weight, geometry.clone()));
            }
            baked.push(BakedVariantGroup {
                salt,
                total_weight,
                choices,
            });
        }
        Some(baked)
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

type BakedVariants = HashMap<(String, i32, i32, bool), Option<Arc<BakedModel>>>;
type GreedyMaterialCheck = dyn Fn(u32, &BakedModelQuad) -> bool + Send + Sync;

struct BakedVariantGroup {
    salt: u64,
    total_weight: u64,
    choices: Vec<(u64, Option<Arc<BakedModel>>)>,
}

impl BakedVariantGroup {
    fn at(&self, pos: [i32; 3]) -> Option<&BakedModel> {
        let draw = blockstate::position_hash(pos, self.salt) % self.total_weight;
        let index = self.choices.partition_point(|(upper, _)| *upper <= draw);
        self.choices[index].1.as_deref()
    }
}

struct CachedState {
    groups: Vec<BakedVariantGroup>,
    origin: Option<BakedBlockStateMesh>,
}

impl CachedState {
    fn faces_at(&self, pos: [i32; 3]) -> Option<impl Iterator<Item = (&BakedModelQuad, bool)>> {
        if self.groups.iter().any(|group| group.at(pos).is_none()) {
            return None;
        }
        let first_ambient_occlusion = self
            .groups
            .iter()
            .filter_map(|group| group.at(pos))
            .next()
            .is_some_and(|model| model.ambient_occlusion);
        Some(
            self.groups
                .iter()
                .filter_map(move |group| group.at(pos))
                .flat_map(move |model| {
                    model
                        .quads
                        .iter()
                        .map(move |quad| (quad, first_ambient_occlusion))
                }),
        )
    }
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

/// Lazy state-id cache. State predicates are resolved once and rotated
/// alternatives share baked geometry across states.
pub struct BakedBlockStateCache {
    catalog: VanillaBlockModelCatalog,
    state_names: Vec<String>,
    baked: HashMap<u32, Option<CachedState>>,
    variants: BakedVariants,
    greedy_material_check: Option<Box<GreedyMaterialCheck>>,
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
            variants: HashMap::new(),
            greedy_material_check: None,
            stats,
        }
    }

    /// Enable greedy classification only for quads certified by the caller.
    /// The check must confirm an opaque layer and compatible atlas mapping,
    /// including the texture's alpha coverage and the greedy path's UV
    /// projection. The default constructor has no such material information.
    pub fn new_with_greedy_material_check(
        catalog: VanillaBlockModelCatalog,
        state_names: Vec<String>,
        check: impl Fn(u32, &BakedModelQuad) -> bool + Send + Sync + 'static,
    ) -> Self {
        let mut cache = Self::new(catalog, state_names);
        cache.greedy_material_check = Some(Box::new(check));
        cache
    }

    fn cached_state(&mut self, state_id: u32) -> Option<&CachedState> {
        if self.baked.contains_key(&state_id) {
            self.stats.baked_hits += 1;
            return self.baked.get(&state_id).and_then(Option::as_ref);
        }
        let result = usize::try_from(state_id)
            .ok()
            .and_then(|index| self.state_names.get(index))
            .and_then(|state| self.catalog.bake_state(state, &mut self.variants))
            .map(|groups| {
                let origin = self.summarize_origin(state_id, &groups);
                CachedState { groups, origin }
            });
        if let Some(mesh) = result.as_ref().and_then(|state| state.origin.as_ref()) {
            if mesh.is_full_opaque_cube {
                self.stats.full_cube_fast_paths += 1;
            }
        } else {
            self.stats.baked_misses += 1;
        }
        self.baked.insert(state_id, result);
        self.baked.get(&state_id).and_then(Option::as_ref)
    }

    fn summarize_origin(
        &self,
        state_id: u32,
        groups: &[BakedVariantGroup],
    ) -> Option<BakedBlockStateMesh> {
        let mut quads = Vec::new();
        for group in groups {
            quads.extend_from_slice(&group.at([0; 3])?.quads);
        }
        let first = quads.first()?;
        let uses_tint = quads.iter().any(|quad| quad.tintindex >= 0);
        let is_full_opaque_cube = self.greedy_material_check.as_ref().is_some_and(|check| {
            !uses_tint
                && groups.len() == 1
                && groups[0].choices.len() == 1
                && groups[0]
                    .at([0; 3])
                    .is_some_and(|model| model.ambient_occlusion)
                && is_full_unit_cube(&quads)
                && quads
                    .iter()
                    .all(|quad| quad.texture == first.texture && check(state_id, quad))
        });
        let textures = quads
            .iter()
            .map(|quad| quad.texture.clone())
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect();
        Some(BakedBlockStateMesh {
            quads,
            is_full_opaque_cube,
            uses_tint,
            textures,
        })
    }

    /// Return the origin mesh for previews and existing state-id callers.
    /// Section meshing uses [`Self::baked_quads_at`] to select by world position.
    pub fn get_or_bake(&mut self, state_id: u32) -> Option<&BakedBlockStateMesh> {
        self.cached_state(state_id)?.origin.as_ref()
    }

    /// Select baked quads at a world block position. Multipart selectors use
    /// the same per-selector salt and file order as the standalone baker.
    pub fn baked_quads_at(
        &mut self,
        state_id: u32,
        world_pos: [i32; 3],
    ) -> Option<impl Iterator<Item = &BakedModelQuad>> {
        Some(
            self.cached_state(state_id)?
                .faces_at(world_pos)?
                .map(|(quad, _)| quad),
        )
    }

    pub(crate) fn baked_faces_for_terrain(
        &mut self,
        block: Block,
        world_pos: [i32; 3],
    ) -> Option<impl Iterator<Item = (&BakedModelQuad, bool)>> {
        let state = self.cached_state(block.state_id)?;
        if block.flags & (OPAQUE | CUTOUT | TRANSLUCENT | TINTED) == OPAQUE
            && block.occlusion & ALL_FACES == ALL_FACES
            && block.shape() == BlockShape::Cube
            && state
                .origin
                .as_ref()
                .is_some_and(|mesh| mesh.is_full_opaque_cube)
        {
            return None;
        }
        state.faces_at(world_pos)
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
        if quad
            .normal
            .iter()
            .enumerate()
            .any(|(i, component)| i != axis && component.abs() > 0.0001)
        {
            return false;
        }
        let sign = quad.normal[axis] > 0.0;
        let direction = match (axis, sign) {
            (0, false) => Direction::West,
            (0, true) => Direction::East,
            (1, false) => Direction::Down,
            (1, true) => Direction::Up,
            (_, false) => Direction::North,
            (_, true) => Direction::South,
        };
        let face = direction as usize;
        if faces[face] || quad.cullface != Some(direction) || !quad.shade {
            return false;
        }
        faces[face] = true;
        let mut corners = 0;
        let (u, v) = direction.tangents();
        for position in quad.positions {
            if (position[axis] - if sign { 1.0 } else { 0.0 }).abs() > 0.0001 {
                return false;
            }
            let u = position[u];
            let v = position[v];
            if (u - u.round()).abs() > 0.0001
                || (v - v.round()).abs() > 0.0001
                || !(-0.0001..=1.0001).contains(&u)
                || !(-0.0001..=1.0001).contains(&v)
            {
                return false;
            }
            corners |= 1 << (u.round() as u8 + 2 * v.round() as u8);
        }
        if corners != 0b1111 {
            return false;
        }
    }
    faces.into_iter().all(|face| face)
}
