// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
//! `megu_core::nbt_wire` — the canonical, zero-allocation NBT wire-skip and
//! bounds-validation foundation.
//!
//! Every NBT consumer in megu (the play-protocol packet readers, `proto`'s
//! opaque registry-data spans, `client-app`'s packet reader, item components
//! and the test-side packet decoders) used to carry its own hand-written
//! skipper. Those copies disagreed on depth, on negative lengths, on
//! `TAG_List(TAG_End, n > 0)` and on whether a declared length was checked
//! against the bytes actually left in the packet. This module is the single
//! implementation those call sites can converge on.
//!
//! Two wire shapes are supported explicitly:
//!
//! * [`skip_network_nbt`] — anonymous (network) NBT as used from MC 1.20.2
//!   through 1.21.1 / protocol 767: a tag byte followed by its payload and
//!   **no root name**. The canonical network root is `TAG_Compound`; the
//!   function deliberately accepts any non-`TAG_End` root because the same
//!   anonymous shape also carries item-component and text-component values.
//! * [`skip_named_nbt`] — named-root NBT (save files and older wire formats):
//!   a tag byte, a `u16` name length, the name bytes, then the payload.
//! * [`skip_nbt_payload`] — the payload only, for callers that already
//!   consumed the tag byte.
//!
//! All three return the exact number of bytes consumed, so trailing bytes
//! (the next field of the enclosing packet) stay untouched.
//!
//! # Security invariants
//!
//! The skipper is a pure slice cursor. It performs **no heap allocation**, no
//! `unsafe`, no locking and no clock reads, and uses a fixed frame stack (see
//! [`NBT_WIRE_HARD_MAX_DEPTH`]) so attacker-controlled nesting can never
//! exhaust the process stack. Every path is fail-closed:
//!
//! * [`NbtWireLimits`] bounds depth, aggregate tag count, list length and
//!   array bytes.
//! * `TAG_List` with `elem_tag == TAG_End` and a non-zero length is rejected
//!   immediately ([`NbtWireError::InvalidEmptyElementList`]); this closes the
//!   classic `543`-byte `TAG_List(TAG_End, 1_048_576)` CPU DoS.
//! * Negative `i32` list/array lengths are rejected
//!   ([`NbtWireError::NegativeLength`]) before any cast.
//! * A declared element/byte length larger than the remaining input is
//!   rejected ([`NbtWireError::LengthExceedsRemaining`]) before the cursor
//!   advances; multiplications use checked arithmetic.
//! * Every value tag visited decrements one aggregate budget, so deeply
//!   branching or packed structures cannot exceed [`NbtWireLimits::max_tags`]
//!   units of work.
//!
//! Strings are skipped as opaque bytes: UTF-8 payloads and Java's modified
//! UTF-8 (MUTF-8) encoding are equally accepted, because validation belongs to
//! a decoding consumer, not to a skipper.

use std::error::Error;
use std::fmt;

/// Absolute ceiling for the fixed frame stack. [`NbtWireLimits::max_depth`] is
/// clamped to this value so a caller-supplied limit can never overflow the
/// stack array, regardless of how large a number it passes.
pub const NBT_WIRE_HARD_MAX_DEPTH: usize = 512;

/// Default maximum container nesting depth (the root container is depth 1).
pub const NBT_WIRE_DEFAULT_MAX_DEPTH: usize = NBT_WIRE_HARD_MAX_DEPTH;

/// Default aggregate budget for value tags visited in one skip operation.
pub const NBT_WIRE_DEFAULT_MAX_TAGS: usize = 65_536;

/// Default maximum element count for a single `TAG_List`.
pub const NBT_WIRE_DEFAULT_MAX_LIST_LEN: usize = 65_536;

/// Default maximum byte size of a single `TAG_ByteArray`/`TAG_IntArray`/
/// `TAG_LongArray` payload (4 MiB).
pub const NBT_WIRE_DEFAULT_MAX_ARRAY_BYTES: usize = 4 * 1024 * 1024;

