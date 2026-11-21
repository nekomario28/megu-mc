// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later

//! Clean-room Java-edition resource-pack block & item model baker.
//!
//! A Java model file is JSON: an optional `parent` chain, a `textures` map of
//! `#alias` references, arbitrary 3D `elements` (boxes with per-face UV,
//! `cullface` and `tintindex`, optional single-axis rotation with `rescale`),
//! and item `overrides` driven by the `custom_model_data` predicate. Ordered
//! resource packs overlay each other per `ResourceLocation`: the highest
//! priority pack that defines a model wins, and its `parent` chain then walks
//! the whole stack. This module parses that format, resolves the overlay +
//! inheritance merge, and bakes [`BakedModelQuad`]s in block-local `0..=1`
//! space for the mesher/upload path.
//!
//! Every limit is fail-closed because a hostile 3D pack must never hang or
//! OOM the client:
//!
//! * [`MAX_JSON_DEPTH`] bounds the recursive JSON parser's stack use.
//! * [`MAX_PARENT_DEPTH`] bounds `parent` inheritance walks; cycles error out.
//! * [`MAX_ELEMENTS_PER_MODEL`] bounds quad emission per model.
//! * Texture `#alias` chains resolve under a visited set (cycle = error).
//! * All coordinates must be finite and inside Vanilla's `−16..=32` model-unit
//!   bounds; non-finite literals (e.g. `1e999`) are rejected by the parser.
//!
//! No Mojang or third-party texture data is referenced or bundled anywhere in
//! this file: texture *identifiers* pass through opaquely.

use std::collections::{BTreeMap, HashSet};
use std::error::Error;
use std::fmt;

use crate::section::Direction;

// ---------------------------------------------------------------------------
// Limits (fail-closed)
// ---------------------------------------------------------------------------

/// Hard cap on JSON nesting depth, shared shape with the other clean-room
/// parsers in this tree. 64 levels is far deeper than any legitimate model
/// file; a hostile document must not drive the recursive parser into
/// unbounded stack use.
pub const MAX_JSON_DEPTH: usize = 64;

/// Hard cap on `parent` inheritance chain length. Vanilla models are a few
/// levels deep; 32 is generous headroom and bounds the resolution walk.
pub const MAX_PARENT_DEPTH: usize = 32;

/// Hard cap on `elements[]` per model. Vanilla cubes use 1; complex models a
/// few dozen. Each element emits at most 6 quads, so 512 elements bounds a
/// baked model at 3072 quads.
pub const MAX_ELEMENTS_PER_MODEL: usize = 512;

/// Vanilla model-unit lower bound: elements may overhang the block by at
/// most one block (`−16` model units).
pub const MIN_MODEL_COORD: f32 = -16.0;
/// Vanilla model-unit upper bound: `32` model units = one block overhang.
pub const MAX_MODEL_COORD: f32 = 32.0;
/// Vanilla model units per block: `from`/`to`/UV coordinates are authored in
/// sixteenths of a block.
pub const MODEL_UNITS_PER_BLOCK: f32 = 16.0;

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Parse offset + message for a JSON-level failure.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JsonError {
    pub offset: usize,
    pub message: String,
}

impl fmt::Display for JsonError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "JSON error at byte {}: {}", self.offset, self.message)
    }
}

/// Every way loading or baking a model can fail. All variants are terminal:
/// the caller drops the model rather than guessing a fallback.
#[derive(Clone, Debug, PartialEq)]
pub enum ModelError {
    /// Malformed JSON (including nesting deeper than [`MAX_JSON_DEPTH`]).
    Json(JsonError),
    /// Well-formed JSON that does not match the model schema (missing
    /// `from`, non-array `elements`, unknown `cullface`, ...).
    Schema(String),
    /// No pack in the stack defines the requested location.
    MissingModel(String),
    /// A `parent` chain loops back on itself.
    ParentCycle(String),
    /// A `parent` chain exceeded [`MAX_PARENT_DEPTH`].
    ParentDepthExceeded,
    /// A `textures` `#alias` chain loops (`#a -> #b -> #a`).
    TextureAliasCycle(String),
    /// A `#alias` has no entry in the merged texture map.
    UnresolvedTextureAlias(String),
    /// More than [`MAX_ELEMENTS_PER_MODEL`] elements in one model.
    TooManyElements(usize),
    /// A coordinate outside `MIN_MODEL_COORD..=MAX_MODEL_COORD`, or
    /// `from > to` on an axis.
    OutOfRangeCoordinate(String),
    /// A non-finite value reached the baker (a programmatic `NaN`; the JSON
    /// parser already refuses non-finite literals).
    NonFiniteValue(String),
    /// A blockstate rotation that is not a multiple of 90 degrees.
    InvalidRotationAngle(String),
}

impl fmt::Display for ModelError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Json(e) => e.fmt(f),
            Self::Schema(m) => write!(f, "model schema error: {m}"),
            Self::MissingModel(m) => write!(f, "no resource pack defines model {m:?}"),
            Self::ParentCycle(m) => write!(f, "parent inheritance cycle at {m:?}"),
            Self::ParentDepthExceeded => {
                write!(f, "parent chain deeper than {MAX_PARENT_DEPTH} models")
            }
            Self::TextureAliasCycle(k) => write!(f, "texture alias cycle at {k:?}"),
            Self::UnresolvedTextureAlias(k) => write!(f, "unresolved texture alias {k:?}"),
            Self::TooManyElements(n) => {
                write!(f, "{n} elements exceeds limit of {MAX_ELEMENTS_PER_MODEL}")
            }
            Self::OutOfRangeCoordinate(m) => write!(f, "coordinate out of range: {m}"),
            Self::NonFiniteValue(m) => write!(f, "non-finite value: {m}"),
            Self::InvalidRotationAngle(m) => write!(f, "invalid rotation: {m}"),
        }
    }
}

impl Error for ModelError {}
impl Error for JsonError {}

// ---------------------------------------------------------------------------
// Clean-room JSON value + parser
// ---------------------------------------------------------------------------

/// Parsed JSON value. Numbers keep `f64`: model files only need finite
/// doubles, and integer fields validate their range when they are read.
#[derive(Clone, Debug, PartialEq)]
enum Json {
    Null,
    Bool(bool),
    Num(f64),
    Str(String),
    Arr(Vec<Json>),
    Obj(BTreeMap<String, Json>),
}

