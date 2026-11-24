// SPDX-License-Identifier: LGPL-3.0-or-later

//! `.nbt` structure schematic loader — the format Minecraft's Structure
//! Blocks write and Create ships for its Ponder/showcase factories.
//!
//! The container is one named-root `TAG_Compound` holding `size: [i32; 3]`
//! (list or int array), `palette: [{ Name, Properties? }]` and
//! `blocks: [{ pos: [i32; 3], state: i32, nbt? }]`, usually gzip-compressed
//! (single member, RFC 1952). Both the gzipped and raw forms are accepted.
//!
//! The reader is fail-closed and DoS-bounded throughout:
//!
//! * Decompression runs through [`inflate_raw_rfc1951_with_consumed`] with a
//!   [`SCHEMATIC_MAX_DECOMPRESSED_BYTES`] ceiling — a zip bomb rejects at the
//!   cap instead of growing memory, and the gzip CRC32/ISIZE trailer is
//!   verified against [`crc32_ieee`].
//! * The NBT walk enforces a fixed depth ceiling plus an aggregate tag
//!   budget and bounds every declared length against the bytes actually
//!   remaining, so attacker-controlled nesting, negative lengths and
//!   `TAG_List(TAG_End, n)` shapes cannot exhaust the stack or CPU.
//! * `palette` is capped at [`SCHEMATIC_MAX_PALETTE_ENTRIES`], `blocks` at
//!   [`SCHEMATIC_MAX_BLOCKS`], and every `state` index is validated against
//!   the decoded palette before it lands in the result.

use std::fmt;

use megu_core::hash::crc32_ieee;
use megu_core::zip_archive::{inflate_raw_rfc1951_with_consumed, ZipError};

/// Maximum decompressed byte size of one schematic (16 MiB).
pub const SCHEMATIC_MAX_DECOMPRESSED_BYTES: usize = 16 * 1024 * 1024;
/// Maximum entries accepted in the `blocks` list.
pub const SCHEMATIC_MAX_BLOCKS: usize = 262_144;
/// Maximum entries accepted in the `palette` list.
pub const SCHEMATIC_MAX_PALETTE_ENTRIES: usize = 16_384;
/// Nesting ceiling for the NBT walk (the root compound is depth 1).
const NBT_MAX_DEPTH: usize = 64;
/// Aggregate number of NBT values one parse may materialize. Bounded anyway
/// by the decompression cap (~1.4 M tiny compounds per 16 MiB), the budget
/// keeps a defensive ceiling that cannot be argued above by padding.
const NBT_MAX_TAGS: usize = 4_000_000;
/// One `TAG_String` may not exceed 32 KiB — real structure names are short.
const NBT_MAX_STRING_BYTES: usize = 32 * 1024;

/// One decoded palette entry: the block id plus its property map.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SchematicBlockState {
    /// `namespace:path` as written in the palette (`minecraft:` inferred
    /// when the palette omits the namespace).
    pub name: String,
    /// Property map sorted by key — the same order
    /// `BlockRegistry::encode_canonical` and `format_canonical` emit.
    pub properties: Vec<(String, String)>,
    /// `name[k=v,...]` with sorted keys — resolves directly against the
    /// live block registries (`state_from_arg`, `live_mod_state_for_arg`).
    pub canonical: String,
}

/// One `blocks` list entry: a structure-relative cell and the palette index
/// (already bounds-checked) of the state that belongs there.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SchematicBlock {
    pub pos: [i32; 3],
    /// Index into [`SchematicStructure::palette`].
    pub state: usize,
}

/// A decoded `.nbt` structure.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SchematicStructure {
    pub size: [i32; 3],
    pub palette: Vec<SchematicBlockState>,
    pub blocks: Vec<SchematicBlock>,
}

