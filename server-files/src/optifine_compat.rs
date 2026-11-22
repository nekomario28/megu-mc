// SPDX-License-Identifier: LGPL-3.0-or-later
//! Bounded, clean-room parsers for OptiFine/EMF CEM models and CTM properties.

use crate::json::{self, Json};
use crate::properties::PropertiesFile;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

pub const MAX_PARTS: usize = 256;
pub const MAX_BOXES_PER_PART: usize = 256;
pub const MAX_SUBMODEL_DEPTH: usize = 16;
pub const MAX_ANIMATIONS: usize = 256;
pub const MAX_CTM_TILES: usize = 1024;

const MAX_JSON_BYTES: usize = 1024 * 1024;
const MAX_PROPERTIES_BYTES: usize = 1024 * 1024;
const MAX_STRING_BYTES: usize = 4096;
const MAX_ANIMATION_VARIABLES: usize = 256;
const MAX_ANIMATION_EXPRESSION_BYTES: usize = 4096;
const DEFAULT_TEXTURE_SIZE: [u32; 2] = [64, 32];

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OptifineCompatError {
    InvalidJson(String),
    InvalidField { field: String, reason: String },
    MissingField(String),
    LimitExceeded { field: String, limit: usize },
    MissingModelReference(String),
    ModelReferenceCycle(String),
}

impl fmt::Display for OptifineCompatError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidJson(message) => write!(f, "invalid JSON: {message}"),
            Self::InvalidField { field, reason } => write!(f, "invalid {field}: {reason}"),
            Self::MissingField(field) => write!(f, "missing required field {field}"),
            Self::LimitExceeded { field, limit } => {
                write!(f, "{field} exceeds the limit of {limit}")
            }
            Self::MissingModelReference(path) => {
                write!(f, "missing JPM model reference {path:?}")
            }
            Self::ModelReferenceCycle(path) => {
                write!(f, "cyclic JPM model reference at {path:?}")
            }
        }
    }
}

impl std::error::Error for OptifineCompatError {}

