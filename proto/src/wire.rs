// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
//! Minecraft Java wire primitives: `VarInt`/`VarLong`, big-endian scalars,
//! bounded strings, UUIDs, packed block positions, multi-word bit sets and
//! framed packets (`VarInt(len) + VarInt(id) + payload`).
//!
//! Every read fails closed through [`ProtoError`]: truncated input, overlong
//! varints, forged lengths, invalid UTF-8 and trailing garbage are all errors,
//! never panics. These are the same layouts `src/play_protocol.rs` and
//! `client-app/src/protocol.rs` implement twice today; this module is the
//! single canonical copy they can converge on.

use std::fmt;

/// Vanilla's maximum for most length-prefixed text (`Utf8String`, 32767 chars).
pub const MAX_STRING_CHARS: usize = 32_767;

/// Upper bound accepted on a frame's declared length (`VarInt21FrameDecoder`
/// allows three VarInt bytes; this stays far below that while covering every
/// vanilla packet, including chunk columns).
pub const MAX_FRAME_LENGTH: usize = 16 * 1024 * 1024;

/// The vanilla-compatible per-frame cap: 2 MiB (`2_097_152` bytes), the
/// bound a stock client/server applies to a single packet frame. Offered as
/// the conservative [`PacketCapPolicy`] for deployments that want vanilla
/// semantics without touching the crate-wide [`MAX_FRAME_LENGTH`] default.
pub const VANILLA_FRAME_BYTES: usize = 2 * 1024 * 1024;

/// Hard ceiling for [`PacketCapPolicy`]: 32 MiB. Sized for giant modded
/// recipe/NBT payloads while still failing closed on absurd declarations —
/// a policy can never widen the accepted frame beyond this.
pub const MAX_EXTENDED_FRAME_BYTES: usize = 32 * 1024 * 1024;

/// Bound on anonymous-NBT payload size where a raw span is decoded
/// (configuration registry entries, components). Matches the client reader.
const MAX_NBT_BYTES: usize = 2 * 1024 * 1024;

/// Maximum NBT nesting depth, same bound the existing decoders use.
const MAX_NBT_DEPTH: usize = 32;

/// Every wire-level failure. The variants carry enough context to log a useful
/// rejection without ever exposing or panicking on attacker-controlled sizes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProtoError {
    /// Fewer bytes remain than the field needs.
    UnexpectedEof { needed: usize, remaining: usize },
    /// VarInt without a terminator within five bytes.
    VarIntTooLong,
    /// VarInt whose fifth byte carries bits that do not fit in 32 bits.
    VarIntOverflow,
    /// VarLong without a terminator within ten bytes.
    VarLongTooLong,
    /// VarLong whose tenth byte carries bits that do not fit in 64 bits.
    VarLongOverflow,
    /// A length or count prefix decoded to a negative number.
    NegativeLength,
    /// A length/count exceeds the bound the caller set for it.
    LengthOutOfBounds { length: usize, max: usize },
    /// Bytes that must be UTF-8 were not.
    InvalidUtf8,
    /// Decoded string exceeds the character cap (UTF-16 units, like vanilla).
    StringTooLong { units: usize, max: usize },
    /// A boolean wire byte was neither 0 nor 1.
    BoolOutOfRange(u8),
    /// A frame/payload declared more bytes than the frame bound allows.
    FrameTooLarge { length: usize, max: usize },
    /// The packet id has no entry for the direction the codec was decoding.
    UnknownPacketId { id: i32, context: &'static str },
    /// A decoded enum/variant selector has no meaning in this protocol.
    InvalidDiscriminant { value: i64, what: &'static str },
    /// A parsed structure violates an invariant (e.g. empty SetEquipment).
    Malformed(&'static str),
    /// Version-pack JSON could not be parsed or violates the normalized shape.
    VersionPack(&'static str),
    /// A registry/table received a name or id it already holds under a
    /// different spelling, or a JSON object repeated a key.
    DuplicateEntry(&'static str),
    /// A handshake asked for a protocol version no registered codec speaks.
    UnsupportedVersion {
        /// The protocol number the peer requested.
        requested: i32,
        /// Protocol numbers with a registered codec, ascending.
        supported: Vec<i32>,
    },
}

impl fmt::Display for ProtoError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnexpectedEof { needed, remaining } => {
                write!(f, "truncated wire: need {needed} byte(s), {remaining} left")
            }
            Self::VarIntTooLong => write!(f, "varint exceeds five bytes"),
            Self::VarIntOverflow => write!(f, "varint fifth byte overflows 32 bits"),
            Self::VarLongTooLong => write!(f, "varlong exceeds ten bytes"),
            Self::VarLongOverflow => write!(f, "varlong tenth byte overflows 64 bits"),
            Self::NegativeLength => write!(f, "negative length on the wire"),
            Self::LengthOutOfBounds { length, max } => {
                write!(f, "length {length} exceeds bound {max}")
            }
            Self::InvalidUtf8 => write!(f, "invalid UTF-8 on the wire"),
            Self::StringTooLong { units, max } => {
                write!(f, "string of {units} UTF-16 units exceeds cap {max}")
            }
            Self::BoolOutOfRange(b) => write!(f, "boolean wire byte {b:#04x} is not 0 or 1"),
            Self::FrameTooLarge { length, max } => {
                write!(f, "frame of {length} bytes exceeds bound {max}")
            }
            Self::UnknownPacketId { id, context } => {
                write!(f, "unknown packet id {id:#04x} in {context}")
            }
            Self::InvalidDiscriminant { value, what } => {
                write!(f, "invalid {what} discriminant {value}")
            }
            Self::Malformed(what) => write!(f, "malformed structure: {what}"),
            Self::VersionPack(what) => write!(f, "invalid version pack: {what}"),
            Self::DuplicateEntry(what) => write!(f, "duplicate entry: {what}"),
            Self::UnsupportedVersion {
                requested,
                supported,
            } => {
                write!(f, "unsupported protocol version {requested} (supported:")?;
                for v in supported {
                    write!(f, " {v}")?;
                }
                write!(f, ")")
            }
        }
    }
}

impl std::error::Error for ProtoError {}

/// A 64-bit-packed block position: `x:26 | z:26 | y:12`, all signed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct BlockPos {
    pub x: i32,
    pub y: i32,
    pub z: i32,
}