/// Counts returned by a live schematic placement (`live_mod_bridge`'s
/// `live_place_nbt_schematic_in_dim`): how many cells were written, how many
/// of those armed a node in the Create factory runtime, how many palette
/// entries never resolved to a live state id, and how many `blocks` entries
/// were skipped (unresolved palette entry or rejected write).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SchematicPlacementReport {
    pub placed_blocks: usize,
    pub kinetic_nodes_armed: usize,
    pub unresolved_palette_entries: usize,
    pub skipped_blocks: usize,
}

/// Every deterministic failure of the schematic reader.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SchematicError {
    /// The input is neither a valid gzip member nor a named-root NBT blob.
    Malformed(&'static str),
    /// The deflate stream failed, or its gzip framing was invalid.
    Inflate(String),
    /// The gzip CRC32 or ISIZE trailer does not match the inflated bytes.
    GzipTrailerMismatch,
    /// Bytes follow the single accepted gzip member / NBT root.
    TrailingBytes,
    /// The NBT walk hit a structural limit or invalid tag id.
    Nbt(&'static str),
    /// `size`, `palette` or `blocks` is missing or has the wrong shape.
    MissingField(&'static str),
    /// `palette` exceeded [`SCHEMATIC_MAX_PALETTE_ENTRIES`].
    PaletteTooLarge,
    /// `blocks` exceeded [`SCHEMATIC_MAX_BLOCKS`].
    BlocksTooLarge,
    /// A `blocks` entry's `state` index lies outside the palette.
    PaletteIndexOutOfBounds { index: usize, palette: usize },
    /// A `size` component was negative.
    NegativeSize,
}

impl fmt::Display for SchematicError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Malformed(what) => write!(f, "malformed schematic input: {what}"),
            Self::Inflate(what) => write!(f, "schematic inflate failed: {what}"),
            Self::GzipTrailerMismatch => {
                write!(f, "gzip CRC32/ISIZE trailer mismatch")
            }
            Self::TrailingBytes => write!(f, "trailing bytes after schematic payload"),
            Self::Nbt(what) => write!(f, "invalid NBT: {what}"),
            Self::MissingField(what) => write!(f, "structure field missing or wrong shape: {what}"),
            Self::PaletteTooLarge => write!(f, "palette exceeds {SCHEMATIC_MAX_PALETTE_ENTRIES}"),
            Self::BlocksTooLarge => write!(f, "blocks list exceeds {SCHEMATIC_MAX_BLOCKS}"),
            Self::PaletteIndexOutOfBounds { index, palette } => {
                write!(f, "block state index {index} outside palette of {palette}")
            }
            Self::NegativeSize => write!(f, "structure size has a negative component"),
        }
    }
}

impl std::error::Error for SchematicError {}

// ---------------------------------------------------------------------------
// gzip container (RFC 1952, single member, fail-closed)
// ---------------------------------------------------------------------------

