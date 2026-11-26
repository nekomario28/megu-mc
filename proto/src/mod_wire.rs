// SPDX-License-Identifier: LGPL-3.0-or-later
//! Canonical mod synchronization wire codecs shared by server and client.
//!
//! `MRS1` is the dynamic mod-registry frame on channel
//! `megu:mod_registry_sync_v1`:
//!
//! ```text
//! MRS1 | u16 version | u32 base_block_state_id | u32 base_item_id
//!      | u32 block_state_count | block states... | u32 item_count | items...
//!      | u32 crc32_ieee(all preceding bytes)
//! block state = u32 state_id | u16len block_id | u16 property_count
//!             | (u16len key | u16len value)* | [u8 facts_byte on v2]
//! item        = u32 item_id | u16len item_id
//! ```
//!
//! Version 2 differs only by the trailing per-state `facts_byte`
//! `(luminance << 4) | opacity`; version 1 decoders derive the same value
//! through the generic rules below. State ids, item ids, and property keys are
//! strictly monotonic. All integers are big-endian and all strings are non-empty
//! UTF-8 with `u16` byte lengths.
//!
//! The canonical per-state property cap is 64, the cap enforced by the registry
//! planner and server decoder. Some older client-side code used 4,096 as a
//! local pre-allocation ceiling; that value is documented as
//! [`MAX_MRS1_LEGACY_CLIENT_PROPERTY_BUDGET`] but is not the interoperable wire
//! contract. Canonical encoders and decoders therefore reject 65 or more
//! properties, eliminating the server/client drift rather than widening the
//! server contract after the fact.
//!
//! `MSY1` is the mod-sync manifest on channel `megu:mod_sync_v1`:
//!
//! ```text
//! MSY1 | u16 count | entries...
//! entry = u8 mod_id_len | mod_id | u8 file_name_len | file_name
//!       | u16be download_url_len | download_url | u64be size_bytes
//!       | 32-byte sha256 digest
//! ```
//!
//! MSY1 structural validation (safe mod id, jailed `.jar` basename, HTTPS URL
//! syntax, digest and size bounds) is part of the wire contract. CDN/mirror
//! trust is client policy layered above it and is exposed separately through
//! [`validate_msy1_canonical_download_url`].

use std::collections::BTreeMap;

/// Protocol 767 custom-payload channel carrying `MRS1` frames.
pub const MOD_REGISTRY_SYNC_CHANNEL: &str = "megu:mod_registry_sync_v1";
/// Protocol 767 custom-payload channel carrying `MSY1` manifests.
pub const MOD_SYNC_CHANNEL: &str = "megu:mod_sync_v1";

pub const MRS1_MAGIC: [u8; 4] = *b"MRS1";
pub const MRS1_VERSION_V1: u16 = 1;
pub const MRS1_VERSION_V2: u16 = 2;
pub const MAX_MRS1_BLOCK_STATES: usize = 65_536;
pub const MAX_MRS1_ITEMS: usize = 65_536;
/// Canonical producer/consumer property cap for one block state.
pub const MAX_MRS1_PROPERTIES_PER_STATE: usize = 64;
/// Historical client-only allocation ceiling. Frames in the 65..=4096 range
/// are not canonical because the server-side contract rejects them.
pub const MAX_MRS1_LEGACY_CLIENT_PROPERTY_BUDGET: usize = 4_096;
pub const MAX_MRS1_STRING_BYTES: usize = u16::MAX as usize;
pub const MAX_MRS1_FRAME_BYTES: usize = 8 * 1024 * 1024;
/// Facts byte of an opaque, non-luminous full cube (`0x0f`).
pub const MRS1_DEFAULT_FACTS_BYTE: u8 = 0x0f;

pub const MSY1_MAGIC: [u8; 4] = *b"MSY1";
pub const MAX_MSY1_MODS: usize = 512;
pub const MAX_MSY1_MOD_ID_LEN: usize = 128;
pub const MAX_MSY1_FILE_NAME_LEN: usize = 255;
pub const MAX_MSY1_URL_LEN: usize = 2048;
pub const MAX_MSY1_JAR_BYTES: u64 = 256 * 1024 * 1024;
const MIN_MSY1_ENTRY_WIRE_BYTES: usize = 1 + 1 + 2 + 8 + 32;
/// Canonical trusted mod-host CDNs. This is client download policy, not a
/// structural property required to parse the frame.
pub const TRUSTED_MSY1_CDN_HOSTS: &[&str] = &[
    "cdn.modrinth.com",
    "edge.forgecdn.net",
    "mediafilez.forgecdn.net",
];

fn err(message: impl Into<String>) -> String {
    message.into()
}

// ---------------------------------------------------------------------------
// MRS1 physical facts derivation
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mrs1Shape {
    Empty,
    Thin,
    Partial,
    Leaves,
    Liquid,
    Full,
}

fn mrs1_block_shape(path: &str) -> Mrs1Shape {
    // `_block` names are cubes first: `grass_block` must not read as `grass`.
    if path == "block" || path.ends_with("_block") {
        return Mrs1Shape::Full;
    }
    // Suffixes, not substrings: `fluid_pipe` and `water_wheel` are mounts,
    // not fluids.
    if ["water", "lava", "fluid", "liquid"]
        .iter()
        .any(|k| path.ends_with(k))
    {
        return Mrs1Shape::Liquid;
    }
    if path.contains("leaves") {
        return Mrs1Shape::Leaves;
    }
    const EMPTY: &[&str] = &[
        "torch", "rail", "flower", "sapling", "crop", "vine", "wire", "button", "lever", "sign",
        "banner", "candle", "mushroom", "fern", "coral", "reed", "lichen", "grass", "plant",
        "seed",
    ];
    if EMPTY.iter().any(|k| path.contains(k)) {
        return Mrs1Shape::Empty;
    }
    const THIN: &[&str] = &[
        "glass",
        "pane",
        "grate",
        "bars",
        "chain",
        "lantern",
        "pipe",
        "shaft",
        "belt",
        "cogwheel",
        "wheel",
        "funnel",
        "chute",
        "spout",
        "depot",
        "basin",
        "pump",
        "bearing",
        "ladder",
        "scaffold",
        "frame",
        "pressure_plate",
        "door",
        "trapdoor",
        "hopper",
        "cauldron",
        "anvil",
        "conduit",
    ];
    if THIN.iter().any(|k| path.contains(k)) {
        return Mrs1Shape::Thin;
    }
    const PARTIAL: &[&str] = &[
        "slab", "stair", "fence", "wall", "carpet", "snow", "path", "farmland",
    ];
    if PARTIAL.iter().any(|k| path.contains(k)) || path.ends_with("_bed") {
        return Mrs1Shape::Partial;
    }
    Mrs1Shape::Full
}

