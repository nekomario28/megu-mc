// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later

//! Shrink-to-generic unified declarative mod bundle
//! (`mc-spine-mod-bundle/1`).
//!
//! One pure-JSON schema onboards an entire dimension/biome/creature/QoL mod
//! with **zero mod-specific Rust code**: a [`DeclarativeModBundle`] declares
//! the mod's portals, progression barriers, biome regions, creature specs,
//! and custom payload channels; [`UnifiedModRuntime::register_bundle`] folds
//! the whole document into one runtime registry that drives the existing
//! generic primitives ([`PortalRegistry`], [`ProgressionBarrierEngine`],
//! [`BiomeRegionBlender`], [`CreatureSpec`]) directly.
//!
//! Everything is fail-closed: the schema tag must match exactly, every id is
//! a validated [`ResourceLocation`], `mc_version_range` must parse as a
//! [`VersionRange`] that admits the install target, section counts are
//! bounded, and unknown portal trigger kinds / movement profiles / keys are
//! rejected rather than defaulted.
//!
//! Pure `std`: JSON goes through [`crate::data_loader`]'s bounded parser and
//! a deterministic sorted-key emitter (objects are emitted in `BTreeMap`
//! order), so [`DeclarativeModBundle::to_json`] roundtrips byte-for-byte.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use mc_spine_core::id::ResourceLocation;

use crate::creature_primitives::{
    CreatureSpec, MovementProfile, DEFAULT_BABY_GROWTH_TICKS, DEFAULT_BREED_COOLDOWN_TICKS,
};
use crate::data_loader::{parse_json, DataLoadError, JsonValue};
use crate::dimension_mod::{
    builtin_dimension_mod_profiles, PortalRegistry, PortalRule, PortalTrigger,
};
use crate::version_compat::{McVersion, VersionRange};
use crate::world_extensions::{
    BiomeRegion, BiomeRegionBlender, ProgressionBarrierEngine, ProgressionRule,
};

/// The only `schema` value a [`DeclarativeModBundle`] document may carry.
pub const BUNDLE_SCHEMA: &str = "mc-spine-mod-bundle/1";

/// Bound on each top-level section (`portals`, `progression_barriers`,
/// `biome_regions`, `creatures`, `custom_channels`) so a hostile bundle
/// cannot request unbounded allocations; the 4 MiB parser cap applies first.
const MAX_SECTION_ENTRIES: usize = 1024;
/// Bound on nested lists/maps inside a single section entry (tag borders,
/// advancements, item lists, biome overrides, effects).
const MAX_INNER_ENTRIES: usize = 512;

/// Why a bundle document was rejected or an install refused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BundleError {
    /// The document is not well-formed JSON within the loader's bounds.
    Json(String),
    /// `schema` is absent or not `"mc-spine-mod-bundle/1"`.
    UnknownSchema(String),
    /// A field failed fail-closed validation (type, id, bound, unknown key).
    Validation(String),
    /// `mc_version_range` does not admit the requested install target.
    IncompatibleVersion {
        /// Declaring mod id.
        mod_id: String,
        /// Target the install was attempted against.
        target: McVersion,
        /// The bundle's declared range string.
        declared: String,
    },
}

impl fmt::Display for BundleError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Json(msg) => write!(f, "bundle JSON: {msg}"),
            Self::UnknownSchema(found) => {
                write!(f, "unknown bundle schema {found:?} (want {BUNDLE_SCHEMA:?})")
            }
            Self::Validation(msg) => write!(f, "bundle validation: {msg}"),
            Self::IncompatibleVersion {
                mod_id,
                target,
                declared,
            } => write!(
                f,
                "bundle {mod_id:?} declares {declared:?}, which does not admit {target}"
            ),
        }
    }
}

impl std::error::Error for BundleError {}

/// One declarative mod: everything a dimension/biome/creature/QoL mod needs
/// to run natively, expressed as plain data.
///
/// `mod_id` and every referenced block/item/biome/dimension/effect/
/// advancement id is validated as a [`ResourceLocation`]; `#`-prefixed tag
/// references are allowed exactly where the runtime primitive accepts tags
/// (pool borders, creature item lists). `mc_version_range` is a
/// [`VersionRange`] string checked per install target.
#[derive(Clone, Debug, PartialEq)]
pub struct DeclarativeModBundle {
    /// Declaring mod id (a valid [`ResourceLocation`]).
    pub mod_id: String,
    /// Declared Minecraft compatibility range (Maven or SemVer syntax).
    pub mc_version_range: String,
    /// Portal rules feeding [`PortalRegistry`].
    pub portals: Vec<PortalRule>,
    /// Progression gates feeding [`ProgressionBarrierEngine`].
    pub progression_barriers: Vec<ProgressionRule>,
    /// Biome regions feeding [`BiomeRegionBlender`].
    pub biome_regions: Vec<BiomeRegion>,
    /// Creature specs feeding `creature_primitives`.
    pub creatures: Vec<CreatureSpec>,
    /// `CustomPacketPayload` channel ids the bundle listens on.
    pub custom_channels: Vec<String>,
}

impl DeclarativeModBundle {
    /// Parse and fully validate a `mc-spine-mod-bundle/1` document.
    ///
    /// # Errors
    ///
    /// Fail-closed on every deviation: malformed JSON, a missing/foreign
    /// `schema`, non-`ResourceLocation` ids, an unparsable
    /// `mc_version_range`, section counts above [`MAX_SECTION_ENTRIES`]
    /// (nested lists above [`MAX_INNER_ENTRIES`]), unknown keys, unknown
    /// portal trigger kinds, unknown movement profiles, and out-of-range
    /// numbers.
    pub fn from_json(text: &str) -> Result<Self, BundleError> {
        let root = parse_json(text.as_bytes()).map_err(|e| {
            BundleError::Json(match e {
                DataLoadError::Json(m) | DataLoadError::Io(m) | DataLoadError::Zip(m) => m,
            })
        })?;
        Self::from_ast(&root)
    }