/// Fail-closed resource limits for one NBT skip.
///
/// The defaults mirror the hardened values above and are safe to use for
/// untrusted network input. A caller reading a trusted local save file may
/// raise them, but `max_depth` is always clamped to
/// [`NBT_WIRE_HARD_MAX_DEPTH`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NbtWireLimits {
    /// Maximum container nesting depth; the root container counts as depth 1.
    /// Clamped to [`NBT_WIRE_HARD_MAX_DEPTH`].
    pub max_depth: usize,
    /// Maximum number of value tags visited across the whole skip.
    pub max_tags: usize,
    /// Maximum element count of a single `TAG_List`.
    pub max_list_len: usize,
    /// Maximum byte size of a single typed array payload.
    pub max_array_bytes: usize,
}

impl NbtWireLimits {
    /// The hardened defaults, also returned by [`Default::default`].
    #[must_use]
    pub const fn new() -> Self {
        Self {
            max_depth: NBT_WIRE_DEFAULT_MAX_DEPTH,
            max_tags: NBT_WIRE_DEFAULT_MAX_TAGS,
            max_list_len: NBT_WIRE_DEFAULT_MAX_LIST_LEN,
            max_array_bytes: NBT_WIRE_DEFAULT_MAX_ARRAY_BYTES,
        }
    }

    /// The effective depth ceiling after applying the hard stack bound.
    #[must_use]
    pub const fn effective_max_depth(self) -> usize {
        if self.max_depth < NBT_WIRE_HARD_MAX_DEPTH {
            self.max_depth
        } else {
            NBT_WIRE_HARD_MAX_DEPTH
        }
    }
}

impl Default for NbtWireLimits {
    fn default() -> Self {
        Self::new()
    }
}

/// Every deterministic failure mode of the NBT wire skipper.
///
/// Errors carry the tag/offsets that triggered them so a caller can log a
/// precise packet-level diagnostic without re-parsing the input.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NbtWireError {
    /// The input ended before the current field could be read.
    UnexpectedEof {
        /// Bytes the field needs.
        needed: usize,
        /// Bytes left in the input.
        remaining: usize,
    },
    /// A tag id outside `0..=12` appeared where a value tag was required.
    InvalidTagId(u8),
    /// A list or array declared a negative `i32` length.
    NegativeLength {
        /// Tag id that declared the length.
        tag: u8,
    },
    /// `length * element_width` did not fit in `usize` (32-bit targets).
    LengthOverflow {
        /// Tag id that declared the length.
        tag: u8,
        /// Declared element count.
        length: usize,
        /// Element width in bytes.
        width: usize,
    },
    /// A declared element or byte count exceeds the bytes left in the input.
    LengthExceedsRemaining {
        /// Tag id that declared the length.
        tag: u8,
        /// Declared count.
        length: usize,
        /// Bytes actually left when the length was read.
        remaining: usize,
    },
    /// `TAG_List` declared `TAG_End` as its element type with a non-zero
    /// length. Such a list can never terminate and is the classic
    /// zero-byte-element DoS.
    InvalidEmptyElementList {
        /// Declared element count.
        length: usize,
    },
    /// A list exceeded [`NbtWireLimits::max_list_len`].
    ListTooLong {
        /// Declared element count.
        length: usize,
        /// Configured maximum.
        max: usize,
    },
    /// A typed array exceeded [`NbtWireLimits::max_array_bytes`].
    ArrayTooLarge {
        /// Declared byte size.
        bytes: usize,
        /// Configured maximum.
        max: usize,
    },
    /// Container nesting exceeded the effective depth ceiling.
    DepthExceeded {
        /// Depth that would have been entered.
        depth: usize,
        /// Effective maximum depth.
        max: usize,
    },
    /// The aggregate tag budget was exhausted.
    TagBudgetExceeded {
        /// Configured maximum.
        max: usize,
    },
}

