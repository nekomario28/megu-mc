// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
//! Data-driven protocol tables and cross-version id remapping.
//!
//! A [`ProtocolTable`] is the authoritative `resource_name <-> protocol_id` map
//! for one Minecraft version: packet ids per `(phase, direction)` plus the core
//! registries (`minecraft:block`, `minecraft:item`, `minecraft:entity_type`,
//! `minecraft:menu`, `minecraft:data_component_type`).
//!
//! [`ProtocolTable::v767_builtin`] ships the verified 1.21.1 tables — the same
//! ids `src/play_protocol.rs`, `src/menus_and_items.rs` and friends hard-code
//! today. [`ProtocolTable::from_version_pack_json`] loads the identical shape
//! from normalized DataGenerator reports (`packet_ids.json`,
//! `registry_ids.json`), so supporting 1.21.4/770 is a data file, not a new
//! codec.
//!
//! [`IdRemapper`] then translates ids between two versions of one registry in
//! O(1), by matching resource names — correct under renumbering, additions and
//! removals, which a numeric offset table is not.

use std::collections::HashMap;
use std::fmt;

use crate::data_v767::{
    V767_BLOCKS, V767_COMPONENTS, V767_ENTITY_TYPES, V767_ITEMS, V767_MENUS, V767_PACKETS,
};
use crate::wire::ProtoError;

/// The five protocol phases a connection moves through.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PacketPhase {
    Handshake,
    Status,
    Login,
    Configuration,
    Play,
}

impl PacketPhase {
    /// Names accepted in `packet_ids.json` phase objects.
    fn from_pack_name(name: &str) -> Option<Self> {
        Some(match name {
            "handshake" => Self::Handshake,
            "status" => Self::Status,
            "login" => Self::Login,
            "configuration" => Self::Configuration,
            "play" => Self::Play,
            _ => return None,
        })
    }
}

impl fmt::Display for PacketPhase {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Handshake => "handshake",
            Self::Status => "status",
            Self::Login => "login",
            Self::Configuration => "configuration",
            Self::Play => "play",
        })
    }
}

/// Direction of travel on the wire, from the server's point of view
/// (`Clientbound` = S2C, `Serverbound` = C2S).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PacketDir {
    Serverbound,
    Clientbound,
}

impl PacketDir {
    /// Names accepted in `packet_ids.json` direction objects.
    fn from_pack_name(name: &str) -> Option<Self> {
        Some(match name {
            "serverbound" => Self::Serverbound,
            "clientbound" => Self::Clientbound,
            _ => return None,
        })
    }

    pub fn opposite(self) -> Self {
        match self {
            Self::Serverbound => Self::Clientbound,
            Self::Clientbound => Self::Serverbound,
        }
    }
}

impl fmt::Display for PacketDir {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Serverbound => "serverbound",
            Self::Clientbound => "clientbound",
        })
    }
}

/// One bidirectional `resource_name <-> id` map. Names compare case- and
/// spelling-exactly — the table never normalizes `minecraft:` prefixes because
/// a silent canonicalization is how a duplicated alias slips in.
#[derive(Clone, Debug, Default)]
pub struct Registry {
    id_to_name: Vec<String>,
    name_to_id: HashMap<String, u32>,
}

impl Registry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Build a dense registry whose names occupy ids `0..names.len()`.
    pub fn from_dense_names<S: AsRef<str>>(names: &[S]) -> Self {
        let mut reg = Self::new();
        for (id, name) in names.iter().enumerate() {
            reg.insert(name.as_ref(), id as u32)
                .expect("static name table contains a duplicate");
        }
        reg
    }

    /// Insert one entry; fails closed on duplicate names or ids.
    pub fn insert(&mut self, name: &str, id: u32) -> Result<(), ProtoError> {
        if self.name_to_id.contains_key(name) {
            return Err(ProtoError::DuplicateEntry(
                "registry already holds this resource name",
            ));
        }
        match self.id_to_name.get_mut(id as usize) {
            Some(slot) if !slot.is_empty() => {
                return Err(ProtoError::DuplicateEntry(
                    "registry id already bound to a different name",
                ));
            }
            Some(slot) => *slot = name.to_owned(), // fill a sparse hole
            None => {
                while self.id_to_name.len() < id as usize {
                    self.id_to_name.push(String::new());
                }
                self.id_to_name.push(name.to_owned());
            }
        }
        self.name_to_id.insert(name.to_owned(), id);
        Ok(())
    }

    pub fn id(&self, name: &str) -> Option<u32> {
        self.name_to_id.get(name).copied()
    }

    /// Name at `id`; `None` for unbound ids (dense registries never return
    /// `None` inside `0..len`, sparse pack-loaded ones can).
    pub fn name(&self, id: u32) -> Option<&str> {
        let name = self.id_to_name.get(id as usize)?;
        (!name.is_empty()).then_some(name.as_str())
    }

    /// Highest bound id + 1. For sparse (pack-loaded) tables this may exceed
    /// the number of real entries.
    pub fn len(&self) -> u32 {
        self.id_to_name.len() as u32
    }

    pub fn is_empty(&self) -> bool {
        self.id_to_name.is_empty()
    }

    /// Iterate `(id, name)` pairs in id order; sparse holes are skipped.
    pub fn entries(&self) -> impl Iterator<Item = (u32, &str)> + '_ {
        self.id_to_name
            .iter()
            .enumerate()
            .filter_map(|(i, n)| (!n.is_empty()).then_some((i as u32, n.as_str())))
    }
}

