// SPDX-License-Identifier: LGPL-3.0-or-later
//! Validated, read-only resource, shader, and datapack loading.
//!
//! Pack contents are interpreted only after the complete, immutable ZIP image
//! passes `pack_router`'s path, CRC, expansion, texture, and executable checks.

#![deny(unsafe_code)]

use crate::json::{self, Json};
use crate::optifine_compat::{self, CtmRule, EmissiveConfig, JemModel, JpmModel};
use crate::pack_router::{
    self, PackKind, PackRouteDecision, PackRouterPolicy, PackRoutingReport, PackThreat,
    PackThreatKind, MAX_ENTRIES, MAX_TOTAL_UNCOMPRESSED,
};
use std::collections::BTreeMap;
use std::fmt;
use std::fs::{self, File};
use std::io::{self, Read};
use std::path::Path;

const MAX_DOCUMENT_BYTES: usize = 1024 * 1024;

/// Parsed content from a fully inspected pack. Map keys are ZIP-relative paths.
#[derive(Debug)]
pub struct LoadedPack {
    pub kind: PackKind,
    pub inspection: PackRoutingReport,
    /// Shader-only packs may omit `pack.mcmeta`.
    pub pack_format: Option<u32>,
    pub blockstates: BTreeMap<String, Json>,
    pub models: BTreeMap<String, Json>,
    pub jem_models: BTreeMap<String, JemModel>,
    pub jpm_models: BTreeMap<String, JpmModel>,
    pub ctm_rules: BTreeMap<String, CtmRule>,
    pub emissive: BTreeMap<String, EmissiveConfig>,
    /// Optional, malformed resource entries are reported and omitted.
    pub warnings: Vec<String>,
}

#[derive(Debug)]
pub enum PackLoadError {
    Io(io::Error),
    Quarantined(Vec<PackThreat>),
    InvalidPack(String),
}

impl fmt::Display for PackLoadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(f, "pack I/O error: {error}"),
            Self::Quarantined(threats) => write!(f, "pack quarantined: {threats:?}"),
            Self::InvalidPack(reason) => write!(f, "invalid pack: {reason}"),
        }
    }
}

impl std::error::Error for PackLoadError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            _ => None,
        }
    }
}

impl From<io::Error> for PackLoadError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

fn invalid(message: impl Into<String>) -> PackLoadError {
    PackLoadError::InvalidPack(message.into())
}

fn directory_threat(kind: PackThreatKind, name: &str, detail: &str) -> PackLoadError {
    PackLoadError::Quarantined(vec![PackThreat {
        kind,
        member: name.to_owned(),
        detail: detail.to_owned(),
    }])
}

/// Snapshot a directory into a bounded stored ZIP, then apply the same
/// authoritative validation as byte-backed archives. Symlinks and special
/// filesystem nodes are rejected, including links to external directories.
pub fn load_validated_pack_dir(dir: &Path) -> Result<LoadedPack, PackLoadError> {
    let metadata = fs::symlink_metadata(dir)?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(invalid("pack root must be a real directory"));
    }

    let mut archive = Vec::new();
    let mut central = Vec::new();
    let mut count = 0usize;
    let mut total = 0u64;
    let mut pending = vec![(dir.to_owned(), String::new())];
    while let Some((folder, prefix)) = pending.pop() {
        let mut children = fs::read_dir(folder)?.collect::<Result<Vec<_>, _>>()?;
        children.sort_by_key(|entry| entry.file_name());
        for child in children {
            let name = child
                .file_name()
                .into_string()
                .map_err(|_| invalid("non-UTF-8 filesystem entry name"))?;
            let member = format!("{prefix}{name}");
            let path = child.path();
            let metadata = fs::symlink_metadata(&path)?;
            if metadata.file_type().is_symlink() || !metadata.is_file() && !metadata.is_dir() {
                return Err(directory_threat(
                    PackThreatKind::EncryptedOrUnsupportedArchive,
                    &member,
                    "symlink or special filesystem node",
                ));
            }
            if metadata.is_dir() {
                pending.push((path, format!("{member}/")));
                continue;
            }
            if count >= MAX_ENTRIES.min(u16::MAX as usize - 1) {
                return Err(directory_threat(
                    PackThreatKind::ZipBomb,
                    &member,
                    "too many directory entries",
                ));
            }
            let remaining = MAX_TOTAL_UNCOMPRESSED.saturating_sub(total);
            if metadata.len() > remaining {
                return Err(directory_threat(
                    PackThreatKind::ZipBomb,
                    &member,
                    "directory contents exceed uncompressed size cap",
                ));
            }
            let file = File::open(&path)?;
            if !file.metadata()?.is_file() || fs::symlink_metadata(&path)?.file_type().is_symlink()
            {
                return Err(directory_threat(
                    PackThreatKind::EncryptedOrUnsupportedArchive,
                    &member,
                    "file changed to a symlink or special node",
                ));
            }
            let mut contents = Vec::new();
            file.take(remaining + 1).read_to_end(&mut contents)?;
            total = total.saturating_add(contents.len() as u64);
            if total > MAX_TOTAL_UNCOMPRESSED {
                return Err(directory_threat(
                    PackThreatKind::ZipBomb,
                    &member,
                    "directory contents exceed uncompressed size cap",
                ));
            }
            append_stored(&mut archive, &mut central, &member, &contents)?;
            count += 1;
        }
    }
    let offset = u32::try_from(archive.len()).map_err(|_| invalid("ZIP offset overflow"))?;
    let size = u32::try_from(central.len()).map_err(|_| invalid("ZIP index overflow"))?;
    archive.extend(central);
    archive.extend(0x0605_4b50u32.to_le_bytes());
    archive.extend([0; 4]);
    archive.extend((count as u16).to_le_bytes());
    archive.extend((count as u16).to_le_bytes());
    archive.extend(size.to_le_bytes());
    archive.extend(offset.to_le_bytes());
    archive.extend(0u16.to_le_bytes());
    load_validated_pack_zip(&archive)
}

