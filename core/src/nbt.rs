// SPDX-License-Identifier: LGPL-3.0-or-later
#![forbid(unsafe_code)]
//! `megu_core::nbt` — the canonical binary NBT reader/writer, preserving every
//! Java tag type and Java's modified-UTF-8 string encoding.
//!
//! NBT is the on-disk and on-wire object format of Minecraft: chunk sections,
//! `level.dat`, `playerdata`, `data/*.dat`, structure/template files, item
//! components and (before 1.20.2) registry payloads. Four properties make a
//! second implementation dangerous, so this module is the one copy every crate
//! in the tree parses and emits:
//!
//! * **Java big-endian framing.** Fixed-width values, `i32` collection
//!   lengths and `u16` string lengths, all big-endian.
//! * **Modified UTF-8 strings** (`DataInput.readUTF`/`DataOutput.writeUTF`),
//!   not RFC 3629. NUL is encoded as a single `0x00` byte on input (and as the
//!   two-byte overlong form `0xC0 0x80` on output, exactly like Java), scalars
//!   are encoded one to three bytes at a time, and each UTF-16 code unit —
//!   **including a lone surrogate** — is preserved verbatim. Astral scalars
//!   therefore survive as their two surrogate halves and are never folded,
//!   replaced or re-encoded as four-byte UTF-8. Reading returns
//!   [`NbtString`] units, so a value Java can hold round-trips byte for byte
//!   even when it is not valid UTF-16.
//! * **Container depth and tag budget caps.** Attacker-controlled nesting can
//!   otherwise exhaust the process stack, and a wide-but-shallow list can
//!   exhaust memory. [`MAX_NBT_DEPTH`] and [`MAX_NBT_TAGS`] bound both, and
//!   every bound is fail-closed on the *read* path as well as the write path.
//! * **Canonical output.** Compounds are emitted in Java `UTF-16` key order
//!   (a `BTreeMap` over [`NbtString`], not code-point order), `NaN`s collapse
//!   to the canonical quiet-bit patterns, and an empty list carries element
//!   type[`Tag::End`]'s id. Round-tripping a document therefore produces the
//!   bytes vanilla's `NbtIo` would produce, which is what makes a digest of a
//!   written file comparable across implementations.
//!
//! # Shapes
//!
//! * [`read_named`] / [`write_named`] — the classic root: a tag id, a `u16`
//!   name length plus name bytes, then the payload. Used by save files,
//!   `level.dat` and every pre-1.20.2 network root.
//! * [`read_unnamed`] / [`write_unnamed`] — a whole-slice anonymous root: a
//!   tag id then the payload with **no** root name. This is the 1.20.2+ shape
//!   for item components, text components and standalone values.
//! * [`read_network_nbt`] / [`write_network_nbt`] — the same anonymous shape
//!   as a *cursor* over a larger packet: the compound root is read and
//!   `input` is left pointing at the bytes that follow, so one reader can
//!   consume NBT followed by more fields without buffering a sub-slice.
//!
//! Every reader rejects trailing bytes and every reader is total: malformed,
//! truncated, over-deep or over-budget input returns `Err(&'static str)`
//! rather than panicking or partially succeeding.
//!
//! The error type is the message string itself. That is deliberate: the root
//! crate's `src/nbt.rs` still owns its own consumers, and keeping the exact
//! signatures lets that lane migrate onto this module without changing a
//! single call site. [`NBT_DEPTH_ERROR`] is exported so callers that must
//! distinguish the depth cap from generic malformed input do not have to
//! duplicate the literal.

use std::collections::BTreeMap;

/// Largest container nesting a reader accepts, and the deepest nesting a
/// writer emits. The root tag sits at depth `0`, so `512` nested containers
/// are readable and the `513`th is rejected (`NBT exceeds container depth`).
///
/// Vanilla's own `NbtAccounter` uses no depth limit, but a 512-level cap
/// bounds the reader's recursion far below any thread's stack.
pub const MAX_NBT_DEPTH: usize = 512;

/// Aggregate tag budget for one document, root included. A tag costs one
/// unit regardless of type, so a wide-but-shallow `TAG_List` cannot force an
/// allocation larger than this cap before the reader fails closed.
pub const MAX_NBT_TAGS: usize = 250_000;

