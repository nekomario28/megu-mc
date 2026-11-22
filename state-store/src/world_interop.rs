// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
//! Read-only cross-loader world discovery and verified pre-handoff backup.
//!
//! Unknown NBT members retain their original binary tag representation, even
//! when the `DataValue` codec projects numeric widths and array tag types.
//! The pre-handoff snapshot retains the entire original save byte-for-byte.
//! This module never rewrites region files or removes unknown registry entries.

#![deny(unsafe_code)]

use crate::anvil_inspect::{
    decode_nbt_to_data_value, encode_data_value_to_nbt, gzip_decompress, gzip_encode_stored,
    inspect_level_dat, inspect_mca_header, AnvilInspectError, NBT_DEPTH_CAP, NBT_SIZE_CAP,
};
use crate::datafix::DataValue;
use crate::migrate::parse_data_json;
use crate::namespace_guard::{NamespaceGuardError, NamespaceTombstoneSidecar, UnknownEntry};
use crate::world_snapshot::{SnapshotReason, WorldSnapshotError, WorldSnapshotStore};
use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fmt;
use std::fs;
use std::io::{self, Read};
use std::path::{Component, Path, PathBuf};

const MAX_ENTRIES: usize = 250_000;
const MAX_PATH_BYTES: usize = 4_096;
const MAX_DIRECTORY_DEPTH: usize = 32;
const MAX_REGION_BYTES: u64 = 128 * 1_048_576;
const MAX_NBT_FILE_BYTES: u64 = NBT_SIZE_CAP as u64 + 65_536;
const MAX_JSON_FILE_BYTES: u64 = 8 * 1_048_576;

pub const PRESERVED_MOD_ATTACHMENT_KEYS: &[&str] = &[
    "neoforge:attachments",
    "ForgeData",
    "ForgeCaps",
    "fml",
    "fabric:attachments",
    "cardinal_components",
    "DataPacks",
    "WorldGenSettings",
    "CustomBossEvents",
    "DragonFight",
];

const PLAYER_CORE_KEYS: &[&str] = &[
    "DataVersion",
    "Health",
    "Pos",
    "Motion",
    "Rotation",
    "Dimension",
    "foodLevel",
    "foodSaturationLevel",
    "foodExhaustionLevel",
    "XpLevel",
    "XpP",
    "XpTotal",
    "Inventory",
    "EnderItems",
    "SelectedItemSlot",
    "abilities",
    "SpawnX",
    "SpawnY",
    "SpawnZ",
    "OnGround",
    "FallDistance",
    "Fire",
    "Air",
    "UUID",
    "id",
];

const LEVEL_CORE_KEYS: &[&str] = &[
    "DataVersion",
    "LevelName",
    "Version",
    "Time",
    "DayTime",
    "GameType",
    "Difficulty",
    "SpawnX",
    "SpawnY",
    "SpawnZ",
    "RandomSeed",
    "LastPlayed",
    "SizeOnDisk",
    "initialized",
    "allowCommands",
    "hardcore",
    "raining",
    "rainTime",
    "thundering",
    "thunderTime",
    "clearWeatherTime",
    "GameRules",
    "Player",
    "BorderCenterX",
    "BorderCenterZ",
    "BorderSize",
    "BorderSizeLerpTime",
    "BorderSizeLerpTarget",
    "BorderWarningBlocks",
    "BorderWarningTime",
    "BorderSafeZone",
    "BorderDamagePerBlock",
    "WanderingTraderSpawnChance",
    "WanderingTraderSpawnDelay",
];

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WorldLoaderOrigin {
    Vanilla,
    NeoForge,
    Fabric,
    McSpine,
    HybridModded { detected_brands: Vec<String> },
    Unknown,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CompanionFileCounts {
    pub playerdata: usize,
    pub advancements: usize,
    pub stats: usize,
    pub saved_data: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorldDimensionInfo {
    pub id: String,
    pub relative_root: PathBuf,
    pub region_files: usize,
    pub entity_files: usize,
    pub poi_files: usize,
}

#[derive(Clone, Debug, PartialEq)]
pub struct WorldInteropReport {
    pub origin_loader: WorldLoaderOrigin,
    pub target_loader: Option<WorldLoaderOrigin>,
    pub data_version: i32,
    pub level_name: Option<String>,
    pub server_brands: Vec<String>,
    pub data_packs: Option<DataValue>,
    pub world_gen_settings: Option<DataValue>,
    pub loader_tags: BTreeMap<String, DataValue>,
    pub dimensions: Vec<WorldDimensionInfo>,
    pub preserved_attachment_keys: Vec<String>,
    pub companion_files: CompanionFileCounts,
    pub verified_snapshot_id: Option<String>,
    pub snapshot_files_verified: Option<usize>,
    pub parked_unknown_entries: usize,
}

#[derive(Debug)]
pub enum InteropError {
    UnsafePath { path: PathBuf, reason: &'static str },
    InvalidFile { path: PathBuf, reason: String },
    Io { path: PathBuf, source: io::Error },
    Nbt(AnvilInspectError),
    Snapshot(WorldSnapshotError),
    Namespace(NamespaceGuardError),
    InvalidDocument(&'static str),
}

impl fmt::Display for InteropError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsafePath { path, reason } => {
                write!(f, "unsafe path {}: {reason}", path.display())
            }
            Self::InvalidFile { path, reason } => {
                write!(f, "invalid file {}: {reason}", path.display())
            }
            Self::Io { path, source } => write!(f, "{}: {source}", path.display()),
            Self::Nbt(e) => write!(f, "NBT: {e}"),
            Self::Snapshot(e) => write!(f, "snapshot: {e}"),
            Self::Namespace(e) => write!(f, "namespace sidecar: {e}"),
            Self::InvalidDocument(reason) => write!(f, "invalid document: {reason}"),
        }
    }
}

impl Error for InteropError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::Nbt(e) => Some(e),
            Self::Snapshot(e) => Some(e),
            Self::Namespace(e) => Some(e),
            _ => None,
        }
    }
}

