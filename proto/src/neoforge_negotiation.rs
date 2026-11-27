// SPDX-License-Identifier: LGPL-3.0-or-later
//! Clean-room **NeoForge 21.1.248** and **Fabric API 0.116.15** loader protocol:
//! connection negotiation, frozen-registry sync, oversized-packet splitting and
//! the Fabric direct registry sync / extended-screen payloads.
//!
//! Everything here is derived from the shipped JARs with `javap -c` (packet
//! `ID` constants, static codec initialisers, `write*`/`read*` call order) and
//! from the vanilla 1.21.1 stream codecs those codecs delegate to. No mod source
//! was consulted or copied; the JARs are only needed while verifying, and the
//! tests never read them.
//!
//! # Verification basis
//!
//! * NeoForge payload ids, field order and count/string widths were read from
//!   `neoforge-21.1.248-universal.jar`
//!   (`net.neoforged.neoforge.network.payload.*`, `...registries.RegistrySnapshot`,
//!   `...filters.GenericPacketSplitter`).
//! * [`ConnectionProtocol`] ids/names and [`PacketFlow`] ordinals were read from
//!   vanilla `client-1.21.1-srg.jar`
//!   (`net/minecraft/network/ConnectionProtocol`, `.../protocol/PacketFlow`).
//! * The Fabric channel names are the Fabric API 0.116.x registrations. The
//!   Forgified/Sinytra port of that API in this pack ships **no** registry-sync
//!   networking classes at all (it defers registry sync to NeoForge), so the
//!   `fabric:registry/sync/*` and `fabric-screen-handler-api-v1:open_screen`
//!   layouts below follow the documented `DirectRegistryPacketHandler` field
//!   order rather than a machine-checked class. They are megu's contract for
//!   genuine Fabric peers, versioned by [`FABRIC_API_VERSION`].
//!
//! # Channels
//!
//! | channel | direction | body |
//! | --- | --- | --- |
//! | `c:version` | login/config | [`CommonVersionPayload`] (`VarInt` list) |
//! | `c:register` | config | [`CommonRegisterPayload`] |
//! | `neoforge:register` | serverbound config | [`ModdedNetworkQueryPayload`] |
//! | `neoforge:network` | clientbound config | [`ModdedNetworkPayload`] |
//! | `neoforge:modded_network_setup_failed` | clientbound config | [`ModdedNetworkSetupFailedPayload`] |
//! | `neoforge:frozen_registry` | clientbound config | [`FrozenRegistryPayload`] |
//! | `neoforge:split` | both | [`SplitPacketReassembler`] framing |
//! | `neoforge:config_file` | serverbound | [`ConfigFilePayload`] |
//! | `fabric:registry/sync/direct` | clientbound play | [`FabricDirectRegistryPayload`] |
//! | `fabric-screen-handler-api-v1:open_screen` | clientbound play | [`FabricOpenScreenPayload`] |
//!
//! # Negotiation
//!
//! [`NeoForgeNegotiation`] is the stateful version of NeoForge's
//! `NetworkRegistry` handshake: `c:version` fixes the common networking version,
//! the client's `neoforge:register` query is matched against the local payload
//! table, and the outcome is either a [`ModdedNetworkPayload`] channel setup or
//! a [`ModdedNetworkSetupFailedPayload`] carrying one plain-text reason per
//! namespace. A version mismatch on a *required* channel fails the connection;
//! the same mismatch on an *optional* channel only drops that channel.

#![deny(unsafe_code)]

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use crate::channel_router::MAX_REGISTERED_CHANNELS_PER_PEER;
use crate::wire::{ProtoError, WireReader, WireWriter, MAX_STRING_CHARS};

/// NeoForge 21.1.248, as declared by the shipped universal JAR.
pub const NEOFORGE_VERSION: &str = "21.1.248";
/// Fabric API version the `fabric:*` layouts below are pinned to.
pub const FABRIC_API_VERSION: &str = "0.116.15";

pub const COMMON_VERSION_CHANNEL: &str = "c:version";
pub const COMMON_REGISTER_CHANNEL: &str = "c:register";
pub const NEOFORGE_REGISTER_CHANNEL: &str = "neoforge:register";
pub const NEOFORGE_NETWORK_CHANNEL: &str = "neoforge:network";
pub const NEOFORGE_SETUP_FAILED_CHANNEL: &str = "neoforge:modded_network_setup_failed";
pub const NEOFORGE_SPLIT_CHANNEL: &str = "neoforge:split";
pub const NEOFORGE_CONFIG_FILE_CHANNEL: &str = "neoforge:config_file";
pub const NEOFORGE_FROZEN_REGISTRY_CHANNEL: &str = "neoforge:frozen_registry";
pub const NEOFORGE_FROZEN_REGISTRY_SYNC_START_CHANNEL: &str = "neoforge:frozen_registry_sync_start";
pub const NEOFORGE_FROZEN_REGISTRY_SYNC_COMPLETED_CHANNEL: &str =
    "neoforge:frozen_registry_sync_completed";
pub const FABRIC_REGISTER_CHANNEL: &str = "fabric:register";
pub const FABRIC_UNREGISTER_CHANNEL: &str = "fabric:unregister";
pub const FABRIC_REGISTRY_SYNC_DIRECT_CHANNEL: &str = "fabric:registry/sync/direct";
pub const FABRIC_REGISTRY_SYNC_COMPLETE_CHANNEL: &str = "fabric:registry/sync/complete";
pub const FABRIC_OPEN_SCREEN_CHANNEL: &str = "fabric-screen-handler-api-v1:open_screen";

/// Every loader-protocol channel this crate recognizes, in the order the
/// upstream loaders announce them. [`crate::channel_router::classify_channel`]
/// routes all of them (as
/// [`crate::channel_router::ChannelHandlerKind::LoaderProtocolChannel`) instead
/// of dropping them as unknown mod payloads.
///
/// `minecraft:register` and `minecraft:unregister` are part of the loader
/// handshake on both loaders, but the router keeps their dedicated
/// [`crate::channel_router::ChannelHandlerKind::Register`] /
/// [`crate::channel_router::ChannelHandlerKind::Unregister`] kinds because those
/// two channels mutate the per-peer channel set.
pub const LOADER_PROTOCOL_CHANNELS: &[&str] = &[
    // Vanilla + common loader negotiation.
    "minecraft:register",
    "minecraft:unregister",
    COMMON_VERSION_CHANNEL,
    COMMON_REGISTER_CHANNEL,
    // NeoForge 21.1.248.
    NEOFORGE_REGISTER_CHANNEL,
    NEOFORGE_NETWORK_CHANNEL,
    NEOFORGE_SETUP_FAILED_CHANNEL,
    NEOFORGE_SPLIT_CHANNEL,
    NEOFORGE_CONFIG_FILE_CHANNEL,
    NEOFORGE_FROZEN_REGISTRY_SYNC_START_CHANNEL,
    NEOFORGE_FROZEN_REGISTRY_CHANNEL,
    NEOFORGE_FROZEN_REGISTRY_SYNC_COMPLETED_CHANNEL,
    "neoforge:known_registry_data_maps",
    "neoforge:known_registry_data_maps_reply",
    "neoforge:extensible_enum_data",
    "neoforge:extensible_enum_ack",
    "neoforge:feature_flags",
    "neoforge:feature_flags_ack",
    "neoforge:advanced_add_entity",
    "neoforge:advanced_open_screen",
    "neoforge:advanced_container_set_data",
    "neoforge:auxiliary_light_data",
    "neoforge:registry_data_map_sync",
    "neoforge:sync_attachments",
    "neoforge:custom_time_packet",
    // Fabric API 0.116.15 (upstream and the Forgified/Sinytra port).
    FABRIC_REGISTER_CHANNEL,
    FABRIC_UNREGISTER_CHANNEL,
    FABRIC_REGISTRY_SYNC_DIRECT_CHANNEL,
    FABRIC_REGISTRY_SYNC_COMPLETE_CHANNEL,
    "fabric:attachment_sync_v1",
    "fabric:accepted_attachments_v1",
    FABRIC_OPEN_SCREEN_CHANNEL,
];

/// `true` when `channel` belongs to the NeoForge/Fabric loader protocol rather
/// than to a mod's own gameplay payloads.
pub fn is_loader_protocol_channel(channel: &str) -> bool {
    LOADER_PROTOCOL_CHANNELS.contains(&channel)
}

/// `c:version` list entries NeoForge accepts from one payload.
pub const SUPPORTED_COMMON_NETWORKING_VERSIONS: [i32; 1] = [1];
/// Bound on the `c:version` list, wide enough for every future NeoForge line.
pub const MAX_COMMON_VERSION_ENTRIES: usize = 64;
/// The `version` field NeoForge 21.1.248 writes into `c:register`.
pub const COMMON_REGISTER_PAYLOAD_VERSION: i32 = 1;

/// Bound on a full `namespace:path` name; vanilla has no hard cap, so megu
/// bounds it instead of trusting a forged length.
pub const MAX_RESOURCE_LOCATION_BYTES: usize = 256;
/// Bound on protocol groups in one negotiation payload.
pub const MAX_NEGOTIATION_GROUPS: usize = 16;
/// Bound on registered registry snapshots (items alone reach ~2^14 in packs).
pub const MAX_REGISTRY_SNAPSHOT_ENTRIES: usize = 262_144;
/// Bound on a whole `neoforge:frozen_registry` body, matching the splitter's
/// uncompressed ceiling.
pub const MAX_FROZEN_REGISTRY_BYTES: usize = 8 * 1024 * 1024;
/// Bound on one `neoforge:config_file` body.
pub const MAX_CONFIG_FILE_BYTES: usize = 8 * 1024 * 1024;
/// Bound on a config file name, in UTF-16 units like vanilla strings.
pub const MAX_CONFIG_FILE_NAME_CHARS: usize = 512;
/// Bound on failed namespaces reported in one setup-failure payload.
pub const MAX_NEGOTIATION_FAILURES: usize = 64;
/// Bound on one failure reason NBT blob.
pub const MAX_FAILURE_REASON_NBT_BYTES: usize = 8192;

/// `GenericPacketSplitter.SizeLimits(2097152)` — the ceiling used while the
/// connection still compresses.
pub const SPLIT_COMPRESSED_PACKET_LIMIT: usize = 2 * 1024 * 1024;
/// `GenericPacketSplitter.SizeLimits(8388608)` — the ceiling once compression is
/// negotiated.
pub const SPLIT_UNCOMPRESSED_PACKET_LIMIT: usize = 8 * 1024 * 1024;
/// `neoforge:split` part states, in `GenericPacketSplitter` order.
pub const SPLIT_STATE_MIDDLE: u8 = 0;
pub const SPLIT_STATE_FIRST: u8 = 1;
pub const SPLIT_STATE_LAST: u8 = 2;

/// `DirectRegistryPacketHandler.MAX_PAYLOAD_SIZE` — one sync chunk carries at
/// most 1 MiB, and a zero-length payload terminates the stream.
pub const FABRIC_REGISTRY_SYNC_CHUNK_BYTES: usize = 0x10_0000;
/// Bound on one reassembled `fabric:registry/sync/direct` body.
pub const MAX_FABRIC_REGISTRY_SYNC_BYTES: usize = 16 * 1024 * 1024;
/// Bound on registry namespace groups and per-registry group counts.
pub const MAX_FABRIC_REGISTRY_GROUPS: usize = 4096;
/// Bound on entries in one Fabric registry bulk.
pub const MAX_FABRIC_REGISTRY_BULK_ENTRIES: usize = 65_536;
/// Bound on the title NBT of an extended-screen open payload.
pub const MAX_OPEN_SCREEN_TITLE_NBT_BYTES: usize = 262_144;
/// Bound on the extra data of an extended-screen open payload.
pub const MAX_OPEN_SCREEN_EXTRA_DATA_BYTES: usize = 1_048_576;

