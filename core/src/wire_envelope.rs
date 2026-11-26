// SPDX-License-Identifier: LGPL-3.0-or-later
//! Canonical frame layout for custom wire and save formats.
//!
//! A frame is four magic bytes, a big-endian u16 schema version, a big-endian
//! u32 payload length, the payload, and a big-endian IEEE CRC-32 of all
//! preceding bytes. Decoding consumes exactly one whole frame.

use std::fmt;

use crate::hash::crc32_ieee;

const HEADER_BYTES: usize = size_of::<[u8; 4]>() + size_of::<u16>() + size_of::<u32>();
const CRC_BYTES: usize = size_of::<u32>();

/// The format identity, supported version, and payload budget for one codec.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EnvelopeSpec {
    pub magic: [u8; 4],
    pub schema_version: u16,
    pub max_payload_bytes: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EnvelopeError {
    Truncated,
    BadMagic,
    SchemaVersionMismatch { expected: u16, actual: u16 },
    LimitExceeded { limit: usize },
    LengthOverflow,
    TrailingBytes,
    CrcMismatch,
    InvalidUtf8,
    AllocationFailed,
}

impl fmt::Display for EnvelopeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Truncated => f.write_str("truncated wire input"),
            Self::BadMagic => f.write_str("wrong envelope magic"),
            Self::SchemaVersionMismatch { expected, actual } => {
                write!(
                    f,
                    "envelope schema version {actual} differs from {expected}"
                )
            }
            Self::LimitExceeded { limit } => write!(f, "wire size exceeds {limit} bytes"),
            Self::LengthOverflow => f.write_str("wire length cannot be represented"),
            Self::TrailingBytes => f.write_str("trailing wire bytes"),
            Self::CrcMismatch => f.write_str("envelope CRC-32 mismatch"),
            Self::InvalidUtf8 => f.write_str("invalid UTF-8 in wire string"),
            Self::AllocationFailed => f.write_str("could not reserve wire buffer"),
        }
    }
}

impl std::error::Error for EnvelopeError {}

/// Encode one complete frame, rejecting oversized payloads before allocation.
pub fn encode_envelope(spec: EnvelopeSpec, payload: &[u8]) -> Result<Vec<u8>, EnvelopeError> {
    check_limit(payload.len(), spec.max_payload_bytes)?;
    let payload_len = to_wire_len(payload.len())?;
    let frame_len = payload
        .len()
        .checked_add(HEADER_BYTES + CRC_BYTES)
        .ok_or(EnvelopeError::LengthOverflow)?;
    let mut frame = BoundedWriter::new(frame_len);
    frame
        .bytes
        .try_reserve_exact(frame_len)
        .map_err(|_| EnvelopeError::AllocationFailed)?;
    frame.write_bytes(&spec.magic)?;
    frame.write_u16(spec.schema_version)?;
    frame.write_u32(payload_len)?;
    frame.write_bytes(payload)?;
    frame.write_u32(crc32_ieee(frame.as_slice()))?;
    Ok(frame.into_inner())
}

/// Validate one complete frame and borrow its payload directly from the input.
pub fn decode_envelope(spec: EnvelopeSpec, frame: &[u8]) -> Result<&[u8], EnvelopeError> {
    let mut cursor = BoundedCursor::new(frame);
    if cursor.read_bytes(spec.magic.len())? != spec.magic {
        return Err(EnvelopeError::BadMagic);
    }
    let version = cursor.read_u16()?;
    if version != spec.schema_version {
        return Err(EnvelopeError::SchemaVersionMismatch {
            expected: spec.schema_version,
            actual: version,
        });
    }
    let payload_len = cursor.read_len()?;
    check_limit(payload_len, spec.max_payload_bytes)?;
    let payload = cursor.read_bytes(payload_len)?;
    let expected_crc = cursor.read_u32()?;
    cursor.finish_empty()?;
    if crc32_ieee(&frame[..frame.len() - CRC_BYTES]) != expected_crc {
        return Err(EnvelopeError::CrcMismatch);
    }
    Ok(payload)
}

fn check_limit(len: usize, limit: usize) -> Result<(), EnvelopeError> {
    if len > limit {
        return Err(EnvelopeError::LimitExceeded { limit });
    }
    Ok(())
}

fn to_wire_len(len: usize) -> Result<u32, EnvelopeError> {
    u32::try_from(len).map_err(|_| EnvelopeError::LengthOverflow)
}

/// Zero-copy reader over a bounded byte slice. Failed reads leave it unchanged.
#[derive(Clone, Copy, Debug)]
pub struct BoundedCursor<'a> {
    remaining: &'a [u8],
}

impl<'a> BoundedCursor<'a> {
    pub const fn new(bytes: &'a [u8]) -> Self {
        Self { remaining: bytes }
    }