impl From<AnvilInspectError> for InteropError {
    fn from(e: AnvilInspectError) -> Self {
        Self::Nbt(e)
    }
}

impl From<WorldSnapshotError> for InteropError {
    fn from(e: WorldSnapshotError) -> Self {
        Self::Snapshot(e)
    }
}

impl From<NamespaceGuardError> for InteropError {
    fn from(e: NamespaceGuardError) -> Self {
        Self::Namespace(e)
    }
}

fn ioe(path: &Path, source: io::Error) -> InteropError {
    InteropError::Io {
        path: path.to_path_buf(),
        source,
    }
}

fn unsafe_path(path: &Path, reason: &'static str) -> InteropError {
    InteropError::UnsafePath {
        path: path.to_path_buf(),
        reason,
    }
}

fn valid_component(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && !name.contains(['\\', '/', ':'])
        && !name.chars().any(char::is_control)
        && name.len() <= MAX_PATH_BYTES
}

/// Validate all existing path components without following any symlink.
fn check_path(path: &Path) -> Result<(), InteropError> {
    if path.as_os_str().len() > MAX_PATH_BYTES {
        return Err(unsafe_path(path, "path exceeds length limit"));
    }
    let mut cursor = PathBuf::new();
    for part in path.components() {
        match part {
            Component::RootDir => cursor.push(part.as_os_str()),
            Component::Normal(name) => {
                if !name.to_str().is_some_and(valid_component) {
                    return Err(unsafe_path(path, "hostile or non-UTF-8 component"));
                }
                cursor.push(name);
                match fs::symlink_metadata(&cursor) {
                    Ok(meta) if meta.file_type().is_symlink() => {
                        return Err(unsafe_path(&cursor, "symlink"));
                    }
                    Ok(_) => {}
                    Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                    Err(e) => return Err(ioe(&cursor, e)),
                }
            }
            Component::CurDir => {}
            Component::ParentDir | Component::Prefix(_) => {
                return Err(unsafe_path(path, "parent traversal or unsupported prefix"));
            }
        }
    }
    Ok(())
}

fn safe_read(path: &Path, cap: u64) -> Result<Vec<u8>, InteropError> {
    check_path(path)?;
    let meta = fs::symlink_metadata(path).map_err(|e| ioe(path, e))?;
    if !meta.is_file() {
        return Err(unsafe_path(path, "expected regular file"));
    }
    if meta.len() > cap {
        return Err(InteropError::InvalidFile {
            path: path.to_path_buf(),
            reason: format!("size exceeds {cap} bytes"),
        });
    }
    // Cap the read itself, including when another process grows the file.
    let mut reader = fs::File::open(path)
        .map_err(|e| ioe(path, e))?
        .take(cap + 1);
    let mut bytes = Vec::new();
    reader.read_to_end(&mut bytes).map_err(|e| ioe(path, e))?;
    if bytes.len() as u64 > cap {
        return Err(InteropError::InvalidFile {
            path: path.to_path_buf(),
            reason: "file grew beyond cap".into(),
        });
    }
    Ok(bytes)
}

fn decode_document(bytes: &[u8]) -> Result<(String, DataValue), InteropError> {
    let inflated;
    let input = if bytes.starts_with(&[0x1f, 0x8b]) {
        inflated = gzip_decompress(bytes, NBT_SIZE_CAP)?;
        inflated.as_slice()
    } else {
        bytes
    };
    let doc = decode_nbt_to_data_value(input)?;
    if doc.1.as_map().is_none() {
        return Err(InteropError::InvalidDocument("NBT root must be a compound"));
    }
    Ok(doc)
}

