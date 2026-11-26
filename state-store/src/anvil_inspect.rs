// SPDX-License-Identifier: LGPL-3.0-or-later
//! Pure-`std` inspector for vanilla Anvil world directories, plus the binary
//! NBT <-> [`DataValue`] bridge every `level.dat` and `*.mca` payload is
//! stored in.
//!
//! The crate contract is zero crates.io dependencies, so this module carries
//! its own codecs, following the same clean-room pattern as
//! `client-kernel/src/gzip.rs` and `mod-adapter-core/src/modpack.rs`:
//!
//! * an RFC 1952 gzip reader (member header/trailer parsing, CRC32 + ISIZE
//!   verification, multi-member support) over an RFC 1951 DEFLATE inflater —
//!   enough to read `level.dat`, which vanilla always writes gzip-wrapped;
//! * [`gzip_encode_stored`], a deterministic RFC 1952 writer that emits
//!   stored (uncompressed) DEFLATE blocks, so fixtures and tools can produce
//!   `level.dat`-shaped files without a compressor;
//! * a strict binary NBT codec ([`decode_nbt_to_data_value`] /
//!   [`encode_data_value_to_nbt`]) covering every vanilla tag
//!   (`End`/`Byte`/`Short`/`Int`/`Long`/`Float`/`Double`/`ByteArray`/
//!   `String`/`List`/`Compound`/`IntArray`/`LongArray`) with a hard depth cap
//!   of [`NBT_DEPTH_CAP`] and an input cap of [`NBT_SIZE_CAP`]. Strings are
//!   decoded as Java modified UTF-8 and must be representable as UTF-8 —
//!   a lone surrogate fails closed rather than being silently replaced.
//!
//! The NBT -> [`DataValue`] projection is necessarily type-coarser:
//! `DataValue` has a single `Int(i64)`/`Float(f64)`/`List` shape, so the
//! 8/16/32/64-bit integer widths and the three array tags collapse onto them.
//! [`encode_data_value_to_nbt`] picks canonical output types (`Bool` ->
//! `Byte`, `Int` -> `Int` when it fits `i32` else `Long`, `Float` ->
//! `Double`, `Map` -> `Compound`) and refuses what NBT cannot say: `Null`
//! anywhere, heterogeneous lists, or strings past the 64 KiB MUTF-8 limit.
//!
//! Above the codec sit two inspectors:
//!
//! * [`inspect_mca_header`] reads the 8,192-byte Anvil region header (1,024
//!   location entries + 1,024 timestamps), counts populated chunks, checks
//!   every declared sector run against the real file size, and classifies
//!   each chunk's compression byte (`1` GZip, `2` Zlib, `3` uncompressed,
//!   `4` LZ4, anything else unknown) — failing closed on a truncated header,
//!   a half-populated location entry, an out-of-bounds sector run, or a
//!   chunk record length that cannot fit its sectors.
//! * [`inspect_anvil_world_dir`] reads `<dir>/level.dat` (gzip-compressed or
//!   raw uncompressed NBT), extracts `Data.DataVersion`,
//!   `Data.Version.Name`, `Data.LevelName`, `Data.RandomSeed` (falling back
//!   to `Data.WorldGenSettings.seed`) and the `Data.DataPacks` enabled/
//!   disabled lists, classifies the save against
//!   [`megu_core::version::TargetVersion`] (`1.21.1` = 3955, `26.3` =
//!   5023), and scans `region/`, `DIM-1/region/` and `DIM1/region/` for
//!   `r.<x>.<z>.mca` files. The resulting [`AnvilWorldReport`] renders as
//!   deterministic JSON ([`AnvilWorldReport::to_json`]) or a plain-English
//!   summary ([`AnvilWorldReport::to_human`]).
//!
//! [`migrate_anvil_level_dat`] closes the loop: it decodes a `level.dat`
//! (gzip or raw), runs the document through the
//! [`crate::datafix::DataFixer`] between the two pinned targets, and
//! re-seals it as a verified RFC 1952 gzip member — the direct file-level
//! migrator behind `megu-world-migrate migrate-level`.

use crate::datafix::{DataFixError, DataFixReport, DataFixer, DataValue, DV_1_21_1, DV_26_3};
use crate::migrate::to_data_json;
use megu_core::hash::crc32_ieee as crc32;
use megu_core::nbt_wire::{self, NbtWireError, NbtWireLimits};
use megu_core::version::TargetVersion;
use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;
use std::path::{Path, PathBuf};

// ------------------------------------------------------------- caps & consts

/// Hard cap on NBT container nesting for both directions of the codec.
/// `level.dat` and chunk documents nest a handful of levels; 64 is generous
/// headroom that still makes a stack-exhausting document impossible.
pub const NBT_DEPTH_CAP: usize = 64;

/// Maximum byte size of a raw or inflated NBT document the codec accepts.
/// Vanilla `level.dat` files sit in the low kilobytes and chunk payloads in
/// the low megabytes; 32 MiB refuses decompression-bomb-shaped inputs while
/// leaving every real document room.
pub const NBT_SIZE_CAP: usize = 32 << 20;

/// Bytes in one Anvil sector — the unit region files are laid out in.
pub const MCA_SECTOR_BYTES: usize = 4096;
/// Bytes of the two leading header sectors (locations + timestamps).
pub const MCA_HEADER_BYTES: usize = 2 * MCA_SECTOR_BYTES;
/// Chunk slots in one region file (32 x 32).
pub const MCA_SLOT_COUNT: usize = 1024;
/// Chunk records may not declare sector runs starting inside the header.
const MCA_FIRST_DATA_SECTOR: u32 = 2;

// ------------------------------------------------------------------- errors

/// Why an inspect operation could not produce a report.
#[derive(Clone, Debug, PartialEq)]
pub enum AnvilInspectError {
    /// A file under the world directory could not be read (or the directory
    /// itself could not be listed). `io::Error` is not `Clone`, so the
    /// message is captured instead.
    Io { path: PathBuf, message: String },
    /// `<dir>/level.dat` does not exist or is not a regular file.
    MissingLevelDat { path: PathBuf },
    /// Raw or inflated NBT exceeds [`NBT_SIZE_CAP`].
    NbtTooLarge { len: usize, cap: usize },
    /// The stream ends before the value being read completes.
    TruncatedNbt { offset: usize },
    /// Container nesting exceeds [`NBT_DEPTH_CAP`].
    NbtTooDeep { depth: usize },
    /// A tag id, list element type or collection length field is invalid.
    MalformedNbt { offset: usize, message: String },
    /// An NBT string is malformed modified UTF-8 or is not representable as
    /// UTF-8 (e.g. an unpaired surrogate).
    InvalidNbtString { offset: usize },
    /// Bytes remain after the root tag — a complete document never trails.
    TrailingNbtBytes { count: usize },
    /// A [`DataValue`] cannot be written as NBT: `Null`, a heterogeneous
    /// list, or a string that encodes past the 64 KiB limit.
    UnrepresentableValue { message: String },
    /// The gzip wrapper is not valid RFC 1952 / RFC 1951.
    InvalidGzip(&'static str),
    /// The inflated payload exceeds the caller's byte budget.
    GzipOverBudget { budget: usize },
    /// The region file is shorter than the 8,192-byte header.
    TruncatedRegionHeader { len: usize },
    /// A location entry is half-populated or points into the header
    /// sectors.
    InvalidRegionLocation {
        slot: usize,
        offset: u32,
        sectors: u8,
    },
    /// The declared sector run extends past the end of the file.
    SectorRunOutOfBounds {
        slot: usize,
        offset: u32,
        sectors: u8,
        file_len: usize,
    },
    /// The chunk record's declared length cannot fit inside its sectors.
    InvalidChunkRecordLength {
        slot: usize,
        length: u32,
        sectors: u8,
    },
    /// `level.dat` decoded but a required field is missing or has the wrong
    /// shape (`Data` compound, `Data.DataVersion` int).
    MissingLevelField { field: &'static str },
    /// A file inside the world directory failed inspection; `message`
    /// carries the underlying reason with the path attached for context.
    InvalidWorldFile { path: PathBuf, message: String },
    /// A `26.3` -> `1.21.1` `level.dat` migration was requested without the
    /// explicit `allow_downgrade` opt-in. Refusing is the default:
    /// downgrades can lose data the older format cannot represent.
    DowngradeNotAllowed {
        /// `DataVersion` the input document carries.
        from_dv: i32,
        /// `DataVersion` the caller asked for.
        to_dv: i32,
    },
    /// The [`DataFixer`] refused the decoded document (a `DataVersion` it
    /// has no schema for, or one it cannot read).
    Fix(DataFixError),
    /// A freshly encoded `level.dat` failed its own decode check. This is
    /// an internal invariant — it should be unreachable — and it is checked
    /// so a bug can never ship an unreadable output as a successful
    /// migration.
    OutputInvalid { message: String },
}

impl fmt::Display for AnvilInspectError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AnvilInspectError::Io { path, message } => {
                write!(f, "cannot read {}: {message}", path.display())
            }
            AnvilInspectError::MissingLevelDat { path } => {
                write!(f, "no level.dat at {}", path.display())
            }
            AnvilInspectError::NbtTooLarge { len, cap } => {
                write!(f, "NBT document is {len} bytes, over the {cap}-byte cap")
            }
            AnvilInspectError::TruncatedNbt { offset } => {
                write!(f, "NBT ends mid-value at byte {offset}")
            }
            AnvilInspectError::NbtTooDeep { depth } => {
                write!(f, "NBT container nesting exceeds {depth} levels")
            }
            AnvilInspectError::MalformedNbt { offset, message } => {
                write!(f, "malformed NBT at byte {offset}: {message}")
            }
            AnvilInspectError::InvalidNbtString { offset } => {
                write!(
                    f,
                    "NBT string at byte {offset} is not representable as UTF-8"
                )
            }
            AnvilInspectError::TrailingNbtBytes { count } => {
                write!(f, "{count} trailing byte(s) after the NBT root tag")
            }
            AnvilInspectError::UnrepresentableValue { message } => {
                write!(f, "value cannot be written as NBT: {message}")
            }
            AnvilInspectError::InvalidGzip(reason) => write!(f, "invalid gzip: {reason}"),
            AnvilInspectError::GzipOverBudget { budget } => {
                write!(f, "gzip payload exceeds the {budget}-byte budget")
            }
            AnvilInspectError::TruncatedRegionHeader { len } => write!(
                f,
                "region file is {len} bytes, shorter than the 8,192-byte header"
            ),
            AnvilInspectError::InvalidRegionLocation {
                slot,
                offset,
                sectors,
            } => write!(
                f,
                "region location entry {slot} is half-populated or points into the header (offset {offset}, {sectors} sector(s))"
            ),
            AnvilInspectError::SectorRunOutOfBounds {
                slot,
                offset,
                sectors,
                file_len,
            } => write!(
                f,
                "region slot {slot} declares sector run {offset}+{sectors} past the {file_len}-byte file"
            ),
            AnvilInspectError::InvalidChunkRecordLength {
                slot,
                length,
                sectors,
            } => write!(
                f,
                "region slot {slot} declares chunk length {length} that cannot fit {sectors} sector(s)"
            ),
            AnvilInspectError::MissingLevelField { field } => {
                write!(f, "level.dat has no readable {field}")
            }
            AnvilInspectError::InvalidWorldFile { path, message } => {
                write!(f, "{}: {message}", path.display())
            }
            AnvilInspectError::DowngradeNotAllowed { from_dv, to_dv } => write!(
                f,
                "refusing to downgrade data version {from_dv} -> {to_dv} without explicit opt-in"
            ),
            AnvilInspectError::Fix(e) => write!(f, "document migration refused: {e}"),
            AnvilInspectError::OutputInvalid { message } => write!(
                f,
                "internal error: migrated level.dat failed decode: {message}"
            ),
        }
    }
}

impl Error for AnvilInspectError {}

fn io_error(path: &Path, e: std::io::Error) -> AnvilInspectError {
    AnvilInspectError::Io {
        path: path.to_path_buf(),
        message: e.to_string(),
    }
}

// --------------------------------------------------------------------- gzip

/// Inflate `input`, which must be one or more concatenated RFC 1952 gzip
/// members, into at most `budget` bytes.
///
/// Fail-closed end to end: bad magic, reserved flag bits, a malformed
/// deflate stream, a CRC32/ISIZE mismatch or a truncated member are all
/// errors, and the inflater stops at `budget` bytes rather than letting a
/// small file expand without bound.
pub fn gzip_decompress(input: &[u8], budget: usize) -> Result<Vec<u8>, AnvilInspectError> {
    let mut input = input;
    if input.is_empty() {
        return Err(AnvilInspectError::InvalidGzip("empty input"));
    }
    let mut output = Vec::new();
    while !input.is_empty() {
        let start = output.len();
        let payload = gzip_member_payload(input)?;
        let (member, consumed) =
            megu_core::zip_archive::inflate_raw_rfc1951_with_consumed(payload, budget - start)
                .map_err(|error| match error {
                    megu_core::zip_archive::ZipError::OutputCapExceeded => {
                        AnvilInspectError::GzipOverBudget { budget }
                    }
                    _ => AnvilInspectError::InvalidGzip("invalid deflate stream"),
                })?;
        output.extend_from_slice(&member);
        let mut trailer_and_rest = &payload[consumed..];
        let crc = u32::from_le_bytes(gzip_take(&mut trailer_and_rest, 4)?.try_into().unwrap());
        let size = u32::from_le_bytes(gzip_take(&mut trailer_and_rest, 4)?.try_into().unwrap());
        let member = &output[start..];
        if crc != crc32(member) || size != member.len() as u32 {
            return Err(AnvilInspectError::InvalidGzip("CRC32/size mismatch"));
        }
        input = trailer_and_rest;
    }
    Ok(output)
}