    /// Re-run the full fail-closed validation over a (possibly
    /// hand-constructed) bundle by round-tripping its AST through the same
    /// checks [`from_json`](Self::from_json) applies.
    pub fn validate(&self) -> Result<(), BundleError> {
        Self::from_ast(&self.to_ast()).map(|_| ())
    }

    /// Deterministic JSON rendering: fixed schema layout, `BTreeMap`-sorted
    /// object keys, stable number formatting. Guaranteed to re-parse into an
    /// equal bundle for every valid bundle.
    pub fn to_json(&self) -> String {
        let mut out = String::with_capacity(512);
        emit_json(&self.to_ast(), &mut out, 0);
        out
    }

    /// The bundle as a [`JsonValue`] tree; `BTreeMap` object ordering *is*
    /// the deterministic sorted-key wire order.
    fn to_ast(&self) -> JsonValue {
        obj(vec![
            ("schema", jstr(BUNDLE_SCHEMA)),
            ("mod_id", jstr(&self.mod_id)),
            ("mc_version_range", jstr(&self.mc_version_range)),
            (
                "portals",
                JsonValue::Array(self.portals.iter().map(portal_ast).collect()),
            ),
            (
                "progression_barriers",
                JsonValue::Array(
                    self.progression_barriers
                        .iter()
                        .map(progression_ast)
                        .collect(),
                ),
            ),
            (
                "biome_regions",
                JsonValue::Array(self.biome_regions.iter().map(region_ast).collect()),
            ),
            (
                "creatures",
                JsonValue::Array(self.creatures.iter().map(creature_ast).collect()),
            ),
            ("custom_channels", str_arr(&self.custom_channels)),
        ])
    }

    /// Build a bundle from a parsed [`JsonValue`] root under the same
    /// fail-closed rules as [`from_json`](Self::from_json).
    fn from_ast(root: &JsonValue) -> Result<Self, BundleError> {
        const TOP_KEYS: &[&str] = &[
            "schema",
            "mod_id",
            "mc_version_range",
            "portals",
            "progression_barriers",
            "biome_regions",
            "creatures",
            "custom_channels",
        ];
        let top = obj_of(root, "bundle")?;
        check_keys(top, TOP_KEYS, "bundle")?;

        let schema = match top.get("schema") {
            Some(v) => str_of(v, "bundle.schema")?,
            None => return Err(BundleError::UnknownSchema("<missing>".to_string())),
        };
        if schema != BUNDLE_SCHEMA {
            return Err(BundleError::UnknownSchema(schema.to_string()));
        }

        let mod_id = rl_field(top, "mod_id", "bundle.mod_id")?;
        let mc_version_range = str_field(top, "mc_version_range", "bundle.mc_version_range")?;
        VersionRange::parse(mc_version_range)
            .map_err(|e| verr("bundle.mc_version_range", e.reason()))?;

        let bundle = Self {
            mod_id,
            mc_version_range: mc_version_range.to_string(),
            portals: section(top, "portals", parse_portal)?,
            progression_barriers: section(top, "progression_barriers", parse_progression)?,
            biome_regions: section(top, "biome_regions", parse_region)?,
            creatures: section(top, "creatures", parse_creature)?,
            custom_channels: {
                let list = str_list_field(top, "custom_channels", "bundle.custom_channels")?;
                bounded(list.len(), MAX_SECTION_ENTRIES, "bundle.custom_channels")?;
                for ch in &list {
                    check_rl(ch, "bundle.custom_channels[]")?;
                }
                list
            },
        };
        Ok(bundle)
    }
}

// ---------------------------------------------------------------------------
// Section parsers (fail-closed)
// ---------------------------------------------------------------------------

fn parse_portal(v: &JsonValue) -> Result<PortalRule, BundleError> {
    const CTX: &str = "portals[]";
    let obj = obj_of(v, CTX)?;
    check_keys(
        obj,
        &["name", "target_dim", "teleport_grace_ticks", "trigger"],
        CTX,
    )?;
    // `name` is a rule identifier, not a resource location: built-in rules
    // spell e.g. `ad_astra:moon:rocket_ascent` (dimension id + suffix).
    let name = str_field(obj, "name", CTX)?;
    if name.is_empty() {
        return Err(verr(CTX, "name must be non-empty"));
    }
    let name = name.to_string();
    let target_dim = rl_field(obj, "target_dim", CTX)?;
    let trigger = parse_trigger(field(obj, "trigger", CTX)?)?;
    let mut rule = PortalRule::new(name, trigger, target_dim);
    if let Some(v) = obj.get("teleport_grace_ticks") {
        rule.teleport_grace_ticks = u32_of(v, CTX)?;
    }
    Ok(rule)
}

