// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
//! Clean-room `CustomPacketPayload` wire codec for the claims sync feed.
//!
//! [`crate::claims`] keeps a monotonic `revision`-stamped
//! [`ClaimsSyncDelta`] queue plus atomic `openpac_claims.json` persistence;
//! this module is how that state reaches client-side minimap / world-map
//! overlays (Xaero's World Map, OpenPAC, FTB Chunks bridges) through
//! Minecraft custom payload channels.
//!
//! The channel names are the ones those overlays listen on —
//! `openpartiesandclaims:claims_sync`, `ftbchunks:claims_sync` — plus our own
//! `megu:claims_v1`. The *payload* is a deterministic clean-room format
//! defined here, not a byte-compat clone of either mod's private schema:
//!
//! ```text
//! frame   := varint(channel_len) channel_bytes payload
//! payload := u8 protocol_tag (1)
//!            u64be revision_from u64be revision_to
//!            u16be dimension_len dimension_utf8
//!            u32be entry_count
//!            entry[entry_count]
//! entry   := i32be cx i32be cz u8 present (0x00 absent / 0x01 cell follows)
//! cell    := u8 owner_kind (1 player, 2 server, 3 expired)
//!            u16be owner_id_len owner_id_utf8
//!            u16be sub_claim_len sub_claim_utf8
//!            u8 forceloaded (0/1)
//!            u32be color_rgb (0x00RRGGBB)
//! ```
//!
//! Entries are strictly ascending by `(cx, cz)` on the wire — the encoder
//! sorts a copy of the caller's list and rejects duplicate chunks, so the
//! same logical batch always serializes to the same bytes.
//!
//! Decoding fails closed: an unknown protocol tag, a truncated or trailing
//! byte, an entry count above [`MAX_WIRE_CLAIMS_PER_BATCH`], a string
//! length above [`MAX_WIRE_STRING_BYTES`] (both caps are checked *before*
//! any allocation happens), invalid UTF-8, an unknown `owner_kind`, a flag
//! byte other than 0/1, an out-of-range color, an unsorted or duplicated
//! entry list, and an inverted revision range are all
//! [`ClaimsWireError`]s. Nothing in this module panics on hostile input.

use crate::claims::{ClaimOwner, ClaimsSyncDelta, ClaimsWorld, ChunkClaim, SubClaimId};
use megu_core::id::ResourceLocation;
use std::collections::BTreeMap;
use std::fmt;

/// OpenPAC's claims sync channel name.
pub const CHANNEL_OPENPAC_CLAIMS_SYNC: &str = "openpartiesandclaims:claims_sync";
/// FTB Chunks' claims sync channel name.
pub const CHANNEL_FTBCHUNKS_CLAIMS_SYNC: &str = "ftbchunks:claims_sync";
/// megu's own claims sync channel name.
pub const CHANNEL_MEGU_CLAIMS_V1: &str = "megu:claims_v1";

/// The only `protocol_tag` this codec reads or writes. A bump is a format
/// change; unknown tags are rejected, never guessed at.
pub const WIRE_PROTOCOL_TAG: u8 = 1;
/// The most chunk entries one batch may carry (checked before allocation).
pub const MAX_WIRE_CLAIMS_PER_BATCH: usize = 4096;
/// The longest length-prefixed string field, in bytes (checked before
/// allocation). Covers dimensions, channel names, owner ids and sub-claims.
pub const MAX_WIRE_STRING_BYTES: usize = 256;

const OWNER_KIND_PLAYER: u8 = 1;
const OWNER_KIND_SERVER: u8 = 2;
const OWNER_KIND_EXPIRED: u8 = 3;

// ------------------------------------------------------------------ model

/// The `owner_kind` field of a [`ClaimsWireCell`], mirroring
/// [`ClaimOwner`]'s three cases.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WireOwnerKind {
    /// A player's claim; `owner_id` is their UUID.
    Player,
    /// An admin/server claim; `owner_id` is empty on the wire.
    Server,
    /// A claim whose owner expired; `owner_id` is the expired UUID.
    Expired,
}

impl WireOwnerKind {
    fn from_tag(tag: u8) -> Result<Self, ClaimsWireError> {
        match tag {
            OWNER_KIND_PLAYER => Ok(Self::Player),
            OWNER_KIND_SERVER => Ok(Self::Server),
            OWNER_KIND_EXPIRED => Ok(Self::Expired),
            other => Err(ClaimsWireError::InvalidOwnerKind(other)),
        }
    }

    fn tag(self) -> u8 {
        match self {
            Self::Player => OWNER_KIND_PLAYER,
            Self::Server => OWNER_KIND_SERVER,
            Self::Expired => OWNER_KIND_EXPIRED,
        }
    }
}

/// What a client overlay needs to paint one claimed chunk: the owner, the
/// sub-claim it belongs to, the forceload ticket and the overlay color.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClaimsWireCell {
    pub owner_kind: WireOwnerKind,
    /// The owner's UUID for player/expired claims, empty for server claims.
    pub owner_id: String,
    pub sub_claim: String,
    pub forceloaded: bool,
    /// Overlay color `0xRRGGBB`.
    pub color_rgb: u32,
}

impl ClaimsWireCell {
    /// The wire view of an engine [`ChunkClaim`].
    pub fn from_claim(claim: &ChunkClaim) -> Self {
        let (owner_kind, owner_id) = match &claim.owner {
            ClaimOwner::Player(u) => (WireOwnerKind::Player, u.clone()),
            ClaimOwner::Server => (WireOwnerKind::Server, String::new()),
            ClaimOwner::Expired(u) => (WireOwnerKind::Expired, u.clone()),
        };
        Self {
            owner_kind,
            owner_id,
            sub_claim: claim.sub_claim.clone(),
            forceloaded: claim.forceloaded,
            color_rgb: claim.color_rgb,
        }
    }

