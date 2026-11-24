// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
//! Zero-dependency Minecraft 1.21.1 (Protocol 767) TCP wire codec, RFC 1950 zlib,
//! and Handshake -> Login -> Configuration -> Play state machine.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::io::{ErrorKind, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use megu_proto::{PacketDir, PacketPhase, ProtocolCodec, RecipeSyncMode};

use crate::online_auth::{self, Aes128Cfb8, OnlineAuthOptions, SessionJoinClient};
use crate::render::ClientVfxHost;
use crate::world::WorldAnimationHost;
use megu_client_mesh::vfx_engine::Vfx1Packet;
use megu_kernel::skeletal_anim::decode_animation_trigger;

/// Protocol 767 / Minecraft 1.21.1 — the single canonical version constant,
/// sourced from `megu-proto` rather than a local literal.
pub const PROTOCOL_767: i32 = megu_proto::ProtocolVersion::V1_21_1.number();
pub const MAX_PACKET_SIZE: usize = 16 * 1024 * 1024;
pub const CHANNEL_MAGIC_V1: &str = "megu:magic_v1";
pub const CHANNEL_VFX_V1: &str = "megu:vfx_v1";
pub const CHANNEL_ANIM_V1: &str = "megu:anim_v1";
pub const CHANNEL_MOTION_V1: &str = "megu:motion_v1";
pub const CHANNEL_SEASON_V1: &str = "megu:season_v1";
/// Product-level alias the "megu" custom-payload family is addressed by on
/// some peers; decoded identically to [`CHANNEL_SEASON_V1`].
pub const CHANNEL_MEGU_SEASON: &str = "megu:season";

const MAX_MAGIC_COOLDOWNS: usize = 512;
const MAX_MAGIC_STATUSES: usize = 256;
const MAX_MAGIC_ID_LEN: usize = 128;

/// The two active MAG1 wire cast variants. An instant cast has no wire progress.
#[derive(Clone, Debug, PartialEq)]
pub enum DecodedMagicCastState {
    CastTime {
        total_ticks: u32,
        elapsed_ticks: u32,
    },
    Channeled {
        interval_ticks: u32,
        max_ticks: u32,
        elapsed_ticks: u32,
        mana_per_pulse: f32,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub struct DecodedMagicCast {
    pub spell_id: String,
    pub state: DecodedMagicCastState,
}

#[derive(Clone, Debug, PartialEq)]
pub struct DecodedMagicStatus {
    pub id: String,
    pub amplifier: u8,
    pub remaining_ticks: u32,
    pub absorption_remaining: f32,
}

/// Complete MAG1 state, matching the server's `MagicSyncState` wire fields.
#[derive(Clone, Debug, PartialEq)]
pub struct DecodedMagicSync {
    pub mana: f32,
    pub max_mana: f32,
    pub cast: Option<DecodedMagicCast>,
    pub cooldowns: BTreeMap<String, u32>,
    pub statuses: Vec<DecodedMagicStatus>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DecodedMotionHand {
    MainHand,
    OffHand,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DecodedMotionPhase {
    Idle,
    Anticipation,
    ActiveHitWindow,
    Recovery,
}

/// MOT1 fixed-width combat trigger. Multi-byte integers use network byte order.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DecodedMotionTrigger {
    pub entity_id: u64,
    pub action_id: u32,
    pub combo_index: u16,
    pub hand: DecodedMotionHand,
    pub speed_multiplier: f32,
    pub elapsed_ticks: u32,
    pub phase: DecodedMotionPhase,
}

/// Season sync payload carried by one fixed 19-byte `SSN1` frame on
/// [`CHANNEL_SEASON_V1`]. The layout is byte-identical to the `SSN1` codec
/// in `mod-adapter-core::season_primitives` (`encode_ssn1`/`decode_ssn1`):
/// `SSN1 | u8 version | u8 sub_season | u16 progress_q16 | u16
/// day_of_season | u16 daylight_q16 | u8 foliage_rgb[3] | u8 grass_rgb[3]
/// | u8 flags`, all multi-byte fields big-endian.
#[derive(Clone, Debug, PartialEq)]
pub struct SeasonWireSync {
    /// Sub-season calendar index `0..12` (EarlySpring .. LateWinter).
    pub sub_season_idx: u8,
    /// Progress through the current sub-season, `0.0..=1.0` (q16 on the wire).
    pub progress_in_sub_season: f32,
    /// Whole days elapsed inside the current broad season.
    pub day_of_season: u16,
    /// Fraction of the day that is daylight, `0.0..=1.0` (q16 on the wire).
    pub daylight_fraction: f32,
    /// Quantized foliage RGB multiplier (`255` = 1.0).
    pub foliage_rgb: [u8; 3],
    /// Quantized grass RGB multiplier (`255` = 1.0).
    pub grass_rgb: [u8; 3],
    /// Dimension runs wet/dry seasons (tropical climate).
    pub tropical_dim: bool,
    /// Calendar is locked server-side.
    pub frozen_season: bool,
    /// Seasonal snow/freeze is enabled on this dimension.
    pub snow_enabled: bool,
}

pub const SSN1_MAGIC: &[u8; 4] = b"SSN1";
pub const SSN1_VERSION: u8 = 1;
/// Exact `SSN1` frame length in bytes.
pub const SSN1_FRAME_LEN: usize = 19;

const SSN1_FLAG_TROPICAL_DIM: u8 = 1 << 0;
const SSN1_FLAG_FROZEN_SEASON: u8 = 1 << 1;
const SSN1_FLAG_SNOW_ENABLED: u8 = 1 << 2;
const SSN1_FLAG_KNOWN_MASK: u8 =
    SSN1_FLAG_TROPICAL_DIM | SSN1_FLAG_FROZEN_SEASON | SSN1_FLAG_SNOW_ENABLED;

#[derive(Clone, Debug, PartialEq)]
pub enum ClientCustomChannelEvent {
    Vfx(Vec<u8>),
    Anim(Vec<u8>),
    Magic(DecodedMagicSync),
    Motion(DecodedMotionTrigger),
    Season(SeasonWireSync),
    Unknown { channel: String, payload: Vec<u8> },
}

fn magic_f32(reader: &mut WireReader<'_>) -> Result<f32, String> {
    let value = f32::from_le_bytes(reader.take(4)?.try_into().unwrap());
    if !value.is_finite() {
        return Err("non-finite MAG1 float".to_string());
    }
    Ok(value)
}

fn magic_id(reader: &mut WireReader<'_>) -> Result<String, String> {
    let id = reader.read_string(MAX_MAGIC_ID_LEN)?;
    if id.is_empty() {
        return Err("empty MAG1 identifier".to_string());
    }
    Ok(id)
}

// The upstream MAG1 codec writes unsigned u32 Minecraft VarInts.
fn magic_varint(reader: &mut WireReader<'_>) -> Result<u32, String> {
    Ok(reader.read_varint()? as u32)
}

pub fn decode_magic_sync_payload(payload: &[u8]) -> Result<DecodedMagicSync, String> {
    let mut reader = WireReader::new(payload);
    if reader.take(4)? != b"MAG1" {
        return Err("invalid MAG1 magic".to_string());
    }
    let mana = magic_f32(&mut reader)?;
    let max_mana = magic_f32(&mut reader)?;
    let cast = match reader.read_u8()? {
        0 => None,
        tag @ (1 | 2) => {
            let spell_id = magic_id(&mut reader)?;
            let state = if tag == 1 {
                let total_ticks = magic_varint(&mut reader)?;
                let elapsed_ticks = magic_varint(&mut reader)?;
                if total_ticks == 0 {
                    return Err("zero MAG1 cast duration".to_string());
                }
                DecodedMagicCastState::CastTime {
                    total_ticks,
                    elapsed_ticks,
                }
            } else {
                let interval_ticks = magic_varint(&mut reader)?;
                let max_ticks = magic_varint(&mut reader)?;
                let elapsed_ticks = magic_varint(&mut reader)?;
                let mana_per_pulse = magic_f32(&mut reader)?;
                if interval_ticks == 0 || max_ticks == 0 {
                    return Err("zero MAG1 channel duration".to_string());
                }
                DecodedMagicCastState::Channeled {
                    interval_ticks,
                    max_ticks,
                    elapsed_ticks,
                    mana_per_pulse,
                }
            };
            Some(DecodedMagicCast { spell_id, state })
        }
        _ => return Err("invalid MAG1 cast tag".to_string()),
    };

    let cooldown_count = magic_varint(&mut reader)? as usize;
    if cooldown_count > MAX_MAGIC_COOLDOWNS {
        return Err("MAG1 cooldown count exceeds limit".to_string());
    }
    let mut cooldowns = BTreeMap::new();
    for _ in 0..cooldown_count {
        let id = magic_id(&mut reader)?;
        if cooldowns
            .last_key_value()
            .is_some_and(|(previous, _)| id <= *previous)
        {
            return Err("noncanonical MAG1 cooldown ids".to_string());
        }
        cooldowns.insert(id, magic_varint(&mut reader)?);
    }

    let status_count = magic_varint(&mut reader)? as usize;
    if status_count > MAX_MAGIC_STATUSES {
        return Err("MAG1 status count exceeds limit".to_string());
    }
    let mut statuses: Vec<DecodedMagicStatus> = Vec::with_capacity(status_count);
    for _ in 0..status_count {
        let id = magic_id(&mut reader)?;
        if statuses.last().is_some_and(|previous| id <= previous.id) {
            return Err("noncanonical MAG1 status ids".to_string());
        }
        let amplifier = reader.read_u8()?;
        let remaining_ticks = magic_varint(&mut reader)?;
        let absorption_remaining = magic_f32(&mut reader)?;
        statuses.push(DecodedMagicStatus {
            id,
            amplifier,
            remaining_ticks,
            absorption_remaining,
        });
    }

    if reader.remaining() != 0 {
        return Err("trailing MAG1 bytes".to_string());
    }
    Ok(DecodedMagicSync {
        mana,
        max_mana,
        cast,
        cooldowns,
        statuses,
    })
}

pub fn decode_motion_trigger_payload(payload: &[u8]) -> Result<DecodedMotionTrigger, String> {
    if payload.len() != 28 || &payload[..4] != b"MOT1" {
        return Err("invalid MOT1 packet length or magic".to_string());
    }
    let mut reader = WireReader::new(&payload[4..]);
    let entity_id = reader.read_u64()?;
    let action_id = u32::from_be_bytes(reader.take(4)?.try_into().unwrap());
    let combo_index = reader.read_u16()?;
    let hand = match reader.read_u8()? {
        0 => DecodedMotionHand::MainHand,
        1 => DecodedMotionHand::OffHand,
        _ => return Err("invalid MOT1 hand".to_string()),
    };
    let speed_multiplier = reader.read_f32()?;
    if !speed_multiplier.is_finite() || !(0.25..=4.0).contains(&speed_multiplier) {
        return Err("invalid MOT1 speed".to_string());
    }
    let elapsed_ticks = u32::from_be_bytes(reader.take(4)?.try_into().unwrap());
    let phase = match reader.read_u8()? {
        0 => DecodedMotionPhase::Idle,
        1 => DecodedMotionPhase::Anticipation,
        2 => DecodedMotionPhase::ActiveHitWindow,
        3 => DecodedMotionPhase::Recovery,
        _ => return Err("invalid MOT1 phase".to_string()),
    };
    Ok(DecodedMotionTrigger {
        entity_id,
        action_id,
        combo_index,
        hand,
        speed_multiplier,
        elapsed_ticks,
        phase,
    })
}

/// Decode a fixed 19-byte `SSN1` season sync frame, fail-closed: wrong
/// length, bad magic, unknown `version`, `sub_season_idx >= 12`, or any
/// flag bit outside the known mask all reject.
pub fn decode_season_sync_payload(payload: &[u8]) -> Result<SeasonWireSync, String> {
    if payload.len() != SSN1_FRAME_LEN {
        return Err("invalid SSN1 frame length".to_string());
    }
    let mut reader = WireReader::new(payload);
    if reader.take(4)? != SSN1_MAGIC {
        return Err("invalid SSN1 magic".to_string());
    }
    if reader.read_u8()? != SSN1_VERSION {
        return Err("unsupported SSN1 version".to_string());
    }
    let sub_season_idx = reader.read_u8()?;
    if sub_season_idx >= 12 {
        return Err("SSN1 sub-season index out of range".to_string());
    }
    let progress_in_sub_season = f32::from(reader.read_u16()?) / 65535.0;
    let day_of_season = reader.read_u16()?;
    let daylight_fraction = f32::from(reader.read_u16()?) / 65535.0;
    let foliage_rgb: [u8; 3] = reader.take(3)?.try_into().unwrap();
    let grass_rgb: [u8; 3] = reader.take(3)?.try_into().unwrap();
    let flags = reader.read_u8()?;
    if flags & !SSN1_FLAG_KNOWN_MASK != 0 {
        return Err("SSN1 unknown flag bits".to_string());
    }
    debug_assert_eq!(reader.remaining(), 0);
    Ok(SeasonWireSync {
        sub_season_idx,
        progress_in_sub_season,
        day_of_season,
        daylight_fraction,
        foliage_rgb,
        grass_rgb,
        tropical_dim: flags & SSN1_FLAG_TROPICAL_DIM != 0,
        frozen_season: flags & SSN1_FLAG_FROZEN_SEASON != 0,
        snow_enabled: flags & SSN1_FLAG_SNOW_ENABLED != 0,
    })
}

/// Validate recognized channels before a host is permitted to mutate state.
pub fn decode_custom_channel_event(
    channel: &str,
    payload: &[u8],
) -> Result<ClientCustomChannelEvent, String> {
    match channel {
        CHANNEL_VFX_V1 => {
            Vfx1Packet::decode(payload).map_err(|err| format!("invalid VFX1: {err:?}"))?;
            Ok(ClientCustomChannelEvent::Vfx(payload.to_vec()))
        }
        CHANNEL_ANIM_V1 => {
            decode_animation_trigger(payload).map_err(|err| format!("invalid ANM1: {err:?}"))?;
            Ok(ClientCustomChannelEvent::Anim(payload.to_vec()))
        }
        CHANNEL_MAGIC_V1 => decode_magic_sync_payload(payload).map(ClientCustomChannelEvent::Magic),
        CHANNEL_MOTION_V1 => {
            decode_motion_trigger_payload(payload).map(ClientCustomChannelEvent::Motion)
        }
        CHANNEL_SEASON_V1 | CHANNEL_MEGU_SEASON => {
            decode_season_sync_payload(payload).map(ClientCustomChannelEvent::Season)
        }
        _ => Ok(ClientCustomChannelEvent::Unknown {
            channel: channel.to_string(),
            payload: payload.to_vec(),
        }),
    }
}

/// One decoded frame from the "megu" custom-payload family (MAG1 / VFX1 /
/// ANM1 / MOT1 / SSN1). Unlike [`decode_custom_channel_event`], which keeps
/// host-bound bytes opaque and reports unknown channels, this router
/// resolves every recognized frame into a typed value and fails closed.
#[derive(Clone, Debug, PartialEq)]
pub enum MeguCustomPayload {
    Magic(DecodedMagicSync),
    Vfx(Vec<u8>),
    Anim(Vec<u8>),
    Motion(DecodedMotionTrigger),
    Season(SeasonWireSync),
}

/// Decode a "megu" custom-payload frame for live session routing.
/// Returns `None` — ignored, never an error — on an unrecognized channel
/// or any malformed payload (unknown version, truncation, out-of-range
/// fields).
pub fn decode_megu_custom_payload(channel: &str, data: &[u8]) -> Option<MeguCustomPayload> {
    match decode_custom_channel_event(channel, data).ok()? {
        ClientCustomChannelEvent::Magic(sync) => Some(MeguCustomPayload::Magic(sync)),
        ClientCustomChannelEvent::Vfx(bytes) => Some(MeguCustomPayload::Vfx(bytes)),
        ClientCustomChannelEvent::Anim(bytes) => Some(MeguCustomPayload::Anim(bytes)),
        ClientCustomChannelEvent::Motion(trigger) => Some(MeguCustomPayload::Motion(trigger)),
        ClientCustomChannelEvent::Season(sync) => Some(MeguCustomPayload::Season(sync)),
        ClientCustomChannelEvent::Unknown { .. } => None,
    }
}

/// Apply VFX1/ANM1 to the respective host and return decoded non-host events.
pub fn dispatch_custom_channel_to_hosts(
    channel: &str,
    payload: &[u8],
    vfx_host: &mut ClientVfxHost,
    anim_host: &mut WorldAnimationHost,
) -> Result<ClientCustomChannelEvent, String> {
    let event = decode_custom_channel_event(channel, payload)?;
    match &event {
        ClientCustomChannelEvent::Vfx(data) => {
            vfx_host
                .ingest_vfx1_payload(data)
                .map_err(|err| format!("VFX1 host rejected payload: {err:?}"))?;
        }
        ClientCustomChannelEvent::Anim(data) => {
            anim_host
                .ingest_anm1_payload(data)
                .map_err(|err| format!("ANM1 host rejected payload: {err:?}"))?;
        }
        _ => {}
    }
    Ok(event)
}

/// Decode the body of a clientbound `custom_payload` packet (VarInt-length
/// channel identifier followed by unframed channel bytes).
pub fn decode_custom_payload_packet(body: &[u8]) -> Result<ClientCustomChannelEvent, String> {
    let mut reader = WireReader::new(body);
    let channel = reader.read_string(32767)?;
    decode_custom_channel_event(&channel, reader.rest())
}
const MAX_NBT_DEPTH: usize = 64;
const MAX_NBT_ARRAY: usize = 1_048_576;
const MAX_NBT_TAGS: usize = 262_144;
/// Upper bound on the section count a `level_chunk_with_light` /
/// `light_update` packet may claim. The count derives from server-sent
/// `dimension_type` NBT (`height / 16`), so it is attacker-controlled:
/// 256 sections is a 4096-block world height, covering the 1.21.1
/// dimension span while keeping per-section `Vec` pre-allocations bounded.
const MAX_WORLD_SECTIONS: usize = 256;
/// Smallest legal paletted section on the 767 wire: 2-byte block count,
/// a 3-byte single-value block container, and a 3-byte single-value biome
/// container.
const MIN_WIRE_SECTION_BYTES: usize = 8;

// ---------------------------------------------------------------------------
// Wire reader / writer primitives
// ---------------------------------------------------------------------------

pub struct WireReader<'a> {
    data: &'a [u8],
    pos: usize,
    remaining_tags: usize,
}

impl<'a> WireReader<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self {
            data,
            pos: 0,
            remaining_tags: MAX_NBT_TAGS,
        }
    }

    pub fn remaining(&self) -> usize {
        self.data.len() - self.pos
    }

    pub fn rest(&self) -> &'a [u8] {
        &self.data[self.pos..]
    }

    pub fn take(&mut self, n: usize) -> Result<&'a [u8], String> {
        let end = self
            .pos
            .checked_add(n)
            .ok_or_else(|| "wire length overflow".to_string())?;
        if end > self.data.len() {
            return Err(format!(
                "truncated wire field: needed {n} bytes, {} remaining",
                self.remaining()
            ));
        }
        let slice = &self.data[self.pos..end];
        self.pos = end;
        Ok(slice)
    }

    pub fn read_u8(&mut self) -> Result<u8, String> {
        Ok(self.take(1)?[0])
    }

    pub fn read_i8(&mut self) -> Result<i8, String> {
        Ok(self.read_u8()? as i8)
    }

    pub fn read_bool(&mut self) -> Result<bool, String> {
        Ok(self.read_u8()? != 0)
    }

    pub fn read_i16(&mut self) -> Result<i16, String> {
        Ok(i16::from_be_bytes(self.take(2)?.try_into().unwrap()))
    }

    pub fn read_u16(&mut self) -> Result<u16, String> {
        Ok(u16::from_be_bytes(self.take(2)?.try_into().unwrap()))
    }

    pub fn read_i32(&mut self) -> Result<i32, String> {
        Ok(i32::from_be_bytes(self.take(4)?.try_into().unwrap()))
    }

    pub fn read_i64(&mut self) -> Result<i64, String> {
        Ok(i64::from_be_bytes(self.take(8)?.try_into().unwrap()))
    }

    pub fn read_u64(&mut self) -> Result<u64, String> {
        Ok(u64::from_be_bytes(self.take(8)?.try_into().unwrap()))
    }

    pub fn read_f32(&mut self) -> Result<f32, String> {
        Ok(f32::from_be_bytes(self.take(4)?.try_into().unwrap()))
    }

    pub fn read_f64(&mut self) -> Result<f64, String> {
        Ok(f64::from_be_bytes(self.take(8)?.try_into().unwrap()))
    }

    /// Delegated to `megu_core`'s canonical reader — identical bounds
    /// (final byte `& 0xf0` rejected, five bytes max); the `String` error
    /// surface this module's callers use is kept.
    pub fn read_varint(&mut self) -> Result<i32, String> {
        let (value, len) = megu_core::varint::read_varint_i32(self.rest()).map_err(|e| {
            match e {
                megu_core::varint::VarIntError::UnexpectedEof => {
                    "truncated wire: need 1 byte(s), 0 left".to_string()
                }
                other => other.to_string(),
            }
        })?;
        self.pos += len;
        Ok(value)
    }

    /// Delegated to `megu_core`'s canonical reader — identical bounds
    /// (final byte `& 0xfe` rejected, ten bytes max).
    pub fn read_varlong(&mut self) -> Result<i64, String> {
        let (value, len) = megu_core::varint::read_varlong_i64(self.rest()).map_err(|e| {
            match e {
                megu_core::varint::VarIntError::UnexpectedEof => {
                    "truncated wire: need 1 byte(s), 0 left".to_string()
                }
                other => other.to_string(),
            }
        })?;
        self.pos += len;
        Ok(value)
    }

    pub fn read_string(&mut self, max_bytes: usize) -> Result<String, String> {
        let len = usize::try_from(self.read_varint()?)
            .map_err(|_| "negative string length".to_string())?;
        if len > max_bytes {
            return Err(format!("string length {len} exceeds bound {max_bytes}"));
        }
        let raw = self.take(len)?;
        std::str::from_utf8(raw)
            .map(str::to_string)
            .map_err(|_| "invalid UTF-8 string".to_string())
    }

    pub fn read_uuid(&mut self) -> Result<[u8; 16], String> {
        Ok(self.take(16)?.try_into().unwrap())
    }

    pub fn read_position(&mut self) -> Result<[i32; 3], String> {
        let packed = self.read_i64()?;
        Ok(unpack_position(packed))
    }

    pub fn read_bitset(&mut self, max_longs: usize) -> Result<Vec<u64>, String> {
        let count = usize::try_from(self.read_varint()?)
            .map_err(|_| "negative bitset length".to_string())?;
        if count > max_longs {
            return Err(format!("bitset length {count} exceeds bound {max_longs}"));
        }
        let mut words = Vec::with_capacity(count);
        for _ in 0..count {
            words.push(self.read_u64()?);
        }
        Ok(words)
    }

    /// 1.21.5+ `java.util.BitSet` wire form: `VarInt` byte count then
    /// little-endian bytes (`byte[i/8] >> (i%8)`), normalized to the u64-word
    /// representation [`bitset_get`] reads.
    pub fn read_bitset_bytes(&mut self, max_bytes: usize) -> Result<Vec<u64>, String> {
        let count = usize::try_from(self.read_varint()?)
            .map_err(|_| "negative bitset byte length".to_string())?;
        if count > max_bytes {
            return Err(format!(
                "bitset byte length {count} exceeds bound {max_bytes}"
            ));
        }
        let bytes = self.take(count)?;
        let mut words = Vec::with_capacity(count.div_ceil(8));
        for chunk in bytes.chunks(8) {
            let mut word = [0u8; 8];
            word[..chunk.len()].copy_from_slice(chunk);
            words.push(u64::from_le_bytes(word));
        }
        Ok(words)
    }

    pub fn read_nbt(&mut self) -> Result<NbtValue, String> {
        let tag = self.read_u8()?;
        self.read_nbt_payload(tag, 0)
    }

    fn read_nbt_string(&mut self) -> Result<String, String> {
        let len = usize::from(self.read_u16()?);
        let raw = self.take(len)?;
        Ok(String::from_utf8_lossy(raw).into_owned())
    }

    fn consume_nbt_tag_budget(&mut self) -> Result<(), String> {
        if self.remaining_tags == 0 {
            return Err("NBT tag budget exceeded".to_string());
        }
        self.remaining_tags -= 1;
        Ok(())
    }

    fn read_nbt_array_len(
        &mut self,
        element_size: usize,
        label: &str,
    ) -> Result<(usize, usize), String> {
        let raw_len = self.read_i32()?;
        if raw_len < 0 {
            return Err(format!("negative NBT {label} length"));
        }
        let len = raw_len as usize;
        if len > MAX_NBT_ARRAY {
            return Err(format!("NBT {label} length exceeds bound"));
        }
        let bytes = len
            .checked_mul(element_size)
            .ok_or_else(|| format!("NBT {label} length overflow"))?;
        if bytes > self.remaining() {
            return Err(format!(
                "NBT {label} requires {bytes} bytes, {} remain",
                self.remaining()
            ));
        }
        Ok((len, bytes))
    }

    fn read_nbt_payload(&mut self, tag: u8, depth: usize) -> Result<NbtValue, String> {
        if depth >= MAX_NBT_DEPTH {
            return Err("NBT nesting depth exceeded".to_string());
        }
        self.consume_nbt_tag_budget()?;
        match tag {
            0 => Ok(NbtValue::End),
            1 => Ok(NbtValue::Byte(self.read_u8()? as i8)),
            2 => Ok(NbtValue::Short(self.read_i16()?)),
            3 => Ok(NbtValue::Int(self.read_i32()?)),
            4 => Ok(NbtValue::Long(self.read_i64()?)),
            5 => Ok(NbtValue::Float(self.read_f32()?)),
            6 => Ok(NbtValue::Double(self.read_f64()?)),
            7 => {
                let (len, bytes) = self.read_nbt_array_len(1, "byte array")?;
                let _ = self.take(bytes)?;
                Ok(NbtValue::ArrayLen(len))
            }
            8 => Ok(NbtValue::String(self.read_nbt_string()?)),
            9 => {
                let elem_tag = self.read_u8()?;
                let len = usize::try_from(self.read_i32()?)
                    .map_err(|_| "negative NBT list".to_string())?;
                if len > MAX_NBT_ARRAY {
                    return Err("NBT list exceeds bound".to_string());
                }
                if elem_tag == 0 && len != 0 {
                    return Err(format!(
                        "NBT list declares TAG_End elements with non-zero length {len}"
                    ));
                }
                if elem_tag > 12 {
                    return Err(format!("invalid NBT list element tag {elem_tag}"));
                }
                if len > self.remaining() {
                    return Err(format!(
                        "NBT list length {len} exceeds {} remaining bytes",
                        self.remaining()
                    ));
                }
                let mut items = Vec::with_capacity(len.min(self.remaining()).min(4096));
                for _ in 0..len {
                    items.push(self.read_nbt_payload(elem_tag, depth + 1)?);
                }
                Ok(NbtValue::List(items))
            }
            10 => {
                let mut map = HashMap::new();
                loop {
                    let child = self.read_u8()?;
                    if child == 0 {
                        return Ok(NbtValue::Compound(map));
                    }
                    let key = self.read_nbt_string()?;
                    let value = self.read_nbt_payload(child, depth + 1)?;
                    map.insert(key, value);
                }
            }
            11 => {
                let (len, bytes) = self.read_nbt_array_len(4, "int array")?;
                let _ = self.take(bytes)?;
                Ok(NbtValue::ArrayLen(len))
            }
            12 => {
                let (len, bytes) = self.read_nbt_array_len(8, "long array")?;
                let _ = self.take(bytes)?;
                Ok(NbtValue::ArrayLen(len))
            }
            other => Err(format!("unknown NBT tag {other}")),
        }
    }
}

/// Read one anonymous-NBT value and return its raw wire bytes. The bytes are
/// validated by [`WireReader::read_nbt`] (bounded depth/array lengths) before
/// being handed back, so callers keep the exact payload without re-parsing.
fn read_nbt_raw<'a>(reader: &mut WireReader<'a>) -> Result<&'a [u8], String> {
    let before = reader.rest();
    let _ = reader.read_nbt()?;
    let consumed = before.len() - reader.remaining();
    Ok(&before[..consumed])
}

#[derive(Clone, Debug, PartialEq)]
pub enum NbtValue {
    End,
    Byte(i8),
    Short(i16),
    Int(i32),
    Long(i64),
    Float(f32),
    Double(f64),
    String(String),
    List(Vec<NbtValue>),
    Compound(HashMap<String, NbtValue>),
    ArrayLen(usize),
}

impl NbtValue {
    pub fn as_i32(&self) -> Option<i32> {
        match *self {
            Self::Byte(v) => Some(i32::from(v)),
            Self::Short(v) => Some(i32::from(v)),
            Self::Int(v) => Some(v),
            Self::Long(v) => i32::try_from(v).ok(),
            _ => None,
        }
    }

    pub fn as_bool(&self) -> Option<bool> {
        self.as_i32().map(|v| v != 0)
    }

    pub fn extract_text(&self) -> String {
        match self {
            Self::String(s) => s.clone(),
            Self::Compound(map) => {
                let mut out = String::new();
                if let Some(Self::String(text)) = map.get("text") {
                    out.push_str(text);
                } else if let Some(Self::String(trans)) = map.get("translate") {
                    out.push_str(trans);
                }
                if let Some(Self::List(extra)) = map.get("extra") {
                    for part in extra {
                        out.push_str(&part.extract_text());
                    }
                }
                out
            }
            Self::List(items) => items.iter().map(Self::extract_text).collect(),
            _ => String::new(),
        }
    }
}

/// Delegated to `megu_core`'s canonical VarInt encoder.
pub fn write_varint(buf: &mut Vec<u8>, value: i32) {
    megu_core::varint::write_varint_i32(value, buf);
}

/// Delegated to `megu_core`'s canonical VarLong encoder.
pub fn write_varlong(buf: &mut Vec<u8>, value: i64) {
    megu_core::varint::write_varlong_i64(value, buf);
}

pub fn write_string(buf: &mut Vec<u8>, s: &str) {
    write_varint(buf, s.len() as i32);
    buf.extend_from_slice(s.as_bytes());
}

/// `x:26 | z:26 | y:12` packing, delegated to `megu_proto::BlockPos`.
pub fn pack_position(pos: [i32; 3]) -> i64 {
    let [x, y, z] = pos;
    megu_proto::BlockPos::new(x, y, z).pack()
}

/// Inverse of [`pack_position`], sign-extending each field.
pub fn unpack_position(packed: i64) -> [i32; 3] {
    let p = megu_proto::BlockPos::unpack(packed);
    [p.x, p.y, p.z]
}

pub fn bitset_get(words: &[u64], index: usize) -> bool {
    let word = index / 64;
    let bit = index % 64;
    words.get(word).is_some_and(|w| (w >> bit) & 1 != 0)
}

pub fn bitset_count_ones(words: &[u64]) -> usize {
    words.iter().map(|w| w.count_ones() as usize).sum()
}