fn mrs1_state_luminance(path: &str, properties: &[(String, String)]) -> u8 {
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
    for (key, value) in properties {
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

fn mrs1_state_opacity(path: &str, properties: &[(String, String)]) -> u8 {
    let mut opacity = match mrs1_block_shape(path) {
        Mrs1Shape::Empty | Mrs1Shape::Thin => 0,
        Mrs1Shape::Partial | Mrs1Shape::Leaves | Mrs1Shape::Liquid => 1,
        Mrs1Shape::Full => 15,
    };
    // Carried water dims light by one level even on a transparent mount.
    if opacity == 0
        && properties
            .iter()
            .any(|(k, v)| k == "waterlogged" && v == "true")
    {
        opacity = 1;
    }
    opacity
}

/// `(luminance << 4) | opacity` for one mod block state, derived through the
/// same generic name/property rules on every peer. This is what v2 carries on
/// the wire and what v1 implies.
pub fn derive_mrs1_facts_byte(block_id: &str, properties: &[(String, String)]) -> u8 {
    let path = block_id.rsplit(':').next().unwrap_or(block_id);
    (mrs1_state_luminance(path, properties) << 4) | mrs1_state_opacity(path, properties)
}

// ---------------------------------------------------------------------------
// MRS1 registry frame
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Mrs1BlockStateEntry {
    pub state_id: u32,
    pub block_id: String,
    /// Property pairs in strictly ascending key order — the canonical wire
    /// order produced by the registry planner.
    pub properties: Vec<(String, String)>,
}

impl Mrs1BlockStateEntry {
    pub fn facts_byte(&self) -> u8 {
        derive_mrs1_facts_byte(&self.block_id, &self.properties)
    }
}

/// Registry content encoded in one `MRS1` frame.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Mrs1Registry {
    pub base_block_state_id: u32,
    pub base_item_id: u32,
    pub block_states: Vec<Mrs1BlockStateEntry>,
    pub items: BTreeMap<u32, String>,
}

/// Decoded registry. `facts_bytes` is parallel to `block_states`: v2 wire
/// bytes for a v2 frame, derived bytes for v1.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DecodedMrs1Registry {
    pub base_block_state_id: u32,
    pub base_item_id: u32,
    pub block_states: Vec<Mrs1BlockStateEntry>,
    pub facts_bytes: Vec<u8>,
    pub items: BTreeMap<u32, String>,
    pub version: u16,
}

fn valid_mrs1_string(value: &str) -> bool {
    !value.is_empty() && value.len() <= MAX_MRS1_STRING_BYTES
}

fn validate_mrs1_registry(registry: &Mrs1Registry) -> Result<(), String> {
    if registry.block_states.len() > MAX_MRS1_BLOCK_STATES {
        return Err(err("MRS1 block state count exceeds canonical cap"));
    }
    if registry.items.len() > MAX_MRS1_ITEMS {
        return Err(err("MRS1 item count exceeds canonical cap"));
    }
    let mut previous_state = None;
    for entry in &registry.block_states {
        if entry.state_id < registry.base_block_state_id
            || previous_state.is_some_and(|previous| entry.state_id <= previous)
        {
            return Err(err(
                "MRS1 block state ids must be monotonic and at/above base",
            ));
        }
        previous_state = Some(entry.state_id);
        if !valid_mrs1_string(&entry.block_id) {
            return Err(err("invalid MRS1 block id string"));
        }
        if entry.properties.len() > MAX_MRS1_PROPERTIES_PER_STATE {
            return Err(err("MRS1 property count exceeds canonical cap"));
        }
        let mut previous_key: Option<&str> = None;
        for (key, value) in &entry.properties {
            if !valid_mrs1_string(key) || !valid_mrs1_string(value) {
                return Err(err("invalid MRS1 property string"));
            }
            if previous_key.is_some_and(|previous| key.as_str() <= previous) {
                return Err(err("MRS1 property keys must be strictly ascending"));
            }
            previous_key = Some(key);
        }
    }
    let mut previous_item = None;
    for (id, name) in &registry.items {
        if *id < registry.base_item_id || previous_item.is_some_and(|previous| *id <= previous) {
            return Err(err("MRS1 item ids must be monotonic and at/above base"));
        }
        previous_item = Some(*id);
        if !valid_mrs1_string(name) {
            return Err(err("invalid MRS1 item string"));
        }
    }
    Ok(())
}

fn put_u16(out: &mut Vec<u8>, value: u16) {
    out.extend_from_slice(&value.to_be_bytes());
}

fn put_u32(out: &mut Vec<u8>, value: u32) {
    out.extend_from_slice(&value.to_be_bytes());
}

fn put_mrs1_string(out: &mut Vec<u8>, value: &str) {
    put_u16(out, value.len() as u16);
    out.extend_from_slice(value.as_bytes());
}