    /// Back to the engine's shape: `player`/`expired` ids become the owner
    /// UUID, a server cell has none.
    pub fn to_claim(&self) -> ChunkClaim {
        let owner = match self.owner_kind {
            WireOwnerKind::Player => ClaimOwner::Player(self.owner_id.clone()),
            WireOwnerKind::Server => ClaimOwner::Server,
            WireOwnerKind::Expired => ClaimOwner::Expired(self.owner_id.clone()),
        };
        ChunkClaim {
            owner,
            sub_claim: self.sub_claim.clone(),
            forceloaded: self.forceloaded,
            color_rgb: self.color_rgb,
        }
    }
}

/// One chunk in a batch: `state` is the claim to paint, or `None` when the
/// delta removed the claim (paint wilderness).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClaimsWireEntry {
    pub cx: i32,
    pub cz: i32,
    pub state: Option<ClaimsWireCell>,
}

/// A `claims_sync` payload for one dimension: every chunk that changed
/// between `revision_from` and `revision_to` (a delta batch), or the full
/// claim set at `revision_to` with `revision_from == 0` (a snapshot batch).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClaimsWireBatch {
    /// Always [`WIRE_PROTOCOL_TAG`] for this codec.
    pub protocol_tag: u8,
    pub revision_from: u64,
    pub revision_to: u64,
    /// Canonical `namespace:path` dimension id.
    pub dimension: String,
    pub entries: Vec<ClaimsWireEntry>,
}

impl ClaimsWireBatch {
    /// A batch under the current protocol tag.
    pub fn new(
        revision_from: u64,
        revision_to: u64,
        dimension: impl Into<String>,
        entries: Vec<ClaimsWireEntry>,
    ) -> Self {
        Self {
            protocol_tag: WIRE_PROTOCOL_TAG,
            revision_from,
            revision_to,
            dimension: dimension.into(),
            entries,
        }
    }
}

// ------------------------------------------------------------------ errors

/// Every way a wire batch or frame can fail to encode or decode.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ClaimsWireError {
    /// `protocol_tag` was not [`WIRE_PROTOCOL_TAG`].
    UnsupportedProtocolTag(u8),
    /// The buffer ended inside a field.
    Truncated { needed: usize, remaining: usize },
    /// Bytes remained after the last field.
    TrailingBytes { remaining: usize },
    /// The entry count exceeds [`MAX_WIRE_CLAIMS_PER_BATCH`].
    TooManyEntries { count: usize },
    /// A length-prefixed string exceeds [`MAX_WIRE_STRING_BYTES`].
    StringTooLong { field: &'static str, length: usize },
    /// A VarInt ran past five bytes or a frame length was negative.
    VarIntTooLong,
    /// A length-prefixed field's bytes are not UTF-8.
    InvalidUtf8 { field: &'static str },
    /// The `owner_kind` byte was not 1, 2 or 3.
    InvalidOwnerKind(u8),
    /// A flag byte carried a value other than 0 or 1.
    InvalidFlag { field: &'static str, value: u8 },
    /// `owner_id` is empty for a player/expired cell or non-empty for a
    /// server cell.
    InvalidOwnerId { kind: WireOwnerKind, owner_id: String },
    /// The sub-claim id fails [`SubClaimId`] validation.
    InvalidSubClaim(String),
    /// `color_rgb` does not fit `0xRRGGBB`.
    ColorOutOfRange(u32),
    /// A dimension or channel name fails [`ResourceLocation`] validation.
    InvalidIdentifier { field: &'static str, value: String },
    /// `revision_from > revision_to`.
    RevisionRangeInverted,
    /// Two entries name the same chunk.
    DuplicateChunk { cx: i32, cz: i32 },
    /// Wire entries must be strictly ascending by `(cx, cz)`.
    EntriesOutOfOrder,
}

impl fmt::Display for ClaimsWireError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedProtocolTag(t) => {
                write!(f, "unsupported claims wire protocol tag {t}")
            }
            Self::Truncated { needed, remaining } => {
                write!(f, "needed {needed} bytes, {remaining} left")
            }
            Self::TrailingBytes { remaining } => {
                write!(f, "{remaining} bytes left after the last field")
            }
            Self::TooManyEntries { count } => write!(
                f,
                "batch declares {count} entries, more than {MAX_WIRE_CLAIMS_PER_BATCH}"
            ),
            Self::StringTooLong { field, length } => write!(
                f,
                "{field} is {length} bytes, more than {MAX_WIRE_STRING_BYTES}"
            ),
            Self::VarIntTooLong => write!(f, "variable integer longer than five bytes"),
            Self::InvalidUtf8 { field } => write!(f, "{field} is not valid UTF-8"),
            Self::InvalidOwnerKind(k) => write!(f, "invalid owner_kind {k}"),
            Self::InvalidFlag { field, value } => {
                write!(f, "{field} flag must be 0 or 1, got {value}")
            }
            Self::InvalidOwnerId { kind, owner_id } => {
                write!(f, "owner_id {owner_id:?} is invalid for {kind:?} claims")
            }
            Self::InvalidSubClaim(s) => write!(f, "invalid sub-claim id {s:?}"),
            Self::ColorOutOfRange(c) => write!(f, "color {c:#x} does not fit 0xRRGGBB"),
            Self::InvalidIdentifier { field, value } => {
                write!(f, "{field} {value:?} is not a resource location")
            }
            Self::RevisionRangeInverted => {
                write!(f, "revision_from exceeds revision_to")
            }
            Self::DuplicateChunk { cx, cz } => {
                write!(f, "chunk ({cx}, {cz}) appears twice in one batch")
            }
            Self::EntriesOutOfOrder => {
                write!(f, "entries are not sorted by (cx, cz)")
            }
        }
    }
}