/// Every failure a loader-protocol codec can report. Framing failures keep the
/// [`ProtoError`] they came from so a rejection stays explainable.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LoaderCodecError {
    /// Wire-level failure from [`crate::wire`].
    Wire(ProtoError),
    /// Bytes remained after a fully decoded payload.
    TrailingBytes,
    /// A name that must be a canonical `namespace:path` resource location was not.
    InvalidResourceLocation(String),
    /// `c:register` named a `ConnectionProtocol` vanilla does not declare.
    UnknownCommonProtocol(String),
    /// A protocol id outside [`ConnectionProtocol::ALL`].
    UnknownConnectionProtocol(i32),
    /// A `PacketFlow` ordinal outside `0..=1`.
    UnknownPacketFlow(i32),
    /// The payload repeated a key it may only carry once.
    DuplicateEntry(&'static str),
    /// A count exceeded the bound for that field.
    TooManyEntries(&'static str),
    /// A length-prefixed body exceeded its bound.
    PayloadTooLarge(&'static str),
    /// A Fabric bulk's raw ids did not advance past the previous bulk.
    UnorderedBulk(&'static str),
    /// A `fabric:registry/sync/direct` stream ended before any chunk arrived.
    EmptyRegistrySync,
    /// A `neoforge:split` part carried a state byte other than `0..=2`.
    SplitInvalidState(u8),
    /// A `neoforge:split` continuation arrived before its first part.
    SplitNotStarted,
    /// A `neoforge:split` part had no state byte at all.
    SplitEmptyPayload,
    /// Reassembling the parts would exceed the configured ceiling.
    SplitTooLarge { buffered: usize, max: usize },
    /// No common networking version is shared with the peer.
    UnsupportedCommonVersion,
    /// A plain-text failure reason was not ASCII, so NBT encoding would differ.
    UnencodableReason,
}

impl fmt::Display for LoaderCodecError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Wire(error) => write!(f, "loader payload wire error: {error}"),
            Self::TrailingBytes => f.write_str("trailing bytes after loader payload"),
            Self::InvalidResourceLocation(name) => write!(f, "invalid resource location {name:?}"),
            Self::UnknownCommonProtocol(name) => {
                write!(f, "unknown connection protocol {name:?}")
            }
            Self::UnknownConnectionProtocol(id) => {
                write!(f, "unknown connection protocol id {id}")
            }
            Self::UnknownPacketFlow(ordinal) => write!(f, "unknown packet flow ordinal {ordinal}"),
            Self::DuplicateEntry(what) => write!(f, "duplicate {what}"),
            Self::TooManyEntries(what) => write!(f, "too many {what}"),
            Self::PayloadTooLarge(what) => write!(f, "{what} exceeds its byte bound"),
            Self::UnorderedBulk(what) => write!(f, "unordered registry bulk {what}"),
            Self::EmptyRegistrySync => {
                f.write_str("registry sync terminated before its first chunk")
            }
            Self::SplitInvalidState(state) => write!(f, "invalid neoforge:split state {state}"),
            Self::SplitNotStarted => {
                f.write_str("neoforge:split continuation without a first part")
            }
            Self::SplitEmptyPayload => f.write_str("neoforge:split part without a state byte"),
            Self::SplitTooLarge { buffered, max } => {
                write!(
                    f,
                    "neoforge:split reassembly of {buffered} bytes exceeds {max}"
                )
            }
            Self::UnsupportedCommonVersion => {
                f.write_str("no common networking version shared with the peer")
            }
            Self::UnencodableReason => f.write_str("failure reason is not ASCII"),
        }
    }
}

impl std::error::Error for LoaderCodecError {}

impl From<ProtoError> for LoaderCodecError {
    fn from(error: ProtoError) -> Self {
        Self::Wire(error)
    }
}

/// Vanilla 1.21.1 `ConnectionProtocol`. Ids match what vanilla's
/// `ByteBufCodecs.idMapper` writes, and names match `ConnectionProtocol.id()`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ConnectionProtocol {
    Handshaking,
    Play,
    Status,
    Login,
    Configuration,
}

impl ConnectionProtocol {
    /// Declaration order, ascending id.
    pub const ALL: [Self; 5] = [
        Self::Handshaking,
        Self::Play,
        Self::Status,
        Self::Login,
        Self::Configuration,
    ];

    pub fn id(self) -> i32 {
        match self {
            Self::Handshaking => 0,
            Self::Play => 1,
            Self::Status => 2,
            Self::Login => 3,
            Self::Configuration => 4,
        }
    }

    pub fn from_id(id: i32) -> Option<Self> {
        Self::ALL.into_iter().find(|protocol| protocol.id() == id)
    }

    /// `ConnectionProtocol.id()` — the string `c:register` writes.
    pub fn name(self) -> &'static str {
        match self {
            Self::Handshaking => "handshake",
            Self::Play => "play",
            Self::Status => "status",
            Self::Login => "login",
            Self::Configuration => "configuration",
        }
    }

    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|protocol| protocol.name() == name)
    }
}

/// Vanilla 1.21.1 `PacketFlow`: `SERVERBOUND` is ordinal 0, `CLIENTBOUND` 1.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum PacketFlow {
    Serverbound,
    Clientbound,
}

impl PacketFlow {
    pub fn ordinal(self) -> i32 {
        match self {
            Self::Serverbound => 0,
            Self::Clientbound => 1,
        }
    }

    pub fn from_ordinal(ordinal: i32) -> Option<Self> {
        match ordinal {
            0 => Some(Self::Serverbound),
            1 => Some(Self::Clientbound),
            _ => None,
        }
    }
}

/// Reject anything that is not a canonical lowercase `namespace:path`.
pub fn validate_resource_location(value: &str) -> Result<(), LoaderCodecError> {
    fn namespace_byte(byte: u8) -> bool {
        byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"_.-".contains(&byte)
    }
    fn path_byte(byte: u8) -> bool {
        byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"_./-".contains(&byte)
    }
    let invalid = || LoaderCodecError::InvalidResourceLocation(value.to_owned());
    if value.is_empty() || value.len() > MAX_RESOURCE_LOCATION_BYTES {
        return Err(invalid());
    }
    let (namespace, path) = value.split_once(':').ok_or_else(invalid)?;
    if namespace.is_empty()
        || path.is_empty()
        || !namespace.bytes().all(namespace_byte)
        || !path.bytes().all(path_byte)
    {
        return Err(invalid());
    }
    if matches!(namespace, "." | "..") || path.split('/').any(|part| matches!(part, "." | "..")) {
        return Err(invalid());
    }
    Ok(())
}

fn namespace_of(id: &str) -> String {
    id.split_once(':').map_or_else(
        || "minecraft".to_owned(),
        |(namespace, _)| namespace.to_owned(),
    )
}

fn write_rl(writer: &mut WireWriter, value: &str) -> Result<(), LoaderCodecError> {
    validate_resource_location(value)?;
    writer.write_string(value);
    Ok(())
}

fn read_rl(reader: &mut WireReader<'_>) -> Result<String, LoaderCodecError> {
    let text = reader.read_string(MAX_STRING_CHARS)?;
    validate_resource_location(&text)?;
    Ok(text)
}

fn read_utf(width: usize, reader: &mut WireReader<'_>) -> Result<String, LoaderCodecError> {
    Ok(reader.read_string(width)?)
}

/// `c:version` — `ByteBufCodecs.VAR_INT.apply(ByteBufCodecs.list())`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CommonVersionPayload {
    pub versions: Vec<i32>,
}

impl CommonVersionPayload {
    /// What megu advertises: every version NeoForge 21.1.248 speaks.
    pub fn supported() -> Self {
        Self {
            versions: SUPPORTED_COMMON_NETWORKING_VERSIONS.to_vec(),
        }
    }

    /// Highest version both sides speak, which is the one NeoForge keeps.
    pub fn negotiated_version(&self) -> Option<i32> {
        self.versions
            .iter()
            .copied()
            .filter(|version| SUPPORTED_COMMON_NETWORKING_VERSIONS.contains(version))
            .max()
    }

    pub fn encode(&self) -> Result<Vec<u8>, LoaderCodecError> {
        let mut writer = WireWriter::new();
        self.write(&mut writer)?;
        Ok(writer.into_vec())
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, LoaderCodecError> {
        let mut reader = WireReader::new(bytes);
        let payload = Self::read(&mut reader)?;
        reader
            .finish()
            .map_err(|_| LoaderCodecError::TrailingBytes)?;
        Ok(payload)
    }

    pub fn write(&self, writer: &mut WireWriter) -> Result<(), LoaderCodecError> {
        if self.versions.len() > MAX_COMMON_VERSION_ENTRIES {
            return Err(LoaderCodecError::TooManyEntries(
                "common networking versions",
            ));
        }
        writer.write_varint(self.versions.len() as i32);
        for version in &self.versions {
            writer.write_varint(*version);
        }
        Ok(())
    }

    pub fn read(reader: &mut WireReader<'_>) -> Result<Self, LoaderCodecError> {
        let count = reader.read_count(MAX_COMMON_VERSION_ENTRIES)?;
        let mut versions = Vec::with_capacity(count);
        for _ in 0..count {
            versions.push(reader.read_varint()?);
        }
        Ok(Self { versions })
    }
}

/// `c:register` — `version:i32 | protocol:String | channels:Set<ResourceLocation>`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CommonRegisterPayload {
    pub version: i32,
    pub protocol: String,
    pub channels: Vec<String>,
}

impl CommonRegisterPayload {
    /// A `c:register` body for `protocol`.
    pub fn new(
        protocol: ConnectionProtocol,
        channels: impl IntoIterator<Item = String>,
    ) -> Result<Self, LoaderCodecError> {
        let payload = Self {
            version: COMMON_REGISTER_PAYLOAD_VERSION,
            protocol: protocol.name().to_owned(),
            channels: channels.into_iter().collect(),
        };
        payload.validate()?;
        Ok(payload)
    }

    fn validate(&self) -> Result<(), LoaderCodecError> {
        ConnectionProtocol::from_name(&self.protocol)
            .ok_or_else(|| LoaderCodecError::UnknownCommonProtocol(self.protocol.clone()))?;
        if self.channels.len() > MAX_REGISTERED_CHANNELS_PER_PEER {
            return Err(LoaderCodecError::TooManyEntries("common register channels"));
        }
        let mut seen = BTreeSet::new();
        for channel in &self.channels {
            validate_resource_location(channel)?;
            if !seen.insert(channel.as_str()) {
                return Err(LoaderCodecError::DuplicateEntry("common register channel"));
            }
        }
        Ok(())
    }

    pub fn encode(&self) -> Result<Vec<u8>, LoaderCodecError> {
        let mut writer = WireWriter::new();
        self.write(&mut writer)?;
        Ok(writer.into_vec())
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, LoaderCodecError> {
        let mut reader = WireReader::new(bytes);
        let payload = Self::read(&mut reader)?;
        reader
            .finish()
            .map_err(|_| LoaderCodecError::TrailingBytes)?;
        Ok(payload)
    }

    /// Writes channels in sorted order, so one set has exactly one encoding.
    pub fn write(&self, writer: &mut WireWriter) -> Result<(), LoaderCodecError> {
        self.validate()?;
        writer.write_varint(self.version);
        writer.write_string(&self.protocol);
        let mut channels: Vec<&String> = self.channels.iter().collect();
        channels.sort();
        writer.write_varint(channels.len() as i32);
        for channel in channels {
            writer.write_string(channel);
        }
        Ok(())
    }

    /// Accepts any channel order (NeoForge writes a set) but rejects repeats.
    ///
    /// The `protocol` name is bounded to 32 UTF-16 units, far above the longest
    /// `ConnectionProtocol.id()` (`configuration`).
    pub fn read(reader: &mut WireReader<'_>) -> Result<Self, LoaderCodecError> {
        let version = reader.read_varint()?;
        let protocol = read_utf(32, reader)?;
        if ConnectionProtocol::from_name(&protocol).is_none() {
            return Err(LoaderCodecError::UnknownCommonProtocol(protocol));
        }
        let count = reader.read_count(MAX_REGISTERED_CHANNELS_PER_PEER)?;
        let mut channels = Vec::with_capacity(count);
        let mut seen = BTreeSet::new();
        for _ in 0..count {
            let channel = read_rl(reader)?;
            if !seen.insert(channel.clone()) {
                return Err(LoaderCodecError::DuplicateEntry("common register channel"));
            }
            channels.push(channel);
        }
        Ok(Self {
            version,
            protocol,
            channels,
        })
    }
}

/// One `ModdedNetworkQueryComponent`:
/// `id:RL | version:String | Optional<flow> | optional:bool`.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct ModdedNetworkQueryComponent {
    pub id: String,
    pub version: String,
    pub flow: Option<PacketFlow>,
    pub optional: bool,
}

impl ModdedNetworkQueryComponent {
    pub fn new(id: impl Into<String>, version: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            version: version.into(),
            flow: None,
            optional: false,
        }
    }

    pub fn with_flow(mut self, flow: PacketFlow) -> Self {
        self.flow = Some(flow);
        self
    }

    pub fn optional(mut self, optional: bool) -> Self {
        self.optional = optional;
        self
    }

    fn write(&self, writer: &mut WireWriter) -> Result<(), LoaderCodecError> {
        write_rl(writer, &self.id)?;
        writer.write_string(&self.version);
        match self.flow {
            Some(flow) => {
                writer.write_bool(true);
                writer.write_varint(flow.ordinal());
            }
            None => writer.write_bool(false),
        }
        writer.write_bool(self.optional);
        Ok(())
    }

    fn read(reader: &mut WireReader<'_>) -> Result<Self, LoaderCodecError> {
        let id = read_rl(reader)?;
        let version = read_utf(MAX_STRING_CHARS, reader)?;
        let flow = if reader.read_bool()? {
            let ordinal = reader.read_varint()?;
            Some(
                PacketFlow::from_ordinal(ordinal)
                    .ok_or(LoaderCodecError::UnknownPacketFlow(ordinal))?,
            )
        } else {
            None
        };
        let optional = reader.read_bool()?;
        Ok(Self {
            id,
            version,
            flow,
            optional,
        })
    }
}