/// All packet-id tables and core registries for one protocol version.
#[derive(Clone, Debug, Default)]
pub struct ProtocolTable {
    /// Numeric protocol version this table describes (e.g. 767), or `None`
    /// when built ad hoc rather than from a versioned source.
    protocol_version: Option<i32>,
    packets: HashMap<(PacketPhase, PacketDir), Registry>,
    registries: HashMap<String, Registry>,
}

impl ProtocolTable {
    pub fn new() -> Self {
        Self::default()
    }

    /// The verified builtin Protocol 767 (Minecraft 1.21.1) table.
    ///
    /// Packet, menu and data-component ids are the same numeric values
    /// `src/play_protocol.rs`/`src/menus_and_items.rs`/`src/entity_registry.rs`
    /// put on the wire today; `minecraft:item` mirrors `ITEM_NAMES` (verified
    /// against the 1.21.1 vanilla registry) and `minecraft:block` mirrors the
    /// `blocks_1211.json` asset order.
    pub fn v767_builtin() -> Self {
        let mut table = Self::new();
        table.protocol_version = Some(767);
        for (phase, dir, name, id) in V767_PACKETS {
            table
                .insert_packet(phase, dir, name, id)
                .expect("builtin 767 packet table contains a duplicate");
        }
        table.insert_registry("minecraft:block", Registry::from_dense_names(&V767_BLOCKS));
        table.insert_registry("minecraft:item", Registry::from_dense_names(&V767_ITEMS));
        table.insert_registry(
            "minecraft:entity_type",
            Registry::from_dense_names(&V767_ENTITY_TYPES),
        );
        table.insert_registry("minecraft:menu", Registry::from_dense_names(&V767_MENUS));
        table.insert_registry(
            "minecraft:data_component_type",
            Registry::from_dense_names(&V767_COMPONENTS),
        );
        table
    }

    /// Build a table from the normalized version-pack JSON pair produced from
    /// vanilla DataGenerator reports.
    ///
    /// `packet_ids_json` shape:
    /// ```json
    /// {"protocol_version": 767,
    ///  "phases": {"play": {"clientbound": {"keep_alive": 38}, "serverbound": {...}}}}
    /// ```
    /// `registry_ids_json` shape — each registry maps resource names to ids;
    /// a bare array `[name, ...]` means ids `0..len` (the common dense form):
    /// ```json
    /// {"registries": {"minecraft:item": {"minecraft:stone": 1},
    ///                 "minecraft:menu": ["minecraft:anvil", ...]}}
    /// ```
    /// `protocol_version`, unknown phases/directions, non-integer ids, and any
    /// duplicate name or id fail closed with [`ProtoError::VersionPack`].
    pub fn from_version_pack_json(
        packet_ids_json: &str,
        registry_ids_json: &str,
    ) -> Result<Self, ProtoError> {
        let packets_doc = json_parse(packet_ids_json)?;
        let registries_doc = json_parse(registry_ids_json)?;
        let mut table = Self::new();

        let root = object(&packets_doc, "packet_ids root")?;
        if let Some(v) = obj_get(root, "protocol_version") {
            let n = int(v, "protocol_version")?;
            if !(0..=i64::from(i32::MAX)).contains(&n) {
                return Err(ProtoError::VersionPack("protocol_version out of range"));
            }
            table.protocol_version = Some(n as i32);
        }
        let phases = object(
            obj_get(root, "phases")
                .ok_or(ProtoError::VersionPack("packet_ids: missing \"phases\""))?,
            "packet_ids.phases",
        )?;
        for (phase_name, dir_map) in phases {
            let phase = PacketPhase::from_pack_name(phase_name)
                .ok_or(ProtoError::VersionPack("packet_ids: unknown phase name"))?;
            for (dir_name, id_map) in object(dir_map, "packet_ids.phases.<phase>")? {
                let dir = PacketDir::from_pack_name(dir_name)
                    .ok_or(ProtoError::VersionPack("packet_ids: unknown direction"))?;
                for (name, id) in object(id_map, "packet_ids entry table")? {
                    let id = int(id, "packet id")?;
                    if !(0..=255).contains(&id) {
                        return Err(ProtoError::VersionPack("packet id out of byte range"));
                    }
                    if name.is_empty() {
                        return Err(ProtoError::VersionPack("empty packet resource name"));
                    }
                    table.insert_packet(phase, dir, name, id as u32)?;
                }
            }
        }

        let reg_root = object(&registries_doc, "registry_ids root")?;
        let registries = object(
            obj_get(reg_root, "registries")
                .ok_or(ProtoError::VersionPack("registry_ids: missing \"registries\""))?,
            "registry_ids.registries",
        )?;
        for (registry_name, entries) in registries {
            if registry_name.is_empty() {
                return Err(ProtoError::VersionPack("empty registry name"));
            }
            let mut registry = Registry::new();
            match entries {
                Json::Object(map) => {
                    for (name, id) in map {
                        let id = int(id, "registry id")?;
                        if !(0..=i64::from(u32::MAX)).contains(&id) {
                            return Err(ProtoError::VersionPack("registry id out of u32 range"));
                        }
                        if name.is_empty() {
                            return Err(ProtoError::VersionPack("empty resource name"));
                        }
                        registry.insert(name, id as u32)?;
                    }
                }
                Json::Array(names) => {
                    for (id, name) in names.iter().enumerate() {
                        let Json::String(name) = name else {
                            return Err(ProtoError::VersionPack(
                                "registry array entries must be strings",
                            ));
                        };
                        registry.insert(name, id as u32)?;
                    }
                }
                _ => return Err(ProtoError::VersionPack("registry entries must be object or array")),
            }
            if table.registries.contains_key(registry_name) {
                return Err(ProtoError::DuplicateEntry(
                    "registry_ids repeats a registry name",
                ));
            }
            table.registries.insert(registry_name.clone(), registry);
        }
        Ok(table)
    }