impl std::error::Error for ClaimsWireError {}

// ---------------------------------------------------------------- encoding

fn put_u16(out: &mut Vec<u8>, n: usize) {
    out.extend_from_slice(&(n as u16).to_be_bytes());
}

fn put_u32(out: &mut Vec<u8>, n: u32) {
    out.extend_from_slice(&n.to_be_bytes());
}

fn put_u64(out: &mut Vec<u8>, n: u64) {
    out.extend_from_slice(&n.to_be_bytes());
}

fn put_i32(out: &mut Vec<u8>, n: i32) {
    out.extend_from_slice(&n.to_be_bytes());
}

/// Length-prefixed UTF-8 with the cap enforced *before* the write.
fn put_str(out: &mut Vec<u8>, field: &'static str, s: &str) -> Result<(), ClaimsWireError> {
    if s.len() > MAX_WIRE_STRING_BYTES {
        return Err(ClaimsWireError::StringTooLong {
            field,
            length: s.len(),
        });
    }
    put_u16(out, s.len());
    out.extend_from_slice(s.as_bytes());
    Ok(())
}

/// Protocol VarInt: unsigned LEB128 of the 32-bit two's-complement form —
/// the same framing `FriendlyByteBuf` writes.
fn put_varint(out: &mut Vec<u8>, value: i32) {
    megu_core::varint::write_varint_i32(value, out);
}

fn check_identifier(field: &'static str, value: &str) -> Result<(), ClaimsWireError> {
    if value.len() > MAX_WIRE_STRING_BYTES {
        return Err(ClaimsWireError::StringTooLong {
            field,
            length: value.len(),
        });
    }
    ResourceLocation::parse(value).map_err(|_| ClaimsWireError::InvalidIdentifier {
        field,
        value: value.to_owned(),
    })?;
    Ok(())
}

fn validate_cell(cell: &ClaimsWireCell) -> Result<(), ClaimsWireError> {
    let consistent = match cell.owner_kind {
        WireOwnerKind::Player | WireOwnerKind::Expired => !cell.owner_id.is_empty(),
        WireOwnerKind::Server => cell.owner_id.is_empty(),
    };
    if !consistent {
        return Err(ClaimsWireError::InvalidOwnerId {
            kind: cell.owner_kind,
            owner_id: cell.owner_id.clone(),
        });
    }
    if cell.owner_id.len() > MAX_WIRE_STRING_BYTES {
        return Err(ClaimsWireError::StringTooLong {
            field: "owner_id",
            length: cell.owner_id.len(),
        });
    }
    SubClaimId::parse(&cell.sub_claim).map_err(|_| {
        ClaimsWireError::InvalidSubClaim(cell.sub_claim.clone())
    })?;
    if cell.color_rgb > 0x00ff_ffff {
        return Err(ClaimsWireError::ColorOutOfRange(cell.color_rgb));
    }
    Ok(())
}

/// Everything the format requires, checked once up front so a bad batch
/// produces no bytes at all.
fn validate_batch(batch: &ClaimsWireBatch) -> Result<(), ClaimsWireError> {
    if batch.protocol_tag != WIRE_PROTOCOL_TAG {
        return Err(ClaimsWireError::UnsupportedProtocolTag(batch.protocol_tag));
    }
    if batch.revision_from > batch.revision_to {
        return Err(ClaimsWireError::RevisionRangeInverted);
    }
    check_identifier("dimension", &batch.dimension)?;
    if batch.entries.len() > MAX_WIRE_CLAIMS_PER_BATCH {
        return Err(ClaimsWireError::TooManyEntries {
            count: batch.entries.len(),
        });
    }
    for entry in &batch.entries {
        if let Some(cell) = &entry.state {
            validate_cell(cell)?;
        }
    }
    Ok(())
}

/// Serialize `batch` into the deterministic `claims_sync` payload bytes.
///
/// The caller's `entries` need not be ordered — they are sorted by
/// `(cx, cz)` here — but duplicate chunks are rejected, because two states
/// for one chunk in a single batch is ambiguous.
pub fn encode_claims_batch(batch: &ClaimsWireBatch) -> Result<Vec<u8>, ClaimsWireError> {
    validate_batch(batch)?;

    let mut entries = batch.entries.clone();
    entries.sort_by_key(|e| (e.cx, e.cz));
    for pair in entries.windows(2) {
        if (pair[0].cx, pair[0].cz) == (pair[1].cx, pair[1].cz) {
            return Err(ClaimsWireError::DuplicateChunk {
                cx: pair[0].cx,
                cz: pair[0].cz,
            });
        }
    }

    let mut out = Vec::new();
    out.push(batch.protocol_tag);
    put_u64(&mut out, batch.revision_from);
    put_u64(&mut out, batch.revision_to);
    put_str(&mut out, "dimension", &batch.dimension)?;
    put_u32(&mut out, entries.len() as u32);
    for entry in &entries {
        put_i32(&mut out, entry.cx);
        put_i32(&mut out, entry.cz);
        match &entry.state {
            None => out.push(0),
            Some(cell) => {
                out.push(1);
                out.push(cell.owner_kind.tag());
                put_str(&mut out, "owner_id", &cell.owner_id)?;
                put_str(&mut out, "sub_claim", &cell.sub_claim)?;
                out.push(u8::from(cell.forceloaded));
                put_u32(&mut out, cell.color_rgb);
            }
        }
    }
    Ok(out)
}