fn append_stored(
    archive: &mut Vec<u8>,
    central: &mut Vec<u8>,
    name: &str,
    contents: &[u8],
) -> Result<(), PackLoadError> {
    let name_len = u16::try_from(name.len()).map_err(|_| {
        directory_threat(PackThreatKind::PathTraversal, name, "entry name too long")
    })?;
    let data_len = u32::try_from(contents.len()).map_err(|_| invalid("file too large"))?;
    let start = u32::try_from(archive.len()).map_err(|_| invalid("ZIP offset overflow"))?;
    let crc = crc32(contents);
    archive.extend(0x0403_4b50u32.to_le_bytes());
    archive.extend(20u16.to_le_bytes());
    archive.extend(0u16.to_le_bytes());
    archive.extend(0u16.to_le_bytes());
    archive.extend([0; 4]);
    archive.extend(crc.to_le_bytes());
    archive.extend(data_len.to_le_bytes());
    archive.extend(data_len.to_le_bytes());
    archive.extend(name_len.to_le_bytes());
    archive.extend(0u16.to_le_bytes());
    archive.extend(name.as_bytes());
    archive.extend(contents);

    central.extend(0x0201_4b50u32.to_le_bytes());
    central.extend(20u16.to_le_bytes());
    central.extend(20u16.to_le_bytes());
    central.extend(0u16.to_le_bytes());
    central.extend(0u16.to_le_bytes());
    central.extend([0; 4]);
    central.extend(crc.to_le_bytes());
    central.extend(data_len.to_le_bytes());
    central.extend(data_len.to_le_bytes());
    central.extend(name_len.to_le_bytes());
    central.extend(0u16.to_le_bytes());
    central.extend(0u16.to_le_bytes());
    central.extend(0u16.to_le_bytes());
    central.extend(0u16.to_le_bytes());
    central.extend(0u32.to_le_bytes());
    central.extend(start.to_le_bytes());
    central.extend(name.as_bytes());
    Ok(())
}