/// Encode one canonical `MRS1` frame at version 1 or 2. Encoded bytes are
/// identical to the deployed server implementation for every valid registry.
pub fn encode_mrs1_frame(registry: &Mrs1Registry, version: u16) -> Result<Vec<u8>, String> {
    if version != MRS1_VERSION_V1 && version != MRS1_VERSION_V2 {
        return Err(err("unsupported MRS1 encoder version"));
    }
    validate_mrs1_registry(registry)?;
    let mut out = Vec::new();
    out.extend_from_slice(&MRS1_MAGIC);
    put_u16(&mut out, version);
    put_u32(&mut out, registry.base_block_state_id);
    put_u32(&mut out, registry.base_item_id);
    put_u32(&mut out, registry.block_states.len() as u32);
    for entry in &registry.block_states {
        put_u32(&mut out, entry.state_id);
        put_mrs1_string(&mut out, &entry.block_id);
        put_u16(&mut out, entry.properties.len() as u16);
        for (key, value) in &entry.properties {
            put_mrs1_string(&mut out, key);
            put_mrs1_string(&mut out, value);
        }
        if version == MRS1_VERSION_V2 {
            out.push(entry.facts_byte());
        }
    }
    put_u32(&mut out, registry.items.len() as u32);
    for (id, name) in &registry.items {
        put_u32(&mut out, *id);
        put_mrs1_string(&mut out, name);
    }
    if out.len() + 4 > MAX_MRS1_FRAME_BYTES {
        return Err(err("MRS1 frame exceeds byte budget"));
    }
    let checksum = megu_core::hash::crc32_ieee(&out);
    put_u32(&mut out, checksum);
    Ok(out)
}

pub fn encode_mrs1_frame_v1(registry: &Mrs1Registry) -> Result<Vec<u8>, String> {
    encode_mrs1_frame(registry, MRS1_VERSION_V1)
}

pub fn encode_mrs1_frame_v2(registry: &Mrs1Registry) -> Result<Vec<u8>, String> {
    encode_mrs1_frame(registry, MRS1_VERSION_V2)
}

struct Mrs1Reader<'a> {
    bytes: &'a [u8],
    cursor: usize,
}

impl<'a> Mrs1Reader<'a> {
    fn take(&mut self, len: usize) -> Result<&'a [u8], String> {
        let end = self
            .cursor
            .checked_add(len)
            .filter(|end| *end <= self.bytes.len())
            .ok_or_else(|| err("truncated MRS1 frame"))?;
        let out = &self.bytes[self.cursor..end];
        self.cursor = end;
        Ok(out)
    }

    fn u16(&mut self) -> Result<u16, String> {
        Ok(u16::from_be_bytes(
            self.take(2)?
                .try_into()
                .map_err(|_| err("truncated MRS1 u16"))?,
        ))
    }

    fn u32(&mut self) -> Result<u32, String> {
        Ok(u32::from_be_bytes(
            self.take(4)?
                .try_into()
                .map_err(|_| err("truncated MRS1 u32"))?,
        ))
    }

    fn string(&mut self) -> Result<String, String> {
        let len = usize::from(self.u16()?);
        if len == 0 || len > MAX_MRS1_STRING_BYTES {
            return Err(err("invalid MRS1 string length"));
        }
        String::from_utf8(self.take(len)?.to_vec()).map_err(|_| err("invalid MRS1 UTF-8"))
    }
}

/// Decode one canonical `MRS1` v1/v2 frame. Checksum, truncation, trailing
/// bytes, monotonicity, canonical budgets and string validity all fail closed.
pub fn decode_mrs1_frame(bytes: &[u8]) -> Result<DecodedMrs1Registry, String> {
    if bytes.len() > MAX_MRS1_FRAME_BYTES {
        return Err(err("MRS1 frame exceeds byte budget"));
    }
    if bytes.len() < 4 + 2 + 4 + 4 + 4 + 4 + 4 {
        return Err(err("truncated MRS1 frame"));
    }
    let payload_len = bytes.len() - 4;
    let expected_crc = u32::from_be_bytes(
        bytes[payload_len..]
            .try_into()
            .map_err(|_| err("truncated MRS1 crc32"))?,
    );
    if megu_core::hash::crc32_ieee(&bytes[..payload_len]) != expected_crc {
        return Err(err("MRS1 crc32 mismatch"));
    }
    let mut reader = Mrs1Reader {
        bytes: &bytes[..payload_len],
        cursor: 0,
    };
    if reader.take(4)? != MRS1_MAGIC {
        return Err(err("invalid MRS1 magic"));
    }
    let version = reader.u16()?;
    if version != MRS1_VERSION_V1 && version != MRS1_VERSION_V2 {
        return Err(err("unsupported MRS1 version"));
    }
    let has_wire_facts = version == MRS1_VERSION_V2;
    let base_block_state_id = reader.u32()?;
    let base_item_id = reader.u32()?;
    let state_count = reader.u32()? as usize;
    if state_count > MAX_MRS1_BLOCK_STATES {
        return Err(err("MRS1 block state count exceeds canonical cap"));
    }
    let mut block_states = Vec::with_capacity(state_count);
    let mut facts_bytes = Vec::with_capacity(state_count);
    let mut previous_state = None;
    for _ in 0..state_count {
        let state_id = reader.u32()?;
        if state_id < base_block_state_id
            || previous_state.is_some_and(|previous| state_id <= previous)
        {
            return Err(err("non-monotonic MRS1 block state id"));
        }
        previous_state = Some(state_id);
        let block_id = reader.string()?;
        let property_count = usize::from(reader.u16()?);
        if property_count > MAX_MRS1_PROPERTIES_PER_STATE {
            return Err(err("MRS1 property count exceeds canonical cap"));
        }
        let mut properties = Vec::with_capacity(property_count);
        let mut previous_key: Option<String> = None;
        for _ in 0..property_count {
            let key = reader.string()?;
            if previous_key
                .as_ref()
                .is_some_and(|previous| key <= *previous)
            {
                return Err(err("MRS1 property keys must be strictly ascending"));
            }
            previous_key = Some(key.clone());
            properties.push((key, reader.string()?));
        }
        let facts_byte = if has_wire_facts {
            reader.take(1)?[0]
        } else {
            derive_mrs1_facts_byte(&block_id, &properties)
        };
        facts_bytes.push(facts_byte);
        block_states.push(Mrs1BlockStateEntry {
            state_id,
            block_id,
            properties,
        });
    }
    let item_count = reader.u32()? as usize;
    if item_count > MAX_MRS1_ITEMS {
        return Err(err("MRS1 item count exceeds canonical cap"));
    }
    let mut items = BTreeMap::new();
    let mut previous_item = None;
    for _ in 0..item_count {
        let id = reader.u32()?;
        if id < base_item_id || previous_item.is_some_and(|previous| id <= previous) {
            return Err(err("non-monotonic MRS1 item id"));
        }
        previous_item = Some(id);
        items.insert(id, reader.string()?);
    }
    if reader.cursor != reader.bytes.len() {
        return Err(err("trailing bytes in MRS1 frame"));
    }
    Ok(DecodedMrs1Registry {
        base_block_state_id,
        base_item_id,
        block_states,
        facts_bytes,
        items,
        version,
    })
}