/// Wrap `payload` in a valid RFC 1952 gzip member using stored
/// (uncompressed) DEFLATE blocks.
///
/// No compressor lives in this crate, so this emits the legal "stored"
/// block type: larger than a real compressor's output but readable by every
/// inflate implementation, byte-deterministic, and enough for fixtures and
/// round-trip tools.
pub fn gzip_encode_stored(payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(payload.len() + payload.len() / 65_535 * 5 + 23);
    // Magic, method 8 (deflate), no flags, zero MTIME, XFL 0, OS 255.
    out.extend_from_slice(&[0x1f, 0x8b, 8, 0, 0, 0, 0, 0, 0, 0xff]);
    let mut rest = payload;
    loop {
        let n = rest.len().min(65_535);
        let last = n == rest.len();
        // Stored block header: BFINAL + BTYPE=00, then LEN and ~LEN, LE.
        out.push(u8::from(last));
        out.extend_from_slice(&(n as u16).to_le_bytes());
        out.extend_from_slice(&(!(n as u16)).to_le_bytes());
        out.extend_from_slice(&rest[..n]);
        rest = &rest[n..];
        if last {
            break;
        }
    }
    out.extend_from_slice(&crc32(payload).to_le_bytes());
    out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    out
}

fn gzip_take<'a>(input: &mut &'a [u8], n: usize) -> Result<&'a [u8], AnvilInspectError> {
    let (taken, rest) = input
        .split_at_checked(n)
        .ok_or(AnvilInspectError::InvalidGzip("truncated member"))?;
    *input = rest;
    Ok(taken)
}

/// Parse the RFC 1952 member header, returning the deflate payload region.
fn gzip_member_payload(header: &[u8]) -> Result<&[u8], AnvilInspectError> {
    let mut rest = header;
    let fixed = gzip_take(&mut rest, 10)?;
    if fixed[..3] != [0x1f, 0x8b, 8] || fixed[3] & 0xe0 != 0 {
        return Err(AnvilInspectError::InvalidGzip("bad magic/method/flags"));
    }
    let flags = fixed[3];
    if flags & 4 != 0 {
        let length = u16::from_le_bytes(gzip_take(&mut rest, 2)?.try_into().unwrap());
        gzip_take(&mut rest, usize::from(length))?;
    }
    for flag in [8, 16] {
        if flags & flag != 0 {
            let end = rest
                .iter()
                .position(|&b| b == 0)
                .ok_or(AnvilInspectError::InvalidGzip("unterminated header field"))?;
            gzip_take(&mut rest, end + 1)?;
        }
    }
    if flags & 2 != 0 {
        let expected = crc32(&header[..header.len() - rest.len()]) as u16;
        let actual = u16::from_le_bytes(gzip_take(&mut rest, 2)?.try_into().unwrap());
        if actual != expected {
            return Err(AnvilInspectError::InvalidGzip("header CRC16 mismatch"));
        }
    }
    Ok(rest)
}

// ------------------------------------------------------- NBT -> DataValue

/// Java modified-UTF-8 decoder: `DataInput.readUTF` accepts single zero and
/// overlong 2/3-byte forms, rejects four-byte UTF-8, and preserves
/// individual surrogate units. Returning `Option` lets the caller fail
/// closed on strings a Rust `String` cannot hold.
fn mutf8_decode(bytes: &[u8]) -> Option<String> {
    let mut units = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        i += 1;
        let unit = match b >> 4 {
            0..=7 => u16::from(b),
            12 | 13 => {
                let c = *bytes.get(i)?;
                i += 1;
                if c & 0xc0 != 0x80 {
                    return None;
                }
                (u16::from(b & 31) << 6) | u16::from(c & 63)
            }
            14 => {
                let c = *bytes.get(i)?;
                let d = *bytes.get(i + 1)?;
                i += 2;
                if c & 0xc0 != 0x80 || d & 0xc0 != 0x80 {
                    return None;
                }
                (u16::from(b & 15) << 12) | (u16::from(c & 63) << 6) | u16::from(d & 63)
            }
            _ => return None,
        };
        units.push(unit);
    }
    String::from_utf16(&units).ok()
}

/// Modified-UTF-8 encode: supplementary characters become the surrogate
/// pair `writeUTF` emits, and NUL becomes its two-byte encoding.
fn mutf8_encode(value: &str) -> Vec<u8> {
    let mut bytes = Vec::new();
    for unit in value.encode_utf16() {
        if (1..=127).contains(&unit) {
            bytes.push(unit as u8);
        } else if unit <= 0x7ff {
            bytes.push(0xc0 | (unit >> 6) as u8);
            bytes.push(0x80 | (unit & 63) as u8);
        } else {
            bytes.push(0xe0 | (unit >> 12) as u8);
            bytes.push(0x80 | ((unit >> 6) & 63) as u8);
            bytes.push(0x80 | (unit & 63) as u8);
        }
    }
    bytes
}

struct NbtReader<'a> {
    input: &'a [u8],
    pos: usize,
}

impl<'a> NbtReader<'a> {
    fn bytes(&mut self, n: usize) -> Result<&'a [u8], AnvilInspectError> {
        let end = self
            .pos
            .checked_add(n)
            .filter(|&end| end <= self.input.len())
            .ok_or(AnvilInspectError::TruncatedNbt { offset: self.pos })?;
        let out = &self.input[self.pos..end];
        self.pos = end;
        Ok(out)
    }

    fn number<const N: usize>(&mut self) -> Result<[u8; N], AnvilInspectError> {
        Ok(self.bytes(N)?.try_into().unwrap())
    }

    /// A collection length is valid only when the remaining input could
    /// actually hold that many elements — the check runs before any
    /// allocation so a forged count cannot force one.
    fn length(&mut self, element_size: usize) -> Result<usize, AnvilInspectError> {
        let at = self.pos;
        let n = i32::from_be_bytes(self.number::<4>()?);
        if n < 0 || n as usize > (self.input.len() - self.pos) / element_size {
            return Err(AnvilInspectError::MalformedNbt {
                offset: at,
                message: format!("collection length {n} exceeds remaining input"),
            });
        }
        Ok(n as usize)
    }

    fn string(&mut self) -> Result<String, AnvilInspectError> {
        let at = self.pos;
        let len = usize::from(u16::from_be_bytes(self.number::<2>()?));
        let bytes = self.bytes(len)?;
        mutf8_decode(bytes).ok_or(AnvilInspectError::InvalidNbtString { offset: at })
    }

    /// Decode one tag payload. `id` has already been consumed; `depth` is the
    /// nesting level of the value itself (root = 0).
    fn tag(&mut self, id: u8, depth: usize) -> Result<DataValue, AnvilInspectError> {
        Ok(match id {
            0 => DataValue::Null,
            1 => DataValue::Int(i64::from(i8::from_be_bytes(self.number::<1>()?))),
            2 => DataValue::Int(i64::from(i16::from_be_bytes(self.number::<2>()?))),
            3 => DataValue::Int(i64::from(i32::from_be_bytes(self.number::<4>()?))),
            4 => DataValue::Int(i64::from_be_bytes(self.number::<8>()?)),
            5 => DataValue::Float(f64::from(f32::from_be_bytes(self.number::<4>()?))),
            6 => DataValue::Float(f64::from_be_bytes(self.number::<8>()?)),
            7 => {
                let n = self.length(1)?;
                DataValue::List(
                    self.bytes(n)?
                        .iter()
                        .map(|&b| DataValue::Int(i64::from(b as i8)))
                        .collect(),
                )
            }
            8 => DataValue::Str(self.string()?),
            9 => {
                if depth >= NBT_DEPTH_CAP {
                    return Err(AnvilInspectError::NbtTooDeep { depth });
                }
                let kind_at = self.pos;
                let kind = self.number::<1>()?[0];
                let n = self.length(1)?;
                if kind > 12 || (kind == 0 && n != 0) {
                    return Err(AnvilInspectError::MalformedNbt {
                        offset: kind_at,
                        message: format!("invalid list element type {kind}"),
                    });
                }
                let mut values = Vec::with_capacity(n.min(4096));
                for _ in 0..n {
                    values.push(self.tag(kind, depth + 1)?);
                }
                DataValue::List(values)
            }
            10 => {
                if depth >= NBT_DEPTH_CAP {
                    return Err(AnvilInspectError::NbtTooDeep { depth });
                }
                let mut values = BTreeMap::new();
                loop {
                    let kind_at = self.pos;
                    let kind = self.number::<1>()?[0];
                    if kind == 0 {
                        break;
                    }
                    if kind > 12 {
                        return Err(AnvilInspectError::MalformedNbt {
                            offset: kind_at,
                            message: format!("unknown compound member tag {kind}"),
                        });
                    }
                    let key = self.string()?;
                    let value = self.tag(kind, depth + 1)?;
                    values.insert(key, value);
                }
                DataValue::Map(values)
            }
            11 => {
                let n = self.length(4)?;
                let mut values = Vec::with_capacity(n.min(4096));
                for _ in 0..n {
                    values.push(DataValue::Int(i64::from(i32::from_be_bytes(
                        self.number::<4>()?,
                    ))));
                }
                DataValue::List(values)
            }
            12 => {
                let n = self.length(8)?;
                let mut values = Vec::with_capacity(n.min(4096));
                for _ in 0..n {
                    values.push(DataValue::Int(i64::from_be_bytes(self.number::<8>()?)));
                }
                DataValue::List(values)
            }
            other => {
                return Err(AnvilInspectError::MalformedNbt {
                    offset: self.pos.saturating_sub(1),
                    message: format!("unknown tag id {other}"),
                })
            }
        })
    }
}

/// Canonical wire bounds for the NBT codec, expressed in this module's caps:
/// `max_depth` matches [`NBT_DEPTH_CAP`] container levels and the
/// aggregate/list/array budgets are the document size cap — the same
/// fail-closed profile `world_interop`'s raw-NBT skipper uses.
fn nbt_wire_limits() -> NbtWireLimits {
    NbtWireLimits {
        max_depth: NBT_DEPTH_CAP,
        max_tags: NBT_SIZE_CAP,
        max_list_len: NBT_SIZE_CAP,
        max_array_bytes: NBT_SIZE_CAP,
    }
}

/// Map a canonical [`nbt_wire`] rejection onto this codec's error surface.
/// The skipper reports no byte offset, so structural failures land on
/// `MalformedNbt` carrying the wire diagnostic as the message; truncation
/// and depth keep their dedicated variants.
fn nbt_wire_validate(bytes: &[u8]) -> Result<(), AnvilInspectError> {
    nbt_wire::skip_named_nbt(bytes, nbt_wire_limits())
        .map(|_| ())
        .map_err(|error| match error {
            NbtWireError::UnexpectedEof { .. } => AnvilInspectError::TruncatedNbt {
                offset: bytes.len(),
            },
            NbtWireError::DepthExceeded { depth, .. } => AnvilInspectError::NbtTooDeep { depth },
            NbtWireError::InvalidTagId(tag) => AnvilInspectError::MalformedNbt {
                offset: 0,
                message: format!("invalid NBT tag id {tag}"),
            },
            other => AnvilInspectError::MalformedNbt {
                offset: 0,
                message: other.to_string(),
            },
        })
}

/// Decode one named-NBT document (`level.dat`, chunk NBT, `servers.dat`)
/// into its root name and a [`DataValue`] tree.
///
/// Fail-closed: the input may not exceed [`NBT_SIZE_CAP`], structural
/// bounds — tag ids, declared lengths vs remaining input, list/array caps
/// and container depth — are proven up front by the canonical
/// [`megu_core::nbt_wire`] skipper every other NBT consumer in the tree
/// shares, every byte after the root tag is an error, and strings must be
/// valid modified UTF-8 that maps onto real UTF-8.
///
/// Type projection: all four integer widths become [`DataValue::Int`],
/// floats become [`DataValue::Float`], the three array tags become `List`s
/// of `Int` (byte-array elements keep their signed `i8` value), `Compound`
/// becomes `Map`, and a bare `End` becomes `Null`.
pub fn decode_nbt_to_data_value(bytes: &[u8]) -> Result<(String, DataValue), AnvilInspectError> {
    if bytes.len() > NBT_SIZE_CAP {
        return Err(AnvilInspectError::NbtTooLarge {
            len: bytes.len(),
            cap: NBT_SIZE_CAP,
        });
    }
    // Canonical structural validation before any allocation: the wire skipper
    // proves the whole document's tag ids, lengths, caps and depth with the
    // same bounds every other NBT reader enforces. A `TAG_End` root keeps the
    // codec's historic ("", Null) answer and skips the wire pass.
    if bytes.first().copied() != Some(0) {
        nbt_wire_validate(bytes)?;
    }
    let mut reader = NbtReader {
        input: bytes,
        pos: 0,
    };
    let id = reader.number::<1>()?[0];
    let (name, value) = if id == 0 {
        (String::new(), DataValue::Null)
    } else {
        if id > 12 {
            return Err(AnvilInspectError::MalformedNbt {
                offset: 0,
                message: format!("unknown root tag id {id}"),
            });
        }
        let name = reader.string()?;
        (name, reader.tag(id, 0)?)
    };
    if reader.pos != bytes.len() {
        return Err(AnvilInspectError::TrailingNbtBytes {
            count: bytes.len() - reader.pos,
        });
    }
    Ok((name, value))
}