    /// Recorded protocol version (`from_version_pack_json` packs and
    /// `v767_builtin` set it).
    pub fn protocol_version(&self) -> Option<i32> {
        self.protocol_version
    }

    /// Replace/attach a named registry.
    pub fn insert_registry(&mut self, name: &str, registry: Registry) {
        self.registries.insert(name.to_owned(), registry);
    }

    /// Insert one packet id; fails closed on a name or id collision within
    /// `(phase, dir)`.
    pub fn insert_packet(
        &mut self,
        phase: PacketPhase,
        dir: PacketDir,
        name: &str,
        id: u32,
    ) -> Result<(), ProtoError> {
        self.packets
            .entry((phase, dir))
            .or_default()
            .insert(name, id)
    }

    /// Id of a named packet in `(phase, dir)`.
    pub fn packet_id(&self, phase: PacketPhase, dir: PacketDir, name: &str) -> Option<u32> {
        self.packets.get(&(phase, dir))?.id(name)
    }

    /// Name of a packet id in `(phase, dir)`.
    pub fn packet_name(&self, phase: PacketPhase, dir: PacketDir, id: u32) -> Option<&str> {
        self.packets.get(&(phase, dir))?.name(id)
    }

    /// The whole `(phase, dir)` id table, when it exists.
    pub fn packet_registry(&self, phase: PacketPhase, dir: PacketDir) -> Option<&Registry> {
        self.packets.get(&(phase, dir))
    }

    /// A named core registry (`"minecraft:item"`, ...).
    pub fn registry(&self, name: &str) -> Option<&Registry> {
        self.registries.get(name)
    }

    /// Id of `resource` in registry `registry` (e.g. `("minecraft:menu",
    /// "minecraft:anvil") -> 8`).
    pub fn registry_id(&self, registry: &str, resource: &str) -> Option<u32> {
        self.registries.get(registry)?.id(resource)
    }

    /// Resource name bound to `id` in `registry`.
    pub fn registry_name(&self, registry: &str, id: u32) -> Option<&str> {
        self.registries.get(registry)?.name(id)
    }

    /// Names of all attached registries.
    pub fn registry_names(&self) -> impl Iterator<Item = &str> + '_ {
        self.registries.keys().map(String::as_str)
    }
}

/// What an [`IdRemapper`] could and could not translate.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RemapReport {
    /// Names present in both registries and therefore remappable.
    pub mapped_count: usize,
    /// `(canonical id, name)` entries with no counterpart on the wire version
    /// — packets carrying these must degrade or be dropped.
    pub unmapped_canonical: Vec<(u32, String)>,
    /// `(wire id, name)` entries with no canonical counterpart — wire packets
    /// carrying these have no meaning to this codebase.
    pub unmapped_wire: Vec<(u32, String)>,
}

/// O(1) id translation between two versions of one registry, matched by
/// resource name — correct under renumbering, additions and removals.
///
/// `canonical` is the side mc-spine reasons in (normally the builtin table);
/// `wire` is the negotiated side. `to_wire` is indexed by canonical id,
/// `from_wire` by wire id; both are dense `Vec`s so a lookup is one bounds
/// check.
#[derive(Clone, Debug)]
pub struct IdRemapper {
    to_wire: Vec<Option<u32>>,
    from_wire: Vec<Option<u32>>,
    canonical_names: HashMap<String, u32>,
    wire_names: HashMap<String, u32>,
    report: RemapReport,
}