/// One protocol's component set in a `neoforge:register` query.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModdedNetworkQueryGroup {
    pub protocol: ConnectionProtocol,
    pub components: Vec<ModdedNetworkQueryComponent>,
}

/// `neoforge:register` — `Map<ConnectionProtocol, Set<Component>>`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ModdedNetworkQueryPayload {
    pub groups: Vec<ModdedNetworkQueryGroup>,
}

impl ModdedNetworkQueryPayload {
    pub fn encode(&self) -> Result<Vec<u8>, LoaderCodecError> {
        let mut writer = WireWriter::new();
        self.write(&mut writer)?;
        Ok(writer.into_vec())
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, LoaderCodecError> {
        let mut reader = WireReader::new(bytes);
        let payload = Self::read(&mut reader)?;
        reader
            .finish()
            .map_err(|_| LoaderCodecError::TrailingBytes)?;
        Ok(payload)
    }

    /// Writes groups by ascending protocol id and components by ascending id.
    pub fn write(&self, writer: &mut WireWriter) -> Result<(), LoaderCodecError> {
        if self.groups.len() > MAX_NEGOTIATION_GROUPS {
            return Err(LoaderCodecError::TooManyEntries("query protocol groups"));
        }
        let mut groups: Vec<&ModdedNetworkQueryGroup> = self.groups.iter().collect();
        groups.sort_by_key(|group| group.protocol.id());
        let mut protocols = BTreeSet::new();
        for group in &groups {
            if !protocols.insert(group.protocol) {
                return Err(LoaderCodecError::DuplicateEntry("query protocol group"));
            }
        }
        writer.write_varint(groups.len() as i32);
        for group in groups {
            if group.components.len() > MAX_REGISTERED_CHANNELS_PER_PEER {
                return Err(LoaderCodecError::TooManyEntries("query components"));
            }
            writer.write_varint(group.protocol.id());
            let mut components: Vec<&ModdedNetworkQueryComponent> =
                group.components.iter().collect();
            components.sort_by(|left, right| left.id.cmp(&right.id));
            let mut seen = BTreeSet::new();
            for component in &components {
                validate_resource_location(&component.id)?;
                if !seen.insert(component.id.as_str()) {
                    return Err(LoaderCodecError::DuplicateEntry("query component"));
                }
            }
            writer.write_varint(components.len() as i32);
            for component in components {
                component.write(writer)?;
            }
        }
        Ok(())
    }

    pub fn read(reader: &mut WireReader<'_>) -> Result<Self, LoaderCodecError> {
        let group_count = reader.read_count(MAX_NEGOTIATION_GROUPS)?;
        let mut groups = Vec::with_capacity(group_count);
        let mut protocols = BTreeSet::new();
        for _ in 0..group_count {
            let protocol_id = reader.read_varint()?;
            let protocol = ConnectionProtocol::from_id(protocol_id)
                .ok_or(LoaderCodecError::UnknownConnectionProtocol(protocol_id))?;
            if !protocols.insert(protocol) {
                return Err(LoaderCodecError::DuplicateEntry("query protocol group"));
            }
            let component_count = reader.read_count(MAX_REGISTERED_CHANNELS_PER_PEER)?;
            let mut components = Vec::with_capacity(component_count);
            let mut seen = BTreeSet::new();
            for _ in 0..component_count {
                let component = ModdedNetworkQueryComponent::read(reader)?;
                if !seen.insert(component.id.clone()) {
                    return Err(LoaderCodecError::DuplicateEntry("query component"));
                }
                components.push(component);
            }
            groups.push(ModdedNetworkQueryGroup {
                protocol,
                components,
            });
        }
        Ok(Self { groups })
    }

    /// `true` when `channel` was advertised for `protocol`.
    pub fn advertises(&self, protocol: ConnectionProtocol, channel: &str) -> bool {
        self.groups.iter().any(|group| {
            group.protocol == protocol
                && group
                    .components
                    .iter()
                    .any(|component| component.id == channel)
        })
    }
}

/// One negotiated channel: `NetworkChannel(id, chosenVersion)`.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct NetworkChannelEntry {
    pub id: String,
    pub chosen_version: String,
}

/// One protocol's negotiated channel map.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModdedNetworkSetupGroup {
    pub protocol: ConnectionProtocol,
    pub channels: Vec<NetworkChannelEntry>,
}

/// `neoforge:network` — `NetworkPayloadSetup(Map<protocol, Map<RL, NetworkChannel>>)`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ModdedNetworkPayload {
    pub groups: Vec<ModdedNetworkSetupGroup>,
}

impl ModdedNetworkPayload {
    pub fn encode(&self) -> Result<Vec<u8>, LoaderCodecError> {
        let mut writer = WireWriter::new();
        self.write(&mut writer)?;
        Ok(writer.into_vec())
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, LoaderCodecError> {
        let mut reader = WireReader::new(bytes);
        let payload = Self::read(&mut reader)?;
        reader
            .finish()
            .map_err(|_| LoaderCodecError::TrailingBytes)?;
        Ok(payload)
    }

    pub fn write(&self, writer: &mut WireWriter) -> Result<(), LoaderCodecError> {
        if self.groups.len() > MAX_NEGOTIATION_GROUPS {
            return Err(LoaderCodecError::TooManyEntries("setup protocol groups"));
        }
        let mut groups: Vec<&ModdedNetworkSetupGroup> = self.groups.iter().collect();
        groups.sort_by_key(|group| group.protocol.id());
        let mut protocols = BTreeSet::new();
        for group in &groups {
            if !protocols.insert(group.protocol) {
                return Err(LoaderCodecError::DuplicateEntry("setup protocol group"));
            }
        }
        writer.write_varint(groups.len() as i32);
        for group in groups {
            if group.channels.len() > MAX_REGISTERED_CHANNELS_PER_PEER {
                return Err(LoaderCodecError::TooManyEntries("setup channels"));
            }
            writer.write_varint(group.protocol.id());
            let mut channels: Vec<&NetworkChannelEntry> = group.channels.iter().collect();
            channels.sort_by(|left, right| left.id.cmp(&right.id));
            let mut seen = BTreeSet::new();
            for channel in &channels {
                validate_resource_location(&channel.id)?;
                if !seen.insert(channel.id.as_str()) {
                    return Err(LoaderCodecError::DuplicateEntry("setup channel"));
                }
            }
            writer.write_varint(channels.len() as i32);
            for channel in channels {
                writer.write_string(&channel.id);
                writer.write_string(&channel.chosen_version);
            }
        }
        Ok(())
    }

    pub fn read(reader: &mut WireReader<'_>) -> Result<Self, LoaderCodecError> {
        let group_count = reader.read_count(MAX_NEGOTIATION_GROUPS)?;
        let mut groups = Vec::with_capacity(group_count);
        let mut protocols = BTreeSet::new();
        for _ in 0..group_count {
            let protocol_id = reader.read_varint()?;
            let protocol = ConnectionProtocol::from_id(protocol_id)
                .ok_or(LoaderCodecError::UnknownConnectionProtocol(protocol_id))?;
            if !protocols.insert(protocol) {
                return Err(LoaderCodecError::DuplicateEntry("setup protocol group"));
            }
            let channel_count = reader.read_count(MAX_REGISTERED_CHANNELS_PER_PEER)?;
            let mut channels = Vec::with_capacity(channel_count);
            let mut seen = BTreeSet::new();
            for _ in 0..channel_count {
                let id = read_rl(reader)?;
                if !seen.insert(id.clone()) {
                    return Err(LoaderCodecError::DuplicateEntry("setup channel"));
                }
                let chosen_version = read_utf(MAX_STRING_CHARS, reader)?;
                channels.push(NetworkChannelEntry { id, chosen_version });
            }
            groups.push(ModdedNetworkSetupGroup { protocol, channels });
        }
        Ok(Self { groups })
    }

    /// The version the peer chose for `channel`, when it negotiated the channel.
    pub fn channel_version(&self, protocol: ConnectionProtocol, channel: &str) -> Option<&str> {
        self.groups
            .iter()
            .find(|group| group.protocol == protocol)
            .and_then(|group| {
                group
                    .channels
                    .iter()
                    .find(|entry| entry.id == channel)
                    .map(|entry| entry.chosen_version.as_str())
            })
    }
}

/// One `modded_network_setup_failed` entry: namespace + NBT-encoded component.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModdedNetworkFailure {
    pub namespace: String,
    pub reason_nbt: Vec<u8>,
}

/// `neoforge:modded_network_setup_failed` — `Map<ResourceLocation, Component>`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ModdedNetworkSetupFailedPayload {
    pub failures: Vec<ModdedNetworkFailure>,
}

impl ModdedNetworkSetupFailedPayload {
    /// Canonical plain-text component NBT for `text`: a root `TAG_String`.
    ///
    /// Vanilla's `ComponentSerialization` accepts both the bare-string root and
    /// a `{text: ...}` compound; megu emits the bare string, which is what a
    /// plain literal component serialises to. Non-ASCII text is rejected because
    /// NBT strings are modified UTF-8 and a six-byte surrogate pair would not
    /// round-trip through this helper.
    pub fn text_component_nbt(text: &str) -> Result<Vec<u8>, LoaderCodecError> {
        if !text.is_ascii() {
            return Err(LoaderCodecError::UnencodableReason);
        }
        let length = u16::try_from(text.len()).map_err(|_| LoaderCodecError::UnencodableReason)?;
        let mut nbt = Vec::with_capacity(text.len() + 5);
        nbt.push(0x08);
        nbt.extend_from_slice(&[0, 0]);
        nbt.extend_from_slice(&length.to_be_bytes());
        nbt.extend_from_slice(text.as_bytes());
        Ok(nbt)
    }

    pub fn encode(&self) -> Result<Vec<u8>, LoaderCodecError> {
        let mut writer = WireWriter::new();
        self.write(&mut writer)?;
        Ok(writer.into_vec())
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, LoaderCodecError> {
        let mut reader = WireReader::new(bytes);
        let payload = Self::read(&mut reader)?;
        reader
            .finish()
            .map_err(|_| LoaderCodecError::TrailingBytes)?;
        Ok(payload)
    }

    pub fn write(&self, writer: &mut WireWriter) -> Result<(), LoaderCodecError> {
        if self.failures.len() > MAX_NEGOTIATION_FAILURES {
            return Err(LoaderCodecError::TooManyEntries("setup failures"));
        }
        writer.write_varint(self.failures.len() as i32);
        let mut seen = BTreeSet::new();
        for failure in &self.failures {
            if !seen.insert(failure.namespace.as_str()) {
                return Err(LoaderCodecError::DuplicateEntry("setup failure namespace"));
            }
            writer.write_string(&failure.namespace);
            if failure.reason_nbt.len() > MAX_FAILURE_REASON_NBT_BYTES {
                return Err(LoaderCodecError::PayloadTooLarge("setup failure reason"));
            }
            writer.write_bytes(&failure.reason_nbt);
        }
        Ok(())
    }

    pub fn read(reader: &mut WireReader<'_>) -> Result<Self, LoaderCodecError> {
        let count = reader.read_count(MAX_NEGOTIATION_FAILURES)?;
        let mut failures = Vec::with_capacity(count);
        let mut seen = BTreeSet::new();
        for _ in 0..count {
            let namespace = read_utf(64, reader)?;
            if !seen.insert(namespace.clone()) {
                return Err(LoaderCodecError::DuplicateEntry("setup failure namespace"));
            }
            let reason_nbt = reader.read_bytes(MAX_FAILURE_REASON_NBT_BYTES)?.to_vec();
            failures.push(ModdedNetworkFailure {
                namespace,
                reason_nbt,
            });
        }
        Ok(Self { failures })
    }
}

/// Read back the root `TAG_String` [`ModdedNetworkSetupFailedPayload::text_component_nbt`]
/// writes; any other shape is lossily rendered instead of being discarded.
fn decode_text_component_nbt(nbt: &[u8]) -> String {
    const ROOT_STRING_PREFIX: usize = 5;
    if nbt.first() == Some(&0x08) && nbt.len() >= ROOT_STRING_PREFIX {
        let name_len = usize::from(u16::from_be_bytes([nbt[1], nbt[2]]));
        let length_at = 3 + name_len;
        if nbt.len() >= length_at + 2 {
            let text_len = usize::from(u16::from_be_bytes([nbt[length_at], nbt[length_at + 1]]));
            let start = length_at + 2;
            if let Some(bytes) = nbt.get(start..start.saturating_add(text_len)) {
                if let Ok(text) = std::str::from_utf8(bytes) {
                    return text.to_owned();
                }
            }
        }
    }
    String::from_utf8_lossy(nbt).into_owned()
}

/// `RegistrySnapshot` — an id map plus an alias map, written as one byte blob.
///
/// Wire: `VarInt(count) | count * (VarInt id, RL name)` followed by
/// `VarInt(count) | count * (RL alias, RL target)`; [`FrozenRegistryPayload`]
/// prefixes the whole blob with its byte length.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RegistrySnapshot {
    pub ids: BTreeMap<i32, String>,
    pub aliases: BTreeMap<String, String>,
}