impl Json {
    fn as_object(&self) -> Option<&BTreeMap<String, Json>> {
        match self {
            Self::Obj(m) => Some(m),
            _ => None,
        }
    }

    fn as_array(&self) -> Option<&[Json]> {
        match self {
            Self::Arr(v) => Some(v),
            _ => None,
        }
    }

    fn as_str(&self) -> Option<&str> {
        match self {
            Self::Str(s) => Some(s),
            _ => None,
        }
    }

    fn as_f64(&self) -> Option<f64> {
        match self {
            Self::Num(n) => Some(*n),
            _ => None,
        }
    }

    fn as_bool(&self) -> Option<bool> {
        match self {
            Self::Bool(b) => Some(*b),
            _ => None,
        }
    }
}

struct JsonParser<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> JsonParser<'a> {
    fn err(&self, message: impl Into<String>) -> JsonError {
        JsonError {
            offset: self.pos,
            message: message.into(),
        }
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    fn skip_ws(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.pos += 1;
        }
    }

    fn literal(&mut self, literal: &'static [u8], value: Json) -> Result<Json, JsonError> {
        if self.bytes[self.pos..].starts_with(literal) {
            self.pos += literal.len();
            Ok(value)
        } else {
            Err(self.err("expected a JSON value"))
        }
    }

    fn value(&mut self, depth: usize) -> Result<Json, JsonError> {
        self.skip_ws();
        match self.peek() {
            None => Err(self.err("expected a JSON value")),
            Some(b'{') => self.object(depth),
            Some(b'[') => self.array(depth),
            Some(b'"') => Ok(Json::Str(self.string()?)),
            Some(b't') => self.literal(b"true", Json::Bool(true)),
            Some(b'f') => self.literal(b"false", Json::Bool(false)),
            Some(b'n') => self.literal(b"null", Json::Null),
            Some(b'-' | b'0'..=b'9') => self.number(),
            Some(_) => Err(self.err("expected a JSON value")),
        }
    }

    /// Entering a container at `depth` enclosing containers: the cap is
    /// checked here so scalars at maximum depth still parse.
    fn container_open(&self, depth: usize) -> Result<(), JsonError> {
        if depth >= MAX_JSON_DEPTH {
            return Err(self.err(format!("JSON nesting deeper than {MAX_JSON_DEPTH} levels")));
        }
        Ok(())
    }

    fn object(&mut self, depth: usize) -> Result<Json, JsonError> {
        self.container_open(depth)?;
        self.pos += 1; // '{'
        let mut map = BTreeMap::new();
        self.skip_ws();
        if self.peek() == Some(b'}') {
            self.pos += 1;
            return Ok(Json::Obj(map));
        }
        loop {
            self.skip_ws();
            if self.peek() != Some(b'"') {
                return Err(self.err("expected an object key"));
            }
            let key = self.string()?;
            self.skip_ws();
            if self.peek() != Some(b':') {
                return Err(self.err("expected ':' after object key"));
            }
            self.pos += 1;
            let value = self.value(depth + 1)?;
            map.insert(key, value);
            self.skip_ws();
            match self.peek() {
                Some(b',') => self.pos += 1,
                Some(b'}') => {
                    self.pos += 1;
                    return Ok(Json::Obj(map));
                }
                _ => return Err(self.err("expected ',' or '}' in object")),
            }
        }
    }

    fn array(&mut self, depth: usize) -> Result<Json, JsonError> {
        self.container_open(depth)?;
        self.pos += 1; // '['
        let mut items = Vec::new();
        self.skip_ws();
        if self.peek() == Some(b']') {
            self.pos += 1;
            return Ok(Json::Arr(items));
        }
        loop {
            items.push(self.value(depth + 1)?);
            self.skip_ws();
            match self.peek() {
                Some(b',') => self.pos += 1,
                Some(b']') => {
                    self.pos += 1;
                    return Ok(Json::Arr(items));
                }
                _ => return Err(self.err("expected ',' or ']' in array")),
            }
        }
    }

    /// A JSON string starting at the opening quote. Byte-level scanning is
    /// safe: the input is a `&str` (already UTF-8), and the only bytes with
    /// syntactic meaning — `"`, `\` and control bytes below 0x20 — can never
    /// appear inside a multi-byte UTF-8 sequence, so runs copied between
    /// them stay valid UTF-8.
    fn string(&mut self) -> Result<String, JsonError> {
        self.pos += 1; // opening '"'
        let mut out = String::new();
        loop {
            let run_start = self.pos;
            while let Some(b) = self.peek() {
                if b == b'"' || b == b'\\' || b < 0x20 {
                    break;
                }
                self.pos += 1;
            }
            // The slice is valid UTF-8 by the argument above; a failure here
            // would mean the scanner split a multi-byte sequence, which is
            // an internal bug — still reported, never unwrapped.
            let run = std::str::from_utf8(&self.bytes[run_start..self.pos])
                .map_err(|_| self.err("invalid UTF-8 in string"))?;
            out.push_str(run);
            match self.peek() {
                None => return Err(self.err("unterminated string")),
                Some(b'"') => {
                    self.pos += 1;
                    return Ok(out);
                }
                Some(b'\\') => {
                    self.pos += 1;
                    self.escape(&mut out)?;
                }
                Some(_) => return Err(self.err("unescaped control character in string")),
            }
        }
    }

    /// The byte after `\`. `\uXXXX` sequences are combined into scalar
    /// values; a lone surrogate is malformed JSON for this parser's purposes
    /// and refused rather than lossily replaced.
    fn escape(&mut self, out: &mut String) -> Result<(), JsonError> {
        match self.peek() {
            Some(b'"') => out.push('"'),
            Some(b'\\') => out.push('\\'),
            Some(b'/') => out.push('/'),
            Some(b'b') => out.push('\u{8}'),
            Some(b'f') => out.push('\u{c}'),
            Some(b'n') => out.push('\n'),
            Some(b'r') => out.push('\r'),
            Some(b't') => out.push('\t'),
            Some(b'u') => {
                self.pos += 1;
                let hi = self.hex4()?;
                let cp = if (0xD800..=0xDBFF).contains(&hi) {
                    // A high surrogate must be followed by \uDC00-\uDFFF.
                    if self.peek() == Some(b'\\') {
                        self.pos += 1;
                    }
                    if self.peek() != Some(b'u') {
                        return Err(self.err("lone high surrogate in \\u escape"));
                    }
                    self.pos += 1;
                    let lo = self.hex4()?;
                    if !(0xDC00..=0xDFFF).contains(&lo) {
                        return Err(self.err("expected low surrogate after \\uXXXX"));
                    }
                    0x1_0000 + ((hi - 0xD800) << 10) + (lo - 0xDC00)
                } else if (0xDC00..=0xDFFF).contains(&hi) {
                    return Err(self.err("lone low surrogate in \\u escape"));
                } else {
                    hi
                };
                out.push(char::from_u32(cp).ok_or_else(|| self.err("invalid \\u escape"))?);
                return Ok(());
            }
            _ => return Err(self.err("invalid escape sequence")),
        }
        self.pos += 1;
        Ok(())
    }

    fn hex4(&mut self) -> Result<u32, JsonError> {
        let mut value = 0u32;
        for _ in 0..4 {
            let b = self
                .peek()
                .ok_or_else(|| self.err("truncated \\u escape"))?;
            let digit = match b {
                b'0'..=b'9' => b - b'0',
                b'a'..=b'f' => b - b'a' + 10,
                b'A'..=b'F' => b - b'A' + 10,
                _ => return Err(self.err("non-hex digit in \\u escape")),
            };
            value = value * 16 + u32::from(digit);
            self.pos += 1;
        }
        Ok(value)
    }

    /// `-?(0|[1-9][0-9]*)(\.[0-9]+)?([eE][+-]?[0-9]+)?` — the JSON number
    /// grammar, kept strict. The parsed value must stay finite: `1e999`
    /// overflows to infinity and is rejected rather than silently baking an
    /// infinite coordinate.
    fn number(&mut self) -> Result<Json, JsonError> {
        let start = self.pos;
        if self.peek() == Some(b'-') {
            self.pos += 1;
        }
        match self.peek() {
            Some(b'0') => {
                self.pos += 1;
                if matches!(self.peek(), Some(b'0'..=b'9')) {
                    return Err(self.err("leading zeros are not valid JSON"));
                }
            }
            Some(b'1'..=b'9') => {
                while matches!(self.peek(), Some(b'0'..=b'9')) {
                    self.pos += 1;
                }
            }
            _ => return Err(self.err("expected a digit")),
        }
        if self.peek() == Some(b'.') {
            self.pos += 1;
            if !matches!(self.peek(), Some(b'0'..=b'9')) {
                return Err(self.err("expected a digit after '.'"));
            }
            while matches!(self.peek(), Some(b'0'..=b'9')) {
                self.pos += 1;
            }
        }
        if matches!(self.peek(), Some(b'e') | Some(b'E')) {
            self.pos += 1;
            if matches!(self.peek(), Some(b'+') | Some(b'-')) {
                self.pos += 1;
            }
            if !matches!(self.peek(), Some(b'0'..=b'9')) {
                return Err(self.err("expected a digit in the exponent"));
            }
            while matches!(self.peek(), Some(b'0'..=b'9')) {
                self.pos += 1;
            }
        }
        // The accepted grammar is pure ASCII, so this slice is always &str.
        let text = std::str::from_utf8(&self.bytes[start..self.pos])
            .map_err(|_| self.err("invalid number"))?;
        let value: f64 = text.parse().map_err(|_| self.err("invalid number"))?;
        if !value.is_finite() {
            return Err(self.err("number is out of range"));
        }
        Ok(Json::Num(value))
    }
}