fn inspect_nbt_file(path: &Path) -> Result<(), InteropError> {
    let bytes = safe_read(path, MAX_NBT_FILE_BYTES)?;
    decode_document(&bytes)?;
    Ok(())
}

fn inspect_json_file(path: &Path) -> Result<(), InteropError> {
    let bytes = safe_read(path, MAX_JSON_FILE_BYTES)?;
    let json = std::str::from_utf8(&bytes).map_err(|_| InteropError::InvalidFile {
        path: path.to_path_buf(),
        reason: "non-UTF-8 JSON".into(),
    })?;
    parse_data_json(json).map_err(|e| InteropError::InvalidFile {
        path: path.to_path_buf(),
        reason: e.to_string(),
    })?;
    Ok(())
}

fn inspect_region(path: &Path) -> Result<(), InteropError> {
    let bytes = safe_read(path, MAX_REGION_BYTES)?;
    if bytes.len() % 4096 != 0 {
        return Err(InteropError::InvalidFile {
            path: path.to_path_buf(),
            reason: "region file is not sector aligned".into(),
        });
    }
    let header = inspect_mca_header(&bytes)?;
    let mut occupied = BTreeSet::new();
    for chunk in &header.chunks {
        for sector in chunk.sector_offset..chunk.sector_offset + u32::from(chunk.sector_count) {
            if !occupied.insert(sector) {
                return Err(InteropError::InvalidFile {
                    path: path.to_path_buf(),
                    reason: "overlapping chunk sectors".into(),
                });
            }
        }
    }
    Ok(())
}

fn region_filename(name: &str) -> bool {
    let Some(stem) = name.strip_prefix("r.").and_then(|s| s.strip_suffix(".mca")) else {
        return false;
    };
    let mut coords = stem.split('.');
    coords.next().and_then(|v| v.parse::<i32>().ok()).is_some()
        && coords.next().and_then(|v| v.parse::<i32>().ok()).is_some()
        && coords.next().is_none()
}

fn resource_segment(s: &str) -> bool {
    !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"_.-".contains(&b))
}

fn dimension_path<'a>(parts: &'a [&'a str]) -> Option<(String, PathBuf, &'a str)> {
    if parts.len() < 2 {
        return None;
    }
    let (id, root, kind) = if parts.len() == 2 && ["region", "entities", "poi"].contains(&parts[0])
    {
        ("minecraft:overworld".to_string(), PathBuf::new(), parts[0])
    } else if parts.len() == 3 && ["region", "entities", "poi"].contains(&parts[1]) {
        let dim = match parts[0] {
            "DIM-1" => "minecraft:the_nether",
            "DIM1" => "minecraft:the_end",
            _ => return None,
        };
        (dim.to_string(), PathBuf::from(parts[0]), parts[1])
    } else if parts.len() >= 5 && parts[0] == "dimensions" {
        let kind = parts[parts.len() - 2];
        if !["region", "entities", "poi"].contains(&kind)
            || !parts[1..parts.len() - 2]
                .iter()
                .all(|s| resource_segment(s))
        {
            return None;
        }
        let id = format!("{}:{}", parts[1], parts[2..parts.len() - 2].join("/"));
        let root = parts[..parts.len() - 2].iter().collect::<PathBuf>();
        (id, root, kind)
    } else {
        return None;
    };
    Some((id, root, kind))
}

fn find_map<'a>(doc: &'a DataValue, key: &str) -> Option<&'a BTreeMap<String, DataValue>> {
    doc.as_map()
        .and_then(|m| m.get(key))
        .and_then(DataValue::as_map)
}

fn attachment_key(key: &str) -> bool {
    PRESERVED_MOD_ATTACHMENT_KEYS.contains(&key)
}

/// Collect every un-managed compound member, including all known loader keys.
pub fn extract_preserved_attachments(
    doc: &DataValue,
    known_managed_keys: &[&str],
) -> BTreeMap<String, DataValue> {
    doc.as_map()
        .map(|map| {
            map.iter()
                .filter(|(key, _)| {
                    attachment_key(key) || !known_managed_keys.contains(&key.as_str())
                })
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect()
        })
        .unwrap_or_default()
}

/// Fill missing members. Existing fields, including core edits, take priority.
pub fn merge_preserved_attachments(
    target: &mut DataValue,
    preserved: &BTreeMap<String, DataValue>,
) {
    if let Some(map) = target.as_map_mut() {
        for (key, val) in preserved {
            map.entry(key.clone()).or_insert_with(|| val.clone());
        }
    }
}

