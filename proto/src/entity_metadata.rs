// SPDX-License-Identifier: LGPL-3.0-or-later
//! Canonical Minecraft Java Edition 1.21.1 (protocol 767) entity metadata.
//!
//! `set_entity_data` carries `(index, serializer, value)` triples, and nothing
//! on the wire says which index or which serializer a given class expects: the
//! client resolves both from a per-class table it built at load time. A
//! handwritten body therefore cannot be wrong "locally" — it is wrong in the
//! client's table, silently, and the field it lands on is whatever happens to
//! sit at that index. Every defect this packet has produced in this codebase
//! came from one of three spellings of that mistake:
//!
//! * the right field at the wrong index — a living entity's health written at
//!   `8` (`LIVING_FLAGS` territory) instead of `9`, a cat's collar at `21`
//!   (`RELAX_STATE_ONE`) instead of `22`, sheep wool at `16` (`BABY`) instead
//!   of `17`;
//! * the right index with the wrong serializer — a `SnifferState` VarInt
//!   written where `SLEEPING_POS` needs a presence flag plus a packed position,
//!   which reads every following byte of the body at the wrong offset;
//! * the right triple and a malformed frame — a hardcoded packet id, a missing
//!   `0xff` terminator or a payload length that disagrees with the body.
//!
//! This module removes the room for all three:
//!
//! * [`MetaSerializer`] is the `0..=30` serializer id table in vanilla's
//!   registration order (`EntityDataSerializers`).
//! * [`MetaValue`] has exactly one variant per serializer, so the id is
//!   *derived* from the payload ([`MetaValue::serializer`]) and cannot
//!   disagree with it.
//! * [`meta_index`] holds the `ClassTreeIdRegistry` index of every field this
//!   codebase sends, per entity class, so a call site names the field instead
//!   of typing a number.
//! * [`validate_entry`] is the schema guard: a `(kind, index, serializer)`
//!   triple the 1.21.1 class tree does not define is rejected, and
//!   [`encode_set_entity_data_checked`] runs that guard over a whole body
//!   before encoding it.
//! * [`encode_set_entity_data`] frames the body with the packet id resolved
//!   from the canonical [`crate::table`] 767 table — never a literal — and
//!   always terminates it.
//!
//! Be precise about which half is structural: a payload cannot lie about its
//! serializer, and a body cannot lose its terminator or its packet id. The
//! *index* half is a function call, not a type: [`encode_set_entity_data`]
//! cannot know which class an entity belongs to, so call sites that know the
//! kind use [`encode_set_entity_data_checked`] (or [`MetaEntry::validate`])
//! to have the guard reject a foreign index before it reaches a socket.
//!
//! Payloads this crate does not model — NBT components, `Slot` item stacks and
//! particle options — are carried as the exact pre-encoded bytes the owning
//! codec produced. Those bytes are written verbatim, so the serializer table
//! stays the single source of truth for *layout* while NBT, items and
//! particles stay owned by [`crate::codec`], [`crate::item_components`] and the
//! particle encoder.

use crate::data_v767::V767_PACKETS;
use crate::table::{PacketDir, PacketPhase};
use crate::wire::{write_frame, WireWriter};
use std::sync::OnceLock;

/// The single byte that ends a `set_entity_data` body. It is written after the
/// last entry and is the reason no real entry may use index `255`.
pub const METADATA_TERMINATOR: u8 = 0xff;

/// The canonical table name of the packet this module encodes. The numeric id
/// is looked up rather than restated, so a renumbered table cannot leave a
/// stale literal behind.
const SET_ENTITY_DATA_PACKET: &str = "set_entity_data";

/// The `EntityDataSerializers` registration order for 1.21.1 (protocol 767).
///
/// The discriminator on the wire is this ordinal's `VarInt`. Ids are
/// append-only in vanilla, so the ordering is the version's contract, not an
/// implementation detail: a client reads serializer `11` as
/// [`MetaSerializer::OptionalBlockPos`] and will misparse the rest of the body
/// if the server wrote something else there.
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MetaSerializer {
    /// `byte` — one raw signed byte.
    Byte = 0,
    /// `int` — `VarInt`, not a fixed-width `i32`.
    Int = 1,
    /// `long` — `VarLong`.
    Long = 2,
    /// `float` — four big-endian bytes.
    Float = 3,
    /// `string` — `VarInt` UTF-8 byte length plus the bytes.
    String = 4,
    /// `component` — one anonymous-NBT component, no length prefix.
    Component = 5,
    /// `optional_component` — a presence flag, then the component when set.
    OptionalComponent = 6,
    /// `item_stack` — one pre-encoded 1.21.1 `Slot`.
    ItemStack = 7,
    /// `boolean` — one byte, `0` or `1`.
    Boolean = 8,
    /// `rotations` — three big-endian floats.
    Rotations = 9,
    /// `block_pos` — one packed position, eight bytes.
    BlockPos = 10,
    /// `optional_block_pos` — a presence flag, then the packed position.
    OptionalBlockPos = 11,
    /// `direction` — `VarInt` direction ordinal.
    Direction = 12,
    /// `optional_uuid` — a presence flag, then the sixteen UUID bytes.
    OptionalUuid = 13,
    /// `block_state` — `VarInt` block-state registry id.
    BlockState = 14,
    /// `optional_block_state` — `VarInt`, where `0` marks the empty option.
    OptionalBlockState = 15,
    /// `compound_tag` — one anonymous-NBT compound, no length prefix.
    CompoundTag = 16,
    /// `particle` — one pre-encoded particle options payload.
    Particle = 17,
    /// `particles` — `VarInt` count, then that many particle payloads.
    Particles = 18,
    /// `villager_data` — three `VarInt`s: kind, profession, level.
    VillagerData = 19,
    /// `optional_unsigned_int` — `VarInt` registry id offset by one, where `0`
    /// marks the empty option.
    OptionalUnsignedInt = 20,
    /// `pose` — `VarInt` pose ordinal.
    Pose = 21,
    /// `cat_variant` — `VarInt` cat-variant registry id.
    CatVariant = 22,
    /// `wolf_variant` — `VarInt` wolf-variant registry id.
    WolfVariant = 23,
    /// `frog_variant` — `VarInt` frog-variant registry id.
    FrogVariant = 24,
    /// `optional_global_pos` — a presence flag, then a dimension identifier
    /// and a packed position.
    OptionalGlobalPos = 25,
    /// `painting_variant` — `VarInt` painting-variant registry id.
    PaintingVariant = 26,
    /// `sniffer_state` — `VarInt` sniffer-state ordinal.
    SnifferState = 27,
    /// `armadillo_state` — `VarInt` armadillo-state ordinal.
    ArmadilloState = 28,
    /// `vector3` — three big-endian floats.
    Vector3 = 29,
    /// `quaternion` — four big-endian floats.
    Quaternion = 30,
}

impl MetaSerializer {
    /// The on-wire serializer discriminator. Every id fits in one `VarInt`
    /// byte, but the encoder still writes a `VarInt` because vanilla does.
    pub fn id(self) -> u8 {
        self as u8
    }

    /// The serializer a wire id names, or `None` past the 1.21.1 table.
    pub fn from_id(id: u8) -> Option<Self> {
        Some(match id {
            0 => Self::Byte,
            1 => Self::Int,
            2 => Self::Long,
            3 => Self::Float,
            4 => Self::String,
            5 => Self::Component,
            6 => Self::OptionalComponent,
            7 => Self::ItemStack,
            8 => Self::Boolean,
            9 => Self::Rotations,
            10 => Self::BlockPos,
            11 => Self::OptionalBlockPos,
            12 => Self::Direction,
            13 => Self::OptionalUuid,
            14 => Self::BlockState,
            15 => Self::OptionalBlockState,
            16 => Self::CompoundTag,
            17 => Self::Particle,
            18 => Self::Particles,
            19 => Self::VillagerData,
            20 => Self::OptionalUnsignedInt,
            21 => Self::Pose,
            22 => Self::CatVariant,
            23 => Self::WolfVariant,
            24 => Self::FrogVariant,
            25 => Self::OptionalGlobalPos,
            26 => Self::PaintingVariant,
            27 => Self::SnifferState,
            28 => Self::ArmadilloState,
            29 => Self::Vector3,
            30 => Self::Quaternion,
            _ => return None,
        })
    }
}

/// One metadata value, typed by the serializer that owns its layout.
///
/// The variant *is* the serializer id: [`MetaValue::serializer`] maps each one
/// back, so a body can never carry a payload under a serializer that would
/// parse it differently. Values that this crate stores opaquely (`Component`,
/// `CompoundTag`, `ItemStack`, `Particle`, `Particles`) hold bytes that are
/// already in wire form and are written without re-encoding.
#[derive(Clone, Debug, PartialEq)]
pub enum MetaValue {
    /// `Byte` — the raw wire byte.
    Byte(u8),
    /// `Int` — `VarInt`.
    Int(i32),
    /// `Long` — `VarLong`.
    Long(i64),
    /// `Float` — big-endian `f32`.
    Float(f32),
    /// `String` — `VarInt` byte length plus UTF-8.
    String(String),
    /// `Component` — one pre-encoded component: unnamed network NBT, tag
    /// byte first and no root name.
    Component(Vec<u8>),
    /// `OptionalComponent` — the flag, then the component when set.
    OptionalComponent(Option<Vec<u8>>),
    /// `ItemStack` — one pre-encoded 1.21.1 `Slot` (`VarInt` count, then the
    /// item and its component patch when non-empty).
    ItemStack(Vec<u8>),
    /// `Boolean` — one byte.
    Boolean(bool),
    /// `Rotations` — three big-endian floats.
    Rotations([f32; 3]),
    /// `BlockPos` — one packed position; pack with
    /// [`crate::wire::BlockPos::pack`] or [`MetaValue::packed_block_pos`].
    BlockPos(i64),
    /// `OptionalBlockPos` — the flag, then the packed position when set.
    OptionalBlockPos(Option<i64>),
    /// `Direction` — `VarInt` direction ordinal.
    Direction(i32),
    /// `OptionalUuid` — the flag, then the sixteen UUID bytes when set.
    OptionalUuid(Option<[u8; 16]>),
    /// `BlockState` — `VarInt` block-state registry id.
    BlockState(i32),
    /// `OptionalBlockState` — the bare `VarInt`. Vanilla writes the registry id
    /// unchanged and reserves `0` for the empty option, so an explicit `air`
    /// id reads back as absent.
    OptionalBlockState(i32),
    /// `CompoundTag` — one pre-encoded unnamed network NBT compound.
    CompoundTag(Vec<u8>),
    /// `Particle` — one pre-encoded particle options payload.
    Particle(Vec<u8>),
    /// `Particles` — the count, then each particle payload.
    Particles(Vec<Vec<u8>>),
    /// `VillagerData` — three `VarInt`s.
    VillagerData {
        /// Villager kind (`minecraft:villager_type` id).
        kind: i32,
        /// Profession (`minecraft:villager_profession` id).
        profession: i32,
        /// Trade level, `1..=5` in vanilla.
        level: i32,
    },
    /// `OptionalUnsignedInt` — the registry id offset by one, `0` when absent.
    OptionalUnsignedInt(Option<u32>),
    /// `Pose` — `VarInt` pose ordinal.
    Pose(i32),
    /// `CatVariant` — `VarInt` registry id.
    CatVariant(i32),
    /// `WolfVariant` — a registry-reference holder. Vanilla's
    /// `ByteBufCodecs.holder` writes the registry id plus one; zero is
    /// reserved for an inline direct payload rather than registry id zero.
    WolfVariant(u32),
    /// `FrogVariant` — `VarInt` registry id.
    FrogVariant(i32),
    /// `OptionalGlobalPos` — the flag, then the dimension identifier and the
    /// packed position when set.
    OptionalGlobalPos(Option<(String, i64)>),
    /// `PaintingVariant` — a registry-reference holder. Vanilla's
    /// `ByteBufCodecs.holder` writes the registry id plus one; zero is
    /// reserved for an inline direct payload rather than registry id zero.
    PaintingVariant(u32),
    /// `SnifferState` — `VarInt` state ordinal.
    SnifferState(i32),
    /// `ArmadilloState` — `VarInt` state ordinal.
    ArmadilloState(i32),
    /// `Vector3` — three big-endian floats.
    Vector3([f32; 3]),
    /// `Quaternion` — four big-endian floats, `x y z w`.
    Quaternion([f32; 4]),
}

/// Write a zero-based id in vanilla's offset-by-one form. For
/// `ByteBufCodecs.holder`, wire zero means an inline direct payload; for
/// `OptionalUnsignedInt`, wire zero means absent. A value is therefore the
/// zero-based id plus one. The offset shares the signed VarInt's domain with
/// the id, so the encodable range stops one short of `i32::MAX`; a wider id
/// saturates there instead of wrapping into a negative id or zero.
fn write_varint_id_plus_one(w: &mut WireWriter, id: u32) {
    debug_assert!(
        id < i32::MAX as u32,
        "offset registry id exceeds the signed wire domain"
    );
    let encoded = i32::try_from(id)
        .ok()
        .and_then(|id| id.checked_add(1))
        .unwrap_or(i32::MAX);
    w.write_varint(encoded);
}

impl MetaValue {
    /// The serializer this value's layout belongs to.
    pub fn serializer(&self) -> MetaSerializer {
        match self {
            Self::Byte(_) => MetaSerializer::Byte,
            Self::Int(_) => MetaSerializer::Int,
            Self::Long(_) => MetaSerializer::Long,
            Self::Float(_) => MetaSerializer::Float,
            Self::String(_) => MetaSerializer::String,
            Self::Component(_) => MetaSerializer::Component,
            Self::OptionalComponent(_) => MetaSerializer::OptionalComponent,
            Self::ItemStack(_) => MetaSerializer::ItemStack,
            Self::Boolean(_) => MetaSerializer::Boolean,
            Self::Rotations(_) => MetaSerializer::Rotations,
            Self::BlockPos(_) => MetaSerializer::BlockPos,
            Self::OptionalBlockPos(_) => MetaSerializer::OptionalBlockPos,
            Self::Direction(_) => MetaSerializer::Direction,
            Self::OptionalUuid(_) => MetaSerializer::OptionalUuid,
            Self::BlockState(_) => MetaSerializer::BlockState,
            Self::OptionalBlockState(_) => MetaSerializer::OptionalBlockState,
            Self::CompoundTag(_) => MetaSerializer::CompoundTag,
            Self::Particle(_) => MetaSerializer::Particle,
            Self::Particles(_) => MetaSerializer::Particles,
            Self::VillagerData { .. } => MetaSerializer::VillagerData,
            Self::OptionalUnsignedInt(_) => MetaSerializer::OptionalUnsignedInt,
            Self::Pose(_) => MetaSerializer::Pose,
            Self::CatVariant(_) => MetaSerializer::CatVariant,
            Self::WolfVariant(_) => MetaSerializer::WolfVariant,
            Self::FrogVariant(_) => MetaSerializer::FrogVariant,
            Self::OptionalGlobalPos(_) => MetaSerializer::OptionalGlobalPos,
            Self::PaintingVariant(_) => MetaSerializer::PaintingVariant,
            Self::SnifferState(_) => MetaSerializer::SnifferState,
            Self::ArmadilloState(_) => MetaSerializer::ArmadilloState,
            Self::Vector3(_) => MetaSerializer::Vector3,
            Self::Quaternion(_) => MetaSerializer::Quaternion,
        }
    }