fn parse_json(text: &str) -> Result<Json, JsonError> {
    let mut parser = JsonParser {
        bytes: text.as_bytes(),
        pos: 0,
    };
    let value = parser.value(0)?;
    parser.skip_ws();
    if parser.pos != parser.bytes.len() {
        return Err(parser.err("trailing bytes after JSON value"));
    }
    Ok(value)
}

// ---------------------------------------------------------------------------
// Raw model data model
// ---------------------------------------------------------------------------

/// Rotation axis for a [`ModelElement`] rotation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Axis {
    X,
    Y,
    Z,
}

/// One face of a [`ModelElement`]. Stored per [`Direction`] index in
/// [`ModelElement::faces`]: `faces[Direction::Down as usize]` is the `down`
/// JSON face.
#[derive(Clone, Debug, PartialEq)]
pub struct ElementFace {
    /// `[u0, v0, u1, v1]` in model units (`0..16` texture space). `None`
    /// derives the rectangle from the element bounds projected onto the face,
    /// exactly like Vanilla.
    pub uv: Option<[f32; 4]>,
    /// Texture id (`namespace:path`) or `#alias` into the model's texture
    /// map. Aliases resolve through the merged parent-chain map at bake time.
    pub texture: String,
    /// Face index this face culls against (`Direction` discriminant), or
    /// `None` for no culling.
    pub cullface: Option<u8>,
    /// In-face UV rotation; Vanilla only allows 0/90/180/270.
    pub rotation_deg: u16,
    /// Tint layer; `-1` (the default) means untinted.
    pub tintindex: i32,
}

impl ElementFace {
    /// The [`Direction`] this face culls against, if any.
    pub fn cull_direction(&self) -> Option<Direction> {
        self.cullface
            .and_then(|index| Direction::ALL.get(index as usize).copied())
    }
}

/// A single-axis element rotation: `angle_deg` degrees about `axis`, around
/// `origin` (model units). `rescale` applies Vanilla's `1/cos(angle)` scale on
/// the two axes orthogonal to `axis` so 22.5°/45°-rotated bars keep their
/// diagonal span.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ElementRotation {
    pub origin: [f32; 3],
    pub axis: Axis,
    pub angle_deg: f32,
    pub rescale: bool,
}

/// One cuboid (`elements[]` entry). `from`/`to` are model units; Vanilla
/// allows the `−16..=32` overhang bounds ([`MIN_MODEL_COORD`],
/// [`MAX_MODEL_COORD`]). `shade` defaults to `true`.
#[derive(Clone, Debug, PartialEq)]
pub struct ModelElement {
    pub from: [f32; 3],
    pub to: [f32; 3],
    pub rotation: Option<ElementRotation>,
    pub shade: bool,
    /// The six faces in [`Direction`] order (`faces[d as usize]`); absent
    /// faces emit no quad.
    pub faces: [Option<ElementFace>; 6],
}