impl BlockPos {
    pub fn new(x: i32, y: i32, z: i32) -> Self {
        Self { x, y, z }
    }

    /// The protocol's 8-byte big-endian packed form.
    pub fn pack(self) -> i64 {
        ((i64::from(self.x) & 0x3ff_ffff) << 38)
            | ((i64::from(self.z) & 0x3ff_ffff) << 12)
            | (i64::from(self.y) & 0xfff)
    }

    /// Inverse of [`BlockPos::pack`], sign-extending each field.
    pub fn unpack(packed: i64) -> Self {
        Self {
            x: (packed >> 38) as i32,
            y: ((packed << 52) >> 52) as i32,
            z: ((packed << 26) >> 38) as i32,
        }
    }
}

/// A vanilla `BitSet`: `VarInt` word count, then that many big-endian `u64`
/// words where bit `i` of the set is bit `i % 64` of word `i / 64`.
///
/// Vanilla light masks in 1.21.1 only ever need one word, but the type is
/// genuinely variable-length on the wire (dimension mods and taller worlds
/// cross 64 sections), so this keeps the full multi-word form.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BitSet {
    words: Vec<u64>,
}

impl BitSet {
    pub fn new() -> Self {
        Self { words: Vec::new() }
    }

    pub fn from_words(words: Vec<u64>) -> Self {
        Self { words }
    }

    /// Single-word convenience constructor for the common <=64-section case.
    pub fn from_u64(word: u64) -> Self {
        Self { words: vec![word] }
    }

    pub fn words(&self) -> &[u64] {
        &self.words
    }

    pub fn get(&self, index: usize) -> bool {
        self.words
            .get(index / 64)
            .is_some_and(|w| (w >> (index % 64)) & 1 != 0)
    }

    pub fn set(&mut self, index: usize, value: bool) {
        let word = index / 64;
        if word >= self.words.len() {
            self.words.resize(word + 1, 0);
        }
        if value {
            self.words[word] |= 1 << (index % 64);
        } else {
            self.words[word] &= !(1 << (index % 64));
        }
    }

    pub fn count_ones(&self) -> usize {
        self.words.iter().map(|w| w.count_ones() as usize).sum()
    }

    pub fn is_empty(&self) -> bool {
        self.words.iter().all(|&w| w == 0)
    }

    /// Indices of set bits, in ascending order.
    pub fn set_bits(&self) -> impl Iterator<Item = usize> + '_ {
        self.words.iter().enumerate().flat_map(|(w, &word)| {
            (0..64).filter_map(move |b| ((word >> b) & 1 != 0).then_some(w * 64 + b))
        })
    }
}