    /// A `BlockPos` value packed from its three coordinates.
    pub fn packed_block_pos(x: i32, y: i32, z: i32) -> Self {
        Self::BlockPos(crate::wire::BlockPos::new(x, y, z).pack())
    }

    /// Append this value's wire form. The serializer discriminator is written
    /// by the entry, so this is the payload alone.
    fn write_into(&self, w: &mut WireWriter) {
        match self {
            Self::Byte(value) => w.write_u8(*value),
            Self::Int(value) => w.write_varint(*value),
            Self::Long(value) => w.write_varlong(*value),
            Self::Float(value) => w.write_f32(*value),
            Self::String(value) => w.write_string(value),
            Self::Component(nbt) | Self::CompoundTag(nbt) => w.raw(nbt),
            Self::OptionalComponent(Some(nbt)) => {
                w.write_bool(true);
                w.raw(nbt);
            }
            Self::OptionalComponent(None) => w.write_bool(false),
            Self::ItemStack(slot) => w.raw(slot),
            Self::Boolean(value) => w.write_bool(*value),
            Self::Rotations([x, y, z]) => {
                w.write_f32(*x);
                w.write_f32(*y);
                w.write_f32(*z);
            }
            Self::BlockPos(packed) => w.write_i64(*packed),
            Self::OptionalBlockPos(Some(packed)) => {
                w.write_bool(true);
                w.write_i64(*packed);
            }
            Self::OptionalBlockPos(None) => w.write_bool(false),
            Self::Direction(value) | Self::Pose(value) => w.write_varint(*value),
            Self::OptionalUuid(Some(uuid)) => {
                w.write_bool(true);
                w.write_uuid(uuid);
            }
            Self::OptionalUuid(None) => w.write_bool(false),
            Self::BlockState(value) | Self::OptionalBlockState(value) => w.write_varint(*value),
            Self::Particle(payload) => w.raw(payload),
            Self::Particles(parts) => {
                w.write_varint(parts.len() as i32);
                for part in parts {
                    w.raw(part);
                }
            }
            Self::VillagerData {
                kind,
                profession,
                level,
            } => {
                w.write_varint(*kind);
                w.write_varint(*profession);
                w.write_varint(*level);
            }
            Self::OptionalUnsignedInt(None) => w.write_varint(0),
            Self::OptionalUnsignedInt(Some(id)) => {
                // Vanilla offsets the id by one so that 0 can mean "absent".
                write_varint_id_plus_one(w, *id);
            }
            Self::CatVariant(value)
            | Self::FrogVariant(value)
            | Self::SnifferState(value)
            | Self::ArmadilloState(value) => w.write_varint(*value),
            Self::WolfVariant(id) | Self::PaintingVariant(id) => {
                write_varint_id_plus_one(w, *id);
            }
            Self::OptionalGlobalPos(Some((dimension, packed))) => {
                w.write_bool(true);
                w.write_string(dimension);
                w.write_i64(*packed);
            }
            Self::OptionalGlobalPos(None) => w.write_bool(false),
            Self::Vector3([x, y, z]) => {
                w.write_f32(*x);
                w.write_f32(*y);
                w.write_f32(*z);
            }
            Self::Quaternion([x, y, z, q]) => {
                w.write_f32(*x);
                w.write_f32(*y);
                w.write_f32(*z);
                w.write_f32(*q);
            }
        }
    }
}

/// One `set_entity_data` entry: a class-tree index and the value that belongs
/// to it. Build it through [`MetaEntry::new`] (or a typed constructor), never
/// through struct literal syntax — the index and serializer pair is the thing
/// [`validate_entry`] checks.
#[derive(Clone, Debug, PartialEq)]
pub struct MetaEntry {
    index: u8,
    value: MetaValue,
}

impl MetaEntry {
    /// Pair `index` with `value`.
    ///
    /// # Panics
    ///
    /// Rejects index `0xff`, which is the end-of-list terminator rather than a
    /// slot: writing it would truncate the body at that entry.
    pub fn new(index: u8, value: MetaValue) -> Self {
        assert!(
            index != METADATA_TERMINATOR,
            "metadata index 0xff is the end-of-list terminator, not a slot"
        );
        Self { index, value }
    }

    /// The class-tree index this entry writes.
    pub fn index(&self) -> u8 {
        self.index
    }

    /// The typed value this entry carries.
    pub fn value(&self) -> &MetaValue {
        &self.value
    }

    /// The serializer id the wire form will carry, derived from the value.
    pub fn serializer(&self) -> MetaSerializer {
        self.value.serializer()
    }

    /// Check this entry against `kind`'s 1.21.1 schema. Shorthand for
    /// [`validate_entry`] with the entry's own index and serializer.
    pub fn validate(&self, kind: MetaEntityKind) -> Result<(), &'static str> {
        validate_entry(kind, self.index, self.serializer())
    }
}

/// The protocol-767 clientbound play id of `set_entity_data`, resolved from
/// the canonical 767 packet table.
///
/// The value is looked up once and cached. A table that lacks the packet is a
/// broken static table rather than a runtime condition, so the lookup panics
/// on the spot instead of returning a caller-chosen fallback that would put a
/// wrong id on the wire and send every later frame to the wrong handler.
pub fn set_entity_data_packet_id() -> i32 {
    static ID: OnceLock<i32> = OnceLock::new();
    *ID.get_or_init(|| {
        V767_PACKETS
            .iter()
            .find(|entry| {
                entry.0 == PacketPhase::Play
                    && entry.1 == PacketDir::Clientbound
                    && entry.2 == SET_ENTITY_DATA_PACKET
            })
            .and_then(|entry| i32::try_from(entry.3).ok())
            .expect("the 767 table needs a clientbound play set_entity_data id that fits an i32")
    })
}

/// The `set_entity_data` packet *body*: `VarInt(entity id)`, then every entry
/// as `u8(index) | VarInt(serializer) | payload`, then [`METADATA_TERMINATOR`].
///
/// The entity is listed by id rather than being the packet's subject, and the
/// entry count is not on the wire: the terminator is the only end marker, so
/// an empty `entries` slice is a valid, complete body.
pub fn encode_set_entity_data_payload(entity_id: i32, entries: &[MetaEntry]) -> Vec<u8> {
    let mut w = WireWriter::with_capacity(1 + entries.len() * 8);
    w.write_varint(entity_id);
    for entry in entries {
        w.write_u8(entry.index);
        w.write_varint(i32::from(entry.serializer().id()));
        entry.value.write_into(&mut w);
    }
    w.write_u8(METADATA_TERMINATOR);
    w.into_vec()
}

/// The complete `set_entity_data` packet: `VarInt(len) | VarInt(id) | body`,
/// with the id taken from [`set_entity_data_packet_id`].
///
/// Use this when writing to a socket; use [`encode_set_entity_data_payload`]
/// only for a pipeline that adds the frame itself.
pub fn encode_set_entity_data(entity_id: i32, entries: &[MetaEntry]) -> Vec<u8> {
    let payload = encode_set_entity_data_payload(entity_id, entries);
    let mut frame = Vec::with_capacity(payload.len() + 10);
    write_frame(&mut frame, set_entity_data_packet_id(), &payload);
    frame
}

/// [`encode_set_entity_data`] after checking every entry against `kind`'s 1.21.1
/// schema.
///
/// This is the encode path for a call site that knows the entity class: a
/// foreign index or a mismatched serializer is refused before any bytes are
/// built, so the guard cannot be forgotten. [`encode_set_entity_data`] stays
/// available for bodies whose class the caller does not have (mixed batches,
/// replay tooling) and for pipelines that validate entries with
/// [`MetaEntry::validate`] themselves.
pub fn encode_set_entity_data_checked(
    kind: MetaEntityKind,
    entity_id: i32,
    entries: &[MetaEntry],
) -> Result<Vec<u8>, &'static str> {
    for entry in entries {
        entry.validate(kind)?;
    }
    Ok(encode_set_entity_data(entity_id, entries))
}

/// The 1.21.1 `ClassTreeIdRegistry` indices, grouped by the class that defines
/// them.
///
/// Vanilla allocates one contiguous range per class, in `defineId` order,
/// starting after the parent's last index. That is why indices repeat across
/// unrelated branches (`15` is `Mob`'s flags *and* `Player`'s absorption) and
/// why an index is only meaningful together with its class tree — the source
/// of every wrong-index defect. Name the field, not the number.
pub mod meta_index {
    /// `net.minecraft.world.entity.Entity` — the root of every class tree.
    pub mod entity {
        /// `DATA_SHARED_FLAGS_ID` — `Byte`, the on-fire/crouching/sprinting
        /// bit field.
        pub const SHARED_FLAGS: u8 = 0;
        /// `DATA_AIR_SUPPLY_ID` — `Int`, remaining ticks of air.
        pub const AIR_SUPPLY: u8 = 1;
        /// `DATA_CUSTOM_NAME` — `OptionalComponent`.
        pub const CUSTOM_NAME: u8 = 2;
        /// `DATA_CUSTOM_NAME_VISIBLE` — `Boolean`.
        pub const CUSTOM_NAME_VISIBLE: u8 = 3;
        /// `DATA_SILENT` — `Boolean`.
        pub const SILENT: u8 = 4;
        /// `DATA_NO_GRAVITY` — `Boolean`.
        pub const NO_GRAVITY: u8 = 5;
        /// `DATA_POSE` — `Pose`.
        pub const POSE: u8 = 6;
        /// `DATA_TICKS_FROZEN` — `Int`.
        pub const TICKS_FROZEN: u8 = 7;
    }

    /// `net.minecraft.world.entity.LivingEntity`.
    pub mod living_entity {
        /// `DATA_LIVING_ENTITY_FLAGS` — `Byte`, the hand-active/riptide bits.
        pub const LIVING_FLAGS: u8 = 8;
        /// `DATA_HEALTH_ID` — `Float`. Not `8`.
        pub const HEALTH: u8 = 9;
        /// `DATA_EFFECT_PARTICLES` — `Particles`.
        pub const EFFECT_PARTICLES: u8 = 10;
        /// `DATA_EFFECT_AMBIENCE_ID` — `Boolean`.
        pub const EFFECT_AMBIENCE: u8 = 11;
        /// `DATA_ARROW_COUNT_ID` — `Int`.
        pub const ARROW_COUNT: u8 = 12;
        /// `DATA_STINGER_COUNT_ID` — `Int`.
        pub const STINGER_COUNT: u8 = 13;
        /// `DATA_SLEEPING_POS_ID` — `OptionalBlockPos`. A packed position where
        /// a state ordinal would silently consume eight extra bytes.
        pub const SLEEPING_POS: u8 = 14;
    }

    /// `net.minecraft.world.entity.Mob`.
    pub mod mob {
        /// `DATA_MOB_FLAGS_ID` — `Byte`, the no-ai/left-handed/aggressive bits.
        pub const MOB_FLAGS: u8 = 15;
    }

    /// `net.minecraft.world.entity.player.Player` — extends `LivingEntity`, so
    /// its range starts at `15` and collides with
    /// [`crate::entity_metadata::meta_index::mob::MOB_FLAGS`].
    pub mod player {
        /// `DATA_PLAYER_ABSORPTION_ID` — `Float`.
        pub const ABSORPTION: u8 = 15;
        /// `DATA_SCORE_ID` — `Int`.
        pub const SCORE: u8 = 16;
        /// `DATA_PLAYER_MODE_CUSTOMISATION` — `Byte`.
        pub const MODEL_CUSTOMISATION: u8 = 17;
        /// `DATA_PLAYER_MAIN_HAND` — `Byte`.
        pub const MAIN_HAND: u8 = 18;
        /// `DATA_SHOULDER_ENTITY_LEFT` — `CompoundTag`.
        pub const SHOULDER_LEFT: u8 = 19;
        /// `DATA_SHOULDER_ENTITY_RIGHT` — `CompoundTag`.
        pub const SHOULDER_RIGHT: u8 = 20;
    }

    /// `net.minecraft.world.entity.AgeableMob`.
    pub mod ageable_mob {
        /// `DATA_BABY_ID` — `Boolean`.
        pub const BABY: u8 = 16;
    }

    /// `net.minecraft.world.entity.npc.Villager`.
    pub mod villager {
        /// `DATA_UNHAPPY_COUNTER` — `Int`.
        pub const UNHAPPY_COUNTER: u8 = 17;
        /// `DATA_VILLAGER_DATA` — `VillagerData`.
        pub const VILLAGER_DATA: u8 = 18;
    }

    /// `net.minecraft.world.entity.animal.Sheep`.
    pub mod sheep {
        /// `DATA_WOOL_ID` — `Byte`, the wool colour bits. Not `16` (`BABY`).
        pub const WOOL_ID: u8 = 17;
    }

    /// `net.minecraft.world.entity.TamableAnimal`.
    pub mod tamable_animal {
        /// `DATA_FLAGS_ID` — `Byte`, sitting/tame bits.
        pub const FLAGS: u8 = 17;
        /// `DATA_OWNERUUID_ID` — `OptionalUuid`.
        pub const OWNER_UUID: u8 = 18;
    }

    /// `net.minecraft.world.entity.animal.Wolf`.
    pub mod wolf {
        /// `DATA_INTERESTED_ID` — `Boolean`.
        pub const INTERESTED: u8 = 19;
        /// `DATA_COLLAR_COLOR` — `Int`.
        pub const COLLAR_COLOR: u8 = 20;
        /// `DATA_ANGER_END_TIME_ID` — `Int`.
        pub const ANGER_TIME: u8 = 21;
        /// `DATA_VARIANT_ID` — `WolfVariant`.
        pub const VARIANT: u8 = 22;
    }

    /// `net.minecraft.world.entity.animal.Cat`.
    pub mod cat {
        /// `DATA_VARIANT_ID` — `CatVariant`.
        pub const VARIANT: u8 = 19;
        /// `DATA_IS_LYING` — `Boolean`.
        pub const IS_LYING: u8 = 20;
        /// `DATA_RELAX_STATE_ONE` — `Boolean`.
        pub const RELAX_STATE_ONE: u8 = 21;
        /// `DATA_COLLAR_COLOR` — `Int`. Not `21` (`RELAX_STATE_ONE`), and not
        /// the wolf's `20`.
        pub const COLLAR_COLOR: u8 = 22;
    }

    /// `net.minecraft.world.entity.animal.horse.AbstractHorse`.
    pub mod abstract_horse {
        /// `DATA_ID_FLAGS` — `Byte`, the tamed/saddled/bred bits.
        pub const FLAGS: u8 = 17;
    }

    /// `net.minecraft.world.entity.animal.horse.AbstractChestedHorse`.
    pub mod abstract_chested_horse {
        /// `DATA_ID_CHEST` — `Boolean`.
        pub const CHEST: u8 = 18;
    }

    /// `net.minecraft.world.entity.animal.horse.Horse`.
    pub mod horse {
        /// `DATA_ID_TYPE_VARIANT` — `Int`, the coat/pattern bits.
        pub const VARIANT: u8 = 18;
    }

    /// `net.minecraft.world.entity.monster.Creeper`.
    pub mod creeper {
        /// `DATA_SWELL_DIR` — `Int`, `-1..=1`.
        pub const SWELL_DIR: u8 = 16;
        /// `DATA_IS_POWERED` — `Boolean`, the charged state.
        pub const IS_POWERED: u8 = 17;
        /// `DATA_IS_IGNITED` — `Boolean`.
        pub const IS_IGNITED: u8 = 18;
    }