fn parse_trigger(v: &JsonValue) -> Result<PortalTrigger, BundleError> {
    const CTX: &str = "portals[].trigger";
    let obj = obj_of(v, CTX)?;
    let kind = str_field(obj, "kind", CTX)?;
    match kind {
        "vertical_frame" => {
            check_keys(
                obj,
                &[
                    "kind",
                    "frame_block",
                    "igniter_item",
                    "min_width",
                    "min_height",
                    "max_width",
                    "max_height",
                    "portal_block",
                ],
                CTX,
            )?;
            let frame_block = rl_field(obj, "frame_block", CTX)?;
            let igniter_item = rl_field(obj, "igniter_item", CTX)?;
            let portal_block = rl_field(obj, "portal_block", CTX)?;
            let min_width = u32_field(obj, "min_width", CTX)?;
            let min_height = u32_field(obj, "min_height", CTX)?;
            let max_width = u32_field(obj, "max_width", CTX)?;
            let max_height = u32_field(obj, "max_height", CTX)?;
            if min_width == 0 || min_height == 0 {
                return Err(verr(CTX, "frame minimums must be >= 1"));
            }
            if max_width < min_width || max_height < min_height {
                return Err(verr(CTX, "frame maximums must be >= minimums"));
            }
            Ok(PortalTrigger::VerticalFrame {
                frame_block,
                min_width,
                min_height,
                max_width,
                max_height,
                igniter_item,
                portal_block,
            })
        }
        "horizontal_pool" => {
            check_keys(
                obj,
                &[
                    "kind",
                    "border_tags_or_blocks",
                    "fluid_block",
                    "min_size",
                    "drop_item",
                    "portal_block",
                ],
                CTX,
            )?;
            let border_tags_or_blocks =
                tag_list_field(obj, "border_tags_or_blocks", CTX)?;
            if border_tags_or_blocks.is_empty() {
                return Err(verr(CTX, "border_tags_or_blocks must be non-empty"));
            }
            let min_size = u32_field(obj, "min_size", CTX)?;
            if min_size == 0 {
                return Err(verr(CTX, "min_size must be >= 1"));
            }
            Ok(PortalTrigger::HorizontalPool {
                border_tags_or_blocks,
                fluid_block: rl_field(obj, "fluid_block", CTX)?,
                min_size,
                drop_item: rl_field(obj, "drop_item", CTX)?,
                portal_block: rl_field(obj, "portal_block", CTX)?,
            })
        }
        "altitude_threshold" => {
            check_keys(
                obj,
                &["kind", "min_y_trigger", "required_vehicle_or_item"],
                CTX,
            )?;
            Ok(PortalTrigger::AltitudeThreshold {
                min_y_trigger: i32_field(obj, "min_y_trigger", CTX)?,
                required_vehicle_or_item: opt_rl_field(
                    obj,
                    "required_vehicle_or_item",
                    CTX,
                )?,
            })
        }
        "direct_block_interact" => {
            check_keys(obj, &["kind", "block_id", "required_item"], CTX)?;
            Ok(PortalTrigger::DirectBlockInteract {
                block_id: rl_field(obj, "block_id", CTX)?,
                required_item: opt_rl_field(obj, "required_item", CTX)?,
            })
        }
        other => Err(verr(
            CTX,
            format!("unknown portal trigger kind '{other}'"),
        )),
    }
}

fn parse_progression(v: &JsonValue) -> Result<ProgressionRule, BundleError> {
    const CTX: &str = "progression_barriers[]";
    let obj = obj_of(v, CTX)?;
    check_keys(
        obj,
        &[
            "id",
            "dimension",
            "target_biomes",
            "required_advancements",
            "block_break_protected",
            "block_place_protected",
            "enforce_effects",
            "periodic_damage",
        ],
        CTX,
    )?;
    let target_biomes: BTreeSet<String> = rl_list_field(obj, "target_biomes", CTX)?
        .into_iter()
        .collect();
    if target_biomes.is_empty() {
        return Err(verr(CTX, "target_biomes must be non-empty"));
    }
    let required_advancements = rl_list_field(obj, "required_advancements", CTX)?;
    if required_advancements.is_empty() {
        return Err(verr(CTX, "required_advancements must be non-empty"));
    }

    let mut enforce_effects = Vec::new();
    if let Some(v) = obj.get("enforce_effects") {
        let items = v
            .as_array()
            .ok_or_else(|| verr(CTX, "enforce_effects must be an array"))?;
        bounded(items.len(), MAX_INNER_ENTRIES, CTX)?;
        for item in items {
            let eobj = obj_of(item, CTX)?;
            check_keys(eobj, &["effect", "duration_ticks", "amplifier"], CTX)?;
            enforce_effects.push((
                rl_field(eobj, "effect", CTX)?,
                u32_field(eobj, "duration_ticks", CTX)?,
                u8_field(eobj, "amplifier", CTX)?,
            ));
        }
    }

    let periodic_damage = match obj.get("periodic_damage") {
        None | Some(JsonValue::Null) => None,
        Some(v) => {
            let pobj = obj_of(v, CTX)?;
            check_keys(pobj, &["hearts", "interval_ticks"], CTX)?;
            let hearts = f64_field(pobj, "hearts", CTX)?;
            if hearts <= 0.0 {
                return Err(verr(CTX, "periodic_damage.hearts must be > 0"));
            }
            let interval_ticks = u32_field(pobj, "interval_ticks", CTX)?;
            if interval_ticks == 0 {
                return Err(verr(CTX, "periodic_damage.interval_ticks must be >= 1"));
            }
            Some((hearts as f32, interval_ticks))
        }
    };

    Ok(ProgressionRule {
        id: rl_field(obj, "id", CTX)?,
        dimension: rl_field(obj, "dimension", CTX)?,
        target_biomes,
        required_advancements,
        block_break_protected: bool_field(obj, "block_break_protected", CTX, true)?,
        block_place_protected: bool_field(obj, "block_place_protected", CTX, true)?,
        enforce_effects,
        periodic_damage,
    })
}

fn parse_region(v: &JsonValue) -> Result<BiomeRegion, BundleError> {
    const CTX: &str = "biome_regions[]";
    let obj = obj_of(v, CTX)?;
    check_keys(
        obj,
        &["mod_id", "dimension", "weight", "biome_overrides"],
        CTX,
    )?;
    let overrides = field(obj, "biome_overrides", CTX)?
        .as_object()
        .ok_or_else(|| verr(CTX, "biome_overrides must be an object"))?;
    bounded(overrides.len(), MAX_INNER_ENTRIES, CTX)?;
    if overrides.is_empty() {
        return Err(verr(CTX, "biome_overrides must be non-empty"));
    }
    let mut biome_overrides = BTreeMap::new();
    for (from, to) in overrides {
        check_rl(from, CTX)?;
        let to = str_of(to, CTX)?;
        check_rl(to, CTX)?;
        biome_overrides.insert(from.clone(), to.to_string());
    }
    Ok(BiomeRegion {
        mod_id: rl_field(obj, "mod_id", CTX)?,
        weight: u32_field(obj, "weight", CTX)?,
        dimension: rl_field(obj, "dimension", CTX)?,
        biome_overrides,
    })
}