fn gunzip(input: &[u8]) -> Result<Vec<u8>, SchematicError> {
    const FHCRC: u8 = 0x02;
    const FEXTRA: u8 = 0x04;
    const FNAME: u8 = 0x08;
    const FCOMMENT: u8 = 0x10;
    if input.len() < 10 {
        return Err(SchematicError::Malformed("gzip header truncated"));
    }
    if input[2] != 8 {
        return Err(SchematicError::Malformed("gzip method is not deflate"));
    }
    let flg = input[3];
    if flg & 0xE0 != 0 {
        return Err(SchematicError::Malformed("gzip reserved flag bits set"));
    }
    let mut pos = 10usize;
    if flg & FEXTRA != 0 {
        if input.len() < pos + 2 {
            return Err(SchematicError::Malformed("gzip header truncated"));
        }
        let xlen = u16::from_le_bytes(input[pos..pos + 2].try_into().unwrap()) as usize;
        pos += 2;
        if input.len().saturating_sub(pos) < xlen {
            return Err(SchematicError::Malformed("gzip FEXTRA truncated"));
        }
        pos += xlen;
    }
    for field in [FNAME, FCOMMENT] {
        if flg & field != 0 {
            let nul = input[pos..]
                .iter()
                .position(|&b| b == 0)
                .ok_or(SchematicError::Malformed("gzip zero-terminated field unterminated"))?;
            pos += nul + 1;
        }
    }
    if flg & FHCRC != 0 {
        if input.len().saturating_sub(pos) < 2 {
            return Err(SchematicError::Malformed("gzip FHCRC truncated"));
        }
        pos += 2;
    }
    let (out, consumed) = inflate_raw_rfc1951_with_consumed(
        &input[pos..],
        SCHEMATIC_MAX_DECOMPRESSED_BYTES,
    )
    .map_err(|e: ZipError| SchematicError::Inflate(e.to_string()))?;
    pos += consumed;
    if input.len().saturating_sub(pos) < 8 {
        return Err(SchematicError::Malformed("gzip trailer truncated"));
    }
    let trailer = &input[pos..pos + 8];
    let want_crc = u32::from_le_bytes(trailer[0..4].try_into().unwrap());
    let want_size = u32::from_le_bytes(trailer[4..8].try_into().unwrap());
    if crc32_ieee(&out) != want_crc || out.len() as u32 != want_size {
        return Err(SchematicError::GzipTrailerMismatch);
    }
    if pos + 8 != input.len() {
        return Err(SchematicError::TrailingBytes);
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Bounded NBT DOM reader
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq)]
enum Tag {
    Byte(i8),
    Short(i16),
    Int(i32),
    Long(i64),
    Float(f32),
    Double(f64),
    ByteArray(Vec<u8>),
    String(String),
    List(Vec<Tag>),
    Compound(Vec<(String, Tag)>),
    IntArray(Vec<i32>),
    LongArray(Vec<i64>),
}

struct NbtReader<'a> {
    input: &'a [u8],
    pos: usize,
    depth: usize,
    tags_left: usize,
}