    /// `net.minecraft.world.entity.monster.EnderMan`.
    pub mod ender_man {
        /// `DATA_CARRY_STATE` — `OptionalBlockState`.
        pub const CARRY_STATE: u8 = 16;
        /// `DATA_CREEPY` — `Boolean`.
        pub const CREEPY: u8 = 17;
        /// `DATA_STARED_AT` — `Boolean`.
        pub const STARED_AT: u8 = 18;
    }

    /// `net.minecraft.world.entity.monster.Slime`.
    pub mod slime {
        /// `DATA_ID_SIZE` — `Int`, the size ordinal.
        pub const SIZE: u8 = 16;
    }

    /// `net.minecraft.world.entity.monster.Zombie`.
    pub mod zombie {
        /// `DATA_BABY_ID` — `Boolean`.
        pub const BABY: u8 = 16;
        /// `DATA_SPECIAL_TYPE_ID` — `Int`, the 1.21.1 zombie-variant id.
        pub const SPECIAL_TYPE: u8 = 17;
        /// `DATA_DROWNED_CONVERSION_ID` — `Boolean`.
        pub const DROWNED_CONVERSION: u8 = 18;
    }

    /// `net.minecraft.world.entity.decoration.ArmorStand`.
    pub mod armor_stand {
        /// `DATA_CLIENT_FLAGS` — `Byte`, the small/invisible/base-plate bits.
        pub const CLIENT_FLAGS: u8 = 15;
        /// `DATA_HEAD_POSE` — `Rotations`.
        pub const HEAD_POSE: u8 = 16;
        /// `DATA_BODY_POSE` — `Rotations`.
        pub const BODY_POSE: u8 = 17;
        /// `DATA_LEFT_ARM_POSE` — `Rotations`.
        pub const LEFT_ARM_POSE: u8 = 18;
        /// `DATA_RIGHT_ARM_POSE` — `Rotations`.
        pub const RIGHT_ARM_POSE: u8 = 19;
        /// `DATA_LEFT_LEG_POSE` — `Rotations`.
        pub const LEFT_LEG_POSE: u8 = 20;
        /// `DATA_RIGHT_LEG_POSE` — `Rotations`. The six pose slots are the
        /// contiguous range `16..=21`; legs follow arms in allocation order.
        pub const RIGHT_LEG_POSE: u8 = 21;
    }

    /// `net.minecraft.world.entity.item.ItemEntity`.
    pub mod item_entity {
        /// `DATA_ITEM` — `ItemStack`.
        pub const ITEM: u8 = 8;
    }

    /// `net.minecraft.world.entity.decoration.ItemFrame`.
    pub mod item_frame {
        /// `DATA_ITEM` — `ItemStack`.
        pub const ITEM: u8 = 8;
        /// `DATA_ROTATION` — `Int`.
        pub const ROTATION: u8 = 9;
    }

    /// `net.minecraft.world.entity.projectile.Arrow`.
    pub mod arrow {
        /// `ID_FLAGS` — `Byte`, the critical/no-clip bits.
        pub const FLAGS: u8 = 8;
        /// `ID_PIERCE_LEVEL` — `Byte`.
        pub const PIERCE_LEVEL: u8 = 9;
        /// `ID_EFFECT_COLOR` — `Int`, the potion tint.
        pub const EFFECT_COLOR: u8 = 10;
    }

    /// `net.minecraft.world.entity.vehicle.VehicleEntity` — inserted into
    /// the vehicle trees in 1.21.1, so vehicle subclasses start after `10`.
    pub mod vehicle_entity {
        /// `DATA_ID_HURT` — `Int`.
        pub const HURT: u8 = 8;
        /// `DATA_ID_HURTDIR` — `Int`.
        pub const HURTDIR: u8 = 9;
        /// `DATA_ID_DAMAGE` — `Float`.
        pub const DAMAGE: u8 = 10;
    }

    /// `net.minecraft.world.entity.vehicle.Boat`.
    pub mod boat {
        /// `DATA_ID_TYPE` — `Int`, the wood type. Not `8`: `VehicleEntity`
        /// owns `8..=10` in 1.21.1.
        pub const TYPE: u8 = 11;
        /// `DATA_ID_PADDLE_LEFT` — `Boolean`.
        pub const PADDLE_LEFT: u8 = 12;
        /// `DATA_ID_PADDLE_RIGHT` — `Boolean`.
        pub const PADDLE_RIGHT: u8 = 13;
        /// `DATA_ID_BUBBLE_TIME` — `Int`.
        pub const BUBBLE_TIME: u8 = 14;
    }

    /// `net.minecraft.world.entity.vehicle.AbstractMinecart`.
    pub mod abstract_minecart {
        /// `DATA_ID_DISPLAY_BLOCK` — `Int`, a block-state id in an `Int` slot.
        /// Not `8`: `VehicleEntity` owns `8..=10` in 1.21.1.
        pub const DISPLAY_BLOCK: u8 = 11;
        /// `DATA_ID_DISPLAY_OFFSET` — `Int`.
        pub const DISPLAY_OFFSET: u8 = 12;
        /// `DATA_ID_CUSTOM_DISPLAY` — `Boolean`.
        pub const CUSTOM_DISPLAY: u8 = 13;
    }

    /// `net.minecraft.world.entity.boss.wither.WitherBoss`.
    pub mod wither_boss {
        /// `DATA_TARGET_A` — `Int`.
        pub const TARGET_A: u8 = 16;
        /// `DATA_TARGET_B` — `Int`.
        pub const TARGET_B: u8 = 17;
        /// `DATA_TARGET_C` — `Int`.
        pub const TARGET_C: u8 = 18;
        /// `DATA_ID_INV` — `Int`, the invulnerability ticks.
        pub const INVULNERABLE_TICKS: u8 = 19;
    }

    /// `net.minecraft.world.entity.monster.warden.Warden`.
    pub mod warden {
        /// `CLIENT_ANGER_LEVEL` — `Int`.
        pub const CLIENT_ANGER_LEVEL: u8 = 16;
    }

    /// `net.minecraft.world.entity.animal.armadillo.Armadillo`.
    pub mod armadillo {
        /// `ARMADILLO_STATE` — `ArmadilloState`.
        pub const STATE: u8 = 17;
    }

    /// `net.minecraft.world.entity.boss.enderdragon.EndCrystal`.
    pub mod end_crystal {
        /// `DATA_BEAM_TARGET` — `OptionalBlockPos`.
        pub const BEAM_TARGET: u8 = 8;
        /// `DATA_SHOW_BOTTOM` — `Boolean`.
        pub const SHOW_BOTTOM: u8 = 9;
    }

    /// `net.minecraft.world.entity.boss.enderdragon.EnderDragon`.
    pub mod ender_dragon {
        /// `DATA_PHASE` — `Int`, the dragon phase ordinal.
        pub const PHASE: u8 = 16;
    }

    /// `net.minecraft.world.entity.animal.horse.Llama`.
    pub mod llama {
        /// `DATA_STRENGTH_ID` — `Int`.
        pub const STRENGTH: u8 = 19;
        /// `DATA_VARIANT_ID` — `Int`.
        pub const VARIANT: u8 = 20;
    }

    /// `net.minecraft.world.entity.animal.sniffer.Sniffer`.
    pub mod sniffer {
        /// `DATA_STATE` — `SnifferState`.
        pub const STATE: u8 = 17;
        /// `DATA_DROP_SEED_AT_TICK` — `Int`.
        pub const DROP_SEED_AT_TICK: u8 = 18;
    }

    /// `net.minecraft.world.entity.animal.SnowGolem`.
    pub mod snow_golem {
        /// `DATA_PUMPKIN_ID` — `Byte`.
        pub const PUMPKIN: u8 = 16;
    }

    /// `net.minecraft.world.entity.animal.allay.Allay`.
    pub mod allay {
        /// `DATA_DANCING` — `Boolean`.
        pub const DANCING: u8 = 16;
        /// `DATA_CAN_DUPLICATE` — `Boolean`.
        pub const CAN_DUPLICATE: u8 = 17;
    }

    /// `net.minecraft.world.entity.monster.Blaze`.
    pub mod blaze {
        /// `DATA_FLAGS_ID` — `Byte`.
        pub const FLAGS: u8 = 16;
    }

    /// `net.minecraft.world.entity.animal.IronGolem`.
    pub mod iron_golem {
        /// `DATA_FLAGS_ID` — `Byte`.
        pub const FLAGS: u8 = 16;
    }

    /// `net.minecraft.world.entity.projectile.FireworkRocketEntity`.
    pub mod firework_rocket {
        /// `DATA_ID_FIREWORKS_ITEM` — `ItemStack`.
        pub const ITEM: u8 = 8;
        /// `DATA_ATTACHED_TO_TARGET` — `OptionalUnsignedInt`, the boosted
        /// entity id.
        pub const ATTACHED_TO: u8 = 9;
        /// `DATA_ID_SHOT_AT_ANGLE` — `Boolean`.
        pub const SHOT_AT_ANGLE: u8 = 10;
    }

    /// `net.minecraft.world.entity.Display` — fifteen slots, `8..=22`.
    pub mod display {
        /// `DATA_INTERPOLATION_START_DELTA_TICKS_ID` — `Int`.
        pub const INTERPOLATION_START_DELTA_TICKS: u8 = 8;
        /// `DATA_INTERPOLATION_DURATION_ID` — `Int`.
        pub const INTERPOLATION_DURATION: u8 = 9;
        /// `DATA_POS_ROT_INTERPOLATION_DURATION_ID` — `Int`.
        pub const TELEPORT_DURATION: u8 = 10;
        /// `DATA_TRANSLATION_ID` — `Vector3`.
        pub const TRANSLATION: u8 = 11;
        /// `DATA_SCALE_ID` — `Vector3`.
        pub const SCALE: u8 = 12;
        /// `DATA_LEFT_ROTATION_ID` — `Quaternion`.
        pub const LEFT_ROTATION: u8 = 13;
        /// `DATA_RIGHT_ROTATION_ID` — `Quaternion`.
        pub const RIGHT_ROTATION: u8 = 14;
        /// `DATA_BILLBOARD_RENDER_CONSTRAINTS_ID` — `Byte`.
        pub const BILLBOARD_RENDER_CONSTRAINTS: u8 = 15;
        /// `DATA_BRIGHTNESS_OVERRIDE_ID` — `Int`.
        pub const BRIGHTNESS_OVERRIDE: u8 = 16;
        /// `DATA_VIEW_RANGE_ID` — `Float`.
        pub const VIEW_RANGE: u8 = 17;
        /// `DATA_SHADOW_RADIUS_ID` — `Float`.
        pub const SHADOW_RADIUS: u8 = 18;
        /// `DATA_SHADOW_STRENGTH_ID` — `Float`.
        pub const SHADOW_STRENGTH: u8 = 19;
        /// `DATA_WIDTH_ID` — `Float`.
        pub const WIDTH: u8 = 20;
        /// `DATA_HEIGHT_ID` — `Float`.
        pub const HEIGHT: u8 = 21;
        /// `DATA_GLOW_COLOR_OVERRIDE_ID` — `Int`.
        pub const GLOW_COLOR_OVERRIDE: u8 = 22;
    }

    /// `net.minecraft.world.entity.Display.TextDisplay`.
    pub mod text_display {
        /// `DATA_BACKGROUND_COLOR_ID` — `Int`.
        pub const BACKGROUND: u8 = 23;
        /// `DATA_TEXT_ID` — `Component`.
        pub const TEXT: u8 = 24;
        /// `DATA_STYLE_FLAGS_ID` — `Byte`.
        pub const STYLE_FLAGS: u8 = 25;
        /// `DATA_TEXT_LINE_WIDTH_ID` — `Int`.
        pub const LINE_WIDTH: u8 = 26;
        /// `DATA_SEE_THROUGH_ID` — `Boolean`.
        pub const SEE_THROUGH: u8 = 27;
    }

    /// `net.minecraft.world.entity.Display.BlockDisplay`.
    pub mod block_display {
        /// `DATA_BLOCK_STATE_ID` — `BlockState`.
        pub const BLOCK_STATE: u8 = 23;
    }
}

/// The `Byte` flag bits the shared `Entity` and `LivingEntity` slots carry.
pub mod flags {
    /// [`super::meta_index::entity::SHARED_FLAGS`] on fire.
    pub const SHARED_ON_FIRE: u8 = 0x01;
    /// [`super::meta_index::entity::SHARED_FLAGS`] crouching.
    pub const SHARED_CROUCHING: u8 = 0x02;
    /// [`super::meta_index::entity::SHARED_FLAGS`] sprinting.
    pub const SHARED_SPRINTING: u8 = 0x08;
    /// [`super::meta_index::entity::SHARED_FLAGS`] swimming.
    pub const SHARED_SWIMMING: u8 = 0x10;
    /// [`super::meta_index::entity::SHARED_FLAGS`] invisible.
    pub const SHARED_INVISIBLE: u8 = 0x20;
    /// [`super::meta_index::entity::SHARED_FLAGS`] glowing.
    pub const SHARED_GLOWING: u8 = 0x40;
    /// [`super::meta_index::entity::SHARED_FLAGS`] fall flying (elytra).
    pub const SHARED_FALL_FLYING: u8 = 0x80;
    /// [`super::meta_index::living_entity::LIVING_FLAGS`] using the main hand.
    pub const LIVING_USE_MAIN_HAND: u8 = 0x01;
    /// [`super::meta_index::living_entity::LIVING_FLAGS`] using the off hand.
    pub const LIVING_USE_OFF_HAND: u8 = 0x02;
    /// [`super::meta_index::living_entity::LIVING_FLAGS`] spinning (riptide).
    pub const LIVING_SPIN_ATTACK: u8 = 0x04;
}

/// `Entity` shared flags.
pub fn shared_flags(bits: u8) -> MetaEntry {
    MetaEntry::new(meta_index::entity::SHARED_FLAGS, MetaValue::Byte(bits))
}

/// `Entity` air supply, in ticks (`300` for a sea-level entity in vanilla).
pub fn air_supply(ticks: i32) -> MetaEntry {
    MetaEntry::new(meta_index::entity::AIR_SUPPLY, MetaValue::Int(ticks))
}

/// `Entity` custom name: a pre-encoded NBT component, or `None` to clear it.
/// Vanilla also sets [`custom_name_visible`] when a name is applied.
pub fn custom_name(component: Option<Vec<u8>>) -> MetaEntry {
    MetaEntry::new(
        meta_index::entity::CUSTOM_NAME,
        MetaValue::OptionalComponent(component),
    )
}

/// Whether the custom name renders above the entity.
pub fn custom_name_visible(visible: bool) -> MetaEntry {
    MetaEntry::new(
        meta_index::entity::CUSTOM_NAME_VISIBLE,
        MetaValue::Boolean(visible),
    )
}

/// `Entity` silent: suppresses the entity's sounds client-side.
pub fn silent(silent: bool) -> MetaEntry {
    MetaEntry::new(meta_index::entity::SILENT, MetaValue::Boolean(silent))
}