fn parse_creature(v: &JsonValue) -> Result<CreatureSpec, BundleError> {
    const CTX: &str = "creatures[]";
    let obj = obj_of(v, CTX)?;
    check_keys(
        obj,
        &[
            "entity_type",
            "max_health",
            "tame_items",
            "tame_chance",
            "breed_items",
            "heal_items",
            "baby_growth_ticks",
            "breed_cooldown_ticks",
            "supports_wander_mode",
            "can_shoulder_perch",
            "rideable_when_tamed",
            "movement",
        ],
        CTX,
    )?;
    let max_health = f64_field(obj, "max_health", CTX)?;
    if max_health <= 0.0 {
        return Err(verr(CTX, "max_health must be > 0"));
    }
    let tame_chance = match obj.get("tame_chance") {
        None => 0.0,
        Some(v) => f64_of(v, CTX)?,
    };
    if !(0.0..=1.0).contains(&tame_chance) {
        return Err(verr(CTX, "tame_chance must lie in [0, 1]"));
    }

    let mut heal_items = Vec::new();
    if let Some(v) = obj.get("heal_items") {
        let items = v
            .as_array()
            .ok_or_else(|| verr(CTX, "heal_items must be an array"))?;
        bounded(items.len(), MAX_INNER_ENTRIES, CTX)?;
        for item in items {
            let hobj = obj_of(item, CTX)?;
            check_keys(hobj, &["item", "heal"], CTX)?;
            let heal = f64_field(hobj, "heal", CTX)?;
            if heal < 0.0 {
                return Err(verr(CTX, "heal_items[].heal must be >= 0"));
            }
            heal_items.push((tag_field(hobj, "item", CTX)?, heal as f32));
        }
    }

    Ok(CreatureSpec {
        entity_type: rl_field(obj, "entity_type", CTX)?,
        max_health: max_health as f32,
        tame_items: opt_tag_list_field(obj, "tame_items", CTX)?,
        tame_chance: tame_chance as f32,
        breed_items: opt_tag_list_field(obj, "breed_items", CTX)?,
        heal_items,
        baby_growth_ticks: opt_u32_field(
            obj,
            "baby_growth_ticks",
            CTX,
            DEFAULT_BABY_GROWTH_TICKS,
        )?,
        breed_cooldown_ticks: opt_u32_field(
            obj,
            "breed_cooldown_ticks",
            CTX,
            DEFAULT_BREED_COOLDOWN_TICKS,
        )?,
        supports_wander_mode: bool_field(obj, "supports_wander_mode", CTX, false)?,
        can_shoulder_perch: bool_field(obj, "can_shoulder_perch", CTX, false)?,
        rideable_when_tamed: bool_field(obj, "rideable_when_tamed", CTX, false)?,
        movement: parse_movement(field(obj, "movement", CTX)?)?,
    })
}

fn parse_movement(v: &JsonValue) -> Result<MovementProfile, BundleError> {
    const CTX: &str = "creatures[].movement";
    let obj = obj_of(v, CTX)?;
    let kind = str_field(obj, "kind", CTX)?;
    match kind {
        "land" => {
            check_keys(obj, &["kind", "ground_speed"], CTX)?;
            Ok(MovementProfile::Land {
                ground_speed: speed_field(obj, "ground_speed", CTX)?,
            })
        }
        "amphibious" => {
            check_keys(obj, &["kind", "land_speed", "swim_speed"], CTX)?;
            Ok(MovementProfile::Amphibious {
                land_speed: speed_field(obj, "land_speed", CTX)?,
                swim_speed: speed_field(obj, "swim_speed", CTX)?,
            })
        }
        "aquatic" => {
            check_keys(
                obj,
                &["kind", "swim_speed", "suffocates_in_air_ticks"],
                CTX,
            )?;
            Ok(MovementProfile::Aquatic {
                swim_speed: speed_field(obj, "swim_speed", CTX)?,
                suffocates_in_air_ticks: u32_field(obj, "suffocates_in_air_ticks", CTX)?,
            })
        }
        "gliding_flight" => {
            check_keys(
                obj,
                &["kind", "ground_speed", "fly_speed", "glide_fall_scale"],
                CTX,
            )?;
            Ok(MovementProfile::GlidingFlight {
                ground_speed: speed_field(obj, "ground_speed", CTX)?,
                fly_speed: speed_field(obj, "fly_speed", CTX)?,
                glide_fall_scale: speed_field(obj, "glide_fall_scale", CTX)?,
            })
        }
        "wall_climb" => {
            check_keys(obj, &["kind", "ground_speed", "climb_speed"], CTX)?;
            Ok(MovementProfile::WallClimb {
                ground_speed: speed_field(obj, "ground_speed", CTX)?,
                climb_speed: speed_field(obj, "climb_speed", CTX)?,
            })
        }
        other => Err(verr(
            CTX,
            format!("unknown movement profile kind '{other}'"),
        )),
    }
}

// ---------------------------------------------------------------------------
// Deterministic AST emitters (BTreeMap order = sorted wire order)
// ---------------------------------------------------------------------------

fn jstr(s: &str) -> JsonValue {
    JsonValue::String(s.to_string())
}

fn jnum(v: f64) -> JsonValue {
    JsonValue::Number(v)
}

fn jbool(v: bool) -> JsonValue {
    JsonValue::Bool(v)
}

fn obj(pairs: Vec<(&str, JsonValue)>) -> JsonValue {
    JsonValue::Object(
        pairs
            .into_iter()
            .map(|(k, v)| (k.to_string(), v))
            .collect(),
    )
}

fn opt_str(v: &Option<String>) -> JsonValue {
    match v {
        Some(s) => jstr(s),
        None => JsonValue::Null,
    }
}

fn str_arr(items: &[String]) -> JsonValue {
    JsonValue::Array(items.iter().map(|s| jstr(s)).collect())
}

fn str_arr_set(items: &BTreeSet<String>) -> JsonValue {
    JsonValue::Array(items.iter().map(|s| jstr(s)).collect())
}