/// Validate all members and graphs before decoding any individual pack asset.
pub fn load_validated_pack_zip(bytes: &[u8]) -> Result<LoadedPack, PackLoadError> {
    let inspection = pack_router::inspect_and_route_pack(bytes, &PackRouterPolicy::default());
    if let PackRouteDecision::Quarantine { threats } = &inspection.decision {
        return Err(PackLoadError::Quarantined(threats.clone()));
    }
    let mut pack = LoadedPack {
        kind: inspection.kind,
        inspection,
        pack_format: None,
        blockstates: BTreeMap::new(),
        models: BTreeMap::new(),
        jem_models: BTreeMap::new(),
        jpm_models: BTreeMap::new(),
        ctm_rules: BTreeMap::new(),
        emissive: BTreeMap::new(),
        warnings: Vec::new(),
    };

    for entry in indexed_members(bytes)? {
        let Some(kind) = asset_kind(&entry.name) else {
            continue;
        };
        if entry.uncompressed_len > MAX_DOCUMENT_BYTES {
            if kind == AssetKind::Metadata {
                return Err(invalid("pack.mcmeta exceeds metadata size limit"));
            }
            pack.warnings
                .push(format!("{}: document exceeds 1 MiB", entry.name));
            continue;
        }
        let content = extract_member(bytes, &entry)?;
        let text = match std::str::from_utf8(&content) {
            Ok(text) => text,
            Err(error) if kind != AssetKind::Metadata => {
                pack.warnings
                    .push(format!("{}: invalid UTF-8: {error}", entry.name));
                continue;
            }
            Err(error) => return Err(invalid(format!("pack.mcmeta: invalid UTF-8: {error}"))),
        };
        if kind == AssetKind::Metadata {
            let metadata =
                json::parse(text).map_err(|error| invalid(format!("pack.mcmeta: {error}")))?;
            let format = metadata
                .get("pack")
                .and_then(|value| value.get("pack_format"))
                .and_then(Json::as_f64)
                .ok_or_else(|| invalid("pack.mcmeta missing numeric pack.pack_format"))?;
            if !format.is_finite() || format.fract() != 0.0 || !(8.0..=48.0).contains(&format) {
                return Err(invalid(format!(
                    "unsupported pack_format {format} (expected 8..=48)"
                )));
            }
            pack.pack_format = Some(format as u32);
            continue;
        }

        match kind {
            AssetKind::Blockstate | AssetKind::Model => match json::parse(text) {
                Ok(json) => {
                    if kind == AssetKind::Blockstate {
                        pack.blockstates.insert(entry.name, json);
                    } else {
                        pack.models.insert(entry.name, json);
                    }
                }
                Err(error) => pack.warnings.push(format!("{}: {error}", entry.name)),
            },
            AssetKind::Jem => record(
                &mut pack.jem_models,
                &mut pack.warnings,
                entry.name,
                optifine_compat::parse_jem(text),
            ),
            AssetKind::Jpm => record(
                &mut pack.jpm_models,
                &mut pack.warnings,
                entry.name,
                optifine_compat::parse_jpm(text),
            ),
            AssetKind::Ctm => record(
                &mut pack.ctm_rules,
                &mut pack.warnings,
                entry.name,
                optifine_compat::parse_ctm_properties(text),
            ),
            AssetKind::Emissive => record(
                &mut pack.emissive,
                &mut pack.warnings,
                entry.name,
                optifine_compat::parse_emissive_properties(text),
            ),
            AssetKind::Metadata => unreachable!("metadata handled above"),
        }
    }
    // The router recognizes textures/CTM/sounds, while model-only 3D packs
    // also have legitimate resource content. Classify their parsed assets
    // after the complete router validation has succeeded.
    let has_models = !pack.blockstates.is_empty()
        || !pack.models.is_empty()
        || !pack.jem_models.is_empty()
        || !pack.jpm_models.is_empty();
    if has_models {
        pack.kind = match pack.kind {
            PackKind::Unknown => PackKind::ResourcePack,
            PackKind::ShaderPack | PackKind::Datapack => PackKind::HybridPack,
            kind => kind,
        };
    }
    if pack.pack_format.is_none() && pack.kind != PackKind::ShaderPack {
        return Err(invalid(
            "resource/datapack requires pack.mcmeta with pack_format",
        ));
    }
    if pack.kind == PackKind::Unknown {
        return Err(invalid(
            "pack has no recognized resource, shader, or data capabilities",
        ));
    }
    Ok(pack)
}