/// `Entity` pose ordinal. 1.21.1 `Pose` order: `STANDING` 0, `FALL_FLYING` 1,
/// `SLEEPING` 2, `SWIMMING` 3, `SPIN_ATTACK` 4, `CROUCHING` 5, `LONG_JUMPING` 6.
pub fn entity_pose(pose: i32) -> MetaEntry {
    MetaEntry::new(meta_index::entity::POSE, MetaValue::Pose(pose))
}

/// `Entity` frozen ticks — the powder-snow freeze counter.
pub fn ticks_frozen(ticks: i32) -> MetaEntry {
    MetaEntry::new(meta_index::entity::TICKS_FROZEN, MetaValue::Int(ticks))
}

/// `LivingEntity` hand-active/riptide bits; see [`flags`].
pub fn living_flags(bits: u8) -> MetaEntry {
    MetaEntry::new(
        meta_index::living_entity::LIVING_FLAGS,
        MetaValue::Byte(bits),
    )
}

/// `LivingEntity` health. Index `9` — the field a `Float` at `8` never reaches.
pub fn living_health(health: f32) -> MetaEntry {
    MetaEntry::new(meta_index::living_entity::HEALTH, MetaValue::Float(health))
}

/// `LivingEntity` bed position, packed with [`crate::wire::BlockPos::pack`];
/// `None` wakes the entity client-side.
pub fn living_sleeping_pos(pos: Option<i64>) -> MetaEntry {
    MetaEntry::new(
        meta_index::living_entity::SLEEPING_POS,
        MetaValue::OptionalBlockPos(pos),
    )
}

/// `Mob` flags; bit `0x01` is no-ai, `0x02` left-handed, `0x04` aggressive.
pub fn mob_flags(bits: u8) -> MetaEntry {
    MetaEntry::new(meta_index::mob::MOB_FLAGS, MetaValue::Byte(bits))
}

/// `Villager` type/profession/level triple.
pub fn villager_data(kind: i32, profession: i32, level: i32) -> MetaEntry {
    MetaEntry::new(
        meta_index::villager::VILLAGER_DATA,
        MetaValue::VillagerData {
            kind,
            profession,
            level,
        },
    )
}

/// `Sheep` wool colour bits. Index `17` — not `16`, which is `BABY`.
pub fn sheep_wool(color: u8) -> MetaEntry {
    MetaEntry::new(meta_index::sheep::WOOL_ID, MetaValue::Byte(color))
}

/// `Cat` collar colour, a `0..=15` dye id. Index `22` — not the wolf's `20`,
/// and not the `21` a cat's `RELAX_STATE_ONE` occupies.
pub fn cat_collar(color: i32) -> MetaEntry {
    MetaEntry::new(meta_index::cat::COLLAR_COLOR, MetaValue::Int(color))
}

/// `Wolf` collar colour, a `0..=15` dye id.
pub fn wolf_collar(color: i32) -> MetaEntry {
    MetaEntry::new(meta_index::wolf::COLLAR_COLOR, MetaValue::Int(color))
}

/// `Creeper` charged state.
pub fn creeper_powered(powered: bool) -> MetaEntry {
    MetaEntry::new(meta_index::creeper::IS_POWERED, MetaValue::Boolean(powered))
}

/// `ItemEntity` stack, pre-encoded as one 1.21.1 `Slot`.
pub fn item_entity_stack(slot: Vec<u8>) -> MetaEntry {
    MetaEntry::new(meta_index::item_entity::ITEM, MetaValue::ItemStack(slot))
}

/// A concrete 1.21.1 entity class whose metadata schema this crate knows.
///
/// The variants mirror the class tree: a kind accepts every index its ancestors
/// define, plus its own. `Animal` defines no metadata of its own in 1.21.1, so
/// it has no variant and contributes nothing between `AgeableMob` and the
/// species that extend it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MetaEntityKind {
    /// `Entity`.
    Entity,
    /// `LivingEntity`.
    LivingEntity,
    /// `Mob`.
    Mob,
    /// `Player`.
    Player,
    /// `AgeableMob`.
    AgeableMob,
    /// `Villager`.
    Villager,
    /// `Sheep`.
    Sheep,
    /// `TamableAnimal`.
    TamableAnimal,
    /// `Wolf`.
    Wolf,
    /// `Cat`.
    Cat,
    /// `AbstractHorse`.
    AbstractHorse,
    /// `AbstractChestedHorse`.
    AbstractChestedHorse,
    /// `Horse`.
    Horse,
    /// `Creeper`.
    Creeper,
    /// `EnderMan`.
    EnderMan,
    /// `Slime`.
    Slime,
    /// `Zombie`.
    Zombie,
    /// `ArmorStand`.
    ArmorStand,
    /// `ItemEntity`.
    ItemEntity,
    /// `ItemFrame`.
    ItemFrame,
    /// `Arrow`.
    Arrow,
    /// `VehicleEntity`.
    VehicleEntity,
    /// `Boat`.
    Boat,
    /// `AbstractMinecart`.
    AbstractMinecart,
    /// `WitherBoss`.
    WitherBoss,
    /// `Warden`.
    Warden,
    /// `Breeze`.
    Breeze,
    /// `Armadillo`.
    Armadillo,
    /// `EndCrystal`.
    EndCrystal,
    /// `EnderDragon`.
    EnderDragon,
    /// `Llama`.
    Llama,
    /// `Sniffer`.
    Sniffer,
    /// `SnowGolem`.
    SnowGolem,
    /// `Allay`.
    Allay,
    /// `Blaze`.
    Blaze,
    /// `IronGolem`.
    IronGolem,
    /// `FireworkRocket`.
    FireworkRocket,
    /// `Display`.
    Display,
    /// `TextDisplay`.
    TextDisplay,
    /// `BlockDisplay`.
    BlockDisplay,
}

mod schema {
    //! The serializer each index must carry, per class. Kept separate from
    //! [`super::meta_index`] so the numeric table stays a plain list of
    //! numbers and this table stays a plain list of types.

    use super::{meta_index, MetaSerializer};

    /// One `(index, serializer)` row of a class's own `defineId` block.
    pub(super) type Row = (u8, MetaSerializer);

    pub(super) const ENTITY: &[Row] = &[
        (meta_index::entity::SHARED_FLAGS, MetaSerializer::Byte),
        (meta_index::entity::AIR_SUPPLY, MetaSerializer::Int),
        (
            meta_index::entity::CUSTOM_NAME,
            MetaSerializer::OptionalComponent,
        ),
        (
            meta_index::entity::CUSTOM_NAME_VISIBLE,
            MetaSerializer::Boolean,
        ),
        (meta_index::entity::SILENT, MetaSerializer::Boolean),
        (meta_index::entity::NO_GRAVITY, MetaSerializer::Boolean),
        (meta_index::entity::POSE, MetaSerializer::Pose),
        (meta_index::entity::TICKS_FROZEN, MetaSerializer::Int),
    ];

    pub(super) const LIVING_ENTITY: &[Row] = &[
        (
            meta_index::living_entity::LIVING_FLAGS,
            MetaSerializer::Byte,
        ),
        (meta_index::living_entity::HEALTH, MetaSerializer::Float),
        (
            meta_index::living_entity::EFFECT_PARTICLES,
            MetaSerializer::Particles,
        ),
        (
            meta_index::living_entity::EFFECT_AMBIENCE,
            MetaSerializer::Boolean,
        ),
        (meta_index::living_entity::ARROW_COUNT, MetaSerializer::Int),
        (
            meta_index::living_entity::STINGER_COUNT,
            MetaSerializer::Int,
        ),
        (
            meta_index::living_entity::SLEEPING_POS,
            MetaSerializer::OptionalBlockPos,
        ),
    ];

    pub(super) const MOB: &[Row] = &[(meta_index::mob::MOB_FLAGS, MetaSerializer::Byte)];

    pub(super) const PLAYER: &[Row] = &[
        (meta_index::player::ABSORPTION, MetaSerializer::Float),
        (meta_index::player::SCORE, MetaSerializer::Int),
        (
            meta_index::player::MODEL_CUSTOMISATION,
            MetaSerializer::Byte,
        ),
        (meta_index::player::MAIN_HAND, MetaSerializer::Byte),
        (
            meta_index::player::SHOULDER_LEFT,
            MetaSerializer::CompoundTag,
        ),
        (
            meta_index::player::SHOULDER_RIGHT,
            MetaSerializer::CompoundTag,
        ),
    ];

    pub(super) const AGEABLE_MOB: &[Row] =
        &[(meta_index::ageable_mob::BABY, MetaSerializer::Boolean)];

    pub(super) const VILLAGER: &[Row] = &[
        (meta_index::villager::UNHAPPY_COUNTER, MetaSerializer::Int),
        (
            meta_index::villager::VILLAGER_DATA,
            MetaSerializer::VillagerData,
        ),
    ];

    pub(super) const SHEEP: &[Row] = &[(meta_index::sheep::WOOL_ID, MetaSerializer::Byte)];

    pub(super) const TAMABLE_ANIMAL: &[Row] = &[
        (meta_index::tamable_animal::FLAGS, MetaSerializer::Byte),
        (
            meta_index::tamable_animal::OWNER_UUID,
            MetaSerializer::OptionalUuid,
        ),
    ];

    pub(super) const WOLF: &[Row] = &[
        (meta_index::wolf::INTERESTED, MetaSerializer::Boolean),
        (meta_index::wolf::COLLAR_COLOR, MetaSerializer::Int),
        (meta_index::wolf::ANGER_TIME, MetaSerializer::Int),
        (meta_index::wolf::VARIANT, MetaSerializer::WolfVariant),
    ];

    pub(super) const CAT: &[Row] = &[
        (meta_index::cat::VARIANT, MetaSerializer::CatVariant),
        (meta_index::cat::IS_LYING, MetaSerializer::Boolean),
        (meta_index::cat::RELAX_STATE_ONE, MetaSerializer::Boolean),
        (meta_index::cat::COLLAR_COLOR, MetaSerializer::Int),
    ];

    pub(super) const ABSTRACT_HORSE: &[Row] =
        &[(meta_index::abstract_horse::FLAGS, MetaSerializer::Byte)];

    pub(super) const ABSTRACT_CHESTED_HORSE: &[Row] = &[(
        meta_index::abstract_chested_horse::CHEST,
        MetaSerializer::Boolean,
    )];

    pub(super) const HORSE: &[Row] = &[(meta_index::horse::VARIANT, MetaSerializer::Int)];

    pub(super) const CREEPER: &[Row] = &[
        (meta_index::creeper::SWELL_DIR, MetaSerializer::Int),
        (meta_index::creeper::IS_POWERED, MetaSerializer::Boolean),
        (meta_index::creeper::IS_IGNITED, MetaSerializer::Boolean),
    ];

    pub(super) const ENDER_MAN: &[Row] = &[
        (
            meta_index::ender_man::CARRY_STATE,
            MetaSerializer::OptionalBlockState,
        ),
        (meta_index::ender_man::CREEPY, MetaSerializer::Boolean),
        (meta_index::ender_man::STARED_AT, MetaSerializer::Boolean),
    ];

    pub(super) const SLIME: &[Row] = &[(meta_index::slime::SIZE, MetaSerializer::Int)];

    pub(super) const ZOMBIE: &[Row] = &[
        (meta_index::zombie::BABY, MetaSerializer::Boolean),
        (meta_index::zombie::SPECIAL_TYPE, MetaSerializer::Int),
        (
            meta_index::zombie::DROWNED_CONVERSION,
            MetaSerializer::Boolean,
        ),
    ];

    pub(super) const ARMOR_STAND: &[Row] = &[
        (meta_index::armor_stand::CLIENT_FLAGS, MetaSerializer::Byte),
        (
            meta_index::armor_stand::HEAD_POSE,
            MetaSerializer::Rotations,
        ),
        (
            meta_index::armor_stand::BODY_POSE,
            MetaSerializer::Rotations,
        ),
        (
            meta_index::armor_stand::LEFT_ARM_POSE,
            MetaSerializer::Rotations,
        ),
        (
            meta_index::armor_stand::RIGHT_ARM_POSE,
            MetaSerializer::Rotations,
        ),
        (
            meta_index::armor_stand::LEFT_LEG_POSE,
            MetaSerializer::Rotations,
        ),
        (
            meta_index::armor_stand::RIGHT_LEG_POSE,
            MetaSerializer::Rotations,
        ),
    ];

    pub(super) const ITEM_ENTITY: &[Row] =
        &[(meta_index::item_entity::ITEM, MetaSerializer::ItemStack)];

    pub(super) const ITEM_FRAME: &[Row] = &[
        (meta_index::item_frame::ITEM, MetaSerializer::ItemStack),
        (meta_index::item_frame::ROTATION, MetaSerializer::Int),
    ];

    pub(super) const ARROW: &[Row] = &[
        (meta_index::arrow::FLAGS, MetaSerializer::Byte),
        (meta_index::arrow::PIERCE_LEVEL, MetaSerializer::Byte),
        (meta_index::arrow::EFFECT_COLOR, MetaSerializer::Int),
    ];

    pub(super) const VEHICLE_ENTITY: &[Row] = &[
        (meta_index::vehicle_entity::HURT, MetaSerializer::Int),
        (meta_index::vehicle_entity::HURTDIR, MetaSerializer::Int),
        (meta_index::vehicle_entity::DAMAGE, MetaSerializer::Float),
    ];

    pub(super) const BOAT: &[Row] = &[
        (meta_index::boat::TYPE, MetaSerializer::Int),
        (meta_index::boat::PADDLE_LEFT, MetaSerializer::Boolean),
        (meta_index::boat::PADDLE_RIGHT, MetaSerializer::Boolean),
        (meta_index::boat::BUBBLE_TIME, MetaSerializer::Int),
    ];

    pub(super) const ABSTRACT_MINECART: &[Row] = &[
        (
            meta_index::abstract_minecart::DISPLAY_BLOCK,
            MetaSerializer::Int,
        ),
        (
            meta_index::abstract_minecart::DISPLAY_OFFSET,
            MetaSerializer::Int,
        ),
        (
            meta_index::abstract_minecart::CUSTOM_DISPLAY,
            MetaSerializer::Boolean,
        ),
    ];

    pub(super) const WITHER_BOSS: &[Row] = &[
        (meta_index::wither_boss::TARGET_A, MetaSerializer::Int),
        (meta_index::wither_boss::TARGET_B, MetaSerializer::Int),
        (meta_index::wither_boss::TARGET_C, MetaSerializer::Int),
        (
            meta_index::wither_boss::INVULNERABLE_TICKS,
            MetaSerializer::Int,
        ),
    ];

    pub(super) const WARDEN: &[Row] =
        &[(meta_index::warden::CLIENT_ANGER_LEVEL, MetaSerializer::Int)];

    /// `Breeze` defines no metadata of its own in 1.21.1; this empty row set
    /// pins that bytecode fact instead of leaving the class implicit.
    pub(super) const BREEZE: &[Row] = &[];

    pub(super) const ARMADILLO: &[Row] =
        &[(meta_index::armadillo::STATE, MetaSerializer::ArmadilloState)];

    pub(super) const END_CRYSTAL: &[Row] = &[
        (
            meta_index::end_crystal::BEAM_TARGET,
            MetaSerializer::OptionalBlockPos,
        ),
        (
            meta_index::end_crystal::SHOW_BOTTOM,
            MetaSerializer::Boolean,
        ),
    ];

    pub(super) const ENDER_DRAGON: &[Row] =
        &[(meta_index::ender_dragon::PHASE, MetaSerializer::Int)];