/// One item-model `overrides` entry. Only the `custom_model_data` predicate
/// is modelled; an override carries the model location to use when it wins.
#[derive(Clone, Debug, PartialEq)]
pub struct ItemModelOverride {
    /// `predicate.custom_model_data` threshold, if present. Vanilla matches a
    /// predicate when `threshold <= actual value`.
    pub custom_model_data: Option<f64>,
    /// Model location to bake when this override wins.
    pub model: String,
}

/// A model JSON file as authored in a resource pack: `parent`, texture
/// `#alias` map, optional `elements`, and item `overrides`. Unresolved — see
/// [`ResourcePackModelStack`] for the multi-pack + parent merge.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RawModelJson {
    pub parent: Option<String>,
    pub ambientocclusion: Option<bool>,
    pub textures: BTreeMap<String, String>,
    /// `Some` (even `Some([])`) replaces the parent's elements wholesale, per
    /// Vanilla semantics; `None` inherits.
    pub elements: Option<Vec<ModelElement>>,
    pub overrides: Vec<ItemModelOverride>,
}

// ---------------------------------------------------------------------------
// Model JSON schema mapping
// ---------------------------------------------------------------------------

fn schema_err(message: impl Into<String>) -> ModelError {
    ModelError::Schema(message.into())
}

/// Reads `value` as `f32`, requiring the result to stay finite. An `f64`
/// literal such as `1e40` parses fine but overflows `f32` to infinity —
/// caught here instead of baking an infinite vertex.
fn finite_f32(value: &Json, field: &str) -> Result<f32, ModelError> {
    let n = value
        .as_f64()
        .ok_or_else(|| schema_err(format!("{field} must be a number")))?;
    let f = n as f32;
    if !f.is_finite() {
        return Err(ModelError::NonFiniteValue(format!("{field} overflows f32")));
    }
    Ok(f)
}

fn bounded_f32(value: &Json, field: &str) -> Result<f32, ModelError> {
    let f = finite_f32(value, field)?;
    if !(MIN_MODEL_COORD..=MAX_MODEL_COORD).contains(&f) {
        return Err(ModelError::OutOfRangeCoordinate(format!(
            "{field} = {f} is outside {MIN_MODEL_COORD}..={MAX_MODEL_COORD}"
        )));
    }
    Ok(f)
}

/// Reads a JSON array of `N` numbers as `[f32; N]` via `read`.
fn f32_array<const N: usize>(
    value: &Json,
    field: &str,
    read: fn(&Json, &str) -> Result<f32, ModelError>,
) -> Result<[f32; N], ModelError> {
    let items = value
        .as_array()
        .ok_or_else(|| schema_err(format!("{field} must be an array of {N} numbers")))?;
    if items.len() != N {
        return Err(schema_err(format!(
            "{field} must have {N} numbers, got {}",
            items.len()
        )));
    }
    let mut out = [0.0; N];
    for (i, item) in items.iter().enumerate() {
        out[i] = read(item, field)?;
    }
    Ok(out)
}

/// The six JSON face names, in [`Direction`] order.
const FACE_NAMES: [&str; 6] = ["down", "up", "north", "south", "west", "east"];

fn face_index(name: &str) -> Option<usize> {
    FACE_NAMES.iter().position(|n| *n == name)
}

fn parse_element_face(value: &Json, field: &str) -> Result<ElementFace, ModelError> {
    let obj = value
        .as_object()
        .ok_or_else(|| schema_err(format!("{field} must be an object")))?;
    let texture = obj
        .get("texture")
        .and_then(Json::as_str)
        .ok_or_else(|| schema_err(format!("{field}.texture must be a string")))?
        .to_owned();
    if texture.is_empty() {
        return Err(schema_err(format!("{field}.texture is empty")));
    }
    let uv = match obj.get("uv") {
        None | Some(Json::Null) => None,
        Some(v) => Some(f32_array(v, &format!("{field}.uv"), finite_f32)?),
    };
    let cullface = match obj.get("cullface") {
        None | Some(Json::Null) => None,
        Some(Json::Str(name)) => Some(
            face_index(name)
                .ok_or_else(|| schema_err(format!("{field}.cullface is not a direction")))?
                as u8,
        ),
        Some(_) => return Err(schema_err(format!("{field}.cullface must be a string"))),
    };
    let rotation_deg = match obj.get("rotation") {
        None | Some(Json::Null) => 0,
        Some(v) => {
            let n = v
                .as_f64()
                .ok_or_else(|| schema_err(format!("{field}.rotation must be a number")))?;
            if n.fract() != 0.0 || !(0.0..=270.0).contains(&n) || !(n as u16).is_multiple_of(90) {
                return Err(schema_err(format!(
                    "{field}.rotation must be 0, 90, 180 or 270"
                )));
            }
            n as u16
        }
    };
    let tintindex = match obj.get("tintindex") {
        None | Some(Json::Null) => -1,
        Some(v) => {
            let n = v
                .as_f64()
                .ok_or_else(|| schema_err(format!("{field}.tintindex must be a number")))?;
            if n.fract() != 0.0 || n < f64::from(i32::MIN) || n > f64::from(i32::MAX) {
                return Err(schema_err(format!("{field}.tintindex must be an integer")));
            }
            n as i32
        }
    };
    Ok(ElementFace {
        uv,
        texture,
        cullface,
        rotation_deg,
        tintindex,
    })
}