impl<'a> NbtReader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], SchematicError> {
        if self.input.len().saturating_sub(self.pos) < n {
            return Err(SchematicError::Nbt("input truncated"));
        }
        let out = &self.input[self.pos..self.pos + n];
        self.pos += n;
        Ok(out)
    }

    fn u8(&mut self) -> Result<u8, SchematicError> {
        Ok(self.take(1)?[0])
    }

    fn u16(&mut self) -> Result<u16, SchematicError> {
        Ok(u16::from_be_bytes(self.take(2)?.try_into().unwrap()))
    }

    fn i32(&mut self) -> Result<i32, SchematicError> {
        Ok(i32::from_be_bytes(self.take(4)?.try_into().unwrap()))
    }

    fn i64(&mut self) -> Result<i64, SchematicError> {
        Ok(i64::from_be_bytes(self.take(8)?.try_into().unwrap()))
    }

    fn string(&mut self) -> Result<String, SchematicError> {
        let len = usize::from(self.u16()?);
        if len > NBT_MAX_STRING_BYTES {
            return Err(SchematicError::Nbt("string exceeds length cap"));
        }
        let bytes = self.take(len)?;
        String::from_utf8(bytes.to_vec()).map_err(|_| SchematicError::Nbt("invalid UTF-8 string"))
    }

    /// Read one value payload of `tag`. Every value spends one unit of the
    /// aggregate tag budget; containers recurse under the depth ceiling.
    fn payload(&mut self, tag: u8) -> Result<Tag, SchematicError> {
        if self.tags_left == 0 {
            return Err(SchematicError::Nbt("tag budget exhausted"));
        }
        self.tags_left -= 1;
        match tag {
            1 => Ok(Tag::Byte(self.u8()? as i8)),
            2 => Ok(Tag::Short(i16::from_be_bytes(
                self.take(2)?.try_into().unwrap(),
            ))),
            3 => Ok(Tag::Int(self.i32()?)),
            4 => Ok(Tag::Long(self.i64()?)),
            5 => Ok(Tag::Float(f32::from_be_bytes(
                self.take(4)?.try_into().unwrap(),
            ))),
            6 => Ok(Tag::Double(f64::from_be_bytes(
                self.take(8)?.try_into().unwrap(),
            ))),
            7 => {
                let len = self.array_len(tag)?;
                Ok(Tag::ByteArray(self.take(len)?.to_vec()))
            }
            8 => Ok(Tag::String(self.string()?)),
            9 => {
                let elem = self.u8()?;
                let len = self.array_len(tag)?;
                if elem == 0 {
                    if len != 0 {
                        return Err(SchematicError::Nbt("list of TAG_End with elements"));
                    }
                    return Ok(Tag::List(Vec::new()));
                }
                if elem > 12 {
                    return Err(SchematicError::Nbt("invalid list element tag"));
                }
                // Every element occupies at least one byte of input, so a
                // declared length past the remaining input cannot decode.
                if len > self.input.len().saturating_sub(self.pos) {
                    return Err(SchematicError::Nbt("list length exceeds input"));
                }
                if self.depth >= NBT_MAX_DEPTH {
                    return Err(SchematicError::Nbt("depth ceiling exceeded"));
                }
                self.depth += 1;
                let mut items = Vec::with_capacity(len.min(1024));
                for _ in 0..len {
                    items.push(self.payload(elem)?);
                }
                self.depth -= 1;
                Ok(Tag::List(items))
            }
            10 => {
                if self.depth >= NBT_MAX_DEPTH {
                    return Err(SchematicError::Nbt("depth ceiling exceeded"));
                }
                self.depth += 1;
                let mut entries = Vec::new();
                loop {
                    let child = self.u8()?;
                    if child == 0 {
                        break;
                    }
                    if child > 12 {
                        return Err(SchematicError::Nbt("invalid compound entry tag"));
                    }
                    let name = self.string()?;
                    entries.push((name, self.payload(child)?));
                }
                self.depth -= 1;
                Ok(Tag::Compound(entries))
            }
            11 => {
                let len = self.array_len(tag)?;
                let bytes = len
                    .checked_mul(4)
                    .ok_or(SchematicError::Nbt("int array length overflow"))?;
                let raw = self.take(bytes)?;
                Ok(Tag::IntArray(
                    raw.as_chunks::<4>()
                        .0
                        .iter()
                        .map(|&c| i32::from_be_bytes(c))
                        .collect(),
                ))
            }
            12 => {
                let len = self.array_len(tag)?;
                let bytes = len
                    .checked_mul(8)
                    .ok_or(SchematicError::Nbt("long array length overflow"))?;
                let raw = self.take(bytes)?;
                Ok(Tag::LongArray(
                    raw.as_chunks::<8>()
                        .0
                        .iter()
                        .map(|&c| i64::from_be_bytes(c))
                        .collect(),
                ))
            }
            _ => Err(SchematicError::Nbt("invalid tag id")),
        }
    }

    fn array_len(&mut self, _tag: u8) -> Result<usize, SchematicError> {
        let len = self.i32()?;
        if len < 0 {
            return Err(SchematicError::Nbt("negative length"));
        }
        let len = len as usize;
        if len > self.input.len().saturating_sub(self.pos) {
            return Err(SchematicError::Nbt("declared length exceeds input"));
        }
        Ok(len)
    }
}

/// Read one named-root NBT document, requiring a `TAG_Compound` root and
/// full input consumption.
fn read_root_compound(input: &[u8]) -> Result<Vec<(String, Tag)>, SchematicError> {
    let mut reader = NbtReader {
        input,
        pos: 0,
        depth: 0,
        tags_left: NBT_MAX_TAGS,
    };
    let root_tag = reader.u8()?;
    if root_tag != 10 {
        return Err(SchematicError::Malformed("root tag is not TAG_Compound"));
    }
    let _name = reader.string()?;
    let root = reader.payload(root_tag)?;
    if reader.pos != input.len() {
        return Err(SchematicError::TrailingBytes);
    }
    match root {
        Tag::Compound(entries) => Ok(entries),
        _ => Err(SchematicError::Malformed("root tag is not TAG_Compound")),
    }
}