    pub(super) const LLAMA: &[Row] = &[
        (meta_index::llama::STRENGTH, MetaSerializer::Int),
        (meta_index::llama::VARIANT, MetaSerializer::Int),
    ];

    pub(super) const SNIFFER: &[Row] = &[
        (meta_index::sniffer::STATE, MetaSerializer::SnifferState),
        (meta_index::sniffer::DROP_SEED_AT_TICK, MetaSerializer::Int),
    ];

    pub(super) const SNOW_GOLEM: &[Row] =
        &[(meta_index::snow_golem::PUMPKIN, MetaSerializer::Byte)];

    pub(super) const ALLAY: &[Row] = &[
        (meta_index::allay::DANCING, MetaSerializer::Boolean),
        (meta_index::allay::CAN_DUPLICATE, MetaSerializer::Boolean),
    ];

    pub(super) const BLAZE: &[Row] = &[(meta_index::blaze::FLAGS, MetaSerializer::Byte)];

    pub(super) const IRON_GOLEM: &[Row] = &[(meta_index::iron_golem::FLAGS, MetaSerializer::Byte)];

    pub(super) const FIREWORK_ROCKET: &[Row] = &[
        (meta_index::firework_rocket::ITEM, MetaSerializer::ItemStack),
        (
            meta_index::firework_rocket::ATTACHED_TO,
            MetaSerializer::OptionalUnsignedInt,
        ),
        (
            meta_index::firework_rocket::SHOT_AT_ANGLE,
            MetaSerializer::Boolean,
        ),
    ];

    pub(super) const DISPLAY: &[Row] = &[
        (
            meta_index::display::INTERPOLATION_START_DELTA_TICKS,
            MetaSerializer::Int,
        ),
        (
            meta_index::display::INTERPOLATION_DURATION,
            MetaSerializer::Int,
        ),
        (meta_index::display::TELEPORT_DURATION, MetaSerializer::Int),
        (meta_index::display::TRANSLATION, MetaSerializer::Vector3),
        (meta_index::display::SCALE, MetaSerializer::Vector3),
        (
            meta_index::display::LEFT_ROTATION,
            MetaSerializer::Quaternion,
        ),
        (
            meta_index::display::RIGHT_ROTATION,
            MetaSerializer::Quaternion,
        ),
        (
            meta_index::display::BILLBOARD_RENDER_CONSTRAINTS,
            MetaSerializer::Byte,
        ),
        (
            meta_index::display::BRIGHTNESS_OVERRIDE,
            MetaSerializer::Int,
        ),
        (meta_index::display::VIEW_RANGE, MetaSerializer::Float),
        (meta_index::display::SHADOW_RADIUS, MetaSerializer::Float),
        (meta_index::display::SHADOW_STRENGTH, MetaSerializer::Float),
        (meta_index::display::WIDTH, MetaSerializer::Float),
        (meta_index::display::HEIGHT, MetaSerializer::Float),
        (
            meta_index::display::GLOW_COLOR_OVERRIDE,
            MetaSerializer::Int,
        ),
    ];

    pub(super) const TEXT_DISPLAY: &[Row] = &[
        (meta_index::text_display::BACKGROUND, MetaSerializer::Int),
        (meta_index::text_display::TEXT, MetaSerializer::Component),
        (meta_index::text_display::STYLE_FLAGS, MetaSerializer::Byte),
        (meta_index::text_display::LINE_WIDTH, MetaSerializer::Int),
        (
            meta_index::text_display::SEE_THROUGH,
            MetaSerializer::Boolean,
        ),
    ];

    pub(super) const BLOCK_DISPLAY: &[Row] = &[(
        meta_index::block_display::BLOCK_STATE,
        MetaSerializer::BlockState,
    )];
}

/// The inheritance chain each kind carries, ordered parent-first.
const CHAIN_ENTITY: &[&[schema::Row]] = &[schema::ENTITY];
const CHAIN_LIVING: &[&[schema::Row]] = &[schema::ENTITY, schema::LIVING_ENTITY];
const CHAIN_MOB: &[&[schema::Row]] = &[schema::ENTITY, schema::LIVING_ENTITY, schema::MOB];
const CHAIN_PLAYER: &[&[schema::Row]] = &[schema::ENTITY, schema::LIVING_ENTITY, schema::PLAYER];
const CHAIN_AGEABLE: &[&[schema::Row]] = &[
    schema::ENTITY,
    schema::LIVING_ENTITY,
    schema::MOB,
    schema::AGEABLE_MOB,
];
const CHAIN_VILLAGER: &[&[schema::Row]] = &[
    schema::ENTITY,
    schema::LIVING_ENTITY,
    schema::MOB,
    schema::AGEABLE_MOB,
    schema::VILLAGER,
];
const CHAIN_SHEEP: &[&[schema::Row]] = &[
    schema::ENTITY,
    schema::LIVING_ENTITY,
    schema::MOB,
    schema::AGEABLE_MOB,
    schema::SHEEP,
];
const CHAIN_TAMABLE: &[&[schema::Row]] = &[
    schema::ENTITY,
    schema::LIVING_ENTITY,
    schema::MOB,
    schema::AGEABLE_MOB,
    schema::TAMABLE_ANIMAL,
];
const CHAIN_WOLF: &[&[schema::Row]] = &[
    schema::ENTITY,
    schema::LIVING_ENTITY,
    schema::MOB,
    schema::AGEABLE_MOB,
    schema::TAMABLE_ANIMAL,
    schema::WOLF,
];
const CHAIN_CAT: &[&[schema::Row]] = &[
    schema::ENTITY,
    schema::LIVING_ENTITY,
    schema::MOB,
    schema::AGEABLE_MOB,
    schema::TAMABLE_ANIMAL,
    schema::CAT,
];
const CHAIN_ABSTRACT_HORSE: &[&[schema::Row]] = &[
    schema::ENTITY,
    schema::LIVING_ENTITY,
    schema::MOB,
    schema::AGEABLE_MOB,
    schema::ABSTRACT_HORSE,
];
const CHAIN_ABSTRACT_CHESTED_HORSE: &[&[schema::Row]] = &[
    schema::ENTITY,
    schema::LIVING_ENTITY,
    schema::MOB,
    schema::AGEABLE_MOB,
    schema::ABSTRACT_HORSE,
    schema::ABSTRACT_CHESTED_HORSE,
];
const CHAIN_HORSE: &[&[schema::Row]] = &[
    schema::ENTITY,
    schema::LIVING_ENTITY,
    schema::MOB,
    schema::AGEABLE_MOB,
    schema::ABSTRACT_HORSE,
    schema::HORSE,
];
const CHAIN_CREEPER: &[&[schema::Row]] = &[
    schema::ENTITY,
    schema::LIVING_ENTITY,
    schema::MOB,
    schema::CREEPER,
];
const CHAIN_ENDER_MAN: &[&[schema::Row]] = &[
    schema::ENTITY,
    schema::LIVING_ENTITY,
    schema::MOB,
    schema::ENDER_MAN,
];
const CHAIN_SLIME: &[&[schema::Row]] = &[
    schema::ENTITY,
    schema::LIVING_ENTITY,
    schema::MOB,
    schema::SLIME,
];
const CHAIN_ZOMBIE: &[&[schema::Row]] = &[
    schema::ENTITY,
    schema::LIVING_ENTITY,
    schema::MOB,
    schema::ZOMBIE,
];
const CHAIN_ARMOR_STAND: &[&[schema::Row]] =
    &[schema::ENTITY, schema::LIVING_ENTITY, schema::ARMOR_STAND];
const CHAIN_ITEM_ENTITY: &[&[schema::Row]] = &[schema::ENTITY, schema::ITEM_ENTITY];
const CHAIN_ITEM_FRAME: &[&[schema::Row]] = &[schema::ENTITY, schema::ITEM_FRAME];
const CHAIN_ARROW: &[&[schema::Row]] = &[schema::ENTITY, schema::ARROW];
const CHAIN_VEHICLE_ENTITY: &[&[schema::Row]] = &[schema::ENTITY, schema::VEHICLE_ENTITY];
const CHAIN_BOAT: &[&[schema::Row]] = &[schema::ENTITY, schema::VEHICLE_ENTITY, schema::BOAT];
const CHAIN_ABSTRACT_MINECART: &[&[schema::Row]] = &[
    schema::ENTITY,
    schema::VEHICLE_ENTITY,
    schema::ABSTRACT_MINECART,
];
const CHAIN_WITHER_BOSS: &[&[schema::Row]] = &[
    schema::ENTITY,
    schema::LIVING_ENTITY,
    schema::MOB,
    schema::WITHER_BOSS,
];
const CHAIN_WARDEN: &[&[schema::Row]] = &[
    schema::ENTITY,
    schema::LIVING_ENTITY,
    schema::MOB,
    schema::WARDEN,
];
const CHAIN_BREEZE: &[&[schema::Row]] = &[
    schema::ENTITY,
    schema::LIVING_ENTITY,
    schema::MOB,
    schema::BREEZE,
];
const CHAIN_ARMADILLO: &[&[schema::Row]] = &[
    schema::ENTITY,
    schema::LIVING_ENTITY,
    schema::MOB,
    schema::AGEABLE_MOB,
    schema::ARMADILLO,
];
const CHAIN_END_CRYSTAL: &[&[schema::Row]] = &[schema::ENTITY, schema::END_CRYSTAL];
const CHAIN_ENDER_DRAGON: &[&[schema::Row]] = &[
    schema::ENTITY,
    schema::LIVING_ENTITY,
    schema::MOB,
    schema::ENDER_DRAGON,
];
const CHAIN_LLAMA: &[&[schema::Row]] = &[
    schema::ENTITY,
    schema::LIVING_ENTITY,
    schema::MOB,
    schema::AGEABLE_MOB,
    schema::ABSTRACT_HORSE,
    schema::ABSTRACT_CHESTED_HORSE,
    schema::LLAMA,
];
const CHAIN_SNIFFER: &[&[schema::Row]] = &[
    schema::ENTITY,
    schema::LIVING_ENTITY,
    schema::MOB,
    schema::AGEABLE_MOB,
    schema::SNIFFER,
];
const CHAIN_SNOW_GOLEM: &[&[schema::Row]] = &[
    schema::ENTITY,
    schema::LIVING_ENTITY,
    schema::MOB,
    schema::SNOW_GOLEM,
];
const CHAIN_ALLAY: &[&[schema::Row]] = &[
    schema::ENTITY,
    schema::LIVING_ENTITY,
    schema::MOB,
    schema::ALLAY,
];
const CHAIN_BLAZE: &[&[schema::Row]] = &[
    schema::ENTITY,
    schema::LIVING_ENTITY,
    schema::MOB,
    schema::BLAZE,
];
const CHAIN_IRON_GOLEM: &[&[schema::Row]] = &[
    schema::ENTITY,
    schema::LIVING_ENTITY,
    schema::MOB,
    schema::IRON_GOLEM,
];
const CHAIN_FIREWORK_ROCKET: &[&[schema::Row]] = &[schema::ENTITY, schema::FIREWORK_ROCKET];
const CHAIN_DISPLAY: &[&[schema::Row]] = &[schema::ENTITY, schema::DISPLAY];
const CHAIN_TEXT_DISPLAY: &[&[schema::Row]] =
    &[schema::ENTITY, schema::DISPLAY, schema::TEXT_DISPLAY];
const CHAIN_BLOCK_DISPLAY: &[&[schema::Row]] =
    &[schema::ENTITY, schema::DISPLAY, schema::BLOCK_DISPLAY];

impl MetaEntityKind {
    fn chains(self) -> &'static [&'static [schema::Row]] {
        match self {
            Self::Entity => CHAIN_ENTITY,
            Self::LivingEntity => CHAIN_LIVING,
            Self::Mob => CHAIN_MOB,
            Self::Player => CHAIN_PLAYER,
            Self::AgeableMob => CHAIN_AGEABLE,
            Self::Villager => CHAIN_VILLAGER,
            Self::Sheep => CHAIN_SHEEP,
            Self::TamableAnimal => CHAIN_TAMABLE,
            Self::Wolf => CHAIN_WOLF,
            Self::Cat => CHAIN_CAT,
            Self::AbstractHorse => CHAIN_ABSTRACT_HORSE,
            Self::AbstractChestedHorse => CHAIN_ABSTRACT_CHESTED_HORSE,
            Self::Horse => CHAIN_HORSE,
            Self::Creeper => CHAIN_CREEPER,
            Self::EnderMan => CHAIN_ENDER_MAN,
            Self::Slime => CHAIN_SLIME,
            Self::Zombie => CHAIN_ZOMBIE,
            Self::ArmorStand => CHAIN_ARMOR_STAND,
            Self::ItemEntity => CHAIN_ITEM_ENTITY,
            Self::ItemFrame => CHAIN_ITEM_FRAME,
            Self::Arrow => CHAIN_ARROW,
            Self::VehicleEntity => CHAIN_VEHICLE_ENTITY,
            Self::Boat => CHAIN_BOAT,
            Self::AbstractMinecart => CHAIN_ABSTRACT_MINECART,
            Self::WitherBoss => CHAIN_WITHER_BOSS,
            Self::Warden => CHAIN_WARDEN,
            Self::Breeze => CHAIN_BREEZE,
            Self::Armadillo => CHAIN_ARMADILLO,
            Self::EndCrystal => CHAIN_END_CRYSTAL,
            Self::EnderDragon => CHAIN_ENDER_DRAGON,
            Self::Llama => CHAIN_LLAMA,
            Self::Sniffer => CHAIN_SNIFFER,
            Self::SnowGolem => CHAIN_SNOW_GOLEM,
            Self::Allay => CHAIN_ALLAY,
            Self::Blaze => CHAIN_BLAZE,
            Self::IronGolem => CHAIN_IRON_GOLEM,
            Self::FireworkRocket => CHAIN_FIREWORK_ROCKET,
            Self::Display => CHAIN_DISPLAY,
            Self::TextDisplay => CHAIN_TEXT_DISPLAY,
            Self::BlockDisplay => CHAIN_BLOCK_DISPLAY,
        }
    }
}

/// The serializer `kind` requires at `index`, including everything it inherits,
/// or `None` when no class in its tree defines that index.
pub fn expected_serializer(kind: MetaEntityKind, index: u8) -> Option<MetaSerializer> {
    kind.chains()
        .iter()
        .flat_map(|rows| rows.iter())
        .find(|row| row.0 == index)
        .map(|row| row.1)
}

