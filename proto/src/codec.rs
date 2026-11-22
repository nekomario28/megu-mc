// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
//! Canonical packet IR and the protocol-767 codec.
//!
//! [`S2CPacket`] and [`C2SPacket`] are the version-agnostic intermediate
//! representation: variants model *what* a packet says, and the codec maps
//! them to numeric ids through its [`ProtocolTable`] rather than embedding
//! constants. `0x71` never appears in an IR type — `TickingState` resolves to
//! whatever id the negotiated table assigns `play/clientbound/ticking_state`.
//!
//! [`CodecV767`] is the protocol-767 implementation of [`ProtocolCodec`]. Its
//! encoders are byte-exact with the ones scattered across `src/` today
//! (`ticking_state_packet`, `set_player_team_packet`, `encode_set_equipment`,
//! `encode_set_passengers`, the 0x33 window open, 0x6d tab list and the
//! vehicle/input decoders all produce the layouts used here), and its decoders
//! share mc-spine's fail-closed stance: trailing bytes, malformed varints,
//! over-long strings and unknown component types are [`ProtoError`]s, never
//! partial packets.
//!
//! Text components and registry-entry NBT ride the IR as raw anonymous-NBT
//! byte spans — the same bytes `chat_nbt`/registry writes emit — so this crate
//! stays byte-exact without growing an NBT model; consumers can decode spans
//! with any NBT layer they already have.

use crate::table::{PacketDir, PacketPhase, ProtocolTable};
use crate::version::{ProtocolVersion, VersionCapabilities};
use crate::wire::{
    read_frame, write_frame, BitSet, BlockPos, ProtoError, WireReader, WireWriter,
    MAX_STRING_CHARS,
};

/// Vanilla `Utf8String` bound for the team/packet names on the wire.
const MAX_NAME_CHARS: usize = 32_767;
/// Bound on registry-entry counts in `ClientboundRegistryDataPacket`.
const MAX_REGISTRY_ENTRIES: usize = 1 << 20;
/// Bound on component counts in an item stack patch.
const MAX_COMPONENTS: usize = 128;
/// Bound on entity-id lists in `ClientboundSetPassengersPacket`.
const MAX_PASSENGERS: usize = 1 << 15;
/// Bound on string lists in `ClientboundSetPlayerTeamPacket`.
const MAX_TEAM_MEMBERS: usize = 1 << 15;
/// Vanilla's hard cap for cookie payloads in every connection phase.
pub const MAX_COOKIE_PAYLOAD_BYTES: usize = 5_120;

/// A `minecraft:menu` resource, expressed semantically so codecs map it to a
/// numeric id through their [`ProtocolTable`] — the same enum yields different
/// wire ids under a different version's table.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum MenuKind {
    Generic9x1,
    Generic9x2,
    Generic9x3,
    Generic9x4,
    Generic9x5,
    Generic9x6,
    Generic3x3,
    Crafter3x3,
    Anvil,
    Beacon,
    BlastFurnace,
    BrewingStand,
    Crafting,
    Enchantment,
    Furnace,
    Grindstone,
    Hopper,
    Lectern,
    Loom,
    Merchant,
    ShulkerBox,
    Smithing,
    Smoker,
    CartographyTable,
    Stonecutter,
}

impl MenuKind {
    /// The `minecraft:menu` registry resource this kind resolves through.
    pub fn resource_name(self) -> &'static str {
        match self {
            Self::Generic9x1 => "minecraft:generic_9x1",
            Self::Generic9x2 => "minecraft:generic_9x2",
            Self::Generic9x3 => "minecraft:generic_9x3",
            Self::Generic9x4 => "minecraft:generic_9x4",
            Self::Generic9x5 => "minecraft:generic_9x5",
            Self::Generic9x6 => "minecraft:generic_9x6",
            Self::Generic3x3 => "minecraft:generic_3x3",
            Self::Crafter3x3 => "minecraft:crafter_3x3",
            Self::Anvil => "minecraft:anvil",
            Self::Beacon => "minecraft:beacon",
            Self::BlastFurnace => "minecraft:blast_furnace",
            Self::BrewingStand => "minecraft:brewing_stand",
            Self::Crafting => "minecraft:crafting",
            Self::Enchantment => "minecraft:enchantment",
            Self::Furnace => "minecraft:furnace",
            Self::Grindstone => "minecraft:grindstone",
            Self::Hopper => "minecraft:hopper",
            Self::Lectern => "minecraft:lectern",
            Self::Loom => "minecraft:loom",
            Self::Merchant => "minecraft:merchant",
            Self::ShulkerBox => "minecraft:shulker_box",
            Self::Smithing => "minecraft:smithing",
            Self::Smoker => "minecraft:smoker",
            Self::CartographyTable => "minecraft:cartography_table",
            Self::Stonecutter => "minecraft:stonecutter",
        }
    }

    /// Inverse of [`MenuKind::resource_name`]; `None` for menu resources this
    /// version's IR does not model (e.g. a menu added in a newer pack).
    fn from_resource_name(name: &str) -> Option<Self> {
        Some(match name {
            "minecraft:generic_9x1" => Self::Generic9x1,
            "minecraft:generic_9x2" => Self::Generic9x2,
            "minecraft:generic_9x3" => Self::Generic9x3,
            "minecraft:generic_9x4" => Self::Generic9x4,
            "minecraft:generic_9x5" => Self::Generic9x5,
            "minecraft:generic_9x6" => Self::Generic9x6,
            "minecraft:generic_3x3" => Self::Generic3x3,
            "minecraft:crafter_3x3" => Self::Crafter3x3,
            "minecraft:anvil" => Self::Anvil,
            "minecraft:beacon" => Self::Beacon,
            "minecraft:blast_furnace" => Self::BlastFurnace,
            "minecraft:brewing_stand" => Self::BrewingStand,
            "minecraft:crafting" => Self::Crafting,
            "minecraft:enchantment" => Self::Enchantment,
            "minecraft:furnace" => Self::Furnace,
            "minecraft:grindstone" => Self::Grindstone,
            "minecraft:hopper" => Self::Hopper,
            "minecraft:lectern" => Self::Lectern,
            "minecraft:loom" => Self::Loom,
            "minecraft:merchant" => Self::Merchant,
            "minecraft:shulker_box" => Self::ShulkerBox,
            "minecraft:smithing" => Self::Smithing,
            "minecraft:smoker" => Self::Smoker,
            "minecraft:cartography_table" => Self::CartographyTable,
            "minecraft:stonecutter" => Self::Stonecutter,
            _ => return None,
        })
    }
}

/// One item-stack component as it sits on the wire: a registry id plus the
/// already-encoded value bytes. Component *values* have type-specific shapes;
/// the IR keeps them raw so the same IR serves every version whose
/// `minecraft:data_component_type` table is loaded.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RawComponent {
    /// `minecraft:data_component_type` registry id (e.g. 5 = `custom_name`).
    pub type_id: u32,
    /// Serialized component payload, verbatim.
    pub data: Vec<u8>,
}

/// A protocol item stack (`VarInt count`, then `VarInt item`, then the
/// component patch `VarInt added, VarInt removed, added…, removed…`).
///
/// `count == 0` means the empty stack and carries no further bytes — the
/// `SlotDisplay` convention vanilla and mc-spine share.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ItemStack {
    /// `minecraft:item` registry id.
    pub item_id: u32,
    /// Stack count (1..=127 meaningful; 0 = empty stack).
    pub count: u8,
    /// Components added over the item's prototype, in wire order.
    pub added: Vec<RawComponent>,
    /// Prototype component ids removed, in wire order.
    pub removed: Vec<u32>,
}

impl ItemStack {
    /// An empty stack (`count == 0`).
    pub fn empty() -> Self {
        Self::default()
    }

    pub fn is_empty(&self) -> bool {
        self.count == 0
    }
}

/// One `(equipment slot, stack)` entry of `ClientboundSetEquipmentPacket`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EquipmentSlot {
    /// `EquipmentSlot` enum ordinal: 0 mainhand, 1 offhand, 2 feet, 3 legs,
    /// 4 chest, 5 head.
    pub slot: u8,
    pub item: ItemStack,
}

/// One `LoginSuccess` profile property.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LoginProperty {
    pub name: String,
    pub value: String,
    /// `Some(sig)` writes the signature flag true plus the signature string.
    pub signature: Option<String>,
}

/// The `ClientboundSetPlayerTeamPacket` method byte, modelled semantically.
/// `display_name`/`prefix`/`suffix` are anonymous-NBT component spans (the
/// bytes `chat_nbt` produces).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TeamOperation {
    /// Mode 0 — full parameter block plus the initial member list.
    Create { info: TeamInfo, members: Vec<String> },
    /// Mode 1 — remove the team entirely.
    Remove,
    /// Mode 2 — new parameter block, members unchanged.
    UpdateInfo { info: TeamInfo },
    /// Mode 3 — entities join the team.
    AddMembers { members: Vec<String> },
    /// Mode 4 — entities leave the team.
    RemoveMembers { members: Vec<String> },
}

/// The parameter block modes 0 and 2 carry: the fields vanilla's
/// `PlayerTeam.Pack` serializes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TeamInfo {
    /// Anonymous-NBT chat component bytes.
    pub display_name: Vec<u8>,
    /// Bit 0 friendly fire, bit 1 see friendly invisibles.
    pub friendly_flags: u8,
    /// `always`/`hide_for_other_teams`/`hide_for_own_team`/`never`.
    pub nametag_visibility: String,
    /// `always`/`push_own_team`/`push_other_teams`/`never`.
    pub collision_rule: String,
    /// `ChatFormatting` id, or -1 for reset.
    pub color: i32,
    /// Anonymous-NBT chat component bytes.
    pub prefix: Vec<u8>,
    /// Anonymous-NBT chat component bytes.
    pub suffix: Vec<u8>,
}

/// One entry of `ClientboundRegistryDataPacket`: the holder id plus the
/// optional serialized value (anonymous NBT span when present).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RegistryDataEntry {
    pub entry_id: String,
    /// `None` encodes the "no data" boolean flag.
    pub data: Option<Vec<u8>>,
}