impl IdRemapper {
    /// Match `canonical` and `wire` on resource name and precompute both
    /// translation directions plus the coverage report.
    pub fn build(canonical: &Registry, wire: &Registry) -> Self {
        let mut to_wire = vec![None; canonical.len() as usize];
        let mut from_wire = vec![None; wire.len() as usize];
        let mut report = RemapReport::default();

        for (id, name) in canonical.entries() {
            match wire.id(name) {
                Some(wid) => {
                    to_wire[id as usize] = Some(wid);
                    report.mapped_count += 1;
                }
                None => report.unmapped_canonical.push((id, name.to_owned())),
            }
        }
        for (wid, name) in wire.entries() {
            match canonical.id(name) {
                Some(id) => from_wire[wid as usize] = Some(id),
                None => report.unmapped_wire.push((wid, name.to_owned())),
            }
        }
        Self {
            to_wire,
            from_wire,
            canonical_names: canonical
                .entries()
                .map(|(i, n)| (n.to_owned(), i))
                .collect(),
            wire_names: wire.entries().map(|(i, n)| (n.to_owned(), i)).collect(),
            report,
        }
    }

    /// Canonical id -> wire id (`None` = absent on the wire version).
    pub fn to_wire(&self, canonical_id: u32) -> Option<u32> {
        self.to_wire.get(canonical_id as usize).copied().flatten()
    }

    /// Wire id -> canonical id (`None` = no canonical meaning).
    pub fn from_wire(&self, wire_id: u32) -> Option<u32> {
        self.from_wire.get(wire_id as usize).copied().flatten()
    }

    /// Named variant of [`IdRemapper::to_wire`]: look the name up in the
    /// canonical registry, then translate its id.
    pub fn to_wire_named(&self, canonical_name: &str) -> Option<u32> {
        let id = self.canonical_names.get(canonical_name)?;
        self.to_wire(*id)
    }

    /// Named variant of [`IdRemapper::from_wire`].
    pub fn from_wire_named(&self, wire_name: &str) -> Option<u32> {
        let id = self.wire_names.get(wire_name)?;
        self.from_wire(*id)
    }

    /// Coverage details computed at build time.
    pub fn report(&self) -> &RemapReport {
        &self.report
    }
}

// ---------------------------------------------------------------------------
// Minimal JSON for version packs
//
// Version packs are plain data files, so this is a deliberately small,
// fail-closed parser: objects, arrays, strings, booleans, null and *integer*
// numbers only (DataGenerator reports never emit fractions; accepting floats
// would blur "id" fields). Duplicate object keys and nesting beyond a fixed
// depth are rejected, and the document must consume the whole input.
// ---------------------------------------------------------------------------

#[derive(Debug, PartialEq)]
enum Json {
    Null,
    Bool(bool),
    Int(i64),
    String(String),
    Array(Vec<Json>),
    Object(Vec<(String, Json)>),
}

const MAX_JSON_DEPTH: usize = 64;