/// Wrap a batch in the custom-payload frame the channel rides on:
/// `VarInt(channel_len) + channel + payload`.
pub fn encode_custom_payload_frame(
    channel: &str,
    batch: &ClaimsWireBatch,
) -> Result<Vec<u8>, ClaimsWireError> {
    check_identifier("channel", channel)?;
    let payload = encode_claims_batch(batch)?;
    let mut out = Vec::with_capacity(channel.len() + payload.len() + 8);
    put_varint(&mut out, channel.len() as i32);
    out.extend_from_slice(channel.as_bytes());
    out.extend_from_slice(&payload);
    Ok(out)
}

// ---------------------------------------------------------------- decoding

struct Reader<'a> {
    bytes: &'a [u8],
    cursor: usize,
}

impl<'a> Reader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, cursor: 0 }
    }

    fn remaining(&self) -> usize {
        self.bytes.len() - self.cursor
    }

    fn rest(&self) -> &'a [u8] {
        &self.bytes[self.cursor..]
    }

    fn take(&mut self, count: usize) -> Result<&'a [u8], ClaimsWireError> {
        if count > self.remaining() {
            return Err(ClaimsWireError::Truncated {
                needed: count,
                remaining: self.remaining(),
            });
        }
        let part = &self.bytes[self.cursor..self.cursor + count];
        self.cursor += count;
        Ok(part)
    }

    fn u8(&mut self) -> Result<u8, ClaimsWireError> {
        Ok(self.take(1)?[0])
    }

    fn u16(&mut self) -> Result<u16, ClaimsWireError> {
        Ok(u16::from_be_bytes(self.take(2)?.try_into().unwrap()))
    }

    fn u32(&mut self) -> Result<u32, ClaimsWireError> {
        Ok(u32::from_be_bytes(self.take(4)?.try_into().unwrap()))
    }

    fn u64(&mut self) -> Result<u64, ClaimsWireError> {
        Ok(u64::from_be_bytes(self.take(8)?.try_into().unwrap()))
    }

    fn i32(&mut self) -> Result<i32, ClaimsWireError> {
        Ok(i32::from_be_bytes(self.take(4)?.try_into().unwrap()))
    }

    /// Protocol VarInt: at most five bytes, the last carrying ≤4 payload
    /// bits.
    fn varint(&mut self) -> Result<i32, ClaimsWireError> {
        let tail = self
            .bytes
            .get(self.cursor..)
            .ok_or(ClaimsWireError::VarIntTooLong)?;
        match megu_core::varint::read_varint_i32(tail) {
            Ok((value, consumed)) => {
                self.cursor += consumed;
                Ok(value)
            }
            Err(megu_core::varint::VarIntError::UnexpectedEof) => {
                Err(ClaimsWireError::Truncated {
                    needed: 1,
                    remaining: 0,
                })
            }
            Err(_) => Err(ClaimsWireError::VarIntTooLong),
        }
}

    /// A `u16be` length-prefixed UTF-8 string. The cap is checked *before*
    /// the body is touched, so a hostile length yields `StringTooLong`
    /// rather than a `Truncated` wild goose chase or an oversized read.
    fn read_str(&mut self, field: &'static str) -> Result<String, ClaimsWireError> {
        let len = usize::from(self.u16()?);
        if len > MAX_WIRE_STRING_BYTES {
            return Err(ClaimsWireError::StringTooLong { field, length: len });
        }
        let bytes = self.take(len)?;
        String::from_utf8(bytes.to_vec())
            .map_err(|_| ClaimsWireError::InvalidUtf8 { field })
    }

    fn read_cell(&mut self) -> Result<ClaimsWireCell, ClaimsWireError> {
        let owner_kind = WireOwnerKind::from_tag(self.u8()?)?;
        let owner_id = self.read_str("owner_id")?;
        let consistent = match owner_kind {
            WireOwnerKind::Player | WireOwnerKind::Expired => !owner_id.is_empty(),
            WireOwnerKind::Server => owner_id.is_empty(),
        };
        if !consistent {
            return Err(ClaimsWireError::InvalidOwnerId { kind: owner_kind, owner_id });
        }
        let sub_claim = self.read_str("sub_claim")?;
        SubClaimId::parse(&sub_claim)
            .map_err(|_| ClaimsWireError::InvalidSubClaim(sub_claim.clone()))?;
        let forceloaded = match self.u8()? {
            0 => false,
            1 => true,
            value => return Err(ClaimsWireError::InvalidFlag { field: "forceloaded", value }),
        };
        let color_rgb = self.u32()?;
        if color_rgb > 0x00ff_ffff {
            return Err(ClaimsWireError::ColorOutOfRange(color_rgb));
        }
        Ok(ClaimsWireCell {
            owner_kind,
            owner_id,
            sub_claim,
            forceloaded,
            color_rgb,
        })
    }
}

