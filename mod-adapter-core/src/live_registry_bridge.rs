// SPDX-License-Identifier: LGPL-3.0-or-later

//! Deterministic multi-mod dynamic registry planning and its bounded MRS1
//! server-to-client synchronization frame.

use std::collections::{BTreeMap, BTreeSet};

use crate::auto_adapter::ModsDirAdaptationReport;
use crate::blockstates::PropertyDef;

pub const MOD_REGISTRY_SYNC_CHANNEL: &str = "megu:mod_registry_sync_v1";

const WIRE_MAGIC: &[u8; 4] = b"MRS1";
const WIRE_VERSION: u16 = 1;
/// Version-2 frames append one `facts_byte` per block state after its
/// properties; everything else is byte-identical to v1.
const WIRE_VERSION_V2: u16 = 2;
const MAX_BLOCK_STATES: usize = 65_536;
const MAX_ITEMS: usize = 65_536;
const MAX_PROPERTIES: usize = 64;
const MAX_STRING_BYTES: usize = u16::MAX as usize;
const MAX_FRAME_BYTES: usize = 8 * 1024 * 1024;

/// The `facts_byte` of an opaque, non-luminous full cube —
/// `(luminance << 4) | (opacity & 0x0f)` = `0x0F` — the conservative
/// default a v1 frame implies for every state.
pub const DEFAULT_FACTS_BYTE: u8 = 0x0f;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DynamicBlockStateEntry {
    pub state_id: u32,
    pub block_id: String,
    pub properties: Vec<(String, String)>,
    /// Bytecode-extracted `(luminance << 4) | opacity` facts for this
    /// state, when the JAR's `.class` property builders established them —
    /// beats the name/property heuristic in [`Self::facts_byte`]. `None`
    /// on Create-catalog entries, v1-decoded frames, and blocks whose
    /// builders carried no light/occlusion facts.
    pub facts_override: Option<u8>,
}

impl DynamicBlockStateEntry {
    /// The packed `(luminance << 4) | opacity` physical-facts byte for this
    /// state: the bytecode-extracted override when present, otherwise the
    /// value derived from the block id and property pairs. It is the value
    /// [`encode_registry_sync_frame_v2`](UnifiedModRegistryPlan::encode_registry_sync_frame_v2)
    /// writes and the value a v1 decoder derives for [`SyncedModRegistry::facts_bytes`].
    pub fn facts_byte(&self) -> u8 {
        self.facts_override.unwrap_or_else(|| {
            derive_block_facts_byte(&self.block_id, &self.properties)
        })
    }
}

/// Physical facts one mod block's `.class` property builders established —
/// the `(luminance, no_occlusion)` pair [`UnifiedModRegistryPlan`] folds
/// into every expanded state's [`DynamicBlockStateEntry::facts_override`].
/// `None` fields mean the bytecode did not statically establish them, so
/// the name/property heuristic still answers for that nibble.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BytecodeBlockFacts {
    /// `lightLevel`/`luminance` from the block's `Properties`/`Settings`
    /// builder chain, clamped to the vanilla `0..=15` nibble.
    pub light_level: Option<i32>,
    /// `noOcclusion`/`nonOpaque` from the same builder chain.
    pub no_occlusion: Option<bool>,
}

impl BytecodeBlockFacts {
    /// The `(luminance << 4) | opacity` byte for one expanded state of the
    /// block, or `None` when the bytecode established neither fact (the
    /// caller then keeps the heuristic derivation).
    pub fn facts_override(&self, block_id: &str, properties: &[(String, String)]) -> Option<u8> {
        if self.light_level.is_none() && self.no_occlusion.is_none() {
            return None;
        }
        let derived = derive_block_facts_byte(block_id, properties);
        let luminance = self
            .light_level
            .map(|level| level.clamp(0, 15) as u8)
            .unwrap_or(derived >> 4);
        let mut opacity = derived & 0x0f;
        // `noOcclusion` blocks still shape the world but never block
        // light — cap the opacity nibble at the partial-occluder level.
        if self.no_occlusion == Some(true) {
            opacity = opacity.min(1);
        }
        Some((luminance << 4) | opacity)
    }
}