/// Captured non-core members from either level.dat or playerdata.
#[derive(Clone, Debug, PartialEq)]
pub struct PreservedNbtEnvelope {
    pub root_name: String,
    pub root_attachments: BTreeMap<String, DataValue>,
    pub data_attachments: BTreeMap<String, DataValue>,
}

fn restore_attachments(
    target: &mut DataValue,
    entries: &BTreeMap<String, DataValue>,
) -> Result<(), InteropError> {
    let map = target.as_map_mut().ok_or(InteropError::InvalidDocument(
        "updated NBT compound missing",
    ))?;
    for (key, value) in entries {
        map.insert(key.clone(), value.clone());
    }
    Ok(())
}

// Inspect raw NBT tag boundaries after the shared codec has checked its
// structure. Reusing unchanged spans preserves Byte/Short/Float and the
// ByteArray/IntArray/LongArray wire types absent from DataValue.
struct RawNbtCursor<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> RawNbtCursor<'a> {
    fn take(&mut self, len: usize) -> Result<&'a [u8], InteropError> {
        let end = self
            .pos
            .checked_add(len)
            .filter(|end| *end <= self.bytes.len())
            .ok_or(InteropError::InvalidDocument("truncated raw NBT tag"))?;
        let bytes = &self.bytes[self.pos..end];
        self.pos = end;
        Ok(bytes)
    }

    fn u8(&mut self) -> Result<u8, InteropError> {
        Ok(self.take(1)?[0])
    }

    fn u16(&mut self) -> Result<usize, InteropError> {
        let bytes: [u8; 2] = self
            .take(2)?
            .try_into()
            .map_err(|_| InteropError::InvalidDocument("invalid NBT string size"))?;
        Ok(usize::from(u16::from_be_bytes(bytes)))
    }

    fn count(&mut self) -> Result<usize, InteropError> {
        let bytes: [u8; 4] = self
            .take(4)?
            .try_into()
            .map_err(|_| InteropError::InvalidDocument("invalid NBT array size"))?;
        usize::try_from(i32::from_be_bytes(bytes))
            .map_err(|_| InteropError::InvalidDocument("negative NBT array size"))
    }

    fn skip_payload(&mut self, id: u8, depth: usize) -> Result<(), InteropError> {
        if depth > NBT_DEPTH_CAP {
            return Err(InteropError::InvalidDocument("raw NBT exceeds nesting cap"));
        }
        match id {
            1 => {
                self.take(1)?;
            }
            2 => {
                self.take(2)?;
            }
            3 | 5 => {
                self.take(4)?;
            }
            4 | 6 => {
                self.take(8)?;
            }
            7 | 11 | 12 => {
                let count = self.count()?;
                let width = match id {
                    7 => 1,
                    11 => 4,
                    _ => 8,
                };
                let size = count
                    .checked_mul(width)
                    .ok_or(InteropError::InvalidDocument("NBT array size overflow"))?;
                self.take(size)?;
            }
            8 => {
                let size = self.u16()?;
                self.take(size)?;
            }
            9 => {
                let kind = self.u8()?;
                let count = self.count()?;
                if kind > 12 || (kind == 0 && count != 0) {
                    return Err(InteropError::InvalidDocument("invalid NBT list type"));
                }
                for _ in 0..count {
                    self.skip_payload(kind, depth + 1)?;
                }
            }
            10 => loop {
                let kind = self.u8()?;
                if kind == 0 {
                    break;
                }
                if kind > 12 {
                    return Err(InteropError::InvalidDocument("invalid NBT compound tag"));
                }
                let size = self.u16()?;
                self.take(size)?;
                self.skip_payload(kind, depth + 1)?;
            },
            _ => return Err(InteropError::InvalidDocument("invalid raw NBT tag")),
        }
        Ok(())
    }
}

struct RawNbtField<'a> {
    bytes: &'a [u8],
    payload_start: usize,
    id: u8,
}

fn raw_compound_fields(bytes: &[u8]) -> Result<BTreeMap<&[u8], RawNbtField<'_>>, InteropError> {
    let mut cursor = RawNbtCursor { bytes, pos: 0 };
    let mut fields = BTreeMap::new();
    loop {
        let start = cursor.pos;
        let id = cursor.u8()?;
        if id == 0 {
            break;
        }
        if id > 12 {
            return Err(InteropError::InvalidDocument("invalid raw NBT field type"));
        }
        let size = cursor.u16()?;
        let name = cursor.take(size)?;
        let payload_start = cursor.pos - start;
        cursor.skip_payload(id, 1)?;
        if fields
            .insert(
                name,
                RawNbtField {
                    bytes: &bytes[start..cursor.pos],
                    payload_start,
                    id,
                },
            )
            .is_some()
        {
            return Err(InteropError::InvalidDocument(
                "duplicate raw NBT field name",
            ));
        }
    }
    if cursor.pos != bytes.len() {
        return Err(InteropError::InvalidDocument(
            "trailing data in raw NBT compound",
        ));
    }
    Ok(fields)
}