/// The message every depth-cap rejection carries, exported so a caller that
/// maps errors onto its own status codes matches on a shared spelling.
pub const NBT_DEPTH_ERROR: &str = "NBT exceeds container depth";

/// A Java `String` as NBT stores it: UTF-16 code units, one per unit.
///
/// Deliberately *not* a Rust `String`. Java strings are UTF-16, may contain
/// lone surrogates, and NBT preserves them verbatim; [`NbtString::to_utf8`]
/// is the fallible conversion for callers that need text, and it fails rather
/// than replacing a surrogate with `U+FFFD`.
#[derive(Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct NbtString(pub Vec<u16>);

impl NbtString {
    /// The UTF-16 units of this string, in order.
    pub fn units(&self) -> &[u16] {
        &self.0
    }

    /// Build a string from Rust text, splitting astral scalars into their two
    /// surrogate units exactly as Java would.
    pub fn text(value: &str) -> Self {
        Self(value.encode_utf16().collect())
    }

    /// Convert to Rust text, failing on a lone surrogate instead of replacing
    /// it. Use this when a caller needs `&str`; keep [`NbtString`] when the
    /// value must be written back unchanged.
    pub fn to_utf8(&self) -> Result<String, std::string::FromUtf16Error> {
        String::from_utf16(&self.0)
    }

    /// True when the string holds no units.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl From<&str> for NbtString {
    fn from(value: &str) -> Self {
        Self::text(value)
    }
}

impl From<String> for NbtString {
    fn from(value: String) -> Self {
        Self::text(&value)
    }
}

impl From<Vec<u16>> for NbtString {
    fn from(value: Vec<u16>) -> Self {
        Self(value)
    }
}

/// Every NBT tag type, with the exact payload the wire carries.
///
/// `Float` and `Double` keep their IEEE-754 **bits**, not the float value, so
/// a document whose floats are `NaN` or negative zero survives a load/save
/// cycle unchanged instead of being normalized by a parse to `f32`/`f64`.
/// Compound keys are [`NbtString`]s in a `BTreeMap`, which is Java's
/// `UTF-16`-unit ordering — the order vanilla writes.
#[derive(Clone, Debug, PartialEq)]
pub enum Tag {
    /// `TAG_End`, id `0`. Valid only as a list element type and as a
    /// compound terminator; never a value.
    End,
    Byte(i8),
    Short(i16),
    Int(i32),
    Long(i64),
    /// `TAG_Float` payload bits, as read from the wire.
    Float(u32),
    /// `TAG_Double` payload bits, as read from the wire.
    Double(u64),
    ByteArray(Vec<u8>),
    String(NbtString),
    /// `TAG_List` carries its homogeneous element id and its elements.
    List(u8, Vec<Tag>),
    Compound(BTreeMap<NbtString, Tag>),
    IntArray(Vec<i32>),
    LongArray(Vec<i64>),
}

impl Tag {
    /// The numeric tag id this value writes.
    pub fn id(&self) -> u8 {
        match self {
            Self::End => 0,
            Self::Byte(_) => 1,
            Self::Short(_) => 2,
            Self::Int(_) => 3,
            Self::Long(_) => 4,
            Self::Float(_) => 5,
            Self::Double(_) => 6,
            Self::ByteArray(_) => 7,
            Self::String(_) => 8,
            Self::List(..) => 9,
            Self::Compound(_) => 10,
            Self::IntArray(_) => 11,
            Self::LongArray(_) => 12,
        }
    }

    /// The value of `key` when this tag is a compound, else `None`.
    pub fn get(&self, key: &str) -> Option<&Tag> {
        if let Self::Compound(values) = self {
            values.get(&NbtString::text(key))
        } else {
            None
        }
    }

    /// The elements when this tag is a list, else `None`.
    pub fn list(&self) -> Option<&[Tag]> {
        if let Self::List(_, values) = self {
            Some(values)
        } else {
            None
        }
    }