/// Canonical clientbound IR. Each variant names the *semantic* packet; its
/// phase, wire name and id resolve through the codec's [`ProtocolTable`], so
/// the same IR describes every protocol version whose table is loaded.
#[derive(Clone, Debug, PartialEq)]
pub enum S2CPacket {
    /// Login-phase clientbound cookie request.
    LoginCookieRequest { key: String },
    /// Configuration-phase clientbound cookie request.
    ConfigCookieRequest { key: String },
    /// Play-phase clientbound cookie request.
    PlayCookieRequest { key: String },
    /// Configuration-phase cookie storage instruction.
    ConfigStoreCookie { key: String, payload: Vec<u8> },
    /// Play-phase cookie storage instruction.
    PlayStoreCookie { key: String, payload: Vec<u8> },
    /// Configuration-phase server transfer instruction.
    ConfigTransfer { host: String, port: i32 },
    /// Play-phase server transfer instruction.
    PlayTransfer { host: String, port: i32 },
    // -- login phase --
    /// `login/clientbound/login_disconnect` (0x00) — reason component bytes.
    LoginDisconnect { reason: Vec<u8> },
    /// `login/clientbound/encryption_request` (0x01).
    EncryptionRequest {
        /// Vanilla always sends the empty string.
        server_id: String,
        /// `VarInt` length-prefixed DER public key.
        public_key: Vec<u8>,
        /// `VarInt` length-prefixed challenge token.
        verify_token: Vec<u8>,
        should_authenticate: bool,
    },
    /// `login/clientbound/login_success` (0x02).
    LoginSuccess {
        uuid: [u8; 16],
        username: String,
        properties: Vec<LoginProperty>,
    },
    /// `login/clientbound/set_compression` (0x03).
    SetCompression { threshold: i32 },
    // -- configuration phase --
    /// `configuration/clientbound/disconnect` — reason component bytes
    /// (anonymous NBT, the same wire name the play phase reuses under
    /// [`S2CPacket::PlayDisconnect`]).
    ConfigDisconnect { reason: Vec<u8> },
    /// `configuration/clientbound/keep_alive` (0x04).
    ConfigKeepAlive { id: i64 },
    /// `configuration/clientbound/registry_data` (0x07 in 767).
    ConfigRegistryData {
        /// e.g. `"minecraft:dimension_type"`.
        registry_id: String,
        entries: Vec<RegistryDataEntry>,
    },
    /// `configuration/clientbound/finish_configuration` (0x03).
    ConfigFinishConfiguration,
    // -- play phase --
    /// `play/clientbound/disconnect` — reason component bytes (anonymous NBT).
    PlayDisconnect { reason: Vec<u8> },
    /// `play/clientbound/keep_alive` (0x26).
    KeepAlive { id: i64 },
    /// `play/clientbound/block_update` — packed [`BlockPos`] + `VarInt`
    /// global block-state id.
    BlockUpdate { pos: BlockPos, state_id: u32 },
    /// `play/clientbound/forget_level_chunk` — the packed-`i64` `ChunkPos`
    /// (`z << 32 | (x as u32)`), unloaded without a light payload.
    ForgetLevelChunk { cx: i32, cz: i32 },
    /// `play/clientbound/light_update` (0x2a) — chunk coords plus the four
    /// section masks and light arrays. The masks are full multi-word
    /// [`BitSet`]s: a single-`u64` mask silently corrupts heights past 64
    /// sections, which is exactly the version-sensitive shape this packet
    /// exists to carry.
    LightUpdateMask {
        chunk_x: i32,
        chunk_z: i32,
        sky_mask: BitSet,
        block_mask: BitSet,
        empty_sky_mask: BitSet,
        empty_block_mask: BitSet,
        /// 2048-byte nibble arrays, one per set bit of `sky_mask`, in order.
        sky_light: Vec<Vec<u8>>,
        /// One per set bit of `block_mask`.
        block_light: Vec<Vec<u8>>,
    },
    /// `play/clientbound/open_screen` (0x33): `VarInt containerId`,
    /// `VarInt menuType` resolved from [`MenuKind`] through the table, then
    /// the title component bytes.
    OpenScreen {
        container_id: i32,
        menu: MenuKind,
        /// Anonymous-NBT chat component bytes.
        title: Vec<u8>,
    },
    /// `play/clientbound/set_equipment` (0x5b). At least one entry; the
    /// 0x80 "more entries follow" bit on each slot byte is derived from
    /// position, matching `encode_set_equipment`.
    SetEquipment {
        entity_id: i32,
        slots: Vec<EquipmentSlot>,
    },
    /// `play/clientbound/set_passengers` (0x5f).
    SetPassengers { vehicle: i32, passengers: Vec<i32> },
    /// `play/clientbound/set_player_team` (0x60).
    SetPlayerTeam { name: String, operation: TeamOperation },
    /// `play/clientbound/tab_list` (0x6d): header and footer components as
    /// anonymous-NBT spans.
    TabList { header: Vec<u8>, footer: Vec<u8> },
    /// `play/clientbound/ticking_state` (0x71): BE f32 rate + frozen bool.
    TickingState { tick_rate: f32, is_frozen: bool },
    /// `play/clientbound/ticking_step` (0x72): `VarInt` step count.
    TickingStep { steps: i32 },
}

/// Canonical serverbound IR — same contract as [`S2CPacket`].
#[derive(Clone, Debug, PartialEq)]
pub enum C2SPacket {
    /// Login-phase client cookie response.
    LoginCookieResponse { key: String, payload: Option<Vec<u8>> },
    /// Configuration-phase client cookie response.
    ConfigCookieResponse { key: String, payload: Option<Vec<u8>> },
    /// Play-phase client cookie response.
    PlayCookieResponse { key: String, payload: Option<Vec<u8>> },
    /// `handshake/serverbound/handshake` (0x00). `next_state` must be 1
    /// (status) or 2 (login); anything else fails closed like
    /// `handle_handshake`.
    Handshake {
        protocol_version: i32,
        server_address: String,
        server_port: u16,
        next_state: i32,
    },
    /// `login/serverbound/login_start` (0x00): name (16 chars) + UUID.
    LoginStart { name: String, uuid: [u8; 16] },
    /// `login/serverbound/encryption_response` (0x01).
    EncryptionResponse {
        shared_secret: Vec<u8>,
        verify_token: Vec<u8>,
    },
    /// `login/serverbound/login_acknowledged` (0x03) — the transition into
    /// configuration phase.
    LoginAcknowledged,
    /// `configuration/serverbound/keep_alive` (0x04).
    ConfigKeepAlive { id: i64 },
    /// `configuration/serverbound/finish_configuration` (0x03).
    ConfigFinishConfiguration,
    /// `play/serverbound/move_vehicle` (0x1e): f64 xyz + f32 yaw/pitch.
    /// Encoding non-finite values is rejected (vanilla disconnects on them).
    MoveVehicle {
        x: f64,
        y: f64,
        z: f64,
        yaw: f32,
        pitch: f32,
    },
    /// `play/serverbound/paddle_boat` (0x1f): left/right paddle flags.
    PaddleBoat { left: bool, right: bool },
    /// `play/serverbound/keep_alive` (0x18).
    KeepAlive { id: i64 },
    /// `play/serverbound/player_input` (0x26): f32 sideways/forward impulses
    /// plus a flag byte (bit0 jump, bit1 unmount).
    PlayerInput {
        sideways: f32,
        forward: f32,
        jump: bool,
        unmount: bool,
    },
    /// `play/serverbound/player_input` in its 1.21.2+ form: vanilla replaced
    /// the two f32 impulses with an `Input` record serialized as a single
    /// byte of flags (`0x01` forward, `0x02` backward, `0x04` left, `0x08`
    /// right, `0x10` jump, `0x20` shift, `0x40` sprint). A codec produces
    /// this variant when [`VersionCapabilities::player_input_flags`] is set.
    /// The float [`C2SPacket::PlayerInput`] still encodes under that
    /// capability by quantizing each impulse to its sign (sprint has no
    /// float-form source); this variant encodes under the float form as the
    /// vanilla ±1.0 magnitudes with `shift` mapped to `unmount` and `sprint`
    /// dropped — the two wire shapes cannot express each other's full range.
    PlayerInputFlags {
        forward: bool,
        backward: bool,
        left: bool,
        right: bool,
        jump: bool,
        shift: bool,
        sprint: bool,
    },
}

/// Version-agnostic codec contract: encode/decode the canonical IR against
/// the codec's [`ProtocolTable`].
///
/// `frame` is the complete on-the-wire frame — `VarInt(len)` +
/// `VarInt(id)` + payload — as produced by `send_packet`. Decoders require the
/// declared length to match the buffer exactly and the payload to be consumed
/// completely: any surplus or shortfall fails closed.
///
/// `Send + Sync` so a [`crate::router::VersionRouter`]-selected codec can be
/// handed to a connection thread.
pub trait ProtocolCodec: Send + Sync {
    /// Structured facts about the version this codec speaks.
    fn capabilities(&self) -> &VersionCapabilities;
    /// The id table ids resolve through.
    fn table(&self) -> &ProtocolTable;
    /// `frame = VarInt(len) + VarInt(id) + payload` for `packet`.
    fn encode_s2c(&self, packet: &S2CPacket, out: &mut Vec<u8>) -> Result<(), ProtoError>;
    /// Parse one clientbound frame in the codec's current phase.
    fn decode_s2c(&self, frame: &[u8]) -> Result<S2CPacket, ProtoError>;
    fn encode_c2s(&self, packet: &C2SPacket, out: &mut Vec<u8>) -> Result<(), ProtoError>;
    /// Parse one serverbound frame in the codec's current phase.
    fn decode_c2s(&self, frame: &[u8]) -> Result<C2SPacket, ProtoError>;
}

// ---------------------------------------------------------------------------
// Shared body encoders/decoders (payload only; ids live in the table)
//
// These free functions are the whole wire engine: `CodecV767` and
// `crate::router::TableDrivenCodec` are both thin shells over them, so packet
// bodies are written exactly once. Ids and menu/component registries resolve
// through the `ProtocolTable` argument; wire-shape deltas resolve through the
// `VersionCapabilities` argument — never a raw protocol number.
// ---------------------------------------------------------------------------

/// The 1.21.2+ `Input` record's flag byte layout (vanilla's
/// `Input.STREAM_CODEC` packs the seven booleans into one byte; bit 7 is
/// reserved).
const INPUT_FORWARD: u8 = 0x01;
const INPUT_BACKWARD: u8 = 0x02;
const INPUT_LEFT: u8 = 0x04;
const INPUT_RIGHT: u8 = 0x08;
const INPUT_JUMP: u8 = 0x10;
const INPUT_SHIFT: u8 = 0x20;
const INPUT_SPRINT: u8 = 0x40;

fn validate_cookie_key(key: &str) -> Result<(), ProtoError> {
    let (namespace, path) = key.split_once(':').unwrap_or(("minecraft", key));
    let valid_namespace = !namespace.is_empty()
        && namespace.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"_.-".contains(&b));
    let valid_path = !path.is_empty()
        && path.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"_./-".contains(&b));
    if !valid_namespace || !valid_path || key.len() > MAX_STRING_CHARS {
        return Err(ProtoError::Malformed("invalid cookie resource location"));
    }
    Ok(())
}

fn validate_cookie_payload(payload: &[u8]) -> Result<(), ProtoError> {
    if payload.len() > MAX_COOKIE_PAYLOAD_BYTES {
        return Err(ProtoError::LengthOutOfBounds {
            length: payload.len(),
            max: MAX_COOKIE_PAYLOAD_BYTES,
        });
    }
    Ok(())
}

fn validate_transfer(host: &str, port: i32) -> Result<(), ProtoError> {
    let units = host.encode_utf16().count();
    if units == 0 || units > 255 || host.chars().any(|c| c.is_control() || c.is_whitespace() || "/\\@?#".contains(c)) {
        return Err(ProtoError::Malformed("invalid transfer host"));
    }
    if !(1..=65535).contains(&port) {
        return Err(ProtoError::InvalidDiscriminant {
            value: i64::from(port),
            what: "transfer port",
        });
    }
    Ok(())
}

fn write_cookie_request(w: &mut WireWriter, key: &str) -> Result<(), ProtoError> {
    validate_cookie_key(key)?;
    w.write_string(key);
    Ok(())
}

fn write_store_cookie(w: &mut WireWriter, key: &str, payload: &[u8]) -> Result<(), ProtoError> {
    validate_cookie_key(key)?;
    validate_cookie_payload(payload)?;
    w.write_string(key);
    w.write_bytes(payload);
    Ok(())
}