struct JsonCursor<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> JsonCursor<'a> {
    fn parse(input: &'a str) -> Result<Json, ProtoError> {
        let mut cur = Self {
            bytes: input.as_bytes(),
            pos: 0,
        };
        cur.ws();
        let value = cur.value(0)?;
        cur.ws();
        if cur.pos != cur.bytes.len() {
            return Err(ProtoError::VersionPack("trailing data after JSON document"));
        }
        Ok(value)
    }

    fn ws(&mut self) {
        while matches!(self.bytes.get(self.pos), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.pos += 1;
        }
    }

    fn value(&mut self, depth: usize) -> Result<Json, ProtoError> {
        if depth > MAX_JSON_DEPTH {
            return Err(ProtoError::VersionPack("JSON nesting too deep"));
        }
        match self.bytes.get(self.pos) {
            Some(b'{') => self.object(depth),
            Some(b'[') => self.array(depth),
            Some(b'"') => self.string().map(Json::String),
            Some(b't') => self.literal(b"true", Json::Bool(true)),
            Some(b'f') => self.literal(b"false", Json::Bool(false)),
            Some(b'n') => self.literal(b"null", Json::Null),
            Some(b'-' | b'0'..=b'9') => self.number(),
            _ => Err(ProtoError::VersionPack("unexpected JSON token")),
        }
    }

    fn literal(&mut self, lit: &[u8], value: Json) -> Result<Json, ProtoError> {
        if self.bytes[self.pos..].starts_with(lit) {
            self.pos += lit.len();
            Ok(value)
        } else {
            Err(ProtoError::VersionPack("malformed JSON literal"))
        }
    }

    fn number(&mut self) -> Result<Json, ProtoError> {
        let start = self.pos;
        if self.bytes.get(self.pos) == Some(&b'-') {
            self.pos += 1;
        }
        let int_start = self.pos;
        while matches!(self.bytes.get(self.pos), Some(b'0'..=b'9')) {
            self.pos += 1;
        }
        if self.pos == int_start {
            return Err(ProtoError::VersionPack("malformed JSON number"));
        }
        if matches!(self.bytes.get(self.pos), Some(b'.' | b'e' | b'E')) {
            return Err(ProtoError::VersionPack(
                "non-integer numbers are not valid in version packs",
            ));
        }
        let text = std::str::from_utf8(&self.bytes[start..self.pos])
            .map_err(|_| ProtoError::VersionPack("non-UTF8 number"))?;
        text.parse::<i64>()
            .map(Json::Int)
            .map_err(|_| ProtoError::VersionPack("number out of i64 range"))
    }

    fn hex4(&mut self) -> Result<u32, ProtoError> {
        let mut v = 0u32;
        for _ in 0..4 {
            let Some(&b) = self.bytes.get(self.pos) else {
                return Err(ProtoError::VersionPack("truncated \\u escape"));
            };
            self.pos += 1;
            v = (v << 4)
                | match b {
                    b'0'..=b'9' => u32::from(b - b'0'),
                    b'a'..=b'f' => u32::from(b - b'a' + 10),
                    b'A'..=b'F' => u32::from(b - b'A' + 10),
                    _ => return Err(ProtoError::VersionPack("bad \\u escape digit")),
                };
        }
        Ok(v)
    }

    fn string(&mut self) -> Result<String, ProtoError> {
        self.pos += 1; // opening quote
        let mut out = String::new();
        loop {
            let Some(&b) = self.bytes.get(self.pos) else {
                return Err(ProtoError::VersionPack("unterminated JSON string"));
            };
            self.pos += 1;
            match b {
                b'"' => return Ok(out),
                b'\\' => {
                    let Some(&esc) = self.bytes.get(self.pos) else {
                        return Err(ProtoError::VersionPack("truncated escape"));
                    };
                    self.pos += 1;
                    match esc {
                        b'"' => out.push('"'),
                        b'\\' => out.push('\\'),
                        b'/' => out.push('/'),
                        b'b' => out.push('\u{8}'),
                        b'f' => out.push('\u{c}'),
                        b'n' => out.push('\n'),
                        b'r' => out.push('\r'),
                        b't' => out.push('\t'),
                        b'u' => {
                            let hi = self.hex4()?;
                            let c = if (0xd800..0xdc00).contains(&hi) {
                                if self.bytes.get(self.pos) == Some(&b'\\')
                                    && self.bytes.get(self.pos + 1) == Some(&b'u')
                                {
                                    self.pos += 2;
                                    let lo = self.hex4()?;
                                    if !(0xdc00..0xe000).contains(&lo) {
                                        return Err(ProtoError::VersionPack(
                                            "lone high surrogate in string",
                                        ));
                                    }
                                    let c = 0x1_0000
                                        + (((hi - 0xd800) << 10) | (lo - 0xdc00));
                                    char::from_u32(c).ok_or(ProtoError::VersionPack(
                                        "bad surrogate pair",
                                    ))?
                                } else {
                                    return Err(ProtoError::VersionPack(
                                        "lone high surrogate in string",
                                    ));
                                }
                            } else {
                                char::from_u32(hi).ok_or(ProtoError::VersionPack(
                                    "bad unicode escape",
                                ))?
                            };
                            out.push(c);
                        }
                        _ => return Err(ProtoError::VersionPack("bad string escape")),
                    }
                }
                0x00..=0x1f => {
                    return Err(ProtoError::VersionPack("raw control character in string"))
                }
                _ => {
                    // Copy the full UTF-8 sequence verbatim — validation
                    // happens once via from_utf8 at the end.
                    let extra = match b {
                        0x20..=0x7f => 0,
                        0xc0..=0xdf => 1,
                        0xe0..=0xef => 2,
                        0xf0..=0xf7 => 3,
                        _ => return Err(ProtoError::VersionPack("invalid UTF-8 in string")),
                    };
                    let start = self.pos - 1;
                    let end = start + 1 + extra;
                    if end > self.bytes.len() {
                        return Err(ProtoError::VersionPack("truncated UTF-8 in string"));
                    }
                    let text = std::str::from_utf8(&self.bytes[start..end])
                        .map_err(|_| ProtoError::VersionPack("invalid UTF-8 in string"))?;
                    out.push_str(text);
                    self.pos = end;
                }
            }
        }
    }

    fn array(&mut self, depth: usize) -> Result<Json, ProtoError> {
        self.pos += 1; // '['
        let mut items = Vec::new();
        self.ws();
        if self.bytes.get(self.pos) == Some(&b']') {
            self.pos += 1;
            return Ok(Json::Array(items));
        }
        loop {
            self.ws();
            items.push(self.value(depth + 1)?);
            self.ws();
            match self.bytes.get(self.pos) {
                Some(b',') => self.pos += 1,
                Some(b']') => {
                    self.pos += 1;
                    return Ok(Json::Array(items));
                }
                _ => return Err(ProtoError::VersionPack("malformed JSON array")),
            }
        }
    }

    fn object(&mut self, depth: usize) -> Result<Json, ProtoError> {
        self.pos += 1; // '{'
        let mut entries = Vec::new();
        self.ws();
        if self.bytes.get(self.pos) == Some(&b'}') {
            self.pos += 1;
            return Ok(Json::Object(entries));
        }
        loop {
            self.ws();
            if self.bytes.get(self.pos) != Some(&b'"') {
                return Err(ProtoError::VersionPack("JSON object keys must be strings"));
            }
            let key = self.string()?;
            self.ws();
            if self.bytes.get(self.pos) != Some(&b':') {
                return Err(ProtoError::VersionPack("missing ':' in JSON object"));
            }
            self.pos += 1;
            self.ws();
            let value = self.value(depth + 1)?;
            if entries.iter().any(|(k, _)| k == &key) {
                return Err(ProtoError::DuplicateEntry("JSON object repeats a key"));
            }
            entries.push((key, value));
            self.ws();
            match self.bytes.get(self.pos) {
                Some(b',') => self.pos += 1,
                Some(b'}') => {
                    self.pos += 1;
                    return Ok(Json::Object(entries));
                }
                _ => return Err(ProtoError::VersionPack("malformed JSON object")),
            }
        }
    }
}