fn record<T>(
    target: &mut BTreeMap<String, T>,
    warnings: &mut Vec<String>,
    name: String,
    result: Result<T, optifine_compat::OptifineCompatError>,
) {
    match result {
        Ok(value) => {
            target.insert(name, value);
        }
        Err(error) => warnings.push(format!("{name}: {error}")),
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum AssetKind {
    Metadata,
    Blockstate,
    Model,
    Jem,
    Jpm,
    Ctm,
    Emissive,
}

fn asset_kind(name: &str) -> Option<AssetKind> {
    if name == "pack.mcmeta" {
        return Some(AssetKind::Metadata);
    }
    let parts: Vec<_> = name.split('/').collect();
    if parts.len() < 4 || parts[0] != "assets" || parts[1].is_empty() {
        return None;
    }
    if name.ends_with(".json") {
        match parts[2] {
            "blockstates" => return Some(AssetKind::Blockstate),
            "models" => return Some(AssetKind::Model),
            _ => {}
        }
    }
    if parts[2] == "optifine" {
        if parts.len() == 4 && parts[3] == "emissive.properties" {
            return Some(AssetKind::Emissive);
        }
        if parts.len() >= 5 {
            match parts[3] {
                "cem" if name.ends_with(".jem") => return Some(AssetKind::Jem),
                "cem" if name.ends_with(".jpm") => return Some(AssetKind::Jpm),
                "ctm" if name.ends_with(".properties") => return Some(AssetKind::Ctm),
                _ => {}
            }
        }
    }
    None
}

struct IndexedMember {
    name: String,
    method: u16,
    crc: u32,
    uncompressed_len: usize,
    compressed_start: usize,
    compressed_len: usize,
}

fn bytes_at(bytes: &[u8], start: usize, len: usize) -> Option<&[u8]> {
    bytes.get(start..start.checked_add(len)?)
}

fn le16(bytes: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes(bytes_at(bytes, at, 2)?.try_into().ok()?))
}

fn le32(bytes: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(bytes_at(bytes, at, 4)?.try_into().ok()?))
}

// The router has already validated the complete central directory and every
// member. This second pass obtains offsets for selected, bounded documents.
fn indexed_members(bytes: &[u8]) -> Result<Vec<IndexedMember>, PackLoadError> {
    let eocd = (bytes.len().saturating_sub(22 + 65_535)..=bytes.len() - 22)
        .rev()
        .find(|&at| {
            le32(bytes, at) == Some(0x0605_4b50)
                && le16(bytes, at + 20)
                    .is_some_and(|comment| at + 22 + comment as usize == bytes.len())
        })
        .ok_or_else(|| invalid("validated ZIP EOCD missing"))?;
    let mut cursor =
        le32(bytes, eocd + 16).ok_or_else(|| invalid("validated ZIP index missing"))? as usize;
    let count = le16(bytes, eocd + 10).ok_or_else(|| invalid("validated ZIP count missing"))?;
    let mut entries = Vec::with_capacity(count as usize);
    for _ in 0..count {
        if le32(bytes, cursor) != Some(0x0201_4b50) {
            return Err(invalid("validated ZIP index mismatch"));
        }
        let name_len =
            le16(bytes, cursor + 28).ok_or_else(|| invalid("ZIP entry name length"))? as usize;
        let extra_len =
            le16(bytes, cursor + 30).ok_or_else(|| invalid("ZIP extra length"))? as usize;
        let comment_len =
            le16(bytes, cursor + 32).ok_or_else(|| invalid("ZIP comment length"))? as usize;
        let name = std::str::from_utf8(
            bytes_at(bytes, cursor + 46, name_len).ok_or_else(|| invalid("ZIP entry name"))?,
        )
        .map_err(|_| invalid("ZIP entry name encoding"))?;
        let local = le32(bytes, cursor + 42).ok_or_else(|| invalid("ZIP local offset"))? as usize;
        let name_size = le16(bytes, local + 26).ok_or_else(|| invalid("ZIP local name"))? as usize;
        let extra_size =
            le16(bytes, local + 28).ok_or_else(|| invalid("ZIP local extra"))? as usize;
        let start = local
            .checked_add(30 + name_size + extra_size)
            .ok_or_else(|| invalid("ZIP member offset"))?;
        entries.push(IndexedMember {
            name: name.to_owned(),
            method: le16(bytes, cursor + 10).ok_or_else(|| invalid("ZIP method"))?,
            crc: le32(bytes, cursor + 16).ok_or_else(|| invalid("ZIP CRC"))?,
            compressed_len: le32(bytes, cursor + 20)
                .ok_or_else(|| invalid("ZIP compressed size"))?
                as usize,
            uncompressed_len: le32(bytes, cursor + 24)
                .ok_or_else(|| invalid("ZIP uncompressed size"))?
                as usize,
            compressed_start: start,
        });
        cursor = cursor
            .checked_add(46 + name_len + extra_len + comment_len)
            .ok_or_else(|| invalid("ZIP index offset overflow"))?;
    }
    Ok(entries)
}