pub fn parse_uuid(input: &str) -> Result<[u8; 16], String> {
    let hex: String = input.chars().filter(|&c| c != '-').collect();
    if hex.len() != 32 {
        return Err(format!("invalid UUID length: '{input}'"));
    }
    let mut out = [0u8; 16];
    for (idx, slot) in out.iter_mut().enumerate() {
        let part = &hex[idx * 2..idx * 2 + 2];
        *slot = u8::from_str_radix(part, 16).map_err(|_| format!("invalid UUID hex: '{input}'"))?;
    }
    Ok(out)
}

/// Vanilla Minecraft offline-mode UUID v3 (`MD5("OfflinePlayer:" + name)`).
pub fn offline_uuid(username: &str) -> [u8; 16] {
    megu_core::crypto::offline_player_uuid(username)
}

// ---------------------------------------------------------------------------
// Pure-Rust RFC 1950 Zlib / RFC 1951 DEFLATE (zero external dependencies)
// ---------------------------------------------------------------------------

pub fn adler32(data: &[u8]) -> u32 {
    megu_core::zip_archive::adler32(data)
}

/// Encode bytes into a valid RFC 1950 zlib stream using RFC 1951 stored blocks.
pub fn zlib_compress_stored(input: &[u8]) -> Vec<u8> {
    megu_core::zip_archive::zlib_compress_stored(input)
}

/// Decode an RFC 1950 zlib stream with an explicit uncompressed byte budget.
pub fn zlib_decompress(input: &[u8], budget: usize) -> Result<Vec<u8>, String> {
    megu_core::zip_archive::zlib_decompress_capped(input, budget).map_err(|e| e.to_string())
}


// ---------------------------------------------------------------------------
// Packet framing (uncompressed and compressed)
// ---------------------------------------------------------------------------

pub fn encode_packet(pid: i32, payload: &[u8], compression: Option<usize>) -> Vec<u8> {
    let mut raw = Vec::with_capacity(5 + payload.len());
    write_varint(&mut raw, pid);
    raw.extend_from_slice(payload);

    let mut frame = Vec::new();
    match compression {
        None => {
            write_varint(&mut frame, raw.len() as i32);
            frame.extend_from_slice(&raw);
        }
        Some(threshold) => {
            let mut body = Vec::new();
            if raw.len() < threshold {
                write_varint(&mut body, 0);
                body.extend_from_slice(&raw);
            } else {
                write_varint(&mut body, raw.len() as i32);
                body.extend_from_slice(&zlib_compress_stored(&raw));
            }
            write_varint(&mut frame, body.len() as i32);
            frame.extend_from_slice(&body);
        }
    }
    frame
}

pub fn decode_packet_body(
    body: &[u8],
    compression: Option<usize>,
) -> Result<(i32, Vec<u8>), String> {
    let uncompressed = match compression {
        None => body.to_vec(),
        Some(_threshold) => {
            let mut r = WireReader::new(body);
            let data_len = usize::try_from(r.read_varint()?)
                .map_err(|_| "negative compressed data length".to_string())?;
            if data_len == 0 {
                if r.remaining() > MAX_PACKET_SIZE {
                    return Err("uncompressed frame exceeds max packet size".to_string());
                }
                r.rest().to_vec()
            } else {
                if data_len > MAX_PACKET_SIZE {
                    return Err("compressed packet exceeds max packet size".to_string());
                }
                let out = zlib_decompress(r.rest(), data_len)?;
                if out.len() != data_len {
                    return Err(format!(
                        "zlib decoded length {} does not match header {}",
                        out.len(),
                        data_len
                    ));
                }
                out
            }
        }
    };
    let mut r = WireReader::new(&uncompressed);
    let pid = r.read_varint()?;
    let payload = r.rest().to_vec();
    Ok((pid, payload))
}

// ---------------------------------------------------------------------------
// Paletted container & Chunk / Light decoders
// ---------------------------------------------------------------------------

pub fn decode_paletted_container(
    reader: &mut WireReader<'_>,
    entry_count: usize,
    indirect_max: u8,
    min_bits: u8,
    direct_bits: u8,
) -> Result<Vec<u32>, String> {
    let mut out = vec![0; entry_count];
    decode_paletted_container_into(
        reader,
        entry_count,
        indirect_max,
        min_bits,
        direct_bits,
        &mut out,
    )?;
    Ok(out)
}

fn decode_paletted_container_into(
    reader: &mut WireReader<'_>,
    entry_count: usize,
    indirect_max: u8,
    min_bits: u8,
    direct_bits: u8,
    out: &mut [u32],
) -> Result<(), String> {
    if out.len() != entry_count {
        return Err(format!("palette output length {} != {entry_count}", out.len()));
    }
    let bits = reader.read_u8()?;
    if bits == 0 {
        let single = u32::try_from(reader.read_varint()?)
            .map_err(|_| "negative singleton palette id".to_string())?;
        let data_len = usize::try_from(reader.read_varint()?)
            .map_err(|_| "negative singleton long count".to_string())?;
        if data_len != 0 {
            let _ = reader.take(data_len * 8)?;
        }
        out.fill(single);
        return Ok(());
    }

    let palette = if bits <= indirect_max {
        if bits < min_bits {
            return Err(format!("palette bits {bits} below minimum {min_bits}"));
        }
        let plen = usize::try_from(reader.read_varint()?)
            .map_err(|_| "negative palette length".to_string())?;
        // Every palette entry is a VarInt of at least one byte, so a length
        // past the remaining input can never be satisfied — reject before
        // `with_capacity` turns a hostile count into a huge allocation.
        if plen > reader.remaining() {
            return Err(format!(
                "palette length {plen} exceeds {} remaining bytes",
                reader.remaining()
            ));
        }
        let mut values = Vec::with_capacity(plen);
        for _ in 0..plen {
            values.push(
                u32::try_from(reader.read_varint()?)
                    .map_err(|_| "negative palette entry".to_string())?,
            );
        }
        Some(values)
    } else {
        if bits != direct_bits && bits > 32 {
            return Err(format!("invalid direct palette width {bits}"));
        }
        None
    };

    let data_len = usize::try_from(reader.read_varint()?)
        .map_err(|_| "negative palette data long count".to_string())?;
    let per_word = 64 / usize::from(bits);
    let expected_len = entry_count.div_ceil(per_word);
    if data_len != expected_len {
        return Err(format!(
            "palette long count {data_len} != expected {expected_len} for {bits} bits"
        ));
    }

    let mask = (1u64 << bits) - 1;
    let mut words = Vec::with_capacity(data_len);
    for _ in 0..data_len {
        words.push(reader.read_u64()?);
    }

    for i in 0..entry_count {
        let word = words[i / per_word];
        let shift = (i % per_word) * usize::from(bits);
        let raw = ((word >> shift) & mask) as u32;
        let state = match &palette {
            Some(pal) => *pal
                .get(raw as usize)
                .ok_or_else(|| format!("palette index {raw} out of range {}", pal.len()))?,
            None => raw,
        };
        out[i] = state;
    }
    Ok(())
}

#[derive(Clone, Debug)]
pub struct DecodedSection {
    pub non_air: i16,
    pub states: Box<[u32; 4096]>,
    pub biomes: [u32; 64],
}

#[derive(Clone, Debug)]
pub struct DecodedLightData {
    pub sky_mask: Vec<u64>,
    pub block_mask: Vec<u64>,
    pub empty_sky_mask: Vec<u64>,
    pub empty_block_mask: Vec<u64>,
    pub sky_arrays: Vec<[u8; 2048]>,
    pub block_arrays: Vec<[u8; 2048]>,
}

/// One block-entity record from a `level_chunk_with_light` packet: the
/// packed local X/Z byte, section-relative Y, block-entity type id, and
/// the raw anonymous-NBT payload exactly as it arrived on the wire.
#[derive(Clone, Debug)]
pub struct DecodedBlockEntity {
    pub packed_xz: u8,
    pub y: i16,
    pub kind: i32,
    pub nbt_bytes: Vec<u8>,
}

#[derive(Clone, Debug)]
pub struct DecodedChunk {
    pub cx: i32,
    pub cz: i32,
    pub sections: Vec<DecodedSection>,
    pub light: DecodedLightData,
    /// Block entities embedded in the chunk payload, in wire order.
    pub block_entities: Vec<DecodedBlockEntity>,
}

fn decode_light_section_payload(
    reader: &mut WireReader<'_>,
    light_section_count: usize,
    byte_array_masks: bool,
) -> Result<DecodedLightData, String> {
    let read_mask = |r: &mut WireReader<'_>| -> Result<Vec<u64>, String> {
        if byte_array_masks {
            r.read_bitset_bytes(light_section_count.div_ceil(8))
        } else {
            r.read_bitset(light_section_count.div_ceil(64))
        }
    };
    let sky_mask = read_mask(reader)?;
    let block_mask = read_mask(reader)?;
    let empty_sky_mask = read_mask(reader)?;
    let empty_block_mask = read_mask(reader)?;

    let sky_count = usize::try_from(reader.read_varint()?)
        .map_err(|_| "negative sky light array count".to_string())?;
    if sky_count != bitset_count_ones(&sky_mask) {
        return Err(format!(
            "sky light array count {sky_count} != mask popcount {}",
            bitset_count_ones(&sky_mask)
        ));
    }
    let mut sky_arrays = Vec::with_capacity(sky_count);
    for _ in 0..sky_count {
        let len = reader.read_varint()?;
        if len != 2048 {
            return Err(format!("expected 2048-byte sky light array, got {len}"));
        }
        let mut arr = [0u8; 2048];
        arr.copy_from_slice(reader.take(2048)?);
        sky_arrays.push(arr);
    }

    let block_count = usize::try_from(reader.read_varint()?)
        .map_err(|_| "negative block light array count".to_string())?;
    if block_count != bitset_count_ones(&block_mask) {
        return Err(format!(
            "block light array count {block_count} != mask popcount {}",
            bitset_count_ones(&block_mask)
        ));
    }
    let mut block_arrays = Vec::with_capacity(block_count);
    for _ in 0..block_count {
        let len = reader.read_varint()?;
        if len != 2048 {
            return Err(format!("expected 2048-byte block light array, got {len}"));
        }
        let mut arr = [0u8; 2048];
        arr.copy_from_slice(reader.take(2048)?);
        block_arrays.push(arr);
    }

    Ok(DecodedLightData {
        sky_mask,
        block_mask,
        empty_sky_mask,
        empty_block_mask,
        sky_arrays,
        block_arrays,
    })
}

/// Version-sensitive knobs for `level_chunk_with_light` decoding: the
/// block-state bound a version pack declares (`registry.state_count()`),
/// whether the heightmaps field is the 1.21.5+ array-of-`Heightmap`-records
/// form (`false` is the 767 single-NBT-compound form), and whether the four
/// light masks are the 1.21.5+ `java.util.BitSet` byte-array form (`false`
/// is the 767 `VarInt` + `u64` word form).
#[derive(Clone, Copy, Debug)]
pub struct ChunkDecodeProfile {
    pub state_count: usize,
    pub heightmaps_are_records: bool,
    pub byte_array_light_masks: bool,
}

impl ChunkDecodeProfile {
    /// The pinned 1.21.1 profile: kernel `STATE_COUNT` + NBT heightmaps +
    /// long-array light masks.
    pub fn v767() -> Self {
        Self {
            state_count: megu_kernel::STATE_COUNT as usize,
            heightmaps_are_records: false,
            byte_array_light_masks: false,
        }
    }
}

pub fn decode_chunk_packet(
    body: &[u8],
    section_count: usize,
    biome_count: usize,
) -> Result<DecodedChunk, String> {
    decode_chunk_packet_profiled(body, section_count, biome_count, ChunkDecodeProfile::v767())
}

pub fn decode_chunk_packet_profiled(
    body: &[u8],
    section_count: usize,
    biome_count: usize,
    profile: ChunkDecodeProfile,
) -> Result<DecodedChunk, String> {
    if section_count > MAX_WORLD_SECTIONS {
        return Err(format!(
            "section count {section_count} exceeds bound {MAX_WORLD_SECTIONS}"
        ));
    }
    let mut reader = WireReader::new(body);
    let cx = reader.read_i32()?;
    let cz = reader.read_i32()?;
    if profile.heightmaps_are_records {
        // 1.21.5+ form: VarInt count of {type VarInt, packed long array}.
        let count = usize::try_from(reader.read_varint()?)
            .map_err(|_| "negative heightmap count".to_string())?;
        for _ in 0..count {
            let _hm_type = reader.read_varint()?;
            let longs = usize::try_from(reader.read_varint()?)
                .map_err(|_| "negative heightmap long count".to_string())?;
            let longs_bytes = longs
                .checked_mul(8)
                .ok_or_else(|| "heightmap long count overflow".to_string())?;
            let _ = reader.take(longs_bytes)?;
        }
    } else {
        let _heightmaps = reader.read_nbt()?;
    }

    let data_len = usize::try_from(reader.read_varint()?)
        .map_err(|_| "negative chunk data length".to_string())?;
    let section_bytes = reader.take(data_len)?;
    let min_section_bytes = section_count
        .checked_mul(MIN_WIRE_SECTION_BYTES)
        .ok_or_else(|| "chunk section count overflow".to_string())?;
    if data_len < min_section_bytes {
        return Err(format!(
            "chunk section data length {data_len} is below minimum {min_section_bytes} for {section_count} sections"
        ));
    }
    let mut sec_reader = WireReader::new(section_bytes);

    let biome_direct_bits = if biome_count <= 1 {
        0
    } else {
        (usize::BITS - (biome_count - 1).leading_zeros()) as u8
    };
    let state_bound = u32::try_from(profile.state_count.saturating_sub(1)).unwrap_or(u32::MAX);
    let state_direct_bits = (u32::BITS - state_bound.leading_zeros()) as u8;

    let mut sections = Vec::with_capacity(section_count);
    for _ in 0..section_count {
        let non_air = sec_reader.read_i16()?;
        let mut states = Box::new([0u32; 4096]);
        let mut biomes = [0u32; 64];
        decode_paletted_container_into(
            &mut sec_reader,
            states.len(),
            8,
            4,
            state_direct_bits,
            &mut states[..],
        )?;
        decode_paletted_container_into(
            &mut sec_reader,
            biomes.len(),
            3,
            1,
            biome_direct_bits,
            &mut biomes,
        )?;
        sections.push(DecodedSection {
            non_air,
            states,
            biomes,
        });
    }
    if sec_reader.remaining() != 0 {
        return Err(format!(
            "unparsed {} bytes in chunk section payload",
            sec_reader.remaining()
        ));
    }

    let block_entity_count = usize::try_from(reader.read_varint()?)
        .map_err(|_| "negative block entity count".to_string())?;
    // Each record is at least 4 bytes (xz + y + kind varint), so a count past
    // the remaining input can never be satisfied.
    if block_entity_count > reader.remaining() {
        return Err(format!(
            "block entity count {block_entity_count} exceeds {} remaining bytes",
            reader.remaining()
        ));
    }
    let mut block_entities = Vec::with_capacity(block_entity_count);
    for _ in 0..block_entity_count {
        let packed_xz = reader.read_u8()?;
        let y = reader.read_i16()?;
        let kind = reader.read_varint()?;
        let nbt_bytes = read_nbt_raw(&mut reader)?.to_vec();
        block_entities.push(DecodedBlockEntity {
            packed_xz,
            y,
            kind,
            nbt_bytes,
        });
    }

    let light = decode_light_section_payload(
        &mut reader,
        section_count + 2,
        profile.byte_array_light_masks,
    )?;
    Ok(DecodedChunk {
        cx,
        cz,
        sections,
        light,
        block_entities,
    })
}

pub fn decode_update_light_packet(
    body: &[u8],
    section_count: usize,
    byte_array_masks: bool,
) -> Result<(i32, i32, DecodedLightData), String> {
    if section_count > MAX_WORLD_SECTIONS {
        return Err(format!(
            "section count {section_count} exceeds bound {MAX_WORLD_SECTIONS}"
        ));
    }
    let mut reader = WireReader::new(body);
    let cx = reader.read_varint()?;
    let cz = reader.read_varint()?;
    let light = decode_light_section_payload(&mut reader, section_count + 2, byte_array_masks)?;
    Ok((cx, cz, light))
}

pub fn decode_section_blocks_update(body: &[u8]) -> Result<Vec<([i32; 3], u32)>, String> {
    let mut reader = WireReader::new(body);
    let section_pos = reader.read_i64()?;
    let sx = (section_pos >> 42) as i32;
    let sz = ((section_pos << 22) >> 42) as i32;
    let sy = ((section_pos << 44) >> 44) as i32;

    let count = usize::try_from(reader.read_varint()?)
        .map_err(|_| "negative section update count".to_string())?;
    // Each update record is a VarLong of at least one byte, so a count past
    // the remaining input can never be satisfied.
    if count > reader.remaining() {
        return Err(format!(
            "section update count {count} exceeds {} remaining bytes",
            reader.remaining()
        ));
    }
    let mut updates = Vec::with_capacity(count);
    for _ in 0..count {
        let record = reader.read_varlong()? as u64;
        let state = (record >> 12) as u32;
        let lx = ((record >> 8) & 0x0f) as i32;
        let lz = ((record >> 4) & 0x0f) as i32;
        let ly = (record & 0x0f) as i32;
        updates.push(([sx * 16 + lx, sy * 16 + ly, sz * 16 + lz], state));
    }
    Ok(updates)
}

// ---------------------------------------------------------------------------
// Dimension metadata & Play events
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DimensionMeta {
    pub name: String,
    pub min_y: i32,
    pub height: i32,
    pub has_skylight: bool,
}

impl Default for DimensionMeta {
    fn default() -> Self {
        Self {
            name: "minecraft:overworld".to_string(),
            min_y: -64,
            height: 384,
            has_skylight: true,
        }
    }
}

impl DimensionMeta {
    pub fn section_count(&self) -> usize {
        (self.height / 16).max(1) as usize
    }

    pub fn min_section_y(&self) -> i32 {
        self.min_y.div_euclid(16)
    }

    fn builtin(name: &str) -> Option<(i32, i32, bool)> {
        match name {
            "minecraft:overworld" | "minecraft:overworld_caves" => Some((-64, 384, true)),
            "minecraft:the_nether" | "minecraft:the_end" => Some((0, 256, false)),
            _ => None,
        }
    }
}

#[derive(Clone, Debug)]
pub struct TeleportSync {
    pub x: f64,
    pub y: f64,
    pub z: f64,
    pub yaw: f32,
    pub pitch: f32,
    /// Relative-flag bitmask — a `u8` on the 767 wire but a full `Int` on
    /// 26.x (which adds `DELTA_*`/`ROTATE_DELTA` bits above `0x10`).
    pub flags: i32,
    pub teleport_id: i32,
}

/// One decoded `sound_effect` (0x68) or `sound_entity` (0x67) payload —
/// the typed wire event [`crate::audio_out::AudioRuntime`] ingests.
#[derive(Clone, Debug)]
pub struct WireSoundEvent {
    /// Inline `sound_event` identifier (holder id `0`), e.g.
    /// `minecraft:entity.player.levelup`.
    pub name: Option<String>,
    /// `minecraft:sound_event` registry id (holder id `>0`, minus one).
    pub registry_id: Option<u32>,
    /// Vanilla sound-source category (0=master .. 9=voice).
    pub category: i32,
    /// World position in blocks — fixed-point /8 decoded (`sound_effect`
    /// only).
    pub position: Option<[f32; 3]>,
    /// Attached entity id (`sound_entity` only).
    pub entity_id: Option<i32>,
    /// Wire volume (0..=1 typical; the server may send larger).
    pub volume: f32,
    /// Wire pitch multiplier (0.5..=2.0 per vanilla).
    pub pitch: f32,
    /// Wire seed — used for variant selection like vanilla's seeded pick.
    pub seed: i64,
    /// Optional fixed audible range on inline events (blocks).
    pub fixed_range: Option<f32>,
}

/// One decoded `stop_sound` (0x6A) payload: flags byte, optional category
/// varint, optional sound-event identifier.
#[derive(Clone, Debug)]
pub struct WireStopSound {
    /// Sound-source category filter when flag 0x01 is set.
    pub category: Option<i32>,
    /// Event-name filter when flag 0x02 is set.
    pub name: Option<String>,
}

/// One decoded `system_chat` payload retained bounded for consumers that
/// need the raw NBT component (styled text, click events, translatable
/// keys). `PlayEvent::SystemChat` carries only the extracted plain text —
/// its field shape is frozen by downstream consumers — so the raw bytes
/// ride this side channel exactly like [`WireSoundEvent`] does for audio.
#[derive(Clone, Debug)]
pub struct WireChatEvent {
    /// Anonymous-NBT component bytes exactly as received on the wire.
    pub nbt_bytes: Vec<u8>,
    /// Action-bar (overlay) flag from the packet trailer.
    pub overlay: bool,
}

/// Decode `sound_effect`/`sound_entity` payloads (`entity_bound` selects
/// entity-id vs fixed-point-position layout) on the 767 wire.
pub fn decode_play_sound_body(body: &[u8], entity_bound: bool) -> Result<WireSoundEvent, String> {
    let mut r = WireReader::new(body);
    let holder = r.read_varint()?;
    let (name, registry_id, fixed_range) = if holder == 0 {
        let name = r.read_string(256)?;
        let has_range = r.read_bool()?;
        let range = if has_range { Some(r.read_f32()?) } else { None };
        (Some(name), None, range)
    } else {
        let id =
            u32::try_from(holder - 1).map_err(|_| "invalid sound_event registry id".to_string())?;
        (None, Some(id), None)
    };
    let category = r.read_varint()?;
    let (position, entity_id) = if entity_bound {
        (None, Some(r.read_varint()?))
    } else {
        // Effect positions are fixed-point Ints: real = raw / 8.
        let x = r.read_i32()?;
        let y = r.read_i32()?;
        let z = r.read_i32()?;
        (Some([x as f32 / 8.0, y as f32 / 8.0, z as f32 / 8.0]), None)
    };
    Ok(WireSoundEvent {
        name,
        registry_id,
        category,
        position,
        entity_id,
        volume: r.read_f32()?,
        pitch: r.read_f32()?,
        seed: r.read_i64()?,
        fixed_range,
    })
}

/// Decode a `stop_sound` payload on the 767 wire: flags byte (0x01 category
/// present, 0x02 name present), optional category varint, optional
/// sound-event identifier.
pub fn decode_stop_sound_body(body: &[u8]) -> Result<WireStopSound, String> {
    let mut r = WireReader::new(body);
    let flags = r.read_u8()?;
    let category = if flags & 0x01 != 0 {
        Some(r.read_varint()?)
    } else {
        None
    };
    let name = if flags & 0x02 != 0 {
        Some(r.read_string(256)?)
    } else {
        None
    };
    Ok(WireStopSound { category, name })
}