// ---------------------------------------------------------------------------
// Structure extraction
// ---------------------------------------------------------------------------

fn find<'a>(entries: &'a [(String, Tag)], name: &str) -> Option<&'a Tag> {
    entries
        .iter()
        .find(|(key, _)| key == name)
        .map(|(_, tag)| tag)
}

/// `size`/`pos` decode: `TAG_List` of three `TAG_Int` or a `TAG_IntArray`
/// of three — both spellings appear in shipped `.nbt` structures.
fn int3(tag: &Tag) -> Option<[i32; 3]> {
    match tag {
        Tag::IntArray(v) if v.len() == 3 => Some([v[0], v[1], v[2]]),
        Tag::List(items) if items.len() == 3 => match (&items[0], &items[1], &items[2]) {
            (Tag::Int(x), Tag::Int(y), Tag::Int(z)) => Some([*x, *y, *z]),
            _ => None,
        },
        _ => None,
    }
}

/// Decode a `.nbt` structure — gzipped or raw — into a
/// [`SchematicStructure`]. Fails closed on every malformed or over-cap
/// input; see the module docs for the bound list.
pub fn parse_nbt_schematic(input: &[u8]) -> Result<SchematicStructure, SchematicError> {
    let nbt;
    let bytes: &[u8] = if input.len() >= 2 && input[0] == 0x1f && input[1] == 0x8b {
        nbt = gunzip(input)?;
        &nbt
    } else {
        input
    };
    let root = read_root_compound(bytes)?;

    let size = find(&root, "size")
        .and_then(int3)
        .ok_or(SchematicError::MissingField("size"))?;
    if size.iter().any(|&n| n < 0) {
        return Err(SchematicError::NegativeSize);
    }

    let palette_tag = find(&root, "palette").ok_or(SchematicError::MissingField("palette"))?;
    let Tag::List(palette_entries) = palette_tag else {
        return Err(SchematicError::MissingField("palette"));
    };
    if palette_entries.len() > SCHEMATIC_MAX_PALETTE_ENTRIES {
        return Err(SchematicError::PaletteTooLarge);
    }
    let mut palette = Vec::with_capacity(palette_entries.len());
    for entry in palette_entries {
        let Tag::Compound(fields) = entry else {
            return Err(SchematicError::MissingField("palette entry"));
        };
        let Some(Tag::String(raw_name)) = find(fields, "Name") else {
            return Err(SchematicError::MissingField("palette Name"));
        };
        let name = if raw_name.contains(':') {
            raw_name.clone()
        } else {
            format!("minecraft:{raw_name}")
        };
        let mut properties: Vec<(String, String)> = match find(fields, "Properties") {
            Some(Tag::Compound(props)) => props
                .iter()
                .filter_map(|(key, value)| match value {
                    Tag::String(v) => Some((key.clone(), v.clone())),
                    _ => None,
                })
                .collect(),
            Some(_) => return Err(SchematicError::MissingField("palette Properties")),
            None => Vec::new(),
        };
        properties.sort_by(|a, b| a.0.cmp(&b.0));
        let canonical =
            mod_adapter_core::blockstates::format_canonical(&name, &properties);
        palette.push(SchematicBlockState {
            name,
            properties,
            canonical,
        });
    }

    let blocks_tag = find(&root, "blocks").ok_or(SchematicError::MissingField("blocks"))?;
    let Tag::List(block_entries) = blocks_tag else {
        return Err(SchematicError::MissingField("blocks"));
    };
    if block_entries.len() > SCHEMATIC_MAX_BLOCKS {
        return Err(SchematicError::BlocksTooLarge);
    }
    let mut blocks = Vec::with_capacity(block_entries.len());
    for entry in block_entries {
        let Tag::Compound(fields) = entry else {
            return Err(SchematicError::MissingField("block entry"));
        };
        let pos = find(fields, "pos")
            .and_then(int3)
            .ok_or(SchematicError::MissingField("block pos"))?;
        let index = match find(fields, "state") {
            Some(Tag::Int(state)) if *state >= 0 => *state as usize,
            _ => return Err(SchematicError::MissingField("block state")),
        };
        if index >= palette.len() {
            return Err(SchematicError::PaletteIndexOutOfBounds {
                index,
                palette: palette.len(),
            });
        }
        // `nbt` block-entity data, when present, is parsed and bounds
        // checked like every other tag but intentionally not surfaced:
        // the live placement path only consumes geometry + state.
        blocks.push(SchematicBlock { pos, state: index });
    }

    Ok(SchematicStructure {
        size,
        palette,
        blocks,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- minimal NBT + gzip writers (test fixtures only) ----

    fn w_str(out: &mut Vec<u8>, name: &str, value: &str) {
        out.push(8);
        out.extend_from_slice(&(name.len() as u16).to_be_bytes());
        out.extend_from_slice(name.as_bytes());
        out.extend_from_slice(&(value.len() as u16).to_be_bytes());
        out.extend_from_slice(value.as_bytes());
    }

    fn w_i32(out: &mut Vec<u8>, name: &str, value: i32) {
        out.push(3);
        out.extend_from_slice(&(name.len() as u16).to_be_bytes());
        out.extend_from_slice(name.as_bytes());
        out.extend_from_slice(&value.to_be_bytes());
    }

    fn w_int_list(out: &mut Vec<u8>, name: &str, values: &[i32]) {
        out.push(9);
        out.extend_from_slice(&(name.len() as u16).to_be_bytes());
        out.extend_from_slice(name.as_bytes());
        out.push(3);
        out.extend_from_slice(&(values.len() as i32).to_be_bytes());
        for value in values {
            out.extend_from_slice(&value.to_be_bytes());
        }
    }

    fn w_list_header(out: &mut Vec<u8>, name: &str, elem: u8, len: i32) {
        out.push(9);
        out.extend_from_slice(&(name.len() as u16).to_be_bytes());
        out.extend_from_slice(name.as_bytes());
        out.push(elem);
        out.extend_from_slice(&len.to_be_bytes());
    }

    fn w_compound_header(out: &mut Vec<u8>, name: &str) {
        out.push(10);
        out.extend_from_slice(&(name.len() as u16).to_be_bytes());
        out.extend_from_slice(name.as_bytes());
    }

    /// One `{ pos: [x,y,z], state: i }` block entry payload.
    fn w_block(out: &mut Vec<u8>, pos: [i32; 3], state: i32) {
        w_int_list(out, "pos", &pos);
        w_i32(out, "state", state);
        out.push(0);
    }

    /// The canonical structure-document shape used by every fixture.
    fn structure_nbt(palette: &[(String, Vec<(String, String)>)], blocks: &[([i32; 3], i32)]) -> Vec<u8> {
        let mut out = vec![10, 0, 0]; // TAG_Compound, empty root name
        w_int_list(&mut out, "size", &[4, 4, 4]);
        w_list_header(&mut out, "palette", 10, palette.len() as i32);
        for (name, props) in palette {
            w_str(&mut out, "Name", name);
            if !props.is_empty() {
                w_compound_header(&mut out, "Properties");
                for (k, v) in props {
                    w_str(&mut out, k, v);
                }
                out.push(0);
            }
            out.push(0);
        }
        w_list_header(&mut out, "blocks", 10, blocks.len() as i32);
        for &(pos, state) in blocks {
            w_block(&mut out, pos, state);
        }
        out.push(0);
        out
    }

    /// RFC 1952 single-member wrapper over one stored deflate block.
    fn gzip_stored(data: &[u8]) -> Vec<u8> {
        assert!(data.len() <= 0xFFFF);
        let mut out = vec![0x1f, 0x8b, 0x08, 0, 0, 0, 0, 0, 0, 0x03];
        out.push(0x01); // BFINAL + stored block type
        let len = data.len() as u16;
        out.extend_from_slice(&len.to_le_bytes());
        out.extend_from_slice(&(!len).to_le_bytes());
        out.extend_from_slice(data);
        out.extend_from_slice(&crc32_ieee(data).to_le_bytes());
        out.extend_from_slice(&(data.len() as u32).to_le_bytes());
        out
    }

    #[test]
    fn parses_raw_and_gzipped_structure_identically() {
        let nbt = structure_nbt(
            &[
                (
                    "create:belt".to_string(),
                    vec![
                        ("slope".to_string(), "horizontal".to_string()),
                        ("facing".to_string(), "south".to_string()),
                    ],
                ),
                ("minecraft:stone".to_string(), Vec::new()),
                ("dirt".to_string(), Vec::new()),
            ],
            &[([0, 0, 0], 0), ([1, 0, 0], 1), ([2, 0, 0], 2)],
        );
        for bytes in [nbt.clone(), gzip_stored(&nbt)] {
            let structure = parse_nbt_schematic(&bytes).expect("structure decodes");
            assert_eq!(structure.size, [4, 4, 4]);
            assert_eq!(structure.palette.len(), 3);
            // Property keys sort into canonical order regardless of file order.
            assert_eq!(
                structure.palette[0].canonical,
                "create:belt[facing=south,slope=horizontal]"
            );
            // Namespace-less names adopt the `minecraft:` namespace.
            assert_eq!(structure.palette[2].canonical, "minecraft:dirt");
            assert_eq!(structure.palette[1].canonical, "minecraft:stone");
            assert_eq!(structure.blocks.len(), 3);
            assert_eq!(structure.blocks[2].state, 2);
        }
    }

    #[test]
    fn fail_closed_on_framing_and_dos_shapes() {
        let good = structure_nbt(&[("minecraft:stone".into(), vec![])], &[([0, 0, 0], 0)]);
        assert!(parse_nbt_schematic(&good).is_ok());

        // Corrupt gzip CRC trailer.
        let mut bad = gzip_stored(&good);
        let n = bad.len();
        bad[n - 5] ^= 0xFF;
        assert_eq!(
            parse_nbt_schematic(&bad),
            Err(SchematicError::GzipTrailerMismatch)
        );
        // Trailing garbage after the single gzip member.
        let mut bad = gzip_stored(&good);
        bad.push(0);
        assert_eq!(
            parse_nbt_schematic(&bad),
            Err(SchematicError::TrailingBytes)
        );
        // Truncated raw NBT.
        assert!(parse_nbt_schematic(&good[..good.len() - 2]).is_err());
        // Out-of-bounds palette index.
        let oob = structure_nbt(
            &[("minecraft:stone".into(), vec![])],
            &[([0, 0, 0], 3)],
        );
        assert_eq!(
            parse_nbt_schematic(&oob),
            Err(SchematicError::PaletteIndexOutOfBounds {
                index: 3,
                palette: 1
            })
        );
        // A raw input that is not a named-root compound is malformed.
        assert!(parse_nbt_schematic(b"\x07\x00\x04abc").is_err());
        // `size` is required.
        let mut no_size = vec![10, 0, 0];
        w_list_header(&mut no_size, "palette", 10, 0);
        w_list_header(&mut no_size, "blocks", 10, 0);
        no_size.push(0);
        assert_eq!(
            parse_nbt_schematic(&no_size),
            Err(SchematicError::MissingField("size"))
        );
    }
}