fn raw_root_payload(bytes: &[u8]) -> Result<&[u8], InteropError> {
    let mut cursor = RawNbtCursor { bytes, pos: 0 };
    if cursor.u8()? != 10 {
        return Err(InteropError::InvalidDocument("NBT root is not a compound"));
    }
    let size = cursor.u16()?;
    cursor.take(size)?;
    Ok(&bytes[cursor.pos..])
}

fn mutf8_name(name: &str) -> Vec<u8> {
    let mut bytes = Vec::new();
    for unit in name.encode_utf16() {
        if (1..=127).contains(&unit) {
            bytes.push(unit as u8);
        } else if unit <= 0x07ff {
            bytes.push(0xc0 | (unit >> 6) as u8);
            bytes.push(0x80 | (unit & 63) as u8);
        } else {
            bytes.push(0xe0 | (unit >> 12) as u8);
            bytes.push(0x80 | ((unit >> 6) & 63) as u8);
            bytes.push(0x80 | (unit & 63) as u8);
        }
    }
    bytes
}

fn same_nbt_value(a: &DataValue, b: &DataValue) -> bool {
    match (a, b) {
        (DataValue::Float(left), DataValue::Float(right)) => left.to_bits() == right.to_bits(),
        (DataValue::List(left), DataValue::List(right)) => {
            left.len() == right.len() && left.iter().zip(right).all(|(x, y)| same_nbt_value(x, y))
        }
        (DataValue::Map(left), DataValue::Map(right)) => {
            left.len() == right.len()
                && left
                    .iter()
                    .all(|(k, v)| right.get(k).is_some_and(|r| same_nbt_value(v, r)))
        }
        _ => a == b,
    }
}

/// Keep the original numeric wire tag when changing a core field. Vanilla
/// expects specific widths for fields such as DayTime (Long) and Health
/// (Float); silently changing them to Int/Double can make saves unreadable.
fn updated_scalar_with_original_type(
    original: &RawNbtField<'_>,
    value: &DataValue,
) -> Result<Option<Vec<u8>>, InteropError> {
    let payload = match (original.id, value) {
        (1, DataValue::Int(n)) => i8::try_from(*n).map(|v| v.to_be_bytes().to_vec()).ok(),
        (2, DataValue::Int(n)) => i16::try_from(*n).map(|v| v.to_be_bytes().to_vec()).ok(),
        (3, DataValue::Int(n)) => i32::try_from(*n).map(|v| v.to_be_bytes().to_vec()).ok(),
        (4, DataValue::Int(n)) => Some(n.to_be_bytes().to_vec()),
        (1, DataValue::Bool(n)) => Some(vec![u8::from(*n)]),
        (5, DataValue::Float(n)) => {
            let narrowed = *n as f32;
            if f64::from(narrowed).to_bits() == n.to_bits() || (n.is_nan() && narrowed.is_nan()) {
                Some(narrowed.to_be_bytes().to_vec())
            } else {
                None
            }
        }
        (6, DataValue::Float(n)) => Some(n.to_be_bytes().to_vec()),
        _ => return Ok(None),
    };
    let payload = payload.ok_or(InteropError::InvalidDocument(
        "updated core value cannot fit original NBT numeric tag",
    ))?;
    let mut encoded = original.bytes[..original.payload_start].to_vec();
    encoded.extend_from_slice(&payload);
    Ok(Some(encoded))
}

fn preserve_raw_compound(
    original: &[u8],
    updated: &[u8],
    before: &BTreeMap<String, DataValue>,
    after: &BTreeMap<String, DataValue>,
) -> Result<Vec<u8>, InteropError> {
    let original_fields = raw_compound_fields(original)?;
    let updated_fields = raw_compound_fields(updated)?;
    let mut result = Vec::new();
    for (name, value) in after {
        let encoded_name = mutf8_name(name);
        let new_field = updated_fields
            .get(encoded_name.as_slice())
            .ok_or(InteropError::InvalidDocument("updated NBT field missing"))?;
        if let Some(old_value) = before.get(name) {
            let old_field = original_fields.get(encoded_name.as_slice()).ok_or(
                InteropError::InvalidDocument("original NBT field name was not canonical"),
            )?;
            if same_nbt_value(old_value, value) {
                result.extend_from_slice(old_field.bytes);
                continue;
            }
            if let Some(encoded) = updated_scalar_with_original_type(old_field, value)? {
                result.extend_from_slice(&encoded);
                continue;
            }
            if let (DataValue::Map(old_map), DataValue::Map(new_map)) = (old_value, value) {
                if old_field.id == 10 && new_field.id == 10 {
                    result.extend_from_slice(&old_field.bytes[..old_field.payload_start]);
                    result.extend_from_slice(&preserve_raw_compound(
                        &old_field.bytes[old_field.payload_start..],
                        &new_field.bytes[new_field.payload_start..],
                        old_map,
                        new_map,
                    )?);
                    continue;
                }
            }
        }
        result.extend_from_slice(new_field.bytes);
    }
    result.push(0);
    if result.len() > NBT_SIZE_CAP {
        return Err(InteropError::InvalidDocument(
            "patched NBT exceeds size cap",
        ));
    }
    Ok(result)
}