#[derive(Clone, Debug)]
pub enum PlayEvent {
    JoinGame {
        entity_id: i32,
        dimension: DimensionMeta,
        /// `login.is_hardcore` — the death screen switches to the
        /// hardcore "Game over!" / spectate-only variant on this.
        hardcore: bool,
    },
    Respawn {
        dimension: DimensionMeta,
    },
    KeepAlive(i64),
    Teleport(TeleportSync),
    ChunkBatchStart,
    ChunkData(Box<DecodedChunk>),
    ChunkBatchFinished(i32),
    UpdateLight {
        cx: i32,
        cz: i32,
        light: DecodedLightData,
    },
    BlockUpdate {
        pos: [i32; 3],
        state_id: u32,
    },
    SectionBlocksUpdate(Vec<([i32; 3], u32)>),
    UnloadChunk {
        cx: i32,
        cz: i32,
    },
    SetCenterChunk {
        cx: i32,
        cz: i32,
    },
    SetHealth {
        health: f32,
        food: i32,
        saturation: f32,
    },
    /// `system_chat` — extracted plain text plus the overlay (action-bar)
    /// flag; the NBT component itself stays in `protocol`'s decoder.
    SystemChat {
        text: String,
        overlay: bool,
    },
    // -- Entities ---------------------------------------------------------
    /// `add_entity` — a non-player entity spawn.
    AddEntity {
        eid: i32,
        uuid: [u8; 16],
        entity_type: i32,
        x: f64,
        y: f64,
        z: f64,
        /// Head yaw in degrees (wire angle byte decoded as `byte * 360/256`).
        pitch: f32,
        yaw: f32,
        head_yaw: f32,
        /// `objectData` — entity-type-specific spawn data (e.g. falling-block
        /// state id, area-effect cloud radius).
        data: i32,
        /// Velocity in blocks/tick (wire i16 fixed-point / 8000).
        vx: f64,
        vy: f64,
        vz: f64,
    },
    /// `remove_entities` — entity ids destroyed this tick.
    RemoveEntities {
        eids: Vec<i32>,
    },
    /// `move_entity_pos` — relative position delta in blocks (wire i16 / 4096).
    MoveEntityPos {
        eid: i32,
        dx: f64,
        dy: f64,
        dz: f64,
        on_ground: bool,
    },
    /// `move_entity_pos_rot` — relative move plus yaw/pitch (degrees).
    MoveEntityPosRot {
        eid: i32,
        dx: f64,
        dy: f64,
        dz: f64,
        yaw: f32,
        pitch: f32,
        on_ground: bool,
    },
    /// `move_entity_rot` — yaw/pitch only (degrees).
    MoveEntityRot {
        eid: i32,
        yaw: f32,
        pitch: f32,
        on_ground: bool,
    },
    /// `teleport_entity` — absolute entity teleport.
    TeleportEntity {
        eid: i32,
        x: f64,
        y: f64,
        z: f64,
        yaw: f32,
        pitch: f32,
        on_ground: bool,
    },
    /// `rotate_head` — entity head-yaw rotation (degrees).
    RotateHead {
        eid: i32,
        head_yaw: f32,
    },
    /// `set_entity_motion` — entity velocity (wire i16 / 8000).
    SetEntityMotion {
        eid: i32,
        vx: f64,
        vy: f64,
        vz: f64,
    },
    /// `entity_event` — entity status byte (hurt flash, death, totem, ...).
    EntityEvent {
        eid: i32,
        status: i8,
    },
    /// `hurt_animation` — red hurt-flash yaw for the entity.
    HurtAnimation {
        eid: i32,
        yaw: f32,
    },
    /// `damage_event` — damage source ids plus an optional source position.
    DamageEvent {
        eid: i32,
        source_type: i32,
        source_cause: i32,
        source_direct: i32,
        source_position: Option<[f64; 3]>,
    },
    /// `set_equipment` — equipment slot entries; each `item_wire` is the raw
    /// `Slot` payload exactly as it arrived (bounded by the slot skipper).
    SetEquipment {
        eid: i32,
        entries: Vec<EquipmentEntry>,
    },
    /// `set_entity_data` — entity metadata entries kept as raw wire bytes
    /// (index/type/value records terminated by `0xFF`).
    SetEntityData {
        eid: i32,
        raw_entries: Vec<u8>,
    },
    // -- World / time / weather / particles / block entities ----------------
    /// `set_time` — world age and time-of-day (both i64 game ticks).
    SetTime {
        world_age: i64,
        time_of_day: i64,
    },
    /// `game_event` — game-state change (weather, game mode, win, ...).
    GameEvent {
        event: u8,
        value: f32,
    },
    /// `level_event` — world effect at a position (particles, sounds, ...).
    LevelEvent {
        event: i32,
        pos: [i32; 3],
        data: i32,
        disable_relative_volume: bool,
    },
    /// `level_particles` — particle spawn; `particle_data` is the raw
    /// particle-type-specific payload following the type id.
    LevelParticles {
        particle_id: i32,
        long_distance: bool,
        x: f64,
        y: f64,
        z: f64,
        offset_x: f32,
        offset_y: f32,
        offset_z: f32,
        max_speed: f32,
        count: i32,
        particle_data: Vec<u8>,
    },
    /// `block_destruction` — block-break stage animation.
    BlockDestruction {
        breaker_eid: i32,
        pos: [i32; 3],
        stage: i8,
    },
    /// `explode` — blast effect: centre, per-block destruction records,
    /// local-player knockback, interaction kind, emitter particle ids and
    /// the explosion sound-event holder.
    Explosion {
        x: f64,
        y: f64,
        z: f64,
        /// Blast strength (`radius`).
        power: f32,
        /// `(dx, dy, dz)` byte offsets of destroyed blocks, relative to
        /// `floor(x, y, z)` — capped by the packet bounds and
        /// [`MAX_EXPLOSION_RECORDS`].
        records: Vec<[i8; 3]>,
        /// Knockback delta applied to the local player's motion.
        knockback: [f32; 3],
        /// `block_interaction` enum ordinal (0 keep, 1 destroy,
        /// 2 destroy-with-decay, 3 trigger).
        block_interaction: i32,
        /// Small explosion emitter `ParticleOptions` id (payload-free
        /// kinds only — see the `explode` decoder).
        small_particle_id: i32,
        /// Large explosion emitter `ParticleOptions` id.
        large_particle_id: i32,
        /// Explosion sound event: inline holder name or `sound_event`
        /// registry id.
        sound_name: Option<String>,
        sound_id: Option<u32>,
    },
    /// `block_entity_data` — block-entity NBT update; `nbt_bytes` is empty
    /// when the packet carries the absent-NBT marker (`0x00`).
    BlockEntityData {
        pos: [i32; 3],
        kind: i32,
        nbt_bytes: Vec<u8>,
    },
    // -- Containers / menus -------------------------------------------------
    /// `open_screen` — menu open; `title_nbt` is the raw anonymous-NBT title.
    OpenScreen {
        window_id: i32,
        menu_type: i32,
        title_nbt: Vec<u8>,
    },
    /// `container_set_content` — full container contents. `slot_count` is the
    /// item-array count; `raw_payload` holds the remaining slot bytes
    /// (items plus the carried item) exactly as received.
    ContainerSetContent {
        window_id: u8,
        state_id: i32,
        slot_count: i32,
        raw_payload: Vec<u8>,
    },
    /// `container_set_slot` — single slot update; `raw_item` is the raw
    /// `Slot` wire bytes.
    ContainerSetSlot {
        window_id: u8,
        state_id: i32,
        slot: i16,
        raw_item: Vec<u8>,
    },
    /// `container_set_data` — container property update (progress, fuel, ...).
    ContainerSetData {
        window_id: u8,
        property: i16,
        value: i16,
    },
    /// `container_close` — server-initiated container close.
    ContainerClose {
        window_id: u8,
    },
    // -- HUD / player / chat ------------------------------------------------
    /// `set_experience` — experience bar fill, level, and total.
    SetExperience {
        bar: f32,
        level: i32,
        total: i32,
    },
    /// `update_mob_effect` — mob-effect add/refresh.
    UpdateMobEffect {
        eid: i32,
        effect_id: i32,
        amplifier: i32,
        duration: i32,
        flags: u8,
    },
    /// `remove_mob_effect` — mob-effect removal.
    RemoveMobEffect {
        eid: i32,
        effect_id: i32,
    },
    /// `set_action_bar_text` — action-bar text as raw anonymous-NBT bytes.
    SetActionBarText {
        nbt_bytes: Vec<u8>,
    },
    /// `set_title_text` — title text as raw anonymous-NBT bytes.
    SetTitleText {
        nbt_bytes: Vec<u8>,
    },
    /// `set_subtitle_text` — subtitle text as raw anonymous-NBT bytes.
    SetSubtitleText {
        nbt_bytes: Vec<u8>,
    },
    /// `boss_event` — boss-bar action; `raw_payload` holds the
    /// action-dependent fields (title/health/color/dividers/flags) raw.
    BossEvent {
        uuid: [u8; 16],
        action: i32,
        raw_payload: Vec<u8>,
    },
    /// `player_info_remove` — player-info entries removed by UUID.
    PlayerInfoRemove {
        uuids: Vec<[u8; 16]>,
    },
    /// `player_info_update` — tab-list entry mutations; `actions` is the
    /// wire bitmask (0x01 add, 0x02 init-chat, 0x04 gamemode, 0x08 listed,
    /// 0x10 latency, 0x20 display-name) shared by every entry.
    PlayerInfo {
        actions: u8,
        entries: Vec<PlayerInfoUpdateEntry>,
    },
    /// `set_objective` — scoreboard objective create/remove/update; the
    /// display fields are `Some` only for create/update (`mode` 0 or 2).
    SetObjective {
        objective: String,
        mode: i8,
        display_name_nbt: Option<Vec<u8>>,
        render_type: Option<i32>,
        number_format_kind: Option<i32>,
        number_format_nbt: Option<Vec<u8>>,
    },
    /// `set_score` — one score line under an objective.
    SetScore {
        owner: String,
        objective: String,
        value: i32,
        display_nbt: Option<Vec<u8>>,
        number_format_kind: Option<i32>,
    },
    /// `set_display_objective` — pins an objective to a sidebar slot
    /// (0 = sidebar, 3..=18 = per-team colors, 1 = below-name, 2 = list).
    SetDisplayObjective {
        slot: i32,
        objective: String,
    },
    /// `reset_score` — clears one owner's lines, optionally scoped to a
    /// single objective.
    ResetScore {
        owner: String,
        objective: Option<String>,
    },
    /// `set_titles_animation` — title timing in ticks (vanilla
    /// `ClientboundSetTitlesAnimationPacket`).
    SetTitle {
        fade_in: i32,
        stay: i32,
        fade_out: i32,
    },
    /// `transfer` — server instructs the client to reconnect to another
    /// `host`/`port` pair (1.20.5+ cross-server handoff).
    Transfer {
        host: String,
        port: i32,
    },
    // -- Play coverage expansion (v1211 lane) --------------------------------
    /// `pong_response` — echoes the client's play `ping_request` i64 payload.
    PongResponse {
        id: i64,
    },
    /// `initialize_border` — full world-border state pushed on join/respawn.
    InitializeBorder {
        x: f64,
        z: f64,
        old_diameter: f64,
        new_diameter: f64,
        /// Lerp time in real-time milliseconds (`VarLong`; 0 when static).
        lerp_ms: i64,
        portal_boundary: i32,
        warning_blocks: i32,
        warning_time: i32,
    },
    /// `set_border_center` — world-border centre move.
    SetBorderCenter {
        x: f64,
        z: f64,
    },
    /// `set_border_size` — instant world-border diameter.
    SetBorderSize {
        diameter: f64,
    },
    /// `set_border_lerp_size` — animated diameter change (VarLong ms).
    SetBorderLerpSize {
        old_diameter: f64,
        new_diameter: f64,
        lerp_ms: i64,
    },
    SetBorderWarningDelay {
        warning_time: i32,
    },
    SetBorderWarningDistance {
        warning_blocks: i32,
    },
    Animate {
        eid: i32,
        animation: u8,
    },
    BlockEvent {
        pos: [i32; 3],
        action: u8,
        parameter: u8,
        block_type: i32,
    },
    SetDefaultSpawnPosition {
        pos: [i32; 3],
        angle: f32,
    },
    SetChunkCacheRadius(i32),
    SetSimulationDistance(i32),
    TickingState {
        tick_rate: f32,
        frozen: bool,
    },
    TickingStep {
        steps: i32,
    },
    MoveVehicle {
        x: f64,
        y: f64,
        z: f64,
        yaw: f32,
        pitch: f32,
    },
    SetPassengers {
        vehicle: i32,
        passengers: Vec<i32>,
    },
    SetEntityLink {
        source: i32,
        destination: i32,
    },
    TakeItemEntity {
        item_entity: i32,
        player: i32,
        amount: i32,
    },
    UpdateAttributes {
        eid: i32,
        attributes: Vec<WireAttribute>,
    },
    SetCamera {
        eid: i32,
    },
    OpenBook {
        hand: u8,
    },
    OpenSignEditor {
        pos: [i32; 3],
        front_side: bool,
    },
    TabList {
        header_nbt: Vec<u8>,
        footer_nbt: Vec<u8>,
    },
    CommandSuggestions {
        transaction_id: i32,
        start: i32,
        length: i32,
        matches: Vec<WireSuggestion>,
    },
    PlayerChat {
        sender: [u8; 16],
        index: i32,
        signature: Option<[u8; 256]>,
        text: String,
        nbt_bytes: Vec<u8>,
    },
    DisguisedChat {
        text: String,
        nbt_bytes: Vec<u8>,
    },
    ChatCompletions {
        action: i32,
        entries: Vec<String>,
    },
    Ping(i32),
    CookieRequest(String),
    PlaceGhostRecipe {
        window_id: u8,
        recipe: String,
    },
    TagQuery {
        transaction_id: i32,
        nbt_bytes: Vec<u8>,
    },
    SetProjectilePower {
        eid: i32,
        acceleration: f64,
    },
    CustomReportDetails(Vec<(String, String)>),
    ServerLinks(Vec<WireServerLink>),
    BlockChangedAck(i32),
    ChangeDifficulty { difficulty: u8, locked: bool },
    ClearTitles,
    Cooldown { item: i32, ticks: i32 },
    PlayerAbilities { flags: u8, flying_speed: f32, walking_speed: f32 },
    PlayerCombatEnd { duration: i32 },
    PlayerCombatEnter,
    PlayerCombatKill { player_id: i32, message_nbt: Vec<u8> },
    ResourcePackPop { uuid: Option<[u8; 16]> },
    ResourcePackPush { uuid: [u8; 16], url: String, hash: String, forced: bool, prompt_nbt: Option<Vec<u8>> },
    SetCarriedItem(i16),
    StoreCookie { key: String, payload: Vec<u8> },
    ResetChat,
    DeleteChat {
        message_index: Option<i32>,
        signature: Option<[u8; 256]>,
    },
    /// `set_player_team` — scoreboard-team create/remove/update/membership.
    SetPlayerTeam {
        name: String,
        /// 0 create, 1 remove, 2 update info, 3 add entities, 4 remove
        /// entities; `info` is `Some` only for modes 0 and 2.
        mode: u8,
        info: Option<Box<PlayerTeamInfo>>,
        entities: Vec<String>,
    },
    /// `merchant_offers` — villager trade list for the open merchant window.
    MerchantOffers {
        window_id: i32,
        offers: Vec<MerchantOfferEntry>,
        villager_level: i32,
        villager_xp: i32,
        show_progress: bool,
        can_restock: bool,
    },
    /// `horse_screen_open` — horse inventory screen request.
    HorseScreenOpen {
        window_id: u8,
        slots: i32,
        entity_id: i32,
    },
    /// `map_item_data` — map-item patch; `colors` is the `columns * rows`
    /// pixel block when `columns > 0` (empty otherwise).
    MapItemData {
        map_id: i32,
        scale: i8,
        locked: bool,
        icons: Option<Vec<MapItemIcon>>,
        columns: u8,
        rows: u8,
        x: u8,
        y: u8,
        colors: Vec<u8>,
    },
    /// `player_look_at` — rotate the client player toward a point or entity.
    PlayerLookAt {
        /// 0 = feet, 1 = eyes.
        anchor: i32,
        target: [f64; 3],
        /// `(entity_id, entity_anchor)` when `is_entity` was set.
        entity: Option<(i32, i32)>,
    },
    /// `update_advancements` — full advancement tree + progress sync.
    UpdateAdvancements {
        reset: bool,
        advancements: Vec<WireAdvancement>,
        removed: Vec<String>,
        progress: Vec<WireAdvancementProgress>,
    },
    /// `select_advancements_tab` — switch the advancements screen tab.
    SelectAdvancementsTab {
        tab: Option<String>,
    },
    /// `award_stats` — statistic counter updates (stat-request reply).
    AwardStats {
        entries: Vec<AwardStatEntry>,
    },
    /// `server_data` — MOTD component plus optional PNG favicon bytes.
    ServerData {
        motd_nbt: Vec<u8>,
        icon: Option<Vec<u8>>,
    },
    /// `chunks_biomes` — per-chunk-column biome payloads (opaque `data`
    /// kept raw; the biome paletted-container lives inside it).
    ChunksBiomes {
        chunks: Vec<ChunkBiomeData>,
    },
    Disconnect(String),
    Other(i32),
}

#[derive(Clone, Debug)]
pub struct WireAttribute {
    pub attribute: i32,
    pub base: f64,
    pub modifiers: Vec<WireAttributeModifier>,
}

#[derive(Clone, Debug)]
pub struct WireAttributeModifier {
    pub uuid: [u8; 16],
    pub amount: f64,
    pub operation: i32,
}

#[derive(Clone, Debug)]
pub struct WireSuggestion {
    pub text: String,
    pub tooltip_nbt: Option<Vec<u8>>,
}

#[derive(Clone, Debug)]
pub struct WireServerLink {
    pub label: Option<i32>,
    pub custom_label_nbt: Option<Vec<u8>>,
    pub url: String,
}

#[derive(Clone, Debug)]
pub struct ChatLogEntry {
    pub index: Option<i32>,
    pub signature: Option<[u8; 256]>,
    pub text: String,
}

/// Parameter block of `set_player_team` modes 0 (create) and 2 (update).
#[derive(Clone, Debug)]
pub struct PlayerTeamInfo {
    /// Display-name text component as raw anonymous-NBT bytes.
    pub display_name_nbt: Vec<u8>,
    /// Friendly-fire (0x01) / see-invisible-teammates (0x02) bitmask.
    pub friendly_flags: u8,
    /// `always`/`never`/`hide_for_other_teams`/`hide_for_own_team`.
    pub nametag_visibility: String,
    /// `always`/`never`/`push_other_teams`/`push_own_team`.
    pub collision_rule: String,
    /// Team colour/formatting id (-1 = none).
    pub color: i32,
    /// Prefix/suffix text components as raw anonymous-NBT bytes.
    pub prefix_nbt: Vec<u8>,
    pub suffix_nbt: Vec<u8>,
}

/// One merchant `ItemCost` (item id, count, added-component wire blob).
#[derive(Clone, Debug)]
pub struct MerchantItemCost {
    pub item: i64,
    pub count: i32,
    /// Raw bytes of the `addedComponentCount` varint plus every
    /// `SlotComponent` entry — kept verbatim for component-aware consumers.
    pub components_wire: Vec<u8>,
}

/// One `merchant_offers` trade entry.
#[derive(Clone, Debug)]
pub struct MerchantOfferEntry {
    pub cost_a: MerchantItemCost,
    /// Result stack as raw `Slot` wire bytes.
    pub result_slot_wire: Vec<u8>,
    pub cost_b: Option<MerchantItemCost>,
    pub disabled: bool,
    pub uses: i32,
    pub max_uses: i32,
    pub xp: i32,
    /// Special price adjustment (reputation discounts go negative).
    pub special_price: i32,
    pub price_multiplier: f32,
    pub demand: i32,
}

/// One `map_item_data` icon.
#[derive(Clone, Debug)]
pub struct MapItemIcon {
    pub kind: i32,
    pub x: i8,
    pub z: i8,
    pub direction: u8,
    /// Display-name text component as raw anonymous-NBT bytes.
    pub name_nbt: Option<Vec<u8>>,
}

/// Display block of one `update_advancements` entry.
#[derive(Clone, Debug)]
pub struct WireAdvancementDisplay {
    pub title_nbt: Vec<u8>,
    pub description_nbt: Vec<u8>,
    /// Icon `Slot` as raw wire bytes.
    pub icon_slot_wire: Vec<u8>,
    /// 0 = task, 1 = challenge, 2 = goal.
    pub frame_type: i32,
    /// 0x01 background texture, 0x02 show toast, 0x04 hidden.
    pub flags: i32,
    pub background: Option<String>,
    pub x: f32,
    pub y: f32,
}

/// One `update_advancements` tree entry.
#[derive(Clone, Debug)]
pub struct WireAdvancement {
    pub key: String,
    pub parent_id: Option<String>,
    pub display: Option<WireAdvancementDisplay>,
    /// OR-of-ANDs criterion lists.
    pub requirements: Vec<Vec<String>>,
    pub sends_telemetry: bool,
}

/// One `update_advancements` progress entry.
#[derive(Clone, Debug)]
pub struct WireAdvancementProgress {
    pub key: String,
    /// `(criterion_id, achieved_millis)` — `None` when not yet achieved.
    pub criteria: Vec<(String, Option<i64>)>,
}

/// One `award_stats` counter update.
#[derive(Clone, Debug)]
pub struct AwardStatEntry {
    pub category: i32,
    pub statistic: i32,
    pub value: i32,
}

/// One `chunks_biomes` column payload.
#[derive(Clone, Debug)]
pub struct ChunkBiomeData {
    pub cx: i32,
    pub cz: i32,
    /// Raw biome paletted-container bytes for the column.
    pub data: Vec<u8>,
}

/// One `player_info_update` entry: the UUID plus each field carried by the
/// packet's action bitmask. Signature/verification blobs stay bounded-raw.
#[derive(Clone, Debug)]
pub struct PlayerInfoUpdateEntry {
    pub uuid: [u8; 16],
    /// `add_player` (0x01): profile name plus up to 16 (name, value,
    /// signature) property triples.
    pub name: Option<String>,
    pub properties: Vec<(String, String, Option<String>)>,
    /// `initialize_chat` (0x02): raw chat-session blob, retained bounded
    /// for future signature verification.
    pub chat_session: Option<Vec<u8>>,
    /// `update_game_mode` (0x04).
    pub game_mode: Option<i32>,
    /// `update_listed` (0x08).
    pub listed: Option<bool>,
    /// `update_latency` (0x10), milliseconds.
    pub latency: Option<i32>,
    /// `update_display_name` (0x20): raw anonymous-NBT display name.
    pub display_name_nbt: Option<Vec<u8>>,
}

/// One `set_equipment` entry: the 7-bit equipment slot and the raw `Slot`
/// wire bytes for the item in that slot.
#[derive(Clone, Debug)]
pub struct EquipmentEntry {
    pub slot: u8,
    pub item_wire: Vec<u8>,
}

/// Opt-in play receiver result with decoded clientbound custom payloads.
// `Play` is the hot variant; boxing it would put an allocation on every frame.
#[allow(clippy::large_enum_variant)]
#[derive(Clone, Debug)]
pub enum ClientPlayEvent {
    Play(PlayEvent),
    CustomChannel(ClientCustomChannelEvent),
}

// ---------------------------------------------------------------------------
// Extended Protocol 767 play decoding (entities, world, containers, HUD/chat)
// ---------------------------------------------------------------------------
//
// Packet layouts follow the canonical 1.21.1 (protocol 767) schemas resolved
// through `megu_proto`'s packet table — names, never hardcoded ids. Every
// reader is bounded: counts are capped by remaining input or explicit limits,
// NBT goes through the depth/array-bounded `WireReader::read_nbt`, and `Slot`
// payloads are skipped by a fail-closed walker that mirrors the server-side
// `slot_read_*` family in `src/menus_and_items.rs`.

/// `explode` block-destruction records are byte offsets; vanilla blasts
/// stay under a few hundred but the wire count is unbounded, so the
/// decoder caps it like the other collection fields (a malicious count
/// can never drive an allocation or a per-block apply storm).
pub const MAX_EXPLOSION_RECORDS: usize = 1 << 16;

/// Decode the `explosion_sound` `Holder<SoundEvent>` trailer of an
/// `explode` body: `0` = inline name (followed by the optional
/// fixed-range pair), `>0` = `sound_event` registry id minus one. The
/// field sits after two `ParticleOptions` whose payload-carrying kinds
/// vanilla never emits here, so a misaligned or truncated holder
/// degrades to `None` rather than failing the packet.
fn decode_explosion_sound(r: &mut WireReader<'_>) -> (Option<String>, Option<u32>) {
    let Ok(holder) = r.read_varint() else {
        return (None, None);
    };
    if holder == 0 {
        let name = r.read_string(256).ok();
        if name.is_some() && matches!(r.read_bool(), Ok(true)) {
            let _ = r.read_f32();
        }
        (name, None)
    } else {
        (None, u32::try_from(holder - 1).ok())
    }
}

/// Wire angle byte -> degrees (`byte * 360 / 256`, signed).
fn wire_angle(byte: i8) -> f32 {
    f32::from(byte) * (360.0 / 256.0)
}

/// Velocity fixed-point i16 -> blocks/tick (`raw / 8000`).
fn wire_velocity(raw: i16) -> f64 {
    f64::from(raw) / 8000.0
}

/// Relative-move fixed-point i16 -> blocks (`raw / 4096`).
fn wire_delta(raw: i16) -> f64 {
    f64::from(raw) / 4096.0
}

/// Read a count-prefixed UUID array bounded by the remaining input (each
/// entry is exactly 16 bytes).
fn read_uuid_list(r: &mut WireReader<'_>) -> Result<Vec<[u8; 16]>, String> {
    let count = usize::try_from(r.read_varint()?).map_err(|_| "negative uuid count".to_string())?;
    if count > r.remaining() / 16 {
        return Err(format!(
            "uuid count {count} exceeds {} remaining bytes",
            r.remaining()
        ));
    }
    let mut out = Vec::with_capacity(count);
    for _ in 0..count {
        out.push(r.read_uuid()?);
    }
    Ok(out)
}

// --- Bounded `Slot` / data-component skipper -------------------------------
//
// A `Slot` is `VarInt count`; when non-zero: `VarInt itemId`,
// `VarInt addedComponentCount`, `VarInt removedComponentCount`, then
// `addedComponentCount` (component-type VarInt + payload) pairs and
// `removedComponentCount` bare component-type VarInts. Component payloads are
// walked — never interpreted — so hostile component bodies cannot desync the
// parse or allocate unboundedly.

const SLOT_COMPONENT_TYPE_COUNT: i64 = 57;
const SLOT_MAX_DEPTH: usize = 16;
const SLOT_MAX_NBT_DEPTH: usize = 64;
const SLOT_MAX_LIST_LEN: usize = 1_000_000;

/// VarInt reader over a raw slice mirroring the server's `read_protocol_i32`:
/// at most five bytes, values must fit `u32`, then sign-extend like i32.
fn slot_varint(b: &[u8], p: &mut usize) -> Option<i64> {
    let mut v = 0i64;
    for i in 0..5 {
        let byte = *b.get(*p)?;
        *p += 1;
        v |= ((byte & 0x7f) as i64) << (7 * i);
        if byte & 0x80 == 0 {
            return Some(i64::from(u32::try_from(v).ok()? as i32));
        }
    }
    None
}

fn slot_bytes(b: &[u8], p: &mut usize, len: usize) -> Option<()> {
    let end = (*p).checked_add(len)?;
    b.get(*p..end)?;
    *p = end;
    Some(())
}

/// Strict boolean byte: only `0`/`1` are valid on the component wire.
fn slot_bool(b: &[u8], p: &mut usize) -> Option<bool> {
    let value = *b.get(*p)?;
    if value > 1 {
        return None;
    }
    *p += 1;
    Some(value != 0)
}

fn slot_count(b: &[u8], p: &mut usize, max: i64) -> Option<usize> {
    usize::try_from(slot_varint(b, p)?)
        .ok()
        .filter(|&n| n <= max as usize)
}

fn slot_string(b: &[u8], p: &mut usize) -> Option<()> {
    let len = slot_count(b, p, 98301)?;
    let text = b.get(*p..(*p).checked_add(len)?)?;
    std::str::from_utf8(text).ok()?;
    *p += len;
    Some(())
}

fn slot_nbt_payload(b: &[u8], p: &mut usize, tag: u8, depth: usize) -> Option<()> {
    if depth > SLOT_MAX_NBT_DEPTH {
        return None;
    }
    match tag {
        1 => slot_bytes(b, p, 1)?,
        2 => slot_bytes(b, p, 2)?,
        3 | 5 => slot_bytes(b, p, 4)?,
        4 | 6 => slot_bytes(b, p, 8)?,
        7 | 11 | 12 => {
            let length = i32::from_be_bytes(b.get(*p..(*p).checked_add(4)?)?.try_into().ok()?);
            *p += 4;
            let length = usize::try_from(length).ok()?;
            let element_size = match tag {
                7 => 1,
                11 => 4,
                12 => 8,
                _ => return None,
            };
            slot_bytes(b, p, length.checked_mul(element_size)?)?;
        }
        8 => {
            let length =
                u16::from_be_bytes(b.get(*p..(*p).checked_add(2)?)?.try_into().ok()?) as usize;
            slot_bytes(b, p, 2 + length)?;
        }
        9 => {
            let element_tag = *b.get(*p)?;
            *p += 1;
            let length = i32::from_be_bytes(b.get(*p..(*p).checked_add(4)?)?.try_into().ok()?);
            *p += 4;
            let length = usize::try_from(length).ok()?;
            if length > SLOT_MAX_LIST_LEN || (element_tag == 0 && length != 0) {
                return None;
            }
            for _ in 0..length {
                slot_nbt_payload(b, p, element_tag, depth + 1)?;
            }
        }
        10 => loop {
            let child_tag = *b.get(*p)?;
            *p += 1;
            if child_tag == 0 {
                break;
            }
            if child_tag > 12 {
                return None;
            }
            let name_length =
                u16::from_be_bytes(b.get(*p..(*p).checked_add(2)?)?.try_into().ok()?) as usize;
            slot_bytes(b, p, 2 + name_length)?;
            slot_nbt_payload(b, p, child_tag, depth + 1)?;
        },
        _ => return None,
    }
    Some(())
}

fn slot_nbt(b: &[u8], p: &mut usize) -> Option<()> {
    let tag = *b.get(*p)?;
    if !(1..=12).contains(&tag) {
        return None;
    }
    *p += 1;
    slot_nbt_payload(b, p, tag, 0)
}

fn slot_optional_nbt(b: &[u8], p: &mut usize) -> Option<()> {
    if *b.get(*p)? == 0 {
        *p += 1;
        Some(())
    } else {
        slot_nbt(b, p)
    }
}

fn slot_text(b: &[u8], p: &mut usize) -> Option<()> {
    slot_nbt(b, p)
}

fn slot_optional_string(b: &[u8], p: &mut usize) -> Option<()> {
    if slot_bool(b, p)? {
        slot_string(b, p)?;
    }
    Some(())
}

fn slot_holder_set(b: &[u8], p: &mut usize) -> Option<()> {
    let kind = slot_varint(b, p)?;
    if kind == 0 {
        slot_string(b, p)?;
    } else {
        for _ in 0..usize::try_from(kind - 1).ok().filter(|&n| n <= 4096)? {
            slot_varint(b, p)?;
        }
    }
    Some(())
}

fn slot_block_predicate(b: &[u8], p: &mut usize) -> Option<()> {
    if slot_bool(b, p)? {
        slot_holder_set(b, p)?;
    }
    if slot_bool(b, p)? {
        for _ in 0..slot_count(b, p, 256)? {
            slot_string(b, p)?;
            if slot_bool(b, p)? {
                slot_string(b, p)?;
            } else {
                slot_string(b, p)?;
                slot_string(b, p)?;
            }
        }
    }
    slot_optional_nbt(b, p)
}

fn slot_trim_material(b: &[u8], p: &mut usize) -> Option<()> {
    slot_string(b, p)?;
    slot_varint(b, p)?;
    for _ in 0..slot_count(b, p, 256)? {
        slot_varint(b, p)?;
        slot_string(b, p)?;
    }
    slot_nbt(b, p)
}

fn slot_trim_pattern(b: &[u8], p: &mut usize) -> Option<()> {
    slot_string(b, p)?;
    slot_varint(b, p)?;
    slot_nbt(b, p)?;
    slot_bool(b, p)?;
    Some(())
}

fn slot_sound_event(b: &[u8], p: &mut usize) -> Option<()> {
    slot_string(b, p)?;
    if slot_bool(b, p)? {
        slot_bytes(b, p, 4)?;
    }
    Some(())
}

fn slot_sound_holder(b: &[u8], p: &mut usize) -> Option<()> {
    if slot_varint(b, p)? == 0 {
        slot_sound_event(b, p)?;
    }
    Some(())
}

fn slot_instrument_data(b: &[u8], p: &mut usize) -> Option<()> {
    slot_sound_holder(b, p)?;
    slot_bytes(b, p, 8)?;
    slot_nbt(b, p)
}

fn slot_jukebox_song_data(b: &[u8], p: &mut usize) -> Option<()> {
    slot_sound_holder(b, p)?;
    slot_nbt(b, p)?;
    slot_bytes(b, p, 4)?;
    slot_varint(b, p)?;
    Some(())
}

fn slot_firework_explosion(b: &[u8], p: &mut usize) -> Option<()> {
    if !(0..=4).contains(&slot_varint(b, p)?) {
        return None;
    }
    for _ in 0..2 {
        let count = slot_count(b, p, 256)?;
        slot_bytes(b, p, count.checked_mul(4)?)?;
    }
    slot_bool(b, p)?;
    slot_bool(b, p)?;
    Some(())
}

/// Skip an `attribute_modifiers` component payload (type 12): bounded
/// modifier count, each {attribute varint, name varint+utf8, f64 amount,
/// operation varint, slot varint}, then the show-in-tooltip byte.
fn slot_attr_modifiers(b: &[u8], p: &mut usize) -> Option<()> {
    let n = slot_varint(b, p)?;
    if !(0..=1024).contains(&n) {
        return None;
    }
    for _ in 0..n {
        slot_varint(b, p)?; // attribute id
        let id_len = usize::try_from(slot_varint(b, p)?).ok()?;
        if id_len > 32767 {
            return None;
        }
        slot_bytes(b, p, id_len)?;
        slot_bytes(b, p, 8)?; // amount f64
        slot_varint(b, p)?; // operation
        slot_varint(b, p)?; // slot group
    }
    slot_bool(b, p)?;
    Some(())
}

/// Skip one mob-effect detail record inside `potion_contents` (recursive via
/// the optional nested-effects flag, depth-capped).
fn slot_potion_details(b: &[u8], p: &mut usize, depth: usize) -> Option<()> {
    if depth == 16 {
        return None;
    }
    slot_varint(b, p)?; // amplifier
    slot_varint(b, p)?; // duration
    for _ in 0..3 {
        slot_bool(b, p)?; // ambient, visible, icon
    }
    if slot_bool(b, p)? {
        slot_potion_details(b, p, depth + 1)?;
    }
    Some(())
}

/// `potion_contents` component (type 31): optional potion id varint, optional
/// custom-color i32, effect list, optional custom name.
fn slot_potion_contents(b: &[u8], p: &mut usize) -> Option<()> {
    if slot_bool(b, p)? {
        slot_varint(b, p)?;
    }
    if slot_bool(b, p)? {
        slot_bytes(b, p, 4)?;
    }
    let n = slot_varint(b, p)?;
    if !(0..=256).contains(&n) {
        return None;
    }
    for _ in 0..n {
        slot_varint(b, p)?;
        slot_potion_details(b, p, 0)?;
    }
    slot_optional_string(b, p)
}

/// Skip one data-component payload of type `ty`. Mirrors the server-side
/// `slot_read_component` match arm-for-arm; every arm only advances `p`.
fn slot_component(b: &[u8], p: &mut usize, ty: i64, depth: usize) -> Option<()> {
    match ty {
        0 => slot_nbt(b, p)?,
        1..=3 | 13 | 16 | 26 | 28 | 41 | 50 => {
            slot_varint(b, p)?;
        }
        8 => {
            if !(0..=3).contains(&slot_varint(b, p)?) {
                return None;
            }
        }
        4 | 18 => {
            slot_bool(b, p)?;
        }
        5 | 6 => slot_text(b, p)?,
        7 => {
            for _ in 0..slot_count(b, p, 256)? {
                slot_optional_nbt(b, p)?;
            }
        }
        9 | 23 => {
            for _ in 0..slot_count(b, p, 256)? {
                slot_varint(b, p)?;
                slot_varint(b, p)?;
            }
            slot_bool(b, p)?;
        }
        10 | 11 => {
            for _ in 0..slot_count(b, p, 256)? {
                slot_block_predicate(b, p)?;
            }
            slot_bool(b, p)?;
        }
        12 => slot_attr_modifiers(b, p)?,
        14 | 15 | 17 | 21 => {} // Unit components have zero-byte payloads.
        19 | 27 | 36..=39 | 43 | 55 | 56 => slot_nbt(b, p)?,
        20 => {
            slot_varint(b, p)?; // nutrition
            slot_bytes(b, p, 4)?; // saturation
            slot_bool(b, p)?; // can always eat
            slot_bytes(b, p, 4)?; // eating seconds
            slot_stack(b, p, depth + 1)?; // conversion item, possibly empty
            for _ in 0..slot_count(b, p, 256)? {
                slot_varint(b, p)?;
                slot_bytes(b, p, 4)?; // probability
            }
        }
        22 => {
            for _ in 0..slot_count(b, p, 256)? {
                slot_holder_set(b, p)?;
                if slot_bool(b, p)? {
                    slot_bytes(b, p, 4)?;
                }
                if slot_bool(b, p)? {
                    slot_bool(b, p)?;
                }
            }
            slot_bytes(b, p, 4)?;
            slot_varint(b, p)?;
        }
        24 => {
            slot_bytes(b, p, 4)?;
            slot_bool(b, p)?;
        }
        25 => slot_bytes(b, p, 4)?,
        29 | 30 | 52 => {
            for _ in 0..slot_count(b, p, 256)? {
                slot_stack(b, p, depth + 1)?;
            }
        }
        31 => slot_potion_contents(b, p)?,
        32 => {
            for _ in 0..slot_count(b, p, 256)? {
                slot_varint(b, p)?;
                slot_varint(b, p)?;
            }
        }
        33 => {
            for _ in 0..slot_count(b, p, 100)? {
                slot_string(b, p)?;
                slot_optional_string(b, p)?;
            }
        }
        34 => {
            slot_string(b, p)?;
            slot_optional_string(b, p)?;
            slot_string(b, p)?;
            slot_varint(b, p)?;
            for _ in 0..slot_count(b, p, 100)? {
                slot_text(b, p)?;
                slot_optional_nbt(b, p)?;
            }
            slot_bool(b, p)?;
        }
        35 => {
            if slot_varint(b, p)? == 0 {
                slot_trim_material(b, p)?;
            }
            if slot_varint(b, p)? == 0 {
                slot_trim_pattern(b, p)?;
            }
            slot_bool(b, p)?;
        }
        40 => {
            if slot_varint(b, p)? == 0 {
                slot_instrument_data(b, p)?;
            }
        }
        42 => {
            if slot_bool(b, p)? {
                if slot_varint(b, p)? == 0 {
                    slot_jukebox_song_data(b, p)?;
                }
            } else {
                slot_string(b, p)?;
            }
            slot_bool(b, p)?;
        }
        44 => {
            if slot_bool(b, p)? {
                slot_string(b, p)?;
                slot_bytes(b, p, 8)?;
            }
            slot_bool(b, p)?;
        }
        45 => slot_firework_explosion(b, p)?,
        46 => {
            slot_varint(b, p)?;
            for _ in 0..slot_count(b, p, 256)? {
                slot_firework_explosion(b, p)?;
            }
        }
        47 => {
            if slot_bool(b, p)? {
                slot_string(b, p)?;
            }
            if slot_bool(b, p)? {
                slot_bytes(b, p, 16)?;
            }
            for _ in 0..slot_count(b, p, 256)? {
                slot_string(b, p)?;
                slot_string(b, p)?;
                if slot_bool(b, p)? {
                    slot_string(b, p)?;
                }
            }
        }
        48 => slot_string(b, p)?,
        49 => {
            for _ in 0..slot_count(b, p, 256)? {
                if slot_varint(b, p)? == 0 {
                    slot_string(b, p)?;
                    slot_string(b, p)?;
                }
                if !(0..=15).contains(&slot_varint(b, p)?) {
                    return None;
                }
            }
        }
        51 => {
            for _ in 0..slot_count(b, p, 4)? {
                slot_varint(b, p)?;
            }
        }
        53 => {
            for _ in 0..slot_count(b, p, 256)? {
                slot_string(b, p)?;
                slot_string(b, p)?;
            }
        }
        54 => {
            for _ in 0..slot_count(b, p, 256)? {
                slot_nbt(b, p)?;
                slot_varint(b, p)?;
                slot_varint(b, p)?;
            }
        }
        _ => return None,
    }
    Some(())
}