fn write_transfer(w: &mut WireWriter, host: &str, port: i32) -> Result<(), ProtoError> {
    validate_transfer(host, port)?;
    w.write_string(host);
    w.write_varint(port);
    Ok(())
}

fn write_cookie_response(w: &mut WireWriter, key: &str, payload: &Option<Vec<u8>>) -> Result<(), ProtoError> {
    validate_cookie_key(key)?;
    if let Some(bytes) = payload {
        validate_cookie_payload(bytes)?;
    }
    w.write_string(key);
    w.write_bool(payload.is_some());
    if let Some(bytes) = payload {
        w.write_bytes(bytes);
    }
    Ok(())
}

fn read_cookie_key(r: &mut WireReader<'_>) -> Result<String, ProtoError> {
    let key = r.read_string(MAX_STRING_CHARS)?;
    validate_cookie_key(&key)?;
    Ok(key)
}

fn read_store_cookie(r: &mut WireReader<'_>) -> Result<(String, Vec<u8>), ProtoError> {
    let key = read_cookie_key(r)?;
    let payload = r.read_bytes(MAX_COOKIE_PAYLOAD_BYTES)?.to_vec();
    Ok((key, payload))
}

fn read_transfer(r: &mut WireReader<'_>) -> Result<(String, i32), ProtoError> {
    let host = r.read_string(255)?;
    let port = r.read_varint()?;
    validate_transfer(&host, port)?;
    Ok((host, port))
}

fn read_cookie_response(r: &mut WireReader<'_>) -> Result<(String, Option<Vec<u8>>), ProtoError> {
    let key = read_cookie_key(r)?;
    let payload = if r.read_bool()? {
        Some(r.read_bytes(MAX_COOKIE_PAYLOAD_BYTES)?.to_vec())
    } else {
        None
    };
    Ok((key, payload))
}

/// Quantize the pre-1.21.2 float impulses into the `Input` flag byte. The
/// signs follow `KeyboardInput`: a positive sideways impulse is the left key,
/// a positive forward impulse is forward. Non-finite inputs encode as 0 —
/// the same "don't propagate NaN" stance the decoders take.
fn input_flags_byte(sideways: f32, forward: f32, jump: bool, unmount: bool) -> u8 {
    let mut bits = 0u8;
    if forward > 0.0 {
        bits |= INPUT_FORWARD;
    }
    if forward < 0.0 {
        bits |= INPUT_BACKWARD;
    }
    if sideways > 0.0 {
        bits |= INPUT_LEFT;
    }
    if sideways < 0.0 {
        bits |= INPUT_RIGHT;
    }
    if jump {
        bits |= INPUT_JUMP;
    }
    if unmount {
        bits |= INPUT_SHIFT;
    }
    bits
}

/// The `Input` record's impulse magnitudes when downgraded to the pre-1.21.2
/// float form: each boolean pair becomes ±1.0, the values `KeyboardInput`
/// itself produced.
fn input_flags_impulse(positive: bool, negative: bool) -> f32 {
    f32::from(i8::from(positive) - i8::from(negative))
}

fn write_item_stack(w: &mut WireWriter, stack: &ItemStack) {
    w.write_varint(i32::from(stack.count));
    if stack.count == 0 {
        return;
    }
    w.write_varint(stack.item_id as i32);
    w.write_varint(stack.added.len() as i32);
    w.write_varint(stack.removed.len() as i32);
    for comp in &stack.added {
        w.write_varint(comp.type_id as i32);
        w.raw(&comp.data);
    }
    for &id in &stack.removed {
        w.write_varint(id as i32);
    }
}

/// Consume one wire item stack. Component payloads have no length prefix, so
/// boundaries can only be recovered for the component shapes mc-spine
/// understands — the same accepted-type table `slot_read_wire`/`wm_stack_decode`
/// enforce. Unknown types fail the whole stack rather than desynchronizing the
/// entry list.
///
/// The *type* of each component resolves through `table`'s
/// `minecraft:data_component_type` registry, so a renumbered 1.21.4 table
/// decodes the same names under different ids; a name the codec has no shape
/// for fails closed.
fn read_item_stack(r: &mut WireReader<'_>, table: &ProtocolTable) -> Result<ItemStack, ProtoError> {
    let is_v767 = table.protocol_version() == Some(767);
    let count = r.read_varint()?;
    if !(0..=127).contains(&count) {
        return Err(ProtoError::InvalidDiscriminant {
            value: i64::from(count),
            what: "item stack count",
        });
    }
    if count == 0 {
        return Ok(ItemStack::empty());
    }
    let item_id = r.read_varint()?;
    if item_id < 0 {
        return Err(ProtoError::InvalidDiscriminant {
            value: i64::from(item_id),
            what: "item id",
        });
    }
    let max_components = if is_v767 {
        crate::item_components::MAX_LIST_ENTRIES
    } else {
        MAX_COMPONENTS
    };
    let n_add = r.read_count(max_components)?;
    let n_rem = r.read_count(max_components)?;
    let mut total_component_bytes = 0usize;
    let mut seen = std::collections::HashSet::new();
    let mut added = Vec::with_capacity(n_add.min(16));
    for _ in 0..n_add {
        let type_id = r.read_varint()?;
        if type_id < 0 {
            return Err(ProtoError::InvalidDiscriminant {
                value: i64::from(type_id),
                what: "component type",
            });
        }
        let type_name = table
            .registry_name("minecraft:data_component_type", type_id as u32)
            .ok_or(ProtoError::InvalidDiscriminant {
                value: i64::from(type_id),
                what: "component type",
            })?;
        if is_v767 && !seen.insert(type_id as u32) {
            return Err(ProtoError::Malformed("duplicate item stack component"));
        }
        let component = if is_v767 && crate::item_components::is_verified_component_type(type_id as u32) {
            crate::item_components::read_component_from_wire(type_id as u32, r)?
        } else {
            let start = r.position();
            skip_component_value(r, type_id as u32, type_name)?;
            RawComponent {
                type_id: type_id as u32,
                data: r.buffer()[start..r.position()].to_vec(),
            }
        };
        if is_v767 {
            if component.data.len() > crate::item_components::MAX_COMPONENT_BYTES {
                return Err(ProtoError::LengthOutOfBounds {
                    length: component.data.len(),
                    max: crate::item_components::MAX_COMPONENT_BYTES,
                });
            }
            total_component_bytes += crate::wire::varint_len(type_id) + component.data.len();
            if total_component_bytes > crate::item_components::MAX_STACK_COMPONENTS_BYTES {
                return Err(ProtoError::LengthOutOfBounds {
                    length: total_component_bytes,
                    max: crate::item_components::MAX_STACK_COMPONENTS_BYTES,
                });
            }
        }
        added.push(component);
    }
    let mut removed = Vec::with_capacity(n_rem.min(16));
    for _ in 0..n_rem {
        let id = r.read_varint()?;
        if id < 0 {
            return Err(ProtoError::InvalidDiscriminant {
                value: i64::from(id),
                what: "removed component type",
            });
        }
        if is_v767 {
            if table.registry_name("minecraft:data_component_type", id as u32).is_none()
                || !seen.insert(id as u32)
            {
                return Err(ProtoError::Malformed("invalid or duplicate removed item component"));
            }
            total_component_bytes += crate::wire::varint_len(id);
            if total_component_bytes > crate::item_components::MAX_STACK_COMPONENTS_BYTES {
                return Err(ProtoError::LengthOutOfBounds {
                    length: total_component_bytes,
                    max: crate::item_components::MAX_STACK_COMPONENTS_BYTES,
                });
            }
        }
        removed.push(id as u32);
    }
    Ok(ItemStack {
        item_id: item_id as u32,
        count: count as u8,
        added,
        removed,
    })
}

/// Skip one component *value* for the shapes the canonical codec understands:
/// the exact accepted set of `slot_read_wire` (protocol 767). Values use the
/// vanilla `DataComponentType` stream encodings. Keyed by resource name so
/// any version's renumbered registry decodes the same shapes; an
/// unrecognized name fails closed on its numeric id.
fn skip_component_value(
    r: &mut WireReader<'_>,
    type_id: u32,
    type_name: &str,
) -> Result<(), ProtoError> {
    match type_name {
        // max_stack_size / max_damage / damage / repair_cost: VarInt.
        "minecraft:max_stack_size"
        | "minecraft:max_damage"
        | "minecraft:damage"
        | "minecraft:repair_cost" => {
            r.read_varint()?;
        }
        // unbreakable: strict bool byte.
        "minecraft:unbreakable" => {
            r.read_bool()?;
        }
        // custom_name / item_name: anonymous NBT holding a string component.
        "minecraft:custom_name" | "minecraft:item_name" => {
            let span = r.read_anonymous_nbt()?;
            // The codec stores *components as NBT*; the span is validated but
            // not interpreted.
            let _ = span;
        }
        // lore: VarInt count of anonymous-NBT components.
        "minecraft:lore" => {
            let n = r.read_count(MAX_COMPONENTS)?;
            for _ in 0..n {
                r.read_anonymous_nbt()?;
            }
        }
        // enchantments / stored_enchantments: VarInt count of
        // {VarInt enchantment, VarInt level} plus a show-in-tooltip bool.
        "minecraft:enchantments" | "minecraft:stored_enchantments" => {
            let n = r.read_count(256)?;
            for _ in 0..n {
                r.read_varint()?;
                r.read_varint()?;
            }
            r.read_bool()?;
        }
        // potion_contents: optional VarInt potion, optional i32 custom color,
        // VarInt count of {VarInt effect, MobEffectInstance details}.
        "minecraft:potion_contents" => {
            if r.read_bool()? {
                r.read_varint()?;
            }
            if r.read_bool()? {
                r.take(4)?;
            }
            let effects = r.read_count(256)?;
            for _ in 0..effects {
                r.read_varint()?; // effect registry id
                skip_effect_details(r, 0)?;
            }
        }
        // trim: two registry holders (VarInt id each; a `0` holder would mean
        // "by name" which this codec's accepted set rejects like mc-spine) and
        // a show-in-tooltip bool.
        "minecraft:trim" => {
            let material = r.read_varint()?;
            let pattern = r.read_varint()?;
            if material == 0 || pattern == 0 {
                return Err(ProtoError::Malformed("trim holder is not a registry id"));
            }
            r.read_bool()?;
        }
        _ => {
            return Err(ProtoError::UnknownPacketId {
                id: type_id as i32,
                context: "data component value",
            })
        }
    }
    Ok(())
}

/// `MobEffectInstance` details: amplifier, duration, three bools, optional
/// nested details (the `hidden_effect` field), bounded at the same depth
/// mc-spine's `read_live_potion_effect_details` uses.
fn skip_effect_details(r: &mut WireReader<'_>, depth: usize) -> Result<(), ProtoError> {
    if depth == 16 {
        return Err(ProtoError::Malformed("effect details too deep"));
    }
    r.read_varint()?; // amplifier
    r.read_varint()?; // duration
    for _ in 0..3 {
        r.read_bool()?; // ambient, show_particles, show_icon
    }
    if r.read_bool()? {
        skip_effect_details(r, depth + 1)?;
    }
    Ok(())
}

fn write_team_info(w: &mut WireWriter, info: &TeamInfo) {
    w.raw(&info.display_name);
    w.write_u8(info.friendly_flags);
    w.write_string(&info.nametag_visibility);
    w.write_string(&info.collision_rule);
    w.write_varint(info.color);
    w.raw(&info.prefix);
    w.raw(&info.suffix);
}