fn parse_element(value: &Json, index: usize) -> Result<ModelElement, ModelError> {
    let field = format!("elements[{index}]");
    let obj = value
        .as_object()
        .ok_or_else(|| schema_err(format!("{field} must be an object")))?;
    let from = f32_array(
        obj.get("from")
            .ok_or_else(|| schema_err(format!("{field}.from is required")))?,
        &format!("{field}.from"),
        bounded_f32,
    )?;
    let to = f32_array(
        obj.get("to")
            .ok_or_else(|| schema_err(format!("{field}.to is required")))?,
        &format!("{field}.to"),
        bounded_f32,
    )?;
    let rotation = match obj.get("rotation") {
        None | Some(Json::Null) => None,
        Some(v) => {
            let robj = v
                .as_object()
                .ok_or_else(|| schema_err(format!("{field}.rotation must be an object")))?;
            let rfield = format!("{field}.rotation");
            let origin = f32_array(
                robj.get("origin")
                    .ok_or_else(|| schema_err(format!("{rfield}.origin is required")))?,
                &format!("{rfield}.origin"),
                bounded_f32,
            )?;
            let axis = match robj.get("axis").and_then(Json::as_str) {
                Some("x") => Axis::X,
                Some("y") => Axis::Y,
                Some("z") => Axis::Z,
                _ => return Err(schema_err(format!("{rfield}.axis must be x, y or z"))),
            };
            let angle_deg = finite_f32(
                robj.get("angle")
                    .ok_or_else(|| schema_err(format!("{rfield}.angle is required")))?,
                &format!("{rfield}.angle"),
            )?;
            let rescale = robj.get("rescale").and_then(Json::as_bool).unwrap_or(false);
            Some(ElementRotation {
                origin,
                axis,
                angle_deg,
                rescale,
            })
        }
    };
    let shade = obj.get("shade").and_then(Json::as_bool).unwrap_or(true);
    let faces_obj = obj
        .get("faces")
        .and_then(Json::as_object)
        .ok_or_else(|| schema_err(format!("{field}.faces must be an object")))?;
    let mut faces: [Option<ElementFace>; 6] = Default::default();
    for (name, face_value) in faces_obj {
        let Some(index) = face_index(name) else {
            continue; // Unknown keys are ignored, like Vanilla's lenient Gson read.
        };
        faces[index] = Some(parse_element_face(
            face_value,
            &format!("{field}.faces.{name}"),
        )?);
    }
    Ok(ModelElement {
        from,
        to,
        rotation,
        shade,
        faces,
    })
}

fn parse_override(value: &Json, index: usize) -> Result<ItemModelOverride, ModelError> {
    let field = format!("overrides[{index}]");
    let obj = value
        .as_object()
        .ok_or_else(|| schema_err(format!("{field} must be an object")))?;
    let model = obj
        .get("model")
        .and_then(Json::as_str)
        .ok_or_else(|| schema_err(format!("{field}.model must be a string")))?
        .to_owned();
    let custom_model_data = obj
        .get("predicate")
        .and_then(Json::as_object)
        .and_then(|p| p.get("custom_model_data"))
        .and_then(Json::as_f64);
    Ok(ItemModelOverride {
        custom_model_data,
        model,
    })
}

impl RawModelJson {
    /// Parses one `assets/<ns>/models/**/*.json` document. The result is
    /// *raw*: `parent` chains and `#alias` textures resolve through a
    /// [`ResourcePackModelStack`].
    pub fn parse(text: &str) -> Result<Self, ModelError> {
        let root = parse_json(text).map_err(ModelError::Json)?;
        let obj = root
            .as_object()
            .ok_or_else(|| schema_err("model root must be a JSON object"))?;
        let parent = match obj.get("parent") {
            None | Some(Json::Null) => None,
            Some(v) => Some(
                v.as_str()
                    .ok_or_else(|| schema_err("parent must be a string"))?
                    .to_owned(),
            ),
        };
        let ambientocclusion = match obj.get("ambientocclusion") {
            None | Some(Json::Null) => None,
            Some(v) => Some(
                v.as_bool()
                    .ok_or_else(|| schema_err("ambientocclusion must be a boolean"))?,
            ),
        };
        let mut textures = BTreeMap::new();
        if let Some(value) = obj.get("textures") {
            let map = value
                .as_object()
                .ok_or_else(|| schema_err("textures must be an object"))?;
            for (key, value) in map {
                textures.insert(
                    key.clone(),
                    value
                        .as_str()
                        .ok_or_else(|| schema_err("texture values must be strings"))?
                        .to_owned(),
                );
            }
        }
        let elements = match obj.get("elements") {
            None | Some(Json::Null) => None,
            Some(v) => {
                let items = v
                    .as_array()
                    .ok_or_else(|| schema_err("elements must be an array"))?;
                if items.len() > MAX_ELEMENTS_PER_MODEL {
                    return Err(ModelError::TooManyElements(items.len()));
                }
                let mut parsed = Vec::with_capacity(items.len());
                for (index, item) in items.iter().enumerate() {
                    parsed.push(parse_element(item, index)?);
                }
                Some(parsed)
            }
        };
        let mut overrides = Vec::new();
        if let Some(value) = obj.get("overrides") {
            let items = value
                .as_array()
                .ok_or_else(|| schema_err("overrides must be an array"))?;
            for (index, item) in items.iter().enumerate() {
                overrides.push(parse_override(item, index)?);
            }
        }
        Ok(Self {
            parent,
            ambientocclusion,
            textures,
            elements,
            overrides,
        })
    }
}

// ---------------------------------------------------------------------------
// Multi-pack stack + parent resolution
// ---------------------------------------------------------------------------

/// A `ResourceLocation` normalised for map lookup: a bare `block/cube`
/// parent reference means `minecraft:block/cube` per Vanilla.
fn normalize_location(raw: &str) -> String {
    if raw.contains(':') {
        raw.to_owned()
    } else {
        format!("minecraft:{raw}")
    }
}

/// Ordered resource packs: index 0 is the lowest priority, the last pack
/// pushed is the highest. For each `ResourceLocation` the highest pack that
/// defines the model wins (a user's 3D pack overlays the vanilla base); the
/// winning file's `parent` chain then resolves against the whole stack.
#[derive(Clone, Debug, Default)]
pub struct ResourcePackModelStack {
    packs: Vec<BTreeMap<String, RawModelJson>>,
}

impl ResourcePackModelStack {
    pub fn new() -> Self {
        Self::default()
    }

    /// Pushes one pack. Later pushes have higher priority. Model ids are
    /// normalised on the way in so `block/cube` and `minecraft:block/cube`
    /// name the same location.
    pub fn push_pack(&mut self, models: BTreeMap<String, RawModelJson>) {
        let normalized = models
            .into_iter()
            .map(|(id, model)| (normalize_location(&id), model))
            .collect();
        self.packs.push(normalized);
    }

    /// Builder-style [`push_pack`](Self::push_pack).
    #[must_use]
    pub fn with_pack(mut self, models: BTreeMap<String, RawModelJson>) -> Self {
        self.push_pack(models);
        self
    }

    /// The highest-priority definition of `model_id` in the stack.
    pub fn get(&self, model_id: &str) -> Option<&RawModelJson> {
        let id = normalize_location(model_id);
        self.packs.iter().rev().find_map(|pack| pack.get(&id))
    }