/// Skip one full `Slot` (count + optional item payload) starting at `*p`;
/// advances `p` past it. Depth-capped for nested stacks (food conversion,
/// bundle/container contents).
fn slot_stack(b: &[u8], p: &mut usize, depth: usize) -> Option<()> {
    if depth > SLOT_MAX_DEPTH {
        return None;
    }
    let cnt = slot_varint(b, p)?;
    if cnt == 0 {
        return Some(());
    }
    if !(1..=127).contains(&cnt) {
        return None;
    }
    let item = slot_varint(b, p)?;
    if !(0..=i64::from(i32::MAX)).contains(&item) {
        return None;
    }
    let nadd = slot_count(b, p, 128)?;
    let nrem = slot_count(b, p, 128)?;
    for _ in 0..nadd {
        let ty = slot_varint(b, p)?;
        if !(0..SLOT_COMPONENT_TYPE_COUNT).contains(&ty) {
            return None;
        }
        slot_component(b, p, ty, depth)?;
    }
    for _ in 0..nrem {
        let ty = slot_varint(b, p)?;
        if !(0..SLOT_COMPONENT_TYPE_COUNT).contains(&ty) {
            return None;
        }
    }
    Some(())
}

/// Skip one `Slot` at `start`; returns the offset just past it.
fn slot_skip_extent(b: &[u8], start: usize) -> Option<usize> {
    let mut p = start;
    slot_stack(b, &mut p, 0)?;
    Some(p)
}

/// One scalar `set_entity_data` metadata record value, keyed by serializer
/// type in [`parse_entity_metadata`]'s `(index, type, value)` triples.
/// Payloads the client does not model surface as
/// [`EntityMetaValue::Skipped`] — the walker still advances over them so
/// later scalars stay reachable.
#[derive(Clone, Debug, PartialEq)]
pub enum EntityMetaValue {
    Byte(u8),
    VarInt(i32),
    Float(f32),
    Text(String),
    OptionalText(Option<String>),
    Bool(bool),
    /// Raw `Slot` wire bytes (item-entity stacks decode via
    /// [`decode_item_stack_components`]).
    Slot(Vec<u8>),
    /// Recognized-but-unmodeled payload; the walker skipped it.
    Skipped,
}

/// Walk one `set_entity_data` entry list (`index u8` / `type VarInt` /
/// value records terminated by `0xFF`) into scalar
/// [`EntityMetaValue`] triples. Fail-closed like the rest of the wire
/// layer: a truncated record or a serializer type with an unbounded
/// payload (particles) ends the walk and returns the entries decoded so
/// far — partial metadata still beats a full desync.
pub fn parse_entity_metadata(raw: &[u8]) -> Vec<(u8, i32, EntityMetaValue)> {
    let mut out = Vec::new();
    let mut p = 0usize;
    while let Some(&index) = raw.get(p) {
        if index == 0xFF {
            break;
        }
        p += 1;
        let Some(ty) = slot_varint(raw, &mut p).and_then(|v| i32::try_from(v).ok()) else {
            break;
        };
        let value = match ty {
            // byte
            0 => {
                let Some(&b) = raw.get(p) else { break };
                p += 1;
                EntityMetaValue::Byte(b)
            }
            // varint
            1 => {
                let Some(v) = slot_varint(raw, &mut p).and_then(|v| i32::try_from(v).ok())
                else {
                    break;
                };
                EntityMetaValue::VarInt(v)
            }
            // varlong
            2 => {
                if slot_varint(raw, &mut p).is_none() {
                    break;
                }
                EntityMetaValue::Skipped
            }
            // float
            3 => {
                let Some(bytes) = raw.get(p..p + 4) else { break };
                p += 4;
                EntityMetaValue::Float(f32::from_be_bytes(bytes.try_into().unwrap_or_default()))
            }
            // string
            4 => {
                let Some(len) =
                    slot_varint(raw, &mut p).and_then(|v| usize::try_from(v).ok())
                else {
                    break;
                };
                let Some(bytes) = raw.get(p..p + len) else { break };
                let text = String::from_utf8_lossy(bytes).into_owned();
                p += len;
                EntityMetaValue::Text(text)
            }
            // text component (NBT)
            5 => {
                let start = p;
                if slot_nbt(raw, &mut p).is_none() {
                    break;
                }
                let text = WireReader::new(&raw[start..p])
                    .read_nbt()
                    .map(|v| v.extract_text())
                    .unwrap_or_default();
                EntityMetaValue::Text(text)
            }
            // optional text component (bool + NBT)
            6 => {
                let Some(&present) = raw.get(p) else { break };
                p += 1;
                if present == 0 {
                    EntityMetaValue::OptionalText(None)
                } else {
                    let start = p;
                    if slot_nbt(raw, &mut p).is_none() {
                        break;
                    }
                    let text = WireReader::new(&raw[start..p])
                        .read_nbt()
                        .map(|v| v.extract_text())
                        .unwrap_or_default();
                    EntityMetaValue::OptionalText(Some(text))
                }
            }
            // item stack (Slot)
            7 => {
                let Some(end) = slot_skip_extent(raw, p) else { break };
                let bytes = raw[p..end].to_vec();
                p = end;
                EntityMetaValue::Slot(bytes)
            }
            // boolean
            8 => {
                let Some(&b) = raw.get(p) else { break };
                p += 1;
                EntityMetaValue::Bool(b != 0)
            }
            // rotations (3 x f32)
            9 => {
                if raw.get(p..p + 12).is_none() {
                    break;
                }
                p += 12;
                EntityMetaValue::Skipped
            }
            // block position (packed i64)
            10 => {
                if raw.get(p..p + 8).is_none() {
                    break;
                }
                p += 8;
                EntityMetaValue::Skipped
            }
            // optional block position (bool + i64)
            11 => {
                let Some(&present) = raw.get(p) else { break };
                p += 1;
                if present != 0 {
                    if raw.get(p..p + 8).is_none() {
                        break;
                    }
                    p += 8;
                }
                EntityMetaValue::Skipped
            }
            // direction / optional-unsigned-int / pose / variant ids —
            // all plain VarInts
            12 | 14 | 15 | 20 | 21 | 22 | 23 | 24 | 26 | 27 | 28 => {
                let Some(v) = slot_varint(raw, &mut p).and_then(|v| i32::try_from(v).ok())
                else {
                    break;
                };
                EntityMetaValue::VarInt(v)
            }
            // optional living-entity reference (bool + uuid)
            13 => {
                let Some(&present) = raw.get(p) else { break };
                p += 1;
                if present != 0 {
                    if raw.get(p..p + 16).is_none() {
                        break;
                    }
                    p += 16;
                }
                EntityMetaValue::Skipped
            }
            // compound tag (NBT)
            16 => {
                if slot_nbt(raw, &mut p).is_none() {
                    break;
                }
                EntityMetaValue::Skipped
            }
            // optional global position (bool + string + i64)
            25 => {
                let Some(&present) = raw.get(p) else { break };
                p += 1;
                if present != 0 {
                    let Some(len) =
                        slot_varint(raw, &mut p).and_then(|v| usize::try_from(v).ok())
                    else {
                        break;
                    };
                    if raw.get(p..p + len + 8).is_none() {
                        break;
                    }
                    p += len + 8;
                }
                EntityMetaValue::Skipped
            }
            // vector3 (3 x f32) / quaternion (4 x f32)
            29 | 30 => {
                let len = if ty == 29 { 12 } else { 16 };
                if raw.get(p..p + len).is_none() {
                    break;
                }
                p += len;
                EntityMetaValue::Skipped
            }
            // particles (17/18) and anything newer carry payloads that
            // cannot be bounded generically — keep what was decoded.
            _ => break,
        };
        out.push((index, ty, value));
    }
    out
}

/// One added `Slot` component: the verified 767 `data_component_type` id
/// plus its exact wire payload bytes (boundary recovered by the
/// `slot_component` walker, retained raw for consumers).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ItemComponentPatch {
    pub type_id: i32,
    pub data: Vec<u8>,
}

/// Decoded component patch of one raw `Slot` wire blob — the payload shape
/// retained in `container_set_content`/`container_set_slot` `raw_*` fields
/// and `set_equipment` `item_wire` entries.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ItemStackComponents {
    /// Stack count; `0` means the empty stack (no further fields encoded).
    pub count: i32,
    /// `minecraft:item` registry id (0 for an empty stack).
    pub item_id: i32,
    /// Components added over the item prototype, in wire order.
    pub added: Vec<ItemComponentPatch>,
    /// Prototype component ids removed, in wire order.
    pub removed: Vec<i32>,
}

/// Decode one raw `Slot` component patch, mirroring `slot_stack`'s bounds:
/// counts are capped, component type ids must fall inside the verified
/// `SLOT_COMPONENT_TYPE_COUNT` table, and unknown component shapes fail
/// closed rather than desynchronizing the patch list.
///
/// Returns the decoded patch plus the number of bytes consumed, so callers
/// walking a `container_set_content` slot array can advance per slot.
pub fn decode_item_stack_components(wire: &[u8]) -> Result<(ItemStackComponents, usize), String> {
    const MALFORMED: &str = "malformed item stack component patch";
    let mut p = 0usize;
    let cnt = slot_varint(wire, &mut p).ok_or(MALFORMED)?;
    if cnt == 0 {
        return Ok((
            ItemStackComponents {
                count: 0,
                item_id: 0,
                added: Vec::new(),
                removed: Vec::new(),
            },
            p,
        ));
    }
    if !(1..=127).contains(&cnt) {
        return Err(MALFORMED.into());
    }
    let item = slot_varint(wire, &mut p).ok_or(MALFORMED)?;
    if !(0..=i64::from(i32::MAX)).contains(&item) {
        return Err(MALFORMED.into());
    }
    let nadd = slot_count(wire, &mut p, 128).ok_or(MALFORMED)?;
    let nrem = slot_count(wire, &mut p, 128).ok_or(MALFORMED)?;
    let mut added = Vec::with_capacity(nadd.min(16));
    for _ in 0..nadd {
        let ty = slot_varint(wire, &mut p).ok_or(MALFORMED)?;
        if !(0..SLOT_COMPONENT_TYPE_COUNT).contains(&ty) {
            return Err(MALFORMED.into());
        }
        let start = p;
        slot_component(wire, &mut p, ty, 0).ok_or(MALFORMED)?;
        added.push(ItemComponentPatch {
            type_id: ty as i32,
            data: wire[start..p].to_vec(),
        });
    }
    let mut removed = Vec::with_capacity(nrem.min(16));
    for _ in 0..nrem {
        let ty = slot_varint(wire, &mut p).ok_or(MALFORMED)?;
        if !(0..SLOT_COMPONENT_TYPE_COUNT).contains(&ty) {
            return Err(MALFORMED.into());
        }
        removed.push(ty as i32);
    }
    Ok((
        ItemStackComponents {
            count: cnt as i32,
            item_id: item as i32,
            added,
            removed,
        },
        p,
    ))
}

/// A validated `transfer` reconnect target: the host string plus a port
/// normalized into the `u16` range the socket path expects.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TransferTarget {
    pub host: String,
    pub port: u16,
}

/// Handle one decoded play-phase `transfer` instruction: validate the
/// advertised `host`/`port` (matching `megu_proto`'s wire-side
/// `validate_transfer` rules) and return the reconnect target the session
/// should dial next. Fails closed on empty/oversized hosts and
/// out-of-range ports so a hostile packet cannot steer the reconnect.
pub fn handle_transfer_packet(host: &str, port: i32) -> Result<TransferTarget, String> {
    if host.is_empty() || host.len() > 255 {
        return Err(format!("invalid transfer host ({} bytes)", host.len()));
    }
    if !(1..=65535).contains(&port) {
        return Err(format!("invalid transfer port {port}"));
    }
    Ok(TransferTarget {
        host: host.to_string(),
        port: port as u16,
    })
}

/// Read one `Slot` payload from `r`, returning its raw wire bytes — bounded
/// by the same fail-closed walker the container decoders use.
fn read_slot_wire(r: &mut WireReader<'_>) -> Result<Vec<u8>, String> {
    let end = slot_skip_extent(r.rest(), 0).ok_or("malformed slot payload")?;
    Ok(r.take(end)?.to_vec())
}

/// Read one merchant `ItemCost`: varint item id, varint count, then the
/// `addedComponentCount` varint plus every `SlotComponent` entry kept as
/// raw wire bytes (`components_wire` covers the count prefix too).
fn read_merchant_item_cost(r: &mut WireReader<'_>) -> Result<MerchantItemCost, String> {
    let item = i64::from(r.read_varint()?);
    let count = r.read_varint()?;
    let body = r.rest();
    let mut p = 0usize;
    let ncomp_raw = slot_varint(body, &mut p).ok_or("truncated component count")?;
    if ncomp_raw < 0 {
        return Err("negative component count".into());
    }
    let ncomp = ncomp_raw as usize;
    // Each component is at least a type varint plus a payload byte.
    if ncomp > body.len().saturating_sub(p) {
        return Err("component count exceeds remaining input".into());
    }
    for _ in 0..ncomp {
        let ty = slot_varint(body, &mut p).ok_or("truncated component type")?;
        if !(0..SLOT_COMPONENT_TYPE_COUNT).contains(&ty) {
            return Err("unknown component type".into());
        }
        slot_component(body, &mut p, ty, 0).ok_or("malformed component payload")?;
    }
    let components_wire = r.take(p)?.to_vec();
    Ok(MerchantItemCost {
        item,
        count,
        components_wire,
    })
}

/// Read one `update_advancements` display block (title/description NBT,
/// icon slot, frame, flags, optional background texture, x/y floats).
fn read_advancement_display(r: &mut WireReader<'_>) -> Result<WireAdvancementDisplay, String> {
    let title_nbt = read_nbt_raw(r)?.to_vec();
    let description_nbt = read_nbt_raw(r)?.to_vec();
    let icon_slot_wire = read_slot_wire(r)?;
    let frame_type = r.read_varint()?;
    let flags = r.read_i32()?;
    let background = if flags & 0x01 != 0 {
        Some(r.read_string(256)?)
    } else {
        None
    };
    let x = r.read_f32()?;
    let y = r.read_f32()?;
    Ok(WireAdvancementDisplay {
        title_nbt,
        description_nbt,
        icon_slot_wire,
        frame_type,
        flags,
        background,
        x,
        y,
    })
}

/// Read one `update_advancements` tree entry.
fn read_wire_advancement(r: &mut WireReader<'_>) -> Result<WireAdvancement, String> {
    let key = r.read_string(32767)?;
    let parent_id = if r.read_bool()? {
        Some(r.read_string(32767)?)
    } else {
        None
    };
    let display = if r.read_bool()? {
        Some(read_advancement_display(r)?)
    } else {
        None
    };
    let nreq =
        usize::try_from(r.read_varint()?).map_err(|_| "negative requirement count".to_string())?;
    if nreq > r.remaining() {
        return Err("requirement count exceeds remaining input".into());
    }
    let mut requirements = Vec::with_capacity(nreq.min(64));
    for _ in 0..nreq {
        let ncrit = usize::try_from(r.read_varint()?)
            .map_err(|_| "negative criterion count".to_string())?;
        if ncrit > r.remaining() {
            return Err("criterion count exceeds remaining input".into());
        }
        let mut group = Vec::with_capacity(ncrit.min(32));
        for _ in 0..ncrit {
            group.push(r.read_string(32767)?);
        }
        requirements.push(group);
    }
    let sends_telemetry = r.read_bool()?;
    Ok(WireAdvancement {
        key,
        parent_id,
        display,
        requirements,
        sends_telemetry,
    })
}

// --- Extended play event decoding -----------------------------------------