fn json_parse(input: &str) -> Result<Json, ProtoError> {
    JsonCursor::parse(input)
}

fn obj_get<'a>(map: &'a [(String, Json)], key: &str) -> Option<&'a Json> {
    map.iter().find(|(k, _)| k == key).map(|(_, v)| v)
}

fn object<'a>(
    value: &'a Json,
    what: &'static str,
) -> Result<&'a Vec<(String, Json)>, ProtoError> {
    match value {
        Json::Object(map) => Ok(map),
        _ => Err(ProtoError::VersionPack(what)),
    }
}

fn int(value: &Json, _what: &'static str) -> Result<i64, ProtoError> {
    match value {
        Json::Int(n) => Ok(*n),
        _ => Err(ProtoError::VersionPack("expected an integer id")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // -- builtin table agreement with mc-spine -------------------------------

    #[test]
    fn v767_packet_ids_match_server_constants() {
        let t = ProtocolTable::v767_builtin();
        let s2c = |name| t.packet_id(PacketPhase::Play, PacketDir::Clientbound, name);
        // The exact numeric literals from src/play_protocol.rs and siblings.
        assert_eq!(s2c("keep_alive"), Some(0x26));
        assert_eq!(s2c("open_screen"), Some(0x33));
        assert_eq!(s2c("set_carried_item"), Some(0x53));
        assert_eq!(s2c("set_entity_data"), Some(0x58));
        assert_eq!(s2c("set_equipment"), Some(0x5b));
        assert_eq!(s2c("set_player_team"), Some(0x60));
        assert_eq!(s2c("update_attributes"), Some(0x75));
        assert_eq!(s2c("tag_query"), Some(0x6e));
        assert_eq!(s2c("stop_sound"), Some(0x6a));
        assert_eq!(s2c("take_item_entity"), Some(0x6f));
        assert_eq!(s2c("set_score"), Some(0x61));
        assert_eq!(s2c("set_titles_animation"), Some(0x66));
        assert_eq!(s2c("ticking_state"), Some(0x71));
        assert_eq!(s2c("ticking_step"), Some(0x72));
        assert_eq!(s2c("tab_list"), Some(0x6d));
        assert_eq!(s2c("container_set_slot"), Some(0x15));
        assert_eq!(s2c("resource_pack_pop"), Some(0x45));
        assert_eq!(s2c("place_ghost_recipe"), Some(0x37));
        let c2s = |name| t.packet_id(PacketPhase::Play, PacketDir::Serverbound, name);
        assert_eq!(c2s("accept_teleportation"), Some(0x00));
        assert_eq!(c2s("keep_alive"), Some(0x18));
        assert_eq!(c2s("player_input"), Some(0x26));
        assert_eq!(c2s("move_vehicle"), Some(0x1e));
        assert_eq!(c2s("paddle_boat"), Some(0x1f));
        assert_eq!(c2s("use_item_on"), Some(0x38));
        assert_eq!(c2s("use_item"), Some(0x39));
        // Other phases.
        assert_eq!(
            t.packet_id(PacketPhase::Handshake, PacketDir::Serverbound, "handshake"),
            Some(0x00)
        );
        assert_eq!(
            t.packet_id(PacketPhase::Login, PacketDir::Clientbound, "encryption_request"),
            Some(0x01)
        );
        assert_eq!(
            t.packet_id(PacketPhase::Login, PacketDir::Clientbound, "login_success"),
            Some(0x02)
        );
        assert_eq!(
            t.packet_id(
                PacketPhase::Configuration,
                PacketDir::Clientbound,
                "registry_data"
            ),
            Some(0x07)
        );
        assert_eq!(
            t.packet_id(PacketPhase::Login, PacketDir::Clientbound, "set_compression"),
            Some(0x03)
        );
        // Full-table sizes pin the whole 767 surface.
        assert_eq!(
            t.packet_registry(PacketPhase::Play, PacketDir::Clientbound)
                .unwrap()
                .len(),
            0x7c
        );
        assert_eq!(
            t.packet_registry(PacketPhase::Play, PacketDir::Serverbound)
                .unwrap()
                .len(),
            0x3a
        );
    }

    #[test]
    fn v767_menu_and_component_ids_match() {
        let t = ProtocolTable::v767_builtin();
        // Menu ids are the literals used in src/play_protocol.rs OpenWindow.
        for (name, id) in [
            ("minecraft:anvil", 8),
            ("minecraft:brewing_stand", 11),
            ("minecraft:crafter_3x3", 7),
            ("minecraft:lectern", 17),
            ("minecraft:enchantment", 13),
            ("minecraft:merchant", 19),
            ("minecraft:smithing", 21),
            ("minecraft:generic_9x3", 2),
            ("minecraft:generic_9x6", 5),
            ("minecraft:furnace", 14),
            ("minecraft:grindstone", 15),
        ] {
            assert_eq!(t.registry_id("minecraft:menu", name), Some(id), "{name}");
        }
        // Component ids are the literals in menus_and_items.rs DataComponent.
        for (name, id) in [
            ("minecraft:custom_data", 0),
            ("minecraft:max_stack_size", 1),
            ("minecraft:damage", 3),
            ("minecraft:unbreakable", 4),
            ("minecraft:custom_name", 5),
            ("minecraft:item_name", 6),
            ("minecraft:lore", 7),
            ("minecraft:enchantments", 9),
            ("minecraft:repair_cost", 16),
            ("minecraft:stored_enchantments", 23),
            ("minecraft:map_id", 26),
            ("minecraft:potion_contents", 31),
            ("minecraft:trim", 35),
            ("minecraft:container_loot", 56),
        ] {
            assert_eq!(
                t.registry_id("minecraft:data_component_type", name),
                Some(id),
                "{name}"
            );
        }
        // Core registry sizes pin the tables.
        assert_eq!(t.registry("minecraft:item").unwrap().len(), 1333);
        assert_eq!(t.registry("minecraft:block").unwrap().len(), 1060);
        assert_eq!(t.registry("minecraft:entity_type").unwrap().len(), 130);
        assert_eq!(t.registry_id("minecraft:item", "minecraft:stone"), Some(1));
        assert_eq!(t.registry_id("minecraft:item", "minecraft:air"), Some(0));
        assert_eq!(
            t.registry_id("minecraft:entity_type", "minecraft:allay"),
            Some(0)
        );
        assert_eq!(t.registry_id("minecraft:block", "minecraft:air"), Some(0));
    }

    #[test]
    fn v767_reverse_lookup() {
        let t = ProtocolTable::v767_builtin();
        assert_eq!(
            t.packet_name(PacketPhase::Play, PacketDir::Clientbound, 0x26),
            Some("keep_alive")
        );
        assert_eq!(t.registry_name("minecraft:item", 1), Some("minecraft:stone"));
        // Out-of-range and unbound ids fail closed, not panic.
        assert!(t
            .packet_name(PacketPhase::Play, PacketDir::Clientbound, 0x7c)
            .is_none());
        assert!(t.registry_name("minecraft:item", 1333).is_none());
    }

    // -- version-pack JSON loader --------------------------------------------

    fn pack_json_1211() -> (String, String) {
        let packets = r#"{
            "protocol_version": 767,
            "phases": {
                "handshake": {"serverbound": {"handshake": 0}},
                "play": {
                    "clientbound": {"keep_alive": 38, "open_screen": 51},
                    "serverbound": {"keep_alive": 24}
                }
            }
        }"#;
        let registries = r#"{
            "registries": {
                "minecraft:item": {"minecraft:air": 0, "minecraft:stone": 1, "minecraft:dirt": 2},
                "minecraft:menu": ["minecraft:generic_9x1", "minecraft:generic_9x2", "minecraft:anvil"]
            }
        }"#;
        (packets.to_owned(), registries.to_owned())
    }

    fn pack_json_1214() -> (String, String) {
        // Synthetic 769 pack: keep_alive renumbered, open_screen removed,
        // anvil renumbered and a new menu type added.
        let packets = r#"{
            "protocol_version": 769,
            "phases": {
                "handshake": {"serverbound": {"handshake": 0}},
                "play": {
                    "clientbound": {"keep_alive": 39, "new_packet": 120},
                    "serverbound": {"keep_alive": 25}
                }
            }
        }"#;
        let registries = r#"{
            "registries": {
                "minecraft:item": {"minecraft:air": 0, "minecraft:stone": 1, "minecraft:dirt": 2},
                "minecraft:menu": {"minecraft:generic_9x1": 0, "minecraft:generic_9x2": 1,
                                   "minecraft:crafter_3x3": 2, "minecraft:anvil": 3}
            }
        }"#;
        (packets.to_owned(), registries.to_owned())
    }

    #[test]
    fn version_pack_loads_and_remaps() {
        let (p767, r767) = pack_json_1211();
        let t767 = ProtocolTable::from_version_pack_json(&p767, &r767).unwrap();
        assert_eq!(t767.protocol_version(), Some(767));
        assert_eq!(
            t767.packet_id(PacketPhase::Play, PacketDir::Clientbound, "keep_alive"),
            Some(38)
        );
        // Array form assigns dense ids.
        assert_eq!(
            t767.registry_id("minecraft:menu", "minecraft:anvil"),
            Some(2)
        );

        let (p769, r769) = pack_json_1214();
        let t769 = ProtocolTable::from_version_pack_json(&p769, &r769).unwrap();
        assert_eq!(t769.protocol_version(), Some(769));

        // Renumbered keep_alive remaps; open_screen has no 1.21.4 counterpart.
        let ka = IdRemapper::build(
            t767.packet_registry(PacketPhase::Play, PacketDir::Clientbound)
                .unwrap(),
            t769.packet_registry(PacketPhase::Play, PacketDir::Clientbound)
                .unwrap(),
        );
        assert_eq!(ka.to_wire(38), Some(39));
        assert_eq!(ka.from_wire(39), Some(38));
        assert!(ka.to_wire_named("open_screen").is_none());
        assert!(ka.from_wire(120).is_none());
        let report = ka.report();
        assert_eq!(report.mapped_count, 1);
        assert_eq!(
            report.unmapped_canonical.iter().map(|e| e.1.as_str()).collect::<Vec<_>>(),
            ["open_screen"]
        );
        assert_eq!(
            report.unmapped_wire.iter().map(|e| e.1.as_str()).collect::<Vec<_>>(),
            ["new_packet"]
        );

        // Registry remap across renumbering + addition.
        let menus = IdRemapper::build(
            t767.registry("minecraft:menu").unwrap(),
            t769.registry("minecraft:menu").unwrap(),
        );
        assert_eq!(menus.to_wire_named("minecraft:anvil"), Some(3));
        assert_eq!(menus.from_wire_named("minecraft:anvil"), Some(2));
        // Canonical id 2 (anvil) maps to wire id 3; wire id 2 is crafter_3x3,
        // which the canonical pack does not have.
        assert_eq!(menus.to_wire(2), Some(3));
        assert_eq!(menus.from_wire(3), Some(2));
        assert!(menus.from_wire(2).is_none());
        assert!(menus.from_wire_named("minecraft:crafter_3x3").is_none());
    }

    #[test]
    fn version_pack_fail_closed() {
        let (p767, r767) = pack_json_1211();
        // Invalid JSON.
        assert!(ProtocolTable::from_version_pack_json("{oops", &r767).is_err());
        assert!(ProtocolTable::from_version_pack_json(&p767, "[1,2]").is_err());
        // Duplicate packet id within one (phase, dir).
        let dup = r#"{"phases":{"play":{"clientbound":{"a":1,"b":1}}}}"#;
        assert!(matches!(
            ProtocolTable::from_version_pack_json(dup, &r767),
            Err(ProtoError::DuplicateEntry(_))
        ));
        // Duplicate JSON key.
        let dup_key = r#"{"phases":{"play":{"clientbound":{"a":1,"a":2}}}}"#;
        assert!(matches!(
            ProtocolTable::from_version_pack_json(dup_key, &r767),
            Err(ProtoError::DuplicateEntry(_))
        ));
        // Negative / out-of-range packet id.
        let neg = r#"{"phases":{"play":{"clientbound":{"a":-1}}}}"#;
        assert!(ProtocolTable::from_version_pack_json(neg, &r767).is_err());
        let big = r#"{"phases":{"play":{"clientbound":{"a":300}}}}"#;
        assert!(ProtocolTable::from_version_pack_json(big, &r767).is_err());
        // Unknown phase / direction.
        let bad_phase = r#"{"phases":{"play2":{"clientbound":{"a":0}}}}"#;
        assert!(ProtocolTable::from_version_pack_json(bad_phase, &r767).is_err());
        let bad_dir = r#"{"phases":{"play":{"sideways":{"a":0}}}}"#;
        assert!(ProtocolTable::from_version_pack_json(bad_dir, &r767).is_err());
        // Duplicate registry id.
        let dup_reg = r#"{"registries":{"minecraft:item":{"a":0,"b":0}}}"#;
        assert!(ProtocolTable::from_version_pack_json(&p767, dup_reg).is_err());
        // Non-integer id.
        let frac = r#"{"phases":{"play":{"clientbound":{"a":1.5}}}}"#;
        assert!(ProtocolTable::from_version_pack_json(frac, &r767).is_err());
        // Trailing garbage.
        assert!(ProtocolTable::from_version_pack_json(
            &format!("{p767} trailing"),
            &r767
        )
        .is_err());
        // Missing required sections.
        assert!(ProtocolTable::from_version_pack_json("{}", &r767).is_err());
        assert!(ProtocolTable::from_version_pack_json(&p767, "{}").is_err());
    }

    #[test]
    fn remapper_is_o1_dense_lookup() {
        let from = Registry::from_dense_names(&["a", "b", "c"]);
        let mut to = Registry::new();
        to.insert("b", 10).unwrap();
        to.insert("c", 42).unwrap();
        let r = IdRemapper::build(&from, &to);
        assert_eq!(r.to_wire(0), None);
        assert_eq!(r.to_wire(1), Some(10));
        assert_eq!(r.to_wire(2), Some(42));
        assert_eq!(r.from_wire(10), Some(1));
        assert_eq!(r.from_wire(42), Some(2));
        assert_eq!(r.from_wire(9999), None);
        assert_eq!(r.report().mapped_count, 2);
        assert_eq!(r.report().unmapped_canonical, vec![(0, "a".to_string())]);
    }
}