/// A bounds-checked cursor over one packet body. `Clone`/`Copy` are
/// deliberately absent: a decoder must consume fields in order.
pub struct WireReader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> WireReader<'a> {
    pub fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    pub fn remaining(&self) -> usize {
        self.buf.len() - self.pos
    }

    pub fn rest(&self) -> &'a [u8] {
        &self.buf[self.pos..]
    }

    /// The whole backing buffer — for pairing with [`WireReader::position`]
    /// when a decoder needs the exact byte span a value occupied.
    pub fn buffer(&self) -> &'a [u8] {
        self.buf
    }

    pub fn position(&self) -> usize {
        self.pos
    }

    /// Fails when unconsumed bytes remain — the canonical way to reject
    /// trailing garbage at the end of a packet body.
    pub fn finish(self) -> Result<(), ProtoError> {
        if self.pos == self.buf.len() {
            Ok(())
        } else {
            Err(ProtoError::Malformed("trailing bytes after packet body"))
        }
    }

    pub fn take(&mut self, n: usize) -> Result<&'a [u8], ProtoError> {
        let end = self
            .pos
            .checked_add(n)
            .ok_or(ProtoError::Malformed("length arithmetic overflow"))?;
        if end > self.buf.len() {
            return Err(ProtoError::UnexpectedEof {
                needed: n,
                remaining: self.remaining(),
            });
        }
        let out = &self.buf[self.pos..end];
        self.pos = end;
        Ok(out)
    }

    pub fn read_u8(&mut self) -> Result<u8, ProtoError> {
        Ok(self.take(1)?[0])
    }

    pub fn read_i8(&mut self) -> Result<i8, ProtoError> {
        Ok(self.read_u8()? as i8)
    }

    /// Wire boolean: strictly 0 or 1, like the vanilla codec's `readBoolean`
    /// paired with fail-closed decoding elsewhere in this codebase.
    pub fn read_bool(&mut self) -> Result<bool, ProtoError> {
        match self.read_u8()? {
            0 => Ok(false),
            1 => Ok(true),
            other => Err(ProtoError::BoolOutOfRange(other)),
        }
    }

    /// Permissive boolean for codecs that mirror vanilla's `byte != 0` read.
    pub fn read_bool_lenient(&mut self) -> Result<bool, ProtoError> {
        Ok(self.read_u8()? != 0)
    }

    pub fn read_u16(&mut self) -> Result<u16, ProtoError> {
        Ok(u16::from_be_bytes(self.take(2)?.try_into().unwrap()))
    }

    pub fn read_i16(&mut self) -> Result<i16, ProtoError> {
        Ok(i16::from_be_bytes(self.take(2)?.try_into().unwrap()))
    }

    pub fn read_i32(&mut self) -> Result<i32, ProtoError> {
        Ok(i32::from_be_bytes(self.take(4)?.try_into().unwrap()))
    }

    pub fn read_u32(&mut self) -> Result<u32, ProtoError> {
        Ok(u32::from_be_bytes(self.take(4)?.try_into().unwrap()))
    }

    pub fn read_i64(&mut self) -> Result<i64, ProtoError> {
        Ok(i64::from_be_bytes(self.take(8)?.try_into().unwrap()))
    }

    pub fn read_u64(&mut self) -> Result<u64, ProtoError> {
        Ok(u64::from_be_bytes(self.take(8)?.try_into().unwrap()))
    }

    pub fn read_f32(&mut self) -> Result<f32, ProtoError> {
        Ok(f32::from_be_bytes(self.take(4)?.try_into().unwrap()))
    }

    pub fn read_f64(&mut self) -> Result<f64, ProtoError> {
        Ok(f64::from_be_bytes(self.take(8)?.try_into().unwrap()))
    }

    /// Protocol VarInt: up to five bytes of little-endian 7-bit groups decoded
    /// as the low 32 bits of an `i32`. The fifth byte must fit the remaining
    /// four bits — a stricter bound than vanilla's "five bytes then give up",
    /// which silently accepts non-canonical encodings.
    pub fn read_varint(&mut self) -> Result<i32, ProtoError> {
        let mut value = 0u32;
        for i in 0..5 {
            let byte = self.read_u8()?;
            if i == 4 && byte & 0xf0 != 0 {
                return Err(if byte & 0x80 != 0 {
                    ProtoError::VarIntTooLong
                } else {
                    ProtoError::VarIntOverflow
                });
            }
            value |= u32::from(byte & 0x7f) << (7 * i);
            if byte & 0x80 == 0 {
                return Ok(value as i32);
            }
        }
        Err(ProtoError::VarIntTooLong)
    }

    /// Protocol VarLong: up to ten bytes decoded as the low 64 bits of an
    /// `i64`, with the same strict bound on the final byte.
    pub fn read_varlong(&mut self) -> Result<i64, ProtoError> {
        let mut value = 0u64;
        for i in 0..10 {
            let byte = self.read_u8()?;
            if i == 9 && byte & 0xfe != 0 {
                return Err(if byte & 0x80 != 0 {
                    ProtoError::VarLongTooLong
                } else {
                    ProtoError::VarLongOverflow
                });
            }
            value |= u64::from(byte & 0x7f) << (7 * i);
            if byte & 0x80 == 0 {
                return Ok(value as i64);
            }
        }
        Err(ProtoError::VarLongTooLong)
    }

    /// VarInt that must be non-negative, for lengths and counts.
    pub fn read_count(&mut self, max: usize) -> Result<usize, ProtoError> {
        let value = self.read_varint()?;
        if value < 0 {
            return Err(ProtoError::NegativeLength);
        }
        let value = value as usize;
        if value > max {
            return Err(ProtoError::LengthOutOfBounds {
                length: value,
                max,
            });
        }
        Ok(value)
    }

    /// `VarInt(len)` + `len` raw bytes, bounded.
    pub fn read_bytes(&mut self, max: usize) -> Result<&'a [u8], ProtoError> {
        let len = self.read_count(max)?;
        self.take(len)
    }

    /// `VarInt(len)` + UTF-8. `max_chars` is the vanilla-style bound on
    /// *decoded* length measured in UTF-16 code units; the byte length is
    /// pre-bounded at `max_chars * 4` so a forged prefix cannot force a large
    /// allocation.
    pub fn read_string(&mut self, max_chars: usize) -> Result<String, ProtoError> {
        let byte_len = self.read_count(max_chars.saturating_mul(4))?;
        let raw = self.take(byte_len)?;
        let text = std::str::from_utf8(raw).map_err(|_| ProtoError::InvalidUtf8)?;
        let units: usize = text.chars().map(char::len_utf16).sum();
        if units > max_chars {
            return Err(ProtoError::StringTooLong {
                units,
                max: max_chars,
            });
        }
        Ok(text.to_owned())
    }

    pub fn read_uuid(&mut self) -> Result<[u8; 16], ProtoError> {
        Ok(self.take(16)?.try_into().unwrap())
    }

    pub fn read_position(&mut self) -> Result<BlockPos, ProtoError> {
        Ok(BlockPos::unpack(self.read_i64()?))
    }

    /// `VarInt(n)` + `n` big-endian `u64` words, bounded by `max_words`.
    pub fn read_bitset(&mut self, max_words: usize) -> Result<BitSet, ProtoError> {
        let count = self.read_count(max_words)?;
        let mut words = Vec::with_capacity(count.min(64));
        for _ in 0..count {
            words.push(self.read_u64()?);
        }
        Ok(BitSet::from_words(words))
    }

    /// `VarInt(n)` + `n` little-endian bytes — the 1.21.5+
    /// (`ByteBufCodecs.BIT_SET`) `java.util.BitSet` form where bit `i` is bit
    /// `i % 8` of byte `i / 8`, bounded by `max_bytes`. The byte count is not
    /// required to be a multiple of eight: the tail word keeps its unset high
    /// bits zero.
    pub fn read_bitset_bytes(&mut self, max_bytes: usize) -> Result<BitSet, ProtoError> {
        let count = self.read_count(max_bytes)?;
        let bytes = self.take(count)?;
        let mut words = Vec::with_capacity(count.div_ceil(8).min(64));
        for chunk in bytes.chunks(8) {
            let mut word = [0u8; 8];
            word[..chunk.len()].copy_from_slice(chunk);
            words.push(u64::from_le_bytes(word));
        }
        Ok(BitSet::from_words(words))
    }

    /// Consume one anonymous network NBT value and return its raw byte span
    /// (including the leading tag byte). Used where the canonical IR carries
    /// NBT opaquely — registry-data entries, component payloads — so callers
    /// keep exact bytes without this crate needing an NBT model.
    ///
    /// `nbt_skip` enforces the same shape and bounds as the server's
    /// `nbt_wire_skip`: tag bytes 0..=12, byte/int/long arrays capped at
    /// 1<<24 elements, lists at 1<<20, depth at [`MAX_NBT_DEPTH`].
    pub fn read_anonymous_nbt(&mut self) -> Result<&'a [u8], ProtoError> {
        let start = self.pos;
        let tag = self.read_u8()?;
        nbt_skip_value(self.buf, &mut self.pos, tag, 0)?;
        if self.pos - start > MAX_NBT_BYTES {
            return Err(ProtoError::LengthOutOfBounds {
                length: self.pos - start,
                max: MAX_NBT_BYTES,
            });
        }
        Ok(&self.buf[start..self.pos])
    }
}