    /// The value when this tag is a `TAG_Int`, else `None`.
    pub fn int(&self) -> Option<i32> {
        if let Self::Int(v) = self {
            Some(*v)
        } else {
            None
        }
    }

    /// The value when this tag is a `TAG_String`, else `None`.
    pub fn string(&self) -> Option<&NbtString> {
        if let Self::String(v) = self {
            Some(v)
        } else {
            None
        }
    }
}

struct Reader<'a> {
    input: &'a [u8],
    remaining_tags: usize,
}

impl<'a> Reader<'a> {
    fn new(input: &'a [u8]) -> Self {
        Self {
            input,
            remaining_tags: MAX_NBT_TAGS,
        }
    }

    fn bytes(&mut self, n: usize) -> Result<&'a [u8], &'static str> {
        let (v, rest) = self.input.split_at_checked(n).ok_or("truncated NBT")?;
        self.input = rest;
        Ok(v)
    }

    fn number<const N: usize>(&mut self) -> Result<[u8; N], &'static str> {
        let bytes = self.bytes(N)?;
        let mut out = [0u8; N];
        out.copy_from_slice(bytes);
        Ok(out)
    }

    /// Read a collection length and reject it before allocating anything: a
    /// declared length larger than the bytes left in the input cannot be
    /// honest, and a negative `i32` length is never valid.
    fn length(&mut self, element_size: usize) -> Result<usize, &'static str> {
        let n = i32::from_be_bytes(self.number()?);
        if n < 0 || n as usize > self.input.len() / element_size {
            return Err("invalid NBT collection length");
        }
        Ok(n as usize)
    }

    fn string(&mut self) -> Result<NbtString, &'static str> {
        let len = u16::from_be_bytes(self.number()?) as usize;
        let bytes = self.bytes(len)?;
        let mut units = Vec::new();
        let mut i = 0;
        // `DataInput.readUTF` accepts a single zero byte and the overlong
        // two- and three-byte forms, rejects four-byte UTF-8, and preserves
        // individual surrogate units. Anything stricter would refuse files
        // vanilla writes; anything looser would accept bytes vanilla rejects.
        while i < bytes.len() {
            let b = bytes[i];
            i += 1;
            let unit = match b >> 4 {
                0..=7 => u16::from(b),
                12 | 13 => {
                    let c = *bytes.get(i).ok_or("truncated modified UTF-8")?;
                    i += 1;
                    if c & 0xc0 != 0x80 {
                        return Err("invalid modified UTF-8 continuation");
                    }
                    (u16::from(b & 31) << 6) | u16::from(c & 63)
                }
                14 => {
                    let c = *bytes.get(i).ok_or("truncated modified UTF-8")?;
                    let d = *bytes.get(i + 1).ok_or("truncated modified UTF-8")?;
                    i += 2;
                    if c & 0xc0 != 0x80 || d & 0xc0 != 0x80 {
                        return Err("invalid modified UTF-8 continuation");
                    }
                    (u16::from(b & 15) << 12) | (u16::from(c & 63) << 6) | u16::from(d & 63)
                }
                _ => return Err("invalid modified UTF-8 leading byte"),
            };
            units.push(unit);
        }
        Ok(NbtString(units))
    }

    fn tag(&mut self, id: u8, depth: usize) -> Result<Tag, &'static str> {
        if self.remaining_tags == 0 {
            return Err("NBT exceeds tag count budget");
        }
        self.remaining_tags -= 1;
        Ok(match id {
            0 => Tag::End,
            1 => Tag::Byte(i8::from_be_bytes(self.number()?)),
            2 => Tag::Short(i16::from_be_bytes(self.number()?)),
            3 => Tag::Int(i32::from_be_bytes(self.number()?)),
            4 => Tag::Long(i64::from_be_bytes(self.number()?)),
            5 => Tag::Float(u32::from_be_bytes(self.number()?)),
            6 => Tag::Double(u64::from_be_bytes(self.number()?)),
            7 => {
                let n = self.length(1)?;
                Tag::ByteArray(self.bytes(n)?.to_vec())
            }
            8 => Tag::String(self.string()?),
            9 => {
                if depth >= MAX_NBT_DEPTH {
                    return Err(NBT_DEPTH_ERROR);
                }
                let kind = self.number::<1>()?[0];
                let n = self.length(1)?;
                if kind > 12 || (kind == 0 && n != 0) {
                    return Err("invalid NBT list element type");
                }
                let mut values = Vec::new();
                for _ in 0..n {
                    values.push(self.tag(kind, depth + 1)?);
                }
                Tag::List(kind, values)
            }
            10 => {
                if depth >= MAX_NBT_DEPTH {
                    return Err(NBT_DEPTH_ERROR);
                }
                let mut values = BTreeMap::new();
                loop {
                    let kind = self.number::<1>()?[0];
                    if kind == 0 {
                        break;
                    }
                    if kind > 12 {
                        return Err("unknown NBT compound tag");
                    }
                    let key = self.string()?;
                    let value = self.tag(kind, depth + 1)?;
                    values.insert(key, value);
                }
                Tag::Compound(values)
            }
            11 => {
                let n = self.length(4)?;
                let mut values = Vec::new();
                for _ in 0..n {
                    values.push(i32::from_be_bytes(self.number()?));
                }
                Tag::IntArray(values)
            }
            12 => {
                let n = self.length(8)?;
                let mut values = Vec::new();
                for _ in 0..n {
                    values.push(i64::from_be_bytes(self.number()?));
                }
                Tag::LongArray(values)
            }
            _ => return Err("unknown NBT tag"),
        })
    }
}