/// Parse one `claims_sync` payload back into a [`ClaimsWireBatch`].
/// Fail-closed on every deviation from the format documented at the top of
/// this module.
pub fn decode_claims_batch(bytes: &[u8]) -> Result<ClaimsWireBatch, ClaimsWireError> {
    let mut r = Reader::new(bytes);
    let protocol_tag = r.u8()?;
    if protocol_tag != WIRE_PROTOCOL_TAG {
        return Err(ClaimsWireError::UnsupportedProtocolTag(protocol_tag));
    }
    let revision_from = r.u64()?;
    let revision_to = r.u64()?;
    if revision_from > revision_to {
        return Err(ClaimsWireError::RevisionRangeInverted);
    }
    let dimension = r.read_str("dimension")?;
    check_identifier("dimension", &dimension)?;
    let count = r.u32()? as usize;
    if count > MAX_WIRE_CLAIMS_PER_BATCH {
        return Err(ClaimsWireError::TooManyEntries { count });
    }
    let mut entries = Vec::with_capacity(count);
    let mut previous: Option<(i32, i32)> = None;
    for _ in 0..count {
        let (cx, cz) = (r.i32()?, r.i32()?);
        if let Some(prev) = previous {
            match (cx, cz).cmp(&prev) {
                std::cmp::Ordering::Less => return Err(ClaimsWireError::EntriesOutOfOrder),
                std::cmp::Ordering::Equal => {
                    return Err(ClaimsWireError::DuplicateChunk { cx, cz })
                }
                std::cmp::Ordering::Greater => {}
            }
        }
        previous = Some((cx, cz));
        let state = match r.u8()? {
            0 => None,
            1 => Some(r.read_cell()?),
            value => {
                return Err(ClaimsWireError::InvalidFlag {
                    field: "present",
                    value,
                })
            }
        };
        entries.push(ClaimsWireEntry { cx, cz, state });
    }
    if r.remaining() != 0 {
        return Err(ClaimsWireError::TrailingBytes {
            remaining: r.remaining(),
        });
    }
    Ok(ClaimsWireBatch {
        protocol_tag,
        revision_from,
        revision_to,
        dimension,
        entries,
    })
}

/// Unwrap a custom-payload frame into its `(channel, batch)` pair; the
/// channel is returned in canonical `namespace:path` spelling.
pub fn decode_custom_payload_frame(
    frame: &[u8],
) -> Result<(String, ClaimsWireBatch), ClaimsWireError> {
    let mut r = Reader::new(frame);
    let len = r.varint()?;
    if len < 0 || len as usize > MAX_WIRE_STRING_BYTES {
        // Read as unsigned bits: a negative i32 is a five-byte VarInt that
        // decodes to a huge length — StringTooLong names it better than the
        // raw sign bit.
        return Err(ClaimsWireError::StringTooLong {
            field: "channel",
            length: if len < 0 { len as u32 as usize } else { len as usize },
        });
    }
    let bytes = r.take(len as usize)?;
    let channel = String::from_utf8(bytes.to_vec())
        .map_err(|_| ClaimsWireError::InvalidUtf8 { field: "channel" })?;
    let parsed =
        ResourceLocation::parse(&channel).map_err(|_| ClaimsWireError::InvalidIdentifier {
            field: "channel",
            value: channel.clone(),
        })?;
    let batch = decode_claims_batch(r.rest())?;
    Ok((parsed.to_string(), batch))
}

// ------------------------------------------------------------ batch builds

/// Split a dimension's full claim set into snapshot batches of at most
/// `max_per_batch` entries (clamped to `1..=MAX_WIRE_CLAIMS_PER_BATCH`),
/// sorted by `(cx, cz)` — exactly what [`ClaimsWorld::claims`]'s `BTreeMap`
/// order already yields once filtered. `revision_from` is `0` and
/// `revision_to` is the world's current revision: a snapshot states what
/// is true *as of* that revision. An invalid `dimension` or a dimension
/// with no claims yields no batches.
pub fn build_dimension_sync_batches(
    world: &ClaimsWorld,
    dimension: &str,
    max_per_batch: usize,
) -> Vec<ClaimsWireBatch> {
    let Ok(loc) = ResourceLocation::parse(dimension) else {
        return Vec::new();
    };
    let dimension = loc.to_string();
    let cap = max_per_batch.clamp(1, MAX_WIRE_CLAIMS_PER_BATCH);
    let revision_to = world.revision();
    let mut batches = Vec::new();
    let mut entries: Vec<ClaimsWireEntry> = Vec::with_capacity(cap.min(1024));
    for (key, claim) in world.claims() {
        if key.dimension != dimension {
            continue;
        }
        if entries.len() == cap {
            batches.push(ClaimsWireBatch::new(
                0,
                revision_to,
                dimension.clone(),
                std::mem::take(&mut entries),
            ));
        }
        entries.push(ClaimsWireEntry {
            cx: key.cx,
            cz: key.cz,
            state: Some(ClaimsWireCell::from_claim(claim)),
        });
    }
    if !entries.is_empty() {
        batches.push(ClaimsWireBatch::new(0, revision_to, dimension, entries));
    }
    batches
}

/// Fold a run of [`ClaimsSyncDelta`]s into wire batches: deltas are grouped
/// by dimension (preserving delta order), chunked to
/// [`MAX_WIRE_CLAIMS_PER_BATCH`], and compacted so a chunk touched more
/// than once in a batch carries only its latest state. `revision_from` /
/// `revision_to` are the first and last delta revisions the batch covers,
/// so a client that kept every batch sees a gap-free per-dimension feed.
/// Output order is `(dimension, revision)`.
pub fn build_delta_batches(deltas: &[ClaimsSyncDelta]) -> Vec<ClaimsWireBatch> {
    let mut by_dimension: BTreeMap<String, Vec<&ClaimsSyncDelta>> = BTreeMap::new();
    for delta in deltas {
        let ClaimsSyncDelta::ClaimUpdated { key, .. } = delta;
        by_dimension
            .entry(key.dimension.clone())
            .or_default()
            .push(delta);
    }
    let mut batches = Vec::new();
    for (dimension, group) in by_dimension {
        for window in group.chunks(MAX_WIRE_CLAIMS_PER_BATCH) {
            let (revision_from, revision_to) = (window[0].revision(), window[window.len() - 1].revision());
            // BTreeMap<(i32,i32)> gives the wire's ascending (cx, cz) order
            // and keeps only each chunk's last state.
            let mut latest: BTreeMap<(i32, i32), Option<ClaimsWireCell>> = BTreeMap::new();
            for delta in window {
                let ClaimsSyncDelta::ClaimUpdated { key, claim, .. } = delta;
                latest.insert((key.cx, key.cz), claim.as_ref().map(ClaimsWireCell::from_claim));
            }
            let entries = latest
                .into_iter()
                .map(|((cx, cz), state)| ClaimsWireEntry { cx, cz, state })
                .collect();
            batches.push(ClaimsWireBatch::new(
                revision_from,
                revision_to,
                dimension.clone(),
                entries,
            ));
        }
    }
    batches
}