// ------------------------------------------------------- DataValue -> NBT

fn unrepresentable(message: impl Into<String>) -> AnvilInspectError {
    AnvilInspectError::UnrepresentableValue {
        message: message.into(),
    }
}

/// The NBT tag id a value is written as, when written on its own.
/// `DataValue::Int` picks `Int` inside `i32` range and `Long` outside it;
/// inside a list the chosen element width is applied uniformly instead.
fn nbt_tag_id(value: &DataValue) -> Result<u8, AnvilInspectError> {
    Ok(match value {
        DataValue::Null => return Err(unrepresentable("null has no NBT tag")),
        DataValue::Bool(_) => 1,
        DataValue::Int(v) => {
            if i32::try_from(*v).is_ok() {
                3
            } else {
                4
            }
        }
        DataValue::Float(_) => 6,
        DataValue::Str(_) => 8,
        DataValue::List(_) => 9,
        DataValue::Map(_) => 10,
    })
}

/// NBT lists are homogeneous. All-`Int` lists unify onto a single width
/// (`Int` when every element fits `i32`, else `Long`); every other list
/// must share one variant, and `Null` anywhere is unrepresentable.
fn list_element_type(items: &[DataValue]) -> Result<u8, AnvilInspectError> {
    let Some(first) = items.first() else {
        return Ok(0);
    };
    if items.iter().all(|v| matches!(v, DataValue::Int(_))) {
        let all_i32 = items
            .iter()
            .all(|v| v.as_int().is_some_and(|v| i32::try_from(v).is_ok()));
        return Ok(if all_i32 { 3 } else { 4 });
    }
    let kind = nbt_tag_id(first)?;
    for item in items {
        if nbt_tag_id(item)? != kind {
            return Err(unrepresentable(
                "heterogeneous list has no NBT element type",
            ));
        }
    }
    Ok(kind)
}

fn write_nbt_string(value: &str, out: &mut Vec<u8>) -> Result<(), AnvilInspectError> {
    let bytes = mutf8_encode(value);
    let len = u16::try_from(bytes.len())
        .map_err(|_| unrepresentable("string exceeds the 65535-byte MUTF-8 limit"))?;
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(&bytes);
    Ok(())
}

fn write_nbt_len(n: usize, out: &mut Vec<u8>) -> Result<(), AnvilInspectError> {
    out.extend_from_slice(
        &i32::try_from(n)
            .map_err(|_| unrepresentable("collection exceeds the NBT length range"))?
            .to_be_bytes(),
    );
    Ok(())
}

fn write_nbt_payload(
    value: &DataValue,
    out: &mut Vec<u8>,
    depth: usize,
) -> Result<(), AnvilInspectError> {
    match value {
        DataValue::Null => Err(unrepresentable("null has no NBT tag")),
        DataValue::Bool(b) => {
            out.push(u8::from(*b));
            Ok(())
        }
        DataValue::Int(v) => match i32::try_from(*v) {
            Ok(v) => {
                out.extend_from_slice(&v.to_be_bytes());
                Ok(())
            }
            Err(_) => {
                out.extend_from_slice(&v.to_be_bytes());
                Ok(())
            }
        },
        DataValue::Float(v) => {
            let bits = if v.is_nan() {
                0x7ff8_0000_0000_0000
            } else {
                v.to_bits()
            };
            out.extend_from_slice(&bits.to_be_bytes());
            Ok(())
        }
        DataValue::Str(s) => write_nbt_string(s, out),
        DataValue::List(items) => {
            if depth >= NBT_DEPTH_CAP {
                return Err(AnvilInspectError::NbtTooDeep { depth });
            }
            let kind = list_element_type(items)?;
            out.push(kind);
            write_nbt_len(items.len(), out)?;
            for item in items {
                match (kind, item) {
                    // Homogeneity is already proven; only the int width the
                    // list chose differs from the element's standalone pick.
                    (3, DataValue::Int(v)) => out.extend_from_slice(&(*v as i32).to_be_bytes()),
                    (4, DataValue::Int(v)) => out.extend_from_slice(&v.to_be_bytes()),
                    _ => write_nbt_payload(item, out, depth + 1)?,
                }
            }
            Ok(())
        }
        DataValue::Map(values) => {
            if depth >= NBT_DEPTH_CAP {
                return Err(AnvilInspectError::NbtTooDeep { depth });
            }
            for (key, item) in values {
                out.push(nbt_tag_id(item)?);
                write_nbt_string(key, out)?;
                write_nbt_payload(item, out, depth + 1)?;
            }
            out.push(0);
            Ok(())
        }
    }
}

/// Encode `val` as a named-NBT document under `root_name` — the inverse
/// projection of [`decode_nbt_to_data_value`], picking canonical NBT types
/// (`Bool` -> `Byte`, `Int` -> `Int`/`Long` by range, `Float` -> `Double`,
/// `Str` -> `String`, `Map` -> `Compound`, homogeneous `List` -> `List`).
///
/// Fails closed on what NBT cannot express: `Null` anywhere in the tree,
/// heterogeneous lists, strings past the 64 KiB MUTF-8 limit, and trees
/// deeper than [`NBT_DEPTH_CAP`]. `BTreeMap` iteration order makes the
/// output byte-deterministic for a given tree.
pub fn encode_data_value_to_nbt(
    root_name: &str,
    val: &DataValue,
) -> Result<Vec<u8>, AnvilInspectError> {
    let id = nbt_tag_id(val)?;
    let mut out = Vec::new();
    out.push(id);
    write_nbt_string(root_name, &mut out)?;
    write_nbt_payload(val, &mut out, 0)?;
    if out.len() > NBT_SIZE_CAP {
        return Err(AnvilInspectError::NbtTooLarge {
            len: out.len(),
            cap: NBT_SIZE_CAP,
        });
    }
    Ok(out)
}

// ---------------------------------------------------------- .mca inspection

/// How one populated region slot's chunk payload is compressed, per the
/// byte after the record length.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum McaCompression {
    /// `1` — gzip. Historically the vanilla default.
    Gzip,
    /// `2` — zlib. The long-standing vanilla default.
    Zlib,
    /// `3` — no compression wrapper.
    Uncompressed,
    /// `4` — LZ4 (vanilla-supported since 1.20.5 behind a flag).
    Lz4,
    /// Any other byte — not a compression vanilla writes.
    Unknown(u8),
}

impl McaCompression {
    fn from_id(id: u8) -> Self {
        match id {
            1 => Self::Gzip,
            2 => Self::Zlib,
            3 => Self::Uncompressed,
            4 => Self::Lz4,
            other => Self::Unknown(other),
        }
    }

    /// The compression byte as stored in the chunk record.
    pub fn id(self) -> u8 {
        match self {
            Self::Gzip => 1,
            Self::Zlib => 2,
            Self::Uncompressed => 3,
            Self::Lz4 => 4,
            Self::Unknown(v) => v,
        }
    }

    /// Short stable label for JSON and human output.
    pub fn label(self) -> &'static str {
        match self {
            Self::Gzip => "gzip",
            Self::Zlib => "zlib",
            Self::Uncompressed => "uncompressed",
            Self::Lz4 => "lz4",
            Self::Unknown(_) => "unknown",
        }
    }
}

/// One populated slot of a region header.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct McaChunkInfo {
    /// Header slot index `0..1024`; local chunk x is `slot % 32`, z is
    /// `slot / 32`.
    pub slot: usize,
    /// First sector the chunk record occupies (never inside the header).
    pub sector_offset: u32,
    /// Sectors the record spans.
    pub sector_count: u8,
    /// The slot's last-modified timestamp, seconds since the epoch —
    /// reported verbatim; the inspector never reads the clock.
    pub timestamp: u32,
    /// The record's declared byte length, including the compression byte.
    pub declared_len: u32,
    /// The compression byte, classified.
    pub compression: McaCompression,
}

/// What [`inspect_mca_header`] found in one region file.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct McaRegionSummary {
    /// Size of the file the header was checked against.
    pub file_len: usize,
    /// Slots whose location entry is populated and whose record lies inside
    /// the file.
    pub populated_chunks: usize,
    /// Per-method populated-chunk counts.
    pub gzip_chunks: usize,
    /// Per-method populated-chunk counts.
    pub zlib_chunks: usize,
    /// Per-method populated-chunk counts.
    pub uncompressed_chunks: usize,
    /// Per-method populated-chunk counts.
    pub lz4_chunks: usize,
    /// Chunks whose compression byte matches no vanilla method.
    pub unknown_compression_chunks: usize,
    /// The populated slots, in header order.
    pub chunks: Vec<McaChunkInfo>,
}

/// Inspect the 8,192-byte header of one Anvil `r.<x>.<z>.mca` file carried
/// in `bytes` (the whole file — sector bounds are checked against
/// `bytes.len()`).
///
/// Populated slots are counted and their compression bytes classified.
/// Fail-closed: a file shorter than the header, a location entry that is
/// half-populated or lands inside the header sectors, a declared sector
/// run reaching past EOF, or a record length that cannot fit its sectors
/// are all errors — never silently skipped.
pub fn inspect_mca_header(bytes: &[u8]) -> Result<McaRegionSummary, AnvilInspectError> {
    if bytes.len() < MCA_HEADER_BYTES {
        return Err(AnvilInspectError::TruncatedRegionHeader { len: bytes.len() });
    }
    let (locations, timestamps) = bytes.split_at(MCA_SECTOR_BYTES);
    let mut summary = McaRegionSummary {
        file_len: bytes.len(),
        ..McaRegionSummary::default()
    };
    for slot in 0..MCA_SLOT_COUNT {
        let entry = &locations[slot * 4..slot * 4 + 4];
        let offset = (u32::from(entry[0]) << 16) | (u32::from(entry[1]) << 8) | u32::from(entry[2]);
        let sectors = entry[3];
        if offset == 0 && sectors == 0 {
            continue;
        }
        // A real slot needs both halves of the entry and may not point into
        // the two header sectors.
        if offset < MCA_FIRST_DATA_SECTOR || sectors == 0 {
            return Err(AnvilInspectError::InvalidRegionLocation {
                slot,
                offset,
                sectors,
            });
        }
        // Sector indices are 24-bit, so `u64` math keeps every product exact.
        let start = u64::from(offset) * MCA_SECTOR_BYTES as u64;
        let end = u64::from(offset + u32::from(sectors)) * MCA_SECTOR_BYTES as u64;
        if end > bytes.len() as u64 || start + 5 > bytes.len() as u64 {
            return Err(AnvilInspectError::SectorRunOutOfBounds {
                slot,
                offset,
                sectors,
                file_len: bytes.len(),
            });
        }
        let start = start as usize;
        let declared_len = u32::from_be_bytes(bytes[start..start + 4].try_into().unwrap());
        // The record is 4 length bytes + `declared_len` (1 compression byte +
        // payload), and must live inside the declared sector run.
        if declared_len < 2
            || 4 + u64::from(declared_len) > u64::from(sectors) * MCA_SECTOR_BYTES as u64
        {
            return Err(AnvilInspectError::InvalidChunkRecordLength {
                slot,
                length: declared_len,
                sectors,
            });
        }
        let compression = McaCompression::from_id(bytes[start + 4]);
        match compression {
            McaCompression::Gzip => summary.gzip_chunks += 1,
            McaCompression::Zlib => summary.zlib_chunks += 1,
            McaCompression::Uncompressed => summary.uncompressed_chunks += 1,
            McaCompression::Lz4 => summary.lz4_chunks += 1,
            McaCompression::Unknown(_) => summary.unknown_compression_chunks += 1,
        }
        summary.chunks.push(McaChunkInfo {
            slot,
            sector_offset: offset,
            sector_count: sectors,
            timestamp: u32::from_be_bytes(timestamps[slot * 4..slot * 4 + 4].try_into().unwrap()),
            declared_len,
            compression,
        });
    }
    summary.populated_chunks = summary.chunks.len();
    Ok(summary)
}

// ----------------------------------------------------------- world inspect

/// Whether a `level.dat` file was gzip-wrapped on disk.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LevelDatCompression {
    /// RFC 1952 gzip — how vanilla writes `level.dat`.
    Gzip,
    /// Raw uncompressed NBT — accepted for hand-built or exported saves.
    None,
}

impl LevelDatCompression {
    /// Short stable label for JSON and human output.
    pub fn label(self) -> &'static str {
        match self {
            Self::Gzip => "gzip",
            Self::None => "uncompressed",
        }
    }
}

/// How the world relates to the two targets megu serves.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AnvilCompatibility {
    /// `DataVersion` pins exactly one supported target.
    Supported(TargetVersion),
    /// `DataVersion` is newer than the newest target (`26.3`, 5023): the
    /// save was written by a release this build has no schema for.
    NewerThanTargets,
    /// `DataVersion` sits strictly between the two pinned targets: a real
    /// vanilla version, just not one this build serves.
    BetweenTargets,
    /// `DataVersion` is older than the oldest target (`1.21.1`, 3955).
    OlderThanTargets,
}

impl AnvilCompatibility {
    /// Classify a `DataVersion` against the pinned target registry.
    pub fn classify(data_version: i32) -> Self {
        if let Some(target) = TargetVersion::from_data_version(data_version) {
            Self::Supported(target)
        } else if data_version > DV_26_3 {
            Self::NewerThanTargets
        } else if data_version < DV_1_21_1 {
            Self::OlderThanTargets
        } else {
            Self::BetweenTargets
        }
    }