impl fmt::Display for NbtWireError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnexpectedEof { needed, remaining } => write!(
                f,
                "NBT input truncated: needed {needed} byte(s), {remaining} remaining"
            ),
            Self::InvalidTagId(tag) => write!(f, "invalid NBT tag id {tag}"),
            Self::NegativeLength { tag } => {
                write!(f, "NBT tag {tag} declared a negative length")
            }
            Self::LengthOverflow { tag, length, width } => write!(
                f,
                "NBT tag {tag} length {length} x width {width} overflows usize"
            ),
            Self::LengthExceedsRemaining {
                tag,
                length,
                remaining,
            } => write!(
                f,
                "NBT tag {tag} declared {length} element(s)/byte(s) but only {remaining} remain"
            ),
            Self::InvalidEmptyElementList { length } => write!(
                f,
                "NBT list declares TAG_End elements with non-zero length {length}"
            ),
            Self::ListTooLong { length, max } => {
                write!(f, "NBT list length {length} exceeds limit {max}")
            }
            Self::ArrayTooLarge { bytes, max } => {
                write!(f, "NBT array of {bytes} bytes exceeds limit {max}")
            }
            Self::DepthExceeded { depth, max } => {
                write!(f, "NBT nesting depth {depth} exceeds limit {max}")
            }
            Self::TagBudgetExceeded { max } => {
                write!(f, "NBT tag budget of {max} exhausted")
            }
        }
    }
}

impl Error for NbtWireError {}

/// One open container on the fixed frame stack.
#[derive(Clone, Copy)]
enum Frame {
    /// A compound whose next entry header has not been read yet.
    Compound,
    /// A list with `remaining` elements still to visit *after* the current
    /// one (which the outer loop is about to process).
    List { elem_tag: u8, remaining: usize },
}

/// Skip one anonymous (network) NBT value starting at byte 0.
///
/// The input begins with the value's tag byte and carries **no root name**,
/// matching MC 1.20.2+ network NBT. The canonical root is `TAG_Compound`, but
/// any non-`TAG_End` tag is accepted so item-component and text-component
/// anonymous values share this entry point.
///
/// Returns the exact number of bytes consumed; bytes after the value are not
/// examined.
///
/// # Errors
///
/// Returns the first [`NbtWireError`] encountered. See the module docs for the
/// fail-closed invariants.
pub fn skip_network_nbt(input: &[u8], limits: NbtWireLimits) -> Result<usize, NbtWireError> {
    let mut pos = 0usize;
    let tag = read_root_tag(input, &mut pos)?;
    skip_payload_impl(input, &mut pos, tag, limits)?;
    Ok(pos)
}

/// Skip one named-root NBT value starting at byte 0.
///
/// The input is a tag byte, a `u16` big-endian name length, the name bytes,
/// and then the payload — the save-file / older-wire shape.
///
/// Returns the exact number of bytes consumed; bytes after the value are not
/// examined.
///
/// # Errors
///
/// Returns the first [`NbtWireError`] encountered. See the module docs for the
/// fail-closed invariants.
pub fn skip_named_nbt(input: &[u8], limits: NbtWireLimits) -> Result<usize, NbtWireError> {
    let mut pos = 0usize;
    let tag = read_root_tag(input, &mut pos)?;
    let name_len = usize::from(read_u16_be(input, &mut pos)?);
    let remaining = remaining(input, pos);
    if name_len > remaining {
        return Err(NbtWireError::LengthExceedsRemaining {
            tag: 8,
            length: name_len,
            remaining,
        });
    }
    pos += name_len;
    skip_payload_impl(input, &mut pos, tag, limits)?;
    Ok(pos)
}

