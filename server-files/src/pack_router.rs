// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
//! Bounded, read-only ZIP inspection and capability routing for untrusted packs.
//!
//! EOCD and Central Directory parsing is delegated to the canonical
//! [`megu_core::zip_archive::ZipSliceIndex`]; every member is then verified
//! against its declared CRC before a route is returned. Encryption,
//! non-file members, unsupported compression methods, unsafe extra fields,
//! and local/central metadata disagreement are quarantined. No filesystem
//! writes occur.

#![deny(unsafe_code)]

use std::collections::{BTreeMap, HashMap, HashSet};

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
    let index = match megu_core::zip_archive::ZipSliceIndex::open(zip_bytes) {
        Ok(index) => index,
        Err(problem) => {
            return quarantine(PackKind::Unknown, capabilities, vec![zip_index_problem(&problem)])
        }
    };
    let entries = match parse_archive(zip_bytes, &index, policy) {
        Ok(entries) => entries,
        Err(problem) => return quarantine(PackKind::Unknown, capabilities, vec![problem]),
    };
    let mut threats = Vec::new();
    let mut shader_sources = BTreeMap::new();
    let mut function_sources = BTreeMap::new();
    for entry in &entries {
        capabilities.observe(&entry.name);
        let lower = entry.name.to_ascii_lowercase();
        if is_executable(&lower) {
            threats.push(threat(
                PackThreatKind::EmbeddedExecutableOrScript,
                &entry.name,
                "executable or script member",
            ));
        }
        let data = match read_member(&index, entry) {
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
    crc: u32,
}

/// Map a canonical index-open failure onto the quarantine surface. Unsafe
/// names and encrypted members keep their dedicated threat kinds; every
/// other structural failure is an unsupported-archive threat.
fn zip_index_problem(error: &megu_core::zip_archive::ZipError) -> PackThreat {
    use megu_core::zip_archive::ZipError as Z;
    match error {
        Z::Encrypted(name) => unsupported(name, "encrypted entry"),
        Z::InvalidEntryName(name) => {
            threat(PackThreatKind::PathTraversal, name, "unsafe ZIP entry path")
        }
        // A saturated 32-bit Central Directory field declares a member of at
        // least 4 GiB; without a ZIP64 extra field the canonical index
        // rejects it, and the pack policy classifies that as a size bomb.
        Z::Malformed("missing ZIP64 extra field") => threat(
            PackThreatKind::ZipBomb,
            "<archive>",
            "member declares a saturated ZIP64 size without ZIP64 metadata",
        ),
        other => unsupported("<archive>", other.to_string()),
    }
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

/// Validate every indexed member against pack policy. EOCD/Central
/// Directory structure comes from the canonical
/// [`megu_core::zip_archive::ZipSliceIndex`]; the fixed Central Directory
/// header is re-read per member only for the policy fields the canonical
/// index does not model (flags, made-by, external attributes, disk start,
/// extra fields), and the Local File Header is cross-checked for
/// local/central disagreement.
fn parse_archive(
    zip: &[u8],
    index: &megu_core::zip_archive::ZipSliceIndex<'_>,
    policy: &PackRouterPolicy,
) -> Result<Vec<Entry>, PackThreat> {
    let invalid = |why| unsupported("<archive>", why);
    let cap = policy
        .max_total_uncompressed_bytes
        .min(MAX_TOTAL_UNCOMPRESSED);
    let mut entries = Vec::new();
    let mut seen = HashSet::new();
    let mut total = 0u64;
    for meta in index.entries() {
        // Only top-level members are pack content; Jar-in-Jar children are
        // not part of the pack layout.
        if meta.archive_id != 0 {
            continue;
        }
        let name = meta.name.clone();
        if let Err(kind) = validate_name(&name) {
            return Err(threat(kind, &name, "unsafe ZIP entry path"));
        }
        if !seen.insert(name.to_ascii_lowercase()) {
            return Err(unsupported(&name, "duplicate or case-colliding ZIP entry"));
        }

        // Policy fields live in the raw Central Directory record, addressed
        // by the canonical index's recorded header offset.
        let cd_at = usize::try_from(meta.central_header_offset)
            .map_err(|_| invalid("central header offset overflow"))?;
        let fixed = bytes_at(zip, cd_at, 46)
            .ok_or_else(|| unsupported(&name, "truncated central directory"))?;
        if le32(fixed, 0) != Some(0x0201_4b50) {
            return Err(unsupported(&name, "central directory record mismatch"));
        }
        let made_by = le16(fixed, 4).ok_or_else(|| invalid("truncated central directory"))?;
        let flags = le16(fixed, 8).ok_or_else(|| invalid("truncated central directory"))?;
        let disk_start =
            le16(fixed, 34).ok_or_else(|| invalid("truncated central directory"))?;
        let external =
            le32(fixed, 38).ok_or_else(|| invalid("truncated central directory"))?;
        let name_len = usize::from(
            le16(fixed, 28).ok_or_else(|| invalid("truncated central directory"))?,
        );
        let extra_len = usize::from(
            le16(fixed, 30).ok_or_else(|| invalid("truncated central directory"))?,
        );
        let extra = bytes_at(zip, cd_at + 46 + name_len, extra_len)
            .ok_or_else(|| unsupported(&name, "truncated extra fields"))?;
        extra_fields(extra).map_err(|why| unsupported(&name, why))?;

        let method = meta.compression_method;
        let compressed_u64 = meta.compressed_size;
        let size_u64 = meta.uncompressed_size;
        let mode = (external >> 16) & 0xf000;
        if disk_start != 0
            || flags & !(0x0800 | 0x0008 | 0x0006) != 0
            || flags & 0x0006 != 0
            || !matches!(method, 0 | 8)
            || ((made_by >> 8) == 3 && mode != 0 && mode != 0x8000)
        {
            return Err(unsupported(
                &name,
                "encrypted, unsupported compression, flags, or special file",
            ));
        }
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
            || (compressed_u64 == 0 && size_u64 != 0)
        {
            return Err(threat(
                PackThreatKind::ZipBomb,
                &name,
                "uncompressed size or compression ratio exceeds policy",
            ));
        }
        if method == 0 && compressed_u64 != size_u64 {
            return Err(unsupported(&name, "invalid stored size"));
        }

        // Local File Header cross-check: flags, method, name bytes, extra
        // fields and the CRC/size fields must agree with the Central
        // Directory (zeroed fields are tolerated only under the data-
        // descriptor flag).
        let local = usize::try_from(meta.local_header_offset)
            .map_err(|_| unsupported(&name, "local header offset overflow"))?;
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
            || bytes_at(zip, local + 30, local_name_len) != Some(name.as_bytes())
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
            && (local_crc != meta.crc32
                || u64::from(local_compressed) != compressed_u64
                || u64::from(local_size) != size_u64))
            || (flags & 8 != 0
                && (local_crc != 0 && local_crc != meta.crc32
                    || local_compressed != 0 && u64::from(local_compressed) != compressed_u64
                    || local_size != 0 && u64::from(local_size) != size_u64))
        {
            return Err(unsupported(
                &name,
                "local and central sizes or CRC disagree",
            ));
        }
        // The member's compressed payload must end exactly at the declared
        // boundary, and a data descriptor (general-purpose flag bit 3) must
        // repeat the Central Directory CRC and sizes.
        let compressed_usize = usize::try_from(compressed_u64)
            .map_err(|_| unsupported(&name, "member size overflow"))?;
        let data_start = local
            .checked_add(30 + local_name_len + local_extra_len)
            .ok_or_else(|| unsupported(&name, "local-header offset overflow"))?;
        let data_end = data_start
            .checked_add(compressed_usize)
            .ok_or_else(|| unsupported(&name, "member offset overflow"))?;
        if data_end > zip.len() {
            return Err(unsupported(&name, "member out of range"));
        }
        if flags & 8 != 0 {
            let has_signature = le32(zip, data_end) == Some(0x0807_4b50);
            let descriptor = data_end + usize::from(has_signature) * 4;
            if le32(zip, descriptor) != Some(meta.crc32)
                || le32(zip, descriptor + 4) != u32::try_from(compressed_u64).ok()
                || le32(zip, descriptor + 8) != u32::try_from(size_u64).ok()
            {
                return Err(unsupported(&name, "invalid data descriptor"));
            }
        }
        entries.push(Entry {
            name,
            crc: meta.crc32,
        });
    }
    Ok(entries)
}

fn read_member(
    index: &megu_core::zip_archive::ZipSliceIndex<'_>,
    entry: &Entry,
) -> Result<Vec<u8>, PackThreat> {
    let data = index
        .read_entry(&entry.name, MAX_TOTAL_UNCOMPRESSED as usize)
        .map_err(|error| unsupported(&entry.name, format!("invalid member: {error}")))?;
    if crc32(&data) != entry.crc {
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