    /// Short stable label for JSON output.
    pub fn label(self) -> &'static str {
        match self {
            Self::Supported(_) => "supported",
            Self::NewerThanTargets => "newer_than_targets",
            Self::BetweenTargets => "between_targets",
            Self::OlderThanTargets => "older_than_targets",
        }
    }

    /// The resolved target when [`Self::Supported`].
    pub fn target(self) -> Option<TargetVersion> {
        match self {
            Self::Supported(t) => Some(t),
            _ => None,
        }
    }

    /// One plain-English sentence about what the version means for megu.
    fn describe(self, data_version: i32) -> String {
        match self {
            Self::Supported(t) => {
                format!("a {t} world, a version this server build supports")
            }
            Self::NewerThanTargets => format!(
                "data version {data_version} is newer than 26.3 ({DV_26_3}); this build cannot read its schema"
            ),
            Self::BetweenTargets => format!(
                "data version {data_version} belongs to a release between 1.21.1 ({DV_1_21_1}) and 26.3 ({DV_26_3}); this build does not serve it"
            ),
            Self::OlderThanTargets => format!(
                "data version {data_version} is older than 1.21.1 ({DV_1_21_1}); it needs upgrading before this build serves it"
            ),
        }
    }
}

/// What [`inspect_level_dat`] extracted from a `level.dat` payload.
#[derive(Clone, Debug, PartialEq)]
pub struct LevelDatSummary {
    /// How the bytes were wrapped on disk.
    pub compression: LevelDatCompression,
    /// Size of the file as read.
    pub file_len: usize,
    /// Size of the NBT document after unwrapping.
    pub nbt_len: usize,
    /// The NBT root name (vanilla writes an empty one for `level.dat`).
    pub root_name: String,
    /// `Data.LevelName`, when present.
    pub level_name: Option<String>,
    /// `Data.Version.Name` — the release that last saved the world.
    pub version_name: Option<String>,
    /// `Data.DataVersion` — required; a `level.dat` without one is refused.
    pub data_version: i32,
    /// `Data.RandomSeed`, falling back to `Data.WorldGenSettings.seed`.
    pub random_seed: Option<i64>,
    /// Version classification against the two megu targets.
    pub compatibility: AnvilCompatibility,
    /// `Data.DataPacks.Enabled` entries (string items only).
    pub enabled_datapacks: Vec<String>,
    /// `Data.DataPacks.Disabled` entries (string items only).
    pub disabled_datapacks: Vec<String>,
    /// The fully decoded document — the bridge output a caller can run
    /// through [`crate::datafix::DataFixer`] directly.
    pub root: DataValue,
}

fn str_list_field(map: &BTreeMap<String, DataValue>, key: &str) -> Vec<String> {
    match map.get(key) {
        Some(DataValue::List(items)) => items
            .iter()
            .filter_map(DataValue::as_str)
            .map(str::to_string)
            .collect(),
        _ => Vec::new(),
    }
}

/// Inspect raw `level.dat` bytes — gzip-wrapped (the vanilla on-disk form)
/// or a bare uncompressed NBT document.
///
/// Required structure: a `Data` compound carrying an integer `DataVersion`;
/// a `level.dat` without either is corrupt in every version that writes
/// Anvil and is refused. Everything else (`LevelName`, `Version.Name`,
/// seed, `DataPacks`) is reported when present and typed as expected.
pub fn inspect_level_dat(bytes: &[u8]) -> Result<LevelDatSummary, AnvilInspectError> {
    let inflated;
    let (nbt, compression) = if bytes.starts_with(&[0x1f, 0x8b]) {
        inflated = gzip_decompress(bytes, NBT_SIZE_CAP)?;
        (inflated.as_slice(), LevelDatCompression::Gzip)
    } else {
        (bytes, LevelDatCompression::None)
    };
    let (root_name, root) = decode_nbt_to_data_value(nbt)?;
    let data = root
        .as_map()
        .and_then(|m| m.get("Data"))
        .and_then(DataValue::as_map)
        .ok_or(AnvilInspectError::MissingLevelField { field: "Data" })?;
    let data_version = data
        .get("DataVersion")
        .and_then(DataValue::as_int)
        .and_then(|v| i32::try_from(v).ok())
        .ok_or(AnvilInspectError::MissingLevelField {
            field: "Data.DataVersion",
        })?;
    let version_name = data
        .get("Version")
        .and_then(DataValue::as_map)
        .and_then(|v| v.get("Name"))
        .and_then(DataValue::as_str)
        .map(str::to_string);
    let level_name = data
        .get("LevelName")
        .and_then(DataValue::as_str)
        .map(str::to_string);
    let random_seed = data
        .get("RandomSeed")
        .and_then(DataValue::as_int)
        .or_else(|| {
            data.get("WorldGenSettings")
                .and_then(DataValue::as_map)
                .and_then(|m| m.get("seed"))
                .and_then(DataValue::as_int)
        });
    let (enabled_datapacks, disabled_datapacks) =
        match data.get("DataPacks").and_then(DataValue::as_map) {
            Some(packs) => (
                str_list_field(packs, "Enabled"),
                str_list_field(packs, "Disabled"),
            ),
            None => (Vec::new(), Vec::new()),
        };
    Ok(LevelDatSummary {
        compression,
        file_len: bytes.len(),
        nbt_len: nbt.len(),
        root_name,
        level_name,
        version_name,
        data_version,
        random_seed,
        compatibility: AnvilCompatibility::classify(data_version),
        enabled_datapacks,
        disabled_datapacks,
        root,
    })
}

// --------------------------------------------------------- level.dat migrate

/// Migrate a vanilla `level.dat` between the two supported releases.
///
/// `bytes` may be gzip-wrapped (the vanilla on-disk form) or a bare NBT
/// document; the output is always a fresh RFC 1952 gzip member wrapping the
/// re-encoded document, with the NBT root name preserved verbatim (vanilla
/// writes an empty one). `Data.DataVersion` is required and authoritative —
/// [`inspect_level_dat`] reads it fail-closed, and it is mirrored at the
/// document root for the fix run exactly the way
/// [`crate::migrate::migrate_json_document`] presents a `level.dat` export.
///
/// Toward `26.3` the document runs [`DataFixer::v1_21_1_to_26_3`]; toward
/// `1.21.1` it runs the explicit
/// [`DataFixer::v26_3_to_1_21_1_downgrade`] pipeline, which any `file_dv`
/// above `3955` reaches only when `allow_downgrade` is set — a document
/// already at `3955` passes through as a clean no-op without the flag.
/// Either way the output is re-encoded and verified, so an already-at-target
/// input still produces the canonical gzip form of itself.
///
/// Fail-closed end to end: a missing or non-integer `Data.DataVersion`, a
/// document newer than the pipeline's ceiling, and a refused downgrade are
/// errors — and the encoded output is itself decoded through
/// [`inspect_level_dat`] before it is returned, so the bytes can only ever
/// be a `level.dat` this build reads back.
pub fn migrate_anvil_level_dat(
    bytes: &[u8],
    to: TargetVersion,
    allow_downgrade: bool,
) -> Result<(Vec<u8>, DataFixReport), AnvilInspectError> {
    let summary = inspect_level_dat(bytes)?;
    let file_dv = summary.data_version;
    let fixer = match to {
        TargetVersion::V26_3 => DataFixer::v1_21_1_to_26_3(),
        TargetVersion::V1_21_1 => {
            if file_dv > DV_1_21_1 && !allow_downgrade {
                return Err(AnvilInspectError::DowngradeNotAllowed {
                    from_dv: file_dv,
                    to_dv: DV_1_21_1,
                });
            }
            DataFixer::v26_3_to_1_21_1_downgrade()
        }
    };
    // The fixer reads `DataVersion` from the document root; a real level.dat
    // keeps it inside `Data`, so mirror it at the root for the run and drop
    // the mirror afterward unless the input already carried one.
    let mut root = summary.root;
    let map = root
        .as_map_mut()
        .ok_or(AnvilInspectError::MissingLevelField { field: "Data" })?;
    let had_root_dv = map.contains_key("DataVersion");
    map.insert("DataVersion".to_string(), DataValue::Int(i64::from(file_dv)));
    let report = fixer.fix(&mut root).map_err(AnvilInspectError::Fix)?;
    if !had_root_dv {
        if let Some(map) = root.as_map_mut() {
            map.remove("DataVersion");
        }
    }
    let out = gzip_encode_stored(&encode_data_value_to_nbt(&summary.root_name, &root)?);
    // Round-trip check on the bytes about to leave: `inspect_level_dat`
    // verifies the gzip trailer and requires `Data.DataVersion`, so the
    // output can only ever be a document this build reads back. A failure
    // here is an internal invariant violation, not bad user data.
    inspect_level_dat(&out).map_err(|e| AnvilInspectError::OutputInvalid {
        message: e.to_string(),
    })?;
    Ok((out, report))
}

/// `r.<rx>.<rz>.mca` -> `(rx, rz)`; names in any other shape are ignored by
/// the scan the same way vanilla ignores stray files in `region/`.
fn region_file_coords(file_name: &str) -> Option<(i32, i32)> {
    let stem = file_name.strip_suffix(".mca")?;
    let (r, coords) = stem.split_once('.')?;
    if r != "r" {
        return None;
    }
    let (x, z) = coords.split_once('.')?;
    if z.contains('.') {
        return None;
    }
    Some((x.parse().ok()?, z.parse().ok()?))
}

/// One region file's inspection result.
#[derive(Clone, Debug, PartialEq)]
pub struct AnvilRegionFileReport {
    /// File name as it appears in the region directory (`r.0.-1.mca`).
    pub file_name: String,
    /// Region grid coordinates parsed from the file name.
    pub region_x: i32,
    /// Region grid coordinates parsed from the file name.
    pub region_z: i32,
    /// The header inspection.
    pub summary: McaRegionSummary,
}

/// The `region/` directory of one dimension.
#[derive(Clone, Debug, PartialEq)]
pub struct AnvilDimensionReport {
    /// Which dimension the directory holds: `overworld`, `nether` or `end`.
    pub dimension: String,
    /// Directory path relative to the world root (`region`,
    /// `DIM-1/region`, `DIM1/region`).
    pub region_dir: String,
    /// One entry per `r.<x>.<z>.mca` file, sorted by file name.
    pub regions: Vec<AnvilRegionFileReport>,
    /// Populated chunk slots summed over the dimension's region files.
    pub populated_chunks: usize,
}

/// Everything [`inspect_anvil_world_dir`] learned about one world folder.
#[derive(Clone, Debug, PartialEq)]
pub struct AnvilWorldReport {
    /// The directory that was inspected.
    pub world_dir: PathBuf,
    /// `level.dat` contents.
    pub level_dat: LevelDatSummary,
    /// Per-dimension region scans, in overworld/nether/end order.
    pub dimensions: Vec<AnvilDimensionReport>,
    /// Total `r.*.*.mca` files across every scanned dimension.
    pub region_file_count: usize,
    /// Total populated chunk slots across every scanned region file.
    pub populated_chunk_count: usize,
}

impl AnvilWorldReport {
    /// The report as a deterministic sorted-key JSON document.
    pub fn to_json(&self) -> String {
        to_data_json(&self.to_data_value())
    }

    /// The report as a [`DataValue`] tree — the same tree [`Self::to_json`]
    /// serializes, exposed so callers can walk it without reparsing JSON.
    pub fn to_data_value(&self) -> DataValue {
        fn map(pairs: Vec<(&str, DataValue)>) -> DataValue {
            DataValue::Map(
                pairs
                    .into_iter()
                    .map(|(k, v)| (k.to_string(), v))
                    .collect::<BTreeMap<_, _>>(),
            )
        }
        fn int(v: usize) -> DataValue {
            DataValue::Int(v as i64)
        }
        fn opt_int(v: Option<i64>) -> DataValue {
            v.map_or(DataValue::Null, DataValue::Int)
        }
        fn opt_str(v: &Option<String>) -> DataValue {
            v.clone().map_or(DataValue::Null, DataValue::Str)
        }
        fn str_list(values: &[String]) -> DataValue {
            DataValue::List(values.iter().cloned().map(DataValue::Str).collect())
        }
        let ld = &self.level_dat;
        let level_dat = map(vec![
            (
                "compatibility",
                DataValue::Str(ld.compatibility.label().to_string()),
            ),
            ("compressed_bytes", int(ld.file_len)),
            (
                "compression",
                DataValue::Str(ld.compression.label().to_string()),
            ),
            ("data_version", DataValue::Int(i64::from(ld.data_version))),
            (
                "datapacks",
                map(vec![
                    ("disabled", str_list(&ld.disabled_datapacks)),
                    ("enabled", str_list(&ld.enabled_datapacks)),
                ]),
            ),
            (
                "detected_target",
                ld.compatibility
                    .target()
                    .map_or(DataValue::Null, |t| DataValue::Str(t.id().to_string())),
            ),
            ("level_name", opt_str(&ld.level_name)),
            ("nbt_bytes", int(ld.nbt_len)),
            ("random_seed", opt_int(ld.random_seed)),
            ("root_name", DataValue::Str(ld.root_name.clone())),
            ("version_name", opt_str(&ld.version_name)),
        ]);
        let dimensions = DataValue::List(
            self.dimensions
                .iter()
                .map(|dim| {
                    map(vec![
                        ("dimension", DataValue::Str(dim.dimension.clone())),
                        ("populated_chunks", int(dim.populated_chunks)),
                        ("region_dir", DataValue::Str(dim.region_dir.clone())),
                        ("region_file_count", int(dim.regions.len())),
                        (
                            "regions",
                            DataValue::List(
                                dim.regions
                                    .iter()
                                    .map(|region| {
                                        let s = &region.summary;
                                        map(vec![
                                            (
                                                "compression",
                                                map(vec![
                                                    ("gzip", int(s.gzip_chunks)),
                                                    ("lz4", int(s.lz4_chunks)),
                                                    ("uncompressed", int(s.uncompressed_chunks)),
                                                    ("unknown", int(s.unknown_compression_chunks)),
                                                    ("zlib", int(s.zlib_chunks)),
                                                ]),
                                            ),
                                            ("file", DataValue::Str(region.file_name.clone())),
                                            ("file_bytes", int(s.file_len)),
                                            ("populated_chunks", int(s.populated_chunks)),
                                            (
                                                "region_x",
                                                DataValue::Int(i64::from(region.region_x)),
                                            ),
                                            (
                                                "region_z",
                                                DataValue::Int(i64::from(region.region_z)),
                                            ),
                                        ])
                                    })
                                    .collect(),
                            ),
                        ),
                    ])
                })
                .collect(),
        );
        map(vec![
            (
                "compatibility",
                DataValue::Str(ld.compatibility.label().to_string()),
            ),
            ("data_version", DataValue::Int(i64::from(ld.data_version))),
            (
                "detected_target",
                ld.compatibility
                    .target()
                    .map_or(DataValue::Null, |t| DataValue::Str(t.id().to_string())),
            ),
            ("dimensions", dimensions),
            ("level_dat", level_dat),
            ("level_name", opt_str(&ld.level_name)),
            ("populated_chunks", int(self.populated_chunk_count)),
            ("region_file_count", int(self.region_file_count)),
            ("version_name", opt_str(&ld.version_name)),
            (
                "world_dir",
                DataValue::Str(self.world_dir.display().to_string()),
            ),
        ])
    }