fn roundtrip_document(
    bytes: &[u8],
    level: bool,
    update: impl FnOnce(&mut DataValue),
) -> Result<Vec<u8>, InteropError> {
    let raw = if bytes.starts_with(&[0x1f, 0x8b]) {
        gzip_decompress(bytes, NBT_SIZE_CAP)?
    } else {
        bytes.to_vec()
    };
    let (root_name, mut doc) = decode_nbt_to_data_value(&raw)?;
    if doc.as_map().is_none() {
        return Err(InteropError::InvalidDocument("NBT root must be a compound"));
    }
    if level {
        inspect_level_dat(bytes)?;
    }
    let before = doc.clone();
    let preserved = PreservedNbtEnvelope {
        root_name: root_name.clone(),
        root_attachments: extract_preserved_attachments(
            &doc,
            if level { &["Data"] } else { PLAYER_CORE_KEYS },
        ),
        data_attachments: if level {
            let data = find_map(&doc, "Data")
                .ok_or(InteropError::InvalidDocument("missing Data compound"))?;
            extract_preserved_attachments(&DataValue::Map(data.clone()), LEVEL_CORE_KEYS)
        } else {
            BTreeMap::new()
        },
    };
    update(&mut doc);
    restore_attachments(&mut doc, &preserved.root_attachments)?;
    if level {
        let data = doc.as_map_mut().and_then(|map| map.get_mut("Data")).ok_or(
            InteropError::InvalidDocument("updated level.dat removed Data"),
        )?;
        restore_attachments(data, &preserved.data_attachments)?;
    }
    let canonical = encode_data_value_to_nbt(&preserved.root_name, &doc)?;
    let old_root = before
        .as_map()
        .ok_or(InteropError::InvalidDocument("missing original root"))?;
    let new_root = doc
        .as_map()
        .ok_or(InteropError::InvalidDocument("missing updated root"))?;
    let original_payload = raw_root_payload(&raw)?;
    let updated_payload = raw_root_payload(&canonical)?;
    let mut nbt = raw[..raw.len() - original_payload.len()].to_vec();
    nbt.extend_from_slice(&preserve_raw_compound(
        original_payload,
        updated_payload,
        old_root,
        new_root,
    )?);
    if nbt.len() > NBT_SIZE_CAP {
        return Err(InteropError::InvalidDocument("NBT size cap exceeded"));
    }
    let (_, decoded) = decode_nbt_to_data_value(&nbt)?;
    if !same_nbt_value(&decoded, &doc) {
        return Err(InteropError::InvalidDocument(
            "NBT encode/decode changed document values",
        ));
    }
    if level {
        inspect_level_dat(&nbt)?;
    }
    Ok(gzip_encode_stored(&nbt))
}

pub fn roundtrip_playerdata_preserving_attachments(
    gzip_bytes: &[u8],
    update: impl FnOnce(&mut DataValue),
) -> Result<Vec<u8>, InteropError> {
    if !gzip_bytes.starts_with(&[0x1f, 0x8b]) {
        return Err(InteropError::InvalidDocument(
            "playerdata requires gzip NBT",
        ));
    }
    roundtrip_document(gzip_bytes, false, update)
}

pub fn roundtrip_level_dat_preserving_attachments(
    gzip_bytes: &[u8],
    update: impl FnOnce(&mut DataValue),
) -> Result<Vec<u8>, InteropError> {
    if !gzip_bytes.starts_with(&[0x1f, 0x8b]) {
        return Err(InteropError::InvalidDocument("level.dat requires gzip NBT"));
    }
    roundtrip_document(gzip_bytes, true, update)
}