/// Decode one clientbound play packet body into a [`PlayEvent`] for the
/// families added beyond the original 18: entities, world/time/weather/
/// particles/block entities, containers, and HUD/chat. Returns `Ok(None)`
/// for packet names outside this set so the caller falls back to
/// [`PlayEvent::Other`]. Only valid on the 767 wire — callers gate on
/// `!wire_26x()`.
pub fn decode_extended_play_event(name: &str, body: &[u8]) -> Result<Option<PlayEvent>, String> {
    let mut r = WireReader::new(body);
    Ok(Some(match name {
        "add_entity" => {
            let eid = r.read_varint()?;
            let uuid = r.read_uuid()?;
            let entity_type = r.read_varint()?;
            let x = r.read_f64()?;
            let y = r.read_f64()?;
            let z = r.read_f64()?;
            let pitch = wire_angle(r.read_i8()?);
            let yaw = wire_angle(r.read_i8()?);
            let head_yaw = wire_angle(r.read_i8()?);
            let data = r.read_varint()?;
            let vx = wire_velocity(r.read_i16()?);
            let vy = wire_velocity(r.read_i16()?);
            let vz = wire_velocity(r.read_i16()?);
            PlayEvent::AddEntity {
                eid,
                uuid,
                entity_type,
                x,
                y,
                z,
                pitch,
                yaw,
                head_yaw,
                data,
                vx,
                vy,
                vz,
            }
        }
        "remove_entities" => {
            let count = usize::try_from(r.read_varint()?)
                .map_err(|_| "negative entity count".to_string())?;
            // Each id is at least one VarInt byte.
            if count > r.remaining() {
                return Err(format!(
                    "entity count {count} exceeds {} remaining bytes",
                    r.remaining()
                ));
            }
            let mut eids = Vec::with_capacity(count);
            for _ in 0..count {
                eids.push(r.read_varint()?);
            }
            PlayEvent::RemoveEntities { eids }
        }
        "move_entity_pos" => {
            let eid = r.read_varint()?;
            let dx = wire_delta(r.read_i16()?);
            let dy = wire_delta(r.read_i16()?);
            let dz = wire_delta(r.read_i16()?);
            let on_ground = r.read_bool()?;
            PlayEvent::MoveEntityPos {
                eid,
                dx,
                dy,
                dz,
                on_ground,
            }
        }
        "move_entity_pos_rot" => {
            let eid = r.read_varint()?;
            let dx = wire_delta(r.read_i16()?);
            let dy = wire_delta(r.read_i16()?);
            let dz = wire_delta(r.read_i16()?);
            let yaw = wire_angle(r.read_i8()?);
            let pitch = wire_angle(r.read_i8()?);
            let on_ground = r.read_bool()?;
            PlayEvent::MoveEntityPosRot {
                eid,
                dx,
                dy,
                dz,
                yaw,
                pitch,
                on_ground,
            }
        }
        "move_entity_rot" => {
            let eid = r.read_varint()?;
            let yaw = wire_angle(r.read_i8()?);
            let pitch = wire_angle(r.read_i8()?);
            let on_ground = r.read_bool()?;
            PlayEvent::MoveEntityRot {
                eid,
                yaw,
                pitch,
                on_ground,
            }
        }
        "teleport_entity" => {
            let eid = r.read_varint()?;
            let x = r.read_f64()?;
            let y = r.read_f64()?;
            let z = r.read_f64()?;
            let yaw = wire_angle(r.read_i8()?);
            let pitch = wire_angle(r.read_i8()?);
            let on_ground = r.read_bool()?;
            PlayEvent::TeleportEntity {
                eid,
                x,
                y,
                z,
                yaw,
                pitch,
                on_ground,
            }
        }
        "rotate_head" => {
            let eid = r.read_varint()?;
            let head_yaw = wire_angle(r.read_i8()?);
            PlayEvent::RotateHead { eid, head_yaw }
        }
        "set_entity_motion" => {
            let eid = r.read_varint()?;
            let vx = wire_velocity(r.read_i16()?);
            let vy = wire_velocity(r.read_i16()?);
            let vz = wire_velocity(r.read_i16()?);
            PlayEvent::SetEntityMotion { eid, vx, vy, vz }
        }
        "entity_event" => {
            // entity_status writes the entity id as a fixed i32, not a VarInt.
            let eid = r.read_i32()?;
            let status = r.read_i8()?;
            PlayEvent::EntityEvent { eid, status }
        }
        "hurt_animation" => {
            let eid = r.read_varint()?;
            let yaw = r.read_f32()?;
            PlayEvent::HurtAnimation { eid, yaw }
        }
        "damage_event" => {
            let eid = r.read_varint()?;
            let source_type = r.read_varint()?;
            let source_cause = r.read_varint()?;
            let source_direct = r.read_varint()?;
            let source_position = if r.read_bool()? {
                Some([r.read_f64()?, r.read_f64()?, r.read_f64()?])
            } else {
                None
            };
            PlayEvent::DamageEvent {
                eid,
                source_type,
                source_cause,
                source_direct,
                source_position,
            }
        }
        "set_equipment" => {
            let eid = r.read_varint()?;
            // topBitSetTerminatedArray: each entry is a slot byte (0x80 = more
            // follow) plus a Slot; the entry without 0x80 terminates. Vanilla
            // equipment slots are a small fixed enum, so cap the walk to keep
            // hostile continuation bytes from amplifying into huge vecs.
            let mut entries = Vec::new();
            loop {
                if entries.len() >= 64 {
                    return Err("set_equipment entry count exceeds bound".to_string());
                }
                let byte = r.read_u8()?;
                let slot = byte & 0x7f;
                let item_start = body.len() - r.remaining();
                let item_end = slot_skip_extent(body, item_start)
                    .ok_or_else(|| "malformed set_equipment slot".to_string())?;
                // Advance the reader past the skipped slot bytes.
                r.take(item_end - item_start)?;
                entries.push(EquipmentEntry {
                    slot,
                    item_wire: body[item_start..item_end].to_vec(),
                });
                if byte & 0x80 == 0 {
                    break;
                }
            }
            PlayEvent::SetEquipment { eid, entries }
        }
        "set_entity_data" => {
            let eid = r.read_varint()?;
            PlayEvent::SetEntityData {
                eid,
                raw_entries: r.rest().to_vec(),
            }
        }
        "set_time" => {
            let world_age = r.read_i64()?;
            let time_of_day = r.read_i64()?;
            PlayEvent::SetTime {
                world_age,
                time_of_day,
            }
        }
        "game_event" => {
            let event = r.read_u8()?;
            let value = r.read_f32()?;
            PlayEvent::GameEvent { event, value }
        }
        "level_event" => {
            let event = r.read_i32()?;
            let pos = r.read_position()?;
            let data = r.read_i32()?;
            let disable_relative_volume = r.read_bool()?;
            PlayEvent::LevelEvent {
                event,
                pos,
                data,
                disable_relative_volume,
            }
        }
        "level_particles" => {
            let long_distance = r.read_bool()?;
            let x = r.read_f64()?;
            let y = r.read_f64()?;
            let z = r.read_f64()?;
            let offset_x = r.read_f32()?;
            let offset_y = r.read_f32()?;
            let offset_z = r.read_f32()?;
            let max_speed = r.read_f32()?;
            let count = r.read_i32()?;
            let particle_id = r.read_varint()?;
            PlayEvent::LevelParticles {
                particle_id,
                long_distance,
                x,
                y,
                z,
                offset_x,
                offset_y,
                offset_z,
                max_speed,
                count,
                particle_data: r.rest().to_vec(),
            }
        }
        "block_destruction" => {
            let breaker_eid = r.read_varint()?;
            let pos = r.read_position()?;
            let stage = r.read_i8()?;
            PlayEvent::BlockDestruction {
                breaker_eid,
                pos,
                stage,
            }
        }
        "explode" => {
            let x = r.read_f64()?;
            let y = r.read_f64()?;
            let z = r.read_f64()?;
            let power = r.read_f32()?;
            let record_count = usize::try_from(r.read_varint()?)
                .map_err(|_| "negative explosion record count".to_string())?;
            if record_count > MAX_EXPLOSION_RECORDS || record_count > r.remaining() / 3 {
                return Err(format!(
                    "explosion record count {record_count} exceeds {} remaining bytes",
                    r.remaining()
                ));
            }
            let mut records = Vec::with_capacity(record_count);
            for _ in 0..record_count {
                records.push([r.read_i8()?, r.read_i8()?, r.read_i8()?]);
            }
            let knockback = [r.read_f32()?, r.read_f32()?, r.read_f32()?];
            let block_interaction = r.read_varint()?;
            // `ParticleOptions` ids: vanilla only ever sends payload-free
            // emitters in this packet (`poof`, `explosion`,
            // `explosion_emitter`, `gust*`), so each id is a bare varint.
            let small_particle_id = r.read_varint()?;
            let large_particle_id = r.read_varint()?;
            let (sound_name, sound_id) = decode_explosion_sound(&mut r);
            PlayEvent::Explosion {
                x,
                y,
                z,
                power,
                records,
                knockback,
                block_interaction,
                small_particle_id,
                large_particle_id,
                sound_name,
                sound_id,
            }
        }
        "block_entity_data" => {
            let pos = r.read_position()?;
            let kind = r.read_varint()?;
            // anonOptionalNbt: a leading 0x00 tag byte means absent; any other
            // byte is the NBT tag type and the value follows.
            let nbt_bytes = if r.rest().first() == Some(&0) {
                r.read_u8()?;
                Vec::new()
            } else {
                read_nbt_raw(&mut r)?.to_vec()
            };
            PlayEvent::BlockEntityData {
                pos,
                kind,
                nbt_bytes,
            }
        }
        "open_screen" => {
            let window_id = r.read_varint()?;
            let menu_type = r.read_varint()?;
            let title_nbt = read_nbt_raw(&mut r)?.to_vec();
            PlayEvent::OpenScreen {
                window_id,
                menu_type,
                title_nbt,
            }
        }
        "container_set_content" => {
            let window_id = r.read_u8()?;
            let state_id = r.read_varint()?;
            let slot_count = r.read_varint()?;
            PlayEvent::ContainerSetContent {
                window_id,
                state_id,
                slot_count,
                raw_payload: r.rest().to_vec(),
            }
        }
        "container_set_slot" => {
            let window_id = r.read_u8()?;
            let state_id = r.read_varint()?;
            let slot = r.read_i16()?;
            PlayEvent::ContainerSetSlot {
                window_id,
                state_id,
                slot,
                raw_item: r.rest().to_vec(),
            }
        }
        "container_set_data" => {
            let window_id = r.read_u8()?;
            let property = r.read_i16()?;
            let value = r.read_i16()?;
            PlayEvent::ContainerSetData {
                window_id,
                property,
                value,
            }
        }
        "container_close" => {
            let window_id = r.read_u8()?;
            PlayEvent::ContainerClose { window_id }
        }
        "set_experience" => {
            let bar = r.read_f32()?;
            let level = r.read_varint()?;
            let total = r.read_varint()?;
            PlayEvent::SetExperience { bar, level, total }
        }
        "update_mob_effect" => {
            let eid = r.read_varint()?;
            let effect_id = r.read_varint()?;
            let amplifier = r.read_varint()?;
            let duration = r.read_varint()?;
            let flags = r.read_u8()?;
            PlayEvent::UpdateMobEffect {
                eid,
                effect_id,
                amplifier,
                duration,
                flags,
            }
        }
        "remove_mob_effect" => {
            let eid = r.read_varint()?;
            let effect_id = r.read_varint()?;
            PlayEvent::RemoveMobEffect { eid, effect_id }
        }
        "set_action_bar_text" => PlayEvent::SetActionBarText {
            nbt_bytes: read_nbt_raw(&mut r)?.to_vec(),
        },
        "set_title_text" => PlayEvent::SetTitleText {
            nbt_bytes: read_nbt_raw(&mut r)?.to_vec(),
        },
        "set_subtitle_text" => PlayEvent::SetSubtitleText {
            nbt_bytes: read_nbt_raw(&mut r)?.to_vec(),
        },
        "boss_event" => {
            let uuid = r.read_uuid()?;
            let action = r.read_varint()?;
            PlayEvent::BossEvent {
                uuid,
                action,
                raw_payload: r.rest().to_vec(),
            }
        }
        "player_info_remove" => PlayEvent::PlayerInfoRemove {
            uuids: read_uuid_list(&mut r)?,
        },
        "player_info_update" => {
            let actions = r.read_u8()?;
            let count = r.read_varint()?;
            if !(0..=1024).contains(&count) {
                return Err(format!(
                    "player_info_update entry count {count} out of range"
                ));
            }
            let mut entries = Vec::with_capacity(count as usize & 0xff);
            for _ in 0..count {
                let uuid = r.read_uuid()?;
                let mut entry = PlayerInfoUpdateEntry {
                    uuid,
                    name: None,
                    properties: Vec::new(),
                    chat_session: None,
                    game_mode: None,
                    listed: None,
                    latency: None,
                    display_name_nbt: None,
                };
                if actions & 0x01 != 0 {
                    entry.name = Some(r.read_string(16)?);
                    let props = r.read_varint()?;
                    if !(0..=16).contains(&props) {
                        return Err(format!("player_info property count {props} out of range"));
                    }
                    for _ in 0..props {
                        let pname = r.read_string(64)?;
                        let pvalue = r.read_string(32767)?;
                        let sig = if r.read_bool()? {
                            Some(r.read_string(1024)?)
                        } else {
                            None
                        };
                        entry.properties.push((pname, pvalue, sig));
                    }
                }
                if actions & 0x02 != 0 && r.read_bool()? {
                    // session id (16) + key expiry (8) + two varint-prefixed
                    // blobs; retain the whole bounded tail for the
                    // signature verifier rather than re-serializing it.
                    let start = body.len() - r.rest().len();
                    let mut tail = r;
                    tail.read_uuid()?;
                    tail.read_i64()?;
                    let key_len = tail.read_varint()?;
                    if !(0..=4096).contains(&key_len) {
                        return Err("player_info key blob out of range".into());
                    }
                    for _ in 0..key_len {
                        tail.read_u8()?;
                    }
                    let sig_len = tail.read_varint()?;
                    if !(0..=4096).contains(&sig_len) {
                        return Err("player_info signature blob out of range".into());
                    }
                    for _ in 0..sig_len {
                        tail.read_u8()?;
                    }
                    let end = body.len() - tail.rest().len();
                    entry.chat_session = Some(body[start..end].to_vec());
                    r = tail;
                }
                if actions & 0x04 != 0 {
                    entry.game_mode = Some(r.read_varint()?);
                }
                if actions & 0x08 != 0 {
                    entry.listed = Some(r.read_bool()?);
                }
                if actions & 0x10 != 0 {
                    entry.latency = Some(r.read_varint()?);
                }
                if actions & 0x20 != 0 && r.read_bool()? {
                    entry.display_name_nbt = Some(read_nbt_raw(&mut r)?.to_vec());
                }
                entries.push(entry);
            }
            PlayEvent::PlayerInfo { actions, entries }
        }
        "set_objective" => {
            let objective = r.read_string(16)?;
            let mode = r.read_i8()?;
            let (display_name_nbt, render_type, number_format_kind, number_format_nbt) =
                if mode == 0 || mode == 2 {
                    let display = read_nbt_raw(&mut r)?.to_vec();
                    let render = r.read_varint()?;
                    let (kind, nbt) = if r.read_bool()? {
                        let kind = r.read_varint()?;
                        let nbt = if kind == 1 || kind == 2 {
                            Some(read_nbt_raw(&mut r)?.to_vec())
                        } else {
                            None
                        };
                        (Some(kind), nbt)
                    } else {
                        (None, None)
                    };
                    (Some(display), Some(render), kind, nbt)
                } else {
                    (None, None, None, None)
                };
            PlayEvent::SetObjective {
                objective,
                mode,
                display_name_nbt,
                render_type,
                number_format_kind,
                number_format_nbt,
            }
        }
        "set_score" => {
            let owner = r.read_string(32767)?;
            let objective = r.read_string(32767)?;
            let value = r.read_varint()?;
            let display_nbt = if r.read_bool()? {
                Some(read_nbt_raw(&mut r)?.to_vec())
            } else {
                None
            };
            let number_format_kind = if r.read_bool()? {
                let kind = r.read_varint()?;
                if kind == 1 || kind == 2 {
                    let _ = read_nbt_raw(&mut r)?;
                }
                Some(kind)
            } else {
                None
            };
            PlayEvent::SetScore {
                owner,
                objective,
                value,
                display_nbt,
                number_format_kind,
            }
        }
        "set_display_objective" => PlayEvent::SetDisplayObjective {
            slot: r.read_varint()?,
            objective: r.read_string(16)?,
        },
        "reset_score" => {
            let owner = r.read_string(32767)?;
            let objective = if r.read_bool()? {
                Some(r.read_string(32767)?)
            } else {
                None
            };
            PlayEvent::ResetScore { owner, objective }
        }
        "set_titles_animation" => PlayEvent::SetTitle {
            fade_in: r.read_i32()?,
            stay: r.read_i32()?,
            fade_out: r.read_i32()?,
        },
        "pong_response" => PlayEvent::PongResponse { id: r.read_i64()? },
        "initialize_border" => PlayEvent::InitializeBorder {
            x: r.read_f64()?,
            z: r.read_f64()?,
            old_diameter: r.read_f64()?,
            new_diameter: r.read_f64()?,
            lerp_ms: r.read_varlong()?,
            portal_boundary: r.read_varint()?,
            warning_blocks: r.read_varint()?,
            warning_time: r.read_varint()?,
        },
        "set_border_center" => PlayEvent::SetBorderCenter {
            x: r.read_f64()?,
            z: r.read_f64()?,
        },
        "set_border_size" => PlayEvent::SetBorderSize {
            diameter: r.read_f64()?,
        },
        "set_border_lerp_size" => PlayEvent::SetBorderLerpSize {
            old_diameter: r.read_f64()?,
            new_diameter: r.read_f64()?,
            lerp_ms: r.read_varlong()?,
        },
        "set_border_warning_delay" => PlayEvent::SetBorderWarningDelay {
            warning_time: r.read_varint()?,
        },
        "set_border_warning_distance" => PlayEvent::SetBorderWarningDistance {
            warning_blocks: r.read_varint()?,
        },
        "animate" => PlayEvent::Animate {
            eid: r.read_varint()?,
            animation: r.read_u8()?,
        },
        "block_event" => PlayEvent::BlockEvent {
            pos: r.read_position()?,
            action: r.read_u8()?,
            parameter: r.read_u8()?,
            block_type: r.read_varint()?,
        },
        "set_default_spawn_position" => PlayEvent::SetDefaultSpawnPosition {
            pos: r.read_position()?,
            angle: r.read_f32()?,
        },
        "set_chunk_cache_radius" => PlayEvent::SetChunkCacheRadius(r.read_varint()?),
        "set_simulation_distance" => PlayEvent::SetSimulationDistance(r.read_varint()?),
        "ticking_state" => PlayEvent::TickingState {
            tick_rate: r.read_f32()?,
            frozen: r.read_bool()?,
        },
        "ticking_step" => {
            let steps = r.read_varint()?;
            if steps < 0 {
                return Err("negative ticking step count".into());
            }
            PlayEvent::TickingStep { steps }
        }
        "move_vehicle" => PlayEvent::MoveVehicle {
            x: r.read_f64()?,
            y: r.read_f64()?,
            z: r.read_f64()?,
            yaw: r.read_f32()?,
            pitch: r.read_f32()?,
        },
        "set_passengers" => {
            let vehicle = r.read_varint()?;
            let count = usize::try_from(r.read_varint()?)
                .map_err(|_| "negative passenger count".to_string())?;
            if count > r.remaining() {
                return Err("passenger count exceeds remaining input".into());
            }
            let mut passengers = Vec::with_capacity(count.min(256));
            for _ in 0..count {
                passengers.push(r.read_varint()?);
            }
            PlayEvent::SetPassengers {
                vehicle,
                passengers,
            }
        }
        "set_entity_link" => PlayEvent::SetEntityLink {
            source: r.read_varint()?,
            destination: r.read_varint()?,
        },
        "take_item_entity" => PlayEvent::TakeItemEntity {
            item_entity: r.read_varint()?,
            player: r.read_varint()?,
            amount: r.read_varint()?,
        },
        "set_camera" => PlayEvent::SetCamera {
            eid: r.read_varint()?,
        },
        "open_book" => {
            let hand = r.read_varint()?;
            if !(0..=1).contains(&hand) {
                return Err("invalid hand in open_book".into());
            }
            PlayEvent::OpenBook { hand: hand as u8 }
        }
        "open_sign_editor" => PlayEvent::OpenSignEditor {
            pos: r.read_position()?,
            front_side: r.read_bool()?,
        },
        "tab_list" => PlayEvent::TabList {
            header_nbt: read_nbt_raw(&mut r)?.to_vec(),
            footer_nbt: read_nbt_raw(&mut r)?.to_vec(),
        },
        "update_attributes" => {
            let eid = r.read_varint()?;
            let count = usize::try_from(r.read_varint()?)
                .map_err(|_| "negative attribute count".to_string())?;
            if count > r.remaining() / 9 {
                return Err("attribute count exceeds remaining input".into());
            }
            let mut attributes = Vec::with_capacity(count.min(128));
            for _ in 0..count {
                let attribute = r.read_varint()?;
                let base = r.read_f64()?;
                let modifier_count = usize::try_from(r.read_varint()?)
                    .map_err(|_| "negative attribute modifier count".to_string())?;
                if modifier_count > r.remaining() / 25 {
                    return Err("attribute modifier count exceeds remaining input".into());
                }
                let mut modifiers = Vec::with_capacity(modifier_count.min(64));
                for _ in 0..modifier_count {
                    let uuid = r.read_uuid()?;
                    let amount = r.read_f64()?;
                    let operation = r.read_varint()?;
                    if !(0..=2).contains(&operation) {
                        return Err("invalid attribute modifier operation".into());
                    }
                    modifiers.push(WireAttributeModifier {
                        uuid,
                        amount,
                        operation,
                    });
                }
                attributes.push(WireAttribute {
                    attribute,
                    base,
                    modifiers,
                });
            }
            PlayEvent::UpdateAttributes { eid, attributes }
        }
        "command_suggestions" => {
            let transaction_id = r.read_varint()?;
            let start = r.read_varint()?;
            let length = r.read_varint()?;
            let count = usize::try_from(r.read_varint()?)
                .map_err(|_| "negative command suggestion count".to_string())?;
            if count > r.remaining() / 2 {
                return Err("command suggestion count exceeds remaining input".into());
            }
            let mut matches = Vec::with_capacity(count.min(1024));
            for _ in 0..count {
                let text = r.read_string(32767)?;
                let tooltip_nbt = if r.read_bool()? {
                    Some(read_nbt_raw(&mut r)?.to_vec())
                } else {
                    None
                };
                matches.push(WireSuggestion { text, tooltip_nbt });
            }
            PlayEvent::CommandSuggestions {
                transaction_id,
                start,
                length,
                matches,
            }
        }
        "ping" => PlayEvent::Ping(r.read_i32()?),
        "cookie_request" => PlayEvent::CookieRequest(r.read_string(32767)?),
        "place_ghost_recipe" => PlayEvent::PlaceGhostRecipe {
            window_id: r.read_u8()?,
            recipe: r.read_string(32767)?,
        },
        "tag_query" => {
            let transaction_id = r.read_varint()?;
            let nbt_bytes = if r.rest().first() == Some(&0) {
                r.read_u8()?;
                Vec::new()
            } else {
                read_nbt_raw(&mut r)?.to_vec()
            };
            PlayEvent::TagQuery {
                transaction_id,
                nbt_bytes,
            }
        }
        "set_projectile_power" => PlayEvent::SetProjectilePower {
            eid: r.read_varint()?,
            acceleration: r.read_f64()?,
        },
        "custom_report_details" => {
            let count = usize::try_from(r.read_varint()?)
                .map_err(|_| "negative report detail count".to_string())?;
            if count > r.remaining() / 2 {
                return Err("report detail count exceeds remaining input".into());
            }
            let mut details = Vec::with_capacity(count.min(64));
            for _ in 0..count {
                details.push((r.read_string(64)?, r.read_string(1024)?));
            }
            PlayEvent::CustomReportDetails(details)
        }
        "server_links" => {
            let count = usize::try_from(r.read_varint()?)
                .map_err(|_| "negative server link count".to_string())?;
            if count > r.remaining() / 2 {
                return Err("server link count exceeds remaining input".into());
            }
            let mut links = Vec::with_capacity(count.min(64));
            for _ in 0..count {
                let (label, custom_label_nbt) = match r.read_u8()? {
                    0 => (None, Some(read_nbt_raw(&mut r)?.to_vec())),
                    1 => (Some(r.read_varint()?), None),
                    _ => return Err("invalid server link label kind".into()),
                };
                let url = r.read_string(2048)?;
                links.push(WireServerLink {
                    label,
                    custom_label_nbt,
                    url,
                });
            }
            PlayEvent::ServerLinks(links)
        }
        "block_changed_ack" => PlayEvent::BlockChangedAck(r.read_varint()?),
        "change_difficulty" => PlayEvent::ChangeDifficulty { difficulty: r.read_u8()?, locked: r.read_bool()? },
        "clear_titles" => PlayEvent::ClearTitles,
        "cooldown" => PlayEvent::Cooldown { item: r.read_varint()?, ticks: r.read_varint()? },
        "player_abilities" => PlayEvent::PlayerAbilities {
            flags: r.read_u8()?,
            flying_speed: r.read_f32()?,
            walking_speed: r.read_f32()?,
        },
        "player_combat_end" => PlayEvent::PlayerCombatEnd { duration: r.read_varint()? },
        "player_combat_enter" => PlayEvent::PlayerCombatEnter,
        "player_combat_kill" => PlayEvent::PlayerCombatKill {
            player_id: r.read_varint()?,
            message_nbt: read_nbt_raw(&mut r)?.to_vec(),
        },
        "resource_pack_pop" => PlayEvent::ResourcePackPop {
            uuid: if r.read_bool()? { Some(r.read_uuid()?) } else { None },
        },
        "resource_pack_push" => PlayEvent::ResourcePackPush {
            uuid: r.read_uuid()?,
            url: r.read_string(32767)?,
            hash: r.read_string(40)?,
            forced: r.read_bool()?,
            prompt_nbt: if r.read_bool()? { Some(read_nbt_raw(&mut r)?.to_vec()) } else { None },
        },
        "set_carried_item" => PlayEvent::SetCarriedItem(i16::from(r.read_i8()?)),
        "store_cookie" => {
            let key = r.read_string(32767)?;
            let len = usize::try_from(r.read_varint()?)
                .map_err(|_| "negative cookie payload length".to_string())?;
            if len > 5120 { return Err("cookie payload exceeds 5 KiB".into()); }
            PlayEvent::StoreCookie { key, payload: r.take(len)?.to_vec() }
        }
        "custom_chat_completions" => {
            let action = r.read_varint()?;
            if !(0..=2).contains(&action) {
                return Err("invalid chat completions action".into());
            }
            let count = usize::try_from(r.read_varint()?)
                .map_err(|_| "negative chat completion count".to_string())?;
            if count > r.remaining() {
                return Err("chat completion count exceeds remaining input".into());
            }
            let mut entries = Vec::with_capacity(count.min(1024));
            for _ in 0..count {
                entries.push(r.read_string(32767)?);
            }
            PlayEvent::ChatCompletions { action, entries }
        }
        "reset_chat" => PlayEvent::ResetChat,
        "delete_chat" => {
            let encoded_index = r.read_varint()?;
            if encoded_index < 0 {
                return Err("negative delete-chat index".into());
            }
            if encoded_index == 0 {
                let signature = r.take(256)?.try_into().unwrap();
                PlayEvent::DeleteChat {
                    message_index: None,
                    signature: Some(signature),
                }
            } else {
                PlayEvent::DeleteChat {
                    message_index: Some(encoded_index - 1),
                    signature: None,
                }
            }
        }
        "disguised_chat" => {
            let nbt_bytes = read_nbt_raw(&mut r)?.to_vec();
            let text = WireReader::new(&nbt_bytes)
                .read_nbt()
                .map(|v| v.extract_text())
                .unwrap_or_default();
            let _chat_type = r.read_varint()?;
            let _sender_name = read_nbt_raw(&mut r)?;
            if r.read_bool()? {
                let _target_name = read_nbt_raw(&mut r)?;
            }
            PlayEvent::DisguisedChat { text, nbt_bytes }
        }
        "player_chat" => {
            let sender = r.read_uuid()?;
            let index = r.read_varint()?;
            let signature = if r.read_bool()? {
                Some(r.take(256)?.try_into().unwrap())
            } else {
                None
            };
            let message = r.read_string(32767)?;
            let _timestamp = r.read_i64()?;
            let _salt = r.read_i64()?;
            let _last_seen_offset = r.read_varint()?;
            r.take(3)?; // fixed 20-bit acknowledgement bitset
            if r.read_bool()? {
                let _unsigned_content = read_nbt_raw(&mut r)?;
            }
            let filter_type = r.read_varint()?;
            match filter_type {
                0 | 1 => {}
                2 => {
                    let _filter = r.read_bitset(64)?;
                }
                _ => return Err("invalid chat filter type".into()),
            }
            let _chat_type = r.read_varint()?;
            let sender_name = read_nbt_raw(&mut r)?;
            let mut nbt_bytes = sender_name.to_vec();
            if r.read_bool()? {
                nbt_bytes.extend_from_slice(read_nbt_raw(&mut r)?);
            }
            PlayEvent::PlayerChat {
                sender,
                index,
                signature,
                text: message,
                nbt_bytes,
            }
        }
        "set_player_team" => {
            let name = r.read_string(32767)?;
            let mode = r.read_u8()?;
            if mode > 4 {
                return Err("invalid team mode".into());
            }
            let info = if mode == 0 || mode == 2 {
                Some(Box::new(PlayerTeamInfo {
                    display_name_nbt: read_nbt_raw(&mut r)?.to_vec(),
                    friendly_flags: r.read_u8()?,
                    nametag_visibility: r.read_string(64)?,
                    collision_rule: r.read_string(64)?,
                    color: r.read_varint()?,
                    prefix_nbt: read_nbt_raw(&mut r)?.to_vec(),
                    suffix_nbt: read_nbt_raw(&mut r)?.to_vec(),
                }))
            } else {
                None
            };
            let entities = if matches!(mode, 0 | 3 | 4) {
                let count = usize::try_from(r.read_varint()?)
                    .map_err(|_| "negative entity count".to_string())?;
                // Each entity entry is at least a one-byte length prefix.
                if count > r.remaining() {
                    return Err("entity count exceeds remaining input".into());
                }
                let mut list = Vec::with_capacity(count.min(64));
                for _ in 0..count {
                    list.push(r.read_string(32767)?);
                }
                list
            } else {
                Vec::new()
            };
            PlayEvent::SetPlayerTeam {
                name,
                mode,
                info,
                entities,
            }
        }
        "merchant_offers" => {
            let window_id = r.read_varint()?;
            let count = usize::try_from(r.read_varint()?)
                .map_err(|_| "negative trade count".to_string())?;
            // Each offer needs at least ~30 bytes (two costs + slot + two
            // flags + six i32 fields); reject impossible counts up front.
            if count > r.remaining() / 30 {
                return Err("trade count exceeds remaining input".into());
            }
            let mut offers = Vec::with_capacity(count.min(64));
            for _ in 0..count {
                let cost_a = read_merchant_item_cost(&mut r)?;
                let result_slot_wire = read_slot_wire(&mut r)?;
                let cost_b = if r.read_bool()? {
                    Some(read_merchant_item_cost(&mut r)?)
                } else {
                    None
                };
                let disabled = r.read_bool()?;
                let uses = r.read_i32()?;
                let max_uses = r.read_i32()?;
                let xp = r.read_i32()?;
                let special_price = r.read_i32()?;
                let price_multiplier = r.read_f32()?;
                let demand = r.read_i32()?;
                offers.push(MerchantOfferEntry {
                    cost_a,
                    result_slot_wire,
                    cost_b,
                    disabled,
                    uses,
                    max_uses,
                    xp,
                    special_price,
                    price_multiplier,
                    demand,
                });
            }
            let villager_level = r.read_varint()?;
            let villager_xp = r.read_varint()?;
            let show_progress = r.read_bool()?;
            let can_restock = r.read_bool()?;
            PlayEvent::MerchantOffers {
                window_id,
                offers,
                villager_level,
                villager_xp,
                show_progress,
                can_restock,
            }
        }
        "horse_screen_open" => PlayEvent::HorseScreenOpen {
            window_id: r.read_u8()?,
            slots: r.read_varint()?,
            entity_id: r.read_i32()?,
        },
        "map_item_data" => {
            let map_id = r.read_varint()?;
            let scale = r.read_i8()?;
            let locked = r.read_bool()?;
            let icons = if r.read_bool()? {
                let count = usize::try_from(r.read_varint()?)
                    .map_err(|_| "negative icon count".to_string())?;
                // Each icon is at least five bytes (type, x, z, direction,
                // name flag).
                if count > r.remaining() / 5 {
                    return Err("icon count exceeds remaining input".into());
                }
                let mut list = Vec::with_capacity(count.min(64));
                for _ in 0..count {
                    let kind = r.read_varint()?;
                    let x = r.read_i8()?;
                    let z = r.read_i8()?;
                    let direction = r.read_u8()?;
                    let name_nbt = if r.read_bool()? {
                        Some(read_nbt_raw(&mut r)?.to_vec())
                    } else {
                        None
                    };
                    list.push(MapItemIcon {
                        kind,
                        x,
                        z,
                        direction,
                        name_nbt,
                    });
                }
                Some(list)
            } else {
                None
            };
            let columns = r.read_u8()?;
            let (rows, x, y, colors) = if columns == 0 {
                (0, 0, 0, Vec::new())
            } else {
                let rows = r.read_u8()?;
                let x = r.read_u8()?;
                let y = r.read_u8()?;
                let len = usize::try_from(r.read_varint()?)
                    .map_err(|_| "negative color count".to_string())?;
                // Vanilla always sends exactly columns * rows pixels.
                if len != usize::from(columns) * usize::from(rows) {
                    return Err("map color count mismatch".into());
                }
                let colors = r.take(len)?.to_vec();
                (rows, x, y, colors)
            };
            PlayEvent::MapItemData {
                map_id,
                scale,
                locked,
                icons,
                columns,
                rows,
                x,
                y,
                colors,
            }
        }
        "player_look_at" => {
            let anchor = r.read_varint()?;
            if !(0..=1).contains(&anchor) {
                return Err("invalid look-at anchor".into());
            }
            let target = [r.read_f64()?, r.read_f64()?, r.read_f64()?];
            let entity = if r.read_bool()? {
                let entity_id = r.read_varint()?;
                let entity_anchor = r.read_varint()?;
                if !(0..=1).contains(&entity_anchor) {
                    return Err("invalid entity look-at anchor".into());
                }
                Some((entity_id, entity_anchor))
            } else {
                None
            };
            PlayEvent::PlayerLookAt {
                anchor,
                target,
                entity,
            }
        }
        "update_advancements" => {
            let reset = r.read_bool()?;
            let count = usize::try_from(r.read_varint()?)
                .map_err(|_| "negative advancement count".to_string())?;
            // Each entry needs at least a key string, two option flags, a
            // requirements count and the telemetry byte.
            if count > r.remaining() / 5 {
                return Err("advancement count exceeds remaining input".into());
            }
            let mut advancements = Vec::with_capacity(count.min(64));
            for _ in 0..count {
                advancements.push(read_wire_advancement(&mut r)?);
            }
            let nremoved = usize::try_from(r.read_varint()?)
                .map_err(|_| "negative removed count".to_string())?;
            if nremoved > r.remaining() {
                return Err("removed count exceeds remaining input".into());
            }
            let mut removed = Vec::with_capacity(nremoved.min(64));
            for _ in 0..nremoved {
                removed.push(r.read_string(32767)?);
            }
            let nprogress = usize::try_from(r.read_varint()?)
                .map_err(|_| "negative progress count".to_string())?;
            if nprogress > r.remaining() / 2 {
                return Err("progress count exceeds remaining input".into());
            }
            let mut progress = Vec::with_capacity(nprogress.min(64));
            for _ in 0..nprogress {
                let key = r.read_string(32767)?;
                let ncrit = usize::try_from(r.read_varint()?)
                    .map_err(|_| "negative criterion count".to_string())?;
                if ncrit > r.remaining() {
                    return Err("criterion count exceeds remaining input".into());
                }
                let mut criteria = Vec::with_capacity(ncrit.min(64));
                for _ in 0..ncrit {
                    let criterion = r.read_string(32767)?;
                    let achieved = if r.read_bool()? {
                        Some(r.read_i64()?)
                    } else {
                        None
                    };
                    criteria.push((criterion, achieved));
                }
                progress.push(WireAdvancementProgress { key, criteria });
            }
            PlayEvent::UpdateAdvancements {
                reset,
                advancements,
                removed,
                progress,
            }
        }
        "select_advancements_tab" => PlayEvent::SelectAdvancementsTab {
            tab: if r.read_bool()? {
                Some(r.read_string(256)?)
            } else {
                None
            },
        },
        "award_stats" => {
            let count =
                usize::try_from(r.read_varint()?).map_err(|_| "negative stat count".to_string())?;
            // Each entry is three varints — at least three bytes.
            if count > r.remaining() / 3 {
                return Err("stat count exceeds remaining input".into());
            }
            let mut entries = Vec::with_capacity(count.min(256));
            for _ in 0..count {
                entries.push(AwardStatEntry {
                    category: r.read_varint()?,
                    statistic: r.read_varint()?,
                    value: r.read_varint()?,
                });
            }
            PlayEvent::AwardStats { entries }
        }
        "server_data" => {
            let motd_nbt = read_nbt_raw(&mut r)?.to_vec();
            let icon = if r.read_bool()? {
                let len = usize::try_from(r.read_varint()?)
                    .map_err(|_| "negative icon length".to_string())?;
                Some(r.take(len)?.to_vec())
            } else {
                None
            };
            PlayEvent::ServerData { motd_nbt, icon }
        }
        "chunks_biomes" => {
            let count = usize::try_from(r.read_varint()?)
                .map_err(|_| "negative chunk count".to_string())?;
            // Each entry is at least nine bytes (8-byte packed pos + a
            // one-byte data length).
            if count > r.remaining() / 9 {
                return Err("chunk count exceeds remaining input".into());
            }
            let mut chunks = Vec::with_capacity(count.min(256));
            for _ in 0..count {
                // ChunkPos wire order is Z then X (big-endian i64 layout).
                let cz = r.read_i32()?;
                let cx = r.read_i32()?;
                let len = usize::try_from(r.read_varint()?)
                    .map_err(|_| "negative biome data length".to_string())?;
                let data = r.take(len)?.to_vec();
                chunks.push(ChunkBiomeData { cx, cz, data });
            }
            PlayEvent::ChunksBiomes { chunks }
        }
        "transfer" => PlayEvent::Transfer {
            host: r.read_string(255)?,
            port: r.read_varint()?,
        },
        _ => return Ok(None),
    }))
}

// ---------------------------------------------------------------------------
// Serverbound (C2S) gameplay encoder helpers
// ---------------------------------------------------------------------------
//
// Each helper returns `(packet_name, payload)`: the canonical 767 packet name
// that `megu_proto`'s table resolves to the version-correct packet id at
// send time (via `ProtocolClient::send_named` / `c2s_id`), plus the encoded
// body. Names come from the Protocol 767 table — never hardcoded ids.

/// `swing` — arm swing animation. `hand`: 0 = main hand, 1 = off hand.
pub fn encode_swing(hand: i32) -> (&'static str, Vec<u8>) {
    let mut p = Vec::with_capacity(6);
    write_varint(&mut p, hand);
    ("swing", p)
}

/// `use_item_on` — use item on a block face. `face` is the direction id
/// (0..=5); `cursor` is the hit position inside the block (`[x, y, z]`,
/// 0..=1 each); `sequence` is the acknowledged-use sequence.
pub fn encode_use_item_on(
    hand: i32,
    pos: [i32; 3],
    face: i32,
    cursor: [f32; 3],
    inside: bool,
    sequence: i32,
) -> (&'static str, Vec<u8>) {
    let mut p = Vec::with_capacity(32);
    write_varint(&mut p, hand);
    p.extend_from_slice(&pack_position(pos).to_be_bytes());
    write_varint(&mut p, face);
    p.extend_from_slice(&cursor[0].to_be_bytes());
    p.extend_from_slice(&cursor[1].to_be_bytes());
    p.extend_from_slice(&cursor[2].to_be_bytes());
    p.push(u8::from(inside));
    write_varint(&mut p, sequence);
    ("use_item_on", p)
}

/// `client_command` — the play-phase server-bound command the death
/// screen rides on (C2S `0x09` on 767): action `0` = `PERFORM_RESPAWN`,
/// action `1` = `REQUEST_STATS`. The body is a single VarInt action.
pub fn encode_client_command(action: i32) -> (&'static str, Vec<u8>) {
    let mut p = Vec::with_capacity(5);
    write_varint(&mut p, action);
    ("client_command", p)
}

/// `use_item` — use item in air. `yaw`/`pitch` are the player look angles.
pub fn encode_use_item(hand: i32, sequence: i32, yaw: f32, pitch: f32) -> (&'static str, Vec<u8>) {
    let mut p = Vec::with_capacity(15);
    write_varint(&mut p, hand);
    write_varint(&mut p, sequence);
    p.extend_from_slice(&yaw.to_be_bytes());
    p.extend_from_slice(&pitch.to_be_bytes());
    ("use_item", p)
}

/// `player_action` — start/abort/finish digging or item use at `pos` on `face`
/// (wire i8). `status` is the player-action id (0 = start digging, ...).
pub fn encode_player_action(
    status: i32,
    pos: [i32; 3],
    face: i8,
    sequence: i32,
) -> (&'static str, Vec<u8>) {
    let mut p = Vec::with_capacity(18);
    write_varint(&mut p, status);
    p.extend_from_slice(&pack_position(pos).to_be_bytes());
    p.push(face as u8);
    write_varint(&mut p, sequence);
    ("player_action", p)
}

/// `pick_item` — pick-block / pick-entity-from-hotbar slot request.
pub fn encode_pick_item(slot: i32) -> (&'static str, Vec<u8>) {
    let mut p = Vec::with_capacity(5);
    write_varint(&mut p, slot);
    ("pick_item", p)
}

/// `interact` with `mouse` = 1 — attack the target entity.
pub fn encode_interact_attack(target: i32, sneaking: bool) -> (&'static str, Vec<u8>) {
    let mut p = Vec::with_capacity(8);
    write_varint(&mut p, target);
    write_varint(&mut p, 1); // mouse: attack
    p.push(u8::from(sneaking));
    ("interact", p)
}

/// `interact` with `mouse` = 0 — interact (right-click) the target entity
/// with `hand`.
pub fn encode_interact_use(target: i32, hand: i32, sneaking: bool) -> (&'static str, Vec<u8>) {
    let mut p = Vec::with_capacity(9);
    write_varint(&mut p, target);
    write_varint(&mut p, 0); // mouse: interact
    write_varint(&mut p, hand);
    p.push(u8::from(sneaking));
    ("interact", p)
}

/// `interact` with `mouse` = 2 — interact-at the target entity at the
/// exact crosshair hit position `at` (absolute world coordinates) with
/// `hand`.
pub fn encode_interact_at(
    target: i32,
    at: [f32; 3],
    hand: i32,
    sneaking: bool,
) -> (&'static str, Vec<u8>) {
    let mut p = Vec::with_capacity(21);
    write_varint(&mut p, target);
    write_varint(&mut p, 2); // mouse: interact_at
    p.extend_from_slice(&at[0].to_be_bytes());
    p.extend_from_slice(&at[1].to_be_bytes());
    p.extend_from_slice(&at[2].to_be_bytes());
    write_varint(&mut p, hand);
    p.push(u8::from(sneaking));
    ("interact", p)
}

/// Unified `interact` encoder: `action` 0 = interact (right-click,
/// `hand`), 1 = attack, 2 = interact-at (`at` = absolute hit position,
/// `hand`). Actions 1/2 keep `hand`/`at` unused per the vanilla layout.
pub fn encode_interact(
    target: i32,
    action: i32,
    at: Option<[f32; 3]>,
    hand: i32,
    sneaking: bool,
) -> (&'static str, Vec<u8>) {
    match action {
        1 => encode_interact_attack(target, sneaking),
        2 => encode_interact_at(target, at.unwrap_or([0.0; 3]), hand, sneaking),
        _ => encode_interact_use(target, hand, sneaking),
    }
}