    /// A plain-English summary: no codec names, no hex — what the world is,
    /// which Minecraft saved it, and how much region data it carries.
    pub fn to_human(&self) -> String {
        let ld = &self.level_dat;
        let mut out = String::new();
        let name = ld.level_name.as_deref().unwrap_or("(unnamed world)");
        let version = match ld.compatibility {
            AnvilCompatibility::Supported(t) => format!("minecraft {t}"),
            _ => "minecraft release unknown to this build".to_string(),
        };
        out.push_str(&format!(
            "{}: {version} world \"{name}\" (data version {})\n",
            self.world_dir.display(),
            ld.data_version
        ));
        let wrapping = match ld.compression {
            LevelDatCompression::Gzip => "gzip-compressed",
            LevelDatCompression::None => "uncompressed",
        };
        out.push_str(&format!(
            "  level.dat: {wrapping} data, {} bytes on disk",
            ld.file_len
        ));
        if let Some(seed) = ld.random_seed {
            out.push_str(&format!(", seed {seed}"));
        }
        if let Some(vn) = &ld.version_name {
            out.push_str(&format!(", saved by version \"{vn}\""));
        }
        out.push('\n');
        out.push_str(&format!(
            "  status: {}\n",
            ld.compatibility.describe(ld.data_version)
        ));
        if !ld.enabled_datapacks.is_empty() || !ld.disabled_datapacks.is_empty() {
            out.push_str(&format!(
                "  data packs: {} enabled ({})",
                ld.enabled_datapacks.len(),
                ld.enabled_datapacks.join(", ")
            ));
            if !ld.disabled_datapacks.is_empty() {
                out.push_str(&format!(
                    "; {} disabled ({})",
                    ld.disabled_datapacks.len(),
                    ld.disabled_datapacks.join(", ")
                ));
            }
            out.push('\n');
        }
        if self.region_file_count == 0 {
            out.push_str("  no region files: no chunks have been generated yet\n");
        } else {
            out.push_str(&format!(
                "  {} region file(s), {} populated chunk(s) total\n",
                self.region_file_count, self.populated_chunk_count
            ));
            for dim in &self.dimensions {
                out.push_str(&format!(
                    "  {} ({}/): {} region file(s), {} populated chunk(s)\n",
                    dim.dimension,
                    dim.region_dir,
                    dim.regions.len(),
                    dim.populated_chunks
                ));
            }
        }
        out
    }
}

/// Inspect a vanilla Anvil world directory: `level.dat` plus the region
/// files of the three vanilla dimensions.
///
/// Scanned: `<dir>/region/` (overworld), `<dir>/DIM-1/region/` (nether) and
/// `<dir>/DIM1/region/` (end). Only files named `r.<x>.<z>.mca` are read —
/// stray files are ignored the way vanilla ignores them — and a region
/// directory that does not exist simply contributes nothing.
///
/// Fail-closed: a missing or unreadable `level.dat`, an undecodable
/// document, a missing `Data.DataVersion`, or a region file whose header
/// inspection fails are errors, never silently skipped entries.
pub fn inspect_anvil_world_dir(dir: &Path) -> Result<AnvilWorldReport, AnvilInspectError> {
    let level_path = dir.join("level.dat");
    if !level_path.is_file() {
        return Err(AnvilInspectError::MissingLevelDat { path: level_path });
    }
    let level_bytes = std::fs::read(&level_path).map_err(|e| io_error(&level_path, e))?;
    let level_dat =
        inspect_level_dat(&level_bytes).map_err(|e| AnvilInspectError::InvalidWorldFile {
            path: level_path.clone(),
            message: e.to_string(),
        })?;

    let mut dim_dirs: Vec<(String, String)> = vec![
        ("overworld".to_string(), "region".to_string()),
        ("nether".to_string(), "DIM-1/region".to_string()),
        ("end".to_string(), "DIM1/region".to_string()),
    ];
    let custom_dims_root = dir.join("dimensions");
    if custom_dims_root.is_dir() {
        let mut custom_dims = Vec::new();
        collect_custom_dimension_region_dirs(&custom_dims_root, &mut Vec::new(), 0, &mut custom_dims)?;
        custom_dims.sort();
        dim_dirs.extend(custom_dims);
    }
    let mut dimensions = Vec::new();
    let mut region_file_count = 0usize;
    let mut populated_chunk_count = 0usize;
    for (dimension, rel) in dim_dirs {
        let region_dir = dir.join(&rel);
        if !region_dir.is_dir() {
            continue;
        }
        let mut names: Vec<String> = Vec::new();
        let entries = std::fs::read_dir(&region_dir).map_err(|e| io_error(&region_dir, e))?;
        for entry in entries {
            let entry = entry.map_err(|e| io_error(&region_dir, e))?;
            if let Some(name) = entry.file_name().to_str() {
                if region_file_coords(name).is_some() && entry.path().is_file() {
                    names.push(name.to_string());
                }
            }
        }
        names.sort();
        let mut regions = Vec::new();
        let mut populated_chunks = 0usize;
        for file_name in names {
            let (region_x, region_z) = region_file_coords(&file_name).unwrap();
            let path = region_dir.join(&file_name);
            let bytes = std::fs::read(&path).map_err(|e| io_error(&path, e))?;
            let summary =
                inspect_mca_header(&bytes).map_err(|e| AnvilInspectError::InvalidWorldFile {
                    path: path.clone(),
                    message: e.to_string(),
                })?;
            populated_chunks += summary.populated_chunks;
            regions.push(AnvilRegionFileReport {
                file_name,
                region_x,
                region_z,
                summary,
            });
        }
        if regions.is_empty() {
            continue;
        }
        region_file_count += regions.len();
        populated_chunk_count += populated_chunks;
        dimensions.push(AnvilDimensionReport {
            dimension,
            region_dir: rel,
            regions,
            populated_chunks,
        });
    }
    Ok(AnvilWorldReport {
        world_dir: dir.to_path_buf(),
        level_dat,
        dimensions,
        region_file_count,
        populated_chunk_count,
    })
}

fn collect_custom_dimension_region_dirs(
    cur: &Path,
    segs: &mut Vec<String>,
    depth: usize,
    out: &mut Vec<(String, String)>,
) -> Result<(), AnvilInspectError> {
    const MAX_DIM_DEPTH: usize = 6;
    const MAX_CUSTOM_DIMS: usize = 256;
    if depth >= MAX_DIM_DEPTH || out.len() >= MAX_CUSTOM_DIMS {
        return Ok(());
    }
    let entries = std::fs::read_dir(cur).map_err(|e| io_error(cur, e))?;
    let mut names = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|e| io_error(cur, e))?;
        let path = entry.path();
        let meta = std::fs::symlink_metadata(&path).map_err(|e| io_error(&path, e))?;
        if meta.file_type().is_symlink() || !meta.is_dir() {
            continue;
        }
        if let Some(name) = entry.file_name().to_str() {
            if !name.is_empty() && name != "." && name != ".." {
                names.push((name.to_string(), path));
            }
        }
    }
    names.sort_by(|a, b| a.0.cmp(&b.0));
    for (name, path) in names {
        if out.len() >= MAX_CUSTOM_DIMS {
            break;
        }
        if name == "region" && segs.len() >= 2 {
            let dim_id = format!("{}:{}", segs[0], segs[1..].join("/"));
            let rel = format!("dimensions/{}/region", segs.join("/"));
            out.push((dim_id, rel));
            continue;
        }
        if matches!(name.as_str(), "region" | "entities" | "poi" | "data") {
            continue;
        }
        segs.push(name);
        collect_custom_dimension_region_dirs(&path, segs, depth + 1, out)?;
        segs.pop();
    }
    Ok(())
}

// ------------------------------------------------------------------- tests

#[cfg(test)]
mod tests {
    use super::*;
    use crate::datafix::DataFixer;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn map(pairs: Vec<(&str, DataValue)>) -> DataValue {
        DataValue::Map(
            pairs
                .into_iter()
                .map(|(k, v)| (k.to_string(), v))
                .collect::<BTreeMap<_, _>>(),
        )
    }

    /// A per-test scratch dir under the system temp dir (TMPDIR-honoring).
    static DIR_COUNTER: AtomicU64 = AtomicU64::new(0);
    struct TestDir(PathBuf);
    impl TestDir {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "megu-anvil-inspect-{}-{}-{tag}",
                std::process::id(),
                DIR_COUNTER.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
        fn join(&self, rel: &str) -> PathBuf {
            self.0.join(rel)
        }
    }
    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// Hand-rolled named-NBT writer for byte-level fixtures: emits `id`,
    /// the MUTF-8 name, then `payload` verbatim.
    fn named(id: u8, name: &str, payload: &[u8]) -> Vec<u8> {
        let mut out = vec![id];
        out.extend_from_slice(&(name.len() as u16).to_be_bytes());
        out.extend_from_slice(name.as_bytes());
        out.extend_from_slice(payload);
        out
    }

    // ------------------------------------------------------------- NBT in

    #[test]
    fn decode_covers_every_vanilla_tag() {
        // Byte(1) Short(2) Int(3) Long(4) Float(5) Double(6) inside a compound.
        let mut payload = Vec::new();
        let mut push = |id: u8, key: &str, body: &[u8]| {
            payload.push(id);
            payload.extend_from_slice(&(key.len() as u16).to_be_bytes());
            payload.extend_from_slice(key.as_bytes());
            payload.extend_from_slice(body);
        };
        push(1, "b", &[0xfe]); // Byte -2
        push(2, "s", &(-300i16).to_be_bytes());
        push(3, "i", &(-70_000i32).to_be_bytes());
        push(4, "l", &(-5_000_000_000i64).to_be_bytes());
        push(5, "f", &1.5f32.to_be_bytes());
        push(6, "d", &(-2.25f64).to_be_bytes());
        push(7, "ba", &[0, 0, 0, 3, 0xff, 0x80, 0x7f]); // ByteArray [-1,-128,127]
        push(8, "str", &[0, 5, b'h', b'e', b'l', b'l', b'o']);
        // List of two Shorts.
        let mut list = vec![2u8];
        list.extend_from_slice(&2i32.to_be_bytes());
        list.extend_from_slice(&7i16.to_be_bytes());
        list.extend_from_slice(&8i16.to_be_bytes());
        push(9, "list", &list);
        // Nested compound {inner: Byte 1}.
        let inner = vec![1u8, 0, 5, b'i', b'n', b'n', b'e', b'r', 1, 0];
        push(10, "nest", &inner);
        // IntArray [1, -1], LongArray [i64::MAX].
        let mut ints = Vec::new();
        ints.extend_from_slice(&2i32.to_be_bytes());
        ints.extend_from_slice(&1i32.to_be_bytes());
        ints.extend_from_slice(&(-1i32).to_be_bytes());
        push(11, "ints", &ints);
        let mut longs = Vec::new();
        longs.extend_from_slice(&1i32.to_be_bytes());
        longs.extend_from_slice(&i64::MAX.to_be_bytes());
        push(12, "longs", &longs);
        payload.push(0); // compound end

        let doc = named(10, "root", &payload);
        let (name, value) = decode_nbt_to_data_value(&doc).unwrap();
        assert_eq!(name, "root");
        let m = value.as_map().unwrap();
        assert_eq!(m["b"], DataValue::Int(-2));
        assert_eq!(m["s"], DataValue::Int(-300));
        assert_eq!(m["i"], DataValue::Int(-70_000));
        assert_eq!(m["l"], DataValue::Int(-5_000_000_000));
        assert_eq!(m["f"], DataValue::Float(1.5));
        assert_eq!(m["d"], DataValue::Float(-2.25));
        assert_eq!(
            m["ba"],
            DataValue::List(vec![
                DataValue::Int(-1),
                DataValue::Int(-128),
                DataValue::Int(127)
            ])
        );
        assert_eq!(m["str"], DataValue::Str("hello".to_string()));
        assert_eq!(
            m["list"],
            DataValue::List(vec![DataValue::Int(7), DataValue::Int(8)])
        );
        assert_eq!(m["nest"].as_map().unwrap()["inner"], DataValue::Int(1));
        assert_eq!(
            m["ints"],
            DataValue::List(vec![DataValue::Int(1), DataValue::Int(-1)])
        );
        assert_eq!(m["longs"], DataValue::List(vec![DataValue::Int(i64::MAX)]));
    }