/// Reject a `(kind, index, serializer)` triple the 1.21.1 class tree does not
/// define.
///
/// This is the guard the historical defects needed: an index that belongs to a
/// different class, and an index that carries a different serializer, are both
/// errors, and both are silent on the wire. Call it on every entry a call site
/// hand-builds — [`MetaEntry::validate`] is the shorthand for one entry.
pub fn validate_entry(
    kind: MetaEntityKind,
    index: u8,
    serializer: MetaSerializer,
) -> Result<(), &'static str> {
    match expected_serializer(kind, index) {
        None => Err("metadata index is not defined for this entity kind in 1.21.1"),
        Some(expected) if expected == serializer => Ok(()),
        Some(_) => {
            Err("metadata index needs a different serializer for this entity kind in 1.21.1")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire::{read_frame, WireReader};
    use meta_index::*;

    /// A one-entry body from entity id `7`, so every byte check reads
    /// `[0x07, index, serializer, payload..., 0xff]`.
    fn body(index: u8, value: MetaValue) -> Vec<u8> {
        encode_set_entity_data_payload(7, &[MetaEntry::new(index, value)])
    }

    /// The payload bytes of a one-entry body, with the entity id, index,
    /// serializer and terminator stripped.
    fn payload_bytes(index: u8, value: MetaValue) -> Vec<u8> {
        let serializer = value.serializer().id();
        let body = body(index, value);
        assert_eq!(body[..3], [0x07, index, serializer]);
        assert_eq!(body.last(), Some(&METADATA_TERMINATOR));
        body[3..body.len() - 1].to_vec()
    }

    /// Decode one value the way a client does. Serializers whose payload is not
    /// self-describing (`ItemStack`, `Particle`, `Particles`) are covered by the
    /// byte-exact tests instead, because decoding them needs the slot and
    /// particle codecs this crate keeps elsewhere.
    fn decode_value(serializer: MetaSerializer, bytes: &[u8]) -> MetaValue {
        let mut r = WireReader::new(bytes);
        let value = match serializer {
            MetaSerializer::Byte => MetaValue::Byte(r.read_u8().expect("byte")),
            MetaSerializer::Int => MetaValue::Int(r.read_varint().expect("int")),
            MetaSerializer::Long => MetaValue::Long(r.read_varlong().expect("long")),
            MetaSerializer::Float => MetaValue::Float(r.read_f32().expect("float")),
            MetaSerializer::String => MetaValue::String(r.read_string(32_767).expect("string")),
            MetaSerializer::Component => {
                MetaValue::Component(r.read_anonymous_nbt().expect("nbt").to_vec())
            }
            MetaSerializer::OptionalComponent => {
                if r.read_bool().expect("flag") {
                    MetaValue::OptionalComponent(Some(
                        r.read_anonymous_nbt().expect("nbt").to_vec(),
                    ))
                } else {
                    MetaValue::OptionalComponent(None)
                }
            }
            MetaSerializer::Boolean => MetaValue::Boolean(r.read_bool().expect("bool")),
            MetaSerializer::Rotations => MetaValue::Rotations([
                r.read_f32().expect("x"),
                r.read_f32().expect("y"),
                r.read_f32().expect("z"),
            ]),
            MetaSerializer::BlockPos => MetaValue::BlockPos(r.read_i64().expect("pos")),
            MetaSerializer::OptionalBlockPos => {
                if r.read_bool().expect("flag") {
                    MetaValue::OptionalBlockPos(Some(r.read_i64().expect("pos")))
                } else {
                    MetaValue::OptionalBlockPos(None)
                }
            }
            MetaSerializer::Direction => MetaValue::Direction(r.read_varint().expect("dir")),
            MetaSerializer::OptionalUuid => {
                if r.read_bool().expect("flag") {
                    MetaValue::OptionalUuid(Some(r.read_uuid().expect("uuid")))
                } else {
                    MetaValue::OptionalUuid(None)
                }
            }
            MetaSerializer::BlockState => MetaValue::BlockState(r.read_varint().expect("state")),
            MetaSerializer::OptionalBlockState => {
                MetaValue::OptionalBlockState(r.read_varint().expect("state"))
            }
            MetaSerializer::CompoundTag => {
                MetaValue::CompoundTag(r.read_anonymous_nbt().expect("nbt").to_vec())
            }
            MetaSerializer::VillagerData => MetaValue::VillagerData {
                kind: r.read_varint().expect("kind"),
                profession: r.read_varint().expect("profession"),
                level: r.read_varint().expect("level"),
            },
            MetaSerializer::OptionalUnsignedInt => {
                let encoded = r.read_varint().expect("optional id");
                MetaValue::OptionalUnsignedInt(u32::try_from(encoded - 1).ok())
            }
            MetaSerializer::Pose => MetaValue::Pose(r.read_varint().expect("pose")),
            MetaSerializer::CatVariant => MetaValue::CatVariant(r.read_varint().expect("id")),
            MetaSerializer::WolfVariant => {
                let encoded = r.read_varint().expect("holder id");
                assert!(encoded > 0, "zero is an inline holder payload");
                MetaValue::WolfVariant(u32::try_from(encoded - 1).expect("holder id"))
            }
            MetaSerializer::FrogVariant => MetaValue::FrogVariant(r.read_varint().expect("id")),
            MetaSerializer::OptionalGlobalPos => {
                if r.read_bool().expect("flag") {
                    let dimension = r.read_string(32_767).expect("dimension");
                    MetaValue::OptionalGlobalPos(Some((dimension, r.read_i64().expect("pos"))))
                } else {
                    MetaValue::OptionalGlobalPos(None)
                }
            }
            MetaSerializer::PaintingVariant => {
                let encoded = r.read_varint().expect("holder id");
                assert!(encoded > 0, "zero is an inline holder payload");
                MetaValue::PaintingVariant(u32::try_from(encoded - 1).expect("holder id"))
            }
            MetaSerializer::SnifferState => {
                MetaValue::SnifferState(r.read_varint().expect("state"))
            }
            MetaSerializer::ArmadilloState => {
                MetaValue::ArmadilloState(r.read_varint().expect("state"))
            }
            MetaSerializer::Vector3 => MetaValue::Vector3([
                r.read_f32().expect("x"),
                r.read_f32().expect("y"),
                r.read_f32().expect("z"),
            ]),
            MetaSerializer::Quaternion => MetaValue::Quaternion([
                r.read_f32().expect("x"),
                r.read_f32().expect("y"),
                r.read_f32().expect("z"),
                r.read_f32().expect("w"),
            ]),
            MetaSerializer::ItemStack | MetaSerializer::Particle | MetaSerializer::Particles => {
                unreachable!("opaque serializer values are covered by byte-exact tests")
            }
        };
        r.finish()
            .unwrap_or_else(|error| panic!("{serializer:?} left bytes behind: {error}"));
        value
    }

    /// One value per serializer. The opaque ones carry canonical minimal forms:
    /// a zero-count `Slot`, and a particle type with an empty payload.
    fn fixtures() -> Vec<MetaValue> {
        vec![
            MetaValue::Byte(0x2a),
            MetaValue::Int(300),
            MetaValue::Long(300),
            MetaValue::Float(1.5),
            MetaValue::String("héllo".into()),
            MetaValue::Component(vec![0x0a, 0x00]),
            MetaValue::OptionalComponent(Some(vec![0x03, 0x00, 0x00, 0x00, 0x2a])),
            MetaValue::OptionalComponent(None),
            MetaValue::ItemStack(vec![0x00]),
            MetaValue::Boolean(true),
            MetaValue::Rotations([1.0, -1.0, 0.0]),
            MetaValue::BlockPos(crate::wire::BlockPos::new(0, 64, 0).pack()),
            MetaValue::OptionalBlockPos(Some(crate::wire::BlockPos::new(-1, 65, 2).pack())),
            MetaValue::OptionalBlockPos(None),
            MetaValue::Direction(3),
            MetaValue::OptionalUuid(Some([0x11; 16])),
            MetaValue::OptionalUuid(None),
            MetaValue::BlockState(9),
            MetaValue::OptionalBlockState(0),
            MetaValue::OptionalBlockState(9),
            MetaValue::CompoundTag(vec![0x03, 0x00, 0x00, 0x00, 0x2a]),
            MetaValue::Particle(vec![0x00]),
            MetaValue::Particles(vec![vec![0x00], vec![0x01, 0x00]]),
            MetaValue::VillagerData {
                kind: 0,
                profession: 2,
                level: 1,
            },
            MetaValue::OptionalUnsignedInt(Some(41)),
            MetaValue::OptionalUnsignedInt(None),
            MetaValue::Pose(1),
            MetaValue::CatVariant(4),
            MetaValue::WolfVariant(2),
            MetaValue::FrogVariant(1),
            MetaValue::OptionalGlobalPos(Some(("minecraft:overworld".into(), 0x40))),
            MetaValue::OptionalGlobalPos(None),
            MetaValue::PaintingVariant(7),
            MetaValue::SnifferState(2),
            MetaValue::ArmadilloState(1),
            MetaValue::Vector3([0.0, 0.5, 1.0]),
            MetaValue::Quaternion([0.0, 0.0, 0.0, 1.0]),
        ]
    }

    #[test]
    fn serializer_ids_are_the_vanilla_registration_order() {
        let all: [(MetaSerializer, u8); 31] = [
            (MetaSerializer::Byte, 0),
            (MetaSerializer::Int, 1),
            (MetaSerializer::Long, 2),
            (MetaSerializer::Float, 3),
            (MetaSerializer::String, 4),
            (MetaSerializer::Component, 5),
            (MetaSerializer::OptionalComponent, 6),
            (MetaSerializer::ItemStack, 7),
            (MetaSerializer::Boolean, 8),
            (MetaSerializer::Rotations, 9),
            (MetaSerializer::BlockPos, 10),
            (MetaSerializer::OptionalBlockPos, 11),
            (MetaSerializer::Direction, 12),
            (MetaSerializer::OptionalUuid, 13),
            (MetaSerializer::BlockState, 14),
            (MetaSerializer::OptionalBlockState, 15),
            (MetaSerializer::CompoundTag, 16),
            (MetaSerializer::Particle, 17),
            (MetaSerializer::Particles, 18),
            (MetaSerializer::VillagerData, 19),
            (MetaSerializer::OptionalUnsignedInt, 20),
            (MetaSerializer::Pose, 21),
            (MetaSerializer::CatVariant, 22),
            (MetaSerializer::WolfVariant, 23),
            (MetaSerializer::FrogVariant, 24),
            (MetaSerializer::OptionalGlobalPos, 25),
            (MetaSerializer::PaintingVariant, 26),
            (MetaSerializer::SnifferState, 27),
            (MetaSerializer::ArmadilloState, 28),
            (MetaSerializer::Vector3, 29),
            (MetaSerializer::Quaternion, 30),
        ];
        for (serializer, id) in all {
            assert_eq!(serializer.id(), id, "wire id of {serializer:?}");
            assert_eq!(MetaSerializer::from_id(id), Some(serializer));
        }
        // The fixtures cover the table exactly once per serializer, so a value
        // with no fixture (or a fixture with no id) fails here.
        let mut covered: Vec<u8> = fixtures()
            .iter()
            .map(|value| value.serializer().id())
            .collect();
        covered.sort_unstable();
        covered.dedup();
        assert_eq!(covered, (0..=30).collect::<Vec<u8>>());
        assert_eq!(MetaSerializer::from_id(31), None);
        assert_eq!(MetaSerializer::from_id(METADATA_TERMINATOR), None);
    }

    #[test]
    fn every_self_describing_value_round_trips() {
        for value in fixtures() {
            if matches!(
                value.serializer(),
                MetaSerializer::ItemStack | MetaSerializer::Particle | MetaSerializer::Particles
            ) {
                continue;
            }
            let bytes = payload_bytes(14, value.clone());
            assert_eq!(
                decode_value(value.serializer(), &bytes),
                value,
                "round trip of {value:?}"
            );
        }
    }

    #[test]
    fn byte_value_is_one_raw_byte() {
        assert_eq!(payload_bytes(0, MetaValue::Byte(0x2a)), vec![0x2a]);
    }

    #[test]
    fn int_value_is_a_varint() {
        assert_eq!(payload_bytes(0, MetaValue::Int(300)), vec![0xac, 0x02]);
        assert_eq!(
            payload_bytes(0, MetaValue::Int(-1)),
            vec![0xff, 0xff, 0xff, 0xff, 0x0f]
        );
    }

    #[test]
    fn long_value_is_a_varlong() {
        assert_eq!(payload_bytes(0, MetaValue::Long(300)), vec![0xac, 0x02]);
        assert_eq!(
            payload_bytes(0, MetaValue::Long(-1)),
            vec![0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x01]
        );
    }

    #[test]
    fn float_value_is_big_endian() {
        assert_eq!(
            payload_bytes(0, MetaValue::Float(1.5)),
            vec![0x3f, 0xc0, 0x00, 0x00]
        );
    }

    #[test]
    fn string_value_is_length_prefixed_utf8() {
        assert_eq!(
            payload_bytes(0, MetaValue::String("hi".into())),
            vec![0x02, b'h', b'i']
        );
        assert_eq!(
            payload_bytes(0, MetaValue::String("é".into())),
            vec![0x02, 0xc3, 0xa9]
        );
    }

    #[test]
    fn component_serializers_write_nbt_verbatim() {
        // Network NBT is unnamed: the tag byte is followed straight by the
        // payload, with no root name.
        let nbt = vec![0x03, 0x00, 0x00, 0x00, 0x2a];
        assert_eq!(
            body(1, MetaValue::Component(nbt.clone())),
            [vec![0x07, 0x01, 5], nbt.clone(), vec![METADATA_TERMINATOR]].concat()
        );
        assert_eq!(
            body(1, MetaValue::CompoundTag(nbt.clone())),
            [vec![0x07, 0x01, 16], nbt.clone(), vec![METADATA_TERMINATOR]].concat()
        );
        assert_eq!(
            body(1, MetaValue::OptionalComponent(Some(nbt.clone()))),
            [vec![0x07, 0x01, 6, 1], nbt, vec![METADATA_TERMINATOR]].concat()
        );
        assert_eq!(
            body(1, MetaValue::OptionalComponent(None)),
            vec![0x07, 0x01, 6, 0, METADATA_TERMINATOR]
        );
    }

    #[test]
    fn item_stack_serializer_writes_the_pre_encoded_slot() {
        assert_eq!(
            body(0, MetaValue::ItemStack(vec![0x00])),
            vec![0x07, 0x00, 7, 0x00, METADATA_TERMINATOR]
        );
    }

    #[test]
    fn boolean_value_is_one_byte() {
        assert_eq!(payload_bytes(0, MetaValue::Boolean(true)), vec![0x01]);
        assert_eq!(payload_bytes(0, MetaValue::Boolean(false)), vec![0x00]);
    }

    #[test]
    fn rotations_value_is_three_floats() {
        assert_eq!(
            payload_bytes(0, MetaValue::Rotations([1.0, -1.0, 0.0])),
            vec![0x3f, 0x80, 0x00, 0x00, 0xbf, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]
        );
    }

    #[test]
    fn block_pos_value_is_eight_packed_bytes() {
        assert_eq!(
            payload_bytes(0, MetaValue::packed_block_pos(0, 64, 0)),
            vec![0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x40]
        );
        // A position far outside one byte in every field still packs to the
        // same eight big-endian bytes the client unpacks.
        let packed = crate::wire::BlockPos::new(-33_000_000, -64, 33_000_000).pack();
        assert_eq!(
            payload_bytes(0, MetaValue::packed_block_pos(-33_000_000, -64, 33_000_000)),
            packed.to_be_bytes()
        );
    }

    #[test]
    fn optional_block_pos_value_is_a_flag_then_the_position() {
        let packed = crate::wire::BlockPos::new(1, 2, 3).pack();
        let mut expected = vec![0x01];
        expected.extend_from_slice(&packed.to_be_bytes());
        assert_eq!(
            payload_bytes(0, MetaValue::OptionalBlockPos(Some(packed))),
            expected
        );
        assert_eq!(
            payload_bytes(0, MetaValue::OptionalBlockPos(None)),
            vec![0x00]
        );
    }

    #[test]
    fn direction_value_is_a_varint() {
        assert_eq!(payload_bytes(0, MetaValue::Direction(3)), vec![0x03]);
    }

    #[test]
    fn optional_uuid_value_is_a_flag_then_sixteen_bytes() {
        let uuid = [0x11; 16];
        assert_eq!(
            payload_bytes(0, MetaValue::OptionalUuid(Some(uuid))),
            [vec![0x01], uuid.to_vec()].concat()
        );
        assert_eq!(payload_bytes(0, MetaValue::OptionalUuid(None)), vec![0x00]);
    }

    #[test]
    fn block_state_values_are_varints_with_zero_meaning_absent() {
        assert_eq!(payload_bytes(0, MetaValue::BlockState(9)), vec![0x09]);
        assert_eq!(
            payload_bytes(0, MetaValue::OptionalBlockState(9)),
            vec![0x09]
        );
        assert_eq!(
            payload_bytes(0, MetaValue::OptionalBlockState(0)),
            vec![0x00]
        );
    }

    #[test]
    fn particle_serializers_write_pre_encoded_payloads() {
        assert_eq!(
            body(0, MetaValue::Particle(vec![0x00])),
            vec![0x07, 0x00, 17, 0x00, METADATA_TERMINATOR]
        );
        assert_eq!(
            body(0, MetaValue::Particles(vec![vec![0x00], vec![0x01, 0x00]])),
            vec![0x07, 0x00, 18, 0x02, 0x00, 0x01, 0x00, METADATA_TERMINATOR]
        );
    }

    #[test]
    fn villager_data_value_is_three_varints() {
        assert_eq!(
            payload_bytes(
                0,
                MetaValue::VillagerData {
                    kind: 0,
                    profession: 2,
                    level: 1,
                }
            ),
            vec![0x00, 0x02, 0x01]
        );
    }

    #[test]
    fn optional_unsigned_int_value_is_offset_by_one() {
        assert_eq!(
            payload_bytes(0, MetaValue::OptionalUnsignedInt(None)),
            vec![0x00]
        );
        assert_eq!(
            payload_bytes(0, MetaValue::OptionalUnsignedInt(Some(0))),
            vec![0x01]
        );
        assert_eq!(
            payload_bytes(0, MetaValue::OptionalUnsignedInt(Some(41))),
            vec![0x2a]
        );
    }

    #[test]
    fn plain_registry_id_values_are_varints() {
        for value in [
            MetaValue::Pose(1),
            MetaValue::CatVariant(1),
            MetaValue::FrogVariant(1),
            MetaValue::SnifferState(1),
            MetaValue::ArmadilloState(1),
        ] {
            assert_eq!(payload_bytes(0, value.clone()), vec![0x01], "{value:?}");
        }
    }

    #[test]
    fn holder_variant_references_are_offset_by_one() {
        // `ByteBufCodecs.holder` reserves zero for an inline direct payload, so
        // even registry id zero is written as one. Cat and frog use
        // `holderRegistry` instead and stay plain registry ids (covered above).
        for value in [MetaValue::WolfVariant(1), MetaValue::PaintingVariant(1)] {
            assert_eq!(payload_bytes(0, value.clone()), vec![0x02], "{value:?}");
        }
        assert_eq!(payload_bytes(0, MetaValue::WolfVariant(0)), vec![0x01]);
        assert_eq!(payload_bytes(0, MetaValue::PaintingVariant(0)), vec![0x01]);
    }

    #[test]
    fn optional_global_pos_value_is_flag_dimension_and_position() {
        let packed = crate::wire::BlockPos::new(4, 5, 6).pack();
        let mut expected = vec![0x01, 0x13];
        expected.extend_from_slice(b"minecraft:overworld");
        expected.extend_from_slice(&packed.to_be_bytes());
        assert_eq!(
            payload_bytes(
                0,
                MetaValue::OptionalGlobalPos(Some(("minecraft:overworld".into(), packed)))
            ),
            expected
        );
        assert_eq!(
            payload_bytes(0, MetaValue::OptionalGlobalPos(None)),
            vec![0x00]
        );
    }

    #[test]
    fn vector_and_quaternion_values_are_floats() {
        assert_eq!(
            payload_bytes(0, MetaValue::Vector3([0.0, 0.5, 1.0])),
            vec![0x00, 0x00, 0x00, 0x00, 0x3f, 0x00, 0x00, 0x00, 0x3f, 0x80, 0x00, 0x00]
        );
        assert_eq!(
            payload_bytes(0, MetaValue::Quaternion([0.0, 0.0, 0.0, 1.0])),
            vec![
                0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x3f, 0x80,
                0x00, 0x00
            ]
        );
    }

    #[test]
    fn body_is_entity_id_then_entries_then_one_terminator() {
        let entries = [
            MetaEntry::new(0, MetaValue::Byte(0x01)),
            MetaEntry::new(9, MetaValue::Float(20.0)),
        ];
        assert_eq!(
            encode_set_entity_data_payload(300, &entries),
            vec![
                0xac,
                0x02, // VarInt(300)
                0x00, // index 0
                0x00, // Byte
                0x01,
                0x09, // index 9
                0x03, // Float
                0x41,
                0xa0,
                0x00,
                0x00,
                METADATA_TERMINATOR,
            ]
        );
        // No entry count on the wire: the terminator is the only end marker, so
        // an empty slice is a complete body.
        assert_eq!(
            encode_set_entity_data_payload(1, &[]),
            vec![0x01, METADATA_TERMINATOR]
        );
    }

    #[test]
    #[should_panic(expected = "end-of-list terminator")]
    fn index_0xff_is_rejected() {
        let _ = MetaEntry::new(METADATA_TERMINATOR, MetaValue::Byte(0));
    }

    #[test]
    fn frame_uses_the_canonical_table_id() {
        let table_id = V767_PACKETS
            .iter()
            .find(|entry| {
                entry.0 == PacketPhase::Play
                    && entry.1 == PacketDir::Clientbound
                    && entry.2 == SET_ENTITY_DATA_PACKET
            })
            .map(|entry| entry.3)
            .expect("the 767 table defines clientbound play set_entity_data");
        assert_eq!(
            i32::try_from(table_id).ok(),
            Some(set_entity_data_packet_id())
        );
        // 1.21.1 clientbound play `set_entity_data`.
        assert_eq!(set_entity_data_packet_id(), 0x58);
    }

    #[test]
    fn frame_round_trips_through_the_canonical_frame_reader() {
        let entries = [MetaEntry::new(0, MetaValue::Byte(1))];
        let frame = encode_set_entity_data(300, &entries);
        assert_eq!(
            frame,
            vec![
                0x07, // body length: the id VarInt plus six payload bytes
                0x58, // canonical set_entity_data id
                0xac,
                0x02,
                0x00,
                0x00,
                0x01,
                METADATA_TERMINATOR,
            ]
        );
        let payload = encode_set_entity_data_payload(300, &entries);
        assert_eq!(read_frame(&frame), Ok((0x58, payload.as_slice())));
    }

    /// Pin one row of the 1.21.1 table: the constant, the number it must hold,
    /// and the serializer a call site has to use.
    fn pinned(kind: MetaEntityKind, constant: u8, index: u8, serializer: MetaSerializer) {
        assert_eq!(constant, index, "{kind:?} constant value");
        assert_eq!(
            expected_serializer(kind, index),
            Some(serializer),
            "{kind:?} index {index}"
        );
        assert_eq!(validate_entry(kind, index, serializer), Ok(()));
    }

    /// The authoritative table, written out independently of `schema::*` and
    /// `meta_index::*`: every `(kind, constant, index, serializer)` row the
    /// module ships. `schema` is the implementation of this table, so this test
    /// is what fails when the implementation drifts from it.
    #[test]
    fn index_and_serializer_table_is_the_1211_class_tree() {
        use MetaSerializer as S;

        pinned(MetaEntityKind::Entity, entity::SHARED_FLAGS, 0, S::Byte);
        pinned(MetaEntityKind::Entity, entity::AIR_SUPPLY, 1, S::Int);
        pinned(
            MetaEntityKind::Entity,
            entity::CUSTOM_NAME,
            2,
            S::OptionalComponent,
        );
        pinned(
            MetaEntityKind::Entity,
            entity::CUSTOM_NAME_VISIBLE,
            3,
            S::Boolean,
        );
        pinned(MetaEntityKind::Entity, entity::SILENT, 4, S::Boolean);
        pinned(MetaEntityKind::Entity, entity::NO_GRAVITY, 5, S::Boolean);
        pinned(MetaEntityKind::Entity, entity::POSE, 6, S::Pose);
        pinned(MetaEntityKind::Entity, entity::TICKS_FROZEN, 7, S::Int);

        pinned(
            MetaEntityKind::LivingEntity,
            living_entity::LIVING_FLAGS,
            8,
            S::Byte,
        );
        pinned(
            MetaEntityKind::LivingEntity,
            living_entity::HEALTH,
            9,
            S::Float,
        );
        pinned(
            MetaEntityKind::LivingEntity,
            living_entity::EFFECT_PARTICLES,
            10,
            S::Particles,
        );
        pinned(
            MetaEntityKind::LivingEntity,
            living_entity::EFFECT_AMBIENCE,
            11,
            S::Boolean,
        );
        pinned(
            MetaEntityKind::LivingEntity,
            living_entity::ARROW_COUNT,
            12,
            S::Int,
        );
        pinned(
            MetaEntityKind::LivingEntity,
            living_entity::STINGER_COUNT,
            13,
            S::Int,
        );
        pinned(
            MetaEntityKind::LivingEntity,
            living_entity::SLEEPING_POS,
            14,
            S::OptionalBlockPos,
        );

        pinned(MetaEntityKind::Mob, mob::MOB_FLAGS, 15, S::Byte);

        pinned(MetaEntityKind::Player, player::ABSORPTION, 15, S::Float);
        pinned(MetaEntityKind::Player, player::SCORE, 16, S::Int);
        pinned(
            MetaEntityKind::Player,
            player::MODEL_CUSTOMISATION,
            17,
            S::Byte,
        );
        pinned(MetaEntityKind::Player, player::MAIN_HAND, 18, S::Byte);
        pinned(
            MetaEntityKind::Player,
            player::SHOULDER_LEFT,
            19,
            S::CompoundTag,
        );
        pinned(
            MetaEntityKind::Player,
            player::SHOULDER_RIGHT,
            20,
            S::CompoundTag,
        );

        pinned(
            MetaEntityKind::AgeableMob,
            ageable_mob::BABY,
            16,
            S::Boolean,
        );

        pinned(
            MetaEntityKind::Villager,
            villager::UNHAPPY_COUNTER,
            17,
            S::Int,
        );
        pinned(
            MetaEntityKind::Villager,
            villager::VILLAGER_DATA,
            18,
            S::VillagerData,
        );

        pinned(MetaEntityKind::Sheep, sheep::WOOL_ID, 17, S::Byte);

        pinned(
            MetaEntityKind::TamableAnimal,
            tamable_animal::FLAGS,
            17,
            S::Byte,
        );
        pinned(
            MetaEntityKind::TamableAnimal,
            tamable_animal::OWNER_UUID,
            18,
            S::OptionalUuid,
        );

        pinned(MetaEntityKind::Wolf, wolf::INTERESTED, 19, S::Boolean);
        pinned(MetaEntityKind::Wolf, wolf::COLLAR_COLOR, 20, S::Int);
        pinned(MetaEntityKind::Wolf, wolf::ANGER_TIME, 21, S::Int);
        pinned(MetaEntityKind::Wolf, wolf::VARIANT, 22, S::WolfVariant);

        pinned(MetaEntityKind::Cat, cat::VARIANT, 19, S::CatVariant);
        pinned(MetaEntityKind::Cat, cat::IS_LYING, 20, S::Boolean);
        pinned(MetaEntityKind::Cat, cat::RELAX_STATE_ONE, 21, S::Boolean);
        pinned(MetaEntityKind::Cat, cat::COLLAR_COLOR, 22, S::Int);

        pinned(
            MetaEntityKind::AbstractHorse,
            abstract_horse::FLAGS,
            17,
            S::Byte,
        );
        pinned(
            MetaEntityKind::AbstractChestedHorse,
            abstract_chested_horse::CHEST,
            18,
            S::Boolean,
        );
        pinned(MetaEntityKind::Horse, horse::VARIANT, 18, S::Int);

        pinned(MetaEntityKind::Creeper, creeper::SWELL_DIR, 16, S::Int);
        pinned(MetaEntityKind::Creeper, creeper::IS_POWERED, 17, S::Boolean);
        pinned(MetaEntityKind::Creeper, creeper::IS_IGNITED, 18, S::Boolean);

        pinned(
            MetaEntityKind::EnderMan,
            ender_man::CARRY_STATE,
            16,
            S::OptionalBlockState,
        );
        pinned(MetaEntityKind::EnderMan, ender_man::CREEPY, 17, S::Boolean);
        pinned(
            MetaEntityKind::EnderMan,
            ender_man::STARED_AT,
            18,
            S::Boolean,
        );

        pinned(MetaEntityKind::Slime, slime::SIZE, 16, S::Int);

        pinned(MetaEntityKind::Zombie, zombie::BABY, 16, S::Boolean);
        pinned(MetaEntityKind::Zombie, zombie::SPECIAL_TYPE, 17, S::Int);
        pinned(
            MetaEntityKind::Zombie,
            zombie::DROWNED_CONVERSION,
            18,
            S::Boolean,
        );

        pinned(
            MetaEntityKind::ArmorStand,
            armor_stand::CLIENT_FLAGS,
            15,
            S::Byte,
        );
        pinned(
            MetaEntityKind::ArmorStand,
            armor_stand::HEAD_POSE,
            16,
            S::Rotations,
        );
        pinned(
            MetaEntityKind::ArmorStand,
            armor_stand::BODY_POSE,
            17,
            S::Rotations,
        );
        pinned(
            MetaEntityKind::ArmorStand,
            armor_stand::LEFT_ARM_POSE,
            18,
            S::Rotations,
        );
        pinned(
            MetaEntityKind::ArmorStand,
            armor_stand::RIGHT_ARM_POSE,
            19,
            S::Rotations,
        );
        pinned(
            MetaEntityKind::ArmorStand,
            armor_stand::LEFT_LEG_POSE,
            20,
            S::Rotations,
        );
        pinned(
            MetaEntityKind::ArmorStand,
            armor_stand::RIGHT_LEG_POSE,
            21,
            S::Rotations,
        );

        pinned(
            MetaEntityKind::ItemEntity,
            item_entity::ITEM,
            8,
            S::ItemStack,
        );

        pinned(MetaEntityKind::ItemFrame, item_frame::ITEM, 8, S::ItemStack);
        pinned(MetaEntityKind::ItemFrame, item_frame::ROTATION, 9, S::Int);

        pinned(MetaEntityKind::Arrow, arrow::FLAGS, 8, S::Byte);
        pinned(MetaEntityKind::Arrow, arrow::PIERCE_LEVEL, 9, S::Byte);
        pinned(MetaEntityKind::Arrow, arrow::EFFECT_COLOR, 10, S::Int);

        pinned(
            MetaEntityKind::VehicleEntity,
            vehicle_entity::HURT,
            8,
            S::Int,
        );
        pinned(
            MetaEntityKind::VehicleEntity,
            vehicle_entity::HURTDIR,
            9,
            S::Int,
        );
        pinned(
            MetaEntityKind::VehicleEntity,
            vehicle_entity::DAMAGE,
            10,
            S::Float,
        );

        pinned(MetaEntityKind::Boat, vehicle_entity::HURT, 8, S::Int);
        pinned(MetaEntityKind::Boat, vehicle_entity::HURTDIR, 9, S::Int);
        pinned(MetaEntityKind::Boat, vehicle_entity::DAMAGE, 10, S::Float);
        pinned(MetaEntityKind::Boat, boat::TYPE, 11, S::Int);
        pinned(MetaEntityKind::Boat, boat::PADDLE_LEFT, 12, S::Boolean);
        pinned(MetaEntityKind::Boat, boat::PADDLE_RIGHT, 13, S::Boolean);
        pinned(MetaEntityKind::Boat, boat::BUBBLE_TIME, 14, S::Int);

        pinned(
            MetaEntityKind::AbstractMinecart,
            vehicle_entity::HURT,
            8,
            S::Int,
        );
        pinned(
            MetaEntityKind::AbstractMinecart,
            vehicle_entity::HURTDIR,
            9,
            S::Int,
        );
        pinned(
            MetaEntityKind::AbstractMinecart,
            vehicle_entity::DAMAGE,
            10,
            S::Float,
        );
        pinned(
            MetaEntityKind::AbstractMinecart,
            abstract_minecart::DISPLAY_BLOCK,
            11,
            S::Int,
        );
        pinned(
            MetaEntityKind::AbstractMinecart,
            abstract_minecart::DISPLAY_OFFSET,
            12,
            S::Int,
        );
        pinned(
            MetaEntityKind::AbstractMinecart,
            abstract_minecart::CUSTOM_DISPLAY,
            13,
            S::Boolean,
        );

        pinned(
            MetaEntityKind::WitherBoss,
            wither_boss::TARGET_A,
            16,
            S::Int,
        );
        pinned(
            MetaEntityKind::WitherBoss,
            wither_boss::TARGET_B,
            17,
            S::Int,
        );
        pinned(
            MetaEntityKind::WitherBoss,
            wither_boss::TARGET_C,
            18,
            S::Int,
        );
        pinned(
            MetaEntityKind::WitherBoss,
            wither_boss::INVULNERABLE_TICKS,
            19,
            S::Int,
        );

        pinned(
            MetaEntityKind::Warden,
            warden::CLIENT_ANGER_LEVEL,
            16,
            S::Int,
        );

        pinned(
            MetaEntityKind::Armadillo,
            armadillo::STATE,
            17,
            S::ArmadilloState,
        );

        pinned(
            MetaEntityKind::EndCrystal,
            end_crystal::BEAM_TARGET,
            8,
            S::OptionalBlockPos,
        );
        pinned(
            MetaEntityKind::EndCrystal,
            end_crystal::SHOW_BOTTOM,
            9,
            S::Boolean,
        );

        pinned(MetaEntityKind::EnderDragon, ender_dragon::PHASE, 16, S::Int);

        pinned(
            MetaEntityKind::Llama,
            abstract_chested_horse::CHEST,
            18,
            S::Boolean,
        );
        pinned(MetaEntityKind::Llama, llama::STRENGTH, 19, S::Int);
        pinned(MetaEntityKind::Llama, llama::VARIANT, 20, S::Int);

        pinned(MetaEntityKind::Sniffer, sniffer::STATE, 17, S::SnifferState);
        pinned(
            MetaEntityKind::Sniffer,
            sniffer::DROP_SEED_AT_TICK,
            18,
            S::Int,
        );

        pinned(MetaEntityKind::SnowGolem, snow_golem::PUMPKIN, 16, S::Byte);

        pinned(MetaEntityKind::Allay, allay::DANCING, 16, S::Boolean);
        pinned(MetaEntityKind::Allay, allay::CAN_DUPLICATE, 17, S::Boolean);

        pinned(MetaEntityKind::Blaze, blaze::FLAGS, 16, S::Byte);

        pinned(MetaEntityKind::IronGolem, iron_golem::FLAGS, 16, S::Byte);

        // Breeze has the Mob base schema and bytecode-verified no metadata
        // rows of its own in 1.21.1.
        assert_eq!(
            expected_serializer(MetaEntityKind::Breeze, 15),
            Some(S::Byte)
        );
        assert_eq!(expected_serializer(MetaEntityKind::Breeze, 16), None);

        pinned(
            MetaEntityKind::FireworkRocket,
            firework_rocket::ITEM,
            8,
            S::ItemStack,
        );
        pinned(
            MetaEntityKind::FireworkRocket,
            firework_rocket::ATTACHED_TO,
            9,
            S::OptionalUnsignedInt,
        );
        pinned(
            MetaEntityKind::FireworkRocket,
            firework_rocket::SHOT_AT_ANGLE,
            10,
            S::Boolean,
        );

        pinned(
            MetaEntityKind::Display,
            display::INTERPOLATION_START_DELTA_TICKS,
            8,
            S::Int,
        );
        pinned(
            MetaEntityKind::Display,
            display::INTERPOLATION_DURATION,
            9,
            S::Int,
        );
        pinned(
            MetaEntityKind::Display,
            display::TELEPORT_DURATION,
            10,
            S::Int,
        );
        pinned(
            MetaEntityKind::Display,
            display::TRANSLATION,
            11,
            S::Vector3,
        );
        pinned(MetaEntityKind::Display, display::SCALE, 12, S::Vector3);
        pinned(
            MetaEntityKind::Display,
            display::LEFT_ROTATION,
            13,
            S::Quaternion,
        );
        pinned(
            MetaEntityKind::Display,
            display::RIGHT_ROTATION,
            14,
            S::Quaternion,
        );
        pinned(
            MetaEntityKind::Display,
            display::BILLBOARD_RENDER_CONSTRAINTS,
            15,
            S::Byte,
        );
        pinned(
            MetaEntityKind::Display,
            display::BRIGHTNESS_OVERRIDE,
            16,
            S::Int,
        );
        pinned(MetaEntityKind::Display, display::VIEW_RANGE, 17, S::Float);
        pinned(
            MetaEntityKind::Display,
            display::SHADOW_RADIUS,
            18,
            S::Float,
        );
        pinned(
            MetaEntityKind::Display,
            display::SHADOW_STRENGTH,
            19,
            S::Float,
        );
        pinned(MetaEntityKind::Display, display::WIDTH, 20, S::Float);
        pinned(MetaEntityKind::Display, display::HEIGHT, 21, S::Float);
        pinned(
            MetaEntityKind::Display,
            display::GLOW_COLOR_OVERRIDE,
            22,
            S::Int,
        );

        pinned(
            MetaEntityKind::TextDisplay,
            text_display::BACKGROUND,
            23,
            S::Int,
        );
        pinned(
            MetaEntityKind::TextDisplay,
            text_display::TEXT,
            24,
            S::Component,
        );
        pinned(
            MetaEntityKind::TextDisplay,
            text_display::STYLE_FLAGS,
            25,
            S::Byte,
        );
        pinned(
            MetaEntityKind::TextDisplay,
            text_display::LINE_WIDTH,
            26,
            S::Int,
        );
        pinned(
            MetaEntityKind::TextDisplay,
            text_display::SEE_THROUGH,
            27,
            S::Boolean,
        );

        pinned(
            MetaEntityKind::BlockDisplay,
            block_display::BLOCK_STATE,
            23,
            S::BlockState,
        );
    }

    #[test]
    fn checked_encoder_refuses_a_foreign_index() {
        let health = [living_health(20.0)];
        assert_eq!(
            encode_set_entity_data_checked(MetaEntityKind::LivingEntity, 7, &health),
            Ok(encode_set_entity_data(7, &health))
        );
        // The historical spelling: a Float written at index 8.
        let wrong_index = [MetaEntry::new(8, MetaValue::Float(20.0))];
        assert!(
            encode_set_entity_data_checked(MetaEntityKind::LivingEntity, 7, &wrong_index).is_err()
        );
        // A valid index from a different class tree.
        let wrong_kind = [cat_collar(10)];
        assert!(encode_set_entity_data_checked(MetaEntityKind::Wolf, 7, &wrong_kind).is_err());
        // The unchecked encoder still writes the wrong triple, byte for byte —
        // which is exactly why the checked entry point exists.
        let payload = encode_set_entity_data_payload(7, &wrong_index);
        assert_eq!(
            payload,
            vec![0x07, 0x08, 0x03, 0x41, 0xa0, 0x00, 0x00, 0xff]
        );
        assert!(encode_set_entity_data(7, &wrong_index).ends_with(&payload));
    }

    #[test]
    fn inherited_indices_resolve_through_the_parent_chain() {
        // A cat answers for every slot its ancestors and its own class define.
        assert_eq!(
            expected_serializer(MetaEntityKind::Cat, entity::SHARED_FLAGS),
            Some(MetaSerializer::Byte)
        );
        assert_eq!(
            expected_serializer(MetaEntityKind::Cat, living_entity::HEALTH),
            Some(MetaSerializer::Float)
        );
        assert_eq!(
            expected_serializer(MetaEntityKind::Cat, mob::MOB_FLAGS),
            Some(MetaSerializer::Byte)
        );
        assert_eq!(
            expected_serializer(MetaEntityKind::Cat, ageable_mob::BABY),
            Some(MetaSerializer::Boolean)
        );
        assert_eq!(
            expected_serializer(MetaEntityKind::Cat, tamable_animal::FLAGS),
            Some(MetaSerializer::Byte)
        );
        assert_eq!(
            expected_serializer(MetaEntityKind::Cat, tamable_animal::OWNER_UUID),
            Some(MetaSerializer::OptionalUuid)
        );
        assert_eq!(
            expected_serializer(MetaEntityKind::Cat, cat::COLLAR_COLOR),
            Some(MetaSerializer::Int)
        );
        // The same number is two different fields in two trees: `15` is the
        // mob flags byte for `Mob` and absorption for `Player`.
        assert_eq!(
            expected_serializer(MetaEntityKind::Mob, mob::MOB_FLAGS),
            Some(MetaSerializer::Byte)
        );
        assert_eq!(
            expected_serializer(MetaEntityKind::Player, mob::MOB_FLAGS),
            Some(MetaSerializer::Float)
        );
        assert!(
            validate_entry(MetaEntityKind::Player, mob::MOB_FLAGS, MetaSerializer::Byte).is_err()
        );
    }

    #[test]
    fn undefined_indices_are_rejected() {
        assert_eq!(expected_serializer(MetaEntityKind::Entity, 8), None);
        assert!(validate_entry(MetaEntityKind::Entity, 8, MetaSerializer::Byte).is_err());
        assert!(validate_entry(MetaEntityKind::Entity, 255, MetaSerializer::Byte).is_err());
        // `Mob` owns only its flags byte; `Player` only its own six slots.
        assert_eq!(expected_serializer(MetaEntityKind::Mob, 16), None);
        assert_eq!(expected_serializer(MetaEntityKind::Player, 21), None);
        assert_eq!(expected_serializer(MetaEntityKind::ItemEntity, 9), None);
    }

    #[test]
    fn regression_health_is_index_nine_not_eight() {
        assert!(validate_entry(MetaEntityKind::LivingEntity, 8, MetaSerializer::Float).is_err());
        assert_eq!(
            validate_entry(MetaEntityKind::LivingEntity, 8, MetaSerializer::Byte),
            Ok(())
        );
        assert_eq!(
            validate_entry(MetaEntityKind::LivingEntity, 9, MetaSerializer::Float),
            Ok(())
        );
    }

    #[test]
    fn regression_sleeping_pos_serializer_is_required() {
        assert!(validate_entry(
            MetaEntityKind::LivingEntity,
            14,
            MetaSerializer::SnifferState
        )
        .is_err());
        assert_eq!(
            validate_entry(
                MetaEntityKind::LivingEntity,
                14,
                MetaSerializer::OptionalBlockPos
            ),
            Ok(())
        );
    }

    #[test]
    fn regression_cat_collar_is_index_twenty_two() {
        assert!(validate_entry(MetaEntityKind::Cat, 21, MetaSerializer::Int).is_err());
        assert_eq!(
            validate_entry(MetaEntityKind::Cat, 21, MetaSerializer::Boolean),
            Ok(())
        );
        assert_eq!(
            validate_entry(MetaEntityKind::Cat, 22, MetaSerializer::Int),
            Ok(())
        );
    }

    #[test]
    fn regression_sheep_wool_is_index_seventeen() {
        assert!(validate_entry(MetaEntityKind::Sheep, 16, MetaSerializer::Byte).is_err());
        assert_eq!(
            validate_entry(MetaEntityKind::Sheep, 16, MetaSerializer::Boolean),
            Ok(())
        );
        assert_eq!(
            validate_entry(MetaEntityKind::Sheep, 17, MetaSerializer::Byte),
            Ok(())
        );
    }

    #[test]
    fn regression_creeper_powered_is_index_seventeen() {
        assert_eq!(
            validate_entry(MetaEntityKind::Creeper, 17, MetaSerializer::Boolean),
            Ok(())
        );
        assert_eq!(
            validate_entry(MetaEntityKind::Creeper, 16, MetaSerializer::Int),
            Ok(())
        );
    }

    #[test]
    fn typed_constructors_satisfy_their_own_kind() {
        let cases = [
            (
                MetaEntityKind::Entity,
                shared_flags(flags::SHARED_SPRINTING),
            ),
            (MetaEntityKind::Entity, air_supply(300)),
            (MetaEntityKind::Entity, custom_name(None)),
            (MetaEntityKind::Entity, custom_name_visible(true)),
            (MetaEntityKind::Entity, silent(true)),
            (MetaEntityKind::Entity, entity_pose(2)),
            (MetaEntityKind::Entity, ticks_frozen(0)),
            (
                MetaEntityKind::LivingEntity,
                living_flags(flags::LIVING_USE_MAIN_HAND),
            ),
            (MetaEntityKind::LivingEntity, living_health(20.0)),
            (MetaEntityKind::LivingEntity, living_sleeping_pos(None)),
            (MetaEntityKind::Mob, mob_flags(0)),
            (MetaEntityKind::Villager, villager_data(0, 2, 1)),
            (MetaEntityKind::Sheep, sheep_wool(0x0f)),
            (MetaEntityKind::Cat, cat_collar(10)),
            (MetaEntityKind::Wolf, wolf_collar(10)),
            (MetaEntityKind::Creeper, creeper_powered(true)),
            (MetaEntityKind::ItemEntity, item_entity_stack(vec![0x00])),
        ];
        for (kind, entry) in cases {
            assert_eq!(entry.validate(kind), Ok(()), "{kind:?} {entry:?}");
        }
        // A constructor from one tree is not a valid entry in another.
        assert!(cat_collar(10).validate(MetaEntityKind::Wolf).is_err());
        assert!(cat_collar(10).validate(MetaEntityKind::Sheep).is_err());
    }

    #[test]
    fn validated_entries_encode_without_a_schema_surprise() {
        // The health write the old call site spelled by hand: index 9, Float.
        let entry = living_health(4.5);
        assert_eq!(entry.index(), 9);
        assert_eq!(entry.serializer(), MetaSerializer::Float);
        assert_eq!(entry.value(), &MetaValue::Float(4.5));
        assert_eq!(
            body(entry.index(), entry.value().clone()),
            vec![
                0x07,
                0x09,
                0x03,
                0x40,
                0x90,
                0x00,
                0x00,
                METADATA_TERMINATOR
            ]
        );
    }
}