fn portal_ast(rule: &PortalRule) -> JsonValue {
    obj(vec![
        ("name", jstr(&rule.name)),
        ("target_dim", jstr(&rule.target_dim)),
        (
            "teleport_grace_ticks",
            jnum(f64::from(rule.teleport_grace_ticks)),
        ),
        ("trigger", trigger_ast(&rule.trigger)),
    ])
}

fn trigger_ast(trigger: &PortalTrigger) -> JsonValue {
    match trigger {
        PortalTrigger::VerticalFrame {
            frame_block,
            min_width,
            min_height,
            max_width,
            max_height,
            igniter_item,
            portal_block,
        } => obj(vec![
            ("kind", jstr("vertical_frame")),
            ("frame_block", jstr(frame_block)),
            ("igniter_item", jstr(igniter_item)),
            ("min_width", jnum(f64::from(*min_width))),
            ("min_height", jnum(f64::from(*min_height))),
            ("max_width", jnum(f64::from(*max_width))),
            ("max_height", jnum(f64::from(*max_height))),
            ("portal_block", jstr(portal_block)),
        ]),
        PortalTrigger::HorizontalPool {
            border_tags_or_blocks,
            fluid_block,
            min_size,
            drop_item,
            portal_block,
        } => obj(vec![
            ("kind", jstr("horizontal_pool")),
            ("border_tags_or_blocks", str_arr(border_tags_or_blocks)),
            ("fluid_block", jstr(fluid_block)),
            ("min_size", jnum(f64::from(*min_size))),
            ("drop_item", jstr(drop_item)),
            ("portal_block", jstr(portal_block)),
        ]),
        PortalTrigger::AltitudeThreshold {
            min_y_trigger,
            required_vehicle_or_item,
        } => obj(vec![
            ("kind", jstr("altitude_threshold")),
            ("min_y_trigger", jnum(f64::from(*min_y_trigger))),
            (
                "required_vehicle_or_item",
                opt_str(required_vehicle_or_item),
            ),
        ]),
        PortalTrigger::DirectBlockInteract {
            block_id,
            required_item,
        } => obj(vec![
            ("kind", jstr("direct_block_interact")),
            ("block_id", jstr(block_id)),
            ("required_item", opt_str(required_item)),
        ]),
    }
}

fn progression_ast(rule: &ProgressionRule) -> JsonValue {
    obj(vec![
        ("id", jstr(&rule.id)),
        ("dimension", jstr(&rule.dimension)),
        ("target_biomes", str_arr_set(&rule.target_biomes)),
        (
            "required_advancements",
            str_arr(&rule.required_advancements),
        ),
        ("block_break_protected", jbool(rule.block_break_protected)),
        ("block_place_protected", jbool(rule.block_place_protected)),
        (
            "enforce_effects",
            JsonValue::Array(
                rule.enforce_effects
                    .iter()
                    .map(|(effect, duration_ticks, amplifier)| {
                        obj(vec![
                            ("effect", jstr(effect)),
                            ("duration_ticks", jnum(f64::from(*duration_ticks))),
                            ("amplifier", jnum(f64::from(*amplifier))),
                        ])
                    })
                    .collect(),
            ),
        ),
        (
            "periodic_damage",
            match rule.periodic_damage {
                Some((hearts, interval_ticks)) => obj(vec![
                    ("hearts", jnum(f64::from(hearts))),
                    ("interval_ticks", jnum(f64::from(interval_ticks))),
                ]),
                None => JsonValue::Null,
            },
        ),
    ])
}

fn region_ast(region: &BiomeRegion) -> JsonValue {
    obj(vec![
        ("mod_id", jstr(&region.mod_id)),
        ("dimension", jstr(&region.dimension)),
        ("weight", jnum(f64::from(region.weight))),
        (
            "biome_overrides",
            JsonValue::Object(
                region
                    .biome_overrides
                    .iter()
                    .map(|(k, v)| (k.clone(), jstr(v)))
                    .collect(),
            ),
        ),
    ])
}

fn creature_ast(spec: &CreatureSpec) -> JsonValue {
    obj(vec![
        ("entity_type", jstr(&spec.entity_type)),
        ("max_health", jnum(f64::from(spec.max_health))),
        ("tame_items", str_arr(&spec.tame_items)),
        ("tame_chance", jnum(f64::from(spec.tame_chance))),
        ("breed_items", str_arr(&spec.breed_items)),
        (
            "heal_items",
            JsonValue::Array(
                spec.heal_items
                    .iter()
                    .map(|(item, heal)| {
                        obj(vec![("item", jstr(item)), ("heal", jnum(f64::from(*heal)))])
                    })
                    .collect(),
            ),
        ),
        (
            "baby_growth_ticks",
            jnum(f64::from(spec.baby_growth_ticks)),
        ),
        (
            "breed_cooldown_ticks",
            jnum(f64::from(spec.breed_cooldown_ticks)),
        ),
        ("supports_wander_mode", jbool(spec.supports_wander_mode)),
        ("can_shoulder_perch", jbool(spec.can_shoulder_perch)),
        ("rideable_when_tamed", jbool(spec.rideable_when_tamed)),
        ("movement", movement_ast(&spec.movement)),
    ])
}

fn movement_ast(profile: &MovementProfile) -> JsonValue {
    match profile {
        MovementProfile::Land { ground_speed } => obj(vec![
            ("kind", jstr("land")),
            ("ground_speed", jnum(*ground_speed)),
        ]),
        MovementProfile::Amphibious {
            land_speed,
            swim_speed,
        } => obj(vec![
            ("kind", jstr("amphibious")),
            ("land_speed", jnum(*land_speed)),
            ("swim_speed", jnum(*swim_speed)),
        ]),
        MovementProfile::Aquatic {
            swim_speed,
            suffocates_in_air_ticks,
        } => obj(vec![
            ("kind", jstr("aquatic")),
            ("swim_speed", jnum(*swim_speed)),
            (
                "suffocates_in_air_ticks",
                jnum(f64::from(*suffocates_in_air_ticks)),
            ),
        ]),
        MovementProfile::GlidingFlight {
            ground_speed,
            fly_speed,
            glide_fall_scale,
        } => obj(vec![
            ("kind", jstr("gliding_flight")),
            ("ground_speed", jnum(*ground_speed)),
            ("fly_speed", jnum(*fly_speed)),
            ("glide_fall_scale", jnum(*glide_fall_scale)),
        ]),
        MovementProfile::WallClimb {
            ground_speed,
            climb_speed,
        } => obj(vec![
            ("kind", jstr("wall_climb")),
            ("ground_speed", jnum(*ground_speed)),
            ("climb_speed", jnum(*climb_speed)),
        ]),
    }
}