    #[test]
    fn roundtrip_all_data_value_shapes() {
        // Identity roundtrip at the DataValue level for every shape that is
        // its own canonical NBT form. Bool is covered separately: it writes
        // NBT Byte and decodes back as Int — the lossy direction of the
        // projection.
        let doc = map(vec![
            ("byte_int", DataValue::Int(-5)),
            ("int", DataValue::Int(-70_000)),
            ("long", DataValue::Int(i64::MAX)),
            ("float", DataValue::Float(-2.25)),
            ("str", DataValue::Str("héllo".to_string())),
            ("empty_list", DataValue::List(vec![])),
            (
                "ints",
                DataValue::List(vec![
                    DataValue::Int(1),
                    DataValue::Int(2),
                    DataValue::Int(3),
                ]),
            ),
            (
                "mixed_width_ints",
                DataValue::List(vec![DataValue::Int(1), DataValue::Int(i64::MAX)]),
            ),
            (
                "strs",
                DataValue::List(vec![
                    DataValue::Str("a".to_string()),
                    DataValue::Str("b".to_string()),
                ]),
            ),
            (
                "floats",
                DataValue::List(vec![DataValue::Float(0.5), DataValue::Float(-7.0)]),
            ),
            (
                "maps",
                DataValue::List(vec![map(vec![("k", DataValue::Int(9))])]),
            ),
            (
                "lists",
                DataValue::List(vec![DataValue::List(vec![DataValue::Int(4)])]),
            ),
            (
                "nest",
                map(vec![
                    ("inner", DataValue::Str("deep".to_string())),
                    ("n", DataValue::Int(1)),
                ]),
            ),
        ]);
        let bytes = encode_data_value_to_nbt("level", &doc).unwrap();
        let (name, back) = decode_nbt_to_data_value(&bytes).unwrap();
        assert_eq!(name, "level");
        assert_eq!(back, doc);
        // Encoding is byte-deterministic for a given tree.
        assert_eq!(encode_data_value_to_nbt("level", &doc).unwrap(), bytes);
    }

    #[test]
    fn bool_writes_nbt_byte_and_decodes_as_int() {
        // DataValue::Bool is the fixer's shape for boolean flags; vanilla
        // writes them as NBT Byte(0|1), which the projection reads back as
        // Int — real bytes like GameType are integers, not booleans.
        let doc = map(vec![
            ("on", DataValue::Bool(true)),
            ("off", DataValue::Bool(false)),
        ]);
        let bytes = encode_data_value_to_nbt("", &doc).unwrap();
        let (_, back) = decode_nbt_to_data_value(&bytes).unwrap();
        assert_eq!(
            back,
            map(vec![("off", DataValue::Int(0)), ("on", DataValue::Int(1)),])
        );
        // Wire check: both members are tag 1 (Byte).
        let pos = bytes.windows(4).position(|w| w == b"\x00\x02on");
        assert_eq!(bytes[pos.unwrap() - 1], 1);
    }

    #[test]
    fn roundtrip_int_widths_are_canonical() {
        // Small ints write tag 3, out-of-i32 ints write tag 4, and both
        // decode back to the same DataValue::Int.
        let doc = map(vec![
            ("small", DataValue::Int(42)),
            ("big", DataValue::Int(i64::from(i32::MAX) + 1)),
        ]);
        let bytes = encode_data_value_to_nbt("", &doc).unwrap();
        assert_eq!(bytes[0], 10, "root is a compound");
        let (_, back) = decode_nbt_to_data_value(&bytes).unwrap();
        assert_eq!(back, doc);
        // Raw tag ids on the wire: "big" writes 4 (Long), "small" writes 3.
        let pos_small = bytes.windows(7).position(|w| w == b"\x00\x05small");
        let pos_big = bytes.windows(5).position(|w| w == b"\x00\x03big");
        assert_eq!(bytes[pos_small.unwrap() - 1], 3);
        assert_eq!(bytes[pos_big.unwrap() - 1], 4);
    }

    #[test]
    fn mutf8_strings_survive_in_both_directions() {
        // MUTF-8 NUL is 0xC0 0x80; a supplementary character is a surrogate
        // pair (six bytes). Both must decode to their real scalar values.
        let mut s = Vec::new();
        s.extend_from_slice(&[0, 9]); // len 9
        s.extend_from_slice(&[0xed, 0xa0, 0xbd, 0xed, 0xb8, 0x80]); // U+1F600 as surrogates
        s.push(0x7f); // DEL, one byte
        s.extend_from_slice(&[0xc0, 0x80]); // NUL, two-byte form
        let mut compound = vec![8u8, 0, 1, b'k'];
        compound.extend_from_slice(&s);
        compound.push(0);
        let doc = named(10, "", &compound);
        let (_, value) = decode_nbt_to_data_value(&doc).unwrap();
        let k = &value.as_map().unwrap()["k"];
        assert_eq!(*k, DataValue::Str("\u{1f600}\u{7f}\u{0}".to_string()));

        // And the encoder emits the same MUTF-8 shape.
        let doc = map(vec![("k", DataValue::Str("\u{1f600}\u{0}x".to_string()))]);
        let bytes = encode_data_value_to_nbt("", &doc).unwrap();
        let (_, back) = decode_nbt_to_data_value(&bytes).unwrap();
        assert_eq!(back, doc);
    }

    #[test]
    fn decode_fails_closed_on_truncation() {
        let doc = map(vec![
            ("a", DataValue::Int(7)),
            ("b", DataValue::List(vec![DataValue::Str("xx".to_string())])),
            ("c", map(vec![("d", DataValue::Float(1.0))])),
        ]);
        let bytes = encode_data_value_to_nbt("root", &doc).unwrap();
        for cut in 0..bytes.len() {
            assert!(
                decode_nbt_to_data_value(&bytes[..cut]).is_err(),
                "prefix of {cut} bytes must not decode"
            );
        }
    }

    #[test]
    fn decode_fails_closed_on_depth_and_size_caps() {
        // The cap allows 64 container levels (depths 0..=63): a tree of 64
        // nested maps still fits, the 65th container is refused.
        let mut deep = DataValue::Int(1);
        for _ in 0..64 {
            deep = map(vec![("n", deep)]);
        }
        let bytes = encode_data_value_to_nbt("", &deep).unwrap();
        decode_nbt_to_data_value(&bytes).unwrap();

        let mut too_deep = DataValue::Int(1);
        for _ in 0..65 {
            too_deep = map(vec![("n", too_deep)]);
        }
        assert!(
            matches!(
                encode_data_value_to_nbt("", &too_deep),
                Err(AnvilInspectError::NbtTooDeep { .. })
            ),
            "encoder must refuse a tree deeper than the cap"
        );
        // A stream that nests past the cap fails the decoder too: build it
        // byte-by-byte since the encoder refuses to produce one.
        let mut raw = Vec::new();
        for _ in 0..65 {
            raw.extend_from_slice(&[10, 0, 1, b'n']); // compound "n"
        }
        raw.extend_from_slice(&[1, 0, 1, b'x', 7]); // byte x = 7
        raw.resize(raw.len() + 65, 0); // 65 compound terminators
        let doc = named(10, "", &raw);
        assert!(matches!(
            decode_nbt_to_data_value(&doc),
            Err(AnvilInspectError::NbtTooDeep { .. })
        ));

        // Input size cap.
        let oversized = vec![0u8; NBT_SIZE_CAP + 1];
        assert!(matches!(
            decode_nbt_to_data_value(&oversized),
            Err(AnvilInspectError::NbtTooLarge { .. })
        ));
    }

    #[test]
    fn decode_fails_closed_on_bad_structure() {
        // Trailing bytes after a complete root.
        let mut doc = named(3, "v", &7i32.to_be_bytes());
        doc.push(0xaa);
        assert!(matches!(
            decode_nbt_to_data_value(&doc),
            Err(AnvilInspectError::TrailingNbtBytes { count: 1 })
        ));

        // Unknown root tag id.
        let doc = named(99, "v", &[]);
        assert!(matches!(
            decode_nbt_to_data_value(&doc),
            Err(AnvilInspectError::MalformedNbt { .. })
        ));

        // List declaring an invalid element type.
        let mut payload = vec![9u8, 0, 1, b'k', 13]; // list kind 13
        payload.extend_from_slice(&0i32.to_be_bytes());
        payload.push(0);
        assert!(matches!(
            decode_nbt_to_data_value(&named(10, "", &payload)),
            Err(AnvilInspectError::MalformedNbt { .. })
        ));

        // List declaring kind 0 with a nonzero count.
        let mut payload = vec![9u8, 0, 1, b'k', 0];
        payload.extend_from_slice(&3i32.to_be_bytes());
        payload.push(0);
        assert!(matches!(
            decode_nbt_to_data_value(&named(10, "", &payload)),
            Err(AnvilInspectError::MalformedNbt { .. })
        ));

        // Collection length larger than the remaining input could hold.
        let mut payload = vec![11u8, 0, 1, b'a']; // int array "a"
        payload.extend_from_slice(&100i32.to_be_bytes()); // 100 ints
        payload.extend_from_slice(&[0, 0, 0]); // only 3 bytes of payload
        payload.push(0);
        assert!(matches!(
            decode_nbt_to_data_value(&named(10, "", &payload)),
            Err(AnvilInspectError::MalformedNbt { .. })
        ));

        // Lone surrogate in a string cannot become UTF-8.
        let mut payload = vec![8u8, 0, 1, b'k', 0, 3, 0xed, 0xa0, 0x80];
        payload.push(0);
        assert!(matches!(
            decode_nbt_to_data_value(&named(10, "", &payload)),
            Err(AnvilInspectError::InvalidNbtString { .. })
        ));

        // Four-byte UTF-8 is not legal MUTF-8.
        let mut payload = vec![8u8, 0, 1, b'k', 0, 4, 0xf0, 0x9f, 0x98, 0x80];
        payload.push(0);
        assert!(matches!(
            decode_nbt_to_data_value(&named(10, "", &payload)),
            Err(AnvilInspectError::InvalidNbtString { .. })
        ));
    }

    #[test]
    fn encode_fails_closed_on_unrepresentable_values() {
        // Null anywhere in the tree.
        for doc in [
            DataValue::Null,
            map(vec![("n", DataValue::Null)]),
            DataValue::List(vec![DataValue::Null]),
        ] {
            assert!(
                matches!(
                    encode_data_value_to_nbt("", &doc),
                    Err(AnvilInspectError::UnrepresentableValue { .. })
                ),
                "{doc:?} must not encode"
            );
        }
        // Heterogeneous lists.
        assert!(matches!(
            encode_data_value_to_nbt(
                "",
                &DataValue::List(vec![DataValue::Int(1), DataValue::Str("s".to_string())])
            ),
            Err(AnvilInspectError::UnrepresentableValue { .. })
        ));
        // Strings past the 64 KiB MUTF-8 limit.
        let huge = "x".repeat(70_000);
        assert!(matches!(
            encode_data_value_to_nbt("", &DataValue::Str(huge)),
            Err(AnvilInspectError::UnrepresentableValue { .. })
        ));
    }

    // -------------------------------------------------------------- gzip

    #[test]
    fn gzip_stored_roundtrip() {
        for payload in [
            b"".as_slice(),
            b"hello".as_slice(),
            &vec![0xabu8; 70_000], // spans multiple stored blocks
        ] {
            let wrapped = gzip_encode_stored(payload);
            assert_eq!(&wrapped[..3], &[0x1f, 0x8b, 8]);
            assert_eq!(gzip_decompress(&wrapped, 1 << 20).unwrap(), payload);
        }
    }

    #[test]
    fn gzip_concatenated_members_validate_each_trailer_and_reject_trailing_data() {
        let mut members = gzip_encode_stored(b"first ");
        members.extend_from_slice(&gzip_encode_stored(b"second"));
        assert_eq!(gzip_decompress(&members, 64).unwrap(), b"first second");

        let second_member = gzip_encode_stored(b"second");
        let mut bad_trailer = gzip_encode_stored(b"first ");
        let second_start = bad_trailer.len();
        bad_trailer.extend_from_slice(&second_member);
        let crc_offset = second_start + second_member.len() - 8;
        bad_trailer[crc_offset] ^= 1;
        assert!(matches!(
            gzip_decompress(&bad_trailer, 64),
            Err(AnvilInspectError::InvalidGzip(_))
        ));

        members.push(0);
        assert!(matches!(
            gzip_decompress(&members, 64),
            Err(AnvilInspectError::InvalidGzip(_))
        ));
    }

