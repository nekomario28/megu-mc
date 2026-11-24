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

use megu_core::json::{JsonLimits, JsonValue};

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
    /// A `blockstates/*.json` document could not be resolved for a block
    /// state: malformed variant selector, unknown property, overlapping
    /// variant definitions, or no matching selector at all.
    BlockState(String),
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
            Self::BlockState(m) => write!(f, "blockstate error: {m}"),
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

/// Bounds for [`parse_json`]. `max_depth` is one past [`MAX_JSON_DEPTH`]:
/// the canonical reader counts the root value at depth 0 and rejects any
/// value *at* `max_depth`, matching the retired parser's "64 containers,
/// scalars at depth 64 still parse" contract. Node and byte ceilings stay
/// off — the retired parser had none and model files can be large.
const JSON_LIMITS: JsonLimits = JsonLimits {
    max_depth: MAX_JSON_DEPTH + 1,
    max_nodes: usize::MAX,
    max_bytes: usize::MAX,
};

fn parse_json(text: &str) -> Result<Json, JsonError> {
    let value = megu_core::json::parse_json_with_limits(text, JSON_LIMITS, false)
        .map_err(|e| JsonError {
            offset: e.offset,
            message: e.message,
        })?;
    json_value_to_json(&value)
}

/// Adapt the canonical [`JsonValue`] tree into this module's [`Json`]:
/// objects fold document-ordered members into a `BTreeMap` (last duplicate
/// key wins, as before), and non-finite numbers fail closed — the retired
/// parser rejected `1e999` at parse time and the baker relies on that.
fn json_value_to_json(value: &JsonValue) -> Result<Json, JsonError> {
    Ok(match value {
        JsonValue::Null => Json::Null,
        JsonValue::Bool(b) => Json::Bool(*b),
        JsonValue::Number(n) => {
            if !n.is_finite() {
                return Err(JsonError {
                    offset: 0,
                    message: "number is out of range".to_string(),
                });
            }
            Json::Num(*n)
        }
        JsonValue::String(s) => Json::Str(s.clone()),
        JsonValue::Array(items) => Json::Arr(
            items
                .iter()
                .map(json_value_to_json)
                .collect::<Result<_, JsonError>>()?,
        ),
        JsonValue::Object(members) => {
            let mut map = BTreeMap::new();
            for (key, member) in members {
                map.insert(key.clone(), json_value_to_json(member)?);
            }
            Json::Obj(map)
        }
    })
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

    /// The model's `parent`-merged `textures` map with every `#alias`
    /// resolved to a concrete texture id — what [`bake_model_with_uvlock`]
    /// consults per face, exposed so callers that only need a sprite name
    /// (e.g. a per-block display texture) pay no quad bake.
    pub fn resolved_textures(
        &self,
        model_id: &str,
    ) -> Result<BTreeMap<String, String>, ModelError> {
        let resolved = self.resolve(model_id)?;
        let mut textures = BTreeMap::new();
        for (key, value) in &resolved.textures {
            textures.insert(key.clone(), Self::resolve_texture(&resolved.textures, value)?);
        }
        Ok(textures)
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

/// UV rectangle corner `(u_index, v_index)` pairs into the `[u0, v0, u1, v1]`
/// rectangle for vanilla's shifted-UV slot `i`, matching `BlockFaceUV.getU`/
/// `getV`: slot 0 = `(u0, v0)`, 1 = `(u0, v1)`, 2 = `(u1, v1)`, 3 = `(u1, v0)`.
const VANILLA_UV_SLOTS: [(usize, usize); 4] = [(0, 1), (0, 3), (2, 3), (2, 1)];

/// Vanilla's default UV rectangle for a face (`[u0, v0, u1, v1]`) when the
/// JSON omits `uv`: the element bounds projected onto the face plane, exactly
/// like `BlockElement.uvsByFace` — note `down` flips V against `+z` while
/// `up` does not.
fn default_uv(direction: Direction, from: [f32; 3], to: [f32; 3]) -> [f32; 4] {
    match direction {
        Direction::Down => [
            from[0],
            MODEL_UNITS_PER_BLOCK - to[2],
            to[0],
            MODEL_UNITS_PER_BLOCK - from[2],
        ],
        Direction::Up => [from[0], from[2], to[0], to[2]],
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

// ---------------------------------------------------------------------------
// UV-lock transform (Vanilla `FaceBakery.recomputeUVs` + `BlockMath`)
// ---------------------------------------------------------------------------

/// Row-major 3×3 matrix. Every rotation this baker composes is a multiple of
/// 90°, so entries stay exactly `0`/`±1` — no trig drift.
type Mat3 = [[f32; 3]; 3];

fn mat3_mul(a: Mat3, b: Mat3) -> Mat3 {
    let mut out = [[0.0; 3]; 3];
    for i in 0..3 {
        for j in 0..3 {
            out[i][j] = a[i][0] * b[0][j] + a[i][1] * b[1][j] + a[i][2] * b[2][j];
        }
    }
    out
}

fn mat3_transpose(m: Mat3) -> Mat3 {
    let mut out = [[0.0; 3]; 3];
    for i in 0..3 {
        for j in 0..3 {
            out[i][j] = m[j][i];
        }
    }
    out
}

fn mat3_vec(m: Mat3, v: [f32; 3]) -> [f32; 3] {
    [
        m[0][0] * v[0] + m[0][1] * v[1] + m[0][2] * v[2],
        m[1][0] * v[0] + m[1][1] * v[1] + m[1][2] * v[2],
        m[2][0] * v[0] + m[2][1] * v[1] + m[2][2] * v[2],
    ]
}

/// The 3×3 matrix of the blockstate `(x, y)` rotation — `x` steps then `y`
/// steps of −90° — built from the same exact step functions that move
/// positions, so normals, `Direction.rotate` and the UV lock agree.
fn blockstate_matrix(x_steps: u32, y_steps: u32) -> Mat3 {
    let mut m = [[0.0; 3]; 3];
    for (col, e) in [[1.0f32, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]]
        .iter()
        .enumerate()
    {
        let v = blockstate_rotate_vec(*e, x_steps, y_steps);
        for row in 0..3 {
            m[row][col] = v[row];
        }
    }
    m
}

/// `Direction.rotate(matrix, facing)` + `Direction.getNearest`: transform the
/// face normal and keep the axis direction with the largest dot product.
/// Vanilla seeds the search with `NORTH`/`Float.MIN_VALUE`; for a rotation
/// matrix exactly one candidate always wins, but the fallback is kept anyway.
fn rotated_direction(model: Mat3, facing: Direction) -> Direction {
    let v = mat3_vec(model, direction_vector(facing));
    let mut best = Direction::North;
    // Java `Float.MIN_VALUE` (smallest positive denormal, 1.4e-45): all dots
    // are `{-1, 0, 1}` for exact 90° matrices, so only the `1` ever wins.
    let mut best_dot = f32::from_bits(1);
    for candidate in Direction::ALL {
        let n = direction_vector(candidate);
        let dot = v[0] * n[0] + v[1] * n[1] + v[2] * n[2];
        if dot > best_dot {
            best_dot = dot;
            best = candidate;
        }
    }
    best
}

/// `BlockMath.VANILLA_UV_TRANSFORM_LOCAL_TO_GLOBAL` restricted to the
/// axis-aligned rotations blockstates can produce, expressed in −90° steps
/// (single-axis, so compose order is irrelevant).
fn l2g_matrix(direction: Direction) -> Mat3 {
    match direction {
        Direction::South => [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]],
        Direction::East => blockstate_matrix(0, 3),  // rotateY(+90°)
        Direction::West => blockstate_matrix(0, 1),  // rotateY(-90°)
        Direction::North => blockstate_matrix(0, 2), // rotateY(180°)
        Direction::Up => blockstate_matrix(1, 0),    // rotateX(-90°)
        Direction::Down => blockstate_matrix(3, 0),  // rotateX(+90°)
    }
}

/// `BlockMath.VANILLA_UV_TRANSFORM_GLOBAL_TO_LOCAL` — the inverse (transpose)
/// of [`l2g_matrix`].
fn g2l_matrix(direction: Direction) -> Mat3 {
    mat3_transpose(l2g_matrix(direction))
}

/// `java.lang.Math.signum` for finite floats: `-1`, `0` or `1`.
fn java_signum(x: f32) -> f32 {
    if x > 0.0 {
        1.0
    } else if x < 0.0 {
        -1.0
    } else {
        0.0
    }
}

/// Vanilla `FaceBakery.recomputeUVs`: counter-rotates a face's UV rectangle
/// so the texture stays locked to the model's *unrotated* axes when the
/// blockstate `(x, y)` rotation is applied with `uvlock`.
///
/// `uv_rect` is the `[u0, v0, u1, v1]` rectangle (explicit or default) and
/// `rotation_deg` the face's in-face UV rotation; `model` is the blockstate
/// rotation matrix. Returns the recomputed rectangle and in-face rotation.
fn recompute_uvs(
    uv_rect: [f32; 4],
    rotation_deg: u16,
    facing: Direction,
    model: Mat3,
) -> ([f32; 4], u16) {
    // getUVLockTransform: G2L[facing] * inverse(model) * L2G[rotated facing],
    // conjugated about the block centre (0.5, 0.5, 0.5) by blockCenterToCorner.
    let rotated_facing = rotated_direction(model, facing);
    let r = mat3_mul(
        g2l_matrix(facing),
        mat3_mul(mat3_transpose(model), l2g_matrix(rotated_facing)),
    );
    // The affine map on a UV point (u/16, v/16, 0) is
    // q -> r * (q - 0.5) + 0.5; only x/y of the result are read back.
    let project = |u: f32, v: f32| -> [f32; 2] {
        let d = [
            u / MODEL_UNITS_PER_BLOCK - 0.5,
            v / MODEL_UNITS_PER_BLOCK - 0.5,
            -0.5,
        ];
        let o = mat3_vec(r, d);
        [
            (o[0] + 0.5) * MODEL_UNITS_PER_BLOCK,
            (o[1] + 0.5) * MODEL_UNITS_PER_BLOCK,
        ]
    };
    // getU/getV(getReverseIndex(j)) always yields slot j of the *input*
    // rotation, i.e. the diagonal corners (u0,v0) and (u1,v1).
    let t0 = project(uv_rect[0], uv_rect[1]);
    let t1 = project(uv_rect[2], uv_rect[3]);
    let (u0_new, u1_new) = if java_signum(uv_rect[2] - uv_rect[0])
        == java_signum(t1[0] - t0[0])
    {
        (t0[0], t1[0])
    } else {
        (t1[0], t0[0])
    };
    let (v0_new, v1_new) = if java_signum(uv_rect[3] - uv_rect[1])
        == java_signum(t1[1] - t0[1])
    {
        (t0[1], t1[1])
    } else {
        (t1[1], t0[1])
    };
    // The in-face rotation re-derives from where the UV plane's (cos, sin)
    // basis vector lands under the same 3x3 — rounded to the nearest 90°.
    let rad = f32::from(rotation_deg).to_radians();
    let axis = mat3_vec(r, [rad.cos(), rad.sin(), 0.0]);
    let degrees = (axis[1] as f64).atan2(axis[0] as f64).to_degrees() / 90.0;
    // Java Math.round == floor(x + 0.5) for this range.
    let quarters = (degrees + 0.5).floor() as i64;
    let new_rotation = (-(quarters as i32) * 90).rem_euclid(360) as u16;
    ([u0_new, v0_new, u1_new, v1_new], new_rotation)
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
    bake_model_with_uvlock(stack, model_id, blockstate_rot_x_deg, blockstate_rot_y_deg, false)
}

/// [`bake_model`] plus the variant `"uvlock"` flag: when `uvlock` is true,
/// each face's UV rectangle is counter-rotated by
/// [`recompute_uvs`] so the texture stays aligned to the model's unrotated
/// axes, exactly like Vanilla `FaceBakery`/`BlockMath.getUVLockTransform`.
pub fn bake_model_with_uvlock(
    stack: &ResourcePackModelStack,
    model_id: &str,
    blockstate_rot_x_deg: i32,
    blockstate_rot_y_deg: i32,
    uvlock: bool,
) -> Result<BakedModel, ModelError> {
    let x_steps = rotation_steps(blockstate_rot_x_deg, "x")?;
    let y_steps = rotation_steps(blockstate_rot_y_deg, "y")?;
    let model_mat = blockstate_matrix(x_steps, y_steps);
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
            let mut uv_rect = face
                .uv
                .unwrap_or_else(|| default_uv(direction, element.from, element.to));
            let mut face_rotation = face.rotation_deg;
            if uvlock {
                let recomputed = recompute_uvs(uv_rect, face_rotation, direction, model_mat);
                uv_rect = recomputed.0;
                face_rotation = recomputed.1;
            }
            // `BlockFaceUV` rotates UV assignment clockwise by `rotation / 90`
            // slots (`getShiftedIndex`). The port's `FACE_CORNERS` winding is
            // one slot ahead of vanilla's `FaceInfo` vertex order on every
            // face, so port corner `c` is vanilla vertex `(c + 1) % 4`.
            let uv_shift = usize::from(face_rotation / 90);
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
                let slot = (corner + 1 + uv_shift) % 4;
                let pair = VANILLA_UV_SLOTS[slot];
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

// ---------------------------------------------------------------------------
// blockstates/*.json resolution
// ---------------------------------------------------------------------------

/// Clean-room resolver for `assets/<ns>/blockstates/<block>.json`, mirroring
/// Vanilla `BlockStateDefinition` loading and `BlockModelDefinition`-time
/// `"variants"`/`"multipart"` selection.
///
/// Determinism: weighted `"variants"`/`"apply"` arrays pick with a position
/// hash derived from `block_pos` — no RNG state, no clock — so the same
/// `(definition, props, pos)` always resolves identically.
///
/// Fail-closed: malformed JSON, schema violations, unknown properties,
/// overlapping variant coverage and unmatchable states are all errors; the
/// caller drops the block rather than guessing a fallback.
pub mod blockstate {
    use super::*;
    use std::collections::BTreeMap;

    /// One concrete model choice inside a variant `"variants"` value or a
    /// multipart `"apply"`.
    #[derive(Clone, Debug, PartialEq)]
    pub struct VariantSpec {
        /// Model location (`"minecraft:block/foo"` after normalization is
        /// *not* applied here — pass it to the baker verbatim).
        pub model: String,
        /// Variant `"x"` rotation in degrees, normalized to `{0,90,180,270}`.
        pub x_deg: i32,
        /// Variant `"y"` rotation in degrees, normalized likewise.
        pub y_deg: i32,
        /// Variant `"uvlock"`: counter-rotate face UVs against the model
        /// rotation (`FaceBakery.recomputeUVs`).
        pub uvlock: bool,
        /// Variant `"weight"` (>= 1); weighted arrays pick by position hash.
        pub weight: i32,
    }

    /// The deterministic outcome of [`resolve_blockstate`].
    #[derive(Clone, Debug, PartialEq)]
    pub enum BlockStateResolution {
        /// A single weighted-picked variant from `"variants"`.
        Variant(VariantSpec),
        /// Every matching `"multipart"` selector's weighted pick, in file
        /// order. May be empty when no selector's `when` matched.
        Multipart(Vec<VariantSpec>),
    }

    /// A parsed `blockstates/*.json` document.
    #[derive(Clone, Debug, Default, PartialEq)]
    pub struct BlockStateDef {
        /// `"variants"` entries in file order.
        variants: Vec<VariantEntry>,
        /// `"multipart"` selectors in file order.
        multipart: Vec<Selector>,
    }

    /// One `"variants"` map entry: the parsed property constraints of its
    /// key plus its variant(s).
    #[derive(Clone, Debug, PartialEq)]
    struct VariantEntry {
        /// Comma-separated `prop=value` segments of the JSON key
        /// (`"facing=north,half=top"` -> `[("facing","north"),("half","top")]`);
        /// an empty vec is the `""` catch-all.
        constraints: Vec<(String, String)>,
        variants: Vec<VariantSpec>,
    }

    #[derive(Clone, Debug, PartialEq)]
    struct Selector {
        condition: Condition,
        variants: Vec<VariantSpec>,
    }

    /// A multipart `"when"` condition: `AND`/`OR` combinators over
    /// `prop=value` key-value tests. Pipe-separated values (`"low|tall"`)
    /// and a leading `!` negation follow Vanilla `KeyValueCondition`.
    #[derive(Clone, Debug, PartialEq)]
    enum Condition {
        /// No `"when"` key — always true.
        Any,
        KeyValue {
            key: String,
            values: Vec<String>,
            negated: bool,
        },
        /// `"AND": [...]` (also the implicit form of multi-key objects).
        All(Vec<Condition>),
        /// `"OR": [...]`.
        AnyOf(Vec<Condition>),
    }

    impl BlockStateDef {
        /// Parses a `blockstates/*.json` document. Reuses the bounded JSON
        /// parser (`MAX_JSON_DEPTH`, finite numbers, no trailing bytes).
        pub fn parse(text: &str) -> Result<Self, ModelError> {
            let root = parse_json(text).map_err(ModelError::Json)?;
            let obj = root
                .as_object()
                .ok_or_else(|| schema_err("blockstate root must be a JSON object"))?;
            let mut variants = Vec::new();
            if let Some(value) = obj.get("variants") {
                let map = value
                    .as_object()
                    .ok_or_else(|| schema_err("blockstates.variants must be an object"))?;
                for (key, value) in map {
                    variants.push(VariantEntry {
                        constraints: parse_variant_key(key)?,
                        variants: parse_multivariant(value, "variants")?,
                    });
                }
            }
            let mut multipart = Vec::new();
            if let Some(value) = obj.get("multipart") {
                let items = value
                    .as_array()
                    .ok_or_else(|| schema_err("blockstates.multipart must be an array"))?;
                for (index, item) in items.iter().enumerate() {
                    multipart.push(parse_selector(item, index)?);
                }
            }
            if variants.is_empty() && multipart.is_empty() {
                return Err(schema_err(
                    "blockstate defines neither 'variants' nor 'multipart'",
                ));
            }
            Ok(Self { variants, multipart })
        }
    }

    /// Resolves `def` for a block state `props` (`property -> value`, e.g.
    /// `{"facing": "north"}`) at world position `block_pos`.
    ///
    /// `"variants"` win over `"multipart"` (Vanilla pre-fills multipart for
    /// every state, then variant predicates overwrite it). Matching variant
    /// keys must be disjoint — two matches are the Vanilla "overlapping
    /// definition" failure. A state matched by nothing is an error: the
    /// caller fails closed rather than substituting a fallback model.
    pub fn resolve_blockstate(
        def: &BlockStateDef,
        props: &BTreeMap<String, String>,
        block_pos: [i32; 3],
    ) -> Result<BlockStateResolution, ModelError> {
        let mut matched: Option<&[VariantSpec]> = None;
        for entry in &def.variants {
            if variant_key_matches(&entry.constraints, props)? {
                if matched.is_some() {
                    return Err(ModelError::BlockState(
                        "overlapping variant definitions".to_owned(),
                    ));
                }
                matched = Some(&entry.variants);
            }
        }
        if let Some(variants) = matched {
            return Ok(BlockStateResolution::Variant(
                pick_weighted(variants, block_pos, 0).clone(),
            ));
        }
        if !def.multipart.is_empty() {
            let mut out = Vec::new();
            for (index, selector) in def.multipart.iter().enumerate() {
                if selector.condition.eval(props)? {
                    out.push(
                        pick_weighted(&selector.variants, block_pos, index as u64 + 1).clone(),
                    );
                }
            }
            return Ok(BlockStateResolution::Multipart(out));
        }
        Err(ModelError::BlockState(
            "no variant or multipart selector matches the block state".to_owned(),
        ))
    }

    /// Resolves `def` and bakes the result: a single model for `"variants"`,
    /// or the concatenation of every matching multipart selector's quads in
    /// file order (like Vanilla's `MultiPartBakedModel`). Ambient occlusion
    /// comes from the first *matching* part — Vanilla reads it from the
    /// first selector unconditionally, but non-matching selectors are never
    /// baked here.
    pub fn bake_blockstate(
        stack: &ResourcePackModelStack,
        def: &BlockStateDef,
        props: &BTreeMap<String, String>,
        block_pos: [i32; 3],
    ) -> Result<BakedModel, ModelError> {
        match resolve_blockstate(def, props, block_pos)? {
            BlockStateResolution::Variant(variant) => bake_model_with_uvlock(
                stack,
                &variant.model,
                variant.x_deg,
                variant.y_deg,
                variant.uvlock,
            ),
            BlockStateResolution::Multipart(parts) => {
                let mut quads = Vec::new();
                let mut ambient_occlusion = true;
                for (index, variant) in parts.iter().enumerate() {
                    let baked = bake_model_with_uvlock(
                        stack,
                        &variant.model,
                        variant.x_deg,
                        variant.y_deg,
                        variant.uvlock,
                    )?;
                    if index == 0 {
                        ambient_occlusion = baked.ambient_occlusion;
                    }
                    quads.extend(baked.quads);
                }
                Ok(BakedModel {
                    ambient_occlusion,
                    quads,
                })
            }
        }
    }

    /// A synthetic `property -> value` map that lets
    /// [`resolve_blockstate`]/[`bake_blockstate`] produce a concrete model
    /// for `def` when no real block state is available — the client-side
    /// registry names blocks per state id but does not retain each state's
    /// properties, so a per-block display texture needs stand-in values.
    ///
    /// Every key any selector references must be *present* or `eval`
    /// fails closed. For `"variants"` defs the first entry's own
    /// constraints are used verbatim (guaranteeing that entry matches)
    /// and keys referenced only by later entries get `""` — a value no
    /// constraint equals, so they evaluate false instead of erroring.
    /// For `"multipart"` defs each condition key gets its first
    /// positively listed value, and keys that appear only negated get
    /// `""`, which satisfies the negation.
    pub fn representative_props(def: &BlockStateDef) -> BTreeMap<String, String> {
        let mut props = BTreeMap::new();
        for (index, entry) in def.variants.iter().enumerate() {
            for (key, value) in &entry.constraints {
                props.entry(key.clone()).or_insert_with(|| {
                    if index == 0 {
                        value.clone()
                    } else {
                        String::new()
                    }
                });
            }
        }
        // First pass: keys a condition asserts positively win their first
        // listed value, so `when` selectors actually apply. Negated-only
        // keys fill in afterwards.
        fn condition_values<'a>(
            condition: &'a Condition,
            positives: &mut BTreeMap<&'a str, &'a str>,
            all: &mut std::collections::BTreeSet<&'a str>,
        ) {
            match condition {
                Condition::Any => {}
                Condition::KeyValue {
                    key,
                    values,
                    negated,
                } => {
                    all.insert(key.as_str());
                    if !negated {
                        positives.entry(key.as_str()).or_insert(values[0].as_str());
                    }
                }
                Condition::All(conditions) | Condition::AnyOf(conditions) => {
                    for condition in conditions {
                        condition_values(condition, positives, all);
                    }
                }
            }
        }
        let mut positives = BTreeMap::new();
        let mut all_keys = std::collections::BTreeSet::new();
        for selector in &def.multipart {
            condition_values(&selector.condition, &mut positives, &mut all_keys);
        }
        for key in all_keys {
            props
                .entry(key.to_owned())
                .or_insert_with(|| positives.get(key).copied().unwrap_or_default().to_owned());
        }
        props
    }

    /// Parses a `"variants"` key: comma-separated `prop=value` segments.
    /// Empty segments (the `""` key, trailing commas, `"=v"`) contribute no
    /// constraint, matching Vanilla's `predicate()` splitter behaviour.
    fn parse_variant_key(key: &str) -> Result<Vec<(String, String)>, ModelError> {
        let mut out = Vec::new();
        for segment in key.split(',') {
            let mut parts = segment.splitn(2, '=');
            let name = parts.next().unwrap_or_default();
            match parts.next() {
                Some(value) => {
                    if !name.is_empty() {
                        out.push((name.to_owned(), value.to_owned()));
                    }
                }
                None => {
                    if !name.is_empty() {
                        return Err(schema_err(format!(
                            "malformed variant key segment {segment:?} in {key:?}"
                        )));
                    }
                }
            }
        }
        Ok(out)
    }

    /// Every `prop=value` constraint must hold; a constraint naming a
    /// property absent from `props` fails closed (Vanilla rejects unknown
    /// blockstate properties at load time).
    fn variant_key_matches(
        constraints: &[(String, String)],
        props: &BTreeMap<String, String>,
    ) -> Result<bool, ModelError> {
        for (key, value) in constraints {
            match props.get(key) {
                Some(actual) if actual == value => {}
                Some(_) => return Ok(false),
                None => {
                    return Err(ModelError::BlockState(format!(
                        "variant selector references unknown property {key:?}"
                    )));
                }
            }
        }
        Ok(true)
    }

    /// A `"variants"`/`"apply"` value: a single variant object, or a
    /// non-empty array of them (Vanilla rejects empty arrays).
    fn parse_multivariant(value: &Json, field: &str) -> Result<Vec<VariantSpec>, ModelError> {
        match value {
            Json::Obj(_) => Ok(vec![parse_variant(value, field)?]),
            Json::Arr(items) => {
                if items.is_empty() {
                    return Err(schema_err(format!("{field}: empty variant array")));
                }
                items
                    .iter()
                    .enumerate()
                    .map(|(index, item)| parse_variant(item, &format!("{field}[{index}]")))
                    .collect()
            }
            _ => Err(schema_err(format!(
                "{field} must be a variant object or array of variants"
            ))),
        }
    }

    fn parse_variant(value: &Json, field: &str) -> Result<VariantSpec, ModelError> {
        let obj = value
            .as_object()
            .ok_or_else(|| schema_err(format!("{field} must be an object")))?;
        let model = obj
            .get("model")
            .and_then(Json::as_str)
            .ok_or_else(|| schema_err(format!("{field}.model must be a string")))?
            .to_owned();
        let x_deg = parse_rotation(obj.get("x"), field, "x")?;
        let y_deg = parse_rotation(obj.get("y"), field, "y")?;
        let uvlock = match obj.get("uvlock") {
            None | Some(Json::Null) => false,
            Some(Json::Bool(b)) => *b,
            Some(_) => {
                return Err(schema_err(format!("{field}.uvlock must be a boolean")));
            }
        };
        let weight = match obj.get("weight") {
            None | Some(Json::Null) => 1,
            Some(value) => {
                let weight = parse_int(value, &format!("{field}.weight"))?;
                if weight < 1 {
                    return Err(schema_err(format!("{field}.weight must be >= 1")));
                }
                weight
            }
        };
        Ok(VariantSpec {
            model,
            x_deg,
            y_deg,
            uvlock,
            weight,
        })
    }

    /// Variant `"x"`/`"y"`: an integer that, after `rem_euclid(360)`, is a
    /// multiple of 90 — the sixteen `BlockModelRotation` combos of Vanilla.
    fn parse_rotation(value: Option<&Json>, field: &str, axis: &str) -> Result<i32, ModelError> {
        let Some(value) = value else {
            return Ok(0);
        };
        if matches!(value, Json::Null) {
            return Ok(0);
        }
        let raw = parse_int(value, &format!("{field}.{axis}"))?;
        let wrapped = raw.rem_euclid(360);
        if wrapped % 90 != 0 {
            return Err(schema_err(format!(
                "{field}.{axis} rotation {raw} is not a multiple of 90"
            )));
        }
        Ok(wrapped)
    }

    /// Strict integer read for blockstate fields (Vanilla reads them via
    /// Gson `getAsInt`): integral `f64` inside `i32` range.
    fn parse_int(value: &Json, field: &str) -> Result<i32, ModelError> {
        let number = value
            .as_f64()
            .ok_or_else(|| schema_err(format!("{field} must be an integer")))?;
        if number.fract() != 0.0 || number < i32::MIN as f64 || number > i32::MAX as f64 {
            return Err(schema_err(format!("{field} must be an integer")));
        }
        Ok(number as i32)
    }

    /// One `"multipart"` selector: optional `"when"` condition plus required
    /// `"apply"` variant(s).
    fn parse_selector(value: &Json, index: usize) -> Result<Selector, ModelError> {
        let field = format!("multipart[{index}]");
        let obj = value
            .as_object()
            .ok_or_else(|| schema_err(format!("{field} must be an object")))?;
        let condition = match obj.get("when") {
            None | Some(Json::Null) => Condition::Any,
            Some(value) => parse_condition(value, &format!("{field}.when"))?,
        };
        let apply = obj
            .get("apply")
            .ok_or_else(|| schema_err(format!("{field}.apply is required")))?;
        Ok(Selector {
            condition,
            variants: parse_multivariant(apply, &format!("{field}.apply"))?,
        })
    }

    /// `Selector.Deserializer.getCondition`: `{"OR": [...]}` /
    /// `{"AND": [...]}` combinators, a single `prop=value` object, or an
    /// implicit `AND` over several `prop=value` entries.
    fn parse_condition(value: &Json, field: &str) -> Result<Condition, ModelError> {
        let obj = value
            .as_object()
            .ok_or_else(|| schema_err(format!("{field} must be an object")))?;
        if obj.is_empty() {
            return Err(schema_err(format!("{field}: no elements found in selector")));
        }
        if obj.len() == 1 {
            if let Some(items) = obj.get("OR") {
                return Ok(Condition::AnyOf(parse_condition_list(
                    items, field, "OR",
                )?));
            }
            if let Some(items) = obj.get("AND") {
                return Ok(Condition::All(parse_condition_list(
                    items, field, "AND",
                )?));
            }
        }
        let mut conditions = Vec::with_capacity(obj.len());
        for (key, value) in obj {
            conditions.push(parse_key_value(key, value, field)?);
        }
        Ok(if conditions.len() == 1 {
            conditions.pop().unwrap_or(Condition::Any)
        } else {
            Condition::All(conditions)
        })
    }

    fn parse_condition_list(
        value: &Json,
        field: &str,
        token: &str,
    ) -> Result<Vec<Condition>, ModelError> {
        let items = value
            .as_array()
            .ok_or_else(|| schema_err(format!("{field}.{token} must be an array")))?;
        items
            .iter()
            .enumerate()
            .map(|(index, item)| parse_condition(item, &format!("{field}.{token}[{index}]")))
            .collect()
    }

    /// `KeyValueCondition`: `"value"` is `prop`'s value, optionally `!`-
    /// negated, with `|`-separated alternatives (`"low|tall"`).
    fn parse_key_value(
        key: &str,
        value: &Json,
        field: &str,
    ) -> Result<Condition, ModelError> {
        let raw = value
            .as_str()
            .ok_or_else(|| schema_err(format!("{field}.{key} must be a string")))?;
        let (negated, body) = match raw.strip_prefix('!') {
            Some(rest) => (true, rest),
            None => (false, raw),
        };
        let values: Vec<String> = body
            .split('|')
            .filter(|part| !part.is_empty())
            .map(str::to_owned)
            .collect();
        if values.is_empty() {
            return Err(schema_err(format!("{field}.{key}: empty value")));
        }
        Ok(Condition::KeyValue {
            key: key.to_owned(),
            values,
            negated,
        })
    }

    impl Condition {
        fn eval(&self, props: &BTreeMap<String, String>) -> Result<bool, ModelError> {
            match self {
                Self::Any => Ok(true),
                Self::KeyValue {
                    key,
                    values,
                    negated,
                } => {
                    let Some(actual) = props.get(key) else {
                        return Err(ModelError::BlockState(format!(
                            "multipart condition references unknown property {key:?}"
                        )));
                    };
                    let hit = values.iter().any(|value| value == actual);
                    Ok(if *negated { !hit } else { hit })
                }
                Self::All(conditions) => {
                    for condition in conditions {
                        if !condition.eval(props)? {
                            return Ok(false);
                        }
                    }
                    Ok(true)
                }
                Self::AnyOf(conditions) => {
                    for condition in conditions {
                        if condition.eval(props)? {
                            return Ok(true);
                        }
                    }
                    Ok(false)
                }
            }
        }
    }

    /// Deterministic stand-in for Vanilla's `RandomSource` in weighted
    /// variant selection: a splitmix64-style hash of `block_pos` salted per
    /// selector. Zero RNG state, zero clock — same inputs, same pick.
    fn position_hash(pos: [i32; 3], salt: u64) -> u64 {
        let mut h = salt
            ^ (pos[0] as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15)
            ^ (pos[1] as u64).wrapping_mul(0xC2B2_AE3D_27D4_EB4F)
            ^ (pos[2] as u64).wrapping_mul(0x1656_67B1_9E37_79F9);
        h ^= h >> 30;
        h = h.wrapping_mul(0xBF58_476D_1CE4_E5B9);
        h ^= h >> 27;
        h = h.wrapping_mul(0x94D0_49BB_1331_11EB);
        h ^ (h >> 31)
    }

    /// Vanilla `WeightedRandom.getWeightedItem`: cumulative weights in file
    /// order; the draw is `position_hash % total_weight`.
    fn pick_weighted(variants: &[VariantSpec], pos: [i32; 3], salt: u64) -> &VariantSpec {
        debug_assert!(!variants.is_empty());
        let total: u64 = variants
            .iter()
            .map(|variant| variant.weight.max(1) as u64)
            .sum();
        let mut draw = position_hash(pos, salt) % total;
        for variant in variants {
            let weight = variant.weight.max(1) as u64;
            if draw < weight {
                return variant;
            }
            draw -= weight;
        }
        variants
            .last()
            .expect("variant list is non-empty")
    }
}
