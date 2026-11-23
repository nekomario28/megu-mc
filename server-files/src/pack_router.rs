// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
//! Bounded, read-only ZIP inspection and capability routing for untrusted packs.
//!
//! Every member is verified against the central directory and its CRC before a
//! route is returned. ZIP64, split archives, encryption, non-file members and
//! unsupported compression methods are quarantined. No filesystem writes occur.

#![deny(unsafe_code)]

use std::collections::{BTreeMap, HashMap, HashSet};
use std::ops::Range;

pub const MAX_ENTRIES: usize = 65_536;
pub const MAX_TOTAL_UNCOMPRESSED: u64 = 512 * 1024 * 1024;
pub const MAX_COMPRESSION_RATIO: u64 = 100;
pub const MAX_PNG_DIMENSION: u32 = 4096;
pub const MAX_PNG_PIXELS: u64 = 4096 * 4096;
const MAX_SHADER_DEPTH: usize = 16;
const MAX_FUNCTION_FAN_OUT: usize = 128;
const MAX_GRAPH_SOURCE_BYTES: usize = 4 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PackKind {
    ResourcePack,
    ShaderPack,
    Datapack,
    HybridPack,
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PackThreatKind {
    PathTraversal,
    ZipBomb,
    EncryptedOrUnsupportedArchive,
    TextureOomBomb,
    ShaderIncludeCycle,
    DatapackFunctionCycle,
    EmbeddedExecutableOrScript,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PackThreat {
    pub kind: PackThreatKind,
    pub member: String,
    pub detail: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PackCapabilities {
    pub has_pack_mcmeta: bool,
    pub has_core_textures: bool,
    pub has_ctm_connected_textures: bool,
    pub has_emissive_overlays: bool,
    pub has_custom_fonts: bool,
    pub has_custom_sounds: bool,
    pub has_optifine_iris_shaders: bool,
    pub has_wgsl_shaders: bool,
    pub has_datapack_worldgen_or_recipes: bool,
    pub has_datapack_functions: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShaderFallbackTarget {
    UsePackWgsl,
    RouteOptiFineIrisBridge,
    FallbackBuiltinJapaneseWgsl,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PackRouteDecision {
    Quarantine {
        threats: Vec<PackThreat>,
    },
    RoutePack {
        kind: PackKind,
        shader_route: Option<ShaderFallbackTarget>,
        notes: Vec<String>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PackRoutingReport {
    pub kind: PackKind,
    pub capabilities: PackCapabilities,
    pub decision: PackRouteDecision,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PackRouterPolicy {
    pub max_texture_dim: u32,
    pub max_texture_pixels: u64,
    pub max_total_uncompressed_bytes: u64,
    pub max_compression_ratio: u64,
    pub gpu_supports_optifine_bridge: bool,
}

impl Default for PackRouterPolicy {
    fn default() -> Self {
        Self {
            max_texture_dim: MAX_PNG_DIMENSION,
            max_texture_pixels: MAX_PNG_PIXELS,
            max_total_uncompressed_bytes: MAX_TOTAL_UNCOMPRESSED,
            max_compression_ratio: MAX_COMPRESSION_RATIO,
            gpu_supports_optifine_bridge: true,
        }
    }
}

fn threat(kind: PackThreatKind, member: &str, detail: impl Into<String>) -> PackThreat {
    PackThreat {
        kind,
        member: member.to_owned(),
        detail: detail.into(),
    }
}

fn unsupported(member: &str, detail: impl Into<String>) -> PackThreat {
    threat(
        PackThreatKind::EncryptedOrUnsupportedArchive,
        member,
        detail,
    )
}

fn quarantine(
    kind: PackKind,
    capabilities: PackCapabilities,
    threats: Vec<PackThreat>,
) -> PackRoutingReport {
    PackRoutingReport {
        kind,
        capabilities,
        decision: PackRouteDecision::Quarantine { threats },
    }
}

/// Inspect an untrusted ZIP in memory, then choose the safest supported route.
///
/// The policy can tighten the hard ceilings but cannot raise them. All returned
/// capabilities are advisory until the entire archive has passed validation.
pub fn inspect_and_route_pack(zip_bytes: &[u8], policy: &PackRouterPolicy) -> PackRoutingReport {
    let mut capabilities = PackCapabilities::default();
    let entries = match parse_archive(zip_bytes, policy) {
        Ok(entries) => entries,
        Err(problem) => return quarantine(PackKind::Unknown, capabilities, vec![problem]),
    };
    let mut threats = Vec::new();
    let mut shader_sources = BTreeMap::new();
    let mut function_sources = BTreeMap::new();
    for entry in &entries {
        capabilities.observe(&entry.name);
        if entry.name.ends_with('/') {
            continue;
        }
        let lower = entry.name.to_ascii_lowercase();
        if is_executable(&lower) {
            threats.push(threat(
                PackThreatKind::EmbeddedExecutableOrScript,
                &entry.name,
                "executable or script member",
            ));
        }
        let data = match read_member(zip_bytes, entry) {
            Ok(data) => data,
            Err(problem) => {
                threats.push(problem);
                continue;
            }
        };
        if lower.ends_with(".png") {
            if let Err(problem) = inspect_png(&entry.name, &data, policy) {
                threats.push(problem);
            }
        }
        if entry.name.starts_with("shaders/") && is_glsl(&lower) {
            match std::str::from_utf8(&data) {
                Ok(source) if data.len() <= MAX_GRAPH_SOURCE_BYTES => {
                    shader_sources.insert(entry.name.clone(), source.to_owned());
                }
                _ => threats.push(unsupported(&entry.name, "invalid or oversized GLSL source")),
            }
        }
        if function_id(&entry.name).is_some() {
            match std::str::from_utf8(&data) {
                Ok(source) if data.len() <= MAX_GRAPH_SOURCE_BYTES => {
                    function_sources.insert(entry.name.clone(), source.to_owned());
                }
                _ => threats.push(unsupported(
                    &entry.name,
                    "invalid or oversized function source",
                )),
            }
        }
    }

    if let Err(problem) = check_shader_graph(&shader_sources) {
        threats.push(problem);
    }
    if let Err(problem) = check_function_graph(&function_sources) {
        threats.push(problem);
    }
    let kind = capabilities.kind();
    if !threats.is_empty() {
        return quarantine(kind, capabilities, threats);
    }
    let shader_route = if capabilities.has_wgsl_shaders {
        Some(ShaderFallbackTarget::UsePackWgsl)
    } else if capabilities.has_optifine_iris_shaders {
        Some(if policy.gpu_supports_optifine_bridge {
            ShaderFallbackTarget::RouteOptiFineIrisBridge
        } else {
            ShaderFallbackTarget::FallbackBuiltinJapaneseWgsl
        })
    } else {
        None
    };
    let mut notes = Vec::new();
    if shader_route == Some(ShaderFallbackTarget::FallbackBuiltinJapaneseWgsl) {
        notes.push(
            "OptiFine/Iris shaders require a bridge; using built-in Japanese WGSL fallback".into(),
        );
    }
    if kind == PackKind::Unknown {
        notes.push("No recognized resource, shader, or data capabilities".into());
    }
    PackRoutingReport {
        kind,
        capabilities,
        decision: PackRouteDecision::RoutePack {
            kind,
            shader_route,
            notes,
        },
    }
}

impl PackCapabilities {
    fn observe(&mut self, name: &str) {
        if name == "pack.mcmeta" {
            self.has_pack_mcmeta = true;
        }
        let parts: Vec<_> = name.split('/').collect();
        if parts.len() >= 4 && parts[0] == "assets" && !parts[1].is_empty() {
            match parts[2] {
                "textures" => self.has_core_textures = true,
                "font" => self.has_custom_fonts = true,
                "sounds" => self.has_custom_sounds = true,
                "optifine" | "mcpatcher" if parts.get(3) == Some(&"ctm") => {
                    self.has_ctm_connected_textures = true;
                }
                "ctm" => self.has_ctm_connected_textures = true,
                _ => {}
            }
        }
        if name.ends_with("_e.png") || name.ends_with("/emissive.properties") {
            self.has_emissive_overlays = true;
        }
        if name.starts_with("shaders/") {
            if name.ends_with(".wgsl") {
                self.has_wgsl_shaders = true;
            }
            if name.ends_with(".fsh")
                || name.ends_with(".vsh")
                || name == "shaders/shaders.properties"
            {
                self.has_optifine_iris_shaders = true;
            }
        }
        if parts.len() >= 4 && parts[0] == "data" && !parts[1].is_empty() {
            if matches!(
                parts[2],
                "recipes"
                    | "recipe"
                    | "worldgen"
                    | "dimension"
                    | "dimension_type"
                    | "loot_tables"
                    | "loot_table"
                    | "tags"
            ) {
                self.has_datapack_worldgen_or_recipes = true;
            }
            if matches!(parts[2], "function" | "functions") {
                self.has_datapack_functions = true;
            }
        }
    }

    fn kind(&self) -> PackKind {
        let resource = self.has_core_textures
            || self.has_ctm_connected_textures
            || self.has_emissive_overlays
            || self.has_custom_fonts
            || self.has_custom_sounds;
        let shader = self.has_optifine_iris_shaders || self.has_wgsl_shaders;
        let data = self.has_datapack_worldgen_or_recipes || self.has_datapack_functions;
        match (resource, shader, data) {
            (true, false, false) => PackKind::ResourcePack,
            (false, true, false) => PackKind::ShaderPack,
            (false, false, true) => PackKind::Datapack,
            (false, false, false) => PackKind::Unknown,
            _ => PackKind::HybridPack,
        }
    }
}

fn is_executable(name: &str) -> bool {
    [
        ".exe", ".dll", ".so", ".dylib", ".bat", ".ps1", ".sh", ".jar", ".class",
    ]
    .iter()
    .any(|suffix| name.ends_with(suffix))
}

fn is_glsl(name: &str) -> bool {
    [".glsl", ".fsh", ".vsh", ".gsh", ".csh", ".tsh"]
        .iter()
        .any(|suffix| name.ends_with(suffix))
}

fn inspect_png(name: &str, data: &[u8], policy: &PackRouterPolicy) -> Result<(), PackThreat> {
    if data.get(..8) != Some(b"\x89PNG\r\n\x1a\n")
        || be32(data, 8) != Some(13)
        || data.get(12..16) != Some(b"IHDR")
    {
        return Err(unsupported(name, "missing or malformed PNG IHDR"));
    }
    let width = be32(data, 16).ok_or_else(|| unsupported(name, "truncated PNG IHDR"))?;
    let height = be32(data, 20).ok_or_else(|| unsupported(name, "truncated PNG IHDR"))?;
    if width == 0
        || height == 0
        || width > policy.max_texture_dim.min(MAX_PNG_DIMENSION)
        || height > policy.max_texture_dim.min(MAX_PNG_DIMENSION)
        || u64::from(width) * u64::from(height) > policy.max_texture_pixels.min(MAX_PNG_PIXELS)
    {
        return Err(threat(
            PackThreatKind::TextureOomBomb,
            name,
            format!("PNG IHDR declares {width}x{height} pixels"),
        ));
    }
    let crc = be32(data, 29).ok_or_else(|| unsupported(name, "truncated PNG IHDR CRC"))?;
    if crc
        != crc32(
            data.get(12..29)
                .ok_or_else(|| unsupported(name, "truncated PNG IHDR"))?,
        )
    {
        return Err(unsupported(name, "PNG IHDR CRC mismatch"));
    }
    Ok(())
}

fn be32(bytes: &[u8], offset: usize) -> Option<u32> {
    Some(u32::from_be_bytes(
        bytes.get(offset..offset.checked_add(4)?)?.try_into().ok()?,
    ))
}

#[derive(Debug)]
struct Entry {
    name: String,
    method: u16,
    compressed: Range<usize>,
    uncompressed_len: usize,
    crc: u32,
}

fn bytes_at(bytes: &[u8], offset: usize, count: usize) -> Option<&[u8]> {
    bytes.get(offset..offset.checked_add(count)?)
}

fn le16(bytes: &[u8], offset: usize) -> Option<u16> {
    Some(u16::from_le_bytes(
        bytes_at(bytes, offset, 2)?.try_into().ok()?,
    ))
}

fn le32(bytes: &[u8], offset: usize) -> Option<u32> {
    Some(u32::from_le_bytes(
        bytes_at(bytes, offset, 4)?.try_into().ok()?,
    ))
}

fn extra_fields(extra: &[u8]) -> Result<(), &'static str> {
    let mut at = 0usize;
    while at < extra.len() {
        let tag = le16(extra, at).ok_or("truncated extra-field header")?;
        let size = usize::from(le16(extra, at + 2).ok_or("truncated extra-field length")?);
        at = at.checked_add(4 + size).ok_or("extra-field overflow")?;
        if at > extra.len() {
            return Err("truncated extra field");
        }
        if matches!(tag, 0x0001 | 0x7075 | 0x9901) {
            return Err("ZIP64, Unicode path override, or AES extension unsupported");
        }
    }
    Ok(())
}

fn validate_name(name: &str) -> Result<(), PackThreatKind> {
    let bytes = name.as_bytes();
    if name.starts_with('/')
        || name.contains('\\')
        || bytes.iter().any(|b| *b <= 31 || *b == 127)
        || (bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':')
    {
        return Err(PackThreatKind::PathTraversal);
    }
    let trimmed = name.strip_suffix('/').unwrap_or(name);
    if trimmed.is_empty()
        || trimmed
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
    {
        return Err(PackThreatKind::PathTraversal);
    }
    Ok(())
}

fn parse_archive(zip: &[u8], policy: &PackRouterPolicy) -> Result<Vec<Entry>, PackThreat> {
    let invalid = |why| unsupported("<archive>", why);
    if zip.len() < 22 {
        return Err(invalid("missing ZIP end-of-central-directory record"));
    }
    let earliest = zip.len().saturating_sub(22 + 65_535);
    let eocd = (earliest..=zip.len() - 22)
        .rev()
        .find(|&at| {
            le32(zip, at) == Some(0x0605_4b50)
                && le16(zip, at + 20)
                    .is_some_and(|comment| at + 22 + usize::from(comment) == zip.len())
        })
        .ok_or_else(|| invalid("missing valid ZIP end-of-central-directory record"))?;
    let disk = le16(zip, eocd)
        .and_then(|_| le16(zip, eocd + 4))
        .ok_or_else(|| invalid("truncated EOCD"))?;
    let cd_disk = le16(zip, eocd + 6).ok_or_else(|| invalid("truncated EOCD"))?;
    let disk_entries = le16(zip, eocd + 8).ok_or_else(|| invalid("truncated EOCD"))?;
    let count = usize::from(le16(zip, eocd + 10).ok_or_else(|| invalid("truncated EOCD"))?);
    let cd_size = le32(zip, eocd + 12).ok_or_else(|| invalid("truncated EOCD"))? as usize;
    let cd_start = le32(zip, eocd + 16).ok_or_else(|| invalid("truncated EOCD"))? as usize;
    if disk != 0
        || cd_disk != 0
        || usize::from(disk_entries) != count
        || count > MAX_ENTRIES
        || count == usize::from(u16::MAX)
        || cd_size == u32::MAX as usize
        || cd_start == u32::MAX as usize
        || cd_start.checked_add(cd_size) != Some(eocd)
    {
        return Err(if count > MAX_ENTRIES {
            threat(PackThreatKind::ZipBomb, "<archive>", "too many ZIP entries")
        } else {
            invalid("multi-disk/ZIP64 archive or inconsistent directory offsets")
        });
    }
    let mut entries = Vec::with_capacity(count);
    let mut ranges = Vec::with_capacity(count);
    let mut seen = HashSet::new();
    let mut total = 0u64;
    let mut cursor = cd_start;
    for _ in 0..count {
        if le32(zip, cursor) != Some(0x0201_4b50) || bytes_at(zip, cursor, 46).is_none() {
            return Err(invalid("invalid central-directory entry"));
        }
        let made_by =
            le16(zip, cursor + 4).ok_or_else(|| invalid("truncated central directory"))?;
        let flags = le16(zip, cursor + 8).ok_or_else(|| invalid("truncated central directory"))?;
        let method =
            le16(zip, cursor + 10).ok_or_else(|| invalid("truncated central directory"))?;
        let crc = le32(zip, cursor + 16).ok_or_else(|| invalid("truncated central directory"))?;
        let compressed_size =
            le32(zip, cursor + 20).ok_or_else(|| invalid("truncated central directory"))?;
        let size = le32(zip, cursor + 24).ok_or_else(|| invalid("truncated central directory"))?;
        let name_len = usize::from(
            le16(zip, cursor + 28).ok_or_else(|| invalid("truncated central directory"))?,
        );
        let extra_len = usize::from(
            le16(zip, cursor + 30).ok_or_else(|| invalid("truncated central directory"))?,
        );
        let comment_len = usize::from(
            le16(zip, cursor + 32).ok_or_else(|| invalid("truncated central directory"))?,
        );
        let disk_start =
            le16(zip, cursor + 34).ok_or_else(|| invalid("truncated central directory"))?;
        let external =
            le32(zip, cursor + 38).ok_or_else(|| invalid("truncated central directory"))?;
        let local =
            le32(zip, cursor + 42).ok_or_else(|| invalid("truncated central directory"))? as usize;
        let name_bytes =
            bytes_at(zip, cursor + 46, name_len).ok_or_else(|| invalid("truncated entry name"))?;
        let name = std::str::from_utf8(name_bytes)
            .map_err(|_| invalid("non-UTF-8 entry name"))?
            .to_owned();
        let extra = bytes_at(zip, cursor + 46 + name_len, extra_len)
            .ok_or_else(|| unsupported(&name, "truncated extra fields"))?;
        extra_fields(extra).map_err(|why| unsupported(&name, why))?;
        cursor = cursor
            .checked_add(46 + name_len + extra_len + comment_len)
            .ok_or_else(|| unsupported(&name, "central-directory offset overflow"))?;
        if cursor > eocd {
            return Err(unsupported(
                &name,
                "central-directory entry exceeds directory",
            ));
        }
        if let Err(kind) = validate_name(&name) {
            return Err(threat(kind, &name, "unsafe ZIP entry path"));
        }
        if !seen.insert(name.to_ascii_lowercase()) {
            return Err(unsupported(&name, "duplicate or case-colliding ZIP entry"));
        }
        let mode = (external >> 16) & 0xf000;
        if disk_start != 0
            || flags & !(0x0800 | 0x0008 | 0x0006) != 0
            || flags & 0x0006 != 0
            || !matches!(method, 0 | 8)
            || ((made_by >> 8) == 3
                && mode != 0
                && mode != if name.ends_with('/') { 0x4000 } else { 0x8000 })
        {
            return Err(unsupported(
                &name,
                "encrypted, unsupported compression, flags, or special file",
            ));
        }
        let compressed_u64 = u64::from(compressed_size);
        let size_u64 = u64::from(size);
        let cap = policy
            .max_total_uncompressed_bytes
            .min(MAX_TOTAL_UNCOMPRESSED);
        total = total.checked_add(size_u64).ok_or_else(|| {
            threat(
                PackThreatKind::ZipBomb,
                &name,
                "uncompressed size sum overflow",
            )
        })?;
        if size_u64 > cap
            || total > cap
            || size_u64
                > compressed_u64
                    .saturating_mul(policy.max_compression_ratio.min(MAX_COMPRESSION_RATIO))
            || (compressed_size == 0 && size != 0)
        {
            return Err(threat(
                PackThreatKind::ZipBomb,
                &name,
                "uncompressed size or compression ratio exceeds policy",
            ));
        }
        if (name.ends_with('/') && (compressed_size != 0 || size != 0))
            || (method == 0 && compressed_size != size)
        {
            return Err(unsupported(&name, "invalid directory or stored size"));
        }
        if le32(zip, local) != Some(0x0403_4b50) {
            return Err(unsupported(&name, "missing matching local-file header"));
        }
        let local_flags =
            le16(zip, local + 6).ok_or_else(|| unsupported(&name, "truncated local header"))?;
        let local_method =
            le16(zip, local + 8).ok_or_else(|| unsupported(&name, "truncated local header"))?;
        let local_name_len = usize::from(
            le16(zip, local + 26).ok_or_else(|| unsupported(&name, "truncated local header"))?,
        );
        let local_extra_len = usize::from(
            le16(zip, local + 28).ok_or_else(|| unsupported(&name, "truncated local header"))?,
        );
        if local_flags != flags
            || local_method != method
            || bytes_at(zip, local + 30, local_name_len) != Some(name_bytes)
        {
            return Err(unsupported(
                &name,
                "local and central entry metadata disagree",
            ));
        }
        let local_extra = bytes_at(zip, local + 30 + local_name_len, local_extra_len)
            .ok_or_else(|| unsupported(&name, "truncated local extra fields"))?;
        extra_fields(local_extra).map_err(|why| unsupported(&name, why))?;
        let local_crc =
            le32(zip, local + 14).ok_or_else(|| unsupported(&name, "truncated local header"))?;
        let local_compressed =
            le32(zip, local + 18).ok_or_else(|| unsupported(&name, "truncated local header"))?;
        let local_size =
            le32(zip, local + 22).ok_or_else(|| unsupported(&name, "truncated local header"))?;
        if (flags & 8 == 0
            && (local_crc != crc || local_compressed != compressed_size || local_size != size))
            || (flags & 8 != 0
                && (local_crc != 0 && local_crc != crc
                    || local_compressed != 0 && local_compressed != compressed_size
                    || local_size != 0 && local_size != size))
        {
            return Err(unsupported(
                &name,
                "local and central sizes or CRC disagree",
            ));
        }
        let start = local
            .checked_add(30 + local_name_len + local_extra_len)
            .ok_or_else(|| unsupported(&name, "local-header offset overflow"))?;
        let end = start
            .checked_add(compressed_size as usize)
            .ok_or_else(|| unsupported(&name, "member offset overflow"))?;
        if end > cd_start {
            return Err(unsupported(&name, "member overlaps central directory"));
        }
        let mut record_end = end;
        if flags & 8 != 0 {
            let has_signature = le32(zip, end) == Some(0x0807_4b50);
            let descriptor = end + if has_signature { 4 } else { 0 };
            if le32(zip, descriptor) != Some(crc)
                || le32(zip, descriptor + 4) != Some(compressed_size)
                || le32(zip, descriptor + 8) != Some(size)
            {
                return Err(unsupported(&name, "invalid data descriptor"));
            }
            record_end = descriptor + 12;
        }
        if record_end > cd_start {
            return Err(unsupported(
                &name,
                "member record overlaps central directory",
            ));
        }
        ranges.push((local, record_end));
        entries.push(Entry {
            name,
            method,
            compressed: start..end,
            uncompressed_len: size as usize,
            crc,
        });
    }
    if cursor != eocd {
        return Err(invalid("unaccounted central-directory bytes"));
    }
    ranges.sort_unstable();
    let mut end = 0usize;
    for (start, next_end) in ranges {
        if start != end {
            return Err(invalid("overlapping ZIP records or unreferenced payload"));
        }
        end = next_end;
    }
    if end != cd_start {
        return Err(invalid("unreferenced bytes before central directory"));
    }
    Ok(entries)
}

fn read_member(zip: &[u8], entry: &Entry) -> Result<Vec<u8>, PackThreat> {
    let input = zip
        .get(entry.compressed.clone())
        .ok_or_else(|| unsupported(&entry.name, "member out of range"))?;
    let data = if entry.method == 0 {
        input.to_vec()
    } else {
        inflate_raw(input, entry.uncompressed_len)
            .map_err(|why| unsupported(&entry.name, format!("invalid DEFLATE member: {why}")))?
    };
    if data.len() != entry.uncompressed_len || crc32(&data) != entry.crc {
        return Err(unsupported(
            &entry.name,
            "uncompressed size or CRC32 mismatch",
        ));
    }
    Ok(data)
}

fn crc32(data: &[u8]) -> u32 {
    megu_core::hash::crc32_ieee(data)
}

fn normalize_include(source: &str, include: &str) -> Option<String> {
    if include.is_empty() || include.contains('\\') || include.as_bytes().contains(&0) {
        return None;
    }
    let mut parts: Vec<&str> = if include.starts_with('/') {
        vec!["shaders"]
    } else {
        source.rsplit_once('/')?.0.split('/').collect()
    };
    for segment in include.trim_start_matches('/').split('/') {
        match segment {
            "" | "." => return None,
            ".." => {
                if parts.len() <= 1 {
                    return None;
                }
                parts.pop();
            }
            _ => parts.push(segment),
        }
    }
    (parts.first() == Some(&"shaders")).then(|| parts.join("/"))
}

fn check_shader_graph(sources: &BTreeMap<String, String>) -> Result<(), PackThreat> {
    let mut graph = BTreeMap::new();
    for (name, source) in sources {
        let mut edges = Vec::new();
        for line in source.lines() {
            let line = line.trim_start_matches('\u{feff}').trim_start();
            let Some(raw) = line.strip_prefix("#include") else {
                continue;
            };
            let path = raw
                .trim_start()
                .strip_prefix('"')
                .and_then(|value| value.split_once('"'))
                .filter(|(_, suffix)| {
                    suffix.trim().is_empty() || suffix.trim_start().starts_with("//")
                })
                .map(|(path, _)| path)
                .ok_or_else(|| unsupported(name, "malformed shader include"))?;
            let target = normalize_include(name, path)
                .ok_or_else(|| unsupported(name, "shader include escapes shaders/"))?;
            if !sources.contains_key(&target) {
                return Err(unsupported(
                    name,
                    format!("missing shader include {target}"),
                ));
            }
            edges.push(target);
        }
        graph.insert(name.clone(), edges);
    }
    detect_graph_issue(
        &graph,
        Some(MAX_SHADER_DEPTH),
        PackThreatKind::ShaderIncludeCycle,
    )
}

fn function_id(name: &str) -> Option<String> {
    let remainder = name.strip_prefix("data/")?;
    let (namespace, path) = remainder.split_once('/')?;
    let path = path
        .strip_prefix("function/")
        .or_else(|| path.strip_prefix("functions/"))?
        .strip_suffix(".mcfunction")?;
    if namespace.is_empty() || path.is_empty() {
        return None;
    }
    Some(format!("{namespace}:{path}"))
}

fn check_function_graph(sources: &BTreeMap<String, String>) -> Result<(), PackThreat> {
    let mut id_to_member = HashMap::new();
    for name in sources.keys() {
        if let Some(id) = function_id(name) {
            if id_to_member.insert(id, name.clone()).is_some() {
                return Err(unsupported(name, "ambiguous datapack function ID"));
            }
        }
    }
    let mut graph = BTreeMap::new();
    for (name, source) in sources {
        let mut edges = Vec::new();
        let mut calls = 0usize;
        for line in source.lines() {
            let command = line.trim_start_matches('\u{feff}').trim_start();
            if command.starts_with('#') || command.is_empty() {
                continue;
            }
            let target = command
                .strip_prefix("function ")
                .or_else(|| command.split_once(" run function ").map(|(_, rest)| rest));
            if let Some(target) = target.and_then(|text| text.split_ascii_whitespace().next()) {
                // Tags can refer to externally provided function sets. They
                // are not statically resolvable to local call-graph edges.
                if target.starts_with('#') {
                    continue;
                }
                // Bound every static call, including external functions that
                // are absent from this archive.
                calls += 1;
                if calls > MAX_FUNCTION_FAN_OUT {
                    return Err(threat(
                        PackThreatKind::DatapackFunctionCycle,
                        name,
                        "datapack function exceeds 128 static call edges",
                    ));
                }
                if let Some(member) = id_to_member.get(target) {
                    edges.push(member.clone());
                }
            }
        }
        graph.insert(name.clone(), edges);
    }
    detect_graph_issue(&graph, None, PackThreatKind::DatapackFunctionCycle)
}

/// Iterative DFS keeps hostile, long acyclic function chains off the call stack.
fn detect_graph_issue(
    graph: &BTreeMap<String, Vec<String>>,
    max_depth: Option<usize>,
    kind: PackThreatKind,
) -> Result<(), PackThreat> {
    let names: Vec<_> = graph.keys().collect();
    let positions: HashMap<_, _> = names
        .iter()
        .enumerate()
        .map(|(at, name)| (*name, at))
        .collect();
    let edges: Vec<Vec<usize>> = graph
        .values()
        .map(|calls| {
            calls
                .iter()
                .filter_map(|call| positions.get(call).copied())
                .collect()
        })
        .collect();
    let mut state = vec![0u8; names.len()];
    for root in 0..names.len() {
        if state[root] != 0 {
            continue;
        }
        let mut stack = vec![(root, 0usize)];
        state[root] = 1;
        while let Some((current, next)) = stack.last().copied() {
            if next == edges[current].len() {
                state[current] = 2;
                stack.pop();
                continue;
            }
            if let Some(frame) = stack.last_mut() {
                frame.1 += 1;
            }
            let child = edges[current][next];
            if state[child] == 1 {
                return Err(threat(
                    kind,
                    names[current],
                    format!("call cycle reaches {}", names[child]),
                ));
            }
            if state[child] == 0 {
                if max_depth.is_some_and(|depth| stack.len() >= depth) {
                    return Err(threat(
                        kind,
                        names[current],
                        "shader include depth exceeds 16",
                    ));
                }
                state[child] = 1;
                stack.push((child, 0));
            }
        }
    }
    Ok(())
}

fn inflate_raw(input: &[u8], expected: usize) -> Result<Vec<u8>, &'static str> {
    let (out, consumed) = megu_core::zip_archive::inflate_raw_rfc1951_with_consumed(input, expected)
        .map_err(|error| match error {
            megu_core::zip_archive::ZipError::OutputCapExceeded => {
                "inflated member exceeds declared length"
            }
            _ => "invalid DEFLATE stream",
        })?;
    if consumed != input.len() || out.len() != expected {
        return Err("trailing DEFLATE bytes or uncompressed size mismatch");
    }
    Ok(out)
}