    /// Walks the `parent` chain from `model_id` and merges per Vanilla:
    /// child `textures` keys override the parent's; the child-most model
    /// that defines `elements` supplies all of them (`elements` replaces
    /// wholesale, it never merges); `ambientocclusion`/`overrides` come from
    /// the child-most model that defines them (overrides from the leaf).
    fn resolve(&self, model_id: &str) -> Result<ResolvedModel<'_>, ModelError> {
        let mut chain: Vec<&RawModelJson> = Vec::new();
        let mut visited: HashSet<String> = HashSet::new();
        let mut current = normalize_location(model_id);
        loop {
            if !visited.insert(current.clone()) {
                return Err(ModelError::ParentCycle(current));
            }
            if chain.len() >= MAX_PARENT_DEPTH {
                return Err(ModelError::ParentDepthExceeded);
            }
            let model = self
                .get(&current)
                .ok_or_else(|| ModelError::MissingModel(current.clone()))?;
            chain.push(model);
            match &model.parent {
                Some(parent) => current = normalize_location(parent),
                None => break,
            }
        }
        // Root first so child keys overwrite in `insert` order.
        let mut textures = BTreeMap::new();
        for model in chain.iter().rev() {
            for (key, value) in &model.textures {
                textures.insert(key.clone(), value.clone());
            }
        }
        let elements = chain
            .iter()
            .find_map(|model| model.elements.as_deref())
            .unwrap_or(&[]);
        if elements.len() > MAX_ELEMENTS_PER_MODEL {
            return Err(ModelError::TooManyElements(elements.len()));
        }
        let ambientocclusion = chain
            .iter()
            .find_map(|model| model.ambientocclusion)
            .unwrap_or(true);
        Ok(ResolvedModel {
            textures,
            elements,
            ambientocclusion,
        })
    }

    /// Resolves a `textures` entry or face `texture` value: `#alias` chains
    /// walk the merged map under a visited set so `#a -> #b -> #a` fails
    /// closed instead of looping forever.
    fn resolve_texture(
        textures: &BTreeMap<String, String>,
        reference: &str,
    ) -> Result<String, ModelError> {
        let Some(rest) = reference.strip_prefix('#') else {
            return Ok(reference.to_owned());
        };
        let mut visited: HashSet<&str> = HashSet::new();
        let mut key = rest;
        loop {
            if !visited.insert(key) {
                return Err(ModelError::TextureAliasCycle(key.to_owned()));
            }
            let value = textures
                .get(key)
                .ok_or_else(|| ModelError::UnresolvedTextureAlias(key.to_owned()))?;
            match value.strip_prefix('#') {
                Some(next) => key = next,
                None => return Ok(value.clone()),
            }
        }
    }
}

/// The merged view of one model id after pack overlay + `parent` merge.
struct ResolvedModel<'a> {
    textures: BTreeMap<String, String>,
    elements: &'a [ModelElement],
    ambientocclusion: bool,
}

// ---------------------------------------------------------------------------
// Baking
// ---------------------------------------------------------------------------

/// One baked quad: four outward-counterclockwise vertex positions in
/// block-local `0..=1` space after element + blockstate rotation, the
/// rotated face normal, resolved texture id, four UVs in vanilla `0..16`
/// texture units (with in-face `rotation` applied), the blockstate-rotated
/// `cullface` direction, `tintindex`, and the element `shade` flag.
#[derive(Clone, Debug, PartialEq)]
pub struct BakedModelQuad {
    pub positions: [[f32; 3]; 4],
    pub normal: [f32; 3],
    pub texture: String,
    pub uv: [[f32; 2]; 4],
    pub cullface: Option<Direction>,
    pub tintindex: i32,
    pub shade: bool,
}

/// A fully baked model: ambient-occlusion flag plus quads in element order.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct BakedModel {
    pub ambient_occlusion: bool,
    pub quads: Vec<BakedModelQuad>,
}

/// Corner table: each of the six faces' four vertices as `[axis -> which
/// bound]` indices (`0` = `from`, `1` = `to`), in outward counterclockwise
/// order — the same winding the section mesher emits.
const FACE_CORNERS: [[[u8; 3]; 4]; 6] = [
    // down (y = from.y), viewed from below
    [[0, 0, 0], [1, 0, 0], [1, 0, 1], [0, 0, 1]],
    // up (y = to.y), viewed from above
    [[0, 1, 1], [1, 1, 1], [1, 1, 0], [0, 1, 0]],
    // north (z = from.z), viewed from -z
    [[1, 0, 0], [0, 0, 0], [0, 1, 0], [1, 1, 0]],
    // south (z = to.z), viewed from +z
    [[0, 0, 1], [1, 0, 1], [1, 1, 1], [0, 1, 1]],
    // west (x = from.x), viewed from -x
    [[0, 0, 0], [0, 0, 1], [0, 1, 1], [0, 1, 0]],
    // east (x = to.x), viewed from +x
    [[1, 0, 1], [1, 0, 0], [1, 1, 0], [1, 1, 1]],
];

/// UV rectangle corners as `(u, v)` index pairs: `0=(u0,v0)`, `1=(u1,v0)`,
/// `2=(u1,v1)`, `3=(u0,v1)`. These map the default-UV projections (below)
/// onto the corner order above.
const FACE_UV_CORNER: [[usize; 4]; 6] = [
    [0, 1, 2, 3], // down
    [3, 2, 1, 0], // up
    [3, 2, 1, 0], // north
    [3, 2, 1, 0], // south
    [3, 2, 1, 0], // west
    [3, 2, 1, 0], // east
];

const UV_PAIRS: [(usize, usize); 4] = [(0, 1), (2, 1), (2, 3), (0, 3)];

/// Vanilla's default UV rectangle for a face (`[u0, v0, u1, v1]`) when the
/// JSON omits `uv`: the element bounds projected onto the face plane.
fn default_uv(direction: Direction, from: [f32; 3], to: [f32; 3]) -> [f32; 4] {
    match direction {
        Direction::Down | Direction::Up => [from[0], from[2], to[0], to[2]],
        Direction::North => [
            MODEL_UNITS_PER_BLOCK - to[0],
            MODEL_UNITS_PER_BLOCK - to[1],
            MODEL_UNITS_PER_BLOCK - from[0],
            MODEL_UNITS_PER_BLOCK - from[1],
        ],
        Direction::South => [
            from[0],
            MODEL_UNITS_PER_BLOCK - to[1],
            to[0],
            MODEL_UNITS_PER_BLOCK - from[1],
        ],
        Direction::West => [
            from[2],
            MODEL_UNITS_PER_BLOCK - to[1],
            to[2],
            MODEL_UNITS_PER_BLOCK - from[1],
        ],
        Direction::East => [
            MODEL_UNITS_PER_BLOCK - to[2],
            MODEL_UNITS_PER_BLOCK - to[1],
            MODEL_UNITS_PER_BLOCK - from[2],
            MODEL_UNITS_PER_BLOCK - from[1],
        ],
    }
}