fn read_team_info(r: &mut WireReader<'_>) -> Result<TeamInfo, ProtoError> {
    let display_name = r.read_anonymous_nbt()?.to_vec();
    // Any byte is a valid flag mask on the wire — same permissive read as
    // decode_set_player_team (bits beyond the defined two are reserved).
    let friendly_flags = r.read_u8()?;
    let nametag_visibility = r.read_string(MAX_NAME_CHARS)?;
    let collision_rule = r.read_string(MAX_NAME_CHARS)?;
    let color = r.read_varint()?;
    let prefix = r.read_anonymous_nbt()?.to_vec();
    let suffix = r.read_anonymous_nbt()?.to_vec();
    Ok(TeamInfo {
        display_name,
        friendly_flags,
        nametag_visibility,
        collision_rule,
        color,
        prefix,
        suffix,
    })
}

fn write_string_list(w: &mut WireWriter, members: &[String]) {
    w.write_varint(members.len() as i32);
    for m in members {
        w.write_string(m);
    }
}

fn read_string_list(r: &mut WireReader<'_>, max: usize) -> Result<Vec<String>, ProtoError> {
    let n = r.read_count(max)?;
    let mut out = Vec::with_capacity(n.min(256));
    for _ in 0..n {
        out.push(r.read_string(MAX_NAME_CHARS)?);
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// CodecV767
// ---------------------------------------------------------------------------

/// Protocol 767 (Minecraft 1.21.1) codec over [`ProtocolTable::v767_builtin`].
///
/// The codec tracks a connection phase (starting at
/// [`PacketPhase::Handshake`]); packet ids resolve against the current phase
/// at decode time because ids like `0x00` repeat across phases. Encode is
/// phase-independent — each IR variant carries its own phase.
#[derive(Debug)]
pub struct CodecV767 {
    table: ProtocolTable,
    capabilities: VersionCapabilities,
    phase: PacketPhase,
}

impl Default for CodecV767 {
    fn default() -> Self {
        Self::new()
    }
}

impl CodecV767 {
    pub fn new() -> Self {
        Self {
            table: ProtocolTable::v767_builtin(),
            capabilities: ProtocolVersion::V1_21_1.capabilities(),
            phase: PacketPhase::Handshake,
        }
    }

    /// The phase the next [`ProtocolCodec::decode_s2c`]/`decode_c2s` resolves
    /// packet ids in.
    pub fn phase(&self) -> PacketPhase {
        self.phase
    }

    /// Advance the phase after a transition packet (handshake → login,
    /// login-ack → configuration, finish → play).
    pub fn set_phase(&mut self, phase: PacketPhase) {
        self.phase = phase;
    }

    /// Decode one clientbound frame as `phase` without changing codec state.
    pub fn decode_s2c_in(&self, phase: PacketPhase, frame: &[u8]) -> Result<S2CPacket, ProtoError> {
        decode_s2c_in(&self.capabilities, &self.table, phase, frame)
    }

    /// Decode one serverbound frame as `phase` without changing codec state.
    pub fn decode_c2s_in(&self, phase: PacketPhase, frame: &[u8]) -> Result<C2SPacket, ProtoError> {
        decode_c2s_in(&self.table, &self.capabilities, phase, frame)
    }
}

/// Decode one clientbound frame as `phase` against `table` — the shared
/// engine behind `CodecV767::decode_s2c_in` and `TableDrivenCodec`. `caps`
/// switches the version-sensitive wire shapes (e.g. the 1.21.5+ byte-array
/// light masks).
pub(crate) fn decode_s2c_in(
    caps: &VersionCapabilities,
    table: &ProtocolTable,
    phase: PacketPhase,
    frame: &[u8],
) -> Result<S2CPacket, ProtoError> {
    let (id, body) = read_frame(frame)?;
    let name = table
        .packet_name(phase, PacketDir::Clientbound, id as u32)
        .ok_or(ProtoError::UnknownPacketId {
            id,
            context: "s2c packet",
        })?;
    let mut r = WireReader::new(body);
    let packet = read_s2c(caps, table, phase, name, &mut r)?;
    r.finish()?;
    Ok(packet)
}

/// Decode one serverbound frame as `phase` against `table`, switching
/// wire-shape deltas on `caps`.
pub(crate) fn decode_c2s_in(
    table: &ProtocolTable,
    caps: &VersionCapabilities,
    phase: PacketPhase,
    frame: &[u8],
) -> Result<C2SPacket, ProtoError> {
    let (id, body) = read_frame(frame)?;
    let name = table
        .packet_name(phase, PacketDir::Serverbound, id as u32)
        .ok_or(ProtoError::UnknownPacketId {
            id,
            context: "c2s packet",
        })?;
    let mut r = WireReader::new(body);
    let packet = read_c2s(caps, phase, name, &mut r)?;
    r.finish()?;
    Ok(packet)
}

/// Emit `body` under the wire `name` registered in `(phase, dir)`;
/// unknown names (a table lacking the packet) fail closed.
pub(crate) fn framed(
    table: &ProtocolTable,
    phase: PacketPhase,
    dir: PacketDir,
    name: &'static str,
    body: &[u8],
    out: &mut Vec<u8>,
) -> Result<(), ProtoError> {
    let id = table
        .packet_id(phase, dir, name)
        .ok_or(ProtoError::UnknownPacketId {
            id: -1,
            context: "table lookup for packet name",
        })?;
    write_frame(out, id as i32, body);
    Ok(())
}

/// Append `packet`'s body to `out` and report its `(phase, name)`. Callers
/// that already hold a reusable buffer never pay a body allocation.
pub(crate) fn encode_s2c_body_into(
    caps: &VersionCapabilities,
    table: &ProtocolTable,
    packet: &S2CPacket,
    out: &mut Vec<u8>,
) -> Result<(PacketPhase, &'static str), ProtoError> {
    let mut w = WireWriter::from_vec(std::mem::take(out));
    let result = encode_s2c_body_write(caps, table, packet, &mut w);
    *out = w.into_vec();
    result
}

fn encode_s2c_body_write(
    caps: &VersionCapabilities,
    table: &ProtocolTable,
    packet: &S2CPacket,
    w: &mut WireWriter,
) -> Result<(PacketPhase, &'static str), ProtoError> {
    let (phase, name) = match packet {
        S2CPacket::LoginCookieRequest { key } => {
            write_cookie_request(w, key)?;
            (PacketPhase::Login, "cookie_request")
        }
        S2CPacket::ConfigCookieRequest { key } => {
            write_cookie_request(w, key)?;
            (PacketPhase::Configuration, "cookie_request")
        }
        S2CPacket::PlayCookieRequest { key } => {
            write_cookie_request(w, key)?;
            (PacketPhase::Play, "cookie_request")
        }
        S2CPacket::ConfigStoreCookie { key, payload } => {
            write_store_cookie(w, key, payload)?;
            (PacketPhase::Configuration, "store_cookie")
        }
        S2CPacket::PlayStoreCookie { key, payload } => {
            write_store_cookie(w, key, payload)?;
            (PacketPhase::Play, "store_cookie")
        }
        S2CPacket::ConfigTransfer { host, port } => {
            write_transfer(w, host, *port)?;
            (PacketPhase::Configuration, "transfer")
        }
        S2CPacket::PlayTransfer { host, port } => {
            write_transfer(w, host, *port)?;
            (PacketPhase::Play, "transfer")
        }
        S2CPacket::LoginDisconnect { reason } => {
            w.raw(reason);
            (PacketPhase::Login, "login_disconnect")
        }
        S2CPacket::EncryptionRequest {
            server_id,
            public_key,
            verify_token,
            should_authenticate,
        } => {
            w.write_string(server_id);
            w.write_bytes(public_key);
            w.write_bytes(verify_token);
            w.write_bool(*should_authenticate);
            (PacketPhase::Login, "encryption_request")
        }
        S2CPacket::LoginSuccess {
            uuid,
            username,
            properties,
        } => {
            w.write_uuid(uuid);
            w.write_string(username);
            w.write_varint(properties.len() as i32);
            for p in properties {
                w.write_string(&p.name);
                w.write_string(&p.value);
                w.write_bool(p.signature.is_some());
                if let Some(sig) = &p.signature {
                    w.write_string(sig);
                }
            }
            (PacketPhase::Login, "login_success")
        }
        S2CPacket::SetCompression { threshold } => {
            w.write_varint(*threshold);
            (PacketPhase::Login, "set_compression")
        }
        S2CPacket::ConfigDisconnect { reason } => {
            w.raw(reason);
            (PacketPhase::Configuration, "disconnect")
        }
        S2CPacket::ConfigKeepAlive { id } => {
            w.write_i64(*id);
            (PacketPhase::Configuration, "keep_alive")
        }
        S2CPacket::ConfigRegistryData {
            registry_id,
            entries,
        } => {
            w.write_string(registry_id);
            w.write_varint(entries.len() as i32);
            for e in entries {
                w.write_string(&e.entry_id);
                w.write_bool(e.data.is_some());
                if let Some(data) = &e.data {
                    w.raw(data);
                }
            }
            (PacketPhase::Configuration, "registry_data")
        }
        S2CPacket::ConfigFinishConfiguration => {
            (PacketPhase::Configuration, "finish_configuration")
        }
        S2CPacket::PlayDisconnect { reason } => {
            w.raw(reason);
            (PacketPhase::Play, "disconnect")
        }
        S2CPacket::KeepAlive { id } => {
            w.write_i64(*id);
            (PacketPhase::Play, "keep_alive")
        }
        S2CPacket::BlockUpdate { pos, state_id } => {
            w.write_position(*pos);
            w.write_varint(*state_id as i32);
            (PacketPhase::Play, "block_update")
        }
        S2CPacket::ForgetLevelChunk { cx, cz } => {
            w.write_i64((i64::from(*cz) << 32) | (i64::from(*cx) & 0xffff_ffff));
            (PacketPhase::Play, "forget_level_chunk")
        }
        S2CPacket::LightUpdateMask {
            chunk_x,
            chunk_z,
            sky_mask,
            block_mask,
            empty_sky_mask,
            empty_block_mask,
            sky_light,
            block_light,
        } => {
            w.write_varint(*chunk_x);
            w.write_varint(*chunk_z);
            let write_mask = |w: &mut WireWriter, mask: &BitSet| {
                if caps.light_mask_byte_array {
                    w.write_bitset_bytes(mask);
                } else {
                    w.write_bitset(mask);
                }
            };
            write_mask(w, sky_mask);
            write_mask(w, block_mask);
            write_mask(w, empty_sky_mask);
            write_mask(w, empty_block_mask);
            w.write_varint(sky_light.len() as i32);
            for array in sky_light {
                w.write_bytes(array);
            }
            w.write_varint(block_light.len() as i32);
            for array in block_light {
                w.write_bytes(array);
            }
            (PacketPhase::Play, "light_update")
        }
        S2CPacket::OpenScreen {
            container_id,
            menu,
            title,
        } => {
            w.write_varint(*container_id);
            // Menu id resolves through the registry, not a constant: a
            // different version's table remaps this packet automatically.
            let menu_id = table
                .registry_id("minecraft:menu", menu.resource_name())
                .ok_or(ProtoError::Malformed(
                    "menu kind missing from the menu registry",
                ))?;
            w.write_varint(menu_id as i32);
            w.raw(title);
            (PacketPhase::Play, "open_screen")
        }
        S2CPacket::SetEquipment { entity_id, slots } => {
            if slots.is_empty() {
                // The wire form cannot express zero entries (the continue
                // bit lives on each slot byte), so refuse to emit one.
                return Err(ProtoError::Malformed("empty equipment list"));
            }
            w.write_varint(*entity_id);
            for (i, entry) in slots.iter().enumerate() {
                let more = u8::from(i + 1 < slots.len()) << 7;
                w.write_u8(entry.slot | more);
                write_item_stack(w, &entry.item);
            }
            (PacketPhase::Play, "set_equipment")
        }
        S2CPacket::SetPassengers {
            vehicle,
            passengers,
        } => {
            w.write_varint(*vehicle);
            w.write_varint(passengers.len() as i32);
            for &p in passengers {
                w.write_varint(p);
            }
            (PacketPhase::Play, "set_passengers")
        }
        S2CPacket::SetPlayerTeam { name, operation } => {
            w.write_string(name);
            match operation {
                TeamOperation::Create { info, members } => {
                    w.write_u8(0);
                    write_team_info(w, info);
                    write_string_list(w, members);
                }
                TeamOperation::Remove => {
                    w.write_u8(1);
                }
                TeamOperation::UpdateInfo { info } => {
                    w.write_u8(2);
                    write_team_info(w, info);
                }
                TeamOperation::AddMembers { members } => {
                    w.write_u8(3);
                    write_string_list(w, members);
                }
                TeamOperation::RemoveMembers { members } => {
                    w.write_u8(4);
                    write_string_list(w, members);
                }
            }
            (PacketPhase::Play, "set_player_team")
        }
        S2CPacket::TabList { header, footer } => {
            w.raw(header);
            w.raw(footer);
            (PacketPhase::Play, "tab_list")
        }
        S2CPacket::TickingState {
            tick_rate,
            is_frozen,
        } => {
            w.write_f32(*tick_rate);
            w.write_bool(*is_frozen);
            (PacketPhase::Play, "ticking_state")
        }
        S2CPacket::TickingStep { steps } => {
            w.write_varint(*steps);
            (PacketPhase::Play, "ticking_step")
        }
    };
    Ok((phase, name))
}

/// [`encode_s2c_body_into`] for the serverbound direction.
pub(crate) fn encode_c2s_body_into(
    caps: &VersionCapabilities,
    packet: &C2SPacket,
    out: &mut Vec<u8>,
) -> Result<(PacketPhase, &'static str), ProtoError> {
    let mut w = WireWriter::from_vec(std::mem::take(out));
    let result = encode_c2s_body_write(caps, packet, &mut w);
    *out = w.into_vec();
    result
}

fn encode_c2s_body_write(
    caps: &VersionCapabilities,
    packet: &C2SPacket,
    w: &mut WireWriter,
) -> Result<(PacketPhase, &'static str), ProtoError> {
    let (phase, name) = match packet {
        C2SPacket::LoginCookieResponse { key, payload } => {
            write_cookie_response(w, key, payload)?;
            (PacketPhase::Login, "cookie_response")
        }
        C2SPacket::ConfigCookieResponse { key, payload } => {
            write_cookie_response(w, key, payload)?;
            (PacketPhase::Configuration, "cookie_response")
        }
        C2SPacket::PlayCookieResponse { key, payload } => {
            write_cookie_response(w, key, payload)?;
            (PacketPhase::Play, "cookie_response")
        }
        C2SPacket::Handshake {
            protocol_version,
            server_address,
            server_port,
            next_state,
        } => {
            w.write_varint(*protocol_version);
            w.write_string(server_address);
            w.write_u16(*server_port);
            w.write_varint(*next_state);
            (PacketPhase::Handshake, "handshake")
        }
        C2SPacket::LoginStart { name, uuid } => {
            w.write_string(name);
            w.write_uuid(uuid);
            (PacketPhase::Login, "login_start")
        }
        C2SPacket::EncryptionResponse {
            shared_secret,
            verify_token,
        } => {
            w.write_bytes(shared_secret);
            w.write_bytes(verify_token);
            (PacketPhase::Login, "encryption_response")
        }
        C2SPacket::LoginAcknowledged => (PacketPhase::Login, "login_acknowledged"),
        C2SPacket::ConfigKeepAlive { id } => {
            w.write_i64(*id);
            (PacketPhase::Configuration, "keep_alive")
        }
        C2SPacket::ConfigFinishConfiguration => {
            (PacketPhase::Configuration, "finish_configuration")
        }
        C2SPacket::MoveVehicle { x, y, z, yaw, pitch } => {
            w.write_f64(*x);
            w.write_f64(*y);
            w.write_f64(*z);
            w.write_f32(*yaw);
            w.write_f32(*pitch);
            (PacketPhase::Play, "move_vehicle")
        }
        C2SPacket::PaddleBoat { left, right } => {
            w.write_bool(*left);
            w.write_bool(*right);
            (PacketPhase::Play, "paddle_boat")
        }
        C2SPacket::KeepAlive { id } => {
            w.write_i64(*id);
            (PacketPhase::Play, "keep_alive")
        }
        C2SPacket::PlayerInput {
            sideways,
            forward,
            jump,
            unmount,
        } => {
            if caps.player_input_flags {
                w.write_u8(input_flags_byte(*sideways, *forward, *jump, *unmount));
            } else {
                w.write_f32(*sideways);
                w.write_f32(*forward);
                w.write_u8(u8::from(*jump) | (u8::from(*unmount) << 1));
            }
            (PacketPhase::Play, "player_input")
        }
        C2SPacket::PlayerInputFlags {
            forward,
            backward,
            left,
            right,
            jump,
            shift,
            sprint,
        } => {
            if caps.player_input_flags {
                let mut bits = 0u8;
                if *forward {
                    bits |= INPUT_FORWARD;
                }
                if *backward {
                    bits |= INPUT_BACKWARD;
                }
                if *left {
                    bits |= INPUT_LEFT;
                }
                if *right {
                    bits |= INPUT_RIGHT;
                }
                if *jump {
                    bits |= INPUT_JUMP;
                }
                if *shift {
                    bits |= INPUT_SHIFT;
                }
                if *sprint {
                    bits |= INPUT_SPRINT;
                }
                w.write_u8(bits);
            } else {
                // Downgrade to the float form: impulses become the vanilla
                // ±1.0 magnitudes, `shift` maps to the old `unmount` bit,
                // `sprint` has no 767 wire slot.
                w.write_f32(input_flags_impulse(*left, *right));
                w.write_f32(input_flags_impulse(*forward, *backward));
                w.write_u8(u8::from(*jump) | (u8::from(*shift) << 1));
            }
            (PacketPhase::Play, "player_input")
        }
    };
    Ok((phase, name))
}

/// Dispatch on the resolved wire name — decoders never see numeric ids.
/// `phase` disambiguates wire names shared across phases (`keep_alive`,
/// `finish_configuration`).
fn read_s2c(
    caps: &VersionCapabilities,
    table: &ProtocolTable,
    phase: PacketPhase,
    name: &str,
    r: &mut WireReader<'_>,
) -> Result<S2CPacket, ProtoError> {
    Ok(match name {
        "cookie_request" => {
            let key = read_cookie_key(r)?;
            match phase {
                PacketPhase::Login => S2CPacket::LoginCookieRequest { key },
                PacketPhase::Configuration => S2CPacket::ConfigCookieRequest { key },
                PacketPhase::Play => S2CPacket::PlayCookieRequest { key },
                _ => return Err(ProtoError::Malformed("cookie request outside connection phase")),
            }
        }
        "store_cookie" => {
            let (key, payload) = read_store_cookie(r)?;
            match phase {
                PacketPhase::Configuration => S2CPacket::ConfigStoreCookie { key, payload },
                PacketPhase::Play => S2CPacket::PlayStoreCookie { key, payload },
                _ => return Err(ProtoError::Malformed("cookie storage outside connection phase")),
            }
        }
        "transfer" => {
            let (host, port) = read_transfer(r)?;
            match phase {
                PacketPhase::Configuration => S2CPacket::ConfigTransfer { host, port },
                PacketPhase::Play => S2CPacket::PlayTransfer { host, port },
                _ => return Err(ProtoError::Malformed("transfer outside connection phase")),
            }
        }
        "login_disconnect" => S2CPacket::LoginDisconnect {
            reason: r.read_anonymous_nbt()?.to_vec(),
        },
        "encryption_request" => S2CPacket::EncryptionRequest {
            server_id: r.read_string(MAX_STRING_CHARS)?,
            public_key: r.read_bytes(1 << 16)?.to_vec(),
            verify_token: r.read_bytes(1 << 16)?.to_vec(),
            should_authenticate: r.read_bool()?,
        },
        "login_success" => S2CPacket::LoginSuccess {
            uuid: r.read_uuid()?,
            username: r.read_string(16)?,
            properties: {
                let n = r.read_count(1024)?;
                let mut props = Vec::with_capacity(n.min(64));
                for _ in 0..n {
                    let name = r.read_string(MAX_NAME_CHARS)?;
                    let value = r.read_string(MAX_NAME_CHARS)?;
                    let signature = if r.read_bool()? {
                        Some(r.read_string(MAX_NAME_CHARS)?)
                    } else {
                        None
                    };
                    props.push(LoginProperty {
                        name,
                        value,
                        signature,
                    });
                }
                props
            },
        },
        "set_compression" => S2CPacket::SetCompression {
            threshold: r.read_varint()?,
        },
        "keep_alive" => {
            let id = r.read_i64()?;
            if phase == PacketPhase::Configuration {
                S2CPacket::ConfigKeepAlive { id }
            } else {
                S2CPacket::KeepAlive { id }
            }
        }
        "disconnect" => {
            let reason = r.read_anonymous_nbt()?.to_vec();
            if phase == PacketPhase::Configuration {
                S2CPacket::ConfigDisconnect { reason }
            } else {
                S2CPacket::PlayDisconnect { reason }
            }
        }
        "block_update" => S2CPacket::BlockUpdate {
            pos: r.read_position()?,
            state_id: u32::try_from(r.read_varint()?).map_err(|_| {
                ProtoError::InvalidDiscriminant {
                    value: -1,
                    what: "block state id",
                }
            })?,
        },
        "forget_level_chunk" => {
            // Packed `ChunkPos` `i64`: x in the low 32 bits, z in the high.
            let packed = r.read_i64()?;
            S2CPacket::ForgetLevelChunk {
                cx: packed as i32,
                cz: (packed >> 32) as i32,
            }
        }
        "registry_data" => S2CPacket::ConfigRegistryData {
            registry_id: r.read_string(MAX_NAME_CHARS)?,
            entries: {
                let n = r.read_count(MAX_REGISTRY_ENTRIES)?;
                let mut entries = Vec::with_capacity(n.min(4096));
                for _ in 0..n {
                    let entry_id = r.read_string(MAX_NAME_CHARS)?;
                    let data = if r.read_bool()? {
                        Some(r.read_anonymous_nbt()?.to_vec())
                    } else {
                        None
                    };
                    entries.push(RegistryDataEntry { entry_id, data });
                }
                entries
            },
        },
        "finish_configuration" => S2CPacket::ConfigFinishConfiguration,
        "light_update" => {
            let chunk_x = r.read_varint()?;
            let chunk_z = r.read_varint()?;
            // 1024 words / 8192 bytes bound the same worst-case section count
            // under each mask encoding.
            let byte_masks = caps.light_mask_byte_array;
            let read_mask = |r: &mut WireReader<'_>| -> Result<BitSet, ProtoError> {
                if byte_masks {
                    r.read_bitset_bytes(8192)
                } else {
                    r.read_bitset(1024)
                }
            };
            let sky_mask = read_mask(r)?;
            let block_mask = read_mask(r)?;
            let empty_sky_mask = read_mask(r)?;
            let empty_block_mask = read_mask(r)?;
            let read_arrays = |r: &mut WireReader<'_>| -> Result<Vec<Vec<u8>>, ProtoError> {
                let n = r.read_count(1024)?;
                let mut arrays = Vec::with_capacity(n.min(64));
                for _ in 0..n {
                    let array = r.read_bytes(1 << 20)?;
                    arrays.push(array.to_vec());
                }
                Ok(arrays)
            };
            let sky_light = read_arrays(r)?;
            let block_light = read_arrays(r)?;
            S2CPacket::LightUpdateMask {
                chunk_x,
                chunk_z,
                sky_mask,
                block_mask,
                empty_sky_mask,
                empty_block_mask,
                sky_light,
                block_light,
            }
        }
        "open_screen" => {
            let container_id = r.read_varint()?;
            let menu_id = r.read_varint()?;
            if menu_id < 0 {
                return Err(ProtoError::InvalidDiscriminant {
                    value: i64::from(menu_id),
                    what: "menu id",
                });
            }
            let menu_name = table
                .registry_name("minecraft:menu", menu_id as u32)
                .ok_or(ProtoError::InvalidDiscriminant {
                    value: i64::from(menu_id),
                    what: "menu id",
                })?;
            let menu = MenuKind::from_resource_name(menu_name).ok_or(
                ProtoError::InvalidDiscriminant {
                    value: i64::from(menu_id),
                    what: "menu kind",
                },
            )?;
            let title = r.read_anonymous_nbt()?.to_vec();
            S2CPacket::OpenScreen {
                container_id,
                menu,
                title,
            }
        }
        "set_equipment" => {
            let entity_id = r.read_varint()?;
            let mut slots = Vec::new();
            loop {
                let byte = r.read_u8()?;
                let slot = byte & 0x7f;
                if slot > 5 {
                    return Err(ProtoError::InvalidDiscriminant {
                        value: i64::from(slot),
                        what: "equipment slot",
                    });
                }
                let item = read_item_stack(r, table)?;
                slots.push(EquipmentSlot { slot, item });
                if byte & 0x80 == 0 {
                    break;
                }
            }
            S2CPacket::SetEquipment { entity_id, slots }
        }
        "set_passengers" => {
            let vehicle = r.read_varint()?;
            let passengers = {
                let n = r.read_count(MAX_PASSENGERS)?;
                let mut list = Vec::with_capacity(n.min(256));
                for _ in 0..n {
                    list.push(r.read_varint()?);
                }
                list
            };
            S2CPacket::SetPassengers { vehicle, passengers }
        }
        "set_player_team" => {
            let name = r.read_string(MAX_NAME_CHARS)?;
            let mode = r.read_u8()?;
            let operation = match mode {
                0 => TeamOperation::Create {
                    info: read_team_info(r)?,
                    members: read_string_list(r, MAX_TEAM_MEMBERS)?,
                },
                1 => TeamOperation::Remove,
                2 => TeamOperation::UpdateInfo {
                    info: read_team_info(r)?,
                },
                3 => TeamOperation::AddMembers {
                    members: read_string_list(r, MAX_TEAM_MEMBERS)?,
                },
                4 => TeamOperation::RemoveMembers {
                    members: read_string_list(r, MAX_TEAM_MEMBERS)?,
                },
                other => {
                    return Err(ProtoError::InvalidDiscriminant {
                        value: i64::from(other),
                        what: "team mode",
                    })
                }
            };
            S2CPacket::SetPlayerTeam { name, operation }
        }
        "tab_list" => S2CPacket::TabList {
            header: r.read_anonymous_nbt()?.to_vec(),
            footer: r.read_anonymous_nbt()?.to_vec(),
        },
        "ticking_state" => S2CPacket::TickingState {
            tick_rate: r.read_f32()?,
            is_frozen: r.read_bool_lenient()?,
        },
        "ticking_step" => {
            let steps = r.read_varint()?;
            if steps < 0 {
                return Err(ProtoError::InvalidDiscriminant {
                    value: i64::from(steps),
                    what: "ticking step count",
                });
            }
            S2CPacket::TickingStep { steps }
        }
        _ => {
            return Err(ProtoError::UnknownPacketId {
                id: -1,
                context: "s2c packet outside the canonical IR",
            })
        }
    })
}