// ---------------------------------------------------------------------------
// MSY1 manifest validation and wire codec
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Msy1Entry {
    pub mod_id: String,
    pub file_name: String,
    pub download_url: String,
    /// Lowercase 64-character SHA-256 hex. Uppercase input is accepted by
    /// structure validation but decoders always render lowercase, matching
    /// the deployed server codec.
    pub sha256_hex: String,
    pub size_bytes: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Msy1Manifest {
    pub entries: Vec<Msy1Entry>,
}

/// Lowercase hex rendering of a 32-byte digest.
pub fn msy1_digest_hex(digest: &[u8; 32]) -> String {
    megu_core::crypto::hex_encode(digest)
}

/// Parse a strict 64-hex-character SHA-256 digest (either case).
pub fn parse_msy1_sha256_hex(text: &str) -> Option<[u8; 32]> {
    if text.len() != 64 || !text.is_ascii() {
        return None;
    }
    let bytes = text.as_bytes();
    let mut out = [0u8; 32];
    for i in 0..32 {
        let hi = (bytes[2 * i] as char).to_digit(16)?;
        let lo = (bytes[2 * i + 1] as char).to_digit(16)?;
        out[i] = ((hi << 4) | lo) as u8;
    }
    Some(out)
}

/// Jailed `.jar` basename contract: pure ASCII, non-empty stem, no path
/// separators, controls, dotfile/traversal forms, `:` or shell metacharacters.
pub fn validate_msy1_jar_file_name(name: &str) -> Result<(), String> {
    if name.is_empty() || name.len() > MAX_MSY1_FILE_NAME_LEN {
        return Err(err("MSY1 file name length out of bounds"));
    }
    if !name.is_ascii() {
        return Err(err("MSY1 file name is not ASCII"));
    }
    for byte in name.bytes() {
        match byte {
            0x00..=0x1f | 0x7f => return Err(err("MSY1 file name has a control byte")),
            b'/' | b'\\' => return Err(err("MSY1 file name contains a path separator")),
            b':' => return Err(err("MSY1 file name contains ':'")),
            b'`' | b'$' | b'(' | b')' | b'{' | b'}' | b'[' | b']' | b'&' | b';' | b'|' | b'<'
            | b'>' | b'*' | b'?' | b'!' | b'\'' | b'"' | b'~' | b'#' => {
                return Err(err("MSY1 file name contains a shell metacharacter"))
            }
            _ => {}
        }
    }
    if name.starts_with('.') {
        return Err(err("MSY1 file name is a hidden dotfile"));
    }
    if name.contains("..") {
        return Err(err("MSY1 file name contains '..'"));
    }
    if name.len() <= 4 || !name[name.len() - 4..].eq_ignore_ascii_case(".jar") {
        return Err(err("MSY1 file name is not a .jar basename"));
    }
    Ok(())
}

/// Structural entry validation independent of download trust policy.
pub fn validate_msy1_entry_structure(entry: &Msy1Entry) -> Result<(), String> {
    if entry.mod_id.is_empty()
        || entry.mod_id.len() > MAX_MSY1_MOD_ID_LEN
        || !entry
            .mod_id
            .bytes()
            .all(|b| matches!(b, b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'_' | b'-' | b'.'))
    {
        return Err(err(
            "MSY1 mod id is empty, over-cap, or has an illegal byte",
        ));
    }
    validate_msy1_jar_file_name(&entry.file_name)?;
    if parse_msy1_sha256_hex(&entry.sha256_hex).is_none() {
        return Err(err("MSY1 sha256 is not a 64-hex digest"));
    }
    if !(1..=MAX_MSY1_JAR_BYTES).contains(&entry.size_bytes) {
        return Err(err("MSY1 jar size out of range"));
    }
    Ok(())
}

fn normalize_msy1_host(host: &str) -> Result<String, String> {
    if host.is_empty() || host.len() > 253 {
        return Err(err("MSY1 host length out of bounds"));
    }
    let mut normalized = host.to_ascii_lowercase();
    if normalized.ends_with('.') {
        normalized.pop();
    }
    if normalized.is_empty()
        || !normalized
            .bytes()
            .all(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'.' | b'-'))
    {
        return Err(err("MSY1 host is not a strict DNS name"));
    }
    for label in normalized.split('.') {
        if label.is_empty() || label.starts_with('-') || label.ends_with('-') {
            return Err(err("MSY1 host has a malformed DNS label"));
        }
    }
    Ok(normalized)
}

fn is_msy1_ip_literal_host(host: &str) -> bool {
    host.parse::<std::net::Ipv4Addr>().is_ok()
        || host.bytes().all(|b| b.is_ascii_digit() || b == b'.')
        || host.starts_with("0x")
}