impl RegistrySnapshot {
    pub fn write(&self, writer: &mut WireWriter) -> Result<(), LoaderCodecError> {
        if self.ids.len() > MAX_REGISTRY_SNAPSHOT_ENTRIES {
            return Err(LoaderCodecError::TooManyEntries("registry snapshot ids"));
        }
        if self.aliases.len() > MAX_REGISTRY_SNAPSHOT_ENTRIES {
            return Err(LoaderCodecError::TooManyEntries(
                "registry snapshot aliases",
            ));
        }
        writer.write_varint(self.ids.len() as i32);
        for (id, name) in &self.ids {
            writer.write_varint(*id);
            write_rl(writer, name)?;
        }
        writer.write_varint(self.aliases.len() as i32);
        for (alias, target) in &self.aliases {
            write_rl(writer, alias)?;
            write_rl(writer, target)?;
        }
        Ok(())
    }

    pub fn read(reader: &mut WireReader<'_>) -> Result<Self, LoaderCodecError> {
        let id_count = reader.read_count(MAX_REGISTRY_SNAPSHOT_ENTRIES)?;
        let mut ids = BTreeMap::new();
        for _ in 0..id_count {
            let id = reader.read_varint()?;
            if ids.contains_key(&id) {
                return Err(LoaderCodecError::DuplicateEntry("registry snapshot id"));
            }
            ids.insert(id, read_rl(reader)?);
        }
        let alias_count = reader.read_count(MAX_REGISTRY_SNAPSHOT_ENTRIES)?;
        let mut aliases = BTreeMap::new();
        for _ in 0..alias_count {
            let alias = read_rl(reader)?;
            if aliases.contains_key(&alias) {
                return Err(LoaderCodecError::DuplicateEntry("registry snapshot alias"));
            }
            aliases.insert(alias, read_rl(reader)?);
        }
        Ok(Self { ids, aliases })
    }
}

/// `neoforge:frozen_registry` — `registryName:RL | snapshot:byte[]`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FrozenRegistryPayload {
    pub registry: String,
    pub snapshot: RegistrySnapshot,
}

impl FrozenRegistryPayload {
    pub fn encode(&self) -> Result<Vec<u8>, LoaderCodecError> {
        let mut writer = WireWriter::new();
        self.write(&mut writer)?;
        Ok(writer.into_vec())
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, LoaderCodecError> {
        let mut reader = WireReader::new(bytes);
        let payload = Self::read(&mut reader)?;
        reader
            .finish()
            .map_err(|_| LoaderCodecError::TrailingBytes)?;
        Ok(payload)
    }

    pub fn write(&self, writer: &mut WireWriter) -> Result<(), LoaderCodecError> {
        write_rl(writer, &self.registry)?;
        let mut snapshot = WireWriter::new();
        self.snapshot.write(&mut snapshot)?;
        let snapshot = snapshot.into_vec();
        if snapshot.len() > MAX_FROZEN_REGISTRY_BYTES {
            return Err(LoaderCodecError::PayloadTooLarge(
                "frozen registry snapshot",
            ));
        }
        writer.write_bytes(&snapshot);
        Ok(())
    }

    pub fn read(reader: &mut WireReader<'_>) -> Result<Self, LoaderCodecError> {
        let registry = read_rl(reader)?;
        let snapshot = reader.read_bytes(MAX_FROZEN_REGISTRY_BYTES)?;
        let mut nested = WireReader::new(snapshot);
        let snapshot = RegistrySnapshot::read(&mut nested)?;
        nested
            .finish()
            .map_err(|_| LoaderCodecError::TrailingBytes)?;
        Ok(Self { registry, snapshot })
    }
}

/// One `neoforge:split` part: `[u8 state][part bytes]`.
pub fn split_packet_payload(state: u8, part: &[u8]) -> Vec<u8> {
    let mut payload = Vec::with_capacity(part.len() + 1);
    payload.push(state);
    payload.extend_from_slice(part);
    payload
}

/// Fail-closed `neoforge:split` reassembly.
///
/// `GenericPacketSplitter` clears its buffer when a first part arrives while the
/// buffer is non-empty; that is reproduced here, as is rejecting a part whose
/// state byte falls outside `0..=2`. Unlike the upstream handler — which appends
/// a continuation into an empty list and then waits forever — a continuation
/// before any first part is an error, and the assembled stream is capped at the
/// configured ceiling (2 MiB compressed, 8 MiB uncompressed).
#[derive(Clone, Debug)]
pub struct SplitPacketReassembler {
    limit: usize,
    buffer: Vec<u8>,
    started: bool,
}

impl Default for SplitPacketReassembler {
    fn default() -> Self {
        Self::uncompressed()
    }
}

impl SplitPacketReassembler {
    /// The default, matching `GenericPacketSplitter.uncompressedSizeLimits`.
    pub fn uncompressed() -> Self {
        Self::with_limit(SPLIT_UNCOMPRESSED_PACKET_LIMIT)
    }

    /// Matching `GenericPacketSplitter.compressedSizeLimits`.
    pub fn compressed() -> Self {
        Self::with_limit(SPLIT_COMPRESSED_PACKET_LIMIT)
    }

    /// A caller ceiling, clamped to [`SPLIT_UNCOMPRESSED_PACKET_LIMIT`].
    pub fn with_limit(max_total_bytes: usize) -> Self {
        Self {
            limit: max_total_bytes.min(SPLIT_UNCOMPRESSED_PACKET_LIMIT),
            buffer: Vec::new(),
            started: false,
        }
    }

    pub fn limit(&self) -> usize {
        self.limit
    }

    pub fn buffered(&self) -> usize {
        self.buffer.len()
    }

    pub fn is_idle(&self) -> bool {
        !self.started && self.buffer.is_empty()
    }

    /// Drop any partial stream. The caller owns the decision to keep waiting.
    pub fn reset(&mut self) {
        self.buffer.clear();
        self.started = false;
    }

    /// Feed one part. `Ok(Some(bytes))` returns the whole reassembled packet on
    /// the last part; `Ok(None)` waits for more.
    pub fn push(&mut self, payload: &[u8]) -> Result<Option<Vec<u8>>, LoaderCodecError> {
        let (state, part) = payload
            .split_first()
            .ok_or(LoaderCodecError::SplitEmptyPayload)?;
        match *state {
            SPLIT_STATE_FIRST => {
                // Out-of-order first part: drop the stale stream, like upstream.
                self.buffer.clear();
                self.started = true;
            }
            SPLIT_STATE_MIDDLE | SPLIT_STATE_LAST if !self.started => {
                return Err(LoaderCodecError::SplitNotStarted);
            }
            SPLIT_STATE_MIDDLE | SPLIT_STATE_LAST => {}
            other => return Err(LoaderCodecError::SplitInvalidState(other)),
        }
        let buffered = self.buffer.len().saturating_add(part.len());
        if buffered > self.limit {
            self.reset();
            return Err(LoaderCodecError::SplitTooLarge {
                buffered,
                max: self.limit,
            });
        }
        self.buffer.extend_from_slice(part);
        if *state == SPLIT_STATE_LAST {
            self.started = false;
            return Ok(Some(std::mem::take(&mut self.buffer)));
        }
        Ok(None)
    }
}

/// `neoforge:config_file` — `file_name:String | contents:byte[]`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ConfigFilePayload {
    pub file_name: String,
    pub contents: Vec<u8>,
}

impl ConfigFilePayload {
    pub fn encode(&self) -> Result<Vec<u8>, LoaderCodecError> {
        let mut writer = WireWriter::new();
        self.write(&mut writer)?;
        Ok(writer.into_vec())
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, LoaderCodecError> {
        let mut reader = WireReader::new(bytes);
        let payload = Self::read(&mut reader)?;
        reader
            .finish()
            .map_err(|_| LoaderCodecError::TrailingBytes)?;
        Ok(payload)
    }

    pub fn write(&self, writer: &mut WireWriter) -> Result<(), LoaderCodecError> {
        if self.file_name.chars().count() > MAX_CONFIG_FILE_NAME_CHARS {
            return Err(LoaderCodecError::PayloadTooLarge("config file name"));
        }
        if self.file_name.contains('/') || self.file_name.contains('\\') {
            return Err(LoaderCodecError::InvalidResourceLocation(
                self.file_name.clone(),
            ));
        }
        if self.contents.len() > MAX_CONFIG_FILE_BYTES {
            return Err(LoaderCodecError::PayloadTooLarge("config file contents"));
        }
        writer.write_string(&self.file_name);
        writer.write_bytes(&self.contents);
        Ok(())
    }

    pub fn read(reader: &mut WireReader<'_>) -> Result<Self, LoaderCodecError> {
        let file_name = read_utf(MAX_CONFIG_FILE_NAME_CHARS, reader)?;
        let contents = reader.read_bytes(MAX_CONFIG_FILE_BYTES)?.to_vec();
        Ok(Self {
            file_name,
            contents,
        })
    }
}

/// One namespace group of a Fabric registry sync.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FabricRegistryNamespace {
    /// Registry namespace; the empty wire form is the `minecraft` sentinel.
    pub namespace: String,
    pub registries: Vec<FabricRegistry>,
}

/// One registry inside a namespace group.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FabricRegistry {
    /// Registry path (`item`, `block`, ...); the namespace is the group's.
    pub path: String,
    pub id_namespaces: Vec<FabricRegistryIdNamespace>,
}

/// One entry-namespace group inside a registry.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FabricRegistryIdNamespace {
    /// Entry namespace; the empty wire form is the `minecraft` sentinel.
    pub namespace: String,
    pub bulks: Vec<FabricRegistryBulk>,
}

/// A run of consecutive raw ids inside one entry namespace.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FabricRegistryBulk {
    /// Raw id of the first path; each following path is one id higher.
    pub first_raw_id: i32,
    pub paths: Vec<String>,
}

impl FabricRegistryBulk {
    /// Raw id of the last path in the bulk.
    pub fn last_raw_id(&self) -> i32 {
        let entries = i32::try_from(self.paths.len()).unwrap_or(i32::MAX);
        self.first_raw_id.saturating_add(entries.saturating_sub(1))
    }
}

fn wire_namespace(namespace: &str) -> &str {
    if namespace == "minecraft" {
        ""
    } else {
        namespace
    }
}

fn group_namespace(wire: &str) -> String {
    if wire.is_empty() {
        "minecraft".to_owned()
    } else {
        wire.to_owned()
    }
}

fn valid_namespace(namespace: &str) -> bool {
    !namespace.is_empty()
        && namespace.len() <= MAX_RESOURCE_LOCATION_BYTES
        && namespace.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"_.-".contains(&byte)
        })
}

fn valid_registry_path(path: &str) -> bool {
    !path.is_empty() && !path.contains(':') && path.len() <= MAX_RESOURCE_LOCATION_BYTES
}

/// `fabric:registry/sync/direct` — the chunked `DirectRegistryPacketHandler` body.
///
/// ```text
/// VarInt registry namespace groups
///   per group:  writeUtf(registry namespace)      "" == minecraft
///               VarInt registries
///                 per registry: writeUtf(path)
///                               VarInt entry-namespace groups
///                                 per group: writeUtf(entry namespace)   "" == minecraft
///                                            VarInt bulks
///                                              per bulk: VarInt(first_raw_id - previous_last)
///                                                        VarInt(size)
///                                                        size * writeUtf(path), id += 1 each
/// ```
///
/// Bulk order and raw ids carry meaning, so they are validated rather than
/// sorted; namespaces, registries and entry namespaces are sorted so one
/// payload has exactly one encoding.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FabricDirectRegistryPayload {
    pub namespaces: Vec<FabricRegistryNamespace>,
}