// ---------------------------------------------------------------------------
// Pretty JSON emitter (deterministic; objects arrive pre-sorted via BTreeMap)
// ---------------------------------------------------------------------------

fn emit_json(v: &JsonValue, out: &mut String, indent: usize) {
    use fmt::Write;
    match v {
        JsonValue::Null => out.push_str("null"),
        JsonValue::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        JsonValue::Number(n) => {
            if !n.is_finite() {
                out.push('0');
            } else {
                // Emit the shortest representation that reparses exactly:
                // values that came from f32 fields print as their f32
                // spelling ("0.35", not "0.3499999940395355").
                let narrowed = *n as f32;
                if f64::from(narrowed) == *n {
                    let _ = write!(out, "{narrowed}");
                } else {
                    let _ = write!(out, "{n}");
                }
            }
        }
        JsonValue::String(s) => emit_json_string(out, s),
        JsonValue::Array(items) => {
            if items.is_empty() {
                out.push_str("[]");
                return;
            }
            out.push_str("[\n");
            for (i, item) in items.iter().enumerate() {
                push_indent(out, indent + 1);
                emit_json(item, out, indent + 1);
                out.push_str(if i + 1 == items.len() { "\n" } else { ",\n" });
            }
            push_indent(out, indent);
            out.push(']');
        }
        JsonValue::Object(map) => {
            if map.is_empty() {
                out.push_str("{}");
                return;
            }
            out.push_str("{\n");
            let last = map.len() - 1;
            for (i, (key, value)) in map.iter().enumerate() {
                push_indent(out, indent + 1);
                emit_json_string(out, key);
                out.push_str(": ");
                emit_json(value, out, indent + 1);
                out.push_str(if i == last { "\n" } else { ",\n" });
            }
            push_indent(out, indent);
            out.push('}');
        }
    }
}

fn push_indent(out: &mut String, indent: usize) {
    for _ in 0..indent {
        out.push_str("  ");
    }
}

fn emit_json_string(out: &mut String, s: &str) {
    use fmt::Write;
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0c}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (' '..='~').contains(&c) => out.push(c),
            c => {
                let mut buf = [0u16; 2];
                for u in c.encode_utf16(&mut buf) {
                    let _ = write!(out, "\\u{u:04x}");
                }
            }
        }
    }
    out.push('"');
}

// ---------------------------------------------------------------------------
// Shared field helpers
// ---------------------------------------------------------------------------

fn verr(ctx: &str, reason: impl fmt::Display) -> BundleError {
    BundleError::Validation(format!("{ctx}: {reason}"))
}

fn bounded(len: usize, max: usize, ctx: &str) -> Result<(), BundleError> {
    if len > max {
        Err(verr(
            ctx,
            format!("{len} entries exceeds the {max} entry cap"),
        ))
    } else {
        Ok(())
    }
}

fn check_keys(
    obj: &BTreeMap<String, JsonValue>,
    allowed: &[&str],
    ctx: &str,
) -> Result<(), BundleError> {
    for key in obj.keys() {
        if !allowed.contains(&key.as_str()) {
            return Err(verr(ctx, format!("unknown key '{key}'")));
        }
    }
    Ok(())
}

fn check_rl(s: &str, ctx: &str) -> Result<(), BundleError> {
    ResourceLocation::parse(s).map_err(|e| verr(ctx, e))?;
    Ok(())
}

fn check_tag_or_rl(s: &str, ctx: &str) -> Result<(), BundleError> {
    match s.strip_prefix('#') {
        Some(tag) => check_rl(tag, ctx),
        None => check_rl(s, ctx),
    }
}

fn obj_of<'a>(
    v: &'a JsonValue,
    ctx: &str,
) -> Result<&'a BTreeMap<String, JsonValue>, BundleError> {
    v.as_object()
        .ok_or_else(|| verr(ctx, "must be a JSON object"))
}

fn field<'a>(
    obj: &'a BTreeMap<String, JsonValue>,
    key: &str,
    ctx: &str,
) -> Result<&'a JsonValue, BundleError> {
    obj.get(key)
        .ok_or_else(|| verr(ctx, format!("missing '{key}'")))
}

fn str_of<'a>(v: &'a JsonValue, ctx: &str) -> Result<&'a str, BundleError> {
    v.as_str().ok_or_else(|| verr(ctx, "must be a string"))
}

fn str_field<'a>(
    obj: &'a BTreeMap<String, JsonValue>,
    key: &str,
    ctx: &str,
) -> Result<&'a str, BundleError> {
    str_of(field(obj, key, ctx)?, ctx)
}

fn rl_field(
    obj: &BTreeMap<String, JsonValue>,
    key: &str,
    ctx: &str,
) -> Result<String, BundleError> {
    let s = str_field(obj, key, ctx)?;
    check_rl(s, ctx)?;
    Ok(s.to_string())
}

fn opt_rl_field(
    obj: &BTreeMap<String, JsonValue>,
    key: &str,
    ctx: &str,
) -> Result<Option<String>, BundleError> {
    match obj.get(key) {
        None | Some(JsonValue::Null) => Ok(None),
        Some(v) => {
            let s = str_of(v, ctx)?;
            check_rl(s, ctx)?;
            Ok(Some(s.to_string()))
        }
    }
}

fn tag_field(
    obj: &BTreeMap<String, JsonValue>,
    key: &str,
    ctx: &str,
) -> Result<String, BundleError> {
    let s = str_field(obj, key, ctx)?;
    check_tag_or_rl(s, ctx)?;
    Ok(s.to_string())
}