/// Advance `pos` by `n`, failing on overflow or overrun.
fn nbt_take(data: &[u8], pos: &mut usize, n: usize) -> Result<(), ProtoError> {
    let end = pos
        .checked_add(n)
        .ok_or(ProtoError::Malformed("NBT length overflow"))?;
    if end > data.len() {
        return Err(ProtoError::UnexpectedEof {
            needed: n,
            remaining: data.len() - *pos,
        });
    }
    *pos = end;
    Ok(())
}

/// Read a big-endian unsigned of `width` bytes.
fn nbt_be(data: &[u8], pos: &mut usize, width: usize) -> Result<u32, ProtoError> {
    if data.len() - *pos < width {
        return Err(ProtoError::UnexpectedEof {
            needed: width,
            remaining: data.len() - *pos,
        });
    }
    let mut v = 0u32;
    for &b in &data[*pos..*pos + width] {
        v = (v << 8) | u32::from(b);
    }
    *pos += width;
    Ok(v)
}

/// Skip the payload of a value whose tag byte is `tag`. `pos` indexes `data`.
fn nbt_skip_value(data: &[u8], pos: &mut usize, tag: u8, depth: usize) -> Result<(), ProtoError> {
    if depth > MAX_NBT_DEPTH {
        return Err(ProtoError::Malformed("NBT nesting too deep"));
    }
    match tag {
        0 => Ok(()),
        1 => nbt_take(data, pos, 1),
        2 => nbt_take(data, pos, 2),
        3 | 5 => nbt_take(data, pos, 4),
        4 | 6 => nbt_take(data, pos, 8),
        7 | 11 | 12 => {
            let width = if tag == 7 { 1 } else if tag == 11 { 4 } else { 8 };
            let n = nbt_be(data, pos, 4)? as usize;
            if n > (1 << 24) {
                return Err(ProtoError::LengthOutOfBounds {
                    length: n,
                    max: 1 << 24,
                });
            }
            nbt_take(data, pos, n.saturating_mul(width))
        }
        8 => {
            let n = nbt_be(data, pos, 2)? as usize;
            nbt_take(data, pos, n)
        }
        9 => {
            if *pos >= data.len() {
                return Err(ProtoError::UnexpectedEof {
                    needed: 1,
                    remaining: 0,
                });
            }
            let elem = data[*pos];
            *pos += 1;
            let n = nbt_be(data, pos, 4)? as usize;
            if n > (1 << 20) {
                return Err(ProtoError::LengthOutOfBounds {
                    length: n,
                    max: 1 << 20,
                });
            }
            for _ in 0..n {
                nbt_skip_value(data, pos, elem, depth + 1)?;
            }
            Ok(())
        }
        10 => loop {
            let Some(&child) = data.get(*pos) else {
                return Err(ProtoError::UnexpectedEof {
                    needed: 1,
                    remaining: 0,
                });
            };
            *pos += 1;
            if child == 0 {
                return Ok(());
            }
            let name_len = nbt_be(data, pos, 2)? as usize;
            nbt_take(data, pos, name_len)?;
            nbt_skip_value(data, pos, child, depth + 1)?;
        },
        _ => Err(ProtoError::InvalidDiscriminant {
            value: i64::from(tag),
            what: "NBT tag",
        }),
    }
}