fn read_c2s(
    caps: &VersionCapabilities,
    phase: PacketPhase,
    name: &str,
    r: &mut WireReader<'_>,
) -> Result<C2SPacket, ProtoError> {
    Ok(match name {
        "cookie_response" => {
            let (key, payload) = read_cookie_response(r)?;
            match phase {
                PacketPhase::Login => C2SPacket::LoginCookieResponse { key, payload },
                PacketPhase::Configuration => C2SPacket::ConfigCookieResponse { key, payload },
                PacketPhase::Play => C2SPacket::PlayCookieResponse { key, payload },
                _ => return Err(ProtoError::Malformed("cookie response outside connection phase")),
            }
        }
        "handshake" => {
            let protocol_version = r.read_varint()?;
            let server_address = r.read_string(255)?;
            let server_port = r.read_u16()?;
            let next_state = r.read_varint()?;
            if !matches!(next_state, 1 | 2) {
                return Err(ProtoError::InvalidDiscriminant {
                    value: i64::from(next_state),
                    what: "handshake next state",
                });
            }
            C2SPacket::Handshake {
                protocol_version,
                server_address,
                server_port,
                next_state,
            }
        }
        "login_start" => C2SPacket::LoginStart {
            name: r.read_string(16)?,
            uuid: r.read_uuid()?,
        },
        "encryption_response" => C2SPacket::EncryptionResponse {
            shared_secret: r.read_bytes(1 << 16)?.to_vec(),
            verify_token: r.read_bytes(1 << 16)?.to_vec(),
        },
        "login_acknowledged" => C2SPacket::LoginAcknowledged,
        "keep_alive" => {
            let id = r.read_i64()?;
            if phase == PacketPhase::Configuration {
                C2SPacket::ConfigKeepAlive { id }
            } else {
                C2SPacket::KeepAlive { id }
            }
        }
        "finish_configuration" => C2SPacket::ConfigFinishConfiguration,
        "move_vehicle" => {
            let x = r.read_f64()?;
            let y = r.read_f64()?;
            let z = r.read_f64()?;
            let yaw = r.read_f32()?;
            let pitch = r.read_f32()?;
            if ![x, y, z].iter().all(|v| v.is_finite())
                || !yaw.is_finite()
                || !pitch.is_finite()
            {
                return Err(ProtoError::Malformed("non-finite vehicle coordinates"));
            }
            C2SPacket::MoveVehicle { x, y, z, yaw, pitch }
        }
        "paddle_boat" => C2SPacket::PaddleBoat {
            left: r.read_bool_lenient()?,
            right: r.read_bool_lenient()?,
        },
        "player_input" => {
            if caps.player_input_flags {
                let bits = r.read_u8()?;
                C2SPacket::PlayerInputFlags {
                    forward: bits & INPUT_FORWARD != 0,
                    backward: bits & INPUT_BACKWARD != 0,
                    left: bits & INPUT_LEFT != 0,
                    right: bits & INPUT_RIGHT != 0,
                    jump: bits & INPUT_JUMP != 0,
                    shift: bits & INPUT_SHIFT != 0,
                    sprint: bits & INPUT_SPRINT != 0,
                }
            } else {
                let sideways = r.read_f32()?;
                let forward = r.read_f32()?;
                let flags = r.read_u8()?;
                if !sideways.is_finite() || !forward.is_finite() {
                    return Err(ProtoError::Malformed("non-finite player input"));
                }
                C2SPacket::PlayerInput {
                    sideways,
                    forward,
                    jump: flags & 0x01 != 0,
                    unmount: flags & 0x02 != 0,
                }
            }
        }
        _ => {
            return Err(ProtoError::UnknownPacketId {
                id: -1,
                context: "c2s packet outside the canonical IR",
            })
        }
    })
}