    #[test]
    fn gzip_fails_closed_on_corruption() {
        let wrapped = gzip_encode_stored(b"payload bytes");
        // Truncated at every point — never a silent partial decode.
        for cut in 0..wrapped.len() {
            assert!(
                gzip_decompress(&wrapped[..cut], 1 << 20).is_err(),
                "prefix of {cut} bytes must fail"
            );
        }
        // Corrupted CRC32 trailer.
        let mut bad_crc = wrapped.clone();
        let n = bad_crc.len();
        bad_crc[n - 8] ^= 0xff;
        assert!(matches!(
            gzip_decompress(&bad_crc, 1 << 20),
            Err(AnvilInspectError::InvalidGzip(_))
        ));
        // Bad magic.
        let mut bad_magic = wrapped.clone();
        bad_magic[0] = 0x1e;
        assert!(matches!(
            gzip_decompress(&bad_magic, 1 << 20),
            Err(AnvilInspectError::InvalidGzip(_))
        ));
        // Output budget is enforced.
        assert!(matches!(
            gzip_decompress(&wrapped, 4),
            Err(AnvilInspectError::GzipOverBudget { .. })
        ));
    }

    // ------------------------------------------------------------ .mca in

    /// Build a region file whose slot `slot` holds a record at
    /// `sector_offset` spanning `sectors` sectors with compression byte
    /// `compression` and declared length `declared_len`.
    struct McaFixture {
        bytes: Vec<u8>,
    }
    impl McaFixture {
        fn new() -> Self {
            Self {
                bytes: vec![0; MCA_HEADER_BYTES],
            }
        }
        fn chunk(
            mut self,
            slot: usize,
            sector_offset: u32,
            sectors: u8,
            declared_len: u32,
            compression: u8,
            timestamp: u32,
        ) -> Self {
            self.bytes[slot * 4] = ((sector_offset >> 16) & 0xff) as u8;
            self.bytes[slot * 4 + 1] = ((sector_offset >> 8) & 0xff) as u8;
            self.bytes[slot * 4 + 2] = (sector_offset & 0xff) as u8;
            self.bytes[slot * 4 + 3] = sectors;
            self.bytes[MCA_SECTOR_BYTES + slot * 4..MCA_SECTOR_BYTES + slot * 4 + 4]
                .copy_from_slice(&timestamp.to_be_bytes());
            let start = sector_offset as usize * MCA_SECTOR_BYTES;
            let end = (sector_offset as usize + sectors as usize) * MCA_SECTOR_BYTES;
            self.bytes.resize(self.bytes.len().max(end), 0);
            self.bytes[start..start + 4].copy_from_slice(&declared_len.to_be_bytes());
            self.bytes[start + 4] = compression;
            self
        }
    }

    #[test]
    fn mca_header_counts_and_classifies() {
        let file = McaFixture::new()
            .chunk(0, 2, 1, 10, 1, 1_700_000_001)
            .chunk(33, 3, 2, 5000, 2, 1_700_000_002)
            .chunk(500, 5, 1, 20, 3, 1_700_000_003)
            .chunk(1023, 6, 3, 9000, 4, 1_700_000_004)
            .chunk(64, 9, 1, 8, 77, 1_700_000_005)
            .bytes;
        let s = inspect_mca_header(&file).unwrap();
        assert_eq!(s.populated_chunks, 5);
        assert_eq!(s.gzip_chunks, 1);
        assert_eq!(s.zlib_chunks, 1);
        assert_eq!(s.uncompressed_chunks, 1);
        assert_eq!(s.lz4_chunks, 1);
        assert_eq!(s.unknown_compression_chunks, 1);
        assert_eq!(s.file_len, file.len());
        let c = &s.chunks[1];
        assert_eq!(c.slot, 33);
        assert_eq!(c.sector_offset, 3);
        assert_eq!(c.sector_count, 2);
        assert_eq!(c.declared_len, 5000);
        assert_eq!(c.compression, McaCompression::Zlib);
        assert_eq!(c.timestamp, 1_700_000_002);
    }

    #[test]
    fn mca_header_fails_closed() {
        // Shorter than the 8 KiB header — including one byte short.
        for len in [0usize, 100, MCA_HEADER_BYTES - 1] {
            assert!(matches!(
                inspect_mca_header(&vec![0; len]),
                Err(AnvilInspectError::TruncatedRegionHeader { .. })
            ));
        }
        // Header alone is a valid, fully empty region.
        assert_eq!(
            inspect_mca_header(&vec![0; MCA_HEADER_BYTES])
                .unwrap()
                .populated_chunks,
            0
        );
        // Half-populated: offset without sectors, and sectors without offset.
        let mut f = vec![0; MCA_HEADER_BYTES];
        f[0] = 0;
        f[1] = 0;
        f[2] = 4;
        f[3] = 0;
        assert!(matches!(
            inspect_mca_header(&f),
            Err(AnvilInspectError::InvalidRegionLocation { slot: 0, .. })
        ));
        let mut f = vec![0; MCA_HEADER_BYTES];
        f[3] = 2; // sectors, no offset
        assert!(matches!(
            inspect_mca_header(&f),
            Err(AnvilInspectError::InvalidRegionLocation { .. })
        ));
        // Sector run reaching past EOF: slot 0 claims sector 2, but the
        // file ends at the header.
        let mut f = vec![0; MCA_HEADER_BYTES];
        f[2] = 2;
        f[3] = 1;
        assert!(matches!(
            inspect_mca_header(&f),
            Err(AnvilInspectError::SectorRunOutOfBounds { slot: 0, .. })
        ));
        // Record length cannot fit the declared sectors.
        let f = McaFixture::new().chunk(0, 2, 1, 4097, 2, 0).bytes;
        assert!(matches!(
            inspect_mca_header(&f),
            Err(AnvilInspectError::InvalidChunkRecordLength { .. })
        ));
        // Record length below the minimum (needs ≥ 1 compression + ≥ 1 payload).
        let f = McaFixture::new().chunk(0, 2, 1, 1, 2, 0).bytes;
        assert!(matches!(
            inspect_mca_header(&f),
            Err(AnvilInspectError::InvalidChunkRecordLength { .. })
        ));
        // Location entry pointing into the header sectors.
        let mut f = vec![0; 3 * MCA_SECTOR_BYTES];
        f[2] = 1;
        f[3] = 1;
        assert!(matches!(
            inspect_mca_header(&f),
            Err(AnvilInspectError::InvalidRegionLocation { .. })
        ));
    }

    // ------------------------------------------------------- level.dat in

    /// A `level.dat`-shaped `DataValue`: root `{ "": {Data: {...}} }`.
    fn level_doc(dv: i64, name: &str, version: &str, seed: i64) -> DataValue {
        map(vec![(
            "Data",
            map(vec![
                ("DataVersion", DataValue::Int(dv)),
                ("LevelName", DataValue::Str(name.to_string())),
                ("RandomSeed", DataValue::Int(seed)),
                (
                    "Version",
                    map(vec![
                        ("Id", DataValue::Int(dv)),
                        ("Name", DataValue::Str(version.to_string())),
                        // Vanilla writes Snapshot as a Byte, which the
                        // bridge reads as Int.
                        ("Snapshot", DataValue::Int(0)),
                    ]),
                ),
                (
                    "DataPacks",
                    map(vec![
                        (
                            "Enabled",
                            DataValue::List(vec![
                                DataValue::Str("vanilla".to_string()),
                                DataValue::Str("test_pack".to_string()),
                            ]),
                        ),
                        (
                            "Disabled",
                            DataValue::List(vec![DataValue::Str("old_pack".to_string())]),
                        ),
                    ]),
                ),
            ]),
        )])
    }

    #[test]
    fn level_dat_gzip_and_raw_both_decode() {
        let doc = level_doc(DV_1_21_1 as i64, "Test World", "1.21.1", 42);
        let nbt = encode_data_value_to_nbt("", &doc).unwrap();

        for bytes in [nbt.clone(), gzip_encode_stored(&nbt)] {
            let s = inspect_level_dat(&bytes).unwrap();
            assert_eq!(s.data_version, DV_1_21_1);
            assert_eq!(
                s.compatibility,
                AnvilCompatibility::Supported(TargetVersion::V1_21_1)
            );
            assert_eq!(s.level_name.as_deref(), Some("Test World"));
            assert_eq!(s.version_name.as_deref(), Some("1.21.1"));
            assert_eq!(s.random_seed, Some(42));
            assert_eq!(s.enabled_datapacks, ["vanilla", "test_pack"]);
            assert_eq!(s.disabled_datapacks, ["old_pack"]);
            assert_eq!(s.root, doc);
            assert_eq!(s.nbt_len, nbt.len());
        }
        let s = inspect_level_dat(&gzip_encode_stored(&nbt)).unwrap();
        assert_eq!(s.compression, LevelDatCompression::Gzip);
        let s = inspect_level_dat(&nbt).unwrap();
        assert_eq!(s.compression, LevelDatCompression::None);
    }

    #[test]
    fn level_dat_5023_and_unsupported_versions_classify() {
        let nbt =
            encode_data_value_to_nbt("", &level_doc(DV_26_3 as i64, "New", "26.3", -1)).unwrap();
        let s = inspect_level_dat(&gzip_encode_stored(&nbt)).unwrap();
        assert_eq!(
            s.compatibility,
            AnvilCompatibility::Supported(TargetVersion::V26_3)
        );

        // WorldGenSettings.seed fallback (no top-level RandomSeed).
        let doc = map(vec![(
            "Data",
            map(vec![
                ("DataVersion", DataValue::Int(5023)),
                ("WorldGenSettings", map(vec![("seed", DataValue::Int(777))])),
            ]),
        )]);
        let nbt = encode_data_value_to_nbt("", &doc).unwrap();
        assert_eq!(inspect_level_dat(&nbt).unwrap().random_seed, Some(777));

        for (dv, want) in [
            (1000, AnvilCompatibility::OlderThanTargets),
            (4500, AnvilCompatibility::BetweenTargets),
            (6000, AnvilCompatibility::NewerThanTargets),
        ] {
            let nbt = encode_data_value_to_nbt("", &level_doc(dv, "W", "x", 0)).unwrap();
            assert_eq!(inspect_level_dat(&nbt).unwrap().compatibility, want);
        }
    }

    #[test]
    fn level_dat_fails_closed() {
        // Missing level.dat content cases: no Data compound, no DataVersion.
        let nbt = encode_data_value_to_nbt("", &map(vec![("Other", DataValue::Int(1))])).unwrap();
        assert!(matches!(
            inspect_level_dat(&nbt),
            Err(AnvilInspectError::MissingLevelField { field: "Data" })
        ));
        let nbt = encode_data_value_to_nbt("", &map(vec![("Data", map(vec![]))])).unwrap();
        assert!(matches!(
            inspect_level_dat(&nbt),
            Err(AnvilInspectError::MissingLevelField {
                field: "Data.DataVersion"
            })
        ));
        // Garbage is neither gzip nor NBT.
        assert!(inspect_level_dat(b"this is not nbt").is_err());
        // Corrupt gzip member.
        let mut bad = gzip_encode_stored(b"x");
        bad[2] = 7; // method 7 is not deflate
        assert!(inspect_level_dat(&bad).is_err());
    }

    #[test]
    fn datafixer_runs_on_decoded_level_dat() {
        // The decoded `Data` compound is a fixer-ready document: its own
        // DataVersion drives the pipeline.
        let doc = level_doc(DV_1_21_1 as i64, "Bridge", "1.21.1", 5);
        let nbt = encode_data_value_to_nbt("", &doc).unwrap();
        let s = inspect_level_dat(&gzip_encode_stored(&nbt)).unwrap();

        // The fixer reads DataVersion from the document root, so wrap the
        // decoded `Data` compound the way a level.dat export presents it.
        let mut doc = map(vec![
            ("Data", s.root.as_map().unwrap()["Data"].clone()),
            ("DataVersion", DataValue::Int(DV_1_21_1 as i64)),
        ]);
        let report = DataFixer::v1_21_1_to_26_3().fix(&mut doc).unwrap();
        assert_eq!(report.original_dv, DV_1_21_1);
        assert_eq!(report.final_dv, DV_26_3);
        let data = doc.as_map().unwrap()["Data"].as_map().unwrap();
        assert_eq!(data["DataVersion"], DataValue::Int(DV_26_3 as i64));
        assert_eq!(
            data["Version"].as_map().unwrap()["Name"],
            DataValue::Str("26.3".to_string())
        );

        // The migrated document re-encodes to a still-decodable level.dat.
        let out = map(vec![("Data", doc.as_map().unwrap()["Data"].clone())]);
        let bytes = encode_data_value_to_nbt("", &out).unwrap();
        let s2 = inspect_level_dat(&bytes).unwrap();
        assert_eq!(s2.data_version, DV_26_3);
        assert_eq!(s2.level_name.as_deref(), Some("Bridge"));
        assert_eq!(s2.version_name.as_deref(), Some("26.3"));
    }

    // ------------------------------------------------- level.dat migrate

    /// Verify the RFC 1952 trailer by hand: CRC32 then ISIZE, both
    /// little-endian, both over the inflated payload.
    fn assert_valid_gzip_member(wrapped: &[u8]) -> Vec<u8> {
        assert_eq!(&wrapped[..3], &[0x1f, 0x8b, 8], "gzip member header");
        let payload = gzip_decompress(wrapped, NBT_SIZE_CAP).unwrap();
        let n = wrapped.len();
        let crc = u32::from_le_bytes(wrapped[n - 8..n - 4].try_into().unwrap());
        let isize = u32::from_le_bytes(wrapped[n - 4..].try_into().unwrap());
        assert_eq!(crc, crc32(&payload), "CRC32 trailer");
        assert_eq!(isize, payload.len() as u32, "ISIZE trailer");
        payload
    }