/// An append-side packet body under construction.
#[derive(Default)]
pub struct WireWriter {
    buf: Vec<u8>,
}

impl WireWriter {
    pub fn new() -> Self {
        Self { buf: Vec::new() }
    }

    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            buf: Vec::with_capacity(capacity),
        }
    }

    /// Wrap an existing buffer, appending after its current contents. Pair
    /// with [`WireWriter::into_vec`] to write into a caller-owned `Vec`
    /// without copying.
    pub fn from_vec(buf: Vec<u8>) -> Self {
        Self { buf }
    }

    pub fn as_slice(&self) -> &[u8] {
        &self.buf
    }

    pub fn into_vec(self) -> Vec<u8> {
        self.buf
    }

    /// Append raw bytes (already-encoded fields).
    pub fn raw(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
    }

    pub fn write_u8(&mut self, v: u8) {
        self.buf.push(v);
    }

    pub fn write_i8(&mut self, v: i8) {
        self.buf.push(v as u8);
    }

    pub fn write_bool(&mut self, v: bool) {
        self.buf.push(u8::from(v));
    }

    pub fn write_u16(&mut self, v: u16) {
        self.buf.extend_from_slice(&v.to_be_bytes());
    }

    pub fn write_i16(&mut self, v: i16) {
        self.buf.extend_from_slice(&v.to_be_bytes());
    }

    pub fn write_i32(&mut self, v: i32) {
        self.buf.extend_from_slice(&v.to_be_bytes());
    }

    pub fn write_u32(&mut self, v: u32) {
        self.buf.extend_from_slice(&v.to_be_bytes());
    }

    pub fn write_i64(&mut self, v: i64) {
        self.buf.extend_from_slice(&v.to_be_bytes());
    }

    pub fn write_u64(&mut self, v: u64) {
        self.buf.extend_from_slice(&v.to_be_bytes());
    }

    pub fn write_f32(&mut self, v: f32) {
        self.buf.extend_from_slice(&v.to_be_bytes());
    }

    pub fn write_f64(&mut self, v: f64) {
        self.buf.extend_from_slice(&v.to_be_bytes());
    }

    /// Write `v` as a VarInt (five or fewer bytes of 7-bit groups).
    pub fn write_varint(&mut self, v: i32) {
        write_varint_into(&mut self.buf, v);
    }

    /// Write `v` as a VarLong (ten or fewer bytes of 7-bit groups).
    pub fn write_varlong(&mut self, v: i64) {
        write_varlong_into(&mut self.buf, v);
    }

    /// `VarInt(len)` + raw bytes.
    pub fn write_bytes(&mut self, bytes: &[u8]) {
        self.write_varint(bytes.len() as i32);
        self.buf.extend_from_slice(bytes);
    }

    /// `VarInt(len)` + UTF-8 bytes. Callers bound semantic length; the wire
    /// form itself can carry any byte length.
    pub fn write_string(&mut self, s: &str) {
        self.write_bytes(s.as_bytes());
    }

    pub fn write_uuid(&mut self, uuid: &[u8; 16]) {
        self.buf.extend_from_slice(uuid);
    }

    pub fn write_position(&mut self, pos: BlockPos) {
        self.write_i64(pos.pack());
    }

    /// `VarInt(n)` + `n` big-endian `u64` words.
    pub fn write_bitset(&mut self, set: &BitSet) {
        self.write_varint(set.words.len() as i32);
        for &word in &set.words {
            self.write_u64(word);
        }
    }

    /// `VarInt(n)` + `n` little-endian bytes — the 1.21.5+
    /// `java.util.BitSet.toByteArray` form, which drops trailing zero bytes.
    /// The trimmed length is computed over the words directly so the encode
    /// never materializes the intermediate byte buffer.
    pub fn write_bitset_bytes(&mut self, set: &BitSet) {
        let mut len = 0usize;
        for (i, &word) in set.words.iter().enumerate() {
            let used = word.to_le_bytes().iter().rposition(|&b| b != 0);
            if let Some(last) = used {
                len = i * 8 + last + 1;
            }
        }
        self.write_varint(len as i32);
        let full_words = len / 8;
        self.buf.reserve(len);
        for &word in &set.words[..full_words] {
            self.buf.extend_from_slice(&word.to_le_bytes());
        }
        if !len.is_multiple_of(8) {
            self.buf
                .extend_from_slice(&set.words[full_words].to_le_bytes()[..len % 8]);
        }
    }
}