fn direction_vector(direction: Direction) -> [f32; 3] {
    match direction {
        Direction::Down => [0.0, -1.0, 0.0],
        Direction::Up => [0.0, 1.0, 0.0],
        Direction::North => [0.0, 0.0, -1.0],
        Direction::South => [0.0, 0.0, 1.0],
        Direction::West => [-1.0, 0.0, 0.0],
        Direction::East => [1.0, 0.0, 0.0],
    }
}

fn direction_of(v: [f32; 3]) -> Option<Direction> {
    for direction in Direction::ALL {
        let d = direction_vector(direction);
        if (v[0] - d[0]).abs() < 0.5 && (v[1] - d[1]).abs() < 0.5 && (v[2] - d[2]).abs() < 0.5 {
            return Some(direction);
        }
    }
    None
}

/// Rotates `p` about `origin` on `axis` by `angle` radians (right-hand
/// rule), matching Vanilla's `rotateAxis(angle, axis)` element rotation.
fn rotate_axis(p: [f32; 3], origin: [f32; 3], axis: Axis, angle: f32) -> [f32; 3] {
    let (sin, cos) = angle.sin_cos();
    let d = [p[0] - origin[0], p[1] - origin[1], p[2] - origin[2]];
    let r = match axis {
        Axis::X => [d[0], d[1] * cos - d[2] * sin, d[1] * sin + d[2] * cos],
        Axis::Y => [d[0] * cos + d[2] * sin, d[1], d[2] * cos - d[0] * sin],
        Axis::Z => [d[0] * cos - d[1] * sin, d[0] * sin + d[1] * cos, d[2]],
    };
    [r[0] + origin[0], r[1] + origin[1], r[2] + origin[2]]
}

/// Rotates a vector by `angle` radians about `axis` (no origin — for
/// normals and direction vectors).
fn rotate_axis_vec(v: [f32; 3], axis: Axis, angle: f32) -> [f32; 3] {
    rotate_axis(v, [0.0; 3], axis, angle)
}

/// Vanilla `rescale`: scales the two axes orthogonal to the rotation axis
/// by `1/cos(angle)` about the origin, applied before the rotation — a 45°
/// bar rescales by √2 so its rotated silhouette keeps the original extent.
fn apply_rescale(p: [f32; 3], origin: [f32; 3], axis: Axis, scale: f32) -> [f32; 3] {
    let scaled = |v: f32, o: f32| o + (v - o) * scale;
    match axis {
        Axis::X => [p[0], scaled(p[1], origin[1]), scaled(p[2], origin[2])],
        Axis::Y => [scaled(p[0], origin[0]), p[1], scaled(p[2], origin[2])],
        Axis::Z => [scaled(p[0], origin[0]), scaled(p[1], origin[1]), p[2]],
    }
}

/// One −90° step of the blockstate `"x"` rotation about the block centre:
/// `(x, y, z) -> (x, z, -y)` in centred coordinates. Vanilla composes its
/// variant rotation as `rotationYXZ(-y, -x, 0)`, i.e. `x` steps first.
fn blockstate_x_step(p: [f32; 3]) -> [f32; 3] {
    [p[0], p[2], -p[1]]
}

/// One −90° step of the blockstate `"y"` rotation: `(x, y, z) -> (-z, y, x)`.
fn blockstate_y_step(p: [f32; 3]) -> [f32; 3] {
    [-p[2], p[1], p[0]]
}

/// Applies the blockstate `(x, y)` rotation — `x` steps then `y` steps —
/// to a point around the block centre `(0.5, 0.5, 0.5)`.
fn blockstate_rotate(p: [f32; 3], x_steps: u32, y_steps: u32, origin: [f32; 3]) -> [f32; 3] {
    let mut d = [p[0] - origin[0], p[1] - origin[1], p[2] - origin[2]];
    for _ in 0..x_steps {
        d = blockstate_x_step(d);
    }
    for _ in 0..y_steps {
        d = blockstate_y_step(d);
    }
    [d[0] + origin[0], d[1] + origin[1], d[2] + origin[2]]
}

/// Rotates a direction/normal vector by the blockstate transform.
fn blockstate_rotate_vec(v: [f32; 3], x_steps: u32, y_steps: u32) -> [f32; 3] {
    let mut d = v;
    for _ in 0..x_steps {
        d = blockstate_x_step(d);
    }
    for _ in 0..y_steps {
        d = blockstate_y_step(d);
    }
    d
}

/// Validates a blockstate `"x"`/`"y"` value: Vanilla only allows multiples
/// of 90; anything else is a hostile or corrupt variant.
fn rotation_steps(degrees: i32, axis: &str) -> Result<u32, ModelError> {
    let wrapped = degrees.rem_euclid(360);
    if !(wrapped as u32).is_multiple_of(90) {
        return Err(ModelError::InvalidRotationAngle(format!(
            "blockstate {axis} rotation {degrees} is not a multiple of 90"
        )));
    }
    Ok((wrapped / 90) as u32)
}

/// Re-checks a programmatically built element (the parser enforces the same
/// bounds, but a `RawModelJson` can be constructed without it).
fn validate_element(element: &ModelElement, index: usize) -> Result<(), ModelError> {
    for (axis, (&from, &to)) in element.from.iter().zip(&element.to).enumerate() {
        for (name, value) in [("from", from), ("to", to)] {
            if !value.is_finite() {
                return Err(ModelError::NonFiniteValue(format!(
                    "elements[{index}].{name}[{axis}]"
                )));
            }
            if !(MIN_MODEL_COORD..=MAX_MODEL_COORD).contains(&value) {
                return Err(ModelError::OutOfRangeCoordinate(format!(
                    "elements[{index}].{name}[{axis}] = {value}"
                )));
            }
        }
    }
    if let Some(rotation) = &element.rotation {
        if !rotation.angle_deg.is_finite() {
            return Err(ModelError::NonFiniteValue(format!(
                "elements[{index}].rotation.angle"
            )));
        }
        for (axis, value) in rotation.origin.iter().enumerate() {
            if !value.is_finite() {
                return Err(ModelError::NonFiniteValue(format!(
                    "elements[{index}].rotation.origin[{axis}]"
                )));
            }
            if !(MIN_MODEL_COORD..=MAX_MODEL_COORD).contains(value) {
                return Err(ModelError::OutOfRangeCoordinate(format!(
                    "elements[{index}].rotation.origin[{axis}] = {value}"
                )));
            }
        }
    }
    Ok(())
}