/// `set_carried_item` — held hotbar slot (0..=8) echo so the server can
/// attribute dig/attack/use packets to the right stack.
pub fn encode_set_carried_item(slot: i32) -> (&'static str, Vec<u8>) {
    ("set_carried_item", (slot as i16).to_be_bytes().to_vec())
}

/// `player_command` — entity action ids (vanilla
/// `ServerboundPlayerCommandPacket`): 0 start-sneak, 1 stop-sneak,
/// 2 leave-bed, 3 start-sprint, 4 stop-sprint, 5 start-horse-jump,
/// 6 stop-horse-jump, 7 open-vehicle-inventory, 8 start-flying-elytra.
/// `jump_boost` rides the horse-jump actions (`0` elsewhere).
pub fn encode_player_command(
    entity_id: i32,
    action: i32,
    jump_boost: i32,
) -> (&'static str, Vec<u8>) {
    let mut p = Vec::with_capacity(15);
    write_varint(&mut p, entity_id);
    write_varint(&mut p, action);
    write_varint(&mut p, jump_boost);
    ("player_command", p)
}

/// Encode one minimal `Slot` payload for [`encode_container_click`]: an empty
/// slot (`count = 0`) or a component-free stack (`count`, `item_id`).
pub fn encode_slot_stack(item_id: i32, count: i32) -> Vec<u8> {
    let mut p = Vec::with_capacity(8);
    write_varint(&mut p, count);
    if count > 0 {
        write_varint(&mut p, item_id);
        write_varint(&mut p, 0); // added components
        write_varint(&mut p, 0); // removed components
    }
    p
}

/// `container_click` — inventory/menu click. `changed_slots` pairs a window
/// slot index (i16) with its raw `Slot` wire bytes (see [`encode_slot_stack`]
/// for simple stacks); `cursor_item` is the carried-item `Slot` payload.
pub fn encode_container_click(
    window_id: u8,
    state_id: i32,
    slot: i16,
    mouse_button: i8,
    mode: i32,
    changed_slots: &[(i16, &[u8])],
    cursor_item: &[u8],
) -> (&'static str, Vec<u8>) {
    let mut p = Vec::with_capacity(64);
    p.push(window_id);
    write_varint(&mut p, state_id);
    p.extend_from_slice(&slot.to_be_bytes());
    p.push(mouse_button as u8);
    write_varint(&mut p, mode);
    write_varint(&mut p, changed_slots.len() as i32);
    for (location, item) in changed_slots {
        p.extend_from_slice(&location.to_be_bytes());
        p.extend_from_slice(item);
    }
    p.extend_from_slice(cursor_item);
    ("container_click", p)
}

/// `container_button_click` (a.k.a. `enchant_item`) — enchant-table-style
/// button press. `window_id` is a `ContainerID` (u8); `button_id` is a VarInt.
pub fn encode_container_button_click(window_id: u8, button_id: i32) -> (&'static str, Vec<u8>) {
    let mut p = Vec::with_capacity(10);
    p.push(window_id);
    write_varint(&mut p, button_id);
    ("container_button_click", p)
}

/// `container_close` — close the open container window.
pub fn encode_container_close(window_id: u8) -> (&'static str, Vec<u8>) {
    ("container_close", vec![window_id])
}

/// `chat_command` — unsigned chat command (no leading `/`).
pub fn encode_chat_command(command: &str) -> (&'static str, Vec<u8>) {
    let mut p = Vec::new();
    write_string(&mut p, command);
    ("chat_command", p)
}

/// `command_suggestion` — play-phase tab-completion request: a
/// monotonically increasing `transaction_id` plus the full input text
/// (vanilla sends the buffer verbatim, `/` included).
pub fn encode_command_suggestion(transaction_id: i32, text: &str) -> (&'static str, Vec<u8>) {
    let mut p = Vec::with_capacity(8 + text.len());
    write_varint(&mut p, transaction_id);
    write_string(&mut p, text);
    ("command_suggestion", p)
}

/// `chat` — unsigned chat message. Produces the unsigned envelope: message,
/// client timestamp (unix millis), zero salt, no signature, offset 0, and an
/// empty acknowledged bitset. Servers that require signed chat will reject
/// this form; signed envelopes need the session's signature chain.
pub fn encode_chat_message(message: &str) -> (&'static str, Vec<u8>) {
    let timestamp_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    let mut p = Vec::with_capacity(64 + message.len());
    write_string(&mut p, message);
    p.extend_from_slice(&timestamp_ms.to_be_bytes());
    p.extend_from_slice(&0i64.to_be_bytes()); // salt
    p.push(0); // signature: absent
    write_varint(&mut p, 0); // offset / last seen index
    p.extend_from_slice(&[0, 0, 0]); // acknowledged bitset
    ("chat", p)
}

/// `ping_request` — play-phase latency probe; the server mirrors the i64
/// back on `pong_response` (S2C 0x36).
pub fn encode_ping_request(id: i64) -> (&'static str, Vec<u8>) {
    ("ping_request", id.to_be_bytes().to_vec())
}

/// `pong` — answers the clientbound play `ping` (S2C 0x35) with its i32 id.
pub fn encode_pong(id: i32) -> (&'static str, Vec<u8>) {
    ("pong", id.to_be_bytes().to_vec())
}

/// `player_abilities` — flags byte (bit 0x02 flying); vanilla 767 carries
/// only the flags byte on this packet.
pub fn encode_player_abilities(flags: u8) -> (&'static str, Vec<u8>) {
    ("player_abilities", vec![flags])
}

/// `resource_pack` action code: the pack's contents were applied.
pub const RESOURCE_PACK_SUCCESSFULLY_LOADED: i32 = 0;
/// `resource_pack` action code: the player declined the pack.
pub const RESOURCE_PACK_DECLINED: i32 = 1;
/// `resource_pack` action code: the download failed.
pub const RESOURCE_PACK_FAILED_DOWNLOAD: i32 = 2;
/// `resource_pack` action code: the player accepted the offer.
pub const RESOURCE_PACK_ACCEPTED: i32 = 3;
/// `resource_pack` action code: the download finished.
pub const RESOURCE_PACK_DOWNLOADED: i32 = 4;
/// `resource_pack` action code: the pushed URL was invalid.
pub const RESOURCE_PACK_INVALID_URL: i32 = 5;
/// `resource_pack` action code: applying the downloaded pack failed.
pub const RESOURCE_PACK_FAILED_RELOAD: i32 = 6;
/// `resource_pack` action code: the pack was discarded (pop echo).
pub const RESOURCE_PACK_DISCARDED: i32 = 7;

/// `resource_pack` — serverbound status answer for a `resource_pack_push`:
/// the pack UUID followed by the vanilla action varint (1.20.3+ shape for
/// both 767 and 777). The configuration-phase sibling carries the same
/// payload under a different table name on 767 (`resource_pack_response`).
pub fn encode_resource_pack(uuid: [u8; 16], status: i32) -> (&'static str, Vec<u8>) {
    let mut p = Vec::with_capacity(21);
    p.extend_from_slice(&uuid);
    write_varint(&mut p, status);
    ("resource_pack", p)
}

/// `edit_book` — writable-book page edits. `slot` is the inventory slot
/// holding the book (vanilla bounds 0..=40); `pages` are capped at 100
/// entries of 1024 chars; `title` is present only for signed books.
pub fn encode_edit_book(slot: i32, pages: &[&str], title: Option<&str>) -> (&'static str, Vec<u8>) {
    let mut p = Vec::with_capacity(16 + pages.iter().map(|s| s.len() + 5).sum::<usize>());
    write_varint(&mut p, slot);
    write_varint(&mut p, pages.len() as i32);
    for page in pages {
        write_string(&mut p, page);
    }
    match title {
        Some(t) => {
            p.push(1);
            write_string(&mut p, t);
        }
        None => p.push(0),
    }
    ("edit_book", p)
}

/// `set_command_block` — command-block editor update: packed `BlockPos`,
/// command string, mode (`0` sequence, `1` auto, `2` redstone) and the
/// track-output/conditional/automatic flag bits (0x01/0x02/0x04).
pub fn encode_set_command_block(
    pos: [i32; 3],
    command: &str,
    mode: i32,
    track_output: bool,
    conditional: bool,
    automatic: bool,
) -> (&'static str, Vec<u8>) {
    let mut p = Vec::with_capacity(16 + command.len());
    p.extend_from_slice(&pack_position(pos).to_be_bytes());
    write_string(&mut p, command);
    write_varint(&mut p, mode);
    p.push((u8::from(track_output)) | (u8::from(conditional) << 1) | (u8::from(automatic) << 2));
    ("set_command_block", p)
}

/// `set_command_minecart` — command-minecart update: entity id, command
/// string, track-output flag.
pub fn encode_set_command_minecart(
    entity_id: i32,
    command: &str,
    track_output: bool,
) -> (&'static str, Vec<u8>) {
    let mut p = Vec::with_capacity(8 + command.len());
    write_varint(&mut p, entity_id);
    write_string(&mut p, command);
    p.push(u8::from(track_output));
    ("set_command_minecart", p)
}

/// `set_beacon` — beacon effect selection: primary/secondary mob-effect
/// registry ids, each `None` for "no effect".
pub fn encode_set_beacon(primary: Option<i32>, secondary: Option<i32>) -> (&'static str, Vec<u8>) {
    let mut p = Vec::with_capacity(10);
    for effect in [primary, secondary] {
        match effect {
            Some(id) => {
                p.push(1);
                write_varint(&mut p, id);
            }
            None => p.push(0),
        }
    }
    ("set_beacon", p)
}

/// `select_trade` — pick a villager trade index in the open merchant menu.
pub fn encode_select_trade(index: i32) -> (&'static str, Vec<u8>) {
    let mut p = Vec::with_capacity(5);
    write_varint(&mut p, index);
    ("select_trade", p)
}

/// `place_recipe` — recipe-book "place recipe" request: `window_id` is the
/// open container id, `recipe` the recipe identifier, `make_all` requests
/// the shift-click (fill-all) variant.
pub fn encode_place_recipe(window_id: u8, recipe: &str, make_all: bool) -> (&'static str, Vec<u8>) {
    let mut p = Vec::with_capacity(8 + recipe.len());
    p.push(window_id);
    write_string(&mut p, recipe);
    p.push(u8::from(make_all));
    ("place_recipe", p)
}

/// `recipe_book_change_settings` — recipe-book category (`book_type`),
/// open state and active-filter flags.
pub fn encode_recipe_book_change_settings(
    book_type: i32,
    book_open: bool,
    filter_active: bool,
) -> (&'static str, Vec<u8>) {
    let mut p = Vec::with_capacity(6);
    write_varint(&mut p, book_type);
    p.push(u8::from(book_open));
    p.push(u8::from(filter_active));
    ("recipe_book_change_settings", p)
}

/// `recipe_book_seen_recipe` — mark one recipe id as seen in the book.
pub fn encode_recipe_book_seen_recipe(recipe: &str) -> (&'static str, Vec<u8>) {
    let mut p = Vec::new();
    write_string(&mut p, recipe);
    ("recipe_book_seen_recipe", p)
}

/// `rename_item` — anvil rename text (vanilla caps at 150 chars).
pub fn encode_rename_item(name: &str) -> (&'static str, Vec<u8>) {
    let mut p = Vec::new();
    write_string(&mut p, name);
    ("rename_item", p)
}

/// `container_slot_state_changed` — crafter-grid slot toggle: slot index,
/// window id, new disabled state (vanilla only honours slots 0..9).
pub fn encode_container_slot_state_changed(
    slot_id: i32,
    window_id: i32,
    state: bool,
) -> (&'static str, Vec<u8>) {
    let mut p = Vec::with_capacity(10);
    write_varint(&mut p, slot_id);
    write_varint(&mut p, window_id);
    p.push(u8::from(state));
    ("container_slot_state_changed", p)
}

/// `seen_advancements` — advancements screen opened on `tab` (when `Some`)
/// or closed (`None`).
pub fn encode_seen_advancements(tab: Option<&str>) -> (&'static str, Vec<u8>) {
    let mut p = Vec::with_capacity(8);
    match tab {
        Some(id) => {
            write_varint(&mut p, 0);
            write_string(&mut p, id);
        }
        None => write_varint(&mut p, 1),
    }
    ("seen_advancements", p)
}

/// `teleport_to_entity` — spectator `/spectate`-style request carrying the
/// bare 16-byte target entity UUID.
pub fn encode_teleport_to_entity(uuid: [u8; 16]) -> (&'static str, Vec<u8>) {
    ("teleport_to_entity", uuid.to_vec())
}

/// `lock_difficulty` — OP-level difficulty lock flag.
pub fn encode_lock_difficulty(locked: bool) -> (&'static str, Vec<u8>) {
    ("lock_difficulty", vec![u8::from(locked)])
}

// ---------------------------------------------------------------------------
// TCP Protocol 767 Client
// ---------------------------------------------------------------------------

pub struct HandshakeReport {
    pub login_ok: bool,
    pub config_ok: bool,
    pub compression_threshold: Option<usize>,
    pub uuid: [u8; 16],
    pub server_username: String,
    pub biome_count: usize,
}

pub struct ProtocolClient {
    stream: TcpStream,
    compression: Option<usize>,
    read_buf: Vec<u8>,
    /// Version-selected packet table (`ProtocolVersion::V1_21_1` or
    /// `ProtocolVersion::V26_3`) — every packet id below resolves through it
    /// by phase/direction/name instead of a hardcoded hex literal.
    codec: Arc<dyn ProtocolCodec>,
    dimension_types_by_id: HashMap<i32, (i32, i32, bool)>,
    dimension_types_by_name: HashMap<String, (i32, i32, bool)>,
    /// Upper bound on global block-state ids for `level_chunk_with_light`
    /// decoding (`megu_kernel::STATE_COUNT` with the pinned assets);
    /// `sim` reseeds it from `WorldState::state_count` so `--version-dir`
    /// packs bound the global palette correctly.
    pub block_state_count: usize,
    pub dimension: DimensionMeta,
    pub biome_count: usize,
    /// The protocol version this client offered in its handshake
    /// (`V1_21_1`/`767` by default, `V26_3`/`777` via `--mc-version`).
    pub protocol_version: megu_proto::ProtocolVersion,
    pub packets_rx: u64,
    pub packets_tx: u64,
    /// Decoded `sound_effect`/`sound_entity` payloads, retained bounded for
    /// non-audio consumers (drained by the audio runtime when present).
    pub sound_events: VecDeque<WireSoundEvent>,
    /// Decoded `stop_sound` payloads, same retention policy.
    pub stop_sound_events: VecDeque<WireStopSound>,
    /// Decoded `system_chat` NBT payloads, retained bounded for consumers
    /// needing the raw component (see [`WireChatEvent`]).
    pub chat_events: VecDeque<WireChatEvent>,
    /// Plain text from incoming system/player/disguised chat, retained for a
    /// future chat surface. The client currently has no in-game chat UI.
    pub chat_log: VecDeque<ChatLogEntry>,
    pub tick_rate: f32,
    pub is_frozen: bool,
    pub ticking_steps: u64,
    pub requested_view_distance: Option<i32>,
    pub simulation_distance: Option<i32>,
    pub default_spawn_position: Option<([i32; 3], f32)>,
    pub border_warning_delay: Option<i32>,
    pub border_warning_distance: Option<i32>,
    pub move_vehicle_pose: Option<([f64; 3], [f32; 2])>,
    pub command_completions: Vec<String>,
    pub latest_command_suggestions: Option<PlayEvent>,
    /// Bounded events for UI surfaces that are not implemented yet.
    pub pending_ui_events: VecDeque<PlayEvent>,
    pub cookies: HashMap<String, Vec<u8>>,
    pub difficulty: Option<(u8, bool)>,
    pub player_abilities: Option<(u8, f32, f32)>,
    pub selected_hotbar_slot: Option<i16>,
    pub item_cooldowns: HashMap<i32, i32>,
    /// Live audio output pipeline — installed from the session audio
    /// config when one is configured (see
    /// [`crate::audio_out::install_session_audio_config`]); `None` when
    /// audio is off or unconfigured.
    pub audio: Option<crate::audio_out::AudioRuntime>,
    /// Online-mode login inputs (`--accessToken`, session-join hook, CSPRNG
    /// override); consumed only if the server sends an Encryption Request.
    online_auth: OnlineAuthOptions,
    /// AES-128-CFB8 encryptor armed on every outbound byte once the
    /// Encryption Response has been sent (online-mode login).
    cipher_tx: Option<Aes128Cfb8>,
    /// AES-128-CFB8 decryptor armed on every inbound byte once the
    /// Encryption Response has been sent.
    cipher_rx: Option<Aes128Cfb8>,
}

impl ProtocolClient {
    /// Wrap an already-connected stream at `protocol_version`: the shared
    /// constructor behind `connect_resolved` (Login path, `auth` carries the
    /// online-mode credentials) and `ping_server_resolved` (Status path,
    /// always offline).
    fn on_stream(
        stream: TcpStream,
        codec: Arc<dyn ProtocolCodec>,
        protocol_version: megu_proto::ProtocolVersion,
        auth: OnlineAuthOptions,
    ) -> Self {
        Self {
            stream,
            compression: None,
            read_buf: Vec::new(),
            codec,
            dimension_types_by_id: HashMap::new(),
            dimension_types_by_name: HashMap::new(),
            block_state_count: megu_kernel::STATE_COUNT as usize,
            dimension: DimensionMeta::default(),
            biome_count: 64,
            protocol_version,
            packets_rx: 0,
            packets_tx: 0,
            sound_events: VecDeque::new(),
            stop_sound_events: VecDeque::new(),
            chat_events: VecDeque::new(),
            chat_log: VecDeque::new(),
            tick_rate: 20.0,
            is_frozen: false,
            ticking_steps: 0,
            requested_view_distance: None,
            simulation_distance: None,
            default_spawn_position: None,
            border_warning_delay: None,
            border_warning_distance: None,
            move_vehicle_pose: None,
            command_completions: Vec::new(),
            latest_command_suggestions: None,
            pending_ui_events: VecDeque::new(),
            cookies: HashMap::new(),
            difficulty: None,
            player_abilities: None,
            selected_hotbar_slot: None,
            item_cooldowns: HashMap::new(),
            audio: crate::audio_out::session_audio_config()
                .filter(crate::audio_out::AudioRuntime::enabled)
                .map(|cfg| crate::audio_out::AudioRuntime::new(&cfg)),
            online_auth: auth,
            cipher_tx: None,
            cipher_rx: None,
        }
    }

    /// True when the selected table carries the 26.x wire profile
    /// (`RecipeBookDisplays` is the proto's 26.x-generation marker): the
    /// post-1.21.5 login/play shapes — Game-Profile login success with a
    /// session id, VarInt game modes + `online_mode` in spawn info, echoed
    /// `accept_teleportation`, Int teleport flags with velocity fields, the
    /// heightmap record array, and the `player_input` flags byte.
    fn wire_26x(&self) -> bool {
        matches!(
            self.codec.capabilities().recipe_sync_mode,
            RecipeSyncMode::RecipeBookDisplays
        )
    }

    /// Resolve an inbound `pid` to its protocol-table name for the active
    /// version — packet ids differ between 767 and 777 (e.g. play
    /// `keep_alive` is `0x26` vs `0x2d`), so dispatch goes through names.
    fn s2c_name(&self, phase: PacketPhase, pid: i32) -> Option<String> {
        let id = u32::try_from(pid).ok()?;
        self.codec
            .table()
            .packet_name(phase, PacketDir::Clientbound, id)
            .map(str::to_string)
    }

    /// Resolve an outbound packet name to its id for the active version.
    fn c2s_id(&self, phase: PacketPhase, name: &'static str) -> Result<i32, String> {
        self.codec
            .table()
            .packet_id(phase, PacketDir::Serverbound, name)
            .map(|id| id as i32)
            .ok_or_else(|| {
                format!(
                    "protocol table for {:?} lacks c2s {phase:?}/{name}",
                    self.protocol_version
                )
            })
    }

    fn send_named(
        &mut self,
        phase: PacketPhase,
        name: &'static str,
        payload: &[u8],
    ) -> Result<(), String> {
        let pid = self.c2s_id(phase, name)?;
        self.send_raw(pid, payload)
    }

    /// Connect, handshake, and ride through Login + Configuration to Play.
    ///
    /// Returns `(Self, HandshakeReport)` with the negotiated profile on
    /// success, or a descriptive error on any wire/parse failure.
    pub fn connect(
        host: &str,
        port: u16,
        username: &str,
        uuid: [u8; 16],
        view_distance: i8,
        timeout: Duration,
    ) -> Result<(Self, HandshakeReport), String> {
        Self::connect_for_version(
            host,
            port,
            username,
            uuid,
            view_distance,
            timeout,
            megu_proto::ProtocolVersion::V1_21_1,
        )
    }

    /// `connect` at an explicit `ProtocolVersion`: the live `_minecraft._tcp`
    /// SRV lookup resolves the socket address while the Handshake still
    /// carries the original `host`/`port` — the vanilla
    /// `ServerNameResolver` rule.
    pub fn connect_for_version(
        host: &str,
        port: u16,
        username: &str,
        uuid: [u8; 16],
        view_distance: i8,
        timeout: Duration,
        protocol_version: megu_proto::ProtocolVersion,
    ) -> Result<(Self, HandshakeReport), String> {
        Self::connect_resolved(
            host,
            port,
            username,
            uuid,
            view_distance,
            timeout,
            protocol_version,
            |h, p| megu_proto::resolve_server_address(h, p).map_err(|e| e.to_string()),
            OnlineAuthOptions::default(),
        )
    }

    /// `connect_for_version` with explicit online-mode credentials/hooks: the
    /// launcher's access token plus (for tests) an injected session-join client
    /// and deterministic CSPRNG.
    #[allow(clippy::too_many_arguments)]
    pub fn connect_for_version_with_auth(
        host: &str,
        port: u16,
        username: &str,
        uuid: [u8; 16],
        view_distance: i8,
        timeout: Duration,
        protocol_version: megu_proto::ProtocolVersion,
        auth: OnlineAuthOptions,
    ) -> Result<(Self, HandshakeReport), String> {
        Self::connect_resolved(
            host,
            port,
            username,
            uuid,
            view_distance,
            timeout,
            protocol_version,
            |h, p| megu_proto::resolve_server_address(h, p).map_err(|e| e.to_string()),
            auth,
        )
    }

    /// `connect_for_version` with the `_minecraft._tcp` SRV / address lookup
    /// injected, so loopback tests can prove both the SRV redirect and the
    /// original-host-preservation rule without live DNS.
    #[allow(clippy::too_many_arguments)]
    pub fn connect_resolved(
        host: &str,
        port: u16,
        username: &str,
        uuid: [u8; 16],
        view_distance: i8,
        timeout: Duration,
        protocol_version: megu_proto::ProtocolVersion,
        resolve: impl FnOnce(&str, u16) -> Result<Vec<SocketAddr>, String>,
        auth: OnlineAuthOptions,
    ) -> Result<(Self, HandshakeReport), String> {
        let codec = megu_proto::VersionRouter::with_v767_and_v777()
            .select(protocol_version.number())
            .map_err(|e| e.to_string())?;
        let addrs = resolve(host, port)?;
        if addrs.is_empty() {
            return Err(format!(
                "resolve_server_address({host}:{port}) yielded no addresses"
            ));
        }
        let addr = format!("{host}:{port}");
        let stream = TcpStream::connect(addrs.as_slice())
            .map_err(|e| format!("failed to connect to {addr}: {e}"))?;
        let _ = stream.set_nodelay(true);
        let _ = stream.set_read_timeout(Some(timeout));
        let _ = stream.set_write_timeout(Some(timeout));

        let mut client = Self::on_stream(stream, codec, protocol_version, auth);

        // 1. Handshake — original host/port even when the SRV lookup
        //    redirected the TCP target (vanilla ServerNameResolver rule).
        let mut hs = Vec::new();
        write_varint(&mut hs, client.protocol_version.number());
        write_string(&mut hs, host);
        hs.extend_from_slice(&port.to_be_bytes());
        write_varint(&mut hs, 2);
        client.send_named(PacketPhase::Handshake, "handshake", &hs)?;

        // 2. Login Start
        let mut login = Vec::new();
        write_string(&mut login, username);
        login.extend_from_slice(&uuid);
        client.send_named(PacketPhase::Login, "login_start", &login)?;

        // 3. Login loop -> Set Compression / Login Success
        let deadline = Instant::now() + timeout;
        let (server_uuid, server_username) = loop {
            let remaining = deadline
                .checked_duration_since(Instant::now())
                .ok_or_else(|| "login timed out".to_string())?;
            let Some((pid, body)) = client.recv_raw(remaining)? else {
                return Err("login timed out waiting for server".to_string());
            };
            match client.s2c_name(PacketPhase::Login, pid).as_deref() {
                Some("disconnect") | Some("login_disconnect") => {
                    let msg = WireReader::new(&body)
                        .read_string(32_767)
                        .unwrap_or_else(|_| "login disconnect".to_string());
                    return Err(format!("disconnected during login: {msg}"));
                }
                Some("encryption_request") => {
                    client.respond_to_encryption_request(&body)?;
                }
                Some("set_compression") | Some("login_compression") => {
                    let threshold = WireReader::new(&body).read_varint()?;
                    client.compression = usize::try_from(threshold).ok();
                }
                Some("login_success") | Some("login_finished") => {
                    let mut r = WireReader::new(&body);
                    let got_uuid = r.read_uuid()?;
                    let got_name = r.read_string(64)?;
                    if client.wire_26x() {
                        // 26.x Game Profile properties (name, value, optional
                        // signature) then a Session ID UUID.
                        let props = usize::try_from(r.read_varint()?)
                            .map_err(|_| "negative profile property count".to_string())?;
                        for _ in 0..props {
                            let _pname = r.read_string(1024)?;
                            let _pvalue = r.read_string(32_767)?;
                            if r.read_bool()? {
                                let _sig = r.read_string(1024)?;
                            }
                        }
                        let _session_id = r.read_uuid()?;
                    }
                    client.send_named(PacketPhase::Login, "login_acknowledged", &[])?;
                    break (got_uuid, got_name);
                }
                _ => {}
            }
        };

        // 4. Configuration loop
        let mut sent_client_info = false;
        let client_info_payload =
            build_client_information(view_distance, client.codec.capabilities());
        let config_deadline = Instant::now() + timeout;
        loop {
            let remaining = config_deadline
                .checked_duration_since(Instant::now())
                .ok_or_else(|| "configuration timed out".to_string())?;
            let Some((pid, body)) = client.recv_raw(remaining)? else {
                return Err("configuration timed out waiting for server".to_string());
            };
            match client.s2c_name(PacketPhase::Configuration, pid).as_deref() {
                Some("reset_chat") => {
                    client.chat_log.clear();
                }
                // Cookie Request — or the legacy 8-byte keepalive fallback on
                // pid 0x00 in the 767 table.
                Some("cookie_request") => {
                    let mut r = WireReader::new(&body);
                    if let Ok(key) = r.read_string(1024) {
                        if r.remaining() == 0 {
                            let mut resp = Vec::new();
                            write_string(&mut resp, &key);
                            resp.push(0); // has_payload = false
                            client.send_named(
                                PacketPhase::Configuration,
                                "cookie_response",
                                &resp,
                            )?;
                            continue;
                        }
                    }
                    if body.len() == 8 {
                        client.send_named(PacketPhase::Configuration, "keep_alive", &body)?;
                    }
                }
                Some("keep_alive") => {
                    client.send_named(PacketPhase::Configuration, "keep_alive", &body)?;
                }
                Some("ping") => {
                    // Configuration-phase latency probe: echo the i32 id
                    // back on `pong` (same payload, different table entry).
                    client.send_named(PacketPhase::Configuration, "pong", &body)?;
                }
                Some("resource_pack_push") => {
                    // Answer required/optional pushes promptly: the pack is
                    // acknowledged rather than downloaded (see the play-phase
                    // handler in `live_session`), so pack-required servers
                    // never hit their response timeout.
                    if let Ok(uuid) = WireReader::new(&body).read_uuid() {
                        client.send_resource_pack_in(
                            PacketPhase::Configuration,
                            uuid,
                            RESOURCE_PACK_ACCEPTED,
                        )?;
                        client.send_resource_pack_in(
                            PacketPhase::Configuration,
                            uuid,
                            RESOURCE_PACK_SUCCESSFULLY_LOADED,
                        )?;
                    }
                }
                Some("disconnect") => {
                    let msg = WireReader::new(&body)
                        .read_nbt()
                        .map(|v| v.extract_text())
                        .unwrap_or_else(|_| "configuration disconnect".to_string());
                    return Err(format!("disconnected during configuration: {msg}"));
                }
                Some("registry_data") => {
                    client.parse_registry_data(&body)?;
                }
                Some("select_known_packs") => {
                    if !sent_client_info {
                        client.send_named(
                            PacketPhase::Configuration,
                            "client_information",
                            &client_info_payload,
                        )?;
                        sent_client_info = true;
                    }
                    let mut packs = Vec::new();
                    write_varint(&mut packs, 0);
                    client.send_named(PacketPhase::Configuration, "select_known_packs", &packs)?;
                }
                Some("finish_configuration") => {
                    if !sent_client_info {
                        client.send_named(
                            PacketPhase::Configuration,
                            "client_information",
                            &client_info_payload,
                        )?;
                    }
                    client.send_named(PacketPhase::Configuration, "finish_configuration", &[])?;
                    break;
                }
                _ => {}
            }
        }

        let report = HandshakeReport {
            login_ok: true,
            config_ok: true,
            compression_threshold: client.compression,
            uuid: server_uuid,
            server_username,
            biome_count: client.biome_count,
        };
        Ok((client, report))
    }

    fn parse_registry_data(&mut self, body: &[u8]) -> Result<(), String> {
        let mut r = WireReader::new(body);
        let registry_name = r.read_string(256)?;
        let count = usize::try_from(r.read_varint()?)
            .map_err(|_| "negative registry entry count".to_string())?;
        if count > 65_536 {
            return Err("registry entry count exceeds bound".to_string());
        }
        if registry_name == "minecraft:worldgen/biome" {
            self.biome_count = count.max(1);
        }
        if registry_name == "minecraft:dimension_type" {
            self.dimension_types_by_id.clear();
            self.dimension_types_by_name.clear();
        }
        let capture_sounds = registry_name == "minecraft:sound_event";
        let mut sound_names = capture_sounds.then(|| Vec::with_capacity(count));
        for idx in 0..count {
            let entry_name = r.read_string(256)?;
            let has_nbt = r.read_bool()?;
            let entry_nbt = if has_nbt { Some(r.read_nbt()?) } else { None };
            if let Some(names) = &mut sound_names {
                // Wire order is the registry id (id = index); Mod
                // namespaces ride along verbatim — the audio library's
                // layered map keys them as `modid:event`.
                names.push(entry_name.clone());
            }
            if registry_name == "minecraft:dimension_type" {
                let meta = entry_nbt
                    .as_ref()
                    .and_then(|v| match v {
                        NbtValue::Compound(map) => {
                            let min_y = map.get("min_y")?.as_i32()?;
                            let height = map.get("height")?.as_i32()?;
                            let has_skylight = map.get("has_skylight")?.as_bool()?;
                            Some((min_y, height, has_skylight))
                        }
                        _ => None,
                    })
                    .or_else(|| DimensionMeta::builtin(&entry_name));
                if let Some(m) = meta {
                    self.dimension_types_by_id.insert(idx as i32, m);
                    self.dimension_types_by_name.insert(entry_name, m);
                }
            }
        }
        if let Some(names) = sound_names {
            if let Some(rt) = &mut self.audio {
                rt.set_sound_event_registry(crate::sound_assets::SoundEventRegistry::from_names(
                    names,
                ));
            }
        }
        Ok(())
    }