    #[test]
    fn migrate_level_dat_upgrade_gzip_end_to_end() {
        let doc = level_doc(DV_1_21_1 as i64, "Direct", "1.21.1", 77);
        let input = gzip_encode_stored(&encode_data_value_to_nbt("", &doc).unwrap());
        let (out, report) = migrate_anvil_level_dat(&input, TargetVersion::V26_3, false).unwrap();
        assert_eq!(report.original_dv, DV_1_21_1);
        assert_eq!(report.final_dv, DV_26_3);
        assert!(report
            .applied_steps
            .contains(&"level_dat_version_metadata".to_string()));
        assert!(report
            .applied_steps
            .contains(&"stamp_data_version_26_3".to_string()));

        // Output is a real gzip member with a correct RFC 1952 trailer...
        let nbt = assert_valid_gzip_member(&out);
        // ...that inspect_level_dat reads back at 26.3 with every field
        // intact and the synthesized root DataVersion gone again.
        let s = inspect_level_dat(&out).unwrap();
        assert_eq!(s.compression, LevelDatCompression::Gzip);
        assert_eq!(s.data_version, DV_26_3);
        assert_eq!(s.version_name.as_deref(), Some("26.3"));
        assert_eq!(s.level_name.as_deref(), Some("Direct"));
        assert_eq!(s.random_seed, Some(77));
        assert_eq!(s.enabled_datapacks, ["vanilla", "test_pack"]);
        assert_eq!(s.disabled_datapacks, ["old_pack"]);
        assert_eq!(s.root_name, "");
        assert!(!s.root.as_map().unwrap().contains_key("DataVersion"));
        assert_eq!(decode_nbt_to_data_value(&nbt).unwrap().0, "");
    }

    #[test]
    fn migrate_level_dat_raw_input_and_root_name_preserved() {
        // Raw (uncompressed) NBT input still produces gzip output, and a
        // non-empty root name survives the round-trip verbatim.
        let doc = level_doc(DV_1_21_1 as i64, "Raw", "1.21.1", 0);
        let nbt = encode_data_value_to_nbt("Level", &doc).unwrap();
        let (out, _) = migrate_anvil_level_dat(&nbt, TargetVersion::V26_3, false).unwrap();
        assert_valid_gzip_member(&out);
        let s = inspect_level_dat(&out).unwrap();
        assert_eq!(s.compression, LevelDatCompression::Gzip);
        assert_eq!(s.root_name, "Level");
        assert_eq!(s.data_version, DV_26_3);
    }

    #[test]
    fn migrate_level_dat_idempotent_byte_deterministic() {
        let nbt = encode_data_value_to_nbt("", &level_doc(DV_26_3 as i64, "N", "26.3", 1)).unwrap();
        let input = gzip_encode_stored(&nbt);
        // Already at the target: a clean no-op that still re-seals to the
        // canonical gzip form (stored blocks, zero MTIME — byte-identical to
        // the identically-produced input).
        let (out1, report) = migrate_anvil_level_dat(&input, TargetVersion::V26_3, false).unwrap();
        assert_eq!(report.original_dv, DV_26_3);
        assert_eq!(report.final_dv, DV_26_3);
        assert!(report.applied_steps.is_empty());
        assert!(report.warnings.is_empty());
        // Re-running the migrator on its own output is byte-identical.
        let (out2, report2) = migrate_anvil_level_dat(&out1, TargetVersion::V26_3, false).unwrap();
        assert_eq!(out1, out2);
        assert_eq!(report2.applied_steps, Vec::<String>::new());
    }

    #[test]
    fn migrate_level_dat_downgrade_gate() {
        let nbt = encode_data_value_to_nbt("", &level_doc(DV_26_3 as i64, "N", "26.3", 1)).unwrap();
        let input = gzip_encode_stored(&nbt);

        // Refused without the explicit opt-in.
        assert_eq!(
            migrate_anvil_level_dat(&input, TargetVersion::V1_21_1, false).unwrap_err(),
            AnvilInspectError::DowngradeNotAllowed {
                from_dv: DV_26_3,
                to_dv: DV_1_21_1
            }
        );

        // Allowed with opt-in: back to 3955 with 1.21.1 metadata stamped.
        let (out, report) =
            migrate_anvil_level_dat(&input, TargetVersion::V1_21_1, true).unwrap();
        assert_eq!(report.original_dv, DV_26_3);
        assert_eq!(report.final_dv, DV_1_21_1);
        assert_valid_gzip_member(&out);
        let s = inspect_level_dat(&out).unwrap();
        assert_eq!(s.data_version, DV_1_21_1);
        assert_eq!(s.version_name.as_deref(), Some("1.21.1"));
        assert!(!s.root.as_map().unwrap().contains_key("DataVersion"));

        // A document already at 3955 targets 1.21.1 with no flag — it is a
        // no-op, not a downgrade.
        let (noop, report) = migrate_anvil_level_dat(&out, TargetVersion::V1_21_1, false).unwrap();
        assert_eq!(report.original_dv, DV_1_21_1);
        assert_eq!(inspect_level_dat(&noop).unwrap().data_version, DV_1_21_1);
    }

    #[test]
    fn migrate_level_dat_fails_closed() {
        // Missing Data.DataVersion.
        let nbt = encode_data_value_to_nbt("", &map(vec![("Data", map(vec![]))])).unwrap();
        assert!(matches!(
            migrate_anvil_level_dat(&nbt, TargetVersion::V26_3, false),
            Err(AnvilInspectError::MissingLevelField {
                field: "Data.DataVersion"
            })
        ));
        // Non-integer DataVersion.
        let doc = map(vec![(
            "Data",
            map(vec![("DataVersion", DataValue::Str("new".to_string()))]),
        )]);
        let nbt = encode_data_value_to_nbt("", &doc).unwrap();
        assert!(matches!(
            migrate_anvil_level_dat(&nbt, TargetVersion::V26_3, false),
            Err(AnvilInspectError::MissingLevelField {
                field: "Data.DataVersion"
            })
        ));
        // Corrupt gzip.
        let mut bad = gzip_encode_stored(b"x");
        bad[2] = 7; // method 7 is not deflate
        assert!(matches!(
            migrate_anvil_level_dat(&bad, TargetVersion::V26_3, false),
            Err(AnvilInspectError::InvalidGzip(_))
        ));
        // DataVersion newer than the pipeline's ceiling.
        let nbt = encode_data_value_to_nbt("", &level_doc(6000, "F", "x", 0)).unwrap();
        assert!(matches!(
            migrate_anvil_level_dat(&nbt, TargetVersion::V26_3, false),
            Err(AnvilInspectError::Fix(
                DataFixError::NewerThanTarget { .. }
            ))
        ));
        // Same refusal going down: 6000 is newer than the downgrade
        // pipeline's source too, even with the flag set.
        assert!(matches!(
            migrate_anvil_level_dat(&nbt, TargetVersion::V1_21_1, true),
            Err(AnvilInspectError::Fix(
                DataFixError::NewerThanTarget { .. }
            ))
        ));
    }

    // ------------------------------------------------------ world inspect

    /// Write a synthetic Anvil world into `dir`: level.dat (gzipped iff
    /// `gzipped`) plus one region file per listed (dir, file) entry.
    fn make_world(root: &Path, dv: i64, gzipped: bool) {
        let nbt = encode_data_value_to_nbt("", &level_doc(dv, "Fixture World", "x.y.z", 123_456))
            .unwrap();
        let level = if gzipped {
            gzip_encode_stored(&nbt)
        } else {
            nbt
        };
        std::fs::write(root.join("level.dat"), level).unwrap();
        for (rel, file) in [
            ("region", "r.0.0.mca"),
            ("region", "r.0.-1.mca"),
            ("DIM-1/region", "r.1.0.mca"),
            ("DIM1/region", "r.-2.3.mca"),
        ] {
            let dir = root.join(rel);
            std::fs::create_dir_all(&dir).unwrap();
            let mca = McaFixture::new()
                .chunk(0, 2, 1, 12, 2, 1_700_000_000)
                .chunk(40, 3, 1, 8, 1, 1_700_000_001)
                .bytes;
            std::fs::write(dir.join(file), mca).unwrap();
        }
    }

    #[test]
    fn world_dir_end_to_end() {
        let dir = TestDir::new("world");
        make_world(&dir.0, DV_1_21_1 as i64, true);
        let report = inspect_anvil_world_dir(&dir.0).unwrap();
        assert_eq!(report.level_dat.data_version, DV_1_21_1);
        assert_eq!(report.region_file_count, 4);
        assert_eq!(report.populated_chunk_count, 8);
        assert_eq!(report.dimensions.len(), 3);
        assert_eq!(report.dimensions[0].dimension, "overworld");
        assert_eq!(report.dimensions[0].regions.len(), 2);
        assert_eq!(report.dimensions[0].populated_chunks, 4);
        assert_eq!(report.dimensions[1].dimension, "nether");
        assert_eq!(report.dimensions[1].region_dir, "DIM-1/region");
        assert_eq!(report.dimensions[2].dimension, "end");
        // Regions are name-sorted; coords came from the file names.
        let nether = &report.dimensions[1].regions[0];
        assert_eq!(nether.file_name, "r.1.0.mca");
        assert_eq!((nether.region_x, nether.region_z), (1, 0));
        assert_eq!(nether.summary.zlib_chunks, 1);
        assert_eq!(nether.summary.gzip_chunks, 1);

        let human = report.to_human();
        assert!(human.contains("minecraft 1.21.1"), "{human}");
        assert!(human.contains("\"Fixture World\""), "{human}");
        assert!(human.contains("data version 3955"), "{human}");
        assert!(human.contains("seed 123456"), "{human}");
        assert!(
            human.contains("4 region file(s), 8 populated chunk(s)"),
            "{human}"
        );
        assert!(human.contains("nether"), "{human}");

        // JSON is deterministic and parses back to the same tree.
        let json = report.to_json();
        assert_eq!(json, report.to_json());
        let parsed = crate::migrate::parse_data_json(&json).unwrap();
        let m = parsed.as_map().unwrap();
        assert_eq!(m["data_version"], DataValue::Int(3955));
        assert_eq!(m["detected_target"], DataValue::Str("1.21.1".to_string()));
        assert_eq!(m["compatibility"], DataValue::Str("supported".to_string()));
        assert_eq!(m["populated_chunks"], DataValue::Int(8));
        assert_eq!(m["region_file_count"], DataValue::Int(4));
        let dims = match &m["dimensions"] {
            DataValue::List(v) => v.clone(),
            _ => panic!(),
        };
        assert_eq!(dims.len(), 3);
        let ld = m["level_dat"].as_map().unwrap();
        assert_eq!(ld["compression"], DataValue::Str("gzip".to_string()));
        assert_eq!(
            ld["datapacks"].as_map().unwrap()["enabled"],
            DataValue::List(vec![
                DataValue::Str("vanilla".to_string()),
                DataValue::Str("test_pack".to_string())
            ])
        );
    }

    #[test]
    fn world_dir_fail_closed_cases() {
        // Missing level.dat.
        let dir = TestDir::new("world-no-level");
        assert!(matches!(
            inspect_anvil_world_dir(&dir.0),
            Err(AnvilInspectError::MissingLevelDat { .. })
        ));
        // Corrupt level.dat.
        std::fs::write(dir.join("level.dat"), b"not nbt at all").unwrap();
        assert!(matches!(
            inspect_anvil_world_dir(&dir.0),
            Err(AnvilInspectError::InvalidWorldFile { .. })
        ));
        // Truncated .mca fails the whole inspection.
        std::fs::remove_file(dir.join("level.dat")).unwrap();
        let nbt = encode_data_value_to_nbt("", &level_doc(3955, "W", "1.21.1", 0)).unwrap();
        std::fs::write(dir.join("level.dat"), nbt).unwrap();
        let rdir = dir.join("region");
        std::fs::create_dir_all(&rdir).unwrap();
        std::fs::write(rdir.join("r.0.0.mca"), vec![0u8; 100]).unwrap();
        assert!(matches!(
            inspect_anvil_world_dir(&dir.0),
            Err(AnvilInspectError::InvalidWorldFile { .. })
        ));
        // Non-r.*.*.mca files are ignored, not errors.
        std::fs::remove_file(rdir.join("r.0.0.mca")).unwrap();
        std::fs::write(rdir.join("notes.txt"), b"hi").unwrap();
        std::fs::write(rdir.join("weird.mca"), vec![0u8; 10]).unwrap();
        let report = inspect_anvil_world_dir(&dir.0).unwrap();
        assert_eq!(report.region_file_count, 0);
        assert!(report.to_human().contains("no region files"));
    }

    #[test]
    fn inspect_anvil_discovers_custom_mod_dimension_regions() {
        let dir = TestDir::new("world-custom-dims");
        let nbt = encode_data_value_to_nbt("", &level_doc(3955, "ModWorld", "1.21.1", 7)).unwrap();
        std::fs::write(dir.join("level.dat"), nbt).unwrap();
        let custom_dir = dir.join("dimensions/aether/the_aether/region");
        std::fs::create_dir_all(&custom_dir).unwrap();
        let mca = McaFixture::new()
            .chunk(0, 2, 1, 12, 2, 1_700_000_000)
            .bytes;
        std::fs::write(custom_dir.join("r.0.0.mca"), mca).unwrap();
        let report = inspect_anvil_world_dir(&dir.0).unwrap();
        assert_eq!(report.region_file_count, 1);
        assert_eq!(report.populated_chunk_count, 1);
        assert_eq!(report.dimensions.len(), 1);
        assert_eq!(report.dimensions[0].dimension, "aether:the_aether");
        assert_eq!(
            report.dimensions[0].region_dir,
            "dimensions/aether/the_aether/region"
        );
    }
}