/// Canonical encoded length of `v` as a VarInt — the exact byte count
/// [`WireWriter::write_varint`] emits, 1–5 bytes of 7-bit groups.
#[inline]
pub const fn varint_len(v: i32) -> usize {
    let mut v = v as u32;
    let mut len = 1;
    while v >= 0x80 {
        v >>= 7;
        len += 1;
    }
    len
}

/// Canonical encoded length of `v` as a VarLong — 1–10 bytes of 7-bit groups.
#[inline]
pub const fn varlong_len(v: i64) -> usize {
    let mut v = v as u64;
    let mut len = 1;
    while v >= 0x80 {
        v >>= 7;
        len += 1;
    }
    len
}

/// Append `v`'s VarInt encoding to `out`. The bytes are staged on the stack so
/// the whole encoding lands in one reserve-and-copy instead of up to five
/// individually bounds-checked pushes.
pub fn write_varint_into(out: &mut Vec<u8>, v: i32) {
    let mut v = v as u32;
    let mut buf = [0u8; 5];
    let mut len = 0;
    loop {
        let mut b = (v & 0x7f) as u8;
        v >>= 7;
        if v != 0 {
            b |= 0x80;
        }
        buf[len] = b;
        len += 1;
        if v == 0 {
            break;
        }
    }
    out.extend_from_slice(&buf[..len]);
}

/// Append `v`'s VarLong encoding to `out`, same staged-copy scheme as
/// [`write_varint_into`].
pub fn write_varlong_into(out: &mut Vec<u8>, v: i64) {
    let mut v = v as u64;
    let mut buf = [0u8; 10];
    let mut len = 0;
    loop {
        let mut b = (v & 0x7f) as u8;
        v >>= 7;
        if v != 0 {
            b |= 0x80;
        }
        buf[len] = b;
        len += 1;
        if v == 0 {
            break;
        }
    }
    out.extend_from_slice(&buf[..len]);
}

/// Write the protocol frame `VarInt(len(body)) + VarInt(id) + payload`, where
/// the length counts the packet-id VarInt plus the payload.
///
/// Both header varints encode straight into `out` — [`varint_len`] computes
/// their widths without materializing them — so a frame costs one reserve and
/// zero intermediate buffers.
pub fn write_frame(out: &mut Vec<u8>, packet_id: i32, payload: &[u8]) {
    let body_len = varint_len(packet_id) + payload.len();
    out.reserve(varint_len(body_len as i32) + body_len);
    write_varint_into(out, body_len as i32);
    write_varint_into(out, packet_id);
    out.extend_from_slice(payload);
}

/// Read one frame's declared length and return `(packet_id, payload)`,
/// requiring the frame to be exactly the declared length with a non-negative
/// packet id. `frame` must be the whole `VarInt(len) + VarInt(id) + payload`
/// byte sequence.
pub fn read_frame(frame: &[u8]) -> Result<(i32, &[u8]), ProtoError> {
    read_frame_bounded(frame, MAX_FRAME_LENGTH)
}

/// [`read_frame`] with an explicit declared-length bound.
pub fn read_frame_bounded(frame: &[u8], max_length: usize) -> Result<(i32, &[u8]), ProtoError> {
    let mut r = WireReader::new(frame);
    let len = r.read_count(max_length)?;
    if len != r.remaining() {
        return Err(ProtoError::Malformed("frame length does not match the buffer"));
    }
    let id = r.read_varint()?;
    if id < 0 {
        return Err(ProtoError::InvalidDiscriminant {
            value: i64::from(id),
            what: "packet id",
        });
    }
    Ok((id, r.rest()))
}

/// Per-connection frame-size policy for [`read_frame_with_policy`].
///
/// The cap is opt-in: [`PacketCapPolicy::vanilla`] keeps the stock 2 MiB
/// bound, [`PacketCapPolicy::extended`] admits frames up to
/// [`MAX_EXTENDED_FRAME_BYTES`] for modded recipe/NBT traffic, and
/// [`PacketCapPolicy::try_new`] accepts any value in `1..=32 MiB`,
/// refusing `0` and anything larger fail-closed. Existing callers of
/// [`read_frame`] are unaffected — they keep [`MAX_FRAME_LENGTH`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PacketCapPolicy {
    max_frame_bytes: usize,
}

impl PacketCapPolicy {
    /// The vanilla 2 MiB frame bound.
    pub fn vanilla() -> Self {
        Self {
            max_frame_bytes: VANILLA_FRAME_BYTES,
        }
    }

    /// The maximum configurable bound, 32 MiB, for giant modded packets.
    pub fn extended() -> Self {
        Self {
            max_frame_bytes: MAX_EXTENDED_FRAME_BYTES,
        }
    }

    /// A policy with an explicit bound. `0` and values above
    /// [`MAX_EXTENDED_FRAME_BYTES`] are refused — a zero cap would reject
    /// every frame and an over-large one would defeat the allocation guard.
    pub fn try_new(max_frame_bytes: usize) -> Result<Self, ProtoError> {
        if max_frame_bytes == 0 || max_frame_bytes > MAX_EXTENDED_FRAME_BYTES {
            return Err(ProtoError::LengthOutOfBounds {
                length: max_frame_bytes,
                max: MAX_EXTENDED_FRAME_BYTES,
            });
        }
        Ok(Self { max_frame_bytes })
    }