thread_local! {
    /// Packet-body scratch shared by every codec on this thread. Encoding used
    /// to pay a fresh `Vec` per packet plus the two tiny writers `write_frame`
    /// built; steady-state emission now allocates nothing.
    static BODY_SCRATCH: std::cell::RefCell<Vec<u8>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// Capacity [`BODY_SCRATCH`] keeps between packets: a one-off giant packet (a
/// multi-MiB registry dump) must not pin that allocation per thread forever.
const BODY_SCRATCH_KEEP: usize = 256 * 1024;

/// Run `f` with the cleared shared packet-body scratch. The encode path never
/// re-enters `with_body_scratch` — nested use would panic on the `RefCell`.
pub(crate) fn with_body_scratch<R>(f: impl FnOnce(&mut Vec<u8>) -> R) -> R {
    BODY_SCRATCH.with(|cell| {
        let mut body = cell.borrow_mut();
        body.clear();
        let result = f(&mut body);
        if body.capacity() > BODY_SCRATCH_KEEP {
            body.shrink_to(BODY_SCRATCH_KEEP);
        }
        result
    })
}

impl ProtocolCodec for CodecV767 {
    fn capabilities(&self) -> &VersionCapabilities {
        &self.capabilities
    }

    fn table(&self) -> &ProtocolTable {
        &self.table
    }

    fn encode_s2c(&self, packet: &S2CPacket, out: &mut Vec<u8>) -> Result<(), ProtoError> {
        // A keep_alive in configuration and play share a field shape but are
        // different packets — the IR variant names the phase explicitly.
        with_body_scratch(|body| {
            let (phase, name) =
                encode_s2c_body_into(&self.capabilities, &self.table, packet, body)?;
            framed(&self.table, phase, PacketDir::Clientbound, name, body, out)
        })
    }

    fn decode_s2c(&self, frame: &[u8]) -> Result<S2CPacket, ProtoError> {
        self.decode_s2c_in(self.phase, frame)
    }

    fn encode_c2s(&self, packet: &C2SPacket, out: &mut Vec<u8>) -> Result<(), ProtoError> {
        with_body_scratch(|body| {
            let (phase, name) = encode_c2s_body_into(&self.capabilities, packet, body)?;
            framed(&self.table, phase, PacketDir::Serverbound, name, body, out)
        })
    }

    fn decode_c2s(&self, frame: &[u8]) -> Result<C2SPacket, ProtoError> {
        self.decode_c2s_in(self.phase, frame)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire::write_frame;

    /// Anonymous-NBT `{"text":"..."}` — identical bytes to `chat_nbt`.
    fn text_component(text: &str) -> Vec<u8> {
        let mut v = vec![10]; // TAG_Compound
        v.push(8); // TAG_String
        v.extend_from_slice(&4u16.to_be_bytes());
        v.extend_from_slice(b"text");
        v.extend_from_slice(&(text.len() as u16).to_be_bytes());
        v.extend_from_slice(text.as_bytes());
        v.push(0);
        v
    }

    fn codec() -> CodecV767 {
        CodecV767::new()
    }

    fn s2c_roundtrip(codec: &CodecV767, packet: &S2CPacket, phase: PacketPhase) -> S2CPacket {
        let mut frame = Vec::new();
        codec.encode_s2c(packet, &mut frame).unwrap();
        codec.decode_s2c_in(phase, &frame).unwrap()
    }

    fn c2s_roundtrip(codec: &CodecV767, packet: &C2SPacket, phase: PacketPhase) -> C2SPacket {
        let mut frame = Vec::new();
        codec.encode_c2s(packet, &mut frame).unwrap();
        codec.decode_c2s_in(phase, &frame).unwrap()
    }

    // -- byte-exact agreement with mc-spine's encoders -----------------------

    #[test]
    fn handshake_frame_is_byte_exact() {
        // ServerboundIntentionPacket layout: VarInt proto, Utf8 addr, u16 port,
        // VarInt next_state — identical to handle_handshake's reader.
        let c = codec();
        let mut frame = Vec::new();
        c.encode_c2s(
            &C2SPacket::Handshake {
                protocol_version: 767,
                server_address: "localhost".into(),
                server_port: 25565,
                next_state: 2,
            },
            &mut frame,
        )
        .unwrap();
        // len=VarInt(14): id(1) + proto(3) + addr(1+9) + port(2) + next(1) = 15
        let mut body = Vec::new();
        let mut w = WireWriter::new();
        w.write_varint(767);
        w.write_string("localhost");
        w.write_u16(25565);
        w.write_varint(2);
        let mut expected = Vec::new();
        write_frame(&mut expected, 0, w.as_slice());
        body.extend_from_slice(&expected);
        assert_eq!(frame, body);
        assert_eq!(
            c.decode_c2s_in(PacketPhase::Handshake, &frame).unwrap(),
            C2SPacket::Handshake {
                protocol_version: 767,
                server_address: "localhost".into(),
                server_port: 25565,
                next_state: 2,
            }
        );
    }

    #[test]
    fn handshake_rejects_bad_next_state() {
        let c = codec();
        let mut body = WireWriter::new();
        body.write_varint(767);
        body.write_string("x");
        body.write_u16(25565);
        body.write_varint(99);
        let mut frame = Vec::new();
        write_frame(&mut frame, 0, body.as_slice());
        assert!(matches!(
            c.decode_c2s_in(PacketPhase::Handshake, &frame),
            Err(ProtoError::InvalidDiscriminant { .. })
        ));
    }

    #[test]
    fn ticking_state_and_step_byte_exact() {
        // ticking_state_packet: f32 BE + u8 frozen; ticking_step_packet: VarInt.
        let c = codec();
        let mut frame = Vec::new();
        c.encode_s2c(
            &S2CPacket::TickingState {
                tick_rate: 20.0,
                is_frozen: true,
            },
            &mut frame,
        )
        .unwrap();
        let mut expected = Vec::new();
        write_frame(&mut expected, 0x71, &[0x41, 0xa0, 0x00, 0x00, 0x01]);
        assert_eq!(frame, expected);

        let mut step_frame = Vec::new();
        c.encode_s2c(&S2CPacket::TickingStep { steps: 5 }, &mut step_frame)
            .unwrap();
        let mut expected = Vec::new();
        write_frame(&mut expected, 0x72, &[0x05]);
        assert_eq!(step_frame, expected);

        assert_eq!(
            s2c_roundtrip(
                &c,
                &S2CPacket::TickingState {
                    tick_rate: 12.5,
                    is_frozen: false,
                },
                PacketPhase::Play
            ),
            S2CPacket::TickingState {
                tick_rate: 12.5,
                is_frozen: false,
            }
        );
        assert_eq!(
            s2c_roundtrip(&c, &S2CPacket::TickingStep { steps: 0 }, PacketPhase::Play),
            S2CPacket::TickingStep { steps: 0 }
        );
    }

    #[test]
    fn keep_alive_byte_exact_both_directions() {
        let c = codec();
        let mut frame = Vec::new();
        c.encode_s2c(&S2CPacket::KeepAlive { id: 42 }, &mut frame)
            .unwrap();
        let mut expected = Vec::new();
        write_frame(&mut expected, 0x26, &42i64.to_be_bytes());
        assert_eq!(frame, expected);

        let mut cframe = Vec::new();
        c.encode_c2s(&C2SPacket::KeepAlive { id: -7 }, &mut cframe)
            .unwrap();
        let mut expected = Vec::new();
        write_frame(&mut expected, 0x18, &(-7i64).to_be_bytes());
        assert_eq!(cframe, expected);

        assert_eq!(
            s2c_roundtrip(&c, &S2CPacket::KeepAlive { id: i64::MIN }, PacketPhase::Play),
            S2CPacket::KeepAlive { id: i64::MIN }
        );
        assert_eq!(
            c2s_roundtrip(&c, &C2SPacket::KeepAlive { id: 9 }, PacketPhase::Play),
            C2SPacket::KeepAlive { id: 9 }
        );
    }

    #[test]
    fn open_screen_maps_menu_via_table() {
        let c = codec();
        let mut frame = Vec::new();
        c.encode_s2c(
            &S2CPacket::OpenScreen {
                container_id: 3,
                menu: MenuKind::Crafting,
                title: text_component("Crafting"),
            },
            &mut frame,
        )
        .unwrap();
        // 0x33: VarInt windowId=3, VarInt menu=12 (crafting), NBT title —
        // identical to the literal block in play_protocol.rs.
        let mut expected = Vec::new();
        write_frame(&mut expected, 0x33, &{
            let mut b = vec![0x03, 0x0c];
            b.extend_from_slice(&text_component("Crafting"));
            b
        });
        assert_eq!(frame, expected);
        assert_eq!(
            s2c_roundtrip(
                &c,
                &S2CPacket::OpenScreen {
                    container_id: 3,
                    menu: MenuKind::Crafting,
                    title: text_component("Crafting"),
                },
                PacketPhase::Play
            ),
            S2CPacket::OpenScreen {
                container_id: 3,
                menu: MenuKind::Crafting,
                title: text_component("Crafting"),
            }
        );
        // Anvil maps to registry id 8 through the table, not a literal.
        let mut frame = Vec::new();
        c.encode_s2c(
            &S2CPacket::OpenScreen {
                container_id: 1,
                menu: MenuKind::Anvil,
                title: text_component("A"),
            },
            &mut frame,
        )
        .unwrap();
        // frame = len, id, VarInt container_id, VarInt menu_id, title…
        assert_eq!(frame[3], 8); // minecraft:anvil = 8
    }

    #[test]
    fn set_equipment_byte_exact() {
        // encode_set_equipment: VarInt eid + (slot|0x80, stack)*, last flagged.
        let c = codec();
        let stack = |item, count| ItemStack {
            item_id: item,
            count,
            added: vec![RawComponent {
                type_id: 1, // max_stack_size: VarInt payload
                data: vec![0x40],
            }],
            removed: Vec::new(),
        };
        let packet = S2CPacket::SetEquipment {
            entity_id: 1337,
            slots: vec![
                EquipmentSlot {
                    slot: 0,
                    item: stack(99, 1),
                },
                EquipmentSlot {
                    slot: 5,
                    item: ItemStack::empty(),
                },
            ],
        };
        let mut frame = Vec::new();
        c.encode_s2c(&packet, &mut frame).unwrap();
        // Expected body: eid=1337 -> VarInt [0xb9,0x0a], entry0 slot|0x80=0x80,
        // stack{count=1,item=99,nadd=1,nrem=0,comp(1,0x40)}, entry1 slot=5,
        // empty stack {count=0}.
        let mut expected = Vec::new();
        write_frame(
            &mut expected,
            0x5b,
            &[0xb9, 0x0a, 0x80, 0x01, 0x63, 0x01, 0x00, 0x01, 0x40, 0x05, 0x00],
        );
        assert_eq!(frame, expected);
        assert_eq!(s2c_roundtrip(&c, &packet, PacketPhase::Play), packet);
        // Non-contiguous encode order derives the continue bit correctly.
        assert_eq!(
            s2c_roundtrip(
                &c,
                &S2CPacket::SetEquipment {
                    entity_id: -1,
                    slots: vec![EquipmentSlot {
                        slot: 4,
                        item: ItemStack {
                            item_id: 7,
                            count: 64,
                            added: Vec::new(),
                            removed: vec![3],
                        },
                    }],
                },
                PacketPhase::Play
            ),
            S2CPacket::SetEquipment {
                entity_id: -1,
                slots: vec![EquipmentSlot {
                    slot: 4,
                    item: ItemStack {
                        item_id: 7,
                        count: 64,
                        added: Vec::new(),
                        removed: vec![3],
                    },
                }],
            }
        );
    }

    #[test]
    fn set_equipment_fail_closed() {
        let c = codec();
        // Truncated eid-only frame.
        let mut frame = Vec::new();
        write_frame(&mut frame, 0x5b, &[0x01]);
        assert!(c.decode_s2c_in(PacketPhase::Play, &frame).is_err());
        // Slot out of range (> 5).
        let mut frame = Vec::new();
        write_frame(&mut frame, 0x5b, &[0x01, 0x06, 0x00]);
        assert!(matches!(
            c.decode_s2c_in(PacketPhase::Play, &frame),
            Err(ProtoError::InvalidDiscriminant { .. })
        ));
        // Unknown component type: payload boundary unrecoverable → reject.
        let mut frame = Vec::new();
        write_frame(
            &mut frame,
            0x5b,
            &[0x01, 0x00, 0x01, 0x63, 0x01, 0x00, 0xc8, 0x01, 0xaa],
        );
        assert!(c.decode_s2c_in(PacketPhase::Play, &frame).is_err());
        // Continue bit set but stream ends.
        let mut frame = Vec::new();
        write_frame(&mut frame, 0x5b, &[0x01, 0x80, 0x00]);
        assert!(c.decode_s2c_in(PacketPhase::Play, &frame).is_err());
    }

    #[test]
    fn set_passengers_byte_exact() {
        let c = codec();
        let packet = S2CPacket::SetPassengers {
            vehicle: 55,
            passengers: vec![100, 200],
        };
        let mut frame = Vec::new();
        c.encode_s2c(&packet, &mut frame).unwrap();
        let mut expected = Vec::new();
        write_frame(&mut expected, 0x5f, &[0x37, 0x02, 0x64, 0xc8, 0x01]);
        assert_eq!(frame, expected);
        assert_eq!(s2c_roundtrip(&c, &packet, PacketPhase::Play), packet);
    }

    #[test]
    fn set_player_team_byte_exact() {
        let c = codec();
        let info = TeamInfo {
            display_name: text_component("Red"),
            friendly_flags: 0x03,
            nametag_visibility: "always".into(),
            collision_rule: "push_own_team".into(),
            color: -1,
            prefix: text_component("R"),
            suffix: text_component(""),
        };
        let packet = S2CPacket::SetPlayerTeam {
            name: "red".into(),
            operation: TeamOperation::Create {
                info: info.clone(),
                members: vec!["alice".into()],
            },
        };
        let mut frame = Vec::new();
        c.encode_s2c(&packet, &mut frame).unwrap();
        // Mirror set_player_team_packet mode 0 byte-for-byte.
        let mut body = WireWriter::new();
        body.write_string("red");
        body.write_u8(0);
        body.raw(&text_component("Red"));
        body.write_u8(0x03);
        body.write_string("always");
        body.write_string("push_own_team");
        body.write_varint(-1);
        body.raw(&text_component("R"));
        body.raw(&text_component(""));
        body.write_varint(1);
        body.write_string("alice");
        let mut expected = Vec::new();
        write_frame(&mut expected, 0x60, body.as_slice());
        assert_eq!(frame, expected);
        assert_eq!(s2c_roundtrip(&c, &packet, PacketPhase::Play), packet);
        // Other modes round-trip.
        for op in [
            TeamOperation::Remove,
            TeamOperation::UpdateInfo {
                info: info.clone(),
            },
            TeamOperation::AddMembers {
                members: vec!["a".into(), "b".into()],
            },
            TeamOperation::RemoveMembers { members: vec![] },
        ] {
            let p = S2CPacket::SetPlayerTeam {
                name: "t".into(),
                operation: op.clone(),
            };
            assert_eq!(s2c_roundtrip(&c, &p, PacketPhase::Play), p);
        }
        // Invalid mode byte fails closed.
        let mut body = WireWriter::new();
        body.write_string("x");
        body.write_u8(9);
        let mut frame = Vec::new();
        write_frame(&mut frame, 0x60, body.as_slice());
        assert!(matches!(
            c.decode_s2c_in(PacketPhase::Play, &frame),
            Err(ProtoError::InvalidDiscriminant { .. })
        ));
    }

    #[test]
    fn tab_list_byte_exact() {
        let c = codec();
        let packet = S2CPacket::TabList {
            header: text_component("Welcome"),
            footer: text_component("mc-spine"),
        };
        let mut frame = Vec::new();
        c.encode_s2c(&packet, &mut frame).unwrap();
        let mut expected = Vec::new();
        write_frame(&mut expected, 0x6d, &{
            let mut b = text_component("Welcome");
            b.extend_from_slice(&text_component("mc-spine"));
            b
        });
        assert_eq!(frame, expected);
        assert_eq!(s2c_roundtrip(&c, &packet, PacketPhase::Play), packet);
        // Truncated mid-footer fails closed.
        assert!(c
            .decode_s2c_in(PacketPhase::Play, &frame[..frame.len() - 3])
            .is_err());
    }

    #[test]
    fn vehicle_packets_byte_exact() {
        let c = codec();
        // MoveVehicle: f64 xyz + f32 yaw/pitch, finite enforced.
        let mv = C2SPacket::MoveVehicle {
            x: 1.5,
            y: -2.25,
            z: 300.0,
            yaw: 90.0,
            pitch: -15.5,
        };
        let mut frame = Vec::new();
        c.encode_c2s(&mv, &mut frame).unwrap();
        let mut expected_body = Vec::new();
        expected_body.extend_from_slice(&1.5f64.to_be_bytes());
        expected_body.extend_from_slice(&(-2.25f64).to_be_bytes());
        expected_body.extend_from_slice(&300.0f64.to_be_bytes());
        expected_body.extend_from_slice(&90.0f32.to_be_bytes());
        expected_body.extend_from_slice(&(-15.5f32).to_be_bytes());
        let mut expected = Vec::new();
        write_frame(&mut expected, 0x1e, &expected_body);
        assert_eq!(frame, expected);
        assert_eq!(c2s_roundtrip(&c, &mv, PacketPhase::Play), mv);
        // NaN on the wire is the vanilla disconnect condition → reject.
        let mut bad = expected_body.clone();
        bad[0..8].copy_from_slice(&f64::NAN.to_be_bytes());
        let mut bad_frame = Vec::new();
        write_frame(&mut bad_frame, 0x1e, &bad);
        assert!(c.decode_c2s_in(PacketPhase::Play, &bad_frame).is_err());

        // PaddleBoat: two bools.
        let pb = C2SPacket::PaddleBoat {
            left: true,
            right: false,
        };
        let mut frame = Vec::new();
        c.encode_c2s(&pb, &mut frame).unwrap();
        let mut expected = Vec::new();
        write_frame(&mut expected, 0x1f, &[1, 0]);
        assert_eq!(frame, expected);
        assert_eq!(c2s_roundtrip(&c, &pb, PacketPhase::Play), pb);

        // PlayerInput: f32 sideways/forward + flag byte.
        let pi = C2SPacket::PlayerInput {
            sideways: 0.98,
            forward: -1.0,
            jump: true,
            unmount: false,
        };
        let mut frame = Vec::new();
        c.encode_c2s(&pi, &mut frame).unwrap();
        let mut expected = Vec::new();
        write_frame(&mut expected, 0x26, &{
            let mut b = Vec::new();
            b.extend_from_slice(&0.98f32.to_be_bytes());
            b.extend_from_slice(&(-1.0f32).to_be_bytes());
            b.push(0x01);
            b
        });
        assert_eq!(frame, expected);
        assert_eq!(c2s_roundtrip(&c, &pi, PacketPhase::Play), pi);
    }

    #[test]
    fn light_update_multi_word_masks() {
        let c = codec();
        // Masks spanning >64 sections: words must survive verbatim — a
        // single-u64 encoder drops them.
        let mut sky = BitSet::new();
        sky.set(0, true);
        sky.set(64, true);
        sky.set(100, true);
        let block = BitSet::from_u64(0b101);
        let packet = S2CPacket::LightUpdateMask {
            chunk_x: -3,
            chunk_z: 9,
            sky_mask: sky.clone(),
            block_mask: block.clone(),
            empty_sky_mask: BitSet::from_u64(1 << 2),
            empty_block_mask: BitSet::new(),
            sky_light: vec![vec![0xffu8; 2048], vec![0xee; 2048], vec![0xdd; 2048]],
            block_light: vec![vec![0x11; 2048], vec![0x22; 2048]],
        };
        let mut frame = Vec::new();
        c.encode_s2c(&packet, &mut frame).unwrap();
        assert_eq!(s2c_roundtrip(&c, &packet, PacketPhase::Play), packet);
    }

    #[test]
    fn registry_data_roundtrip_and_nbt() {
        let c = codec();
        let nbt_entry = {
            // {"min_y":-64} compound.
            let mut v = vec![10u8, 3, 0, 5];
            v.extend_from_slice(b"min_y");
            v.extend_from_slice(&(-64i32).to_be_bytes());
            v.push(0);
            v
        };
        let packet = S2CPacket::ConfigRegistryData {
            registry_id: "minecraft:dimension_type".into(),
            entries: vec![
                RegistryDataEntry {
                    entry_id: "minecraft:overworld".into(),
                    data: Some(nbt_entry.clone()),
                },
                RegistryDataEntry {
                    entry_id: "minecraft:the_nether".into(),
                    data: None,
                },
            ],
        };
        assert_eq!(
            s2c_roundtrip(&c, &packet, PacketPhase::Configuration),
            packet
        );
        // 0x07 is the 767 id for config registry_data.
        let mut frame = Vec::new();
        c.encode_s2c(&packet, &mut frame).unwrap();
        assert_eq!(frame[1], 0x07);
    }

    #[test]
    fn login_packets_roundtrip() {
        let c = codec();
        let login = C2SPacket::LoginStart {
            name: "Steve".into(),
            uuid: [0xaa; 16],
        };
        assert_eq!(c2s_roundtrip(&c, &login, PacketPhase::Login), login);

        let success = S2CPacket::LoginSuccess {
            uuid: [0x11; 16],
            username: "Alex".into(),
            properties: vec![LoginProperty {
                name: "textures".into(),
                value: "AAAA".into(),
                signature: Some("BBBB".into()),
            }],
        };
        assert_eq!(s2c_roundtrip(&c, &success, PacketPhase::Login), success);

        let enc = S2CPacket::EncryptionRequest {
            server_id: String::new(),
            public_key: vec![1, 2, 3],
            verify_token: vec![4, 5],
            should_authenticate: true,
        };
        assert_eq!(s2c_roundtrip(&c, &enc, PacketPhase::Login), enc);

        let resp = C2SPacket::EncryptionResponse {
            shared_secret: vec![9; 16],
            verify_token: vec![8; 4],
        };
        assert_eq!(c2s_roundtrip(&c, &resp, PacketPhase::Login), resp);

        assert_eq!(
            s2c_roundtrip(
                &c,
                &S2CPacket::LoginDisconnect {
                    reason: text_component("no")
                },
                PacketPhase::Login
            ),
            S2CPacket::LoginDisconnect {
                reason: text_component("no")
            }
        );
        assert_eq!(
            s2c_roundtrip(&c, &S2CPacket::SetCompression { threshold: 256 }, PacketPhase::Login),
            S2CPacket::SetCompression { threshold: 256 }
        );
        assert_eq!(
            c2s_roundtrip(&c, &C2SPacket::LoginAcknowledged, PacketPhase::Login),
            C2SPacket::LoginAcknowledged
        );
        assert_eq!(
            s2c_roundtrip(&c, &S2CPacket::ConfigFinishConfiguration, PacketPhase::Configuration),
            S2CPacket::ConfigFinishConfiguration
        );
        assert_eq!(
            c2s_roundtrip(&c, &C2SPacket::ConfigFinishConfiguration, PacketPhase::Configuration),
            C2SPacket::ConfigFinishConfiguration
        );
    }

    #[test]
    fn decode_rejects_unknown_ids_and_garbage() {
        let c = codec();
        // A play-phase id outside the IR (bundle_delimiter 0x00).
        let mut frame = Vec::new();
        write_frame(&mut frame, 0x00, &[]);
        assert!(matches!(
            c.decode_s2c_in(PacketPhase::Play, &frame),
            Err(ProtoError::UnknownPacketId { .. })
        ));
        // Unknown id entirely.
        let mut frame = Vec::new();
        write_frame(&mut frame, 0x7b, &[]);
        // 0x7b is server_links — in the table but not the IR.
        assert!(c.decode_s2c_in(PacketPhase::Play, &frame).is_err());
        // Truncated frame: declared length > buffer.
        let mut frame = Vec::new();
        write_frame(&mut frame, 0x71, &[0x41, 0xa0]);
        frame.pop();
        assert!(c.decode_s2c_in(PacketPhase::Play, &frame).is_err());
        // Trailing garbage in an otherwise valid packet.
        let mut frame = Vec::new();
        write_frame(&mut frame, 0x72, &[0x05, 0x99]);
        assert!(matches!(
            c.decode_s2c_in(PacketPhase::Play, &frame),
            Err(ProtoError::Malformed(_))
        ));
        // Corrupt VarInt in the id slot.
        assert!(c
            .decode_s2c_in(PacketPhase::Play, &[5, 0x80, 0x80, 0x80, 0x80])
            .is_err());
    }

    #[test]
    fn phase_tracking_uses_current_phase() {
        let mut c = codec();
        assert_eq!(c.phase(), PacketPhase::Handshake);
        let mut frame = Vec::new();
        write_frame(&mut frame, 0x26, &42i64.to_be_bytes());
        // 0x26 has no handshake-phase meaning.
        assert!(c.decode_s2c(&frame).is_err());
        c.set_phase(PacketPhase::Play);
        assert_eq!(
            c.decode_s2c(&frame).unwrap(),
            S2CPacket::KeepAlive { id: 42 }
        );
        // The trait sees the same capabilities and table.
        assert_eq!(ProtocolCodec::capabilities(&c).protocol_version, 767);
        assert_eq!(
            c.table()
                .packet_id(PacketPhase::Play, PacketDir::Clientbound, "keep_alive"),
            Some(0x26)
        );
    }
}
