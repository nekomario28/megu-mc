// SPDX-License-Identifier: LGPL-3.0-or-later
#![forbid(unsafe_code)]
//! Type-safe `ByteBufCodecs.holder` wire primitives for the 1.21.1 registry
//! holder codec.
//!
//! A holder either references a registry entry or carries an inline (direct)
//! value. The wire form is a single VarInt tag followed, only for a direct
//! holder, by the value payload:
//!
//! | tag         | meaning                                  |
//! |-------------|------------------------------------------|
//! | `0`         | direct value follows                     |
//! | `id + 1`    | registry id, no payload                  |
//!
//! The `id + 1` shift makes id `0` encodable while keeping `0` free for the
//! direct form. Writing `id` directly (or testing `== 1` for the first
//! registry entry) is the classic off-by-one that silently turns every first
//! registry element into an inline value, or shifts every id by one. Here the
//! shift exists only inside [`RegistryHolderId::wire_tag`] and
//! [`RegistryHolderId::from_wire_tag`], both checked, so a hand-written
//! `+ 1`/`- 1` cannot appear at a call site.
//!
//! VarInt bytes are the canonical protocol encoding from [`crate::varint`];
//! this module only adds the holder tag rules on top.

use crate::varint::{encode_varint_i32, read_varint_i32, VarIntError};
use std::fmt;

/// A registry id as carried by the holder tag (`tag = id + 1` on the wire).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RegistryHolderId(u32);

impl RegistryHolderId {
    /// Wrap a registry id. The id itself is not range-checked; encoding checks
    /// that `id + 1` still fits a protocol VarInt.
    #[must_use]
    pub const fn new(id: u32) -> Self {
        Self(id)
    }

    /// The registry id this tag refers to.
    #[must_use]
    pub const fn get(self) -> u32 {
        self.0
    }

    /// The on-wire tag `id + 1`, rejecting the one id whose shifted tag cannot
    /// be a non-negative VarInt.
    ///
    /// Only `id == i32::MAX - 1` survives; `i32::MAX` and everything above it
    /// is rejected instead of wrapping to a negative tag that a peer would
    /// decode as a different holder.
    #[must_use = "the checked tag is the only safe encoding of this id"]
    pub fn wire_tag(self) -> Result<i32, HolderWireError> {
        let tag = self
            .0
            .checked_add(1)
            .ok_or(HolderWireError::IdOverflow(self.0))?;
        i32::try_from(tag).map_err(|_| HolderWireError::IdOverflow(self.0))
    }

    /// Recover the registry id from a decoded tag. `0` is the direct-value
    /// marker, never a registry id, and negative tags are malformed.
    #[must_use = "the decoded id is the point of the call"]
    pub fn from_wire_tag(tag: i32) -> Result<Self, HolderWireError> {
        if tag <= 0 {
            return Err(HolderWireError::NonPositiveTag(tag));
        }
        Ok(Self((tag as u32) - 1))
    }
}

/// A holder that is either a registry reference or an inline value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HolderWireRef<T> {
    /// Registry reference encoded as `id + 1`; the registry supplies the value.
    Registry(RegistryHolderId),
    /// Inline value encoded after a `0` tag.
    Direct(T),
}

/// Malformed holder tag, truncated payload, or an unusable registry id.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HolderWireError {
    /// `id + 1` does not fit the protocol's non-negative VarInt tag range.
    IdOverflow(u32),
    /// A decoded tag was `0` (direct marker) or negative, where an `id + 1`
    /// tag was required.
    NonPositiveTag(i32),
    /// Input ended inside the tag or inside the direct payload.
    Truncated,
    /// The tag was not a canonical protocol VarInt.
    MalformedTag,
    /// The tag plus direct payload length does not fit `usize`.
    LengthOverflow,
}

impl From<VarIntError> for HolderWireError {
    fn from(error: VarIntError) -> Self {
        match error {
            VarIntError::UnexpectedEof => Self::Truncated,
            VarIntError::TooLong | VarIntError::NegativeLength(_) => Self::MalformedTag,
            // These variants only apply to length-prefixed strings and cannot be
            // produced by `read_varint_i32`, but keep the mapping total.
            VarIntError::StringTooLong { .. } | VarIntError::InvalidUtf8 => Self::MalformedTag,
        }
    }
}

impl fmt::Display for HolderWireError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::IdOverflow(id) => write!(f, "registry id {id} cannot be encoded as id + 1"),
            Self::NonPositiveTag(tag) => write!(f, "holder tag {tag} is not a registry id + 1"),
            Self::Truncated => f.write_str("holder tag or direct payload is truncated"),
            Self::MalformedTag => f.write_str("holder tag is not a canonical protocol VarInt"),
            Self::LengthOverflow => f.write_str("holder payload length overflows usize"),
        }
    }
}