    /// The declared-length bound this policy enforces.
    pub fn max_frame_bytes(&self) -> usize {
        self.max_frame_bytes
    }
}

impl Default for PacketCapPolicy {
    fn default() -> Self {
        Self::vanilla()
    }
}

/// [`read_frame`] under an explicit [`PacketCapPolicy`]: the declared frame
/// length must fit `policy.max_frame_bytes()` and match the buffer exactly.
pub fn read_frame_with_policy(
    frame: &[u8],
    policy: PacketCapPolicy,
) -> Result<(i32, &[u8]), ProtoError> {
    read_frame_bounded(frame, policy.max_frame_bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip_varint(v: i32) {
        let mut w = WireWriter::new();
        w.write_varint(v);
        let mut r = WireReader::new(w.as_slice());
        assert_eq!(r.read_varint().unwrap(), v);
        r.finish().unwrap();
    }

    #[test]
    fn varint_boundaries_roundtrip() {
        for v in [
            0, 1, -1, 127, 128, 255, 2_147_483_647, -2_147_483_648, 0x7fff_ffff,
            2097151, -2097152, 268435455,
        ] {
            roundtrip_varint(v);
        }
        // Canonical encodings: 0 is one byte; -1 is the full five.
        let mut w = WireWriter::new();
        w.write_varint(-1);
        assert_eq!(w.as_slice(), &[0xff, 0xff, 0xff, 0xff, 0x0f]);
        let mut w = WireWriter::new();
        w.write_varint(2147483647);
        assert_eq!(w.as_slice(), &[0xff, 0xff, 0xff, 0xff, 0x07]);
    }

    #[test]
    fn varint_rejects_overlong_and_overflow() {
        // Six continuation bytes: no terminator in range.
        let overlong = [0x80, 0x80, 0x80, 0x80, 0x80, 0x01];
        assert!(matches!(
            WireReader::new(&overlong).read_varint(),
            Err(ProtoError::VarIntTooLong)
        ));
        // Fifth byte 0x7f has no continuation bit but overflows 32 bits.
        let overflow = [0xff, 0xff, 0xff, 0xff, 0x7f];
        assert!(matches!(
            WireReader::new(&overflow).read_varint(),
            Err(ProtoError::VarIntOverflow)
        ));
        // Truncated mid-sequence.
        let short = [0xff, 0xff];
        assert!(matches!(
            WireReader::new(&short).read_varint(),
            Err(ProtoError::UnexpectedEof { .. })
        ));
    }

    #[test]
    fn varlong_boundaries_roundtrip() {
        let mut w = WireWriter::new();
        w.write_varlong(-1);
        assert_eq!(w.as_slice().len(), 10);
        let mut r = WireReader::new(w.as_slice());
        assert_eq!(r.read_varlong().unwrap(), -1);
        for v in [0, 1, -1, i64::MAX, i64::MIN, 1 << 35, -(1 << 62)] {
            let mut w = WireWriter::new();
            w.write_varlong(v);
            let mut r = WireReader::new(w.as_slice());
            assert_eq!(r.read_varlong().unwrap(), v);
        }
        let too_long = [0x80; 11];
        assert!(matches!(
            WireReader::new(&too_long).read_varlong(),
            Err(ProtoError::VarLongTooLong | ProtoError::VarLongOverflow)
        ));
    }

    #[test]
    fn block_pos_packs_and_sign_extends() {
        let cases = [
            (0, 0, 0),
            (1, 2, 3),
            (-1, -1, -1),
            (33_554_431, 2047, -33_554_432),
            (-33_554_432, -2048, 33_554_431),
            (12345, -64, -30000),
        ];
        for (x, y, z) in cases {
            let pos = BlockPos::new(x, y, z);
            assert_eq!(BlockPos::unpack(pos.pack()), pos, "({x},{y},{z})");
        }
        // Wire roundtrip.
        let mut w = WireWriter::new();
        w.write_position(BlockPos::new(-30, 64, 12));
        let mut r = WireReader::new(w.as_slice());
        assert_eq!(r.read_position().unwrap(), BlockPos::new(-30, 64, 12));
    }

    #[test]
    fn bitset_multi_word_roundtrip() {
        // A light mask spanning more than 64 sections needs two words: this is
        // the shape a single-u64 encoder silently corrupts.
        let mut set = BitSet::new();
        for i in [0, 5, 63, 64, 70, 129] {
            set.set(i, true);
        }
        let mut w = WireWriter::new();
        w.write_bitset(&set);
        let mut r = WireReader::new(w.as_slice());
        let back = r.read_bitset(8).unwrap();
        r.finish().unwrap();
        assert_eq!(back, set);
        assert!(back.get(70) && !back.get(69));
        assert_eq!(back.count_ones(), 6);
        let bits: Vec<usize> = back.set_bits().collect();
        assert_eq!(bits, [0, 5, 63, 64, 70, 129]);
        // Forged word counts are bounded before any allocation.
        let forged = [0x7f]; // VarInt 127 words
        assert!(matches!(
            WireReader::new(&forged).read_bitset(4),
            Err(ProtoError::LengthOutOfBounds { .. })
        ));
    }

    #[test]
    fn string_bounds_and_utf8() {
        let mut w = WireWriter::new();
        w.write_string("héllo");
        let mut r = WireReader::new(w.as_slice());
        assert_eq!(r.read_string(16).unwrap(), "héllo");
        // UTF-16 units: astral chars count double like Java's String.length().
        let mut w = WireWriter::new();
        w.write_string("a\u{1f600}");
        let mut r = WireReader::new(w.as_slice());
        assert!(matches!(
            r.read_string(2),
            Err(ProtoError::StringTooLong { units: 3, max: 2 })
        ));
        // Forged byte length beyond max_chars * 4.
        let mut w = WireWriter::new();
        w.write_varint(1000);
        let mut r = WireReader::new(w.as_slice());
        assert!(matches!(
            r.read_string(16),
            Err(ProtoError::LengthOutOfBounds { .. })
        ));
        // Invalid UTF-8.
        let bad = [3u8, 0xff, 0xfe, 0xfd];
        assert!(matches!(
            WireReader::new(&bad).read_string(16),
            Err(ProtoError::InvalidUtf8)
        ));
    }

    #[test]
    fn frame_roundtrip_and_bounds() {
        let mut frame = Vec::new();
        write_frame(&mut frame, 0x26, &[1, 2, 3]);
        let (id, payload) = read_frame(&frame).unwrap();
        assert_eq!((id, payload), (0x26, &[1, 2, 3][..]));
        // Declared length must equal the buffer exactly.
        assert!(read_frame(&frame[..frame.len() - 1]).is_err());
        // A declared length that does not match the buffer fails closed.
        let mut lying = Vec::new();
        let mut w = WireWriter::new();
        w.write_varint(999);
        lying.extend_from_slice(w.as_slice());
        lying.extend_from_slice(&[0x26]);
        assert!(matches!(
            read_frame(&lying),
            Err(ProtoError::Malformed(_))
        ));
    }

    #[test]
    fn packet_cap_policy_bounds_and_validation() {
        assert_eq!(PacketCapPolicy::vanilla().max_frame_bytes(), 2_097_152);
        assert_eq!(
            PacketCapPolicy::extended().max_frame_bytes(),
            MAX_EXTENDED_FRAME_BYTES
        );
        assert_eq!(PacketCapPolicy::default(), PacketCapPolicy::vanilla());
        // 0 and anything above 32 MiB fail closed.
        assert!(PacketCapPolicy::try_new(0).is_err());
        assert!(PacketCapPolicy::try_new(MAX_EXTENDED_FRAME_BYTES + 1).is_err());
        assert!(PacketCapPolicy::try_new(MAX_EXTENDED_FRAME_BYTES).is_ok());
        assert!(PacketCapPolicy::try_new(1).is_ok());

        // Declared len = id varint (1) + payload. A payload of
        // VANILLA_FRAME_BYTES - 1 declares exactly the vanilla cap.
        let payload = vec![0u8; VANILLA_FRAME_BYTES - 1];
        let mut frame = Vec::new();
        write_frame(&mut frame, 0x26, &payload);
        let (id, body) = read_frame_with_policy(&frame, PacketCapPolicy::vanilla()).unwrap();
        assert_eq!((id, body.len()), (0x26, payload.len()));

        // One byte over the vanilla cap is rejected by vanilla() ...
        let mut big = Vec::new();
        write_frame(&mut big, 0x26, &vec![0u8; VANILLA_FRAME_BYTES]);
        assert!(matches!(
            read_frame_with_policy(&big, PacketCapPolicy::vanilla()),
            Err(ProtoError::FrameTooLarge { .. } | ProtoError::LengthOutOfBounds { .. })
        ));
        // ... but extended() admits it, and the crate default is unaffected.
        let (id, body) = read_frame_with_policy(&big, PacketCapPolicy::extended()).unwrap();
        assert_eq!((id, body.len()), (0x26, VANILLA_FRAME_BYTES));
        assert!(read_frame(&big).is_ok());
        // A declaration beyond even the extended cap still fails closed.
        let mut huge = Vec::new();
        write_frame(&mut huge, 0x26, &vec![0u8; MAX_EXTENDED_FRAME_BYTES]);
        assert!(read_frame_with_policy(&huge, PacketCapPolicy::extended()).is_err());
    }

    #[test]
    fn anonymous_nbt_bounds() {
        // {"text":"hi"} in anonymous-NBT form: compound, string field, end.
        let nbt = [10u8, 8, 0, 4, b't', b'e', b'x', b't', 0, 2, b'h', b'i', 0];
        let mut r = WireReader::new(&nbt);
        let span = r.read_anonymous_nbt().unwrap();
        assert_eq!(span, &nbt[..]);
        r.finish().unwrap();
        // Truncated mid-string fails closed.
        let mut r = WireReader::new(&nbt[..nbt.len() - 2]);
        assert!(r.read_anonymous_nbt().is_err());
        // Unknown tag id fails.
        let mut r = WireReader::new(&[77u8]);
        assert!(r.read_anonymous_nbt().is_err());
        // A forged byte-array length fails rather than skipping blindly.
        let forged = [7u8, 0x7f, 0xff, 0xff, 0xff];
        let mut r = WireReader::new(&forged);
        assert!(r.read_anonymous_nbt().is_err());
    }
}