/// `(luminance << 4) | (opacity & 0x0f)` for one mod block state, derived
/// generically from the block id's path and the state's property pairs —
/// name and property vocabulary only, never a mod id. Opaque solids answer
/// `15`, leaves/fluids/partial occluders `1`, glassy fittings and
/// empty-collision decorations `0`; emitter names and `lit`/`powered`/
/// `blaze` properties produce the emission nibble. The same rules the
/// server's registry applies when building its per-state light byte, so v1
/// frames and v2 wire bytes agree.
pub fn derive_block_facts_byte(block_id: &str, properties: &[(String, String)]) -> u8 {
    let path = block_id.rsplit(':').next().unwrap_or(block_id);
    (mod_state_luminance(path, properties) << 4) | mod_state_opacity(path, properties)
}

/// Shape family a mod block's id implies, mirroring the server registry's
/// own classification so both sides derive identical bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ModShape {
    Empty,
    Thin,
    Partial,
    Leaves,
    Liquid,
    Full,
}

fn mod_block_shape(path: &str) -> ModShape {
    // `_block` names are cubes first: `grass_block` must not read as `grass`.
    if path == "block" || path.ends_with("_block") {
        return ModShape::Full;
    }
    // Suffixes, not substrings: `fluid_pipe` and `water_wheel` are mounts,
    // not fluids.
    if ["water", "lava", "fluid", "liquid"]
        .iter()
        .any(|k| path.ends_with(k))
    {
        return ModShape::Liquid;
    }
    if path.contains("leaves") {
        return ModShape::Leaves;
    }
    const EMPTY: &[&str] = &[
        "torch", "rail", "flower", "sapling", "crop", "vine", "wire", "button",
        "lever", "sign", "banner", "candle", "mushroom", "fern", "coral",
        "reed", "lichen", "grass", "plant", "seed",
    ];
    if EMPTY.iter().any(|k| path.contains(k)) {
        return ModShape::Empty;
    }
    const THIN: &[&str] = &[
        "glass", "pane", "grate", "bars", "chain", "lantern", "pipe", "shaft",
        "belt", "cogwheel", "wheel", "funnel", "chute", "spout", "depot",
        "basin", "pump", "bearing", "ladder", "scaffold", "frame",
        "pressure_plate", "door", "trapdoor", "hopper", "cauldron", "anvil",
        "conduit",
    ];
    if THIN.iter().any(|k| path.contains(k)) {
        return ModShape::Thin;
    }
    const PARTIAL: &[&str] = &[
        "slab", "stair", "fence", "wall", "carpet", "snow", "path", "farmland",
    ];
    if PARTIAL.iter().any(|k| path.contains(k)) || path.ends_with("_bed") {
        return ModShape::Partial;
    }
    ModShape::Full
}

fn mod_state_luminance(path: &str, prop_pairs: &[(String, String)]) -> u8 {
    let base = if path.contains("magma") {
        7
    } else if [
        "lamp",
        "lantern",
        "torch",
        "glow",
        "blaze_burner",
        "light",
        "crystal",
        "luminous",
        "forge",
    ]
    .iter()
    .any(|k| path.contains(k))
    {
        14
    } else {
        0
    };
    let mut luminance = base;
    for (key, value) in prop_pairs {
        match (key.as_str(), value.as_str()) {
            ("lit", "true") => luminance = 15,
            ("lit", _) => luminance = 0,
            ("blaze", level) => {
                luminance = match level {
                    "seething" => 15,
                    "kindled" => 11,
                    "smouldering" => 7,
                    _ => 0,
                }
            }
            ("powered", "true") if base > 0 => luminance = 15,
            _ => {}
        }
    }
    luminance.min(15)
}