impl FabricDirectRegistryPayload {
    pub fn encode(&self) -> Result<Vec<u8>, LoaderCodecError> {
        let mut writer = WireWriter::new();
        self.write(&mut writer)?;
        Ok(writer.into_vec())
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, LoaderCodecError> {
        let mut reader = WireReader::new(bytes);
        let payload = Self::read(&mut reader)?;
        reader
            .finish()
            .map_err(|_| LoaderCodecError::TrailingBytes)?;
        Ok(payload)
    }

    /// Encode the whole body, then split it into
    /// [`FABRIC_REGISTRY_SYNC_CHUNK_BYTES`] chunks followed by the zero-length
    /// terminator payload that ends the stream.
    pub fn encode_chunks(&self) -> Result<Vec<Vec<u8>>, LoaderCodecError> {
        let encoded = self.encode()?;
        let mut chunks: Vec<Vec<u8>> = encoded
            .chunks(FABRIC_REGISTRY_SYNC_CHUNK_BYTES)
            .map(<[u8]>::to_vec)
            .collect();
        chunks.push(Vec::new());
        Ok(chunks)
    }

    pub fn write(&self, writer: &mut WireWriter) -> Result<(), LoaderCodecError> {
        if self.namespaces.len() > MAX_FABRIC_REGISTRY_GROUPS {
            return Err(LoaderCodecError::TooManyEntries(
                "registry namespace groups",
            ));
        }
        let mut groups: Vec<&FabricRegistryNamespace> = self.namespaces.iter().collect();
        groups.sort_by(|left, right| {
            wire_namespace(&left.namespace).cmp(wire_namespace(&right.namespace))
        });
        writer.write_varint(groups.len() as i32);
        for group in groups {
            if !valid_namespace(&group.namespace) {
                return Err(LoaderCodecError::InvalidResourceLocation(
                    group.namespace.clone(),
                ));
            }
            writer.write_string(wire_namespace(&group.namespace));
            if group.registries.len() > MAX_FABRIC_REGISTRY_GROUPS {
                return Err(LoaderCodecError::TooManyEntries("registries per namespace"));
            }
            let mut registries: Vec<&FabricRegistry> = group.registries.iter().collect();
            registries.sort_by(|left, right| left.path.cmp(&right.path));
            writer.write_varint(registries.len() as i32);
            for registry in registries {
                if !valid_registry_path(&registry.path) {
                    return Err(LoaderCodecError::InvalidResourceLocation(
                        registry.path.clone(),
                    ));
                }
                writer.write_string(&registry.path);
                if registry.id_namespaces.len() > MAX_FABRIC_REGISTRY_GROUPS {
                    return Err(LoaderCodecError::TooManyEntries("entry namespace groups"));
                }
                let mut id_namespaces: Vec<&FabricRegistryIdNamespace> =
                    registry.id_namespaces.iter().collect();
                id_namespaces.sort_by(|left, right| {
                    wire_namespace(&left.namespace).cmp(wire_namespace(&right.namespace))
                });
                writer.write_varint(id_namespaces.len() as i32);
                for id_namespace in id_namespaces {
                    if !valid_namespace(&id_namespace.namespace) {
                        return Err(LoaderCodecError::InvalidResourceLocation(
                            id_namespace.namespace.clone(),
                        ));
                    }
                    writer.write_string(wire_namespace(&id_namespace.namespace));
                    if id_namespace.bulks.len() > MAX_FABRIC_REGISTRY_GROUPS {
                        return Err(LoaderCodecError::TooManyEntries("registry bulks"));
                    }
                    writer.write_varint(id_namespace.bulks.len() as i32);
                    let mut previous_last = 0i32;
                    for (index, bulk) in id_namespace.bulks.iter().enumerate() {
                        if bulk.paths.is_empty() {
                            return Err(LoaderCodecError::UnorderedBulk("empty bulk"));
                        }
                        if bulk.paths.len() > MAX_FABRIC_REGISTRY_BULK_ENTRIES {
                            return Err(LoaderCodecError::TooManyEntries("bulk entries"));
                        }
                        if bulk.first_raw_id < 0 {
                            return Err(LoaderCodecError::UnorderedBulk("negative first raw id"));
                        }
                        let minimum = if index == 0 {
                            0
                        } else {
                            previous_last.saturating_add(1)
                        };
                        if bulk.first_raw_id < minimum {
                            return Err(LoaderCodecError::UnorderedBulk("overlapping raw ids"));
                        }
                        let delta = bulk
                            .first_raw_id
                            .checked_sub(previous_last)
                            .ok_or(LoaderCodecError::UnorderedBulk("raw id overflow"))?;
                        writer.write_varint(delta);
                        writer.write_varint(bulk.paths.len() as i32);
                        for path in &bulk.paths {
                            if !valid_registry_path(path) {
                                return Err(LoaderCodecError::InvalidResourceLocation(
                                    path.clone(),
                                ));
                            }
                            writer.write_string(path);
                        }
                        previous_last = bulk.last_raw_id();
                    }
                }
            }
        }
        Ok(())
    }

    pub fn read(reader: &mut WireReader<'_>) -> Result<Self, LoaderCodecError> {
        let group_count = reader.read_count(MAX_FABRIC_REGISTRY_GROUPS)?;
        let mut namespaces = Vec::with_capacity(group_count);
        for _ in 0..group_count {
            let namespace = group_namespace(&read_utf(MAX_RESOURCE_LOCATION_BYTES, reader)?);
            if !valid_namespace(&namespace) {
                return Err(LoaderCodecError::InvalidResourceLocation(namespace));
            }
            let registry_count = reader.read_count(MAX_FABRIC_REGISTRY_GROUPS)?;
            let mut registries = Vec::with_capacity(registry_count);
            for _ in 0..registry_count {
                let path = read_utf(MAX_RESOURCE_LOCATION_BYTES, reader)?;
                if !valid_registry_path(&path) {
                    return Err(LoaderCodecError::InvalidResourceLocation(path));
                }
                let id_namespace_count = reader.read_count(MAX_FABRIC_REGISTRY_GROUPS)?;
                let mut id_namespaces = Vec::with_capacity(id_namespace_count);
                for _ in 0..id_namespace_count {
                    let entry_namespace =
                        group_namespace(&read_utf(MAX_RESOURCE_LOCATION_BYTES, reader)?);
                    if !valid_namespace(&entry_namespace) {
                        return Err(LoaderCodecError::InvalidResourceLocation(entry_namespace));
                    }
                    let bulk_count = reader.read_count(MAX_FABRIC_REGISTRY_GROUPS)?;
                    let mut bulks = Vec::with_capacity(bulk_count);
                    let mut previous_last = 0i32;
                    for index in 0..bulk_count {
                        let delta = reader.read_varint()?;
                        if delta < 0 {
                            return Err(LoaderCodecError::UnorderedBulk("negative delta"));
                        }
                        let size = reader.read_count(MAX_FABRIC_REGISTRY_BULK_ENTRIES)?;
                        if size == 0 {
                            return Err(LoaderCodecError::UnorderedBulk("empty bulk"));
                        }
                        let first_raw_id = previous_last
                            .checked_add(delta)
                            .ok_or(LoaderCodecError::UnorderedBulk("raw id overflow"))?;
                        if index > 0 && delta == 0 {
                            return Err(LoaderCodecError::UnorderedBulk("overlapping raw ids"));
                        }
                        let mut paths = Vec::with_capacity(size);
                        for _ in 0..size {
                            let path = read_utf(MAX_RESOURCE_LOCATION_BYTES, reader)?;
                            if !valid_registry_path(&path) {
                                return Err(LoaderCodecError::InvalidResourceLocation(path));
                            }
                            paths.push(path);
                        }
                        let entries = i32::try_from(size).unwrap_or(i32::MAX);
                        previous_last = first_raw_id.saturating_add(entries.saturating_sub(1));
                        bulks.push(FabricRegistryBulk {
                            first_raw_id,
                            paths,
                        });
                    }
                    id_namespaces.push(FabricRegistryIdNamespace {
                        namespace: entry_namespace,
                        bulks,
                    });
                }
                registries.push(FabricRegistry {
                    path,
                    id_namespaces,
                });
            }
            namespaces.push(FabricRegistryNamespace {
                namespace,
                registries,
            });
        }
        Ok(Self { namespaces })
    }
}

/// Fail-closed reassembly of a chunked `fabric:registry/sync/direct` stream.
#[derive(Clone, Debug)]
pub struct DirectRegistryReassembler {
    limit: usize,
    buffer: Vec<u8>,
}

impl Default for DirectRegistryReassembler {
    fn default() -> Self {
        Self::new()
    }
}

impl DirectRegistryReassembler {
    pub fn new() -> Self {
        Self::with_limit(MAX_FABRIC_REGISTRY_SYNC_BYTES)
    }

    pub fn with_limit(max_bytes: usize) -> Self {
        Self {
            limit: max_bytes.min(MAX_FABRIC_REGISTRY_SYNC_BYTES),
            buffer: Vec::new(),
        }
    }

    pub fn limit(&self) -> usize {
        self.limit
    }

    pub fn buffered(&self) -> usize {
        self.buffer.len()
    }

    pub fn reset(&mut self) {
        self.buffer.clear();
    }

    /// Feed one chunk. A zero-length chunk is the stream terminator: it decodes
    /// everything accumulated so far and returns it.
    pub fn push(
        &mut self,
        chunk: &[u8],
    ) -> Result<Option<FabricDirectRegistryPayload>, LoaderCodecError> {
        if chunk.is_empty() {
            if self.buffer.is_empty() {
                return Err(LoaderCodecError::EmptyRegistrySync);
            }
            let bytes = std::mem::take(&mut self.buffer);
            return FabricDirectRegistryPayload::decode(&bytes).map(Some);
        }
        let buffered = self.buffer.len().saturating_add(chunk.len());
        if buffered > self.limit {
            self.reset();
            return Err(LoaderCodecError::PayloadTooLarge("registry sync body"));
        }
        self.buffer.extend_from_slice(chunk);
        Ok(None)
    }
}

/// `fabric-screen-handler-api-v1:open_screen` — the extended-screen open payload.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FabricOpenScreenPayload {
    /// Menu type id, e.g. `create:crafting_blueprint`.
    pub screen_type: String,
    /// The container sync id vanilla would have used.
    pub sync_id: u8,
    /// NBT-encoded title component.
    pub title_nbt: Vec<u8>,
    /// Menu-specific open data.
    pub extra_data: Vec<u8>,
}

impl FabricOpenScreenPayload {
    pub fn encode(&self) -> Result<Vec<u8>, LoaderCodecError> {
        let mut writer = WireWriter::new();
        self.write(&mut writer)?;
        Ok(writer.into_vec())
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, LoaderCodecError> {
        let mut reader = WireReader::new(bytes);
        let payload = Self::read(&mut reader)?;
        reader
            .finish()
            .map_err(|_| LoaderCodecError::TrailingBytes)?;
        Ok(payload)
    }

    pub fn write(&self, writer: &mut WireWriter) -> Result<(), LoaderCodecError> {
        validate_resource_location(&self.screen_type)?;
        if self.title_nbt.len() > MAX_OPEN_SCREEN_TITLE_NBT_BYTES {
            return Err(LoaderCodecError::PayloadTooLarge("open screen title"));
        }
        if self.extra_data.len() > MAX_OPEN_SCREEN_EXTRA_DATA_BYTES {
            return Err(LoaderCodecError::PayloadTooLarge("open screen extra data"));
        }
        writer.write_string(&self.screen_type);
        writer.write_u8(self.sync_id);
        writer.write_bytes(&self.title_nbt);
        writer.write_bytes(&self.extra_data);
        Ok(())
    }

    pub fn read(reader: &mut WireReader<'_>) -> Result<Self, LoaderCodecError> {
        let screen_type = read_rl(reader)?;
        let sync_id = reader.read_u8()?;
        let title_nbt = reader.read_bytes(MAX_OPEN_SCREEN_TITLE_NBT_BYTES)?.to_vec();
        let extra_data = reader
            .read_bytes(MAX_OPEN_SCREEN_EXTRA_DATA_BYTES)?
            .to_vec();
        Ok(Self {
            screen_type,
            sync_id,
            title_nbt,
            extra_data,
        })
    }
}

/// One payload this process registered, as `PayloadRegistration` records it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LocalPayloadRegistration {
    pub id: String,
    pub version: String,
    pub protocol: ConnectionProtocol,
    /// `None` means the payload is accepted in both directions.
    pub flow: Option<PacketFlow>,
    /// `true` when the connection may continue without this payload.
    pub optional: bool,
}

impl LocalPayloadRegistration {
    pub fn required(
        id: impl Into<String>,
        version: impl Into<String>,
        protocol: ConnectionProtocol,
    ) -> Self {
        Self {
            id: id.into(),
            version: version.into(),
            protocol,
            flow: None,
            optional: false,
        }
    }

    pub fn optional(
        id: impl Into<String>,
        version: impl Into<String>,
        protocol: ConnectionProtocol,
    ) -> Self {
        Self {
            optional: true,
            ..Self::required(id, version, protocol)
        }
    }

    pub fn with_flow(mut self, flow: PacketFlow) -> Self {
        self.flow = Some(flow);
        self
    }
}