/// Skip the payload of one NBT value whose tag byte has already been
/// consumed.
///
/// `tag_id` is the previously read tag byte. `TAG_End` (0) has an empty
/// payload and returns `Ok(0)` without touching `input`.
///
/// Returns the exact number of payload bytes consumed.
///
/// # Errors
///
/// Returns the first [`NbtWireError`] encountered. See the module docs for the
/// fail-closed invariants.
pub fn skip_nbt_payload(
    tag_id: u8,
    input: &[u8],
    limits: NbtWireLimits,
) -> Result<usize, NbtWireError> {
    if tag_id == 0 {
        return Ok(0);
    }
    if tag_id > 12 {
        return Err(NbtWireError::InvalidTagId(tag_id));
    }
    let mut pos = 0usize;
    skip_payload_impl(input, &mut pos, tag_id, limits)?;
    Ok(pos)
}

/// Read and validate a root tag byte. `TAG_End` is not a value and is
/// rejected, as is any id outside `0..=12`.
fn read_root_tag(input: &[u8], pos: &mut usize) -> Result<u8, NbtWireError> {
    let tag = read_u8(input, pos)?;
    if tag == 0 || tag > 12 {
        return Err(NbtWireError::InvalidTagId(tag));
    }
    Ok(tag)
}

/// Iterative payload skipper. `initial_tag` must be a non-zero, valid tag.
fn skip_payload_impl(
    input: &[u8],
    pos: &mut usize,
    initial_tag: u8,
    limits: NbtWireLimits,
) -> Result<(), NbtWireError> {
    let max_depth = limits.effective_max_depth();
    let mut tags_left = limits.max_tags;
    let mut stack = [Frame::List {
        elem_tag: 0,
        remaining: 0,
    }; NBT_WIRE_HARD_MAX_DEPTH];
    let mut depth = 0usize;
    let mut tag = initial_tag;

    'values: loop {
        if tags_left == 0 {
            return Err(NbtWireError::TagBudgetExceeded {
                max: limits.max_tags,
            });
        }
        tags_left -= 1;

        match tag {
            1 => take(input, pos, 1)?,
            2 => take(input, pos, 2)?,
            3 | 5 => take(input, pos, 4)?,
            4 | 6 => take(input, pos, 8)?,
            7 | 11 | 12 => {
                let width = match tag {
                    7 => 1,
                    11 => 4,
                    _ => 8,
                };
                let length = read_i32_be(input, pos)?;
                if length < 0 {
                    return Err(NbtWireError::NegativeLength { tag });
                }
                let length = length as usize;
                let remaining = remaining(input, *pos);
                if length > remaining {
                    return Err(NbtWireError::LengthExceedsRemaining {
                        tag,
                        length,
                        remaining,
                    });
                }
                let bytes = length
                    .checked_mul(width)
                    .ok_or(NbtWireError::LengthOverflow { tag, length, width })?;
                if bytes > limits.max_array_bytes {
                    return Err(NbtWireError::ArrayTooLarge {
                        bytes,
                        max: limits.max_array_bytes,
                    });
                }
                take(input, pos, bytes)?;
            }
            8 => {
                let length = usize::from(read_u16_be(input, pos)?);
                let remaining = remaining(input, *pos);
                if length > remaining {
                    return Err(NbtWireError::LengthExceedsRemaining {
                        tag,
                        length,
                        remaining,
                    });
                }
                *pos += length;
            }
            9 => {
                let elem_tag = read_u8(input, pos)?;
                let length = read_i32_be(input, pos)?;
                if length < 0 {
                    return Err(NbtWireError::NegativeLength { tag });
                }
                let length = length as usize;
                if elem_tag == 0 && length != 0 {
                    return Err(NbtWireError::InvalidEmptyElementList { length });
                }
                if elem_tag > 12 {
                    return Err(NbtWireError::InvalidTagId(elem_tag));
                }
                if length > limits.max_list_len {
                    return Err(NbtWireError::ListTooLong {
                        length,
                        max: limits.max_list_len,
                    });
                }
                let remaining = remaining(input, *pos);
                if length > remaining {
                    return Err(NbtWireError::LengthExceedsRemaining {
                        tag,
                        length,
                        remaining,
                    });
                }
                if length != 0 {
                    if depth >= max_depth {
                        return Err(NbtWireError::DepthExceeded {
                            depth: depth + 1,
                            max: max_depth,
                        });
                    }
                    stack[depth] = Frame::List {
                        elem_tag,
                        remaining: length - 1,
                    };
                    depth += 1;
                    tag = elem_tag;
                    continue 'values;
                }
            }
            10 => {
                if depth >= max_depth {
                    return Err(NbtWireError::DepthExceeded {
                        depth: depth + 1,
                        max: max_depth,
                    });
                }
                stack[depth] = Frame::Compound;
                depth += 1;
                if let Some(child) = read_compound_child(input, pos)? {
                    tag = child;
                    continue 'values;
                }
                depth -= 1;
            }
            _ => return Err(NbtWireError::InvalidTagId(tag)),
        }

        // The current value is complete. Walk up the frame stack until a
        // parent has another child (or element), or the whole value is done.
        loop {
            if depth == 0 {
                return Ok(());
            }
            match stack[depth - 1] {
                Frame::List {
                    elem_tag,
                    remaining: left,
                } => {
                    if left != 0 {
                        stack[depth - 1] = Frame::List {
                            elem_tag,
                            remaining: left - 1,
                        };
                        tag = elem_tag;
                        continue 'values;
                    }
                    depth -= 1;
                }
                Frame::Compound => {
                    if let Some(child) = read_compound_child(input, pos)? {
                        tag = child;
                        continue 'values;
                    }
                    depth -= 1;
                }
            }
        }
    }
}