fn origin_for(brands: &[String], tags: &BTreeMap<String, DataValue>) -> WorldLoaderOrigin {
    let mut observed = BTreeSet::new();
    for brand in brands {
        let lowered = brand.to_ascii_lowercase();
        match lowered.as_str() {
            "vanilla" | "neoforge" | "fabric" | "mc-spine" => {
                observed.insert(lowered);
            }
            _ => {}
        }
    }
    if ["fml", "ForgeData", "ForgeCaps", "neoforge:attachments"]
        .iter()
        .any(|k| tags.contains_key(*k))
    {
        observed.insert("neoforge".to_string());
    }
    if ["fabric:attachments", "cardinal_components"]
        .iter()
        .any(|k| tags.contains_key(*k))
    {
        observed.insert("fabric".to_string());
    }
    if observed.len() > 1 {
        return WorldLoaderOrigin::HybridModded {
            detected_brands: observed.into_iter().collect(),
        };
    }
    match observed.iter().next().map(String::as_str) {
        Some("vanilla") => WorldLoaderOrigin::Vanilla,
        Some("neoforge") => WorldLoaderOrigin::NeoForge,
        Some("fabric") => WorldLoaderOrigin::Fabric,
        Some("mc-spine") => WorldLoaderOrigin::McSpine,
        _ if brands.is_empty() => WorldLoaderOrigin::Vanilla,
        _ => WorldLoaderOrigin::Unknown,
    }
}

fn server_brands(data: &BTreeMap<String, DataValue>) -> Vec<String> {
    match data.get("ServerBrands") {
        Some(DataValue::List(brands)) => brands
            .iter()
            .filter_map(DataValue::as_str)
            .map(str::to_string)
            .collect(),
        Some(DataValue::Str(s)) => vec![s.clone()],
        _ => Vec::new(),
    }
}

/// Walk the world without following symlinks and validate every recognized
/// region and companion file before producing a report.
pub fn inspect_world_interop(world_root: &Path) -> Result<WorldInteropReport, InteropError> {
    check_path(world_root)?;
    if !fs::symlink_metadata(world_root)
        .map_err(|e| ioe(world_root, e))?
        .is_dir()
    {
        return Err(unsafe_path(world_root, "world root is not a directory"));
    }
    let level_bytes = safe_read(&world_root.join("level.dat"), MAX_NBT_FILE_BYTES)?;
    let summary = inspect_level_dat(&level_bytes)?;
    let data =
        find_map(&summary.root, "Data").ok_or(InteropError::InvalidDocument("missing Data"))?;
    let brands = server_brands(data);
    let mut loader_tags = BTreeMap::new();
    let mut attached = BTreeSet::new();
    for map in [summary.root.as_map(), Some(data)].into_iter().flatten() {
        for (key, value) in map {
            if attachment_key(key) {
                attached.insert(key.clone());
                loader_tags.insert(key.clone(), value.clone());
            }
        }
    }
    let mut dimensions = BTreeMap::<String, WorldDimensionInfo>::new();
    for (id, root) in [
        ("minecraft:overworld", ""),
        ("minecraft:the_nether", "DIM-1"),
        ("minecraft:the_end", "DIM1"),
    ] {
        dimensions.insert(
            id.to_string(),
            WorldDimensionInfo {
                id: id.to_string(),
                relative_root: PathBuf::from(root),
                region_files: 0,
                entity_files: 0,
                poi_files: 0,
            },
        );
    }
    let mut companion = CompanionFileCounts::default();
    let mut count = 0;
    let mut stack = vec![(world_root.to_path_buf(), Vec::<String>::new())];
    while let Some((dir, rel)) = stack.pop() {
        let entries = fs::read_dir(&dir).map_err(|e| ioe(&dir, e))?;
        for entry in entries {
            let entry = entry.map_err(|e| ioe(&dir, e))?;
            count += 1;
            if count > MAX_ENTRIES {
                return Err(unsafe_path(&dir, "world entry count exceeds cap"));
            }
            let filename = entry.file_name();
            let name = filename
                .to_str()
                .ok_or_else(|| unsafe_path(&entry.path(), "non-UTF-8 filename"))?;
            if !valid_component(name) {
                return Err(unsafe_path(&entry.path(), "hostile filename"));
            }
            let mut rel_path = rel.clone();
            rel_path.push(name.to_string());
            if rel_path.len() > MAX_DIRECTORY_DEPTH {
                return Err(unsafe_path(&entry.path(), "directory nesting exceeds cap"));
            }
            let path = entry.path();
            let typ = entry.file_type().map_err(|e| ioe(&path, e))?;
            if typ.is_symlink() || (!typ.is_file() && !typ.is_dir()) {
                return Err(unsafe_path(&path, "symlink or special file"));
            }
            if typ.is_dir() {
                stack.push((path, rel_path));
                continue;
            }
            let parts: Vec<&str> = rel_path.iter().map(String::as_str).collect();
            if let Some((id, root, kind)) = dimension_path(&parts) {
                if !region_filename(name) {
                    if name.ends_with(".mca") {
                        return Err(unsafe_path(&path, "invalid region filename"));
                    }
                    continue;
                }
                inspect_region(&path)?;
                let dimension =
                    dimensions
                        .entry(id.clone())
                        .or_insert_with(|| WorldDimensionInfo {
                            id,
                            relative_root: root,
                            region_files: 0,
                            entity_files: 0,
                            poi_files: 0,
                        });
                match kind {
                    "region" => dimension.region_files += 1,
                    "entities" => dimension.entity_files += 1,
                    _ => dimension.poi_files += 1,
                }
            } else if name.ends_with(".mca") {
                return Err(InteropError::InvalidFile {
                    path,
                    reason: "unrecognized region/dimension layout".into(),
                });
            } else if parts.len() == 2 {
                match (parts[0], name) {
                    ("playerdata", n) if n.ends_with(".dat") => {
                        inspect_nbt_file(&path)?;
                        companion.playerdata += 1;
                    }
                    ("advancements", n) if n.ends_with(".json") => {
                        inspect_json_file(&path)?;
                        companion.advancements += 1;
                    }
                    ("stats", n) if n.ends_with(".json") => {
                        inspect_json_file(&path)?;
                        companion.stats += 1;
                    }
                    ("data", n) if n.ends_with(".dat") => {
                        inspect_nbt_file(&path)?;
                        companion.saved_data += 1;
                    }
                    _ => {}
                }
            }
        }
    }
    Ok(WorldInteropReport {
        origin_loader: origin_for(&brands, &loader_tags),
        target_loader: None,
        data_version: summary.data_version,
        level_name: summary.level_name,
        server_brands: brands,
        data_packs: data.get("DataPacks").cloned(),
        world_gen_settings: data.get("WorldGenSettings").cloned(),
        loader_tags,
        dimensions: dimensions.into_values().collect(),
        preserved_attachment_keys: attached.into_iter().collect(),
        companion_files: companion,
        verified_snapshot_id: None,
        snapshot_files_verified: None,
        parked_unknown_entries: 0,
    })
}

