// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later

//! Deterministic multi-mod dynamic registry planning and its bounded MRS1
//! server-to-client synchronization frame.

use std::collections::{BTreeMap, BTreeSet};

use crate::auto_adapter::ModsDirAdaptationReport;
use crate::blockstates::PropertyDef;

pub const MOD_REGISTRY_SYNC_CHANNEL: &str = "megu:mod_registry_sync_v1";

const WIRE_MAGIC: &[u8; 4] = b"MRS1";
const WIRE_VERSION: u16 = 1;
const MAX_BLOCK_STATES: usize = 65_536;
const MAX_ITEMS: usize = 65_536;
const MAX_PROPERTIES: usize = 64;
const MAX_STRING_BYTES: usize = u16::MAX as usize;
const MAX_FRAME_BYTES: usize = 8 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DynamicBlockStateEntry {
    pub state_id: u32,
    pub block_id: String,
    pub properties: Vec<(String, String)>,
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

        for jar in &report.jars {
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
                block_states.push(DynamicBlockStateEntry {
                    state_id,
                    block_id: block_id.clone(),
                    properties: state_properties,
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
        let checksum = crc32(&out);
        put_u32(&mut out, checksum);
        out
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SyncedModRegistry {
    pub base_block_state_id: u32,
    pub base_item_id: u32,
    pub block_states: Vec<DynamicBlockStateEntry>,
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
    if crc32(&bytes[..payload_len]) != expected_crc {
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
    if version != WIRE_VERSION {
        return Err(RegistrySyncError::UnsupportedVersion(version));
    }
    let base_block_state_id = reader.u32()?;
    let base_item_id = reader.u32()?;
    let state_count = reader.u32()? as usize;
    if state_count > MAX_BLOCK_STATES {
        return Err(RegistrySyncError::TooManyBlockStates);
    }
    let mut block_states = Vec::with_capacity(state_count);
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
        block_states.push(DynamicBlockStateEntry {
            state_id,
            block_id,
            properties,
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

fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &byte in bytes {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            let mask = 0u32.wrapping_sub(crc & 1);
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    !crc
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