/// Outcome of one `neoforge:register` exchange.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct NeoForgeNegotiationResult {
    /// The `c:version` the two sides agreed on, when that step already ran.
    pub common_version: Option<i32>,
    /// Channels both sides registered with an identical version.
    pub setup: ModdedNetworkPayload,
    /// Present only when at least one required channel failed.
    pub failures: Option<ModdedNetworkSetupFailedPayload>,
    /// Channels that matched, sorted.
    pub accepted_channels: Vec<String>,
    /// Peer channels we do not implement, dropped because they were optional.
    pub dropped_optional: Vec<String>,
}

impl NeoForgeNegotiationResult {
    pub fn is_accepted(&self) -> bool {
        self.failures.is_none()
    }

    /// Plain-text reasons, one per failed namespace, in deterministic order.
    ///
    /// [`ModdedNetworkSetupFailedPayload::text_component_nbt`] writes a root
    /// `TAG_String`, so the text is recovered from that envelope; anything else
    /// is reported lossily rather than dropped.
    pub fn failure_reasons(&self) -> Vec<(String, String)> {
        self.failures
            .as_ref()
            .map(|failures| {
                failures
                    .failures
                    .iter()
                    .map(|failure| {
                        (
                            failure.namespace.clone(),
                            decode_text_component_nbt(&failure.reason_nbt),
                        )
                    })
                    .collect()
            })
            .unwrap_or_default()
    }
}

/// Stateful NeoForge negotiation: `c:version`, then the `neoforge:register` query.
#[derive(Clone, Debug)]
pub struct NeoForgeNegotiation {
    local: Vec<LocalPayloadRegistration>,
    common_version: Option<i32>,
}

impl NeoForgeNegotiation {
    /// Reject malformed or oversized local registrations up front.
    pub fn new(local: Vec<LocalPayloadRegistration>) -> Result<Self, LoaderCodecError> {
        if local.len() > MAX_REGISTERED_CHANNELS_PER_PEER {
            return Err(LoaderCodecError::TooManyEntries(
                "local payload registrations",
            ));
        }
        let mut ids = BTreeSet::new();
        for registration in &local {
            validate_resource_location(&registration.id)?;
            if !ids.insert((registration.protocol, registration.id.as_str())) {
                return Err(LoaderCodecError::DuplicateEntry(
                    "local payload registration",
                ));
            }
        }
        Ok(Self {
            local,
            common_version: None,
        })
    }

    /// Answer `c:version`. `Err(UnsupportedCommonVersion)` is a hard disconnect:
    /// there is no shared networking version to fall back to.
    pub fn accept_common_version(
        &mut self,
        remote: &CommonVersionPayload,
    ) -> Result<i32, LoaderCodecError> {
        let version = remote
            .negotiated_version()
            .ok_or(LoaderCodecError::UnsupportedCommonVersion)?;
        self.common_version = Some(version);
        Ok(version)
    }

    pub fn common_version(&self) -> Option<i32> {
        self.common_version
    }

    /// The `c:register` body to answer with for `protocol`.
    pub fn register_payload(
        &self,
        protocol: ConnectionProtocol,
    ) -> Result<CommonRegisterPayload, LoaderCodecError> {
        CommonRegisterPayload::new(
            protocol,
            self.local
                .iter()
                .filter(|registration| registration.protocol == protocol)
                .map(|registration| registration.id.clone()),
        )
    }

    /// Match the client's `neoforge:register` query against the local table.
    pub fn negotiate(&self, query: &ModdedNetworkQueryPayload) -> NeoForgeNegotiationResult {
        let mut setup: BTreeMap<ConnectionProtocol, BTreeMap<String, String>> = BTreeMap::new();
        let mut failures: BTreeMap<String, String> = BTreeMap::new();
        let mut accepted_channels = Vec::new();
        let mut dropped_optional = Vec::new();
        let mut advertised: BTreeSet<(ConnectionProtocol, String)> = BTreeSet::new();

        for group in &query.groups {
            for component in &group.components {
                advertised.insert((group.protocol, component.id.clone()));
                let local = self.local.iter().find(|registration| {
                    registration.protocol == group.protocol && registration.id == component.id
                });
                let Some(local) = local else {
                    if component.optional {
                        dropped_optional.push(component.id.clone());
                    } else {
                        failures
                            .entry(namespace_of(&component.id))
                            .or_insert_with(|| {
                                format!("payload {} is not registered here", component.id)
                            });
                    }
                    continue;
                };
                if let (Some(local_flow), Some(remote_flow)) = (local.flow, component.flow) {
                    if local_flow != remote_flow {
                        let reason = format!(
                            "payload {} flow mismatch ({local_flow:?} vs {remote_flow:?})",
                            component.id
                        );
                        if local.optional || component.optional {
                            dropped_optional.push(component.id.clone());
                        } else {
                            failures
                                .entry(namespace_of(&component.id))
                                .or_insert(reason);
                        }
                        continue;
                    }
                }
                if local.version != component.version {
                    let reason = format!(
                        "payload {} version mismatch ({} vs {})",
                        component.id, local.version, component.version
                    );
                    if local.optional || component.optional {
                        dropped_optional.push(component.id.clone());
                    } else {
                        failures
                            .entry(namespace_of(&component.id))
                            .or_insert(reason);
                    }
                    continue;
                }
                setup
                    .entry(group.protocol)
                    .or_default()
                    .insert(component.id.clone(), local.version.clone());
                accepted_channels.push(component.id.clone());
            }
        }

        // The other half of NeoForge's check: a required local payload the peer
        // never advertised means the peer silently lacks our channel.
        let spoken: BTreeSet<ConnectionProtocol> =
            query.groups.iter().map(|group| group.protocol).collect();
        for registration in &self.local {
            if registration.optional || !spoken.contains(&registration.protocol) {
                continue;
            }
            if !advertised.contains(&(registration.protocol, registration.id.clone())) {
                failures
                    .entry(namespace_of(&registration.id))
                    .or_insert_with(|| {
                        format!("peer did not offer required payload {}", registration.id)
                    });
            }
        }

        let setup = ModdedNetworkPayload {
            groups: setup
                .into_iter()
                .map(|(protocol, channels)| ModdedNetworkSetupGroup {
                    protocol,
                    channels: channels
                        .into_iter()
                        .map(|(id, chosen_version)| NetworkChannelEntry { id, chosen_version })
                        .collect(),
                })
                .collect(),
        };

        let failures = if failures.is_empty() {
            None
        } else {
            Some(ModdedNetworkSetupFailedPayload {
                failures: failures
                    .into_iter()
                    .take(MAX_NEGOTIATION_FAILURES)
                    .map(|(namespace, reason)| ModdedNetworkFailure {
                        namespace,
                        reason_nbt: ModdedNetworkSetupFailedPayload::text_component_nbt(&reason)
                            .unwrap_or_default(),
                    })
                    .collect(),
            })
        };

        accepted_channels.sort();
        dropped_optional.sort();
        NeoForgeNegotiationResult {
            common_version: self.common_version,
            setup,
            failures,
            accepted_channels,
            dropped_optional,
        }
    }
}

