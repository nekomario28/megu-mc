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

    // The router has already validated the complete central directory and
    // every member. The canonical slice index re-walks it once more to
    // obtain member metadata for the selected, bounded documents.
    let index = megu_core::zip_archive::ZipSliceIndex::open(bytes)
        .map_err(|error| invalid(format!("validated ZIP index: {error}")))?;
    for entry in index.entries() {
        // Only top-level members are pack assets.
        if entry.archive_id != 0 {
            continue;
        }
        let name = entry.name.clone();
        let Some(kind) = asset_kind(&name) else {
            continue;
        };
        if entry.uncompressed_size > MAX_DOCUMENT_BYTES as u64 {
            if kind == AssetKind::Metadata {
                return Err(invalid("pack.mcmeta exceeds metadata size limit"));
            }
            pack.warnings
                .push(format!("{}: document exceeds 1 MiB", name));
            continue;
        }
        let content = extract_member(&index, entry)?;
        let text = match std::str::from_utf8(&content) {
            Ok(text) => text,
            Err(error) if kind != AssetKind::Metadata => {
                pack.warnings
                    .push(format!("{}: invalid UTF-8: {error}", name));
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
                        pack.blockstates.insert(name, json);
                    } else {
                        pack.models.insert(name, json);
                    }
                }
                Err(error) => pack.warnings.push(format!("{}: {error}", name)),
            },
            AssetKind::Jem => record(
                &mut pack.jem_models,
                &mut pack.warnings,
                name,
                optifine_compat::parse_jem(text),
            ),
            AssetKind::Jpm => record(
                &mut pack.jpm_models,
                &mut pack.warnings,
                name,
                optifine_compat::parse_jpm(text),
            ),
            AssetKind::Ctm => record(
                &mut pack.ctm_rules,
                &mut pack.warnings,
                name,
                optifine_compat::parse_ctm_properties(text),
            ),
            AssetKind::Emissive => record(
                &mut pack.emissive,
                &mut pack.warnings,
                name,
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

/// Decode one already-validated member through the canonical slice index.
/// The router verified the whole archive beforehand; the CRC re-check here
/// is cheap defense in depth for the documents actually consumed. The
/// DEFLATE stream must consume its declared compressed bytes exactly.
fn extract_member(
    index: &megu_core::zip_archive::ZipSliceIndex<'_>,
    entry: &megu_core::zip_archive::ZipEntryMeta,
) -> Result<Vec<u8>, PackLoadError> {
    let input = index
        .compressed_member(&entry.name)
        .map_err(|error| invalid(format!("{}: {error}", entry.name)))?;
    let decoded = match entry.compression_method {
        0 => input.to_vec(),
        8 => {
            let (out, consumed) = megu_core::zip_archive::inflate_raw_rfc1951_with_consumed(
                input,
                entry.uncompressed_size as usize,
            )
            .map_err(|error| invalid(format!("{}: {error}", entry.name)))?;
            if consumed != input.len() {
                return Err(invalid(format!(
                    "{}: trailing DEFLATE bytes or uncompressed size mismatch",
                    entry.name
                )));
            }
            out
        }
        method => {
            return Err(invalid(format!(
                "{}: unsupported compression method {method}",
                entry.name
            )))
        }
    };
    if decoded.len() as u64 != entry.uncompressed_size || crc32(&decoded) != entry.crc32 {
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