fn is_msy1_local_or_internal_host(host: &str) -> bool {
    host == "localhost"
        || host.ends_with(".localhost")
        || host == "local"
        || host.ends_with(".local")
        || host.ends_with(".internal")
        || host.ends_with(".lan")
        || host.ends_with(".home")
        || host.ends_with(".corp")
        || host.ends_with(".intranet")
}

/// Validate HTTPS URL syntax without applying CDN trust policy. Returns the
/// normalized hostname. This matches the deployed server-side structural
/// contract: ASCII HTTPS, no credentials/control bytes/IPv6/raw IP/local
/// names, and no non-443 explicit port.
pub fn validate_msy1_url_syntax(url: &str) -> Result<String, String> {
    if url.is_empty() || url.len() > MAX_MSY1_URL_LEN {
        return Err(err("MSY1 URL length out of bounds"));
    }
    if !url.is_ascii() {
        return Err(err("MSY1 URL must be pure ASCII"));
    }
    if url.bytes().any(|b| b <= 0x20 || b == 0x7f) {
        return Err(err("MSY1 URL contains a control or space byte"));
    }
    if url.len() < 9 || !url.as_bytes()[..8].eq_ignore_ascii_case(b"https://") {
        return Err(err("MSY1 URL is not an https:// URL"));
    }
    let rest = &url[8..];
    let auth_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let authority = &rest[..auth_end];
    if authority.is_empty() {
        return Err(err("MSY1 URL has an empty authority"));
    }
    if authority.contains('@') {
        return Err(err("MSY1 URL embeds credentials or userinfo"));
    }
    let host = if authority.starts_with('[') {
        return Err(err("MSY1 URL uses an IPv6 literal"));
    } else if let Some(colon) = authority.rfind(':') {
        if &authority[colon + 1..] != "443" {
            return Err(err("MSY1 URL uses a non-443 port"));
        }
        &authority[..colon]
    } else {
        authority
    };
    let host = normalize_msy1_host(host)?;
    if is_msy1_ip_literal_host(&host) {
        return Err(err("MSY1 URL uses a raw IP literal host"));
    }
    if is_msy1_local_or_internal_host(&host) {
        return Err(err("MSY1 URL targets a local or internal host"));
    }
    Ok(host)
}

/// Canonical client download policy: exact trusted CDN host match only.
pub fn validate_msy1_canonical_download_url(url: &str) -> Result<(), String> {
    let host = validate_msy1_url_syntax(url)?;
    if TRUSTED_MSY1_CDN_HOSTS.contains(&host.as_str()) {
        Ok(())
    } else {
        Err(err("MSY1 URL host is not a trusted canonical mod CDN"))
    }
}

/// Deterministically encode an `MSY1` manifest. Byte order and field widths
/// exactly match the deployed server implementation.
pub fn encode_msy1_manifest(manifest: &Msy1Manifest) -> Result<Vec<u8>, String> {
    if manifest.entries.len() > MAX_MSY1_MODS {
        return Err(err("MSY1 entry count exceeds cap"));
    }
    let mut out = Vec::with_capacity(6 + manifest.entries.len() * 96);
    out.extend_from_slice(&MSY1_MAGIC);
    out.extend_from_slice(&(manifest.entries.len() as u16).to_be_bytes());
    for entry in &manifest.entries {
        validate_msy1_entry_structure(entry)?;
        validate_msy1_url_syntax(&entry.download_url)?;
        let digest = parse_msy1_sha256_hex(&entry.sha256_hex)
            .ok_or_else(|| err("MSY1 sha256 is not a 64-hex digest"))?;
        out.push(entry.mod_id.len() as u8);
        out.extend_from_slice(entry.mod_id.as_bytes());
        out.push(entry.file_name.len() as u8);
        out.extend_from_slice(entry.file_name.as_bytes());
        out.extend_from_slice(&(entry.download_url.len() as u16).to_be_bytes());
        out.extend_from_slice(entry.download_url.as_bytes());
        out.extend_from_slice(&entry.size_bytes.to_be_bytes());
        out.extend_from_slice(&digest);
    }
    Ok(out)
}

struct Msy1Reader<'a> {
    bytes: &'a [u8],
    cursor: usize,
}

impl<'a> Msy1Reader<'a> {
    fn take(&mut self, len: usize) -> Result<&'a [u8], String> {
        let end = self
            .cursor
            .checked_add(len)
            .filter(|end| *end <= self.bytes.len())
            .ok_or_else(|| err("truncated MSY1 body"))?;
        let out = &self.bytes[self.cursor..end];
        self.cursor = end;
        Ok(out)
    }

    fn u8(&mut self) -> Result<u8, String> {
        Ok(self.take(1)?[0])
    }

    fn u16(&mut self) -> Result<u16, String> {
        Ok(u16::from_be_bytes(
            self.take(2)?
                .try_into()
                .map_err(|_| err("truncated MSY1 u16"))?,
        ))
    }

    fn u64(&mut self) -> Result<u64, String> {
        Ok(u64::from_be_bytes(
            self.take(8)?
                .try_into()
                .map_err(|_| err("truncated MSY1 u64"))?,
        ))
    }

    fn string(&mut self, len: usize, cap: usize, what: &str) -> Result<String, String> {
        if len > cap {
            return Err(format!("MSY1 {what} length exceeds cap"));
        }
        let bytes = self.take(len)?;
        String::from_utf8(bytes.to_vec()).map_err(|_| format!("invalid MSY1 {what} UTF-8"))
    }
}