fn mod_state_opacity(path: &str, prop_pairs: &[(String, String)]) -> u8 {
    let mut opacity = match mod_block_shape(path) {
        ModShape::Empty | ModShape::Thin => 0,
        ModShape::Partial | ModShape::Leaves | ModShape::Liquid => 1,
        ModShape::Full => 15,
    };
    // Carried water dims light by one level even on a transparent mount.
    if opacity == 0
        && prop_pairs
            .iter()
            .any(|(k, v)| k == "waterlogged" && v == "true")
    {
        opacity = 1;
    }
    opacity
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RegistryContentIndex {
    /// Recipe id to recipe type, with duplicate ids required to agree.
    pub recipes: BTreeMap<String, String>,
    /// Tag id to the sorted union of declared values.
    pub tags: BTreeMap<String, BTreeSet<String>>,
    /// Loot-table ids discovered across adapted JARs.
    pub loot_tables: BTreeSet<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnifiedModRegistryPlan {
    pub base_block_state_id: u32,
    pub base_item_id: u32,
    pub block_states: Vec<DynamicBlockStateEntry>,
    pub items: BTreeMap<u32, String>,
    pub content: RegistryContentIndex,
    /// Bytecode-extracted physical facts keyed by block id, harvested from
    /// each admitted JAR's `.class` property builders during
    /// [`Self::from_auto_adapt`]. Already folded into each entry's
    /// `facts_override`; retained so consumers can audit what the bytecode
    /// established versus what the name heuristic guessed.
    pub bytecode_facts: BTreeMap<String, BytecodeBlockFacts>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RegistryPlanError {
    ConflictingBlockDefinition(String),
    DuplicateProperty { block: String, property: String },
    EmptyPropertyValues { block: String, property: String },
    PropertyExpansionTooLarge { block: String },
    TooManyBlockStates,
    TooManyItems,
    BlockStateIdOverflow,
    ItemIdOverflow,
    ConflictingRecipe(String),
    TooManyProperties { block: String },
    StringTooLong,
    FrameTooLarge,
}

impl UnifiedModRegistryPlan {
    /// Build a deterministic registry from all JARs in an adaptation report.
    ///
    /// Blockstate expansion is capped at [`MAX_BLOCK_STATES`] total entries
    /// and per block. Duplicate block declarations are accepted only when
    /// their extracted property tables agree exactly.
    pub fn from_auto_adapt(
        base_block_state_id: u32,
        base_item_id: u32,
        report: &ModsDirAdaptationReport,
    ) -> Result<Self, RegistryPlanError> {
        let mut blocks = BTreeMap::<String, Vec<PropertyDef>>::new();
        let mut item_names = BTreeSet::<String>::new();
        let mut content = RegistryContentIndex::default();
        let mut bytecode_facts = BTreeMap::<String, BytecodeBlockFacts>::new();

        for jar in &report.jars {
            // `.class` property builders are the ground truth for a mod
            // block's light/occlusion facts — harvest them per registered
            // block id so every expanded state can carry the override.
            if let Some(scan) = &jar.bytecode_properties {
                for class in &scan.classes {
                    for record in &class.blocks {
                        let Some(block_id) = record.registration_name.as_deref() else {
                            continue;
                        };
                        let props = &record.properties;
                        if props.light_level.is_none() && props.no_occlusion.is_none() {
                            continue;
                        }
                        bytecode_facts
                            .entry(block_id.to_string())
                            .or_insert(BytecodeBlockFacts {
                                light_level: props.light_level,
                                no_occlusion: props.no_occlusion,
                            });
                    }
                }
            }
            for (block_id, properties) in &jar.data_bundle.blockstates {
                match blocks.get(block_id) {
                    Some(existing) if existing != properties => {
                        return Err(RegistryPlanError::ConflictingBlockDefinition(
                            block_id.clone(),
                        ));
                    }
                    Some(_) => {}
                    None => {
                        blocks.insert(block_id.clone(), properties.clone());
                    }
                }
                item_names.insert(block_id.clone());
            }
            item_names.extend(jar.data_bundle.items.iter().cloned());

            for recipe in &jar.data_bundle.recipes {
                match content.recipes.get(&recipe.name) {
                    Some(existing) if existing != &recipe.recipe_type => {
                        return Err(RegistryPlanError::ConflictingRecipe(recipe.name.clone()));
                    }
                    Some(_) => {}
                    None => {
                        content
                            .recipes
                            .insert(recipe.name.clone(), recipe.recipe_type.clone());
                    }
                }
            }
            for (tag, values) in &jar.data_bundle.tags {
                content
                    .tags
                    .entry(tag.clone())
                    .or_default()
                    .extend(values.iter().cloned());
            }
            content
                .loot_tables
                .extend(jar.data_bundle.loot_tables.keys().cloned());
        }

        let mut block_states = Vec::new();
        let mut next_state_id = base_block_state_id;
        for (block_id, mut properties) in blocks {
            validate_wire_string(&block_id)?;
            properties.sort_by(|a, b| a.name.cmp(&b.name));
            for pair in properties.windows(2) {
                if pair[0].name == pair[1].name {
                    return Err(RegistryPlanError::DuplicateProperty {
                        block: block_id,
                        property: pair[0].name.clone(),
                    });
                }
            }
            if properties.len() > MAX_PROPERTIES {
                return Err(RegistryPlanError::TooManyProperties { block: block_id });
            }
            properties.sort_by(|a, b| a.name.cmp(&b.name));
            let mut combinations = 1usize;
            for property in &mut properties {
                validate_wire_string(&property.name)?;
                if property.values.is_empty() {
                    return Err(RegistryPlanError::EmptyPropertyValues {
                        block: block_id,
                        property: property.name.clone(),
                    });
                }
                property.values.sort();
                property.values.dedup();
                for value in &property.values {
                    validate_wire_string(value)?;
                }
                combinations = combinations
                    .checked_mul(property.values.len())
                    .filter(|&count| count <= MAX_BLOCK_STATES)
                    .ok_or_else(|| RegistryPlanError::PropertyExpansionTooLarge {
                        block: block_id.clone(),
                    })?;
            }
            block_states
                .len()
                .checked_add(combinations)
                .filter(|&count| count <= MAX_BLOCK_STATES)
                .ok_or(RegistryPlanError::TooManyBlockStates)?;

            let mut combinations_rows: Vec<Vec<(String, String)>> = vec![Vec::new()];
            for property in &properties {
                let capacity = combinations_rows
                    .len()
                    .checked_mul(property.values.len())
                    .ok_or_else(|| RegistryPlanError::PropertyExpansionTooLarge {
                        block: block_id.clone(),
                    })?;
                let mut expanded = Vec::with_capacity(capacity);
                for prefix in &combinations_rows {
                    for value in &property.values {
                        let mut row = prefix.clone();
                        row.push((property.name.clone(), value.clone()));
                        expanded.push(row);
                    }
                }
                combinations_rows = expanded;
            }

            for state_properties in combinations_rows {
                let state_id = next_state_id;
                next_state_id = next_state_id
                    .checked_add(1)
                    .ok_or(RegistryPlanError::BlockStateIdOverflow)?;
                let facts_override = bytecode_facts
                    .get(&block_id)
                    .and_then(|facts| facts.facts_override(&block_id, &state_properties));
                block_states.push(DynamicBlockStateEntry {
                    state_id,
                    block_id: block_id.clone(),
                    properties: state_properties,
                    facts_override,
                });
            }
        }

        let mut items = BTreeMap::new();
        let mut next_item_id = base_item_id;
        for item in item_names {
            validate_wire_string(&item)?;
            let item_id = next_item_id;
            next_item_id = next_item_id
                .checked_add(1)
                .ok_or(RegistryPlanError::ItemIdOverflow)?;
            if items.len() == MAX_ITEMS {
                return Err(RegistryPlanError::TooManyItems);
            }
            items.insert(item_id, item);
        }

        let plan = Self {
            base_block_state_id,
            base_item_id,
            block_states,
            items,
            content,
            bytecode_facts,
        };
        if encoded_frame_len(&plan).is_none_or(|length| length > MAX_FRAME_BYTES) {
            return Err(RegistryPlanError::FrameTooLarge);
        }
        Ok(plan)
    }

    pub fn resolve_block_state(&self, state_id: u32) -> Option<&DynamicBlockStateEntry> {
        let offset = state_id.checked_sub(self.base_block_state_id)? as usize;
        self.block_states
            .get(offset)
            .filter(|entry| entry.state_id == state_id)
    }

    pub fn block_state_id(&self, block_id: &str, props: &[(String, String)]) -> Option<u32> {
        self.block_states
            .iter()
            .find(|entry| {
                entry.block_id == block_id
                    && entry.properties.len() == props.len()
                    && entry.properties.iter().all(|(name, value)| {
                        props.iter().any(|(candidate_name, candidate_value)| {
                            name == candidate_name && value == candidate_value
                        })
                    })
            })
            .map(|entry| entry.state_id)
    }

    /// Vanilla `Block.byName` + `BlockState.setValue` semantics over the
    /// JAR block table: start from the block's default (first expanded)
    /// state and overlay each named property. Unknown block ids, property
    /// names, or property values answer `None` — the same fail-closed
    /// contract the server's `checked_state` keeps, so `/fill`-style
    /// selectors accept partial property lists.
    pub fn checked_block_state(
        &self,
        block_id: &str,
        props: &[(String, String)],
    ) -> Option<u32> {
        let default = self
            .block_states
            .iter()
            .find(|entry| entry.block_id == block_id)?;
        let mut merged = default.properties.clone();
        for (key, value) in props {
            let slot = merged.iter_mut().find(|(name, _)| name == key)?;
            slot.1 = value.clone();
        }
        self.block_state_id(block_id, &merged)
    }

    pub fn resolve_item(&self, item_id: u32) -> Option<&str> {
        self.items.get(&item_id).map(String::as_str)
    }

    pub fn encode_registry_sync_frame(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(WIRE_MAGIC);
        put_u16(&mut out, WIRE_VERSION);
        put_u32(&mut out, self.base_block_state_id);
        put_u32(&mut out, self.base_item_id);
        put_u32(&mut out, self.block_states.len() as u32);
        for entry in &self.block_states {
            put_u32(&mut out, entry.state_id);
            put_string(&mut out, &entry.block_id);
            put_u16(&mut out, entry.properties.len() as u16);
            for (key, value) in &entry.properties {
                put_string(&mut out, key);
                put_string(&mut out, value);
            }
        }
        put_u32(&mut out, self.items.len() as u32);
        for (id, name) in &self.items {
            put_u32(&mut out, *id);
            put_string(&mut out, name);
        }
        let checksum = megu_core::hash::crc32_ieee(&out);
        put_u32(&mut out, checksum);
        out
    }

    /// Version-2 sync frame: identical to v1 with one trailing `facts_byte`
    /// per block state after its properties — `(luminance << 4) | opacity`,
    /// each entry's [`DynamicBlockStateEntry::facts_byte`] — so the client
    /// light engine and mesher learn every modded state's light facts
    /// without re-deriving them. v1 remains the default frame; both decode
    /// through [`decode_registry_sync_frame`].
    pub fn encode_registry_sync_frame_v2(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(WIRE_MAGIC);
        put_u16(&mut out, WIRE_VERSION_V2);
        put_u32(&mut out, self.base_block_state_id);
        put_u32(&mut out, self.base_item_id);
        put_u32(&mut out, self.block_states.len() as u32);
        for entry in &self.block_states {
            put_u32(&mut out, entry.state_id);
            put_string(&mut out, &entry.block_id);
            put_u16(&mut out, entry.properties.len() as u16);
            for (key, value) in &entry.properties {
                put_string(&mut out, key);
                put_string(&mut out, value);
            }
            out.push(entry.facts_byte());
        }
        put_u32(&mut out, self.items.len() as u32);
        for (id, name) in &self.items {
            put_u32(&mut out, *id);
            put_string(&mut out, name);
        }
        let checksum = megu_core::hash::crc32_ieee(&out);
        put_u32(&mut out, checksum);
        out
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SyncedModRegistry {
    pub base_block_state_id: u32,
    pub base_item_id: u32,
    pub block_states: Vec<DynamicBlockStateEntry>,
    /// `(luminance << 4) | opacity` per entry of `block_states`, in the same
    /// order — the wire byte on a v2 frame, the derived default on v1.
    pub facts_bytes: Vec<u8>,
    pub items: BTreeMap<u32, String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RegistrySyncError {
    Truncated,
    BadMagic,
    UnsupportedVersion(u16),
    ChecksumMismatch,
    TrailingBytes,
    FrameTooLarge,
    TooManyBlockStates,
    TooManyItems,
    TooManyProperties,
    InvalidUtf8,
    InvalidString,
    NonMonotonicBlockStateId,
    NonMonotonicItemId,
    InvalidBaseId,
}

pub fn decode_registry_sync_frame(bytes: &[u8]) -> Result<SyncedModRegistry, RegistrySyncError> {
    if bytes.len() > MAX_FRAME_BYTES {
        return Err(RegistrySyncError::FrameTooLarge);
    }
    if bytes.len() < 4 + 2 + 4 + 4 + 4 + 4 + 4 {
        return Err(RegistrySyncError::Truncated);
    }
    let payload_len = bytes.len() - 4;
    let expected_crc = u32::from_be_bytes(
        bytes[payload_len..]
            .try_into()
            .map_err(|_| RegistrySyncError::Truncated)?,
    );
    if megu_core::hash::crc32_ieee(&bytes[..payload_len]) != expected_crc {
        return Err(RegistrySyncError::ChecksumMismatch);
    }
    let mut reader = Reader {
        bytes: &bytes[..payload_len],
        cursor: 0,
    };
    if reader.take(4)? != WIRE_MAGIC {
        return Err(RegistrySyncError::BadMagic);
    }
    let version = reader.u16()?;
    if version != WIRE_VERSION && version != WIRE_VERSION_V2 {
        return Err(RegistrySyncError::UnsupportedVersion(version));
    }
    // v2 appends one facts byte per state; a v1 frame implies the derived
    // default, so callers always see a populated `facts_bytes`.
    let has_wire_facts = version == WIRE_VERSION_V2;
    let base_block_state_id = reader.u32()?;
    let base_item_id = reader.u32()?;
    let state_count = reader.u32()? as usize;
    if state_count > MAX_BLOCK_STATES {
        return Err(RegistrySyncError::TooManyBlockStates);
    }
    let mut block_states = Vec::with_capacity(state_count);
    let mut facts_bytes = Vec::with_capacity(state_count);
    let mut previous_state = None;
    for _ in 0..state_count {
        let state_id = reader.u32()?;
        if state_id < base_block_state_id
            || previous_state.is_some_and(|previous| state_id <= previous)
        {
            return Err(RegistrySyncError::NonMonotonicBlockStateId);
        }
        previous_state = Some(state_id);
        let block_id = reader.string()?;
        let property_count = reader.u16()? as usize;
        if property_count > MAX_PROPERTIES {
            return Err(RegistrySyncError::TooManyProperties);
        }
        let mut properties = Vec::with_capacity(property_count);
        let mut previous_key: Option<String> = None;
        for _ in 0..property_count {
            let key = reader.string()?;
            if previous_key
                .as_ref()
                .is_some_and(|previous| key <= *previous)
            {
                return Err(RegistrySyncError::InvalidString);
            }
            previous_key = Some(key.clone());
            properties.push((key, reader.string()?));
        }
        let facts_byte = if has_wire_facts {
            reader.take(1)?[0]
        } else {
            derive_block_facts_byte(&block_id, &properties)
        };
        facts_bytes.push(facts_byte);
        let derived = derive_block_facts_byte(&block_id, &properties);
        block_states.push(DynamicBlockStateEntry {
            state_id,
            block_id,
            properties,
            // A v2 wire byte that disagrees with the name/property
            // heuristic is authoritative — keep it as the entry's
            // override so `facts_byte()` agrees with `facts_bytes` even
            // when it came from bytecode extraction. A wire byte equal
            // to the heuristic (or a v1 frame, which carries none)
            // decodes to `None`, preserving round-trip equality.
            facts_override: (has_wire_facts && facts_byte != derived).then_some(facts_byte),
        });
    }
    let item_count = reader.u32()? as usize;
    if item_count > MAX_ITEMS {
        return Err(RegistrySyncError::TooManyItems);
    }
    let mut items = BTreeMap::new();
    let mut previous_item = None;
    for _ in 0..item_count {
        let id = reader.u32()?;
        if id < base_item_id || previous_item.is_some_and(|previous| id <= previous) {
            return Err(RegistrySyncError::NonMonotonicItemId);
        }
        previous_item = Some(id);
        items.insert(id, reader.string()?);
    }
    if reader.cursor != reader.bytes.len() {
        return Err(RegistrySyncError::TrailingBytes);
    }
    if state_count > 0 && base_block_state_id > block_states[0].state_id {
        return Err(RegistrySyncError::InvalidBaseId);
    }
    Ok(SyncedModRegistry {
        base_block_state_id,
        base_item_id,
        block_states,
        facts_bytes,
        items,
    })
}

fn put_u16(out: &mut Vec<u8>, value: u16) {
    out.extend_from_slice(&value.to_be_bytes());
}

fn put_u32(out: &mut Vec<u8>, value: u32) {
    out.extend_from_slice(&value.to_be_bytes());
}

fn put_string(out: &mut Vec<u8>, value: &str) {
    let length =
        u16::try_from(value.len()).expect("registry strings are bounded by extracted data");
    put_u16(out, length);
    out.extend_from_slice(value.as_bytes());
}

fn validate_wire_string(value: &str) -> Result<(), RegistryPlanError> {
    if value.is_empty() || value.len() > MAX_STRING_BYTES {
        Err(RegistryPlanError::StringTooLong)
    } else {
        Ok(())
    }
}

fn encoded_frame_len(plan: &UnifiedModRegistryPlan) -> Option<usize> {
    let mut length = 4usize + 2 + 4 + 4 + 4 + 4 + 4;
    for state in &plan.block_states {
        length = length.checked_add(4 + 2 + state.block_id.len() + 2)?;
        for (key, value) in &state.properties {
            length = length.checked_add(2 + key.len() + 2 + value.len())?;
        }
    }
    for item in plan.items.values() {
        length = length.checked_add(4 + 2 + item.len())?;
    }
    Some(length)
}

struct Reader<'a> {
    bytes: &'a [u8],
    cursor: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, length: usize) -> Result<&'a [u8], RegistrySyncError> {
        let end = self
            .cursor
            .checked_add(length)
            .filter(|&end| end <= self.bytes.len())
            .ok_or(RegistrySyncError::Truncated)?;
        let result = &self.bytes[self.cursor..end];
        self.cursor = end;
        Ok(result)
    }

    fn u16(&mut self) -> Result<u16, RegistrySyncError> {
        Ok(u16::from_be_bytes(
            self.take(2)?
                .try_into()
                .map_err(|_| RegistrySyncError::Truncated)?,
        ))
    }

    fn u32(&mut self) -> Result<u32, RegistrySyncError> {
        Ok(u32::from_be_bytes(
            self.take(4)?
                .try_into()
                .map_err(|_| RegistrySyncError::Truncated)?,
        ))
    }

    fn string(&mut self) -> Result<String, RegistrySyncError> {
        let length = self.u16()? as usize;
        if length == 0 || length > MAX_STRING_BYTES {
            return Err(RegistrySyncError::InvalidString);
        }
        String::from_utf8(self.take(length)?.to_vec()).map_err(|_| RegistrySyncError::InvalidUtf8)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(state_id: u32, block_id: &str, props: &[(&str, &str)]) -> DynamicBlockStateEntry {
        DynamicBlockStateEntry {
            state_id,
            block_id: block_id.to_string(),
            properties: props
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            facts_override: None,
        }
    }

    fn plan(block_states: Vec<DynamicBlockStateEntry>) -> UnifiedModRegistryPlan {
        UnifiedModRegistryPlan {
            base_block_state_id: 50_000,
            base_item_id: 80_000,
            block_states,
            items: BTreeMap::new(),
            content: RegistryContentIndex::default(),
            bytecode_facts: BTreeMap::new(),
        }
    }

    #[test]
    fn facts_byte_derivation_is_generic_and_property_aware() {
        // Opaque non-luminous solid: the 0x0F default.
        assert_eq!(
            derive_block_facts_byte("mymod:brass_block", &[]),
            DEFAULT_FACTS_BYTE
        );
        // Glowing lamp by name: luminance 14 over an opaque body.
        assert_eq!(derive_block_facts_byte("mymod:arc_lamp", &[]), 0xef);
        assert_eq!(
            derive_block_facts_byte(
                "mymod:arc_lamp",
                &[("lit".into(), "true".into())]
            ),
            0xff
        );
        assert_eq!(
            derive_block_facts_byte(
                "mymod:arc_lamp",
                &[("lit".into(), "false".into())]
            ),
            0x0f
        );
        // Transparent glass and a waterlogged thin mount.
        assert_eq!(derive_block_facts_byte("mymod:framed_glass", &[]), 0x00);
        assert_eq!(
            derive_block_facts_byte(
                "mymod:shaft",
                &[("waterlogged".into(), "true".into())]
            ),
            0x01
        );
        // `*_block`/`bedrock` must not read as `grass`/`bed`.
        assert_eq!(mod_block_shape("grass_block"), ModShape::Full);
        assert_eq!(mod_block_shape("bedrock"), ModShape::Full);
        assert_eq!(mod_block_shape("fluid_pipe"), ModShape::Thin);
        // The entry accessor answers the same byte the frame writes.
        let lamp = entry(50_000, "mymod:arc_lamp", &[]);
        assert_eq!(
            lamp.facts_byte(),
            derive_block_facts_byte(&lamp.block_id, &lamp.properties)
        );
    }

    #[test]
    fn v2_frame_round_trips_facts_and_v1_still_decodes() {
        let plan = plan(vec![
            entry(50_000, "mymod:brass_block", &[]),
            entry(50_001, "mymod:arc_lamp", &[]),
            entry(50_002, "mymod:framed_glass", &[]),
        ]);
        let v2 = plan.encode_registry_sync_frame_v2();
        assert!(v2.starts_with(b"MRS1"));
        let decoded = decode_registry_sync_frame(&v2).unwrap();
        assert_eq!(decoded.block_states, plan.block_states);
        assert_eq!(decoded.facts_bytes, vec![0x0f, 0xef, 0x00]);
        assert_eq!(decoded.items, plan.items);

        // v1 carries no byte: the decoder derives the same defaults, so a
        // mixed-fleet client sees identical facts either way.
        let v1 = plan.encode_registry_sync_frame();
        let decoded_v1 = decode_registry_sync_frame(&v1).unwrap();
        assert_eq!(decoded_v1.block_states, decoded.block_states);
        assert_eq!(decoded_v1.facts_bytes, decoded.facts_bytes);

        // An unknown version is still refused, checksum valid and all.
        let mut v3 = plan.encode_registry_sync_frame_v2();
        v3[4] = 0;
        v3[5] = 3;
        let len = v3.len();
        let crc = megu_core::hash::crc32_ieee(&v3[..len - 4]);
        v3[len - 4..].copy_from_slice(&crc.to_be_bytes());
        assert_eq!(
            decode_registry_sync_frame(&v3),
            Err(RegistrySyncError::UnsupportedVersion(3))
        );
    }
}