/// Bakes `model_id` to quads in block-local `0..=1` space.
///
/// `blockstate_rot_x_deg`/`blockstate_rot_y_deg` are the variant `"x"`/`"y"`
/// rotations from the blockstate file in degrees — multiples of 90, applied
/// as Vanilla does (`x` first, then `y`, both clockwise-negative about the
/// block centre). They rotate the quad positions, normals *and* `cullface`
/// directions; element rotations rotate positions and normals but (matching
/// Vanilla) leave `cullface` to the blockstate transform alone.
pub fn bake_model(
    stack: &ResourcePackModelStack,
    model_id: &str,
    blockstate_rot_x_deg: i32,
    blockstate_rot_y_deg: i32,
) -> Result<BakedModel, ModelError> {
    let x_steps = rotation_steps(blockstate_rot_x_deg, "x")?;
    let y_steps = rotation_steps(blockstate_rot_y_deg, "y")?;
    let resolved = stack.resolve(model_id)?;
    if resolved.elements.len() > MAX_ELEMENTS_PER_MODEL {
        return Err(ModelError::TooManyElements(resolved.elements.len()));
    }
    let center = [0.5; 3];
    let mut quads = Vec::new();
    for (index, element) in resolved.elements.iter().enumerate() {
        validate_element(element, index)?;
        // Rescale rotates *with* the element in Vanilla, so it happens in
        // model-unit space before the /16 scale-down.
        let rotation = element.rotation;
        let rad = rotation.map_or(0.0, |r| r.angle_deg.to_radians());
        let rescale = match rotation {
            Some(r) if r.rescale => 1.0 / rad.cos(),
            _ => 1.0,
        };
        for direction in Direction::ALL {
            let Some(face) = &element.faces[direction as usize] else {
                continue;
            };
            let texture =
                ResourcePackModelStack::resolve_texture(&resolved.textures, &face.texture)?;
            let uv_rect = face
                .uv
                .unwrap_or_else(|| default_uv(direction, element.from, element.to));
            let uv_shift = usize::from(face.rotation_deg / 90);
            let mut positions = [[0.0; 3]; 4];
            let mut uvs = [[0.0; 2]; 4];
            for (corner, corner_sel) in FACE_CORNERS[direction as usize].iter().enumerate() {
                let pick = |axis: usize| {
                    if corner_sel[axis] == 0 {
                        element.from[axis]
                    } else {
                        element.to[axis]
                    }
                };
                let mut p = [pick(0), pick(1), pick(2)];
                // Vanilla `Transformation`: scale, then rotate, about origin.
                if let Some(rotation) = rotation {
                    if rescale != 1.0 {
                        p = apply_rescale(p, rotation.origin, rotation.axis, rescale);
                    }
                    p = rotate_axis(p, rotation.origin, rotation.axis, rad);
                }
                positions[corner] = [
                    p[0] / MODEL_UNITS_PER_BLOCK,
                    p[1] / MODEL_UNITS_PER_BLOCK,
                    p[2] / MODEL_UNITS_PER_BLOCK,
                ];
                let pair = UV_PAIRS[(FACE_UV_CORNER[direction as usize][corner] + uv_shift) % 4];
                uvs[corner] = [uv_rect[pair.0], uv_rect[pair.1]];
            }
            let mut normal = direction_vector(direction);
            if let Some(rotation) = rotation {
                normal = rotate_axis_vec(normal, rotation.axis, rad);
            }
            // Blockstate rotation: positions about the block centre, vectors
            // bare. cullface rotates with the blockstate only — Vanilla does
            // not re-derive it from the element rotation.
            for p in &mut positions {
                *p = blockstate_rotate(*p, x_steps, y_steps, center);
                for (axis, value) in p.iter_mut().enumerate() {
                    if !value.is_finite() {
                        return Err(ModelError::NonFiniteValue(format!(
                            "elements[{index}] vertex axis {axis}"
                        )));
                    }
                }
            }
            normal = blockstate_rotate_vec(normal, x_steps, y_steps);
            let cullface = face.cull_direction().and_then(|d| {
                direction_of(blockstate_rotate_vec(direction_vector(d), x_steps, y_steps))
            });
            quads.push(BakedModelQuad {
                positions,
                normal,
                texture,
                uv: uvs,
                cullface,
                tintindex: face.tintindex,
                shade: element.shade,
            });
        }
    }
    Ok(BakedModel {
        ambient_occlusion: resolved.ambientocclusion,
        quads,
    })
}

/// Selects the item model for a `custom_model_data` value, per Vanilla item
/// overrides: the highest predicate threshold `<= value` wins; with no
/// matching override (or no `custom_model_data` on the item) the base model
/// id is returned unchanged.
pub fn resolve_item_override(
    stack: &ResourcePackModelStack,
    item_model_id: &str,
    custom_model_data: Option<i32>,
) -> Result<String, ModelError> {
    let base = normalize_location(item_model_id);
    let leaf = stack
        .get(&base)
        .ok_or_else(|| ModelError::MissingModel(base.clone()))?;
    let Some(value) = custom_model_data else {
        return Ok(base);
    };
    let mut best: Option<(f64, &ItemModelOverride)> = None;
    for item_override in &leaf.overrides {
        let Some(threshold) = item_override.custom_model_data else {
            continue;
        };
        // Vanilla matches threshold <= actual; the highest such threshold
        // wins (the last match in list order when overrides are sorted).
        if threshold <= f64::from(value) && best.is_none_or(|(b, _)| threshold >= b) {
            best = Some((threshold, item_override));
        }
    }
    Ok(best.map_or(base, |(_, o)| normalize_location(&o.model)))
}