/// Read a named root: tag id, modified-UTF-8 name, payload. Trailing bytes are
/// rejected, so a caller cannot silently accept a truncated-then-padded file.
pub fn read_named(input: &[u8]) -> Result<(NbtString, Tag), &'static str> {
    let mut reader = Reader::new(input);
    let id = reader.number::<1>()?[0];
    let name = if id == 0 {
        NbtString(Vec::new())
    } else {
        reader.string()?
    };
    let value = reader.tag(id, 0)?;
    if !reader.input.is_empty() {
        return Err("trailing bytes after named NBT");
    }
    Ok((name, value))
}

/// Read an anonymous (1.20.2+ network, item- and text-component) root: a tag
/// id then its payload, with no name. `TAG_End` is not a value, so it is
/// rejected as a root; trailing bytes are rejected as in [`read_named`].
pub fn read_unnamed(input: &[u8]) -> Result<Tag, &'static str> {
    let mut reader = Reader::new(input);
    let id = reader.number::<1>()?[0];
    if id == 0 {
        return Err("anonymous NBT root must not be TAG_End");
    }
    let value = reader.tag(id, 0)?;
    if !reader.input.is_empty() {
        return Err("trailing bytes after unnamed NBT");
    }
    Ok(value)
}

/// Read one anonymous compound root from a packet stream, leaving every byte
/// after it untouched in `input`.
///
/// Protocol 767's registry and component payloads use this shape: the root is
/// always `TAG_Compound`, the root name and its length are omitted, and the
/// next field of the packet follows immediately.
pub fn read_network_nbt(input: &mut &[u8]) -> Result<Tag, &'static str> {
    let Some((&10, rest)) = input.split_first() else {
        return Err("network registry NBT root must be a compound");
    };
    let mut reader = Reader::new(rest);
    let tag = reader.tag(10, 0)?;
    *input = reader.input;
    Ok(tag)
}

fn write_string(value: &NbtString, out: &mut Vec<u8>) -> Result<(), &'static str> {
    let mut bytes = Vec::new();
    for &unit in &value.0 {
        if (1..=127).contains(&unit) {
            bytes.push(unit as u8);
        } else if unit <= 0x7ff {
            // NUL and the other C0 controls take the overlong two-byte form,
            // exactly as `DataOutput.writeUTF` emits them.
            bytes.push(0xc0 | (unit >> 6) as u8);
            bytes.push(0x80 | (unit & 63) as u8);
        } else {
            bytes.push(0xe0 | (unit >> 12) as u8);
            bytes.push(0x80 | ((unit >> 6) & 63) as u8);
            bytes.push(0x80 | (unit & 63) as u8);
        }
    }
    let len =
        u16::try_from(bytes.len()).map_err(|_| "modified UTF-8 string exceeds 65535 bytes")?;
    out.extend_from_slice(&len.to_be_bytes());
    out.extend(bytes);
    Ok(())
}