/// Caller-supplied inventory of unknown registry objects makes sidecar
/// preparation possible without interpreting unsupported modded chunk codecs.
/// The world itself stays byte-identical during handoff preparation.
pub struct CrossLoaderHandoff<'a> {
    pub world_root: &'a Path,
    pub backup_root: &'a Path,
    pub sidecar_path: &'a Path,
    pub target_loader: WorldLoaderOrigin,
    pub active_namespaces: &'a BTreeSet<String>,
    pub discovered_unknown: &'a [UnknownEntry],
    pub mc_version: &'a str,
    pub timestamp_secs: u64,
}

pub fn prepare_cross_loader_handoff(
    options: CrossLoaderHandoff<'_>,
) -> Result<WorldInteropReport, InteropError> {
    let mut report = inspect_world_interop(options.world_root)?;
    check_path(options.backup_root)?;
    check_path(options.sidecar_path)?;
    if !options.sidecar_path.starts_with(options.world_root) {
        return Err(unsafe_path(
            options.sidecar_path,
            "sidecar must reside under world root",
        ));
    }
    let store = WorldSnapshotStore::new(options.world_root, options.backup_root)?;
    let snapshot = store.create_snapshot(
        SnapshotReason::PreModChange {
            summary: format!("cross-loader handoff to {:?}", options.target_loader),
        },
        options.mc_version,
        options.timestamp_secs,
    )?;
    let verified = store.verify_snapshot(&snapshot.snapshot_id)?;
    if !options.discovered_unknown.is_empty() {
        let mut sidecar = match fs::symlink_metadata(options.sidecar_path) {
            Ok(meta) if meta.is_file() => {
                NamespaceTombstoneSidecar::load_verified(options.sidecar_path)?
            }
            Ok(_) => {
                return Err(unsafe_path(
                    options.sidecar_path,
                    "sidecar must be a regular file",
                ))
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => NamespaceTombstoneSidecar::new(),
            Err(e) => return Err(ioe(options.sidecar_path, e)),
        };
        for unknown in options.discovered_unknown {
            if sidecar
                .preserve_unknown(
                    unknown.clone(),
                    options.timestamp_secs,
                    options.active_namespaces,
                )?
                .is_some()
            {
                report.parked_unknown_entries += 1;
            }
        }
        if report.parked_unknown_entries > 0 {
            sidecar.save_atomic(options.sidecar_path)?;
        }
    }
    report.target_loader = Some(options.target_loader);
    report.verified_snapshot_id = Some(snapshot.snapshot_id);
    report.snapshot_files_verified = Some(verified.files_verified);
    Ok(report)
}