// ------------------------------------------------------------------- tests

#[cfg(test)]
mod tests {
    use super::*;
    use crate::claims::{ClaimChunkKey, ClaimsWorld};

    fn uuid(n: u8) -> String {
        format!("00000000-0000-0000-0000-0000000000{n:02}")
    }

    fn player_cell(n: u8) -> ClaimsWireCell {
        ClaimsWireCell {
            owner_kind: WireOwnerKind::Player,
            owner_id: uuid(n),
            sub_claim: "main".to_owned(),
            forceloaded: false,
            color_rgb: 0x0011_2233,
        }
    }

    fn entry(cx: i32, cz: i32, state: Option<ClaimsWireCell>) -> ClaimsWireEntry {
        ClaimsWireEntry { cx, cz, state }
    }

    fn batch(dim: &str, entries: Vec<ClaimsWireEntry>) -> ClaimsWireBatch {
        ClaimsWireBatch::new(1, 3, dim, entries)
    }

    #[test]
    fn batch_roundtrip_covers_every_cell_kind_and_removals() {
        let cells = vec![
            entry(-5, 9, Some(player_cell(7))),
            entry(
                0,
                0,
                Some(ClaimsWireCell {
                    owner_kind: WireOwnerKind::Server,
                    owner_id: String::new(),
                    sub_claim: "spawn".to_owned(),
                    forceloaded: true,
                    color_rgb: 0x00ff_8800,
                }),
            ),
            entry(
                12,
                -300,
                Some(ClaimsWireCell {
                    owner_kind: WireOwnerKind::Expired,
                    owner_id: uuid(9),
                    sub_claim: "main".to_owned(),
                    forceloaded: false,
                    color_rgb: 0,
                }),
            ),
            entry(4, 4, None), // a removal
        ];
        // Deliberately unsorted: the encoder sorts before writing.
        let batch = batch("minecraft:overworld", cells.clone().into_iter().rev().collect());
        let bytes = encode_claims_batch(&batch).unwrap();
        let decoded = decode_claims_batch(&bytes).unwrap();
        let mut sorted = cells.clone();
        sorted.sort_by_key(|e| (e.cx, e.cz));
        assert_eq!(decoded.entries, sorted);
        assert_eq!(decoded.revision_from, 1);
        assert_eq!(decoded.revision_to, 3);
        assert_eq!(decoded.dimension, "minecraft:overworld");
        // ChunkClaim conversion preserves the engine's shape exactly.
        let cell = decoded.entries[1].state.as_ref().unwrap();
        let claim = cell.to_claim();
        assert_eq!(claim.owner, ClaimOwner::Server);
        assert_eq!(claim.sub_claim, "spawn");
        // Encoding twice is bit-identical.
        assert_eq!(encode_claims_batch(&decoded).unwrap(), bytes);
    }

    #[test]
    fn frames_roundtrip_on_all_three_channels() {
        let batch = batch(
            "aeronautics:space",
            vec![entry(1, 1, Some(player_cell(1))), entry(2, 2, None)],
        );
        for channel in [
            CHANNEL_OPENPAC_CLAIMS_SYNC,
            CHANNEL_FTBCHUNKS_CLAIMS_SYNC,
            CHANNEL_MEGU_CLAIMS_V1,
        ] {
            let frame = encode_custom_payload_frame(channel, &batch).unwrap();
            let (decoded_channel, decoded) = decode_custom_payload_frame(&frame).unwrap();
            assert_eq!(decoded_channel, channel);
            assert_eq!(decoded, batch);
        }
        // The frame really is VarInt(len) + channel + payload.
        let frame =
            encode_custom_payload_frame(CHANNEL_MEGU_CLAIMS_V1, &batch).unwrap();
        assert_eq!(frame[0] as usize, CHANNEL_MEGU_CLAIMS_V1.len());
        assert_eq!(
            &frame[1..1 + CHANNEL_MEGU_CLAIMS_V1.len()],
            CHANNEL_MEGU_CLAIMS_V1.as_bytes()
        );
    }