    /// Shared `login`/`respawn` tail: dimension + seed + game modes + flags
    /// + death location + portal cooldown (+ `sea_level` from 1.21.2). The
    ///   26.x profile switches the two game modes to VarInt enums.
    fn parse_spawn_info(&self, r: &mut WireReader<'_>) -> Result<(i32, String), String> {
        let dim_type_id = r.read_varint()?;
        let dim_name = r.read_string(256)?;
        let _hashed_seed = r.read_i64()?;
        if self.wire_26x() {
            let _game_mode = r.read_varint()?;
            let _prev_game_mode = r.read_varint()?; // 0 = undefined, else id+1
        } else {
            let _game_mode = r.read_u8()?;
            let _prev_game_mode = r.read_u8()?;
        }
        let _is_debug = r.read_bool()?;
        let _is_flat = r.read_bool()?;
        if r.read_bool()? {
            let _death_dim = r.read_string(256)?;
            let _death_pos = r.read_i64()?;
        }
        let _portal_cooldown = r.read_varint()?;
        if self.codec.capabilities().relative_movement_flags {
            let _sea_level = r.read_varint()?;
        }
        Ok((dim_type_id, dim_name))
    }

    fn resolve_dimension(&self, dim_type_id: i32, dim_name: String) -> DimensionMeta {
        let (min_y, height, has_skylight) = self
            .dimension_types_by_id
            .get(&dim_type_id)
            .copied()
            .or_else(|| self.dimension_types_by_name.get(&dim_name).copied())
            .or_else(|| DimensionMeta::builtin(&dim_name))
            .unwrap_or((-64, 384, true));
        DimensionMeta {
            name: dim_name,
            min_y,
            height,
            has_skylight,
        }
    }

    /// Answer a clientbound Login `0x01 Encryption Request`: negotiate the
    /// shared secret + Mojang session join through
    /// [`online_auth::plan_encryption_response`], send `0x01 Encryption
    /// Response` still in plaintext, then arm AES-128-CFB8 on both directions
    /// — every following frame (`set_compression`, `login_success`,
    /// configuration, play) rides the cipher exactly like vanilla.
    fn respond_to_encryption_request(&mut self, body: &[u8]) -> Result<(), String> {
        if self.cipher_tx.is_some() || self.cipher_rx.is_some() {
            return Err("duplicate encryption request".to_string());
        }
        let fields = online_auth::decode_encryption_request_body(body)?;
        let mut rng_slot = self.online_auth.rng.take();
        let mut rng = |buf: &mut [u8]| match &mut rng_slot {
            Some(f) => f(buf),
            None => online_auth::fill_os_random(buf),
        };
        let join: Arc<dyn SessionJoinClient> = self
            .online_auth
            .session_join
            .clone()
            .unwrap_or_else(|| Arc::new(online_auth::MojangSessionJoinClient::new()));
        let plan = online_auth::plan_encryption_response(
            &fields,
            self.online_auth.access_token.as_deref(),
            self.online_auth.profile_uuid,
            join.as_ref(),
            &mut rng,
        )?;
        self.send_named(
            PacketPhase::Login,
            "encryption_response",
            &plan.response_body,
        )?;
        self.cipher_tx = Some(Aes128Cfb8::new(&plan.shared_secret, &plan.shared_secret));
        self.cipher_rx = Some(Aes128Cfb8::new(&plan.shared_secret, &plan.shared_secret));
        Ok(())
    }

    pub fn send_raw(&mut self, pid: i32, payload: &[u8]) -> Result<(), String> {
        let mut frame = encode_packet(pid, payload, self.compression);
        if let Some(tx) = &mut self.cipher_tx {
            tx.encrypt(&mut frame);
        }
        self.stream
            .write_all(&frame)
            .map_err(|e| format!("TCP write failed: {e}"))?;
        self.packets_tx += 1;
        Ok(())
    }

    /// Bare `accept_teleportation` (teleport id only) — the 767 wire form.
    /// 26.x requires the echoed resulting pose, so version-aware callers use
    /// `send_accept_teleportation` instead.
    pub fn send_confirm_teleport(&mut self, teleport_id: i32) -> Result<(), String> {
        let mut p = Vec::with_capacity(5);
        write_varint(&mut p, teleport_id);
        self.send_named(PacketPhase::Play, "accept_teleportation", &p)
    }

    /// Confirm a teleport in the active version's wire shape: 767 sends the
    /// bare teleport id; 26.x echoes the resulting pose and, like vanilla,
    /// immediately follows with `move_player_pos_rot`.
    pub fn send_accept_teleportation(
        &mut self,
        teleport_id: i32,
        pos: [f64; 3],
        yaw: f32,
        pitch: f32,
        on_ground: bool,
    ) -> Result<(), String> {
        let mut p = Vec::with_capacity(33);
        write_varint(&mut p, teleport_id);
        self.audio_listener(pos[0], pos[1], pos[2], Some(yaw), Some(pitch));
        if !self.wire_26x() {
            return self.send_named(PacketPhase::Play, "accept_teleportation", &p);
        }
        p.extend_from_slice(&pos[0].to_be_bytes());
        p.extend_from_slice(&pos[1].to_be_bytes());
        p.extend_from_slice(&pos[2].to_be_bytes());
        p.extend_from_slice(&yaw.to_be_bytes());
        p.extend_from_slice(&pitch.to_be_bytes());
        self.send_named(PacketPhase::Play, "accept_teleportation", &p)?;
        self.send_player_position_rotation(pos[0], pos[1], pos[2], yaw, pitch, on_ground)
    }

    pub fn send_chunk_batch_received(&mut self, chunks_per_tick: f32) -> Result<(), String> {
        self.send_named(
            PacketPhase::Play,
            "chunk_batch_received",
            &chunks_per_tick.to_be_bytes(),
        )
    }

    pub fn send_keep_alive(&mut self, id: i64) -> Result<(), String> {
        self.send_named(PacketPhase::Play, "keep_alive", &id.to_be_bytes())
    }

    /// Reply to a Play `cookie_request` with the stored value when present.
    pub fn send_cookie_response(&mut self, key: &str) -> Result<(), String> {
        let mut body = Vec::new();
        write_string(&mut body, key);
        match self.cookies.get(key) {
            Some(payload) => {
                body.push(1);
                write_varint(&mut body, payload.len() as i32);
                body.extend_from_slice(payload);
            }
            None => body.push(0),
        }
        self.send_named(PacketPhase::Play, "cookie_response", &body)
    }

    /// `player_input` in the active version's shape: 767's float impulse +
    /// jump/unmount flag byte; 26.x's single `Input` flag byte
    /// (`caps.player_input_flags`). Flag bits match vanilla `Input`.
    pub fn send_player_input(
        &mut self,
        sideways: f32,
        forward: f32,
        jump: bool,
        unmount: bool,
    ) -> Result<(), String> {
        let mut p = Vec::with_capacity(9);
        if self.codec.capabilities().player_input_flags {
            let mut bits = 0u8;
            if forward > 0.0 {
                bits |= 0x01;
            }
            if forward < 0.0 {
                bits |= 0x02;
            }
            if sideways < 0.0 {
                bits |= 0x04;
            }
            if sideways > 0.0 {
                bits |= 0x08;
            }
            if jump {
                bits |= 0x10;
            }
            if unmount {
                bits |= 0x20;
            }
            p.push(bits);
        } else {
            p.extend_from_slice(&sideways.to_be_bytes());
            p.extend_from_slice(&forward.to_be_bytes());
            p.push(u8::from(jump) | (u8::from(unmount) << 1));
        }
        self.send_named(PacketPhase::Play, "player_input", &p)
    }

    pub fn send_player_position(
        &mut self,
        x: f64,
        y: f64,
        z: f64,
        on_ground: bool,
    ) -> Result<(), String> {
        // 767 ends with a bare on-ground boolean, 26.x with a flag byte whose
        // 0x01 bit is on-ground — the encoding is identical for a client that
        // never reports horizontal collision.
        self.audio_listener(x, y, z, None, None);
        let mut p = Vec::with_capacity(25);
        p.extend_from_slice(&x.to_be_bytes());
        p.extend_from_slice(&y.to_be_bytes());
        p.extend_from_slice(&z.to_be_bytes());
        p.push(u8::from(on_ground));
        self.send_named(PacketPhase::Play, "move_player_pos", &p)
    }

    pub fn send_player_position_rotation(
        &mut self,
        x: f64,
        y: f64,
        z: f64,
        yaw: f32,
        pitch: f32,
        on_ground: bool,
    ) -> Result<(), String> {
        self.audio_listener(x, y, z, Some(yaw), Some(pitch));
        let mut p = Vec::with_capacity(33);
        p.extend_from_slice(&x.to_be_bytes());
        p.extend_from_slice(&y.to_be_bytes());
        p.extend_from_slice(&z.to_be_bytes());
        p.extend_from_slice(&yaw.to_be_bytes());
        p.extend_from_slice(&pitch.to_be_bytes());
        p.push(u8::from(on_ground));
        self.send_named(PacketPhase::Play, "move_player_pos_rot", &p)
    }

    /// `chat`/`chat_command`: vanilla splits slash-commands onto the
    /// unsigned `chat_command` packet (no leading `/`); plain text rides
    /// `chat` (`ClientboundPlayerChatPacket`'s serverbound pair).
    pub fn send_chat_message(&mut self, message: &str) -> Result<(), String> {
        let (name, payload) = if let Some(command) = message.strip_prefix('/') {
            encode_chat_command(command)
        } else {
            encode_chat_message(message)
        };
        self.send_named(PacketPhase::Play, name, &payload)
    }

    /// `command_suggestion` — request server-driven tab completion for
    /// `text` (the full chat-bar buffer, leading `/` included for
    /// commands). The reply arrives as [`PlayEvent::CommandSuggestions`]
    /// carrying the same `transaction_id`; callers bump a session counter
    /// per request so stale replies are detectable.
    pub fn send_command_suggestion(
        &mut self,
        transaction_id: i32,
        text: &str,
    ) -> Result<(), String> {
        let (name, payload) = encode_command_suggestion(transaction_id, text);
        self.send_named(PacketPhase::Play, name, &payload)
    }

    /// `use_item` — use the item in `hand` while looking at `yaw`/`pitch`;
    /// `sequence` tracks held-item slot changes like vanilla's
    /// `ServerboundUseItemPacket`.
    pub fn send_use_item(
        &mut self,
        hand: i32,
        sequence: i32,
        yaw: f32,
        pitch: f32,
    ) -> Result<(), String> {
        let (name, payload) = encode_use_item(hand, sequence, yaw, pitch);
        self.send_named(PacketPhase::Play, name, &payload)
    }

    /// `player_action` — vanilla `ServerboundPlayerActionPacket` statuses:
    /// 0 start-dig, 1 abort-dig, 2 finish-dig, 3 drop-item-stack,
    /// 4 drop-item, 5 release-use-item, 6 swap-item-hands.
    pub fn send_player_action(
        &mut self,
        status: i32,
        pos: [i32; 3],
        face: i8,
        sequence: i32,
    ) -> Result<(), String> {
        let (name, payload) = encode_player_action(status, pos, face, sequence);
        self.send_named(PacketPhase::Play, name, &payload)
    }

    /// `container_click` — one slot-mode/button/drag transaction against
    /// the open window (`encode_container_click` carries the slot blobs;
    /// build simple stacks with [`encode_slot_stack`]).
    #[allow(clippy::too_many_arguments)]
    pub fn send_container_click(
        &mut self,
        window_id: u8,
        state_id: i32,
        slot: i16,
        button: i8,
        mode: i32,
        changed: &[(i16, &[u8])],
        carried: &[u8],
    ) -> Result<(), String> {
        let (name, payload) =
            encode_container_click(window_id, state_id, slot, button, mode, changed, carried);
        self.send_named(PacketPhase::Play, name, &payload)
    }

    /// `container_close` — client-initiated close of `window_id`.
    pub fn send_container_close(&mut self, window_id: u8) -> Result<(), String> {
        let (name, payload) = encode_container_close(window_id);
        self.send_named(PacketPhase::Play, name, &payload)
    }

    /// `container_button_click` — enchant/enchantment-style window button.
    pub fn send_container_button_click(
        &mut self,
        window_id: u8,
        button_id: i32,
    ) -> Result<(), String> {
        let (name, payload) = encode_container_button_click(window_id, button_id);
        self.send_named(PacketPhase::Play, name, &payload)
    }

    /// `swing` — arm-swing animation for `hand` (0 main, 1 off).
    pub fn send_swing(&mut self, hand: i32) -> Result<(), String> {
        let (name, payload) = encode_swing(hand);
        self.send_named(PacketPhase::Play, name, &payload)
    }

    /// `use_item_on` — right-click on block `pos`/`face`.
    pub fn send_use_item_on(
        &mut self,
        hand: i32,
        pos: [i32; 3],
        face: i32,
        hit: [f32; 3],
        inside: bool,
        sequence: i32,
    ) -> Result<(), String> {
        let (name, payload) = encode_use_item_on(hand, pos, face, hit, inside, sequence);
        self.send_named(PacketPhase::Play, name, &payload)
    }

    /// `interact` — entity interaction dispatcher: `action` 1 = attack
    /// (left-click), 0 = interact, 2 = interact-at (`at` = absolute hit
    /// position). `hand` rides the 0/2 actions; `sneaking` is the
    /// secondary-action flag on every variant.
    pub fn send_interact(
        &mut self,
        target: i32,
        action: i32,
        at: Option<[f32; 3]>,
        hand: i32,
        sneaking: bool,
    ) -> Result<(), String> {
        let (name, payload) = encode_interact(target, action, at, hand, sneaking);
        self.send_named(PacketPhase::Play, name, &payload)
    }

    /// `set_carried_item` — held hotbar slot (0..=8) sync after the local
    /// selection changes (`ServerboundSetCarriedItemPacket` short).
    pub fn send_set_carried_item(&mut self, slot: i32) -> Result<(), String> {
        let (name, payload) = encode_set_carried_item(slot);
        self.send_named(PacketPhase::Play, name, &payload)
    }

    /// `player_command` — sneak/sprint/elytra/vehicle entity action for
    /// `entity_id` (`ServerboundPlayerCommandPacket`; `jump_boost` only
    /// rides the horse-jump actions, `0` elsewhere).
    pub fn send_player_command(
        &mut self,
        entity_id: i32,
        action: i32,
        jump_boost: i32,
    ) -> Result<(), String> {
        let (name, payload) = encode_player_command(entity_id, action, jump_boost);
        self.send_named(PacketPhase::Play, name, &payload)
    }

    /// `ping_request` — play-phase latency probe (`pong_response` echoes it).
    pub fn send_ping_request(&mut self, id: i64) -> Result<(), String> {
        let (name, payload) = encode_ping_request(id);
        self.send_named(PacketPhase::Play, name, &payload)
    }

    /// `pong` — answer a clientbound play `ping` (S2C 0x35).
    pub fn send_pong(&mut self, id: i32) -> Result<(), String> {
        let (name, payload) = encode_pong(id);
        self.send_named(PacketPhase::Play, name, &payload)
    }

    /// `resource_pack` — play-phase status answer for a
    /// `resource_pack_push`/`resource_pack_pop` (see
    /// [`encode_resource_pack`] for the action codes and wire shape).
    pub fn send_resource_pack(&mut self, uuid: [u8; 16], status: i32) -> Result<(), String> {
        self.send_resource_pack_in(PacketPhase::Play, uuid, status)
    }

    /// [`send_resource_pack`](Self::send_resource_pack) in an explicit
    /// phase: the configuration loop answers pushed packs with the same
    /// payload, resolved under `resource_pack` on 777 and
    /// `resource_pack_response` on 767.
    pub fn send_resource_pack_in(
        &mut self,
        phase: PacketPhase,
        uuid: [u8; 16],
        status: i32,
    ) -> Result<(), String> {
        let (_, payload) = encode_resource_pack(uuid, status);
        let name = if self
            .codec
            .table()
            .packet_id(phase, PacketDir::Serverbound, "resource_pack")
            .is_some()
        {
            "resource_pack"
        } else {
            "resource_pack_response"
        };
        self.send_named(phase, name, &payload)
    }

    /// `client_information` — play-phase re-announce (render distance,
    /// locale, ...) after the configuration-phase copy; vanilla emits
    /// this whenever a broadcast-visible setting like view distance
    /// changes so the server re-centers the client's chunk view.
    pub fn send_client_information(&mut self, view_distance: i8) -> Result<(), String> {
        let payload = build_client_information(view_distance, self.codec.capabilities());
        self.send_named(PacketPhase::Play, "client_information", &payload)
    }

    /// `player_abilities` — flags byte; only bit 0x02 (flying) is honoured
    /// server-side while `mayfly` is set.
    pub fn send_player_abilities(&mut self, flags: u8) -> Result<(), String> {
        let (name, payload) = encode_player_abilities(flags);
        self.send_named(PacketPhase::Play, name, &payload)
    }

    /// `client_command` — play-phase command varint (`0` = perform
    /// respawn, `1` = request stats); resolves to C2S `0x09` on 767 and
    /// `0x0c` on 777 via the packet table.
    pub fn send_client_command(&mut self, action: i32) -> Result<(), String> {
        let (name, payload) = encode_client_command(action);
        self.send_named(PacketPhase::Play, name, &payload)
    }

    /// Death-screen `Respawn` button: `client_command` with the
    /// `PERFORM_RESPAWN` action (`0`).
    pub fn send_perform_respawn(&mut self) -> Result<(), String> {
        self.send_client_command(0)
    }

    /// `edit_book` — writable-book edits on the stack in `slot`.
    pub fn send_edit_book(
        &mut self,
        slot: i32,
        pages: &[&str],
        title: Option<&str>,
    ) -> Result<(), String> {
        let (name, payload) = encode_edit_book(slot, pages, title);
        self.send_named(PacketPhase::Play, name, &payload)
    }

    /// `set_command_block` — command-block editor update (gamemaster only).
    pub fn send_set_command_block(
        &mut self,
        pos: [i32; 3],
        command: &str,
        mode: i32,
        track_output: bool,
        conditional: bool,
        automatic: bool,
    ) -> Result<(), String> {
        let (name, payload) =
            encode_set_command_block(pos, command, mode, track_output, conditional, automatic);
        self.send_named(PacketPhase::Play, name, &payload)
    }

    /// `set_command_minecart` — command-minecart update (gamemaster only).
    pub fn send_set_command_minecart(
        &mut self,
        entity_id: i32,
        command: &str,
        track_output: bool,
    ) -> Result<(), String> {
        let (name, payload) = encode_set_command_minecart(entity_id, command, track_output);
        self.send_named(PacketPhase::Play, name, &payload)
    }

    /// `set_beacon` — beacon primary/secondary effect selection.
    pub fn send_set_beacon(
        &mut self,
        primary: Option<i32>,
        secondary: Option<i32>,
    ) -> Result<(), String> {
        let (name, payload) = encode_set_beacon(primary, secondary);
        self.send_named(PacketPhase::Play, name, &payload)
    }

    /// `select_trade` — pick villager trade `index` in the open menu.
    pub fn send_select_trade(&mut self, index: i32) -> Result<(), String> {
        let (name, payload) = encode_select_trade(index);
        self.send_named(PacketPhase::Play, name, &payload)
    }

    /// `place_recipe` — recipe-book place-recipe request.
    pub fn send_place_recipe(
        &mut self,
        window_id: u8,
        recipe: &str,
        make_all: bool,
    ) -> Result<(), String> {
        let (name, payload) = encode_place_recipe(window_id, recipe, make_all);
        self.send_named(PacketPhase::Play, name, &payload)
    }

    /// `recipe_book_change_settings` — book type/open/filter flags.
    pub fn send_recipe_book_change_settings(
        &mut self,
        book_type: i32,
        book_open: bool,
        filter_active: bool,
    ) -> Result<(), String> {
        let (name, payload) =
            encode_recipe_book_change_settings(book_type, book_open, filter_active);
        self.send_named(PacketPhase::Play, name, &payload)
    }

    /// `recipe_book_seen_recipe` — mark `recipe` as seen.
    pub fn send_recipe_book_seen_recipe(&mut self, recipe: &str) -> Result<(), String> {
        let (name, payload) = encode_recipe_book_seen_recipe(recipe);
        self.send_named(PacketPhase::Play, name, &payload)
    }

    /// `rename_item` — anvil rename text.
    pub fn send_rename_item(&mut self, name: &str) -> Result<(), String> {
        let (name, payload) = encode_rename_item(name);
        self.send_named(PacketPhase::Play, name, &payload)
    }

    /// `container_slot_state_changed` — crafter-grid slot toggle.
    pub fn send_container_slot_state_changed(
        &mut self,
        slot_id: i32,
        window_id: i32,
        state: bool,
    ) -> Result<(), String> {
        let (name, payload) = encode_container_slot_state_changed(slot_id, window_id, state);
        self.send_named(PacketPhase::Play, name, &payload)
    }

    /// `seen_advancements` — open the advancements tab (`Some`) or close the
    /// screen (`None`).
    pub fn send_seen_advancements(&mut self, tab: Option<&str>) -> Result<(), String> {
        let (name, payload) = encode_seen_advancements(tab);
        self.send_named(PacketPhase::Play, name, &payload)
    }

    /// `teleport_to_entity` — spectate-target request by entity UUID.
    pub fn send_teleport_to_entity(&mut self, uuid: [u8; 16]) -> Result<(), String> {
        let (name, payload) = encode_teleport_to_entity(uuid);
        self.send_named(PacketPhase::Play, name, &payload)
    }

    /// `lock_difficulty` — difficulty-lock flag (server enforces OP>=2).
    pub fn send_lock_difficulty(&mut self, locked: bool) -> Result<(), String> {
        let (name, payload) = encode_lock_difficulty(locked);
        self.send_named(PacketPhase::Play, name, &payload)
    }

    pub fn recv_play(&mut self, timeout: Duration) -> Result<Option<PlayEvent>, String> {
        let Some((pid, body)) = self.recv_raw(timeout)? else {
            self.pump_audio();
            return Ok(None);
        };
        self.pump_audio();
        let event = self
            .decode_play_event(pid, &body)
            .map_err(|err| format!("play packet 0x{pid:02x}: {err}"))?;
        self.record_play_state(&event);
        Ok(Some(event))
    }

    /// Opt-in receiver for custom channels alongside ordinary play packets.
    /// The established `recv_play` API retains its original event behavior.
    pub fn recv_play_with_custom_channels(
        &mut self,
        timeout: Duration,
    ) -> Result<Option<ClientPlayEvent>, String> {
        let Some((pid, body)) = self.recv_raw(timeout)? else {
            self.pump_audio();
            return Ok(None);
        };
        self.pump_audio();
        if self.s2c_name(PacketPhase::Play, pid).as_deref() == Some("custom_payload") {
            return decode_custom_payload_packet(&body)
                .map(ClientPlayEvent::CustomChannel)
                .map(Some);
        }
        let event = self
            .decode_play_event(pid, &body)
            .map_err(|err| format!("play packet 0x{pid:02x}: {err}"))?;
        self.record_play_state(&event);
        Ok(Some(ClientPlayEvent::Play(event)))
    }

    fn record_play_state(&mut self, event: &PlayEvent) {
        match event {
            PlayEvent::SystemChat { text, .. } | PlayEvent::DisguisedChat { text, .. } => {
                if self.chat_log.len() >= 256 {
                    self.chat_log.pop_front();
                }
                self.chat_log.push_back(ChatLogEntry {
                    index: None,
                    signature: None,
                    text: text.clone(),
                });
            }
            PlayEvent::PlayerChat {
                index,
                signature,
                text,
                ..
            } => {
                if self.chat_log.len() >= 256 {
                    self.chat_log.pop_front();
                }
                self.chat_log.push_back(ChatLogEntry {
                    index: Some(*index),
                    signature: *signature,
                    text: text.clone(),
                });
            }
            PlayEvent::TickingState { tick_rate, frozen } => {
                self.tick_rate = *tick_rate;
                self.is_frozen = *frozen;
            }
            PlayEvent::TickingStep { steps } => {
                self.ticking_steps = self.ticking_steps.saturating_add(*steps as u64);
            }
            PlayEvent::SetChunkCacheRadius(radius) => self.requested_view_distance = Some(*radius),
            PlayEvent::SetSimulationDistance(distance) => {
                self.simulation_distance = Some(*distance)
            }
            PlayEvent::SetDefaultSpawnPosition { pos, angle } => {
                self.default_spawn_position = Some((*pos, *angle));
            }
            PlayEvent::SetBorderWarningDelay { warning_time } => {
                self.border_warning_delay = Some(*warning_time);
            }
            PlayEvent::SetBorderWarningDistance { warning_blocks } => {
                self.border_warning_distance = Some(*warning_blocks);
            }
            PlayEvent::MoveVehicle {
                x,
                y,
                z,
                yaw,
                pitch,
            } => {
                self.move_vehicle_pose = Some(([*x, *y, *z], [*yaw, *pitch]));
            }
            PlayEvent::ChatCompletions { action, entries } => match action {
                0 => {
                    for entry in entries {
                        if self.command_completions.len() >= 1024 {
                            break;
                        }
                        if !self.command_completions.contains(entry) {
                            self.command_completions.push(entry.clone());
                        }
                    }
                }
                1 => self
                    .command_completions
                    .retain(|entry| !entries.contains(entry)),
                2 => self.command_completions.clone_from(entries),
                _ => {}
            },
            PlayEvent::CommandSuggestions { .. } => {
                self.latest_command_suggestions = Some(event.clone());
            }
            PlayEvent::StoreCookie { key, payload } => {
                if payload.len() <= 5120 && (self.cookies.contains_key(key) || self.cookies.len() < 256) {
                    self.cookies.insert(key.clone(), payload.clone());
                }
            }
            PlayEvent::ChangeDifficulty { difficulty, locked } => {
                self.difficulty = Some((*difficulty, *locked));
            }
            PlayEvent::PlayerAbilities { flags, flying_speed, walking_speed } => {
                self.player_abilities = Some((*flags, *flying_speed, *walking_speed));
            }
            PlayEvent::SetCarriedItem(slot) => self.selected_hotbar_slot = Some(*slot),
            PlayEvent::Cooldown { item, ticks } => {
                if self.item_cooldowns.contains_key(item) || self.item_cooldowns.len() < 512 {
                    self.item_cooldowns.insert(*item, *ticks);
                }
            }
            PlayEvent::ResetChat => self.chat_log.clear(),
            PlayEvent::DeleteChat {
                message_index,
                signature,
            } => {
                if let Some(index) = message_index {
                    self.chat_log.retain(|entry| entry.index != Some(*index));
                } else if let Some(signature) = signature {
                    self.chat_log
                        .retain(|entry| entry.signature.as_ref() != Some(signature));
                }
            }
            PlayEvent::SetCamera { .. }
            | PlayEvent::OpenBook { .. }
            | PlayEvent::OpenSignEditor { .. }
            | PlayEvent::TabList { .. }
            | PlayEvent::PlaceGhostRecipe { .. }
            | PlayEvent::TagQuery { .. }
            | PlayEvent::SetProjectilePower { .. }
            | PlayEvent::CustomReportDetails(_)
            | PlayEvent::ServerLinks(_)
            | PlayEvent::BlockChangedAck(_)
            | PlayEvent::ClearTitles
            | PlayEvent::PlayerCombatEnd { .. }
            | PlayEvent::PlayerCombatEnter
            | PlayEvent::PlayerCombatKill { .. }
            | PlayEvent::ResourcePackPop { .. }
            | PlayEvent::ResourcePackPush { .. }
            | PlayEvent::PlayerInfoRemove { .. }
            | PlayEvent::PlayerLookAt { .. } => {
                if self.pending_ui_events.len() == 128 {
                    self.pending_ui_events.pop_front();
                }
                self.pending_ui_events.push_back(event.clone());
            }
            _ => {}
        }
    }

    /// Retain one decoded sound event bounded for non-audio consumers and
    /// feed the live audio runtime when one is installed.
    fn ingest_sound_event(&mut self, ev: WireSoundEvent) {
        if self.sound_events.len() >= crate::audio_out::pending_wire_event_bound() {
            self.sound_events.pop_front();
        }
        if let Some(rt) = &mut self.audio {
            rt.ingest(&ev);
        }
        self.sound_events.push_back(ev);
    }

    fn ingest_stop_sound(&mut self, s: WireStopSound) {
        if self.stop_sound_events.len() >= crate::audio_out::pending_wire_event_bound() {
            self.stop_sound_events.pop_front();
        }
        if let Some(rt) = &mut self.audio {
            rt.ingest_stop(&s);
        }
        self.stop_sound_events.push_back(s);
    }

    /// Retain one decoded `system_chat` NBT payload bounded for consumers
    /// needing the raw component.
    fn ingest_chat_event(&mut self, ev: WireChatEvent) {
        if self.chat_events.len() >= crate::audio_out::pending_wire_event_bound() {
            self.chat_events.pop_front();
        }
        self.chat_events.push_back(ev);
    }

    /// Render whatever audio is due; cheap when nothing is installed or
    /// no wall-clock time has passed.
    fn pump_audio(&mut self) {
        if let Some(rt) = &mut self.audio {
            rt.pump();
        }
    }

    /// Listener pose update from the position packets the session runner
    /// already sends every tick — feet position; `None` look angles keep
    /// the last seen values (position-only packets carry none).
    fn audio_listener(&mut self, x: f64, y: f64, z: f64, yaw: Option<f32>, pitch: Option<f32>) {
        if let Some(rt) = &mut self.audio {
            rt.update_listener([x, y, z], yaw, pitch);
        }
    }