    pub fn read_u8(&mut self) -> Result<u8, EnvelopeError> {
        Ok(u8::from_be_bytes(self.read_array()?))
    }

    pub fn read_u16(&mut self) -> Result<u16, EnvelopeError> {
        Ok(u16::from_be_bytes(self.read_array()?))
    }

    pub fn read_u32(&mut self) -> Result<u32, EnvelopeError> {
        Ok(u32::from_be_bytes(self.read_array()?))
    }

    pub fn read_i32(&mut self) -> Result<i32, EnvelopeError> {
        Ok(i32::from_be_bytes(self.read_array()?))
    }

    pub fn read_u64(&mut self) -> Result<u64, EnvelopeError> {
        Ok(u64::from_be_bytes(self.read_array()?))
    }

    pub fn read_bytes(&mut self, len: usize) -> Result<&'a [u8], EnvelopeError> {
        let (bytes, remaining) = self
            .remaining
            .split_at_checked(len)
            .ok_or(EnvelopeError::Truncated)?;
        self.remaining = remaining;
        Ok(bytes)
    }

    /// Read a UTF-8 string with a big-endian u32 byte length and a field cap.
    pub fn read_str(&mut self, max_len: usize) -> Result<&'a str, EnvelopeError> {
        let mut field = *self;
        let len = field.read_len()?;
        check_limit(len, max_len)?;
        let text =
            std::str::from_utf8(field.read_bytes(len)?).map_err(|_| EnvelopeError::InvalidUtf8)?;
        *self = field;
        Ok(text)
    }

    pub fn finish_empty(&self) -> Result<(), EnvelopeError> {
        if !self.remaining.is_empty() {
            return Err(EnvelopeError::TrailingBytes);
        }
        Ok(())
    }

    fn read_array<const N: usize>(&mut self) -> Result<[u8; N], EnvelopeError> {
        let (bytes, remaining) = self
            .remaining
            .split_first_chunk::<N>()
            .ok_or(EnvelopeError::Truncated)?;
        self.remaining = remaining;
        Ok(*bytes)
    }

    fn read_len(&mut self) -> Result<usize, EnvelopeError> {
        usize::try_from(self.read_u32()?).map_err(|_| EnvelopeError::LengthOverflow)
    }
}

/// Big-endian writer whose serialized bytes stay within its configured cap.
/// Failed writes leave its contents unchanged.
#[derive(Debug)]
pub struct BoundedWriter {
    bytes: Vec<u8>,
    max_bytes: usize,
}

impl BoundedWriter {
    pub const fn new(max_bytes: usize) -> Self {
        Self {
            bytes: Vec::new(),
            max_bytes,
        }
    }

    pub fn as_slice(&self) -> &[u8] {
        &self.bytes
    }

    pub fn into_inner(self) -> Vec<u8> {
        self.bytes
    }

    pub fn write_u8(&mut self, value: u8) -> Result<(), EnvelopeError> {
        self.write_bytes(&value.to_be_bytes())
    }

    pub fn write_u16(&mut self, value: u16) -> Result<(), EnvelopeError> {
        self.write_bytes(&value.to_be_bytes())
    }

    pub fn write_u32(&mut self, value: u32) -> Result<(), EnvelopeError> {
        self.write_bytes(&value.to_be_bytes())
    }

    pub fn write_i32(&mut self, value: i32) -> Result<(), EnvelopeError> {
        self.write_bytes(&value.to_be_bytes())
    }

    pub fn write_u64(&mut self, value: u64) -> Result<(), EnvelopeError> {
        self.write_bytes(&value.to_be_bytes())
    }

    pub fn write_bytes(&mut self, bytes: &[u8]) -> Result<(), EnvelopeError> {
        self.reserve_write(bytes.len())?;
        self.bytes.extend_from_slice(bytes);
        Ok(())
    }

    /// Write a big-endian u32 byte length followed by UTF-8 within a field cap.
    pub fn write_str(&mut self, value: &str, max_len: usize) -> Result<(), EnvelopeError> {
        check_limit(value.len(), max_len)?;
        let len = to_wire_len(value.len())?;
        let field_len = value
            .len()
            .checked_add(size_of::<u32>())
            .ok_or(EnvelopeError::LengthOverflow)?;
        self.reserve_write(field_len)?;
        self.bytes.extend_from_slice(&len.to_be_bytes());
        self.bytes.extend_from_slice(value.as_bytes());
        Ok(())
    }

    fn reserve_write(&mut self, len: usize) -> Result<(), EnvelopeError> {
        if len > self.max_bytes - self.bytes.len() {
            return Err(EnvelopeError::LimitExceeded {
                limit: self.max_bytes,
            });
        }
        self.bytes
            .try_reserve(len)
            .map_err(|_| EnvelopeError::AllocationFailed)
    }
}