    #[test]
    fn decode_rejects_every_corruption_fail_closed() {
        let batch = batch(
            "minecraft:overworld",
            vec![entry(1, 1, Some(player_cell(1))), entry(2, 2, None)],
        );
        let good = encode_claims_batch(&batch).unwrap();

        // Truncation at every byte boundary fails, never panics.
        for len in 0..good.len() {
            assert!(decode_claims_batch(&good[..len]).is_err(), "len {len}");
        }
        // Trailing bytes fail.
        let mut trailed = good.clone();
        trailed.push(0);
        assert!(matches!(
            decode_claims_batch(&trailed),
            Err(ClaimsWireError::TrailingBytes { .. })
        ));
        // Unknown protocol tag.
        let mut bad_tag = good.clone();
        bad_tag[0] = 99;
        assert_eq!(
            decode_claims_batch(&bad_tag),
            Err(ClaimsWireError::UnsupportedProtocolTag(99))
        );
        // Oversized entry count is rejected before allocation: count lives
        // at offset 19 (tag + 2×u64 + u16 + dim bytes) for this fixture.
        let dim_len = u16::from_be_bytes([good[17], good[18]]) as usize;
        let count_at = 19 + dim_len;
        let mut too_many = good.clone();
        too_many[count_at..count_at + 4].copy_from_slice(&(4097u32).to_be_bytes());
        assert_eq!(
            decode_claims_batch(&too_many),
            Err(ClaimsWireError::TooManyEntries { count: 4097 })
        );
        // Oversized string: rewrite the dimension length past the cap.
        let mut long_str = Vec::new();
        long_str.push(1u8);
        long_str.extend_from_slice(&1u64.to_be_bytes());
        long_str.extend_from_slice(&1u64.to_be_bytes());
        long_str.extend_from_slice(&300u16.to_be_bytes());
        long_str.extend_from_slice(&[b'a'; 300]);
        assert_eq!(
            decode_claims_batch(&long_str),
            Err(ClaimsWireError::StringTooLong {
                field: "dimension",
                length: 300
            })
        );
        // Invalid UTF-8 inside the dimension.
        let mut bad_utf8 = Vec::new();
        bad_utf8.push(1u8);
        bad_utf8.extend_from_slice(&1u64.to_be_bytes());
        bad_utf8.extend_from_slice(&1u64.to_be_bytes());
        bad_utf8.extend_from_slice(&2u16.to_be_bytes());
        bad_utf8.extend_from_slice(&[0xff, 0xfe]);
        assert_eq!(
            decode_claims_batch(&bad_utf8),
            Err(ClaimsWireError::InvalidUtf8 {
                field: "dimension"
            })
        );
        // encode sorts, so hand-build an unsorted stream instead.
        let mut manual = Vec::new();
        manual.push(1u8);
        manual.extend_from_slice(&1u64.to_be_bytes());
        manual.extend_from_slice(&1u64.to_be_bytes());
        manual.extend_from_slice(&(b"minecraft:overworld".len() as u16).to_be_bytes());
        manual.extend_from_slice(b"minecraft:overworld");
        manual.extend_from_slice(&2u32.to_be_bytes());
        manual.extend_from_slice(&2i32.to_be_bytes());
        manual.extend_from_slice(&2i32.to_be_bytes());
        manual.push(0);
        manual.extend_from_slice(&1i32.to_be_bytes());
        manual.extend_from_slice(&1i32.to_be_bytes());
        manual.push(0);
        assert_eq!(
            decode_claims_batch(&manual),
            Err(ClaimsWireError::EntriesOutOfOrder)
        );
    }

    #[test]
    fn decode_rejects_bad_owner_kind_flags_and_colors() {
        let cell = player_cell(1);
        let good = encode_claims_batch(&batch(
            "minecraft:overworld",
            vec![entry(0, 0, Some(cell))],
        ))
        .unwrap();
        // owner_kind byte sits after tag(1) + rev(16) + dimlen+dim(2+19) +
        // count(4) + cx(4) + cz(4) + present(1) = offset 51.
        let owner_kind_at = 1 + 16 + 2 + 19 + 4 + 4 + 4 + 1;
        let mut bad_kind = good.clone();
        bad_kind[owner_kind_at] = 7;
        assert_eq!(
            decode_claims_batch(&bad_kind),
            Err(ClaimsWireError::InvalidOwnerKind(7))
        );
        // forceloaded flag = owner_kind_at + 1 + (2 + uuid_len=36) + (2 + 4).
        let forceload_at = owner_kind_at + 1 + 38 + 6;
        let mut bad_flag = good.clone();
        bad_flag[forceload_at] = 9;
        assert_eq!(
            decode_claims_batch(&bad_flag),
            Err(ClaimsWireError::InvalidFlag {
                field: "forceloaded",
                value: 9
            })
        );
        // color out of 0xRRGGBB range.
        let color_at = forceload_at + 1;
        let mut bad_color = good.clone();
        bad_color[color_at..color_at + 4].copy_from_slice(&0x0100_0000u32.to_be_bytes());
        assert_eq!(
            decode_claims_batch(&bad_color),
            Err(ClaimsWireError::ColorOutOfRange(0x0100_0000))
        );
        // Inverted revision range.
        let mut inverted = good.clone();
        inverted[1..9].copy_from_slice(&5u64.to_be_bytes());
        inverted[9..17].copy_from_slice(&1u64.to_be_bytes());
        assert_eq!(
            decode_claims_batch(&inverted),
            Err(ClaimsWireError::RevisionRangeInverted)
        );
        // Encode-side: a batch with a bad tag or oversized data refuses too.
        let mut bad = batch("minecraft:overworld", vec![]);
        bad.protocol_tag = 2;
        assert!(matches!(
            encode_claims_batch(&bad),
            Err(ClaimsWireError::UnsupportedProtocolTag(2))
        ));
        let dup = batch(
            "minecraft:overworld",
            vec![entry(1, 1, None), entry(1, 1, Some(player_cell(2)))],
        );
        assert!(matches!(
            encode_claims_batch(&dup),
            Err(ClaimsWireError::DuplicateChunk { cx: 1, cz: 1 })
        ));
    }

    #[test]
    fn frames_reject_bad_channel_and_varint_overflow() {
        let batch = batch("minecraft:overworld", vec![entry(0, 0, None)]);
        for channel in ["", "not a channel", "a:b:c", "UPPER:x"] {
            assert!(matches!(
                encode_custom_payload_frame(channel, &batch),
                Err(ClaimsWireError::InvalidIdentifier { .. })
                    | Err(ClaimsWireError::StringTooLong { .. })
            ));
        }
        // A VarInt that never terminates is too long.
        let frame = [0x80, 0x80, 0x80, 0x80, 0x80, 0x01];
        assert_eq!(
            decode_custom_payload_frame(&frame),
            Err(ClaimsWireError::VarIntTooLong)
        );
        // A channel length above the cap is rejected before the read.
        let frame = [0x81, 0x02]; // varint 257
        assert_eq!(
            decode_custom_payload_frame(&frame),
            Err(ClaimsWireError::StringTooLong {
                field: "channel",
                length: 257
            })
        );
    }