impl std::error::Error for HolderWireError {}

/// Append a canonical protocol VarInt and return the number of bytes written.
#[must_use = "the byte count is the only feedback that the tag was appended"]
pub fn encode_varint(value: i32, out: &mut Vec<u8>) -> usize {
    let (bytes, len) = encode_varint_i32(value);
    out.extend_from_slice(&bytes[..len]);
    len
}

/// Encode a holder reference and return the number of bytes written.
///
/// `write_direct` runs only for [`HolderWireRef::Direct`] and must append the
/// payload for `value`; for [`HolderWireRef::Registry`] the buffer receives
/// exactly the checked `id + 1` tag. Direct payloads cannot fail here, so any
/// fallible pre-encoding belongs in the caller's scratch buffer before this
/// call; a rejected registry id, by contrast, is detected before any byte is
/// appended.
pub fn encode_holder_ref<T>(
    holder: &HolderWireRef<T>,
    write_direct: impl FnOnce(&T, &mut Vec<u8>),
    out: &mut Vec<u8>,
) -> Result<usize, HolderWireError> {
    let start = out.len();
    match holder {
        HolderWireRef::Registry(id) => {
            let tag = id.wire_tag()?;
            let _ = encode_varint(tag, out);
        }
        HolderWireRef::Direct(value) => {
            let _ = encode_varint(0, out);
            write_direct(value, out);
        }
    }
    Ok(out.len() - start)
}