fn write_length(n: usize, out: &mut Vec<u8>) -> Result<(), &'static str> {
    out.extend_from_slice(
        &i32::try_from(n)
            .map_err(|_| "NBT collection too large")?
            .to_be_bytes(),
    );
    Ok(())
}

fn write_payload(tag: &Tag, out: &mut Vec<u8>, depth: usize) -> Result<(), &'static str> {
    match tag {
        Tag::End => {}
        Tag::Byte(v) => out.push(*v as u8),
        Tag::Short(v) => out.extend_from_slice(&v.to_be_bytes()),
        Tag::Int(v) => out.extend_from_slice(&v.to_be_bytes()),
        Tag::Long(v) => out.extend_from_slice(&v.to_be_bytes()),
        Tag::Float(v) => {
            let bits = if f32::from_bits(*v).is_nan() {
                0x7fc00000
            } else {
                *v
            };
            out.extend_from_slice(&bits.to_be_bytes());
        }
        Tag::Double(v) => {
            let bits = if f64::from_bits(*v).is_nan() {
                0x7ff8000000000000
            } else {
                *v
            };
            out.extend_from_slice(&bits.to_be_bytes());
        }
        Tag::ByteArray(v) => {
            write_length(v.len(), out)?;
            out.extend_from_slice(v);
        }
        Tag::String(v) => write_string(v, out)?,
        Tag::List(kind, values) => {
            if depth >= MAX_NBT_DEPTH {
                return Err(NBT_DEPTH_ERROR);
            }
            if *kind > 12 || values.iter().any(|v| v.id() != *kind || *kind == 0) {
                return Err("invalid NBT list elements");
            }
            out.push(if values.is_empty() { 0 } else { *kind });
            write_length(values.len(), out)?;
            for value in values {
                write_payload(value, out, depth + 1)?;
            }
        }
        Tag::Compound(values) => {
            if depth >= MAX_NBT_DEPTH {
                return Err(NBT_DEPTH_ERROR);
            }
            for (key, value) in values {
                if value.id() == 0 {
                    return Err("NBT compound cannot contain end values");
                }
                out.push(value.id());
                write_string(key, out)?;
                write_payload(value, out, depth + 1)?;
            }
            out.push(0);
        }
        Tag::IntArray(v) => {
            write_length(v.len(), out)?;
            for n in v {
                out.extend_from_slice(&n.to_be_bytes());
            }
        }
        Tag::LongArray(v) => {
            write_length(v.len(), out)?;
            for n in v {
                out.extend_from_slice(&n.to_be_bytes());
            }
        }
    }
    Ok(())
}

/// Canonical named NBT: Java UTF-16 key order, canonical `NaN`s and empty
/// lists. Compound order is semantic, not a promise to preserve source byte
/// order.
pub fn write_named(name: &NbtString, tag: &Tag) -> Result<Vec<u8>, &'static str> {
    let mut out = vec![tag.id()];
    if tag.id() != 0 {
        write_string(name, &mut out)?;
    }
    write_payload(tag, &mut out, 0)?;
    Ok(out)
}

/// Canonical anonymous NBT: a tag id then the payload, with no root name.
/// `TAG_End` is not a value and has nothing to write, so it is rejected.
pub fn write_unnamed(tag: &Tag) -> Result<Vec<u8>, &'static str> {
    if tag.id() == 0 {
        return Err("anonymous NBT root must not be TAG_End");
    }
    let mut out = vec![tag.id()];
    write_payload(tag, &mut out, 0)?;
    Ok(out)
}

/// Write a network registry root: a compound tag byte then its payload, with
/// the root name and its length omitted.
pub fn write_network_nbt(tag: &Tag) -> Result<Vec<u8>, &'static str> {
    if !matches!(tag, Tag::Compound(_)) {
        return Err("network registry NBT root must be a compound");
    }
    let mut out = vec![10];
    write_payload(tag, &mut out, 0)?;
    Ok(out)
}