    #[test]
    fn dimension_sync_batches_split_sort_and_stay_under_the_cap() {
        let mut world = ClaimsWorld::new();
        let sub = SubClaimId::main();
        // Claims in two dimensions; the nether ones must not leak in.
        for i in 0..10 {
            world
                .claim_chunk_as(
                    &uuid(1),
                    false,
                    ClaimChunkKey::new("overworld", i * 3 - 12, i - 5).unwrap(),
                    &sub,
                )
                .unwrap();
        }
        world
            .claim_server_chunk(true, ClaimChunkKey::new("the_nether", 0, 0).unwrap(), &sub)
            .unwrap();

        // One batch when everything fits, preserving (cx, cz) order.
        let batches = build_dimension_sync_batches(&world, "overworld", 100);
        assert_eq!(batches.len(), 1);
        assert_eq!(batches[0].entries.len(), 10);
        assert_eq!(batches[0].revision_from, 0);
        assert_eq!(batches[0].revision_to, world.revision());
        assert_eq!(batches[0].dimension, "minecraft:overworld");
        let coords: Vec<(i32, i32)> =
            batches[0].entries.iter().map(|e| (e.cx, e.cz)).collect();
        let mut sorted = coords.clone();
        sorted.sort();
        assert_eq!(coords, sorted);
        assert!(batches[0]
            .entries
            .iter()
            .all(|e| e.state.as_ref().unwrap().owner_kind == WireOwnerKind::Player));

        // Chunked at max_per_batch; every batch round-trips.
        let batches = build_dimension_sync_batches(&world, "minecraft:overworld", 4);
        assert_eq!(batches.len(), 3);
        assert_eq!(
            batches.iter().map(|b| b.entries.len()).collect::<Vec<_>>(),
            vec![4, 4, 2]
        );
        for b in &batches {
            let bytes = encode_claims_batch(b).unwrap();
            assert_eq!(&decode_claims_batch(&bytes).unwrap(), b);
        }
        // The flattened stream still covers every claim, in order.
        let all: Vec<(i32, i32)> = batches
            .iter()
            .flat_map(|b| b.entries.iter().map(|e| (e.cx, e.cz)))
            .collect();
        assert_eq!(all, coords);

        // Unknown/invalid dimensions yield nothing; over-cap max_per_batch
        // is clamped rather than exploding the batch count.
        assert!(build_dimension_sync_batches(&world, "bad dim", 4).is_empty());
        assert!(build_dimension_sync_batches(&world, "the_end", 4).is_empty());
        assert_eq!(build_dimension_sync_batches(&world, "overworld", usize::MAX).len(), 1);
        assert_eq!(build_dimension_sync_batches(&world, "overworld", 0).len(), 10);
    }

    #[test]
    fn delta_batches_group_compact_and_carry_revision_ranges() {
        let mut world = ClaimsWorld::new();
        let sub = SubClaimId::main();
        let key = |dim: &str, cx, cz| ClaimChunkKey::new(dim, cx, cz).unwrap();
        world.claim_chunk(&uuid(1), key("overworld", 0, 0), &sub).unwrap();
        world.claim_chunk(&uuid(1), key("overworld", 1, 0), &sub).unwrap();
        world.claim_chunk(&uuid(2), key("the_nether", 5, 5), &sub).unwrap();
        world.set_forceload(&uuid(1), &key("overworld", 0, 0), true).unwrap();
        world.unclaim_chunk(&uuid(1), &key("overworld", 1, 0)).unwrap();
        // The wire cell carries the engine-computed claim color.
        let ow_color = world.claim_at(&key("overworld", 0, 0)).unwrap().color_rgb;
        let deltas = world.drain_deltas();
        assert_eq!(deltas.len(), 5);

        let batches = build_delta_batches(&deltas);
        // Two dimensions -> two batches in (dimension, revision) order;
        // overworld covers revisions 1..5 (its own 1,2,4,5 — the range
        // still bounds them), nether covers 3.
        assert_eq!(batches.len(), 2);
        let (ow, nt) = (&batches[0], &batches[1]);
        assert_eq!(ow.dimension, "minecraft:overworld");
        assert_eq!((ow.revision_from, ow.revision_to), (1, 5));
        assert_eq!(nt.dimension, "minecraft:the_nether");
        assert_eq!((nt.revision_from, nt.revision_to), (3, 3));
        // (0,0) collapsed claim+forceload into the final state; (1,0) ends
        // as a removal.
        assert_eq!(ow.entries.len(), 2);
        assert_eq!(
            ow.entries[0],
            entry(
                0,
                0,
                Some(ClaimsWireCell {
                    owner_kind: WireOwnerKind::Player,
                    owner_id: uuid(1),
                    sub_claim: "main".to_owned(),
                    forceloaded: true,
                    color_rgb: ow_color,
                })
            )
        );
        assert_eq!(ow.entries[1], entry(1, 0, None));
        for b in &batches {
            let bytes = encode_claims_batch(b).unwrap();
            assert_eq!(&decode_claims_batch(&bytes).unwrap(), b);
        }

        // An empty feed produces no batches.
        assert!(build_delta_batches(&[]).is_empty());
        // A drained world has nothing left to send.
        assert!(world.pending_deltas().is_empty());
    }
}