/// Decode a holder reference, returning it with the number of bytes consumed.
///
/// `read_direct` receives the bytes after the `0` tag and must return the
/// decoded value plus its payload length. A payload length past the end of the
/// remaining input is rejected as [`HolderWireError::Truncated`] rather than
/// letting a downstream slice panic.
pub fn decode_holder_ref<'a, T>(
    input: &'a [u8],
    read_direct: impl FnOnce(&'a [u8]) -> Result<(T, usize), HolderWireError>,
) -> Result<(HolderWireRef<T>, usize), HolderWireError> {
    let (tag, tag_len) = read_varint_i32(input)?;
    if tag < 0 {
        return Err(HolderWireError::NonPositiveTag(tag));
    }
    if tag == 0 {
        let payload = input.get(tag_len..).ok_or(HolderWireError::Truncated)?;
        let (value, payload_len) = read_direct(payload)?;
        if payload_len > payload.len() {
            return Err(HolderWireError::Truncated);
        }
        let consumed = tag_len
            .checked_add(payload_len)
            .ok_or(HolderWireError::LengthOverflow)?;
        Ok((HolderWireRef::Direct(value), consumed))
    } else {
        Ok((
            HolderWireRef::Registry(RegistryHolderId::from_wire_tag(tag)?),
            tag_len,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::{
        decode_holder_ref, encode_holder_ref, encode_varint, HolderWireError, HolderWireRef,
        RegistryHolderId,
    };

    fn string_payload(value: &String, out: &mut Vec<u8>) {
        let _ = encode_varint(
            i32::try_from(value.len()).expect("test payload fits i32"),
            out,
        );
        out.extend_from_slice(value.as_bytes());
    }

    fn read_string(input: &[u8]) -> Result<(String, usize), HolderWireError> {
        let (len, header) = crate::varint::read_varint_i32(input)?;
        let len = usize::try_from(len).map_err(|_| HolderWireError::MalformedTag)?;
        let body = input.get(header..).ok_or(HolderWireError::Truncated)?;
        let bytes = body.get(..len).ok_or(HolderWireError::Truncated)?;
        let value = String::from_utf8(bytes.to_vec()).map_err(|_| HolderWireError::MalformedTag)?;
        Ok((value, header + len))
    }

    #[test]
    fn registry_id_zero_encodes_as_tag_one() {
        let mut out = Vec::new();
        let used = encode_holder_ref(
            &HolderWireRef::Registry(RegistryHolderId::new(0)),
            string_payload,
            &mut out,
        )
        .expect("id 0 is encodable");
        assert_eq!(used, 1);
        assert_eq!(out, [1]);

        let (holder, consumed) =
            decode_holder_ref(out.as_slice(), read_string).expect("tag 1 is registry id 0");
        assert_eq!(holder, HolderWireRef::Registry(RegistryHolderId::new(0)));
        assert_eq!(consumed, 1);
    }

    #[test]
    fn registry_ids_roundtrip_at_the_checked_edges() {
        for id in [0, 1, 2, 126, 127, 128, 16_383, i32::MAX as u32 - 1] {
            let mut out = Vec::new();
            encode_holder_ref(
                &HolderWireRef::Registry(RegistryHolderId::new(id)),
                string_payload,
                &mut out,
            )
            .expect("checked ids encode");
            let (holder, consumed) =
                decode_holder_ref(out.as_slice(), read_string).expect("checked tag decodes");
            assert_eq!(holder, HolderWireRef::Registry(RegistryHolderId::new(id)));
            assert_eq!(consumed, out.len());
        }
    }

    #[test]
    fn registry_id_overflow_is_rejected_before_writing() {
        for id in [i32::MAX as u32, i32::MAX as u32 + 1, u32::MAX] {
            assert_eq!(
                RegistryHolderId::new(id).wire_tag(),
                Err(HolderWireError::IdOverflow(id))
            );
            let mut out = vec![0xAA];
            let error = encode_holder_ref(
                &HolderWireRef::Registry(RegistryHolderId::new(id)),
                string_payload,
                &mut out,
            )
            .expect_err("overflowing ids must not encode");
            assert_eq!(error, HolderWireError::IdOverflow(id));
            assert_eq!(out, [0xAA], "a rejected id must not append bytes");
        }
        assert_eq!(
            RegistryHolderId::new(i32::MAX as u32 - 1).wire_tag(),
            Ok(i32::MAX)
        );
    }

    #[test]
    fn direct_holders_use_tag_zero_and_roundtrip_their_payload() {
        let value = String::from("minecraft:overworld");
        let mut out = Vec::new();
        let used = encode_holder_ref(
            &HolderWireRef::Direct(value.clone()),
            string_payload,
            &mut out,
        )
        .expect("direct holders encode");
        assert_eq!(out[0], 0);
        assert_eq!(used, out.len());

        let (holder, consumed) =
            decode_holder_ref(out.as_slice(), read_string).expect("direct holder decodes");
        assert_eq!(holder, HolderWireRef::Direct(value));
        assert_eq!(consumed, out.len());
    }

    #[test]
    fn decode_rejects_zero_where_a_registry_id_is_required() {
        assert_eq!(
            RegistryHolderId::from_wire_tag(0),
            Err(HolderWireError::NonPositiveTag(0))
        );
        assert_eq!(
            RegistryHolderId::from_wire_tag(-1),
            Err(HolderWireError::NonPositiveTag(-1))
        );
        assert_eq!(
            RegistryHolderId::from_wire_tag(1),
            Ok(RegistryHolderId::new(0))
        );
    }

    #[test]
    fn decode_rejects_negative_and_malformed_tags() {
        // Negative VarInt tag (-1) is five bytes and canonical, but never an id.
        let mut negative = Vec::new();
        let _ = encode_varint(-1, &mut negative);
        assert_eq!(
            decode_holder_ref(negative.as_slice(), read_string),
            Err(HolderWireError::NonPositiveTag(-1))
        );
        // Non-canonical zero padded with a continuation byte.
        assert_eq!(
            decode_holder_ref(&[0x80, 0x00], read_string),
            Err(HolderWireError::MalformedTag)
        );
        // Five-byte VarInt with high payload bits set.
        assert_eq!(
            decode_holder_ref(&[0xFF, 0xFF, 0xFF, 0xFF, 0x7F], read_string),
            Err(HolderWireError::MalformedTag)
        );
    }

    #[test]
    fn truncation_is_fail_closed() {
        assert_eq!(
            decode_holder_ref(&[], read_string),
            Err(HolderWireError::Truncated)
        );
        // Tag says direct, but the payload header is missing.
        assert_eq!(
            decode_holder_ref(&[0], read_string),
            Err(HolderWireError::Truncated)
        );
        // Payload header claims four bytes with only two present.
        assert_eq!(
            decode_holder_ref(&[0, 4, b'a', b'b'], read_string),
            Err(HolderWireError::Truncated)
        );
        // A misbehaving direct reader cannot report more bytes than it received.
        assert_eq!(
            decode_holder_ref(&[0, 1, 2], |_| Ok(((), 9))),
            Err(HolderWireError::Truncated)
        );
    }

    #[test]
    fn encode_varint_reports_the_canonical_length() {
        let mut out = Vec::new();
        assert_eq!(encode_varint(0, &mut out), 1);
        assert_eq!(encode_varint(i32::MAX, &mut out), 5);
        // The `#[must_use]` byte counts above are the assertions themselves.
        assert_eq!(out.len(), 6);
        let (value, used) = crate::varint::read_varint_i32(&out[1..]).expect("canonical varint");
        assert_eq!(value, i32::MAX);
        assert_eq!(used, 5);
    }
}