    fn decode_play_event(&mut self, pid: i32, body: &[u8]) -> Result<PlayEvent, String> {
        match self.s2c_name(PacketPhase::Play, pid).as_deref() {
            Some("login") => {
                let mut r = WireReader::new(body);
                let entity_id = r.read_i32()?;
                let is_hardcore = r.read_bool()?;
                let dim_count = usize::try_from(r.read_varint()?)
                    .map_err(|_| "negative dimension count".to_string())?;
                for _ in 0..dim_count {
                    let _ = r.read_string(256)?;
                }
                let _max_players = r.read_varint()?;
                let _view_distance = r.read_varint()?;
                let _sim_distance = r.read_varint()?;
                let _reduced_debug = r.read_bool()?;
                let _respawn_screen = r.read_bool()?;
                let _limited_crafting = r.read_bool()?;
                let (dim_type_id, dim_name) = self.parse_spawn_info(&mut r)?;
                if self.wire_26x() {
                    let _online_mode = r.read_bool()?;
                }
                let _enforces_secure_chat = r.read_bool()?;
                let dimension = self.resolve_dimension(dim_type_id, dim_name);
                self.dimension = dimension.clone();
                Ok(PlayEvent::JoinGame {
                    entity_id,
                    dimension,
                    hardcore: is_hardcore,
                })
            }
            Some("respawn") => {
                let mut r = WireReader::new(body);
                let (dim_type_id, dim_name) = self.parse_spawn_info(&mut r)?;
                let _data_kept = r.read_u8()?;
                let dimension = self.resolve_dimension(dim_type_id, dim_name);
                self.dimension = dimension.clone();
                Ok(PlayEvent::Respawn { dimension })
            }
            Some("keep_alive") => {
                let id = WireReader::new(body).read_i64()?;
                Ok(PlayEvent::KeepAlive(id))
            }
            Some("player_position") => {
                let mut r = WireReader::new(body);
                let (teleport_id, x, y, z, yaw, pitch, flags) = if self.wire_26x() {
                    // 26.x: teleport id first, then position/velocity doubles,
                    // yaw/pitch and an Int relative-flag bitmask.
                    let teleport_id = r.read_varint()?;
                    let x = r.read_f64()?;
                    let y = r.read_f64()?;
                    let z = r.read_f64()?;
                    let _vx = r.read_f64()?;
                    let _vy = r.read_f64()?;
                    let _vz = r.read_f64()?;
                    let yaw = r.read_f32()?;
                    let pitch = r.read_f32()?;
                    let flags = r.read_i32()?;
                    (teleport_id, x, y, z, yaw, pitch, flags)
                } else {
                    let x = r.read_f64()?;
                    let y = r.read_f64()?;
                    let z = r.read_f64()?;
                    let yaw = r.read_f32()?;
                    let pitch = r.read_f32()?;
                    let flags = i32::from(r.read_u8()?);
                    let teleport_id = r.read_varint()?;
                    (teleport_id, x, y, z, yaw, pitch, flags)
                };
                Ok(PlayEvent::Teleport(TeleportSync {
                    x,
                    y,
                    z,
                    yaw,
                    pitch,
                    flags,
                    teleport_id,
                }))
            }
            Some("chunk_batch_start") => Ok(PlayEvent::ChunkBatchStart),
            Some("level_chunk_with_light") => {
                let chunk = decode_chunk_packet_profiled(
                    body,
                    self.dimension.section_count(),
                    self.biome_count,
                    ChunkDecodeProfile {
                        state_count: self.block_state_count,
                        heightmaps_are_records: self.wire_26x(),
                        byte_array_light_masks: self.wire_26x(),
                    },
                )?;
                Ok(PlayEvent::ChunkData(Box::new(chunk)))
            }
            Some("chunk_batch_finished") => {
                let batch_size = WireReader::new(body).read_varint()?;
                Ok(PlayEvent::ChunkBatchFinished(batch_size))
            }
            Some("light_update") => {
                let (cx, cz, light) = decode_update_light_packet(
                    body,
                    self.dimension.section_count(),
                    self.wire_26x(),
                )?;
                Ok(PlayEvent::UpdateLight { cx, cz, light })
            }
            Some("block_update") => {
                let mut r = WireReader::new(body);
                let pos = r.read_position()?;
                let state_id = u32::try_from(r.read_varint()?)
                    .map_err(|_| "negative block state id".to_string())?;
                Ok(PlayEvent::BlockUpdate { pos, state_id })
            }
            Some("section_blocks_update") => {
                let updates = decode_section_blocks_update(body)?;
                Ok(PlayEvent::SectionBlocksUpdate(updates))
            }
            Some("forget_level_chunk") => {
                let mut r = WireReader::new(body);
                // ChunkPos is a big-endian i64: cz in high 32 bits, cx in low 32 bits.
                let cz = r.read_i32()?;
                let cx = r.read_i32()?;
                Ok(PlayEvent::UnloadChunk { cx, cz })
            }
            Some("set_chunk_cache_center") => {
                let mut r = WireReader::new(body);
                let cx = r.read_varint()?;
                let cz = r.read_varint()?;
                Ok(PlayEvent::SetCenterChunk { cx, cz })
            }
            Some("set_health") => {
                let mut r = WireReader::new(body);
                let health = r.read_f32()?;
                let food = r.read_varint()?;
                let saturation = r.read_f32()?;
                Ok(PlayEvent::SetHealth {
                    health,
                    food,
                    saturation,
                })
            }
            Some("system_chat") => {
                let mut r = WireReader::new(body);
                // Retain the raw anonymous-NBT component for consumers that
                // need styled/translatable content; the PlayEvent keeps the
                // extracted plain text (frozen downstream shape).
                let nbt_bytes = read_nbt_raw(&mut r).map(|b| b.to_vec()).unwrap_or_default();
                let overlay = r.read_bool()?;
                let text = if nbt_bytes.is_empty() {
                    String::new()
                } else {
                    WireReader::new(&nbt_bytes)
                        .read_nbt()
                        .map(|v| v.extract_text())
                        .unwrap_or_default()
                };
                if !nbt_bytes.is_empty() {
                    self.ingest_chat_event(WireChatEvent { nbt_bytes, overlay });
                }
                Ok(PlayEvent::SystemChat { text, overlay })
            }
            Some("disconnect") => {
                let msg = WireReader::new(body)
                    .read_nbt()
                    .map(|v| v.extract_text())
                    .unwrap_or_else(|_| "disconnected by server".to_string());
                Ok(PlayEvent::Disconnect(msg))
            }
            Some("sound_effect") | Some("sound_entity") => {
                // 767 layout only: on the 26.x wire these packets carry a
                // different shape, so they stay undecoded `Other`s rather
                // than mis-parsed audio.
                if !self.wire_26x() {
                    if let Ok(ev) = decode_play_sound_body(
                        body,
                        self.s2c_name(PacketPhase::Play, pid).as_deref() == Some("sound_entity"),
                    ) {
                        self.ingest_sound_event(ev);
                    }
                }
                Ok(PlayEvent::Other(pid))
            }
            Some("stop_sound") => {
                if !self.wire_26x() {
                    if let Ok(s) = decode_stop_sound_body(body) {
                        self.ingest_stop_sound(s);
                    }
                }
                Ok(PlayEvent::Other(pid))
            }
            // Extended 767 play families (entities, world, containers, HUD).
            // On the 26.x wire these packets carry different shapes, so they
            // stay undecoded `Other`s rather than mis-parsed.
            Some(name) if !self.wire_26x() => match decode_extended_play_event(name, body)? {
                Some(event) => Ok(event),
                None => Ok(PlayEvent::Other(pid)),
            },
            _ => Ok(PlayEvent::Other(pid)),
        }
    }

    fn recv_raw(&mut self, timeout: Duration) -> Result<Option<(i32, Vec<u8>)>, String> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(frame) = self.try_extract_frame()? {
                self.packets_rx += 1;
                return decode_packet_body(&frame, self.compression).map(Some);
            }
            let remaining = match deadline.checked_duration_since(Instant::now()) {
                Some(rem) if !rem.is_zero() => rem,
                _ => return Ok(None),
            };
            let _ = self.stream.set_read_timeout(Some(remaining));
            let mut tmp = [0u8; 8192];
            match self.stream.read(&mut tmp) {
                Ok(0) => return Err("connection closed by peer".to_string()),
                Ok(n) => {
                    if let Some(rx) = &mut self.cipher_rx {
                        rx.decrypt(&mut tmp[..n]);
                    }
                    self.read_buf.extend_from_slice(&tmp[..n]);
                }
                Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                    return Ok(None);
                }
                Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                Err(e) => return Err(format!("TCP read error: {e}")),
            }
        }
    }

    fn try_extract_frame(&mut self) -> Result<Option<Vec<u8>>, String> {
        if self.read_buf.is_empty() {
            return Ok(None);
        }
        let mut value = 0u32;
        for (i, &byte) in self.read_buf.iter().take(5).enumerate() {
            if i == 4 && byte > 0x0f {
                return Err("packet length varint overflow".to_string());
            }
            value |= u32::from(byte & 0x7f) << (7 * i);
            let header_len = i + 1;
            if byte & 0x80 == 0 {
                let packet_len = value as usize;
                if packet_len > MAX_PACKET_SIZE {
                    return Err(format!("packet length {packet_len} exceeds limit"));
                }
                let total = header_len + packet_len;
                if self.read_buf.len() < total {
                    return Ok(None);
                }
                let frame = self.read_buf[header_len..total].to_vec();
                self.read_buf.drain(..total);
                return Ok(Some(frame));
            }
        }
        if self.read_buf.len() >= 5 {
            return Err("packet length varint exceeds 5 bytes".to_string());
        }
        Ok(None)
    }
}

fn build_client_information(view_distance: i8, caps: &megu_proto::VersionCapabilities) -> Vec<u8> {
    let mut out = Vec::with_capacity(18);
    write_string(&mut out, "en_us");
    out.push(view_distance as u8);
    write_varint(&mut out, 0); // chat_mode: enabled
    out.push(1); // chat_colors: true
    out.push(0x7f); // skin_parts: all
    write_varint(&mut out, 1); // main_hand: right
    out.push(0); // enable_text_filtering: false
    out.push(1); // allow_server_listings: true
    if caps.split_particle_status {
        write_varint(&mut out, 0); // particle_status: all (1.21.2+)
    }
    out
}

// ---------------------------------------------------------------------------
// Server-list status ping (Handshake -> Status Request/Response -> Ping/Pong)
// ---------------------------------------------------------------------------

/// Bound on the status-response JSON document: vanilla favicons are a few
/// KiB of base64, so this leaves generous headroom without trusting the peer.
const MAX_STATUS_JSON_BYTES: usize = 1024 * 1024;
const MAX_STATUS_JSON_DEPTH: usize = 64;
const MAX_STATUS_JSON_NODES: usize = 16_384;

/// A completed `next_state = 1` server-list status exchange: the fields a
/// player or operator actually reads off a Multiplayer row, plus the
/// Ping/Pong round trip measured with a monotonic clock.
#[derive(Clone, Debug, PartialEq)]
pub struct ServerStatusPing {
    /// The `host` the Handshake carried (the original name even when a
    /// `_minecraft._tcp` SRV record moved the socket).
    pub host: String,
    /// The `port` the Handshake carried.
    pub port: u16,
    /// The socket address actually dialed after SRV/address resolution.
    pub resolved_addr: SocketAddr,
    /// The protocol version offered in the Handshake.
    pub requested_protocol: i32,
    /// `version.name` the server reported.
    pub version_name: String,
    /// `version.protocol` the server reported.
    pub protocol: i64,
    /// `true` when the reported protocol equals `requested_protocol`.
    pub protocol_match: bool,
    /// `players.online` / `players.max` the server reported.
    pub players_online: i64,
    pub players_max: i64,
    /// Display names from `players.sample` (the hover list), in order.
    pub player_sample: Vec<String>,
    /// Plain-text `description` (the MOTD).
    pub motd: String,
    /// Whether the server advertised a non-empty `favicon`.
    pub has_favicon: bool,
    /// Ping/Pong round trip in milliseconds (`Instant`-measured).
    pub rtt_ms: f64,
}

impl ServerStatusPing {
    /// Machine-readable single-line JSON, shaped like `SessionSummary`'s.
    pub fn to_json(&self) -> String {
        let sample = self
            .player_sample
            .iter()
            .map(|name| format!("\"{}\"", escape_json_string(name)))
            .collect::<Vec<_>>()
            .join(",");
        format!(
            concat!(
                "{{",
                "\"status\":\"ok\",",
                "\"host\":\"{}\",",
                "\"port\":{},",
                "\"resolved_address\":\"{}\",",
                "\"requested_protocol\":{},",
                "\"version_name\":\"{}\",",
                "\"protocol\":{},",
                "\"protocol_match\":{},",
                "\"players_online\":{},",
                "\"players_max\":{},",
                "\"player_sample\":[{}],",
                "\"motd\":\"{}\",",
                "\"has_favicon\":{},",
                "\"rtt_ms\":{:.3}",
                "}}"
            ),
            escape_json_string(&self.host),
            self.port,
            self.resolved_addr,
            self.requested_protocol,
            escape_json_string(&self.version_name),
            self.protocol,
            self.protocol_match,
            self.players_online,
            self.players_max,
            sample,
            escape_json_string(&self.motd),
            self.has_favicon,
            self.rtt_ms,
        )
    }

    /// Plain-language operator output: version, players, MOTD, latency, icon.
    pub fn to_human(&self) -> String {
        let version = if self.protocol_match {
            format!("{} (protocol {})", self.version_name, self.protocol)
        } else {
            format!(
                "{} (protocol {}; you asked for {})",
                self.version_name, self.protocol, self.requested_protocol
            )
        };
        let players = if self.player_sample.is_empty() {
            format!("{} / {}", self.players_online, self.players_max)
        } else {
            format!(
                "{} / {} — {}",
                self.players_online,
                self.players_max,
                self.player_sample.join(", ")
            )
        };
        let motd = strip_format_codes(&self.motd);
        let motd = motd.replace(['\n', '\r'], " ");
        format!(
            "Server status for {}:{}\n  connected to: {}\n  version:      {}\n  players:      {}\n  motd:         {}\n  latency:      {:.1} ms\n  icon:         {}",
            self.host,
            self.port,
            self.resolved_addr,
            version,
            players,
            motd.trim(),
            self.rtt_ms,
            if self.has_favicon { "present" } else { "none" },
        )
    }
}

/// Remove `§`-prefixed formatting codes from a MOTD for terminal display.
fn strip_format_codes(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c == '\u{00a7}' {
            chars.next();
        } else {
            out.push(c);
        }
    }
    out
}

fn escape_json_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c.is_control() => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

/// Monotonic ping nonce: nanoseconds since a process-fixed `Instant` epoch,
/// so the value is clock-independent and strictly monotonic for later calls
/// after a clock tick elapses — never `SystemTime`.
fn status_ping_nonce() -> i64 {
    static EPOCH: OnceLock<Instant> = OnceLock::new();
    let epoch = EPOCH.get_or_init(Instant::now);
    Instant::now().saturating_duration_since(*epoch).as_nanos() as i64
}

/// Run a server-list status ping against `host:port` speaking `version`'s
/// protocol: Handshake (`next_state = 1`), Status Request, Status Response
/// JSON, then a Ping/Pong round trip for the latency figure. The
/// `_minecraft._tcp` SRV lookup is the live one; see
/// [`ping_server_resolved`] for the injectable seam.
pub fn ping_server(
    host: &str,
    port: u16,
    version: megu_proto::ProtocolVersion,
    timeout: Duration,
) -> Result<ServerStatusPing, String> {
    ping_server_resolved(host, port, version, timeout, |h, p| {
        megu_proto::resolve_server_address(h, p).map_err(|e| e.to_string())
    })
}

/// [`ping_server`] with the `_minecraft._tcp` SRV / address lookup injected,
/// so tests can prove the SRV redirect and the original-host-preservation
/// rule without live DNS. Works identically for `1.21.1` (767) and `26.3`
/// (777): the status packet ids resolve through the version's table.
pub fn ping_server_resolved(
    host: &str,
    port: u16,
    version: megu_proto::ProtocolVersion,
    timeout: Duration,
    resolve: impl FnOnce(&str, u16) -> Result<Vec<SocketAddr>, String>,
) -> Result<ServerStatusPing, String> {
    let codec = megu_proto::VersionRouter::with_v767_and_v777()
        .select(version.number())
        .map_err(|e| e.to_string())?;
    let addrs = resolve(host, port)?;
    if addrs.is_empty() {
        return Err(format!(
            "resolve_server_address({host}:{port}) yielded no addresses"
        ));
    }
    let addr = format!("{host}:{port}");
    let stream = TcpStream::connect(addrs.as_slice())
        .map_err(|e| format!("failed to connect to {addr}: {e}"))?;
    let _ = stream.set_nodelay(true);
    let _ = stream.set_read_timeout(Some(timeout));
    let _ = stream.set_write_timeout(Some(timeout));
    let resolved_addr = stream.peer_addr().unwrap_or(addrs[0]);
    let mut client =
        ProtocolClient::on_stream(stream, codec, version, OnlineAuthOptions::default());

    let deadline = Instant::now() + timeout;

    // Handshake — original host/port even when the SRV lookup redirected the
    // TCP target (vanilla ServerNameResolver rule); next_state 1 = Status.
    let mut hs = Vec::new();
    write_varint(&mut hs, version.number());
    write_string(&mut hs, host);
    hs.extend_from_slice(&port.to_be_bytes());
    write_varint(&mut hs, 1);
    client.send_named(PacketPhase::Handshake, "handshake", &hs)?;

    // Status Request (no payload) -> Status Response carries the JSON.
    client.send_named(PacketPhase::Status, "status_request", &[])?;
    let remaining = deadline
        .checked_duration_since(Instant::now())
        .ok_or_else(|| "status ping timed out".to_string())?;
    let Some((pid, body)) = client.recv_raw(remaining)? else {
        return Err("timed out waiting for status response".to_string());
    };
    if client.s2c_name(PacketPhase::Status, pid).as_deref() != Some("status_response") {
        return Err(format!(
            "expected status_response, received packet id {pid:#04x}"
        ));
    }
    let json_text = WireReader::new(&body).read_string(MAX_STATUS_JSON_BYTES)?;
    let mut status = parse_status_response(&json_text)?;

    // Ping Request -> Pong: the echoed payload must match exactly.
    let nonce = status_ping_nonce();
    let ping_start = Instant::now();
    client.send_named(PacketPhase::Status, "ping_request", &nonce.to_be_bytes())?;
    let rtt = loop {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .ok_or_else(|| "status ping timed out".to_string())?;
        let Some((pid, body)) = client.recv_raw(remaining)? else {
            return Err("timed out waiting for pong".to_string());
        };
        if client.s2c_name(PacketPhase::Status, pid).as_deref() == Some("pong_response") {
            let echoed = WireReader::new(&body).read_i64()?;
            if echoed != nonce {
                return Err(format!(
                    "pong payload mismatch: sent {nonce}, server echoed {echoed}"
                ));
            }
            break ping_start.elapsed();
        }
    };

    status.host = host.to_string();
    status.port = port;
    status.resolved_addr = resolved_addr;
    status.requested_protocol = version.number();
    status.protocol_match = status.protocol == i64::from(version.number());
    status.rtt_ms = rtt.as_secs_f64() * 1000.0;
    Ok(status)
}

/// Parse the status-response JSON into a [`ServerStatusPing`]; the caller
/// fills the transport fields (`host`, `port`, `resolved_addr`,
/// `requested_protocol`, `protocol_match`, `rtt_ms`) afterwards.
fn parse_status_response(text: &str) -> Result<ServerStatusPing, String> {
    let root = parse_status_json(text)?;
    let version_name = root
        .get("version")
        .and_then(|v| v.get("name"))
        .and_then(JsonValue::as_str)
        .unwrap_or("unknown")
        .to_string();
    let protocol = root
        .get("version")
        .and_then(|v| v.get("protocol"))
        .and_then(JsonValue::as_i64)
        .ok_or_else(|| "status response missing numeric version.protocol".to_string())?;
    let players = root.get("players");
    let players_online = players
        .and_then(|p| p.get("online"))
        .and_then(JsonValue::as_i64)
        .unwrap_or(0);
    let players_max = players
        .and_then(|p| p.get("max"))
        .and_then(JsonValue::as_i64)
        .unwrap_or(0);
    let player_sample = players
        .and_then(|p| p.get("sample"))
        .map(|s| match s {
            JsonValue::Arr(items) => items
                .iter()
                .filter_map(|e| {
                    e.get("name")
                        .and_then(JsonValue::as_str)
                        .map(str::to_string)
                })
                .collect(),
            _ => Vec::new(),
        })
        .unwrap_or_default();
    let motd = root
        .get("description")
        .map(JsonValue::plain_text)
        .unwrap_or_default();
    let has_favicon = matches!(root.get("favicon"), Some(JsonValue::Str(s)) if !s.is_empty());
    Ok(ServerStatusPing {
        host: String::new(),
        port: 0,
        resolved_addr: SocketAddr::from(([0, 0, 0, 0], 0)),
        requested_protocol: 0,
        version_name,
        protocol,
        protocol_match: false,
        players_online,
        players_max,
        player_sample,
        motd,
        has_favicon,
        rtt_ms: 0.0,
    })
}

// ---------------------------------------------------------------------------
// Minimal bounded JSON parser for the status response (pure std)
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum JsonValue {
    Null,
    Bool(bool),
    Num(f64),
    Str(String),
    Arr(Vec<JsonValue>),
    Obj(Vec<(String, JsonValue)>),
}

impl JsonValue {
    pub(crate) fn get(&self, key: &str) -> Option<&JsonValue> {
        match self {
            Self::Obj(entries) => entries.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    pub(crate) fn as_str(&self) -> Option<&str> {
        match self {
            Self::Str(s) => Some(s),
            _ => None,
        }
    }

    pub(crate) fn as_i64(&self) -> Option<i64> {
        match self {
            Self::Num(n) if n.fract() == 0.0 && n.abs() <= i64::MAX as f64 => Some(*n as i64),
            _ => None,
        }
    }

    /// Numeric value as `f64` (fractional values included).
    pub(crate) fn as_f64(&self) -> Option<f64> {
        match self {
            Self::Num(n) => Some(*n),
            _ => None,
        }
    }

    /// Flatten a chat-component-shaped value into plain text: a bare string,
    /// an array of components, or `{text|translate, extra: [...]}`.
    fn plain_text(&self) -> String {
        match self {
            Self::Str(s) => s.clone(),
            Self::Arr(items) => items.iter().map(Self::plain_text).collect(),
            Self::Obj(_) => {
                let mut out = String::new();
                if let Some(t) = self.get("text").and_then(JsonValue::as_str) {
                    out.push_str(t);
                } else if let Some(t) = self.get("translate").and_then(JsonValue::as_str) {
                    out.push_str(t);
                }
                if let Some(extra) = self.get("extra") {
                    out.push_str(&extra.plain_text());
                }
                out
            }
            _ => String::new(),
        }
    }
}

fn parse_status_json(text: &str) -> Result<JsonValue, String> {
    parse_json(text, MAX_STATUS_JSON_NODES)
}

/// Crate-shared minimal JSON parser (status ping, asset indexes,
/// `sounds.json`). `max_nodes` bounds parser work on hostile input; parsing
/// itself is delegated to the canonical `megu_core::json` reader and lowered
/// into this module's [`JsonValue`] so callers keep their shape.
pub(crate) fn parse_json(text: &str, max_nodes: usize) -> Result<JsonValue, String> {
    let limits = megu_core::json::JsonLimits {
        max_depth: MAX_STATUS_JSON_DEPTH,
        max_nodes,
        max_bytes: usize::MAX,
    };
    megu_core::json::parse_json_with_limits(text, limits, false)
        .map(json_value_from_core)
        .map_err(|e| e.to_string())
}

fn json_value_from_core(value: megu_core::json::JsonValue) -> JsonValue {
    match value {
        megu_core::json::JsonValue::Null => JsonValue::Null,
        megu_core::json::JsonValue::Bool(b) => JsonValue::Bool(b),
        megu_core::json::JsonValue::Number(n) => JsonValue::Num(n),
        megu_core::json::JsonValue::String(s) => JsonValue::Str(s),
        megu_core::json::JsonValue::Array(items) => {
            JsonValue::Arr(items.into_iter().map(json_value_from_core).collect())
        }
        megu_core::json::JsonValue::Object(entries) => JsonValue::Obj(
            entries
                .into_iter()
                .map(|(key, value)| (key, json_value_from_core(value)))
                .collect(),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ssn1_frame(sub_season_idx: u8, flags: u8) -> Vec<u8> {
        let mut frame = Vec::with_capacity(SSN1_FRAME_LEN);
        frame.extend_from_slice(SSN1_MAGIC);
        frame.push(SSN1_VERSION);
        frame.push(sub_season_idx);
        frame.extend_from_slice(&16_383u16.to_be_bytes()); // progress q16 (~0.25)
        frame.extend_from_slice(&17u16.to_be_bytes()); // day_of_season
        frame.extend_from_slice(&32_767u16.to_be_bytes()); // daylight q16 (~0.5)
        frame.extend_from_slice(&[200, 210, 220]); // foliage_rgb
        frame.extend_from_slice(&[100, 150, 90]); // grass_rgb
        frame.push(flags);
        frame
    }

    fn framed(channel: &str, payload: &[u8]) -> Vec<u8> {
        let mut body = Vec::new();
        assert!(channel.len() < 128);
        body.push(channel.len() as u8);
        body.extend_from_slice(channel.as_bytes());
        body.extend_from_slice(payload);
        body
    }

    #[test]
    fn ssn1_frame_decodes_all_wire_fields() {
        let frame = ssn1_frame(5, 0b101);
        let sync = decode_season_sync_payload(&frame).unwrap();
        assert_eq!(sync.sub_season_idx, 5);
        assert!((sync.progress_in_sub_season - 16_383.0 / 65_535.0).abs() < 1e-6);
        assert_eq!(sync.day_of_season, 17);
        assert!((sync.daylight_fraction - 32_767.0 / 65_535.0).abs() < 1e-6);
        assert_eq!(sync.foliage_rgb, [200, 210, 220]);
        assert_eq!(sync.grass_rgb, [100, 150, 90]);
        assert!(sync.tropical_dim);
        assert!(!sync.frozen_season);
        assert!(sync.snow_enabled);

        for channel in [CHANNEL_SEASON_V1, CHANNEL_MEGU_SEASON] {
            let event = decode_custom_channel_event(channel, &frame).unwrap();
            assert_eq!(event, ClientCustomChannelEvent::Season(sync.clone()));
            assert_eq!(
                decode_custom_payload_packet(&framed(channel, &frame)).unwrap(),
                ClientCustomChannelEvent::Season(sync.clone())
            );
            assert_eq!(
                decode_megu_custom_payload(channel, &frame),
                Some(MeguCustomPayload::Season(sync.clone()))
            );
        }
    }

    #[test]
    fn ssn1_fails_closed_on_corrupt_frames() {
        let valid = ssn1_frame(0, 0);
        let mut bad_magic = valid.clone();
        bad_magic[0] = b'X';
        let mut bad_version = valid.clone();
        bad_version[4] = 2;
        let mut trailing = valid.clone();
        trailing.push(0);
        let corrupt: Vec<Vec<u8>> = vec![
            bad_magic,
            bad_version,
            valid[..SSN1_FRAME_LEN - 1].to_vec(),
            trailing,
            ssn1_frame(12, 0),
            ssn1_frame(255, 0),
            ssn1_frame(3, 0b1000),
            Vec::new(),
        ];
        for frame in &corrupt {
            assert!(
                decode_season_sync_payload(frame).is_err(),
                "accepted corrupt SSN1 frame {frame:?}"
            );
            assert_eq!(decode_megu_custom_payload(CHANNEL_SEASON_V1, frame), None);
            assert!(
                decode_custom_channel_event(CHANNEL_SEASON_V1, frame).is_err(),
                "router accepted corrupt SSN1 frame"
            );
        }
    }

    #[test]
    fn megu_router_fails_closed_on_unknown_channels() {
        assert_eq!(
            decode_megu_custom_payload("example:unrecognized", b"\x01\x02"),
            None
        );
        assert_eq!(decode_megu_custom_payload(CHANNEL_MAGIC_V1, b"BAD!"), None);
    }

    /// `explode`: centre + strength + byte-offset records + knockback +
    /// block-interaction + two payload-free `ParticleOptions` ids + the
    /// `Holder<SoundEvent>` trailer all decode; a count the body cannot
    /// hold fails closed instead of driving an allocation.
    #[test]
    fn explode_decodes_records_knockback_and_sound_holder() {
        let mut body = Vec::new();
        body.extend_from_slice(&8.5f64.to_be_bytes());
        body.extend_from_slice(&65.0f64.to_be_bytes());
        body.extend_from_slice(&(-3.25f64).to_be_bytes());
        body.extend_from_slice(&4.0f32.to_be_bytes());
        write_varint(&mut body, 2); // record count
        body.extend_from_slice(&[0u8, 1, (-1i8) as u8]);
        body.extend_from_slice(&[2, (-3i8) as u8, 4]);
        for v in [0.5f32, 1.0, -0.25] {
            body.extend_from_slice(&v.to_be_bytes());
        }
        write_varint(&mut body, 1); // block_interaction: destroy
        write_varint(&mut body, 33); // small ParticleOptions id
        write_varint(&mut body, 37); // large ParticleOptions id
        // Inline sound holder (id 0) + name + no fixed range.
        write_varint(&mut body, 0);
        write_varint(&mut body, "minecraft:entity.generic.explode".len() as i32);
        body.extend_from_slice(b"minecraft:entity.generic.explode");
        body.push(0);

        let event = decode_extended_play_event("explode", &body)
            .unwrap()
            .expect("explode decodes");
        let PlayEvent::Explosion {
            x,
            y,
            z,
            power,
            records,
            knockback,
            block_interaction,
            small_particle_id,
            large_particle_id,
            sound_name,
            sound_id,
        } = event
        else {
            panic!("expected Explosion, got {event:?}");
        };
        assert_eq!((x, y, z), (8.5, 65.0, -3.25));
        assert_eq!(power, 4.0);
        assert_eq!(records, vec![[0, 1, -1], [2, -3, 4]]);
        assert_eq!(knockback, [0.5, 1.0, -0.25]);
        assert_eq!(block_interaction, 1);
        assert_eq!(small_particle_id, 33);
        assert_eq!(large_particle_id, 37);
        assert_eq!(
            sound_name.as_deref(),
            Some("minecraft:entity.generic.explode")
        );
        assert_eq!(sound_id, None);

        // Registry-id holder (>0) surfaces as `sound_id` minus one.
        let mut reg = body.clone();
        // Drop the holder trailer: flag + 32 name bytes + name-len +
        // holder varints = 35 bytes.
        reg.truncate(reg.len() - 35);
        write_varint(&mut reg, 42); // registry id 42 -> sound_id 41
        let event = decode_extended_play_event("explode", &reg)
            .unwrap()
            .expect("registry holder decodes");
        let PlayEvent::Explosion {
            sound_name,
            sound_id,
            ..
        } = event
        else {
            panic!("expected Explosion, got {event:?}");
        };
        assert_eq!(sound_name, None);
        assert_eq!(sound_id, Some(41));

        // A record count beyond the body's bytes fails closed.
        let mut bad = Vec::new();
        bad.extend_from_slice(&[0u8; 28]); // x,y,z + power
        write_varint(&mut bad, 1000);
        assert!(decode_extended_play_event("explode", &bad).is_err());
    }
}