/// Decode and structurally validate one `MSY1` manifest. Bad magic, over-cap
/// or provably impossible counts, truncation, unsafe entry data, malformed URL
/// syntax and trailing bytes all reject the complete frame.
pub fn decode_msy1_manifest(bytes: &[u8]) -> Result<Msy1Manifest, String> {
    if bytes.len() < 6 {
        return Err(err("MSY1 body shorter than header"));
    }
    if bytes[..4] != MSY1_MAGIC {
        return Err(err("bad MSY1 magic"));
    }
    let count = usize::from(u16::from_be_bytes([bytes[4], bytes[5]]));
    if count > MAX_MSY1_MODS {
        return Err(err("MSY1 mod count exceeds cap"));
    }
    if count > (bytes.len() - 6) / MIN_MSY1_ENTRY_WIRE_BYTES {
        return Err(err("MSY1 declared count exceeds remaining body"));
    }
    let mut reader = Msy1Reader { bytes, cursor: 6 };
    let mut entries = Vec::with_capacity(count);
    for _ in 0..count {
        let id_len = usize::from(reader.u8()?);
        let mod_id = reader.string(id_len, MAX_MSY1_MOD_ID_LEN, "mod id")?;
        let file_len = usize::from(reader.u8()?);
        let file_name = reader.string(file_len, MAX_MSY1_FILE_NAME_LEN, "file name")?;
        let url_len = usize::from(reader.u16()?);
        let download_url = reader.string(url_len, MAX_MSY1_URL_LEN, "download URL")?;
        let size_bytes = reader.u64()?;
        let digest_bytes = reader.take(32)?;
        let mut digest = [0u8; 32];
        digest.copy_from_slice(digest_bytes);
        let entry = Msy1Entry {
            mod_id,
            file_name,
            download_url,
            sha256_hex: msy1_digest_hex(&digest),
            size_bytes,
        };
        validate_msy1_entry_structure(&entry)?;
        validate_msy1_url_syntax(&entry.download_url)?;
        entries.push(entry);
    }
    if reader.cursor != bytes.len() {
        return Err(err("trailing bytes in MSY1 body"));
    }
    Ok(Msy1Manifest { entries })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prop(key: &str, value: &str) -> (String, String) {
        (key.to_string(), value.to_string())
    }

    fn entry(state_id: u32, block_id: &str, props: &[(&str, &str)]) -> Mrs1BlockStateEntry {
        Mrs1BlockStateEntry {
            state_id,
            block_id: block_id.to_string(),
            properties: props.iter().map(|(k, v)| prop(k, v)).collect(),
        }
    }

    fn registry() -> Mrs1Registry {
        let mut items = BTreeMap::new();
        items.insert(70, "create:wrench".to_string());
        Mrs1Registry {
            base_block_state_id: 40,
            base_item_id: 70,
            block_states: vec![
                entry(40, "create:brass_block", &[]),
                entry(41, "create:arc_lamp", &[("lit", "true")]),
                entry(42, "create:framed_glass", &[]),
            ],
            items,
        }
    }

    fn msy_entry() -> Msy1Entry {
        Msy1Entry {
            mod_id: "create".to_string(),
            file_name: "create-1.0.jar".to_string(),
            download_url: "https://cdn.modrinth.com/data/create/create-1.0.jar".to_string(),
            sha256_hex: "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
                .to_string(),
            size_bytes: 12_345,
        }
    }

    #[test]
    fn mrs1_v1_and_v2_round_trip_and_derive_identical_facts() {
        let registry = registry();
        let v1 = encode_mrs1_frame_v1(&registry).unwrap();
        let v2 = encode_mrs1_frame_v2(&registry).unwrap();
        assert!(v1.starts_with(&MRS1_MAGIC));
        assert!(v2.starts_with(&MRS1_MAGIC));
        assert_ne!(v1, v2);

        let d1 = decode_mrs1_frame(&v1).unwrap();
        let d2 = decode_mrs1_frame(&v2).unwrap();
        assert_eq!(d1.version, 1);
        assert_eq!(d2.version, 2);
        assert_eq!(d1.block_states, registry.block_states);
        assert_eq!(d2.block_states, registry.block_states);
        assert_eq!(d1.items, registry.items);
        assert_eq!(d2.items, registry.items);
        assert_eq!(d1.facts_bytes, d2.facts_bytes);
        assert_eq!(d2.facts_bytes, vec![0x0f, 0xff, 0x00]);
    }

    #[test]
    fn mrs1_encoder_matches_deployed_v1_byte_layout_exactly() {
        let registry = Mrs1Registry {
            base_block_state_id: 40,
            base_item_id: 70,
            block_states: vec![entry(40, "create:wheel", &[("axis", "y")])],
            items: BTreeMap::from([(70, "create:wrench".to_string())]),
        };
        let encoded = encode_mrs1_frame_v1(&registry).unwrap();
        let mut expected = b"MRS1".to_vec();
        expected.extend_from_slice(&1u16.to_be_bytes());
        expected.extend_from_slice(&40u32.to_be_bytes());
        expected.extend_from_slice(&70u32.to_be_bytes());
        expected.extend_from_slice(&1u32.to_be_bytes());
        expected.extend_from_slice(&40u32.to_be_bytes());
        push_wire_string(&mut expected, "create:wheel");
        expected.extend_from_slice(&1u16.to_be_bytes());
        for string in ["axis", "y"] {
            expected.extend_from_slice(&(string.len() as u16).to_be_bytes());
            expected.extend_from_slice(string.as_bytes());
        }
        expected.extend_from_slice(&1u32.to_be_bytes());
        expected.extend_from_slice(&70u32.to_be_bytes());
        push_wire_string(&mut expected, "create:wrench");
        let crc = megu_core::hash::crc32_ieee(&expected);
        expected.extend_from_slice(&crc.to_be_bytes());
        assert_eq!(encoded, expected);
    }

    #[test]
    fn mrs1_malformed_frames_fail_closed() {
        let valid = encode_mrs1_frame_v2(&registry()).unwrap();
        assert!(decode_mrs1_frame(&valid[..valid.len() - 1]).is_err());
        let mut bad_crc = valid.clone();
        let last = bad_crc.len() - 1;
        bad_crc[last] ^= 0xff;
        assert!(decode_mrs1_frame(&bad_crc).is_err());

        let mut wrong_magic = valid.clone();
        wrong_magic[0] = b'X';
        let len = wrong_magic.len();
        let crc = megu_core::hash::crc32_ieee(&wrong_magic[..len - 4]);
        wrong_magic[len - 4..].copy_from_slice(&crc.to_be_bytes());
        assert!(decode_mrs1_frame(&wrong_magic).is_err());

        let mut wrong_version = valid.clone();
        wrong_version[4] = 0;
        wrong_version[5] = 3;
        let crc = megu_core::hash::crc32_ieee(&wrong_version[..len - 4]);
        wrong_version[len - 4..].copy_from_slice(&crc.to_be_bytes());
        assert!(decode_mrs1_frame(&wrong_version).is_err());
    }

    #[test]
    fn mrs1_caps_order_and_trailing_bytes_are_enforced() {
        let mut too_many_props = Vec::new();
        for i in 0..=MAX_MRS1_PROPERTIES_PER_STATE {
            too_many_props.push((format!("p{i:03}"), "v".to_string()));
        }
        let registry = Mrs1Registry {
            base_block_state_id: 1,
            base_item_id: 1,
            block_states: vec![Mrs1BlockStateEntry {
                state_id: 1,
                block_id: "test:block".to_string(),
                properties: too_many_props,
            }],
            items: BTreeMap::new(),
        };
        assert!(encode_mrs1_frame_v1(&registry).is_err());
        assert_eq!(MAX_MRS1_LEGACY_CLIENT_PROPERTY_BUDGET, 4_096);

        let disordered = Mrs1Registry {
            block_states: vec![entry(1, "test:a", &[]), entry(1, "test:b", &[])],
            ..registry
        };
        assert!(encode_mrs1_frame_v1(&disordered).is_err());

        let mut valid = encode_mrs1_frame_v1(&Mrs1Registry::default()).unwrap();
        // Rewriting the checksum makes the extra byte a trailing-byte failure
        // rather than a checksum failure.
        valid.insert(valid.len() - 4, 0);
        let len = valid.len();
        let crc = megu_core::hash::crc32_ieee(&valid[..len - 4]);
        valid[len - 4..].copy_from_slice(&crc.to_be_bytes());
        assert!(decode_mrs1_frame(&valid).is_err());
    }

    #[test]
    fn mrs1_fact_derivation_matches_server_shape_rules() {
        assert_eq!(derive_mrs1_facts_byte("mymod:brass_block", &[]), 0x0f);
        assert_eq!(derive_mrs1_facts_byte("mymod:arc_lamp", &[]), 0xef);
        assert_eq!(
            derive_mrs1_facts_byte("mymod:arc_lamp", &[prop("lit", "false")]),
            0x0f
        );
        assert_eq!(
            derive_mrs1_facts_byte("mymod:shaft", &[prop("waterlogged", "true")]),
            0x01
        );
        assert_eq!(derive_mrs1_facts_byte("mymod:grass_block", &[]), 0x0f);
        assert_eq!(derive_mrs1_facts_byte("mymod:fluid_pipe", &[]), 0x00);
    }

    #[test]
    fn msy1_round_trip_matches_deployed_byte_layout() {
        let manifest = Msy1Manifest {
            entries: vec![msy_entry()],
        };
        let encoded = encode_msy1_manifest(&manifest).unwrap();
        let entry = msy_entry();
        let mut expected = b"MSY1".to_vec();
        expected.extend_from_slice(&1u16.to_be_bytes());
        expected.push(entry.mod_id.len() as u8);
        expected.extend_from_slice(entry.mod_id.as_bytes());
        expected.push(entry.file_name.len() as u8);
        expected.extend_from_slice(entry.file_name.as_bytes());
        expected.extend_from_slice(&(entry.download_url.len() as u16).to_be_bytes());
        expected.extend_from_slice(entry.download_url.as_bytes());
        expected.extend_from_slice(&entry.size_bytes.to_be_bytes());
        expected.extend_from_slice(&parse_msy1_sha256_hex(&entry.sha256_hex).unwrap());
        assert_eq!(encoded, expected);
        assert_eq!(decode_msy1_manifest(&encoded).unwrap(), manifest);
    }

    #[test]
    fn msy1_empty_manifest_round_trips() {
        let manifest = Msy1Manifest::default();
        let encoded = encode_msy1_manifest(&manifest).unwrap();
        assert_eq!(encoded, b"MSY1\0\0");
        assert_eq!(decode_msy1_manifest(&encoded).unwrap(), manifest);
    }

    #[test]
    fn msy1_malformed_frames_fail_closed() {
        let valid = encode_msy1_manifest(&Msy1Manifest {
            entries: vec![msy_entry()],
        })
        .unwrap();
        assert!(decode_msy1_manifest(b"MSY").is_err());
        assert!(decode_msy1_manifest(b"NOPE\0\0").is_err());
        assert!(decode_msy1_manifest(&valid[..valid.len() - 1]).is_err());
        let mut trailing = valid.clone();
        trailing.push(0);
        assert!(decode_msy1_manifest(&trailing).is_err());

        let mut impossible_count = b"MSY1".to_vec();
        impossible_count.extend_from_slice(&1u16.to_be_bytes());
        assert!(decode_msy1_manifest(&impossible_count).is_err());
    }

    #[test]
    fn msy1_entry_and_url_validation_are_structural_then_policy_layered() {
        let mut entry = msy_entry();
        validate_msy1_entry_structure(&entry).unwrap();
        validate_msy1_canonical_download_url(&entry.download_url).unwrap();

        entry.file_name = "../evil.jar".to_string();
        assert!(validate_msy1_entry_structure(&entry).is_err());
        entry.file_name = "evil.jar".to_string();
        entry.sha256_hex = "not hex".to_string();
        assert!(validate_msy1_entry_structure(&entry).is_err());
        entry.sha256_hex = msy_entry().sha256_hex;
        entry.size_bytes = 0;
        assert!(validate_msy1_entry_structure(&entry).is_err());

        assert!(validate_msy1_url_syntax("http://cdn.modrinth.com/x.jar").is_err());
        assert!(validate_msy1_url_syntax("https://cdn.modrinth.com@evil.example/x.jar").is_err());
        assert!(validate_msy1_url_syntax("https://127.0.0.1/x.jar").is_err());
        assert!(validate_msy1_url_syntax("https://cdn.modrinth.com:444/x.jar").is_err());

        // Structural syntax and client trust policy are intentionally separate:
        // operator mirrors can be structurally valid without being canonical CDNs.
        let mirror = "https://mirror.example.org/create.jar";
        assert!(validate_msy1_url_syntax(mirror).is_ok());
        assert!(validate_msy1_canonical_download_url(mirror).is_err());
    }

    #[test]
    fn msy1_caps_are_enforced_before_allocation() {
        let mut entries = Vec::new();
        for _ in 0..=MAX_MSY1_MODS {
            entries.push(msy_entry());
        }
        assert!(encode_msy1_manifest(&Msy1Manifest { entries }).is_err());

        let mut entry = msy_entry();
        entry.mod_id = "x".repeat(MAX_MSY1_MOD_ID_LEN + 1);
        assert!(validate_msy1_entry_structure(&entry).is_err());
        entry = msy_entry();
        entry.file_name = format!("{}.jar", "x".repeat(MAX_MSY1_FILE_NAME_LEN));
        assert!(validate_msy1_entry_structure(&entry).is_err());
        entry = msy_entry();
        entry.download_url = format!("https://cdn.modrinth.com/{}", "x".repeat(MAX_MSY1_URL_LEN));
        assert!(encode_msy1_manifest(&Msy1Manifest {
            entries: vec![entry]
        })
        .is_err());
    }

    fn push_wire_string(out: &mut Vec<u8>, value: &str) {
        out.extend_from_slice(&(value.len() as u16).to_be_bytes());
        out.extend_from_slice(value.as_bytes());
    }

    fn finish_mrs1_crc(mut frame: Vec<u8>) -> Vec<u8> {
        let crc = megu_core::hash::crc32_ieee(&frame);
        frame.extend_from_slice(&crc.to_be_bytes());
        frame
    }

    #[test]
    fn mrs1_property_boundary_64_passes_and_65_decode_fails() {
        let properties: Vec<_> = (0..MAX_MRS1_PROPERTIES_PER_STATE)
            .map(|i| (format!("p{i:03}"), "v".to_string()))
            .collect();
        let registry = Mrs1Registry {
            base_block_state_id: 1,
            base_item_id: 1,
            block_states: vec![Mrs1BlockStateEntry {
                state_id: 1,
                block_id: "test:boundary".to_string(),
                properties: properties.clone(),
            }],
            items: BTreeMap::new(),
        };
        let encoded = encode_mrs1_frame_v1(&registry).unwrap();
        let decoded = decode_mrs1_frame(&encoded).unwrap();
        assert_eq!(decoded.block_states[0].properties, properties);

        // Hand-encode one byte-valid but over-contract 65-property frame:
        // the old 4,096 client allocation ceiling must not widen the shared
        // canonical decode contract.
        let mut frame = b"MRS1".to_vec();
        frame.extend_from_slice(&1u16.to_be_bytes());
        frame.extend_from_slice(&1u32.to_be_bytes());
        frame.extend_from_slice(&1u32.to_be_bytes());
        frame.extend_from_slice(&1u32.to_be_bytes());
        frame.extend_from_slice(&1u32.to_be_bytes());
        push_wire_string(&mut frame, "test:boundary");
        frame.extend_from_slice(&65u16.to_be_bytes());
        for i in 0..65 {
            push_wire_string(&mut frame, &format!("p{i:03}"));
            push_wire_string(&mut frame, "v");
        }
        frame.extend_from_slice(&0u32.to_be_bytes());
        assert!(decode_mrs1_frame(&finish_mrs1_crc(frame)).is_err());
    }

    #[test]
    fn mrs1_decode_rejects_non_monotonic_state_ids_after_valid_crc() {
        let mut frame = encode_mrs1_frame_v2(&registry()).unwrap();
        // The second state id starts immediately after the first entry. Make
        // it duplicate the first id while preserving a valid checksum, so the
        // failure is specifically monotonicity validation.
        let first_len = 4 + 2 + "create:brass_block".len() + 2 + 1;
        let second_state_offset = 18 + first_len;
        frame[second_state_offset..second_state_offset + 4].copy_from_slice(&40u32.to_be_bytes());
        let len = frame.len();
        let crc = megu_core::hash::crc32_ieee(&frame[..len - 4]);
        frame[len - 4..].copy_from_slice(&crc.to_be_bytes());
        assert!(decode_mrs1_frame(&frame).is_err());
    }

    #[test]
    fn msy1_exact_field_and_size_boundaries_round_trip() {
        let prefix = "https://cdn.modrinth.com/";
        let mut entry = msy_entry();
        entry.mod_id = "m".repeat(MAX_MSY1_MOD_ID_LEN);
        entry.file_name = format!("{}.jar", "f".repeat(MAX_MSY1_FILE_NAME_LEN - 4));
        entry.download_url = format!("{}{}", prefix, "u".repeat(MAX_MSY1_URL_LEN - prefix.len()));
        entry.size_bytes = MAX_MSY1_JAR_BYTES;
        assert_eq!(entry.download_url.len(), MAX_MSY1_URL_LEN);
        let manifest = Msy1Manifest {
            entries: vec![entry],
        };
        let encoded = encode_msy1_manifest(&manifest).unwrap();
        assert_eq!(decode_msy1_manifest(&encoded).unwrap(), manifest);
    }
}