/// Read the next compound entry header, returning `None` for `TAG_End`.
///
/// The header is `tag id (u8)` followed, for non-`TAG_End`, by a `u16` name
/// length and the name bytes. The child payload itself is skipped by the
/// caller.
fn read_compound_child(input: &[u8], pos: &mut usize) -> Result<Option<u8>, NbtWireError> {
    let tag = read_u8(input, pos)?;
    if tag == 0 {
        return Ok(None);
    }
    if tag > 12 {
        return Err(NbtWireError::InvalidTagId(tag));
    }
    let name_len = usize::from(read_u16_be(input, pos)?);
    let remaining = remaining(input, *pos);
    if name_len > remaining {
        return Err(NbtWireError::LengthExceedsRemaining {
            tag: 8,
            length: name_len,
            remaining,
        });
    }
    *pos += name_len;
    Ok(Some(tag))
}

#[inline]
fn remaining(input: &[u8], pos: usize) -> usize {
    input.len().saturating_sub(pos)
}

/// Advance `pos` by `n` bytes, failing closed on overrun.
#[inline]
fn take(input: &[u8], pos: &mut usize, n: usize) -> Result<(), NbtWireError> {
    let remaining = remaining(input, *pos);
    if n > remaining {
        return Err(NbtWireError::UnexpectedEof {
            needed: n,
            remaining,
        });
    }
    *pos += n;
    Ok(())
}

#[inline]
fn read_u8(input: &[u8], pos: &mut usize) -> Result<u8, NbtWireError> {
    let remaining = remaining(input, *pos);
    let Some(&byte) = input.get(*pos) else {
        return Err(NbtWireError::UnexpectedEof {
            needed: 1,
            remaining,
        });
    };
    *pos += 1;
    Ok(byte)
}

#[inline]
fn read_u16_be(input: &[u8], pos: &mut usize) -> Result<u16, NbtWireError> {
    let start = *pos;
    take(input, pos, 2)?;
    Ok(u16::from_be_bytes([input[start], input[start + 1]]))
}

#[inline]
fn read_i32_be(input: &[u8], pos: &mut usize) -> Result<i32, NbtWireError> {
    let start = *pos;
    take(input, pos, 4)?;
    Ok(i32::from_be_bytes([
        input[start],
        input[start + 1],
        input[start + 2],
        input[start + 3],
    ]))
}