fn extract_member(bytes: &[u8], entry: &IndexedMember) -> Result<Vec<u8>, PackLoadError> {
    let input = bytes_at(bytes, entry.compressed_start, entry.compressed_len)
        .ok_or_else(|| invalid(format!("{}: member out of range", entry.name)))?;
    let decoded = match entry.method {
        0 => input.to_vec(),
        8 => inflate_raw(input, entry.uncompressed_len)
            .map_err(|error| invalid(format!("{}: {error}", entry.name)))?,
        _ => return Err(invalid(format!("{}: unsupported compression", entry.name))),
    };
    if decoded.len() != entry.uncompressed_len || crc32(&decoded) != entry.crc {
        return Err(invalid(format!(
            "{}: uncompressed size or CRC mismatch",
            entry.name
        )));
    }
    Ok(decoded)
}

fn crc32(data: &[u8]) -> u32 {
    megu_core::hash::crc32_ieee(data)
}

// The byte-bounded DEFLATE implementation is shared algorithmically with the
// router. It is local because the router deliberately exposes no raw members.
struct Bits<'a> {
    input: &'a [u8],
    used: u8,
}

impl Bits<'_> {
    fn read(&mut self, width: u8) -> Result<u16, &'static str> {
        let mut value = 0u16;
        let mut read = 0;
        while read < width {
            let byte = *self.input.first().ok_or("truncated DEFLATE stream")?;
            let count = (width - read).min(8 - self.used);
            let mask = (1u16 << count) - 1;
            value |= (u16::from(byte >> self.used) & mask) << read;
            self.used += count;
            read += count;
            if self.used == 8 {
                self.used = 0;
                self.input = &self.input[1..];
            }
        }
        Ok(value)
    }

    fn align(&mut self) {
        if self.used != 0 {
            self.input = &self.input[1..];
            self.used = 0;
        }
    }
}

struct Huffman {
    counts: [u16; 16],
    symbols: Vec<u16>,
    max_width: usize,
}

impl Huffman {
    fn new(lengths: &[u8], single_allowed: bool) -> Result<Self, &'static str> {
        let mut counts = [0u16; 16];
        let mut max_width = 0;
        for &length in lengths {
            if length != 0 {
                let count = counts
                    .get_mut(usize::from(length))
                    .ok_or("overlong Huffman code")?;
                *count = count.checked_add(1).ok_or("Huffman count overflow")?;
                max_width = max_width.max(usize::from(length));
            }
        }
        let mut remaining = 1i32;
        let mut offsets = [0usize; 16];
        let mut total = 0usize;
        for width in 1..=max_width {
            remaining = remaining * 2 - i32::from(counts[width]);
            if remaining < 0 {
                return Err("oversubscribed Huffman tree");
            }
            offsets[width] = total;
            total += usize::from(counts[width]);
        }
        if remaining != 0 && !(single_allowed && total == 1 && max_width == 1) {
            return Err("incomplete Huffman tree");
        }
        let mut symbols = vec![0; total];
        for (symbol, &length) in lengths.iter().enumerate() {
            if length > 0 {
                let offset = &mut offsets[usize::from(length)];
                symbols[*offset] = symbol as u16;
                *offset += 1;
            }
        }
        Ok(Self {
            counts,
            symbols,
            max_width,
        })
    }

    fn read(&self, bits: &mut Bits<'_>) -> Result<u16, &'static str> {
        let (mut code, mut first, mut offset) = (0u32, 0u32, 0usize);
        for width in 1..=self.max_width {
            code = (code << 1) | u32::from(bits.read(1)?);
            let count = u32::from(self.counts[width]);
            if code >= first && code - first < count {
                return self
                    .symbols
                    .get(offset + (code - first) as usize)
                    .copied()
                    .ok_or("invalid Huffman lookup");
            }
            first = (first + count) << 1;
            offset += count as usize;
        }
        Err("undecodable Huffman symbol")
    }
}

fn fixed_trees() -> Result<(Huffman, Option<Huffman>), &'static str> {
    let mut literal_lengths = [8; 288];
    literal_lengths[144..256].fill(9);
    literal_lengths[256..280].fill(7);
    Ok((
        Huffman::new(&literal_lengths, false)?,
        Some(Huffman::new(&[5; 32], false)?),
    ))
}