#[derive(Clone, Debug, PartialEq)]
pub struct JemModel {
    pub texture: Option<String>,
    pub texture_size: [u32; 2],
    pub shadow_size: Option<f32>,
    pub models: Vec<JemModelEntry>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct JemModelEntry {
    pub part: String,
    pub id: Option<String>,
    pub model: Option<String>,
    pub invert_axis: String,
    pub translate: [f32; 3],
    pub rotate: [f32; 3],
    pub scale: f32,
    pub attach: bool,
    pub boxes: Vec<CemBox>,
    pub submodels: Vec<CemSubmodel>,
    pub animations: Vec<BTreeMap<String, String>>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct JpmModel {
    pub texture: Option<String>,
    pub texture_size: [u32; 2],
    pub invert_axis: String,
    pub translate: [f32; 3],
    pub rotate: [f32; 3],
    pub boxes: Vec<CemBox>,
    pub submodels: Vec<CemSubmodel>,
    pub mirror_texture: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct CemBox {
    pub coordinates: [f32; 6],
    pub texture_offset: Option<[f32; 2]>,
    pub uv_north: Option<[f32; 4]>,
    pub uv_south: Option<[f32; 4]>,
    pub uv_east: Option<[f32; 4]>,
    pub uv_west: Option<[f32; 4]>,
    pub uv_up: Option<[f32; 4]>,
    pub uv_down: Option<[f32; 4]>,
    pub size_add: f32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct CemSubmodel {
    pub id: Option<String>,
    pub model: Option<String>,
    pub texture: Option<String>,
    pub texture_size: [u32; 2],
    pub invert_axis: String,
    pub translate: [f32; 3],
    pub rotate: [f32; 3],
    pub mirror_texture: String,
    pub boxes: Vec<CemBox>,
    pub submodels: Vec<CemSubmodel>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ResolvedEntityModel {
    pub texture: Option<String>,
    pub texture_size: [u32; 2],
    pub shadow_size: Option<f32>,
    pub models: Vec<ResolvedEntityPart>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ResolvedEntityPart {
    pub part: String,
    pub id: Option<String>,
    pub texture: Option<String>,
    pub texture_size: [u32; 2],
    pub invert_axis: String,
    pub translate: [f32; 3],
    pub rotate: [f32; 3],
    pub scale: f32,
    pub attach: bool,
    pub mirror_texture: String,
    pub boxes: Vec<CemBox>,
    pub submodels: Vec<ResolvedCemSubmodel>,
    pub animations: Vec<BTreeMap<String, String>>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ResolvedCemSubmodel {
    pub id: Option<String>,
    pub texture: Option<String>,
    pub texture_size: [u32; 2],
    pub invert_axis: String,
    pub translate: [f32; 3],
    pub rotate: [f32; 3],
    pub mirror_texture: String,
    pub boxes: Vec<CemBox>,
    pub submodels: Vec<ResolvedCemSubmodel>,
}

pub fn parse_jem(input: &str) -> Result<JemModel, OptifineCompatError> {
    let root = parse_json_document(input, "JEM")?;
    let object = expect_object(&root, "JEM")?;
    let texture = optional_string(&root, "texture", "JEM.texture")?;
    let texture_size = texture_size(&root, "JEM.textureSize")?;
    let shadow_size = optional_number(&root, "shadowSize", "JEM.shadowSize")?;
    if shadow_size.is_some_and(|value| !(0.0..=1.0).contains(&value)) {
        return Err(invalid("JEM.shadowSize", "must be between 0 and 1"));
    }

    let models_value = object
        .iter()
        .find(|(key, _)| key == "models")
        .map(|(_, value)| value)
        .ok_or_else(|| OptifineCompatError::MissingField("JEM.models".into()))?;
    let model_values = models_value
        .as_array()
        .ok_or_else(|| invalid("JEM.models", "must be an array"))?;
    if model_values.is_empty() {
        return Err(invalid("JEM.models", "must contain at least one model"));
    }
    if model_values.len() > MAX_PARTS {
        return Err(limit("JEM.models", MAX_PARTS));
    }

    let mut total_parts = 0usize;
    let mut total_animations = 0usize;
    let mut models = Vec::with_capacity(model_values.len());
    for (index, value) in model_values.iter().enumerate() {
        let field = format!("JEM.models[{index}]");
        models.push(parse_jem_entry(
            value,
            &field,
            0,
            &mut total_parts,
            &mut total_animations,
        )?);
    }

    Ok(JemModel {
        texture,
        texture_size,
        shadow_size,
        models,
    })
}

pub fn parse_jpm(input: &str) -> Result<JpmModel, OptifineCompatError> {
    let root = parse_json_document(input, "JPM")?;
    expect_object(&root, "JPM")?;
    let mut total_parts = 1usize;
    let (boxes, submodels) = parse_part_geometry(&root, "JPM", 0, &mut total_parts)?;
    Ok(JpmModel {
        texture: optional_string(&root, "texture", "JPM.texture")?,
        texture_size: texture_size(&root, "JPM.textureSize")?,
        invert_axis: axis_string(&root, "invertAxis", "JPM.invertAxis", "xyz")?,
        translate: number_array_or_default(&root, "translate", "JPM.translate", [0.0; 3])?,
        rotate: number_array_or_default(&root, "rotate", "JPM.rotate", [0.0; 3])?,
        boxes,
        submodels,
        mirror_texture: axis_string(&root, "mirrorTexture", "JPM.mirrorTexture", "uv")?,
    })
}

fn parse_json_document(input: &str, label: &str) -> Result<Json, OptifineCompatError> {
    if input.len() > MAX_JSON_BYTES {
        return Err(limit(format!("{label} bytes"), MAX_JSON_BYTES));
    }
    let value = json::parse(input).map_err(|error| {
        OptifineCompatError::InvalidJson(format!(
            "{label} at byte {}: {}",
            error.offset, error.message
        ))
    })?;
    validate_unique_keys(&value, label)?;
    Ok(value)
}

fn validate_unique_keys(value: &Json, field: &str) -> Result<(), OptifineCompatError> {
    match value {
        Json::Object(members) => {
            let mut seen = BTreeSet::new();
            for (key, child) in members {
                if !seen.insert(key.as_str()) {
                    return Err(invalid(format!("{field}.{key}"), "duplicate object key"));
                }
                validate_unique_keys(child, &format!("{field}.{key}"))?;
            }
        }
        Json::Array(items) => {
            for (index, child) in items.iter().enumerate() {
                validate_unique_keys(child, &format!("{field}[{index}]"))?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn expect_object<'a>(
    value: &'a Json,
    field: &str,
) -> Result<&'a [(String, Json)], OptifineCompatError> {
    match value {
        Json::Object(members) => Ok(members),
        _ => Err(invalid(field, "must be an object")),
    }
}

fn optional_value<'a>(value: &'a Json, key: &str) -> Option<&'a Json> {
    value.get(key)
}

fn required_value<'a>(
    value: &'a Json,
    key: &str,
    field: &str,
) -> Result<&'a Json, OptifineCompatError> {
    optional_value(value, key).ok_or_else(|| OptifineCompatError::MissingField(field.into()))
}

fn optional_string(
    value: &Json,
    key: &str,
    field: &str,
) -> Result<Option<String>, OptifineCompatError> {
    optional_value(value, key)
        .map(|item| parse_string(item, field))
        .transpose()
}

fn parse_string(value: &Json, field: &str) -> Result<String, OptifineCompatError> {
    let text = value
        .as_str()
        .ok_or_else(|| invalid(field, "must be a string"))?;
    if text.is_empty() || text.len() > MAX_STRING_BYTES || text.chars().any(char::is_control) {
        return Err(invalid(
            field,
            "must be non-empty, bounded, and contain no control characters",
        ));
    }
    Ok(text.to_owned())
}

fn optional_number(
    value: &Json,
    key: &str,
    field: &str,
) -> Result<Option<f32>, OptifineCompatError> {
    optional_value(value, key)
        .map(|item| parse_f32(item, field))
        .transpose()
}

fn parse_f32(value: &Json, field: &str) -> Result<f32, OptifineCompatError> {
    let number = value
        .as_f64()
        .ok_or_else(|| invalid(field, "must be a number"))?;
    let converted = number as f32;
    if !number.is_finite() || !converted.is_finite() {
        return Err(invalid(field, "must be finite and representable as f32"));
    }
    Ok(converted)
}

fn parse_u32(value: &Json, field: &str) -> Result<u32, OptifineCompatError> {
    let number = value
        .as_f64()
        .ok_or_else(|| invalid(field, "must be an integer"))?;
    if !number.is_finite() || number.fract() != 0.0 || !(0.0..=u32::MAX as f64).contains(&number) {
        return Err(invalid(field, "must be an unsigned 32-bit integer"));
    }
    Ok(number as u32)
}

fn number_array<const N: usize>(
    value: &Json,
    field: &str,
) -> Result<[f32; N], OptifineCompatError> {
    let items = value
        .as_array()
        .ok_or_else(|| invalid(field, "must be an array"))?;
    if items.len() != N {
        return Err(invalid(field, &format!("must contain exactly {N} numbers")));
    }
    let mut result = [0.0; N];
    for (index, item) in items.iter().enumerate() {
        result[index] = parse_f32(item, &format!("{field}[{index}]"))?;
    }
    Ok(result)
}

fn number_array_or_default<const N: usize>(
    value: &Json,
    key: &str,
    field: &str,
    default: [f32; N],
) -> Result<[f32; N], OptifineCompatError> {
    optional_value(value, key)
        .map(|item| number_array(item, field))
        .unwrap_or(Ok(default))
}

fn texture_size(value: &Json, field: &str) -> Result<[u32; 2], OptifineCompatError> {
    let Some(item) = optional_value(value, "textureSize") else {
        return Ok(DEFAULT_TEXTURE_SIZE);
    };
    let items = item
        .as_array()
        .ok_or_else(|| invalid(field, "must be an array of two integers"))?;
    if items.len() != 2 {
        return Err(invalid(field, "must contain exactly two integers"));
    }
    let size = [
        parse_u32(&items[0], &format!("{field}[0]"))?,
        parse_u32(&items[1], &format!("{field}[1]"))?,
    ];
    if size.iter().any(|dimension| !(1..=4096).contains(dimension)) {
        return Err(invalid(field, "dimensions must be in 1..=4096"));
    }
    Ok(size)
}

fn axis_string(
    value: &Json,
    key: &str,
    field: &str,
    allowed: &str,
) -> Result<String, OptifineCompatError> {
    let Some(item) = optional_value(value, key) else {
        return Ok(String::new());
    };
    let axis = item
        .as_str()
        .ok_or_else(|| invalid(field, "must be a string"))?;
    let mut seen = BTreeSet::new();
    for character in axis.chars() {
        if !allowed.contains(character) || !seen.insert(character) {
            return Err(invalid(field, "contains an unsupported or repeated axis"));
        }
    }
    Ok(axis.to_owned())
}

fn optional_id(
    value: &Json,
    key: &str,
    field: &str,
) -> Result<Option<String>, OptifineCompatError> {
    let Some(item) = optional_value(value, key) else {
        return Ok(None);
    };
    let id = parse_string(item, field)?;
    if id.len() > 256 || id.trim() != id {
        return Err(invalid(
            field,
            "must be at most 256 bytes without surrounding whitespace",
        ));
    }
    Ok(Some(id))
}

fn model_reference(
    value: &Json,
    key: &str,
    field: &str,
) -> Result<Option<String>, OptifineCompatError> {
    let Some(item) = optional_value(value, key) else {
        return Ok(None);
    };
    let path = parse_string(item, field)?;
    validate_model_reference(&path, field)?;
    Ok(Some(path))
}

fn validate_model_reference(path: &str, field: &str) -> Result<(), OptifineCompatError> {
    if path.starts_with('/')
        || path.contains('\\')
        || path.contains(':')
        || path.len() > MAX_STRING_BYTES
    {
        return Err(invalid(field, "must be a bounded relative JPM path"));
    }
    let components: Vec<&str> = path.split('/').collect();
    if components
        .iter()
        .any(|part| part.is_empty() || *part == "." || *part == "..")
        || path.chars().any(char::is_control)
    {
        return Err(invalid(field, "contains an unsafe path component"));
    }
    Ok(())
}

fn parse_jem_entry(
    value: &Json,
    field: &str,
    depth: usize,
    total_parts: &mut usize,
    total_animations: &mut usize,
) -> Result<JemModelEntry, OptifineCompatError> {
    expect_object(value, field)?;
    count_part(total_parts, field)?;
    let part = parse_string(
        required_value(value, "part", &format!("{field}.part"))?,
        &format!("{field}.part"),
    )?;
    if part.len() > 256 || part.trim() != part {
        return Err(invalid(
            format!("{field}.part"),
            "must be at most 256 bytes without surrounding whitespace",
        ));
    }
    let model = model_reference(value, "model", &format!("{field}.model"))?;
    let (boxes, submodels) = parse_part_geometry(value, field, depth, total_parts)?;
    let scale = optional_number(value, "scale", &format!("{field}.scale"))?.unwrap_or(1.0);
    let animations = parse_animations(value, field, total_animations)?;
    Ok(JemModelEntry {
        part,
        id: optional_id(value, "id", &format!("{field}.id"))?,
        model,
        invert_axis: axis_string(value, "invertAxis", &format!("{field}.invertAxis"), "xyz")?,
        translate: number_array_or_default(
            value,
            "translate",
            &format!("{field}.translate"),
            [0.0; 3],
        )?,
        rotate: number_array_or_default(value, "rotate", &format!("{field}.rotate"), [0.0; 3])?,
        scale,
        attach: optional_bool(value, "attach", &format!("{field}.attach"))?.unwrap_or(false),
        boxes,
        submodels,
        animations,
    })
}

fn optional_bool(
    value: &Json,
    key: &str,
    field: &str,
) -> Result<Option<bool>, OptifineCompatError> {
    optional_value(value, key)
        .map(|item| {
            item.as_bool()
                .ok_or_else(|| invalid(field, "must be a boolean"))
        })
        .transpose()
}

fn parse_part_geometry(
    value: &Json,
    field: &str,
    depth: usize,
    total_parts: &mut usize,
) -> Result<(Vec<CemBox>, Vec<CemSubmodel>), OptifineCompatError> {
    let boxes = parse_boxes(value, field)?;
    let mut submodels = Vec::new();
    if let Some(single) = optional_value(value, "submodel") {
        submodels.push(parse_submodel(
            single,
            &format!("{field}.submodel"),
            depth + 1,
            total_parts,
        )?);
    }
    if let Some(multiple) = optional_value(value, "submodels") {
        let items = multiple
            .as_array()
            .ok_or_else(|| invalid(format!("{field}.submodels"), "must be an array"))?;
        if items.len() > MAX_PARTS {
            return Err(limit(format!("{field}.submodels"), MAX_PARTS));
        }
        for (index, item) in items.iter().enumerate() {
            submodels.push(parse_submodel(
                item,
                &format!("{field}.submodels[{index}]"),
                depth + 1,
                total_parts,
            )?);
        }
    }
    Ok((boxes, submodels))
}

fn count_part(total_parts: &mut usize, field: &str) -> Result<(), OptifineCompatError> {
    *total_parts += 1;
    if *total_parts > MAX_PARTS {
        return Err(limit(field, MAX_PARTS));
    }
    Ok(())
}

fn parse_submodel(
    value: &Json,
    field: &str,
    depth: usize,
    total_parts: &mut usize,
) -> Result<CemSubmodel, OptifineCompatError> {
    if depth > MAX_SUBMODEL_DEPTH {
        return Err(limit(format!("{field} depth"), MAX_SUBMODEL_DEPTH));
    }
    expect_object(value, field)?;
    count_part(total_parts, field)?;
    let (boxes, submodels) = parse_part_geometry(value, field, depth, total_parts)?;
    Ok(CemSubmodel {
        id: optional_id(value, "id", &format!("{field}.id"))?,
        model: model_reference(value, "model", &format!("{field}.model"))?,
        texture: optional_string(value, "texture", &format!("{field}.texture"))?,
        texture_size: texture_size(value, &format!("{field}.textureSize"))?,
        invert_axis: axis_string(value, "invertAxis", &format!("{field}.invertAxis"), "xyz")?,
        translate: number_array_or_default(
            value,
            "translate",
            &format!("{field}.translate"),
            [0.0; 3],
        )?,
        rotate: number_array_or_default(value, "rotate", &format!("{field}.rotate"), [0.0; 3])?,
        mirror_texture: axis_string(
            value,
            "mirrorTexture",
            &format!("{field}.mirrorTexture"),
            "uv",
        )?,
        boxes,
        submodels,
    })
}

fn parse_boxes(value: &Json, field: &str) -> Result<Vec<CemBox>, OptifineCompatError> {
    let Some(boxes_value) = optional_value(value, "boxes") else {
        return Ok(Vec::new());
    };
    let items = boxes_value
        .as_array()
        .ok_or_else(|| invalid(format!("{field}.boxes"), "must be an array"))?;
    if items.len() > MAX_BOXES_PER_PART {
        return Err(limit(format!("{field}.boxes"), MAX_BOXES_PER_PART));
    }
    items
        .iter()
        .enumerate()
        .map(|(index, item)| parse_box(item, &format!("{field}.boxes[{index}]")))
        .collect()
}

fn parse_box(value: &Json, field: &str) -> Result<CemBox, OptifineCompatError> {
    expect_object(value, field)?;
    let coordinates = number_array(
        required_value(value, "coordinates", &format!("{field}.coordinates"))?,
        &format!("{field}.coordinates"),
    )?;
    let texture_offset =
        optional_number_array(value, "textureOffset", &format!("{field}.textureOffset"))?;
    let uv_north = optional_uv(value, "uvNorth", "uvFront", field)?;
    let uv_south = optional_uv(value, "uvSouth", "uvBack", field)?;
    let uv_east = optional_uv(value, "uvEast", "uvRight", field)?;
    let uv_west = optional_uv(value, "uvWest", "uvLeft", field)?;
    let uv_up = optional_uv(value, "uvUp", "uvUp", field)?;
    let uv_down = optional_uv(value, "uvDown", "uvDown", field)?;
    let has_face_uv = [uv_north, uv_south, uv_east, uv_west, uv_up, uv_down]
        .iter()
        .any(Option::is_some);
    if texture_offset.is_some() == has_face_uv {
        return Err(invalid(
            field,
            "must use textureOffset or per-face UVs, but not both",
        ));
    }
    Ok(CemBox {
        coordinates,
        texture_offset,
        uv_north,
        uv_south,
        uv_east,
        uv_west,
        uv_up,
        uv_down,
        size_add: optional_number(value, "sizeAdd", &format!("{field}.sizeAdd"))?.unwrap_or(0.0),
    })
}

fn optional_number_array<const N: usize>(
    value: &Json,
    key: &str,
    field: &str,
) -> Result<Option<[f32; N]>, OptifineCompatError> {
    optional_value(value, key)
        .map(|item| number_array(item, field))
        .transpose()
}

fn optional_uv(
    value: &Json,
    key: &str,
    alias: &str,
    field: &str,
) -> Result<Option<[f32; 4]>, OptifineCompatError> {
    let canonical = optional_value(value, key);
    let alternate = if alias == key {
        None
    } else {
        optional_value(value, alias)
    };
    if canonical.is_some() && alternate.is_some() {
        return Err(invalid(
            format!("{field}.{key}"),
            "canonical and alias UV keys cannot both be present",
        ));
    }
    canonical
        .or(alternate)
        .map(|item| number_array(item, &format!("{field}.{key}")))
        .transpose()
}

fn parse_animations(
    value: &Json,
    field: &str,
    total_animations: &mut usize,
) -> Result<Vec<BTreeMap<String, String>>, OptifineCompatError> {
    let Some(animations_value) = optional_value(value, "animations") else {
        return Ok(Vec::new());
    };
    let items = animations_value
        .as_array()
        .ok_or_else(|| invalid(format!("{field}.animations"), "must be an array"))?;
    *total_animations = total_animations.saturating_add(items.len());
    if *total_animations > MAX_ANIMATIONS {
        return Err(limit(format!("{field}.animations"), MAX_ANIMATIONS));
    }
    let mut result = Vec::with_capacity(items.len());
    for (index, item) in items.iter().enumerate() {
        let animation_field = format!("{field}.animations[{index}]");
        let members = expect_object(item, &animation_field)?;
        if members.is_empty() || members.len() > MAX_ANIMATION_VARIABLES {
            return Err(limit(
                format!("{animation_field} variables"),
                MAX_ANIMATION_VARIABLES,
            ));
        }
        let mut animation = BTreeMap::new();
        for (variable, expression) in members {
            if variable.is_empty() || variable.len() > 256 || variable.chars().any(char::is_control)
            {
                return Err(invalid(
                    format!("{animation_field}.{variable}"),
                    "invalid animation variable",
                ));
            }
            let expression = expression.as_str().ok_or_else(|| {
                invalid(
                    format!("{animation_field}.{variable}"),
                    "expression must be a string",
                )
            })?;
            if expression.trim().is_empty() || expression.len() > MAX_ANIMATION_EXPRESSION_BYTES {
                return Err(invalid(
                    format!("{animation_field}.{variable}"),
                    "expression must be non-empty and at most 4096 bytes",
                ));
            }
            animation.insert(variable.clone(), expression.to_owned());
        }
        result.push(animation);
    }
    Ok(result)
}

#[derive(Clone, Debug, PartialEq)]
struct PartGeometry {
    texture: Option<String>,
    texture_size: [u32; 2],
    invert_axis: String,
    translate: [f32; 3],
    rotate: [f32; 3],
    mirror_texture: String,
    boxes: Vec<CemBox>,
    submodels: Vec<ResolvedCemSubmodel>,
}

impl PartGeometry {
    fn empty(texture_size: [u32; 2]) -> Self {
        Self {
            texture: None,
            texture_size,
            invert_axis: String::new(),
            translate: [0.0; 3],
            rotate: [0.0; 3],
            mirror_texture: String::new(),
            boxes: Vec::new(),
            submodels: Vec::new(),
        }
    }
}

pub fn resolve_jem_with_jpm(
    jem: &JemModel,
    jpm_lookup: &BTreeMap<String, JpmModel>,
) -> Result<ResolvedEntityModel, OptifineCompatError> {
    let mut total_parts = 0usize;
    let mut models = Vec::with_capacity(jem.models.len());
    for entry in &jem.models {
        count_part(&mut total_parts, "resolved JEM models")?;
        let mut reference_stack = Vec::new();
        let mut base = match &entry.model {
            Some(path) => {
                resolve_jpm_reference(path, jpm_lookup, &mut reference_stack, 0, &mut total_parts)?
            }
            None => PartGeometry::empty(jem.texture_size),
        };
        if base.texture.is_none() {
            base.texture = jem.texture.clone();
        }
        if base.texture_size == DEFAULT_TEXTURE_SIZE {
            base.texture_size = jem.texture_size;
        }
        if !entry.invert_axis.is_empty() {
            base.invert_axis = entry.invert_axis.clone();
        }
        if entry.translate != [0.0; 3] {
            base.translate = entry.translate;
        }
        if entry.rotate != [0.0; 3] {
            base.rotate = entry.rotate;
        }
        if !entry.boxes.is_empty() {
            base.boxes = entry.boxes.clone();
        }
        if !entry.submodels.is_empty() {
            base.submodels = resolve_submodels(
                &entry.submodels,
                jpm_lookup,
                &mut reference_stack,
                1,
                &mut total_parts,
            )?;
        }
        models.push(ResolvedEntityPart {
            part: entry.part.clone(),
            id: entry.id.clone(),
            texture: base.texture,
            texture_size: base.texture_size,
            invert_axis: base.invert_axis,
            translate: base.translate,
            rotate: base.rotate,
            scale: entry.scale,
            attach: entry.attach,
            mirror_texture: base.mirror_texture,
            boxes: base.boxes,
            submodels: base.submodels,
            animations: entry.animations.clone(),
        });
    }
    Ok(ResolvedEntityModel {
        texture: jem.texture.clone(),
        texture_size: jem.texture_size,
        shadow_size: jem.shadow_size,
        models,
    })
}

fn resolve_jpm_reference(
    path: &str,
    lookup: &BTreeMap<String, JpmModel>,
    reference_stack: &mut Vec<String>,
    depth: usize,
    total_parts: &mut usize,
) -> Result<PartGeometry, OptifineCompatError> {
    validate_model_reference(path, "JPM model reference")?;
    if depth > MAX_SUBMODEL_DEPTH {
        return Err(limit("resolved JPM submodel depth", MAX_SUBMODEL_DEPTH));
    }
    if reference_stack.iter().any(|active| active == path) {
        return Err(OptifineCompatError::ModelReferenceCycle(path.to_owned()));
    }
    let model = lookup
        .get(path)
        .ok_or_else(|| OptifineCompatError::MissingModelReference(path.to_owned()))?;
    reference_stack.push(path.to_owned());
    let submodels = resolve_submodels(
        &model.submodels,
        lookup,
        reference_stack,
        depth + 1,
        total_parts,
    )?;
    reference_stack.pop();
    Ok(PartGeometry {
        texture: model.texture.clone(),
        texture_size: model.texture_size,
        invert_axis: model.invert_axis.clone(),
        translate: model.translate,
        rotate: model.rotate,
        mirror_texture: model.mirror_texture.clone(),
        boxes: model.boxes.clone(),
        submodels,
    })
}

fn resolve_submodels(
    submodels: &[CemSubmodel],
    lookup: &BTreeMap<String, JpmModel>,
    reference_stack: &mut Vec<String>,
    depth: usize,
    total_parts: &mut usize,
) -> Result<Vec<ResolvedCemSubmodel>, OptifineCompatError> {
    if depth > MAX_SUBMODEL_DEPTH && !submodels.is_empty() {
        return Err(limit("resolved submodel depth", MAX_SUBMODEL_DEPTH));
    }
    let mut result = Vec::with_capacity(submodels.len());
    for submodel in submodels {
        count_part(total_parts, "resolved submodels")?;
        let mut base = match &submodel.model {
            Some(path) => resolve_jpm_reference(path, lookup, reference_stack, depth, total_parts)?,
            None => PartGeometry::empty(submodel.texture_size),
        };
        if submodel.texture.is_some() {
            base.texture = submodel.texture.clone();
        }
        if submodel.texture_size != DEFAULT_TEXTURE_SIZE {
            base.texture_size = submodel.texture_size;
        }
        if !submodel.invert_axis.is_empty() {
            base.invert_axis = submodel.invert_axis.clone();
        }
        if submodel.translate != [0.0; 3] {
            base.translate = submodel.translate;
        }
        if submodel.rotate != [0.0; 3] {
            base.rotate = submodel.rotate;
        }
        if !submodel.mirror_texture.is_empty() {
            base.mirror_texture = submodel.mirror_texture.clone();
        }
        if !submodel.boxes.is_empty() {
            base.boxes = submodel.boxes.clone();
        }
        if !submodel.submodels.is_empty() {
            base.submodels = resolve_submodels(
                &submodel.submodels,
                lookup,
                reference_stack,
                depth + 1,
                total_parts,
            )?;
        }
        result.push(ResolvedCemSubmodel {
            id: submodel.id.clone(),
            texture: base.texture,
            texture_size: base.texture_size,
            invert_axis: base.invert_axis,
            translate: base.translate,
            rotate: base.rotate,
            mirror_texture: base.mirror_texture,
            boxes: base.boxes,
            submodels: base.submodels,
        });
    }
    Ok(result)
}

pub fn model_fingerprint(model: &ResolvedEntityModel) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    hash_option_string(&mut hash, model.texture.as_deref());
    hash_u32(&mut hash, model.texture_size[0]);
    hash_u32(&mut hash, model.texture_size[1]);
    hash_option_f32(&mut hash, model.shadow_size);
    hash_u64(&mut hash, model.models.len() as u64);
    for part in &model.models {
        hash_string(&mut hash, &part.part);
        hash_option_string(&mut hash, part.id.as_deref());
        hash_option_string(&mut hash, part.texture.as_deref());
        hash_u32(&mut hash, part.texture_size[0]);
        hash_u32(&mut hash, part.texture_size[1]);
        hash_string(&mut hash, &part.invert_axis);
        hash_vec_f32(&mut hash, &part.translate);
        hash_vec_f32(&mut hash, &part.rotate);
        hash_f32(&mut hash, part.scale);
        hash_bool(&mut hash, part.attach);
        hash_string(&mut hash, &part.mirror_texture);
        hash_boxes(&mut hash, &part.boxes);
        hash_submodels(&mut hash, &part.submodels);
        hash_u64(&mut hash, part.animations.len() as u64);
        for animation in &part.animations {
            hash_u64(&mut hash, animation.len() as u64);
            for (variable, expression) in animation {
                hash_string(&mut hash, variable);
                hash_string(&mut hash, expression);
            }
        }
    }
    hash
}

fn hash_bytes(hash: &mut u64, bytes: &[u8]) {
    for byte in bytes {
        *hash ^= u64::from(*byte);
        *hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
}

fn hash_u64(hash: &mut u64, value: u64) {
    hash_bytes(hash, &value.to_le_bytes());
}

fn hash_u32(hash: &mut u64, value: u32) {
    hash_bytes(hash, &value.to_le_bytes());
}

fn hash_bool(hash: &mut u64, value: bool) {
    hash_bytes(hash, &[u8::from(value)]);
}

fn hash_f32(hash: &mut u64, value: f32) {
    hash_u32(hash, value.to_bits());
}

fn hash_vec_f32(hash: &mut u64, values: &[f32]) {
    for value in values {
        hash_f32(hash, *value);
    }
}

fn hash_string(hash: &mut u64, value: &str) {
    hash_u64(hash, value.len() as u64);
    hash_bytes(hash, value.as_bytes());
}

fn hash_option_string(hash: &mut u64, value: Option<&str>) {
    match value {
        Some(value) => {
            hash_bool(hash, true);
            hash_string(hash, value);
        }
        None => hash_bool(hash, false),
    }
}

fn hash_option_f32(hash: &mut u64, value: Option<f32>) {
    match value {
        Some(value) => {
            hash_bool(hash, true);
            hash_f32(hash, value);
        }
        None => hash_bool(hash, false),
    }
}

fn hash_boxes(hash: &mut u64, boxes: &[CemBox]) {
    hash_u64(hash, boxes.len() as u64);
    for item in boxes {
        hash_vec_f32(hash, &item.coordinates);
        hash_option_f32_array(hash, item.texture_offset.as_ref());
        hash_option_f32_array(hash, item.uv_north.as_ref());
        hash_option_f32_array(hash, item.uv_south.as_ref());
        hash_option_f32_array(hash, item.uv_east.as_ref());
        hash_option_f32_array(hash, item.uv_west.as_ref());
        hash_option_f32_array(hash, item.uv_up.as_ref());
        hash_option_f32_array(hash, item.uv_down.as_ref());
        hash_f32(hash, item.size_add);
    }
}

fn hash_option_f32_array<const N: usize>(hash: &mut u64, value: Option<&[f32; N]>) {
    match value {
        Some(values) => {
            hash_bool(hash, true);
            hash_vec_f32(hash, values);
        }
        None => hash_bool(hash, false),
    }
}

fn hash_submodels(hash: &mut u64, submodels: &[ResolvedCemSubmodel]) {
    hash_u64(hash, submodels.len() as u64);
    for item in submodels {
        hash_option_string(hash, item.id.as_deref());
        hash_option_string(hash, item.texture.as_deref());
        hash_u32(hash, item.texture_size[0]);
        hash_u32(hash, item.texture_size[1]);
        hash_string(hash, &item.invert_axis);
        hash_vec_f32(hash, &item.translate);
        hash_vec_f32(hash, &item.rotate);
        hash_string(hash, &item.mirror_texture);
        hash_boxes(hash, &item.boxes);
        hash_submodels(hash, &item.submodels);
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CtmMethod {
    Ctm,
    CtmCompact,
    Horizontal,
    Vertical,
    HorizontalVertical,
    VerticalHorizontal,
    Top,
    Random,
    Repeat,
    Fixed,
    Overlay,
    OverlayCtm,
    OverlayRandom,
    OverlayRepeat,
    OverlayFixed,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CtmTile {
    Index(u32),
    Skip,
    Default,
    Sprite(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CtmConnect {
    Block,
    Tile,
    Material,
    State,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CtmSymmetry {
    None,
    Opposite,
    All,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CtmLayer {
    Solid,
    Cutout,
    CutoutMipped,
    Translucent,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HeightRange {
    pub min: Option<i32>,
    pub max: Option<i32>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CtmRule {
    pub method: CtmMethod,
    pub match_blocks: Vec<String>,
    pub match_tiles: Vec<String>,
    pub tiles: Vec<CtmTile>,
    pub connect: CtmConnect,
    pub faces: u8,
    pub weights: Option<Vec<u32>>,
    pub biomes: Vec<String>,
    pub biomes_inverted: bool,
    pub heights: Vec<HeightRange>,
    pub symmetry: Option<CtmSymmetry>,
    pub tint_index: Option<i32>,
    pub tint_block: Option<String>,
    pub layer: Option<CtmLayer>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub extra_properties: BTreeMap<String, String>,
}

pub fn parse_ctm_properties(input: &str) -> Result<CtmRule, OptifineCompatError> {
    if input.len() > MAX_PROPERTIES_BYTES {
        return Err(limit("CTM properties bytes", MAX_PROPERTIES_BYTES));
    }
    let properties = PropertiesFile::parse(input)
        .map_err(|error| invalid("CTM properties", &error.to_string()))?;
    validate_unique_properties(&properties, "CTM properties")?;

    let method_value = property(&properties, "method")
        .ok_or_else(|| OptifineCompatError::MissingField("CTM method".into()))?;
    let method = parse_ctm_method(method_value)?;
    let tiles_value = property(&properties, "tiles")
        .ok_or_else(|| OptifineCompatError::MissingField("CTM tiles".into()))?;
    let tiles = parse_tiles(tiles_value)?;
    validate_tile_count(method, &tiles, &properties)?;

    let match_blocks = property(&properties, "matchBlocks")
        .map(parse_block_ids)
        .transpose()?
        .unwrap_or_default();
    let match_tiles = property(&properties, "matchTiles")
        .map(parse_tile_ids)
        .transpose()?
        .unwrap_or_default();
    let connect = property(&properties, "connect")
        .map(parse_connect)
        .transpose()?
        .unwrap_or(CtmConnect::Block);
    let faces = property(&properties, "faces")
        .map(parse_faces)
        .transpose()?
        .unwrap_or(0b11_1111);
    let weights = property(&properties, "weights")
        .map(parse_weights)
        .transpose()?;
    if let Some(values) = &weights {
        if values.len() != tiles.len() {
            return Err(invalid(
                "CTM weights",
                "must contain exactly one weight per expanded tile",
            ));
        }
        if !matches!(method, CtmMethod::Random | CtmMethod::OverlayRandom) {
            return Err(invalid("CTM weights", "are only valid for random methods"));
        }
    }
    let (biomes, biomes_inverted) = property(&properties, "biomes")
        .map(parse_biomes)
        .transpose()?
        .unwrap_or_default();
    let heights = parse_heights(&properties)?;
    let symmetry = property(&properties, "symmetry")
        .map(parse_symmetry)
        .transpose()?;
    let tint_index = property(&properties, "tintIndex")
        .map(|value| parse_i32(value, "CTM tintIndex"))
        .transpose()?;
    let tint_block = property(&properties, "tintBlock")
        .map(|value| normalize_block_id(value, "CTM tintBlock"))
        .transpose()?;
    let layer = property(&properties, "layer")
        .map(parse_layer)
        .transpose()?;
    let width = property(&properties, "width")
        .map(|value| parse_positive_u32(value, "CTM width"))
        .transpose()?;
    let height = property(&properties, "height")
        .map(|value| parse_positive_u32(value, "CTM height"))
        .transpose()?;
    let mut extra_properties = BTreeMap::new();
    for key in properties.keys() {
        if !is_known_ctm_property(key) {
            extra_properties.insert(
                key.to_owned(),
                property(&properties, key).unwrap_or_default().to_owned(),
            );
        }
    }

    Ok(CtmRule {
        method,
        match_blocks,
        match_tiles,
        tiles,
        connect,
        faces,
        weights,
        biomes,
        biomes_inverted,
        heights,
        symmetry,
        tint_index,
        tint_block,
        layer,
        width,
        height,
        extra_properties,
    })
}

fn validate_unique_properties(
    properties: &PropertiesFile,
    field: &str,
) -> Result<(), OptifineCompatError> {
    let mut seen = BTreeSet::new();
    for key in properties.keys() {
        if !seen.insert(key) {
            return Err(invalid(format!("{field}.{key}"), "duplicate property key"));
        }
    }
    Ok(())
}

fn property<'a>(properties: &'a PropertiesFile, key: &str) -> Option<&'a str> {
    properties.get(key).map(str::trim)
}

fn parse_ctm_method(value: &str) -> Result<CtmMethod, OptifineCompatError> {
    match value.trim().to_ascii_lowercase().as_str() {
        "ctm" => Ok(CtmMethod::Ctm),
        "ctm_compact" => Ok(CtmMethod::CtmCompact),
        "horizontal" => Ok(CtmMethod::Horizontal),
        "vertical" => Ok(CtmMethod::Vertical),
        "horizontal+vertical" => Ok(CtmMethod::HorizontalVertical),
        "vertical+horizontal" => Ok(CtmMethod::VerticalHorizontal),
        "top" => Ok(CtmMethod::Top),
        "random" => Ok(CtmMethod::Random),
        "repeat" => Ok(CtmMethod::Repeat),
        "fixed" => Ok(CtmMethod::Fixed),
        "overlay" => Ok(CtmMethod::Overlay),
        "overlay_ctm" => Ok(CtmMethod::OverlayCtm),
        "overlay_random" => Ok(CtmMethod::OverlayRandom),
        "overlay_repeat" => Ok(CtmMethod::OverlayRepeat),
        "overlay_fixed" => Ok(CtmMethod::OverlayFixed),
        _ => Err(invalid("CTM method", "unsupported method")),
    }
}

fn parse_tiles(value: &str) -> Result<Vec<CtmTile>, OptifineCompatError> {
    let mut result = Vec::new();
    for token in value.split_whitespace() {
        if token == "<skip>" {
            push_tile(&mut result, CtmTile::Skip)?;
            continue;
        }
        if token == "<default>" {
            push_tile(&mut result, CtmTile::Default)?;
            continue;
        }
        if let Ok(index) = token.parse::<u32>() {
            push_tile(&mut result, CtmTile::Index(index))?;
            continue;
        }
        if let Some((start, end)) = token.split_once('-') {
            if let (Ok(start), Ok(end)) = (start.parse::<u32>(), end.parse::<u32>()) {
                if end < start {
                    return Err(invalid("CTM tiles", "tile ranges must be ascending"));
                }
                let count = (u64::from(end) - u64::from(start)) + 1;
                if count > (MAX_CTM_TILES - result.len()) as u64 {
                    return Err(limit("CTM expanded tiles", MAX_CTM_TILES));
                }
                for index in start..=end {
                    result.push(CtmTile::Index(index));
                }
                continue;
            }
        }
        validate_relative_resource_path(token, "CTM tile path")?;
        push_tile(&mut result, CtmTile::Sprite(token.to_owned()))?;
    }
    if result.is_empty() {
        return Err(invalid("CTM tiles", "must contain at least one tile"));
    }
    Ok(result)
}

fn push_tile(tiles: &mut Vec<CtmTile>, tile: CtmTile) -> Result<(), OptifineCompatError> {
    if tiles.len() >= MAX_CTM_TILES {
        return Err(limit("CTM expanded tiles", MAX_CTM_TILES));
    }
    tiles.push(tile);
    Ok(())
}

fn validate_tile_count(
    method: CtmMethod,
    tiles: &[CtmTile],
    properties: &PropertiesFile,
) -> Result<(), OptifineCompatError> {
    let exact = match method {
        CtmMethod::Ctm | CtmMethod::OverlayCtm => Some(47),
        CtmMethod::CtmCompact => Some(5),
        CtmMethod::Horizontal | CtmMethod::Vertical => Some(4),
        CtmMethod::HorizontalVertical | CtmMethod::VerticalHorizontal => Some(7),
        CtmMethod::Top | CtmMethod::Fixed | CtmMethod::OverlayFixed => Some(1),
        CtmMethod::Overlay => Some(17),
        CtmMethod::Random | CtmMethod::OverlayRandom => None,
        CtmMethod::Repeat | CtmMethod::OverlayRepeat => {
            let width = property(properties, "width")
                .map(|value| parse_positive_u32(value, "CTM width"))
                .transpose()?
                .ok_or_else(|| OptifineCompatError::MissingField("CTM width".into()))?;
            let height = property(properties, "height")
                .map(|value| parse_positive_u32(value, "CTM height"))
                .transpose()?
                .ok_or_else(|| OptifineCompatError::MissingField("CTM height".into()))?;
            let expected = u64::from(width) * u64::from(height);
            if expected > MAX_CTM_TILES as u64 {
                return Err(limit("CTM repeat pattern tiles", MAX_CTM_TILES));
            }
            Some(expected as usize)
        }
    };
    if let Some(expected) = exact {
        if tiles.len() != expected {
            return Err(invalid(
                "CTM tiles",
                &format!("method requires exactly {expected} expanded tiles"),
            ));
        }
    }
    Ok(())
}

fn parse_block_ids(value: &str) -> Result<Vec<String>, OptifineCompatError> {
    value
        .split_whitespace()
        .map(|token| normalize_block_id(token, "CTM matchBlocks"))
        .collect()
}

fn parse_tile_ids(value: &str) -> Result<Vec<String>, OptifineCompatError> {
    value
        .split_whitespace()
        .map(|token| normalize_resource_id(token, "CTM matchTiles"))
        .collect()
}

fn normalize_block_id(value: &str, field: &str) -> Result<String, OptifineCompatError> {
    let value = value.trim().to_ascii_lowercase();
    if value.is_empty() || value.contains('\\') || value.starts_with('/') {
        return Err(invalid(field, "invalid block identifier"));
    }
    let parts: Vec<&str> = value.split(':').collect();
    let (namespace, path, states): (&str, &str, &[&str]) = match parts.as_slice() {
        [path] => ("minecraft", path, &[]),
        [path, state, ..] if state.contains('=') => ("minecraft", path, &parts[1..]),
        [namespace, path, rest @ ..] => (namespace, path, rest),
        [] => return Err(invalid(field, "invalid block identifier")),
    };
    validate_namespace(namespace, field)?;
    validate_resource_path(path, field)?;
    for state in states {
        let Some((key, values)) = state.split_once('=') else {
            return Err(invalid(field, "block state filters must use key=value"));
        };
        if key.is_empty()
            || values.is_empty()
            || !key.bytes().all(is_resource_byte)
            || !values
                .bytes()
                .all(|byte| is_resource_byte(byte) || byte == b',')
        {
            return Err(invalid(field, "invalid block state filter"));
        }
    }
    let mut normalized = format!("{namespace}:{path}");
    for state in states {
        normalized.push(':');
        normalized.push_str(state);
    }
    Ok(normalized)
}

fn normalize_resource_id(value: &str, field: &str) -> Result<String, OptifineCompatError> {
    let value = value.trim().to_ascii_lowercase();
    if value.is_empty() || value.contains('\\') || value.starts_with('/') {
        return Err(invalid(field, "invalid resource identifier"));
    }
    let (namespace, path) = match value.split_once(':') {
        Some((namespace, path)) => (namespace, path),
        None => ("minecraft", value.as_str()),
    };
    validate_namespace(namespace, field)?;
    validate_resource_path(path, field)?;
    Ok(format!("{namespace}:{path}"))
}

fn validate_namespace(namespace: &str, field: &str) -> Result<(), OptifineCompatError> {
    if namespace.is_empty()
        || !namespace.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"_.-".contains(&byte)
        })
    {
        return Err(invalid(field, "invalid resource namespace"));
    }
    Ok(())
}

fn validate_resource_path(path: &str, field: &str) -> Result<(), OptifineCompatError> {
    validate_relative_resource_path(path, field)?;
    if !path.bytes().all(is_resource_byte) {
        return Err(invalid(field, "invalid resource path characters"));
    }
    Ok(())
}

fn validate_relative_resource_path(path: &str, field: &str) -> Result<(), OptifineCompatError> {
    if path.is_empty()
        || path.starts_with('/')
        || path.contains('\\')
        || path.len() > MAX_STRING_BYTES
    {
        return Err(invalid(field, "must be a bounded relative path"));
    }
    if path
        .split('/')
        .any(|part| part.is_empty() || part == "." || part == "..")
    {
        return Err(invalid(field, "contains an unsafe path component"));
    }
    Ok(())
}

fn is_resource_byte(byte: u8) -> bool {
    byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"_./-".contains(&byte)
}

fn parse_connect(value: &str) -> Result<CtmConnect, OptifineCompatError> {
    match value.to_ascii_lowercase().as_str() {
        "block" => Ok(CtmConnect::Block),
        "tile" => Ok(CtmConnect::Tile),
        "material" => Ok(CtmConnect::Material),
        "state" => Ok(CtmConnect::State),
        _ => Err(invalid(
            "CTM connect",
            "must be block, tile, material, or state",
        )),
    }
}

fn parse_faces(value: &str) -> Result<u8, OptifineCompatError> {
    let mut mask = 0u8;
    for face in value.split(|character: char| character.is_ascii_whitespace() || character == ',') {
        if face.is_empty() {
            continue;
        }
        let bit = match face.to_ascii_lowercase().as_str() {
            "north" => 1 << 0,
            "south" => 1 << 1,
            "east" => 1 << 2,
            "west" => 1 << 3,
            "top" => 1 << 4,
            "bottom" => 1 << 5,
            "sides" => 0b00_1111,
            "all" => 0b11_1111,
            _ => return Err(invalid("CTM faces", "contains an unknown face")),
        };
        mask |= bit;
    }
    if mask == 0 {
        return Err(invalid("CTM faces", "must select at least one face"));
    }
    Ok(mask)
}

fn parse_weights(value: &str) -> Result<Vec<u32>, OptifineCompatError> {
    value
        .split_whitespace()
        .map(|token| {
            let weight = token
                .parse::<u32>()
                .map_err(|_| invalid("CTM weights", "weights must be positive integers"))?;
            if weight == 0 {
                return Err(invalid("CTM weights", "weights must be positive integers"));
            }
            Ok(weight)
        })
        .collect()
}

fn parse_biomes(value: &str) -> Result<(Vec<String>, bool), OptifineCompatError> {
    let mut tokens = value.split_whitespace();
    let Some(first) = tokens.next() else {
        return Ok((Vec::new(), false));
    };
    let inverted = first.starts_with('!');
    let first = first.strip_prefix('!').unwrap_or(first);
    let mut biomes = vec![normalize_resource_id(first, "CTM biomes")?];
    for token in tokens {
        if token.starts_with('!') {
            return Err(invalid(
                "CTM biomes",
                "inversion marker is only valid on the first biome",
            ));
        }
        biomes.push(normalize_resource_id(token, "CTM biomes")?);
    }
    Ok((biomes, inverted))
}

fn parse_heights(properties: &PropertiesFile) -> Result<Vec<HeightRange>, OptifineCompatError> {
    let heights = property(properties, "heights");
    let minimum = property(properties, "minHeight");
    let maximum = property(properties, "maxHeight");
    if heights.is_some() && (minimum.is_some() || maximum.is_some()) {
        return Err(invalid(
            "CTM heights",
            "cannot be combined with minHeight or maxHeight",
        ));
    }
    if let Some(value) = heights {
        return value.split_whitespace().map(parse_height_range).collect();
    }
    if minimum.is_none() && maximum.is_none() {
        return Ok(Vec::new());
    }
    let min = minimum
        .map(|value| parse_i32(value, "CTM minHeight"))
        .transpose()?;
    let max = maximum
        .map(|value| parse_i32(value, "CTM maxHeight"))
        .transpose()?;
    if min.zip(max).is_some_and(|(min, max)| min > max) {
        return Err(invalid(
            "CTM heights",
            "minimum height exceeds maximum height",
        ));
    }
    Ok(vec![HeightRange { min, max }])
}

fn parse_height_range(value: &str) -> Result<HeightRange, OptifineCompatError> {
    if let Ok(height) = value.parse::<i32>() {
        return Ok(HeightRange {
            min: Some(height),
            max: Some(height),
        });
    }
    for (index, character) in value.char_indices().skip(1) {
        if character == '-' {
            if let (Ok(min), Ok(max)) = (
                value[..index].parse::<i32>(),
                value[index + 1..].parse::<i32>(),
            ) {
                if min <= max {
                    return Ok(HeightRange {
                        min: Some(min),
                        max: Some(max),
                    });
                }
                return Err(invalid("CTM heights", "height ranges must be ascending"));
            }
        }
    }
    Err(invalid(
        "CTM heights",
        "expected an integer or ascending integer range",
    ))
}

fn parse_i32(value: &str, field: &str) -> Result<i32, OptifineCompatError> {
    value
        .parse::<i32>()
        .map_err(|_| invalid(field, "must be a signed 32-bit integer"))
}

fn parse_positive_u32(value: &str, field: &str) -> Result<u32, OptifineCompatError> {
    let number = value
        .parse::<u32>()
        .map_err(|_| invalid(field, "must be a positive integer"))?;
    if number == 0 {
        return Err(invalid(field, "must be a positive integer"));
    }
    Ok(number)
}

fn parse_symmetry(value: &str) -> Result<CtmSymmetry, OptifineCompatError> {
    match value.to_ascii_lowercase().as_str() {
        "none" => Ok(CtmSymmetry::None),
        "opposite" => Ok(CtmSymmetry::Opposite),
        "all" => Ok(CtmSymmetry::All),
        _ => Err(invalid("CTM symmetry", "must be none, opposite, or all")),
    }
}

fn parse_layer(value: &str) -> Result<CtmLayer, OptifineCompatError> {
    match value.to_ascii_lowercase().as_str() {
        "solid" => Ok(CtmLayer::Solid),
        "cutout" => Ok(CtmLayer::Cutout),
        "cutout_mipped" => Ok(CtmLayer::CutoutMipped),
        "translucent" => Ok(CtmLayer::Translucent),
        _ => Err(invalid("CTM layer", "unsupported render layer")),
    }
}

fn is_known_ctm_property(key: &str) -> bool {
    matches!(
        key,
        "method"
            | "matchBlocks"
            | "matchTiles"
            | "tiles"
            | "connect"
            | "faces"
            | "weights"
            | "biomes"
            | "heights"
            | "minHeight"
            | "maxHeight"
            | "symmetry"
            | "tintIndex"
            | "tintBlock"
            | "layer"
            | "width"
            | "height"
    )
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EmissiveConfig {
    pub suffix_emissive: String,
}

pub fn parse_emissive_properties(input: &str) -> Result<EmissiveConfig, OptifineCompatError> {
    if input.len() > MAX_PROPERTIES_BYTES {
        return Err(limit("emissive properties bytes", MAX_PROPERTIES_BYTES));
    }
    let properties = PropertiesFile::parse(input)
        .map_err(|error| invalid("emissive properties", &error.to_string()))?;
    validate_unique_properties(&properties, "emissive properties")?;
    for key in properties.keys() {
        if key != "suffix.emissive" {
            return Err(invalid(
                format!("emissive properties.{key}"),
                "unsupported property",
            ));
        }
    }
    let suffix = property(&properties, "suffix.emissive").unwrap_or("_e");
    if suffix.is_empty()
        || suffix.len() > 64
        || suffix.contains('/')
        || suffix.contains('\\')
        || suffix.contains("..")
        || suffix == "."
        || suffix.chars().any(char::is_control)
    {
        return Err(invalid(
            "emissive suffix",
            "must be a non-empty filename suffix without separators or traversal",
        ));
    }
    Ok(EmissiveConfig {
        suffix_emissive: suffix.to_owned(),
    })
}

fn invalid(field: impl Into<String>, reason: &str) -> OptifineCompatError {
    OptifineCompatError::InvalidField {
        field: field.into(),
        reason: reason.to_owned(),
    }
}

fn limit(field: impl Into<String>, maximum: usize) -> OptifineCompatError {
    OptifineCompatError::LimitExceeded {
        field: field.into(),
        limit: maximum,
    }
}