fn bool_of(v: &JsonValue, ctx: &str) -> Result<bool, BundleError> {
    v.as_bool()
        .ok_or_else(|| verr(ctx, "must be a boolean"))
}

fn bool_field(
    obj: &BTreeMap<String, JsonValue>,
    key: &str,
    ctx: &str,
    default: bool,
) -> Result<bool, BundleError> {
    match obj.get(key) {
        None => Ok(default),
        Some(v) => bool_of(v, ctx),
    }
}

fn f64_of(v: &JsonValue, ctx: &str) -> Result<f64, BundleError> {
    v.as_f64()
        .ok_or_else(|| verr(ctx, "must be a finite number"))
}

fn f64_field(
    obj: &BTreeMap<String, JsonValue>,
    key: &str,
    ctx: &str,
) -> Result<f64, BundleError> {
    f64_of(field(obj, key, ctx)?, ctx)
}

fn speed_field(
    obj: &BTreeMap<String, JsonValue>,
    key: &str,
    ctx: &str,
) -> Result<f64, BundleError> {
    let v = f64_field(obj, key, ctx)?;
    if v < 0.0 {
        return Err(verr(ctx, format!("{key} must be >= 0")));
    }
    Ok(v)
}

fn int_of(v: &JsonValue, ctx: &str) -> Result<f64, BundleError> {
    let n = f64_of(v, ctx)?;
    if n.fract() != 0.0 {
        return Err(verr(ctx, "must be an integer"));
    }
    Ok(n)
}

fn u32_of(v: &JsonValue, ctx: &str) -> Result<u32, BundleError> {
    let n = int_of(v, ctx)?;
    if !(0.0..=f64::from(u32::MAX)).contains(&n) {
        return Err(verr(ctx, "must be a u32 integer"));
    }
    Ok(n as u32)
}

fn u32_field(
    obj: &BTreeMap<String, JsonValue>,
    key: &str,
    ctx: &str,
) -> Result<u32, BundleError> {
    u32_of(field(obj, key, ctx)?, ctx)
}

fn opt_u32_field(
    obj: &BTreeMap<String, JsonValue>,
    key: &str,
    ctx: &str,
    default: u32,
) -> Result<u32, BundleError> {
    match obj.get(key) {
        None => Ok(default),
        Some(v) => u32_of(v, ctx),
    }
}

fn u8_field(
    obj: &BTreeMap<String, JsonValue>,
    key: &str,
    ctx: &str,
) -> Result<u8, BundleError> {
    let n = int_of(field(obj, key, ctx)?, ctx)?;
    if !(0.0..=f64::from(u8::MAX)).contains(&n) {
        return Err(verr(ctx, "must be a u8 integer"));
    }
    Ok(n as u8)
}

fn i32_field(
    obj: &BTreeMap<String, JsonValue>,
    key: &str,
    ctx: &str,
) -> Result<i32, BundleError> {
    let n = int_of(field(obj, key, ctx)?, ctx)?;
    if !(f64::from(i32::MIN)..=f64::from(i32::MAX)).contains(&n) {
        return Err(verr(ctx, "must be an i32 integer"));
    }
    Ok(n as i32)
}

fn string_list(
    v: &JsonValue,
    ctx: &str,
    check: fn(&str, &str) -> Result<(), BundleError>,
) -> Result<Vec<String>, BundleError> {
    let items = v
        .as_array()
        .ok_or_else(|| verr(ctx, "must be an array"))?;
    bounded(items.len(), MAX_INNER_ENTRIES, ctx)?;
    let mut out = Vec::with_capacity(items.len());
    for item in items {
        let s = str_of(item, ctx)?;
        check(s, ctx)?;
        out.push(s.to_string());
    }
    Ok(out)
}

fn rl_list_field(
    obj: &BTreeMap<String, JsonValue>,
    key: &str,
    ctx: &str,
) -> Result<Vec<String>, BundleError> {
    string_list(field(obj, key, ctx)?, ctx, check_rl)
}

fn tag_list_field(
    obj: &BTreeMap<String, JsonValue>,
    key: &str,
    ctx: &str,
) -> Result<Vec<String>, BundleError> {
    string_list(field(obj, key, ctx)?, ctx, check_tag_or_rl)
}

fn opt_tag_list_field(
    obj: &BTreeMap<String, JsonValue>,
    key: &str,
    ctx: &str,
) -> Result<Vec<String>, BundleError> {
    match obj.get(key) {
        None => Ok(Vec::new()),
        Some(v) => string_list(v, ctx, check_tag_or_rl),
    }
}

fn str_list_field(
    obj: &BTreeMap<String, JsonValue>,
    key: &str,
    ctx: &str,
) -> Result<Vec<String>, BundleError> {
    string_list(field(obj, key, ctx)?, ctx, |_, _| Ok(()))
}

fn section<T>(
    top: &BTreeMap<String, JsonValue>,
    key: &str,
    parse: fn(&JsonValue) -> Result<T, BundleError>,
) -> Result<Vec<T>, BundleError> {
    let items = field(top, key, "bundle")?
        .as_array()
        .ok_or_else(|| verr("bundle", format!("'{key}' must be an array")))?;
    bounded(items.len(), MAX_SECTION_ENTRIES, key)?;
    items.iter().map(parse).collect()
}

// ---------------------------------------------------------------------------
// Unified runtime: one registry for every declared section
// ---------------------------------------------------------------------------

/// Per-bundle install receipt returned by
/// [`UnifiedModRuntime::register_bundle`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BundleInstallSummary {
    /// Installed bundle's mod id.
    pub mod_id: String,
    /// Target the bundle was installed for.
    pub target: McVersion,
    pub portals_installed: usize,
    pub progression_barriers_installed: usize,
    pub biome_regions_installed: usize,
    pub creatures_installed: usize,
    /// Channel ids declared by the bundle.
    pub custom_channels_declared: usize,
    /// Channel ids that were not already registered.
    pub custom_channels_newly_registered: usize,
}