fn dynamic_trees(bits: &mut Bits<'_>) -> Result<(Huffman, Option<Huffman>), &'static str> {
    let literals = usize::from(bits.read(5)?) + 257;
    let distances = usize::from(bits.read(5)?) + 1;
    let count = usize::from(bits.read(4)?) + 4;
    if literals > 286 {
        return Err("too many literal codes");
    }
    const ORDER: [usize; 19] = [
        16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15,
    ];
    let mut code_lengths = [0u8; 19];
    for &symbol in &ORDER[..count] {
        code_lengths[symbol] = bits.read(3)? as u8;
    }
    let code_tree = Huffman::new(&code_lengths, false)?;
    let mut lengths = Vec::with_capacity(literals + distances);
    while lengths.len() < literals + distances {
        let (length, repeated) = match code_tree.read(bits)? {
            n @ 0..=15 => (n as u8, 1),
            16 => (
                *lengths
                    .last()
                    .ok_or("repeat without preceding code length")?,
                usize::from(bits.read(2)?) + 3,
            ),
            17 => (0, usize::from(bits.read(3)?) + 3),
            18 => (0, usize::from(bits.read(7)?) + 11),
            _ => return Err("invalid code-length symbol"),
        };
        let end = lengths.len() + repeated;
        if end > literals + distances {
            return Err("code lengths exceed tree");
        }
        lengths.resize(end, length);
    }
    if lengths[256] == 0 {
        return Err("missing end-of-block symbol");
    }
    let (literal_lengths, distance_lengths) = lengths.split_at(literals);
    let distance_tree = if distance_lengths.iter().all(|&len| len == 0) {
        None
    } else {
        Some(Huffman::new(distance_lengths, true)?)
    };
    Ok((Huffman::new(literal_lengths, true)?, distance_tree))
}

const LENGTH_BASE: [u16; 29] = [
    3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115, 131,
    163, 195, 227, 258,
];
const DISTANCE_BASE: [u16; 30] = [
    1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193, 257, 385, 513, 769, 1025, 1537,
    2049, 3073, 4097, 6145, 8193, 12289, 16385, 24577,
];

fn inflate_raw(input: &[u8], expected: usize) -> Result<Vec<u8>, &'static str> {
    let mut bits = Bits { input, used: 0 };
    let mut out = Vec::new();
    out.try_reserve_exact(expected)
        .map_err(|_| "allocation exceeds available memory")?;
    loop {
        let is_final = bits.read(1)? != 0;
        match bits.read(2)? {
            0 => {
                bits.align();
                let len = bits.read(16)?;
                let inverse = bits.read(16)?;
                if len != !inverse {
                    return Err("stored DEFLATE block has bad length complement");
                }
                let bytes = bytes_at(bits.input, 0, usize::from(len))
                    .ok_or("truncated stored DEFLATE block")?;
                if bytes.len() > expected.saturating_sub(out.len()) {
                    return Err("inflated member exceeds declared length");
                }
                out.extend_from_slice(bytes);
                bits.input = &bits.input[usize::from(len)..];
            }
            kind @ (1 | 2) => {
                let (literals, distances) = if kind == 1 {
                    fixed_trees()?
                } else {
                    dynamic_trees(&mut bits)?
                };
                loop {
                    match literals.read(&mut bits)? {
                        literal @ 0..=255 => {
                            if out.len() == expected {
                                return Err("inflated member exceeds declared length");
                            }
                            out.push(literal as u8);
                        }
                        256 => break,
                        symbol @ 257..=285 => {
                            let index = (symbol - 257) as usize;
                            let extra = if symbol < 285 {
                                index.saturating_sub(4) / 4
                            } else {
                                0
                            };
                            let length = usize::from(LENGTH_BASE[index] + bits.read(extra as u8)?);
                            let distance_index = distances
                                .as_ref()
                                .ok_or("length with no distance tree")?
                                .read(&mut bits)?
                                as usize;
                            let base = *DISTANCE_BASE
                                .get(distance_index)
                                .ok_or("invalid distance code")?;
                            let extra = distance_index.saturating_sub(2) / 2;
                            let distance = usize::from(base + bits.read(extra as u8)?);
                            if distance == 0 || distance > out.len() {
                                return Err("distance reaches before output");
                            }
                            if length > expected.saturating_sub(out.len()) {
                                return Err("inflated member exceeds declared length");
                            }
                            for _ in 0..length {
                                out.push(out[out.len() - distance]);
                            }
                        }
                        _ => return Err("reserved literal symbol"),
                    }
                }
            }
            _ => return Err("reserved DEFLATE block type"),
        }
        if is_final {
            bits.align();
            if !bits.input.is_empty() || out.len() != expected {
                return Err("trailing DEFLATE bytes or uncompressed size mismatch");
            }
            return Ok(out);
        }
    }
}