/// One-shot negotiation: `c:version` then the client's `neoforge:register` query.
pub fn negotiate_neoforge_connection(
    local: Vec<LocalPayloadRegistration>,
    common: &CommonVersionPayload,
    query: &ModdedNetworkQueryPayload,
) -> Result<NeoForgeNegotiationResult, LoaderCodecError> {
    let mut negotiation = NeoForgeNegotiation::new(local)?;
    negotiation.accept_common_version(common)?;
    Ok(negotiation.negotiate(query))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `VarInt(len) + bytes`, spelled out so a golden stays readable.
    fn utf(text: &str) -> Vec<u8> {
        let mut out = vec![u8::try_from(text.len()).expect("short test string")];
        out.extend_from_slice(text.as_bytes());
        out
    }

    fn concat(parts: &[Vec<u8>]) -> Vec<u8> {
        parts.concat()
    }

    #[test]
    fn common_version_golden_and_fail_closed() {
        let supported = CommonVersionPayload { versions: vec![1] };
        assert_eq!(supported.encode().unwrap(), vec![1, 1]);
        assert_eq!(supported.negotiated_version(), Some(1));
        assert_eq!(CommonVersionPayload::decode(&[1, 1]).unwrap(), supported);
        assert_eq!(
            CommonVersionPayload::decode(&[2, 1, 7]).unwrap(),
            CommonVersionPayload {
                versions: vec![1, 7]
            }
        );
        // A version NeoForge 21.1.248 does not speak is never negotiated.
        assert_eq!(
            CommonVersionPayload { versions: vec![7] }.negotiated_version(),
            None
        );
        assert_eq!(CommonVersionPayload::decode(&[0]).unwrap().versions, vec![]);
        assert_eq!(
            CommonVersionPayload::decode(&[2, 1]),
            Err(LoaderCodecError::Wire(ProtoError::UnexpectedEof {
                needed: 1,
                remaining: 0,
            }))
        );
        assert_eq!(
            CommonVersionPayload::decode(&[1, 1, 0]),
            Err(LoaderCodecError::TrailingBytes)
        );
        assert_eq!(
            CommonVersionPayload::supported().versions,
            SUPPORTED_COMMON_NETWORKING_VERSIONS.to_vec()
        );
        assert_eq!(
            CommonVersionPayload {
                versions: vec![0; MAX_COMMON_VERSION_ENTRIES + 1]
            }
            .encode(),
            Err(LoaderCodecError::TooManyEntries(
                "common networking versions"
            ))
        );
    }

    #[test]
    fn common_register_golden_sorts_and_rejects_duplicates() {
        let payload = CommonRegisterPayload {
            version: COMMON_REGISTER_PAYLOAD_VERSION,
            protocol: ConnectionProtocol::Play.name().to_owned(),
            // Unsorted on input; the canonical encoding is ascending.
            channels: vec!["neoforge:register".to_owned(), "c:version".to_owned()],
        };
        assert_eq!(
            payload.encode().unwrap(),
            concat(&[
                vec![1],
                utf("play"),
                vec![2],
                utf("c:version"),
                utf("neoforge:register"),
            ])
        );
        let decoded = CommonRegisterPayload::decode(&payload.encode().unwrap()).unwrap();
        assert_eq!(decoded.channels, vec!["c:version", "neoforge:register"]);
        assert_eq!(decoded.version, 1);

        let duplicate = CommonRegisterPayload {
            version: 1,
            protocol: "play".to_owned(),
            channels: vec!["c:version".to_owned(), "c:version".to_owned()],
        };
        assert_eq!(
            duplicate.encode(),
            Err(LoaderCodecError::DuplicateEntry("common register channel"))
        );
        assert_eq!(
            CommonRegisterPayload {
                version: 1,
                protocol: "handshaking".to_owned(),
                channels: Vec::new(),
            }
            .encode(),
            Err(LoaderCodecError::UnknownCommonProtocol(
                "handshaking".to_owned()
            ))
        );
        // The wire string is `handshake`, not the enum constant's name.
        assert_eq!(ConnectionProtocol::Handshaking.name(), "handshake");
        assert_eq!(
            CommonRegisterPayload::decode(&concat(&[vec![1], utf("hands"), vec![0]])),
            Err(LoaderCodecError::UnknownCommonProtocol("hands".to_owned()))
        );
        assert_eq!(
            CommonRegisterPayload::new(ConnectionProtocol::Configuration, ["c:version".to_owned()])
                .expect("valid register body")
                .version,
            COMMON_REGISTER_PAYLOAD_VERSION
        );
        assert_eq!(
            CommonRegisterPayload::new(ConnectionProtocol::Play, ["NotAChannel".to_owned()]).err(),
            Some(LoaderCodecError::InvalidResourceLocation(
                "NotAChannel".to_owned()
            ))
        );
    }

    #[test]
    fn modded_network_query_golden() {
        let payload = ModdedNetworkQueryPayload {
            groups: vec![ModdedNetworkQueryGroup {
                protocol: ConnectionProtocol::Configuration,
                components: vec![
                    ModdedNetworkQueryComponent::new("create:packets", "3"),
                    ModdedNetworkQueryComponent::new("minecraft:brand", "1")
                        .with_flow(PacketFlow::Clientbound)
                        .optional(true),
                ],
            }],
        };
        assert_eq!(
            payload.encode().unwrap(),
            concat(&[
                vec![1, 4, 2],
                utf("create:packets"),
                utf("3"),
                vec![0, 0],
                utf("minecraft:brand"),
                utf("1"),
                vec![1, 1, 1],
            ])
        );
        assert_eq!(
            ModdedNetworkQueryPayload::decode(&payload.encode().unwrap()).unwrap(),
            payload
        );
        assert!(payload.advertises(ConnectionProtocol::Configuration, "create:packets"));
        assert!(!payload.advertises(ConnectionProtocol::Play, "create:packets"));
        assert_eq!(
            ModdedNetworkQueryPayload::decode(&[1, 9, 0]),
            Err(LoaderCodecError::UnknownConnectionProtocol(9))
        );
        assert_eq!(
            ModdedNetworkQueryPayload::decode(&concat(&[
                vec![1, 4, 1],
                utf("minecraft:brand"),
                utf("1"),
                vec![1, 5, 0],
            ])),
            Err(LoaderCodecError::UnknownPacketFlow(5))
        );
        let duplicate = ModdedNetworkQueryPayload {
            groups: vec![ModdedNetworkQueryGroup {
                protocol: ConnectionProtocol::Configuration,
                components: vec![
                    ModdedNetworkQueryComponent::new("create:packets", "3"),
                    ModdedNetworkQueryComponent::new("create:packets", "3"),
                ],
            }],
        };
        assert_eq!(
            duplicate.encode(),
            Err(LoaderCodecError::DuplicateEntry("query component"))
        );
        // ...and on the wire, from a peer that sends them anyway.
        assert_eq!(
            ModdedNetworkQueryPayload::decode(&concat(&[
                vec![1, 4, 2],
                utf("create:packets"),
                utf("3"),
                vec![0, 0],
                utf("create:packets"),
                utf("3"),
                vec![0, 0],
            ])),
            Err(LoaderCodecError::DuplicateEntry("query component"))
        );
    }

    #[test]
    fn modded_network_setup_golden() {
        let payload = ModdedNetworkPayload {
            groups: vec![ModdedNetworkSetupGroup {
                protocol: ConnectionProtocol::Configuration,
                channels: vec![
                    NetworkChannelEntry {
                        id: "c:version".to_owned(),
                        chosen_version: "1".to_owned(),
                    },
                    NetworkChannelEntry {
                        id: "create:packets".to_owned(),
                        chosen_version: "3".to_owned(),
                    },
                ],
            }],
        };
        assert_eq!(
            payload.encode().unwrap(),
            concat(&[
                vec![1, 4, 2],
                utf("c:version"),
                utf("1"),
                utf("create:packets"),
                utf("3"),
            ])
        );
        assert_eq!(
            ModdedNetworkPayload::decode(&payload.encode().unwrap()).unwrap(),
            payload
        );
        assert_eq!(
            payload.channel_version(ConnectionProtocol::Configuration, "create:packets"),
            Some("3")
        );
        assert_eq!(
            payload.channel_version(ConnectionProtocol::Play, "create:packets"),
            None
        );
        assert_eq!(
            ModdedNetworkPayload::decode(&concat(&[vec![1, 4, 1], utf("a:b"), utf("1"),])),
            Ok(ModdedNetworkPayload {
                groups: vec![ModdedNetworkSetupGroup {
                    protocol: ConnectionProtocol::Configuration,
                    channels: vec![NetworkChannelEntry {
                        id: "a:b".to_owned(),
                        chosen_version: "1".to_owned(),
                    }],
                }],
            })
        );
        assert_eq!(
            ModdedNetworkPayload {
                groups: vec![ModdedNetworkSetupGroup {
                    protocol: ConnectionProtocol::Configuration,
                    channels: Vec::new(),
                }],
            }
            .encode()
            .unwrap(),
            vec![1, 4, 0],
            "a protocol group with no channels is legal wire"
        );
    }

    #[test]
    fn setup_failed_golden_and_nbt_reason() {
        let reason =
            ModdedNetworkSetupFailedPayload::text_component_nbt("missing create").expect("ascii");
        assert_eq!(
            reason,
            concat(&[
                vec![0x08, 0x00, 0x00, 0x00, 0x0e],
                b"missing create".to_vec()
            ])
        );
        assert_eq!(
            ModdedNetworkSetupFailedPayload::text_component_nbt("caf\u{e9}"),
            Err(LoaderCodecError::UnencodableReason)
        );
        let payload = ModdedNetworkSetupFailedPayload {
            failures: vec![ModdedNetworkFailure {
                namespace: "create".to_owned(),
                reason_nbt: reason.clone(),
            }],
        };
        assert_eq!(
            payload.encode().unwrap(),
            concat(&[vec![1], utf("create"), vec![reason.len() as u8], reason])
        );
        assert_eq!(
            ModdedNetworkSetupFailedPayload::decode(&payload.encode().unwrap()).unwrap(),
            payload
        );
        let duplicate = ModdedNetworkSetupFailedPayload {
            failures: vec![
                ModdedNetworkFailure {
                    namespace: "create".to_owned(),
                    reason_nbt: Vec::new(),
                },
                ModdedNetworkFailure {
                    namespace: "create".to_owned(),
                    reason_nbt: Vec::new(),
                },
            ],
        };
        assert_eq!(
            duplicate.encode(),
            Err(LoaderCodecError::DuplicateEntry("setup failure namespace"))
        );
    }

    #[test]
    fn frozen_registry_golden_roundtrip() {
        let mut ids = BTreeMap::new();
        ids.insert(0, "minecraft:air".to_owned());
        ids.insert(4, "create:shaft".to_owned());
        let mut aliases = BTreeMap::new();
        aliases.insert("create:old_shaft".to_owned(), "create:shaft".to_owned());
        let payload = FrozenRegistryPayload {
            registry: "minecraft:block".to_owned(),
            snapshot: RegistrySnapshot { ids, aliases },
        };
        let snapshot = concat(&[
            vec![2, 0],
            utf("minecraft:air"),
            vec![4],
            utf("create:shaft"),
            vec![1],
            utf("create:old_shaft"),
            utf("create:shaft"),
        ]);
        assert!(snapshot.len() < 128, "single-byte length prefix expected");
        let mut expected = utf("minecraft:block");
        expected.push(snapshot.len() as u8);
        expected.extend_from_slice(&snapshot);
        assert_eq!(payload.encode().unwrap(), expected);
        assert_eq!(FrozenRegistryPayload::decode(&expected).unwrap(), payload);

        // Trailing bytes inside the nested snapshot are rejected.
        let mut trailing = expected.clone();
        let index = trailing.len() - snapshot.len() - 1;
        trailing[index] += 1;
        trailing.push(0);
        assert_eq!(
            FrozenRegistryPayload::decode(&trailing),
            Err(LoaderCodecError::TrailingBytes)
        );
        // A repeated id in one snapshot is rejected.
        let repeated = concat(&[
            vec![2, 0],
            utf("minecraft:air"),
            vec![0],
            utf("minecraft:air"),
            vec![0],
        ]);
        let mut duplicate = utf("minecraft:block");
        duplicate.push(repeated.len() as u8);
        duplicate.extend_from_slice(&repeated);
        assert_eq!(
            FrozenRegistryPayload::decode(&duplicate),
            Err(LoaderCodecError::DuplicateEntry("registry snapshot id"))
        );
        // A registry name that is not a resource location is rejected.
        assert_eq!(
            FrozenRegistryPayload::decode(&concat(&[utf("NotARegistry"), vec![0, 0, 0]])),
            Err(LoaderCodecError::InvalidResourceLocation(
                "NotARegistry".to_owned()
            ))
        );
    }

    #[test]
    fn split_reassembler_reassembles_and_fails_closed() {
        let mut reassembler = SplitPacketReassembler::compressed();
        assert_eq!(reassembler.limit(), SPLIT_COMPRESSED_PACKET_LIMIT);
        assert!(reassembler.is_idle());
        assert_eq!(
            reassembler.push(&split_packet_payload(SPLIT_STATE_MIDDLE, b"x")),
            Err(LoaderCodecError::SplitNotStarted)
        );
        assert_eq!(
            reassembler.push(&split_packet_payload(SPLIT_STATE_LAST, b"x")),
            Err(LoaderCodecError::SplitNotStarted)
        );
        assert_eq!(
            reassembler.push(&[]),
            Err(LoaderCodecError::SplitEmptyPayload)
        );
        assert_eq!(
            reassembler.push(&split_packet_payload(7, b"x")),
            Err(LoaderCodecError::SplitInvalidState(7))
        );
        assert_eq!(
            reassembler.push(&split_packet_payload(SPLIT_STATE_FIRST, b"payload-")),
            Ok(None)
        );
        assert_eq!(reassembler.buffered(), 8);
        assert_eq!(
            reassembler.push(&split_packet_payload(SPLIT_STATE_MIDDLE, b"and-")),
            Ok(None)
        );
        assert_eq!(
            reassembler.push(&split_packet_payload(SPLIT_STATE_LAST, b"more")),
            Ok(Some(b"payload-and-more".to_vec()))
        );
        assert!(reassembler.is_idle());

        // A first part discards a stale partial stream, like upstream.
        assert_eq!(
            reassembler.push(&split_packet_payload(SPLIT_STATE_FIRST, b"stale")),
            Ok(None)
        );
        assert_eq!(
            reassembler.push(&split_packet_payload(SPLIT_STATE_FIRST, b"fresh")),
            Ok(None)
        );
        assert_eq!(
            reassembler.push(&split_packet_payload(SPLIT_STATE_LAST, b"!")),
            Ok(Some(b"fresh!".to_vec()))
        );

        // The ceiling is enforced before the buffer grows.
        let mut small = SplitPacketReassembler::with_limit(8);
        assert_eq!(
            small.push(&split_packet_payload(SPLIT_STATE_FIRST, &[0u8; 8])),
            Ok(None)
        );
        assert_eq!(
            small.push(&split_packet_payload(SPLIT_STATE_LAST, &[0u8; 1])),
            Err(LoaderCodecError::SplitTooLarge {
                buffered: 9,
                max: 8
            })
        );
        assert!(small.is_idle());
        // A caller ceiling can never lift the vanilla 8 MiB ceiling.
        assert_eq!(
            SplitPacketReassembler::with_limit(usize::MAX).limit(),
            SPLIT_UNCOMPRESSED_PACKET_LIMIT
        );
        assert_eq!(
            SplitPacketReassembler::default().limit(),
            SPLIT_UNCOMPRESSED_PACKET_LIMIT
        );
    }

    #[test]
    fn config_file_golden_and_bounds() {
        let payload = ConfigFilePayload {
            file_name: "neoforge-server.toml".to_owned(),
            contents: b"enabled=true".to_vec(),
        };
        assert_eq!(
            payload.encode().unwrap(),
            concat(&[
                utf("neoforge-server.toml"),
                vec![12],
                b"enabled=true".to_vec()
            ])
        );
        assert_eq!(
            ConfigFilePayload::decode(&payload.encode().unwrap()).unwrap(),
            payload
        );
        assert_eq!(
            ConfigFilePayload {
                file_name: "../evil.toml".to_owned(),
                contents: Vec::new(),
            }
            .encode(),
            Err(LoaderCodecError::InvalidResourceLocation(
                "../evil.toml".to_owned()
            ))
        );
        assert_eq!(
            ConfigFilePayload {
                file_name: "a".repeat(MAX_CONFIG_FILE_NAME_CHARS + 1),
                contents: Vec::new(),
            }
            .encode(),
            Err(LoaderCodecError::PayloadTooLarge("config file name"))
        );
    }

    fn fabric_sample() -> FabricDirectRegistryPayload {
        FabricDirectRegistryPayload {
            namespaces: vec![
                FabricRegistryNamespace {
                    namespace: "minecraft".to_owned(),
                    registries: vec![FabricRegistry {
                        path: "block".to_owned(),
                        id_namespaces: vec![FabricRegistryIdNamespace {
                            namespace: "minecraft".to_owned(),
                            bulks: vec![
                                FabricRegistryBulk {
                                    first_raw_id: 0,
                                    paths: vec!["air".to_owned(), "stone".to_owned()],
                                },
                                // A gap: raw id 2 and 3 are unassigned.
                                FabricRegistryBulk {
                                    first_raw_id: 4,
                                    paths: vec!["oak_planks".to_owned()],
                                },
                            ],
                        }],
                    }],
                },
                FabricRegistryNamespace {
                    namespace: "create".to_owned(),
                    registries: vec![FabricRegistry {
                        path: "block".to_owned(),
                        id_namespaces: vec![FabricRegistryIdNamespace {
                            namespace: "create".to_owned(),
                            bulks: vec![FabricRegistryBulk {
                                first_raw_id: 0,
                                paths: vec!["shaft".to_owned()],
                            }],
                        }],
                    }],
                },
            ],
        }
    }

    #[test]
    fn fabric_registry_sync_golden_with_sentinel_and_deltas() {
        let payload = fabric_sample();
        let encoded = payload.encode().unwrap();
        assert_eq!(
            encoded,
            concat(&[
                vec![2],
                // `minecraft` is the empty string on the wire.
                utf(""),
                vec![1],
                utf("block"),
                vec![1],
                utf(""),
                vec![2],
                // Bulk 1 starts at raw id 0; bulk 2 jumps by 3 ids.
                vec![0, 2],
                utf("air"),
                utf("stone"),
                vec![3, 1],
                utf("oak_planks"),
                utf("create"),
                vec![1],
                utf("block"),
                vec![1],
                utf("create"),
                vec![1, 0, 1],
                utf("shaft"),
            ])
        );
        assert_eq!(
            FabricDirectRegistryPayload::decode(&encoded).unwrap(),
            payload
        );
        assert_eq!(
            payload.namespaces[0].registries[0].id_namespaces[0].bulks[1].last_raw_id(),
            4
        );

        // A second bulk that does not advance past the first is unencodable.
        let mut overlapping = fabric_sample();
        overlapping.namespaces[0].registries[0].id_namespaces[0].bulks[1].first_raw_id = 1;
        assert_eq!(
            overlapping.encode(),
            Err(LoaderCodecError::UnorderedBulk("overlapping raw ids"))
        );
        // ...and a zero delta is rejected on the wire.
        assert_eq!(
            FabricDirectRegistryPayload::decode(&concat(&[
                vec![1],
                utf(""),
                vec![1],
                utf("block"),
                vec![1],
                utf(""),
                vec![2],
                vec![0, 1],
                utf("air"),
                vec![0, 1],
                utf("stone"),
            ])),
            Err(LoaderCodecError::UnorderedBulk("overlapping raw ids"))
        );
        // An empty bulk is rejected on the wire.
        assert_eq!(
            FabricDirectRegistryPayload::decode(&concat(&[
                vec![1],
                utf(""),
                vec![1],
                utf("block"),
                vec![1],
                utf(""),
                vec![1],
                vec![0, 0],
            ])),
            Err(LoaderCodecError::UnorderedBulk("empty bulk"))
        );
        // Truncated bulk contents are rejected.
        assert_eq!(
            FabricDirectRegistryPayload::decode(&concat(&[
                vec![1],
                utf(""),
                vec![1],
                utf("block"),
                vec![1],
                utf(""),
                vec![2],
            ])),
            Err(LoaderCodecError::Wire(ProtoError::UnexpectedEof {
                needed: 1,
                remaining: 0,
            }))
        );
    }

    #[test]
    fn fabric_registry_sync_chunks_at_one_mib_and_terminates() {
        let bulks: Vec<FabricRegistryBulk> = (0..4)
            .map(|chunk| FabricRegistryBulk {
                first_raw_id: chunk * 8000,
                paths: (0..8000)
                    .map(|index| format!("{index:05}_padding_padding_padding_padding"))
                    .collect(),
            })
            .collect();
        let payload = FabricDirectRegistryPayload {
            namespaces: vec![FabricRegistryNamespace {
                namespace: "minecraft".to_owned(),
                registries: vec![FabricRegistry {
                    path: "item".to_owned(),
                    id_namespaces: vec![FabricRegistryIdNamespace {
                        namespace: "minecraft".to_owned(),
                        bulks,
                    }],
                }],
            }],
        };
        let chunks = payload.encode_chunks().unwrap();
        assert!(
            chunks.len() > 2,
            "expected several chunks, got {}",
            chunks.len()
        );
        assert_eq!(
            chunks.last(),
            Some(&Vec::new()),
            "stream ends with the zero-length terminator"
        );
        for chunk in &chunks[..chunks.len() - 1] {
            assert!(!chunk.is_empty());
            assert!(chunk.len() <= FABRIC_REGISTRY_SYNC_CHUNK_BYTES);
        }

        let mut reassembler = DirectRegistryReassembler::new();
        let mut decoded = None;
        for chunk in &chunks {
            match reassembler.push(chunk).expect("valid chunk") {
                Some(complete) => {
                    assert!(decoded.is_none(), "the terminator arrives once");
                    decoded = Some(complete);
                }
                None => assert!(decoded.is_none(), "no payload before the terminator"),
            }
        }
        assert_eq!(decoded, Some(payload));
        assert_eq!(reassembler.buffered(), 0);

        assert_eq!(
            DirectRegistryReassembler::new().push(&[]),
            Err(LoaderCodecError::EmptyRegistrySync)
        );
        let mut tiny = DirectRegistryReassembler::with_limit(4);
        assert_eq!(
            tiny.push(&[0u8; 5]),
            Err(LoaderCodecError::PayloadTooLarge("registry sync body"))
        );
        assert_eq!(tiny.buffered(), 0, "an over-limit body is released");
        let mut garbage = DirectRegistryReassembler::new();
        assert_eq!(garbage.push(&[0xff, 0xfe]), Ok(None));
        assert!(matches!(garbage.push(&[]), Err(LoaderCodecError::Wire(_))));
    }

    #[test]
    fn fabric_open_screen_golden() {
        let title =
            ModdedNetworkSetupFailedPayload::text_component_nbt("Blueprint").expect("ascii title");
        let payload = FabricOpenScreenPayload {
            screen_type: "create:blueprint".to_owned(),
            sync_id: 3,
            title_nbt: title.clone(),
            extra_data: vec![0x2a, 0x00],
        };
        assert_eq!(
            payload.encode().unwrap(),
            concat(&[
                utf("create:blueprint"),
                vec![3],
                vec![title.len() as u8],
                title,
                vec![2, 0x2a, 0x00],
            ])
        );
        assert_eq!(
            FabricOpenScreenPayload::decode(&payload.encode().unwrap()).unwrap(),
            payload
        );
        assert_eq!(
            FabricOpenScreenPayload::decode(&concat(&[utf(""), vec![3, 0, 0]])),
            Err(LoaderCodecError::InvalidResourceLocation(String::new()))
        );
    }

    fn registrations() -> Vec<LocalPayloadRegistration> {
        vec![
            LocalPayloadRegistration::required("c:version", "1", ConnectionProtocol::Configuration),
            LocalPayloadRegistration::required(
                "create:packets",
                "3",
                ConnectionProtocol::Configuration,
            ),
            LocalPayloadRegistration::optional(
                "megu:cosmetic",
                "2",
                ConnectionProtocol::Configuration,
            ),
        ]
    }

    fn query(components: Vec<ModdedNetworkQueryComponent>) -> ModdedNetworkQueryPayload {
        ModdedNetworkQueryPayload {
            groups: vec![ModdedNetworkQueryGroup {
                protocol: ConnectionProtocol::Configuration,
                components,
            }],
        }
    }

    #[test]
    fn negotiation_accepts_matching_channels() {
        let result = negotiate_neoforge_connection(
            registrations(),
            &CommonVersionPayload::supported(),
            &query(vec![
                ModdedNetworkQueryComponent::new("c:version", "1"),
                ModdedNetworkQueryComponent::new("create:packets", "3"),
                ModdedNetworkQueryComponent::new("other:extra", "1").optional(true),
            ]),
        )
        .expect("common version 1 is supported");
        assert!(result.is_accepted());
        assert_eq!(result.common_version, Some(1));
        assert_eq!(
            result.accepted_channels,
            vec!["c:version", "create:packets"]
        );
        assert_eq!(result.dropped_optional, vec!["other:extra"]);
        assert_eq!(
            result
                .setup
                .channel_version(ConnectionProtocol::Configuration, "create:packets"),
            Some("3")
        );
        // The negotiated setup is a byte-exact `neoforge:network` body.
        assert_eq!(
            result.setup.encode().unwrap(),
            concat(&[
                vec![1, 4, 2],
                utf("c:version"),
                utf("1"),
                utf("create:packets"),
                utf("3"),
            ])
        );
        assert_eq!(result.failure_reasons(), Vec::new());
    }

    #[test]
    fn negotiation_reports_version_mismatch_and_missing_required() {
        let result = negotiate_neoforge_connection(
            registrations(),
            &CommonVersionPayload::supported(),
            &query(vec![
                ModdedNetworkQueryComponent::new("c:version", "1"),
                ModdedNetworkQueryComponent::new("create:packets", "2"),
            ]),
        )
        .expect("common version 1 is supported");
        assert!(!result.is_accepted());
        assert_eq!(result.accepted_channels, vec!["c:version"]);
        let reasons = result.failure_reasons();
        assert_eq!(reasons.len(), 1);
        assert_eq!(reasons[0].0, "create");
        assert!(reasons[0].1.contains("version mismatch"));
        let failures = result.failures.expect("required mismatch fails");
        assert_eq!(failures.failures[0].namespace, "create");
        assert_eq!(
            ModdedNetworkSetupFailedPayload::decode(&failures.encode().unwrap()).unwrap(),
            failures
        );

        // No shared networking version is a hard disconnect.
        assert_eq!(
            negotiate_neoforge_connection(
                registrations(),
                &CommonVersionPayload { versions: vec![9] },
                &query(Vec::new()),
            ),
            Err(LoaderCodecError::UnsupportedCommonVersion)
        );
        // A required peer channel we lack fails, keyed by its namespace.
        let missing = negotiate_neoforge_connection(
            registrations(),
            &CommonVersionPayload::supported(),
            &query(vec![
                ModdedNetworkQueryComponent::new("c:version", "1"),
                ModdedNetworkQueryComponent::new("create:packets", "3"),
                ModdedNetworkQueryComponent::new("sodium:options", "1"),
            ]),
        )
        .expect("common version 1 is supported");
        assert!(!missing.is_accepted());
        assert_eq!(missing.failure_reasons()[0].0, "sodium");
        assert!(missing.failure_reasons()[0]
            .1
            .contains("not registered here"));

        // The reverse direction: a required local channel the peer never offered.
        let unoffered = negotiate_neoforge_connection(
            registrations(),
            &CommonVersionPayload::supported(),
            &query(vec![ModdedNetworkQueryComponent::new("c:version", "1")]),
        )
        .expect("common version 1 is supported");
        assert!(!unoffered.is_accepted());
        assert_eq!(unoffered.failure_reasons()[0].0, "create");
        assert!(unoffered.failure_reasons()[0]
            .1
            .contains("peer did not offer required payload"));

        // An optional local channel may be absent without failing.
        let optional_absent = negotiate_neoforge_connection(
            vec![LocalPayloadRegistration::optional(
                "create:packets",
                "3",
                ConnectionProtocol::Configuration,
            )],
            &CommonVersionPayload::supported(),
            &query(Vec::new()),
        )
        .expect("common version 1 is supported");
        assert!(optional_absent.is_accepted());
    }

    #[test]
    fn negotiation_rejects_duplicate_local_registrations() {
        let mut duplicated = registrations();
        duplicated.push(LocalPayloadRegistration::required(
            "c:version",
            "1",
            ConnectionProtocol::Configuration,
        ));
        assert_eq!(
            NeoForgeNegotiation::new(duplicated).err(),
            Some(LoaderCodecError::DuplicateEntry(
                "local payload registration"
            ))
        );
        assert_eq!(
            NeoForgeNegotiation::new(vec![LocalPayloadRegistration::required(
                "NotAChannel",
                "1",
                ConnectionProtocol::Play
            )])
            .err(),
            Some(LoaderCodecError::InvalidResourceLocation(
                "NotAChannel".to_owned()
            ))
        );
        let negotiation = NeoForgeNegotiation::new(registrations()).expect("valid registrations");
        assert_eq!(negotiation.common_version(), None);
        assert_eq!(
            negotiation
                .register_payload(ConnectionProtocol::Configuration)
                .expect("valid register body")
                .channels,
            vec!["c:version", "create:packets", "megu:cosmetic"]
        );
        assert!(negotiation
            .register_payload(ConnectionProtocol::Play)
            .expect("valid register body")
            .channels
            .is_empty());
    }

    #[test]
    fn loader_channel_table_matches_the_codec_constants() {
        for channel in [
            COMMON_VERSION_CHANNEL,
            COMMON_REGISTER_CHANNEL,
            NEOFORGE_REGISTER_CHANNEL,
            NEOFORGE_NETWORK_CHANNEL,
            NEOFORGE_SETUP_FAILED_CHANNEL,
            NEOFORGE_SPLIT_CHANNEL,
            NEOFORGE_CONFIG_FILE_CHANNEL,
            NEOFORGE_FROZEN_REGISTRY_CHANNEL,
            NEOFORGE_FROZEN_REGISTRY_SYNC_START_CHANNEL,
            NEOFORGE_FROZEN_REGISTRY_SYNC_COMPLETED_CHANNEL,
            FABRIC_REGISTER_CHANNEL,
            FABRIC_UNREGISTER_CHANNEL,
            FABRIC_REGISTRY_SYNC_DIRECT_CHANNEL,
            FABRIC_REGISTRY_SYNC_COMPLETE_CHANNEL,
            FABRIC_OPEN_SCREEN_CHANNEL,
        ] {
            assert!(
                is_loader_protocol_channel(channel),
                "{channel} must be a known loader channel"
            );
            assert!(
                validate_resource_location(channel).is_ok(),
                "{channel} must be a plain resource location"
            );
        }
        assert_eq!(
            LOADER_PROTOCOL_CHANNELS.len(),
            4 + 21 + 7,
            "vanilla/common + NeoForge + Fabric channel count"
        );
        assert!(!is_loader_protocol_channel("megu:season_v1"));
        assert_eq!(NEOFORGE_VERSION, "21.1.248");
        assert_eq!(FABRIC_API_VERSION, "0.116.15");
    }
}