/// One runtime registry folding every section of every installed
/// [`DeclarativeModBundle`] — the proof that a new dimension/biome/
/// creature/QoL mod needs zero lines of mod-specific Rust.
#[derive(Clone, Debug)]
pub struct UnifiedModRuntime {
    portals: PortalRegistry,
    progression_rules: Vec<ProgressionRule>,
    biome_blender: BiomeRegionBlender,
    creatures: BTreeMap<String, CreatureSpec>,
    custom_channels: BTreeSet<String>,
    installed_mod_ids: Vec<String>,
}

impl Default for UnifiedModRuntime {
    fn default() -> Self {
        Self::new(1)
    }
}

impl UnifiedModRuntime {
    /// Empty runtime whose vanilla biome baseline carries
    /// `vanilla_region_weight` in the [`BiomeRegionBlender`] pick.
    pub fn new(vanilla_region_weight: u32) -> Self {
        Self {
            portals: PortalRegistry::new(),
            progression_rules: Vec::new(),
            biome_blender: BiomeRegionBlender::new(vanilla_region_weight),
            creatures: BTreeMap::new(),
            custom_channels: BTreeSet::new(),
            installed_mod_ids: Vec::new(),
        }
    }

    /// Validate `bundle` and install every declared section: portals into
    /// [`PortalRegistry`], barriers into the progression rule set, regions
    /// into [`BiomeRegionBlender`], creature specs keyed by `entity_type`,
    /// and channels into the custom-channel table.
    ///
    /// # Errors
    ///
    /// Fails closed (installing nothing) when `bundle` fails
    /// [`DeclarativeModBundle::validate`] or its `mc_version_range` does not
    /// admit `target` (`1.21.1` or `26.3`).
    pub fn register_bundle(
        &mut self,
        bundle: &DeclarativeModBundle,
        target: McVersion,
    ) -> Result<BundleInstallSummary, BundleError> {
        bundle.validate()?;
        let range = VersionRange::parse(&bundle.mc_version_range)
            .map_err(|e| verr("bundle.mc_version_range", e.reason()))?;
        if !range.contains(target) {
            return Err(BundleError::IncompatibleVersion {
                mod_id: bundle.mod_id.clone(),
                target,
                declared: bundle.mc_version_range.clone(),
            });
        }

        for rule in &bundle.portals {
            self.portals.register(rule.clone());
        }
        self.progression_rules
            .extend(bundle.progression_barriers.iter().cloned());
        for region in &bundle.biome_regions {
            self.biome_blender.add_region(region.clone());
        }
        for spec in &bundle.creatures {
            self.creatures.insert(spec.entity_type.clone(), spec.clone());
        }
        let mut new_channels = 0usize;
        for channel in &bundle.custom_channels {
            if self.custom_channels.insert(channel.clone()) {
                new_channels += 1;
            }
        }
        self.installed_mod_ids.push(bundle.mod_id.clone());

        Ok(BundleInstallSummary {
            mod_id: bundle.mod_id.clone(),
            target,
            portals_installed: bundle.portals.len(),
            progression_barriers_installed: bundle.progression_barriers.len(),
            biome_regions_installed: bundle.biome_regions.len(),
            creatures_installed: bundle.creatures.len(),
            custom_channels_declared: bundle.custom_channels.len(),
            custom_channels_newly_registered: new_channels,
        })
    }

    /// Every installed portal rule, in registration order.
    pub fn portal_registry(&self) -> &PortalRegistry {
        &self.portals
    }

    /// A [`ProgressionBarrierEngine`] over all installed barriers.
    pub fn progression_engine(&self) -> ProgressionBarrierEngine {
        ProgressionBarrierEngine::new(self.progression_rules.clone())
    }

    /// Installed progression rules, in registration order.
    pub fn progression_rules(&self) -> &[ProgressionRule] {
        &self.progression_rules
    }

    /// The blended biome region composer across all installed regions.
    pub fn biome_blender(&self) -> &BiomeRegionBlender {
        &self.biome_blender
    }

    /// Declared creature spec for `entity_type`, if any bundle installed it.
    pub fn creature_spec(&self, entity_type: &str) -> Option<&CreatureSpec> {
        self.creatures.get(entity_type)
    }

    /// All installed creature specs keyed by `entity_type`.
    pub fn creature_specs(&self) -> &BTreeMap<String, CreatureSpec> {
        &self.creatures
    }

    /// Every registered custom payload channel id.
    pub fn custom_channels(&self) -> &BTreeSet<String> {
        &self.custom_channels
    }

    /// Whether `channel` was declared by an installed bundle.
    pub fn is_channel_registered(&self, channel: &str) -> bool {
        self.custom_channels.contains(channel)
    }

    /// Mod ids of installed bundles, in install order.
    pub fn installed_mod_ids(&self) -> &[String] {
        &self.installed_mod_ids
    }
}

// ---------------------------------------------------------------------------
// Built-in profile export
// ---------------------------------------------------------------------------

/// Minecraft range covering both first-class targets (`1.21.1`, `26.3`)
/// declared on every exported built-in bundle.
const BUILTIN_BUNDLE_RANGE: &str = "[1.21,)";

/// Convert the existing hardcoded [`builtin_dimension_mod_profiles`] into
/// clean declarative bundles — proof the reference profiles are 100%
/// expressible as pure `mc-spine-mod-bundle/1` JSON data.
pub fn export_builtin_dimension_bundles() -> Vec<DeclarativeModBundle> {
    builtin_dimension_mod_profiles()
        .into_iter()
        .map(|profile| DeclarativeModBundle {
            mod_id: profile.mod_id.to_string(),
            mc_version_range: BUILTIN_BUNDLE_RANGE.to_string(),
            portals: profile.portal_rules,
            progression_barriers: Vec::new(),
            biome_regions: Vec::new(),
            creatures: Vec::new(),
            custom_channels: Vec::new(),
        })
        .collect()
}
