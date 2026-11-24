// SPDX-License-Identifier: LGPL-3.0-or-later

//! Shrink-to-generic unified declarative mod bundle
//! (`megu-mod-bundle/1`).
//!
//! One pure-JSON schema onboards an entire dimension/biome/creature/QoL mod
//! with **zero mod-specific Rust code**: a [`DeclarativeModBundle`] declares
//! the mod's portals, progression barriers, biome regions, creature specs,
//! spells, custom statuses, rituals, and custom payload channels; [`UnifiedModRuntime::register_bundle`] folds
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

use megu_core::id::ResourceLocation;

use crate::creature_primitives::{
    CreatureSpec, MovementProfile, DEFAULT_BABY_GROWTH_TICKS, DEFAULT_BREED_COOLDOWN_TICKS,
};
use crate::data_loader::{parse_json, DataLoadError, JsonValue};
use crate::dimension_mod::{
    builtin_dimension_mod_profiles, PortalRegistry, PortalRule, PortalTrigger,
};
use crate::magic_primitives::{
    CustomStatusRegistry, RitualRecipe, SpellCastState, SpellDefinition, SpellEffect, SpellForm,
    SpellNode, StatusEffectDefinition, MAX_ATTRIBUTE_MODIFIERS, MAX_EFFECTS_PER_STEP, MAX_REAGENTS,
    MAX_RITUAL_OFFSET, MAX_RITUAL_PATTERN_BLOCKS, MAX_SPELL_STEPS, MAX_STATUS_DEFS,
};
use crate::version_compat::{McVersion, VersionRange};
use crate::world_extensions::{
    BiomeRegion, BiomeRegionBlender, ProgressionBarrierEngine, ProgressionRule,
};

/// The only `schema` value a [`DeclarativeModBundle`] document may carry.
pub const BUNDLE_SCHEMA: &str = "megu-mod-bundle/1";

/// Bound on each top-level section (`portals`, `progression_barriers`,
/// `biome_regions`, `creatures`, `custom_channels`, `spells`,
/// `custom_statuses`, `rituals`) so a hostile bundle
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
    /// `schema` is absent or not `"megu-mod-bundle/1"`.
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
                write!(
                    f,
                    "unknown bundle schema {found:?} (want {BUNDLE_SCHEMA:?})"
                )
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
    /// Optional composable spell definitions.
    pub spells: Vec<SpellDefinition>,
    /// Optional custom status effect definitions.
    pub custom_statuses: Vec<StatusEffectDefinition>,
    /// Optional multiblock ritual recipes.
    pub rituals: Vec<RitualRecipe>,
}

impl DeclarativeModBundle {
    /// Parse and fully validate a `megu-mod-bundle/1` document.
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
        // Cast templates contain runtime counters that are intentionally absent
        // from JSON, so validate the original values before serialization.
        for spell in &self.spells {
            spell
                .validate()
                .map_err(|e| verr("spells", format!("{e:?}")))?;
        }
        // JSON rendering replaces non-finite numbers with 0. Check the
        // original definitions before they reach the emitter.
        let mut statuses = CustomStatusRegistry::default();
        for status in &self.custom_statuses {
            statuses
                .register(status.clone())
                .map_err(|e| verr("custom_statuses", format!("{e:?}")))?;
        }
        for ritual in &self.rituals {
            // The primitive validator uses signed abs(). Reject i32::MIN
            // before it can overflow on a hand-constructed bundle.
            for &(dx, dy, dz) in ritual.pattern.keys() {
                if [dx, dy, dz]
                    .into_iter()
                    .any(|v| !(-MAX_RITUAL_OFFSET..=MAX_RITUAL_OFFSET).contains(&v))
                {
                    return Err(verr("rituals", "pattern offset out of range"));
                }
            }
            ritual
                .validate()
                .map_err(|e| verr("rituals", format!("{e:?}")))?;
        }
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
        let mut fields = vec![
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
        ];
        if !self.spells.is_empty() {
            fields.push((
                "spells",
                JsonValue::Array(self.spells.iter().map(spell_ast).collect()),
            ));
        }
        if !self.custom_statuses.is_empty() {
            fields.push((
                "custom_statuses",
                JsonValue::Array(self.custom_statuses.iter().map(status_ast).collect()),
            ));
        }
        if !self.rituals.is_empty() {
            fields.push((
                "rituals",
                JsonValue::Array(self.rituals.iter().map(ritual_ast).collect()),
            ));
        }
        obj(fields)
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
            "spells",
            "custom_statuses",
            "rituals",
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
            spells: optional_section(top, "spells", parse_spell)?,
            custom_statuses: optional_section(top, "custom_statuses", parse_status)?,
            rituals: optional_section(top, "rituals", parse_ritual)?,
        };
        let mut spell_ids = BTreeSet::new();
        for spell in &bundle.spells {
            if !spell_ids.insert(&spell.id) {
                return Err(verr("spells", format!("duplicate id '{}'", spell.id)));
            }
        }
        let mut statuses = CustomStatusRegistry::default();
        for def in &bundle.custom_statuses {
            statuses
                .register(def.clone())
                .map_err(|e| verr("custom_statuses", format!("{e:?}")))?;
        }
        let mut ritual_ids = BTreeSet::new();
        for ritual in &bundle.rituals {
            if !ritual_ids.insert(&ritual.id) {
                return Err(verr("rituals", format!("duplicate id '{}'", ritual.id)));
            }
        }
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
            let border_tags_or_blocks = tag_list_field(obj, "border_tags_or_blocks", CTX)?;
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
                required_vehicle_or_item: opt_rl_field(obj, "required_vehicle_or_item", CTX)?,
            })
        }
        "direct_block_interact" => {
            check_keys(obj, &["kind", "block_id", "required_item"], CTX)?;
            Ok(PortalTrigger::DirectBlockInteract {
                block_id: rl_field(obj, "block_id", CTX)?,
                required_item: opt_rl_field(obj, "required_item", CTX)?,
            })
        }
        other => Err(verr(CTX, format!("unknown portal trigger kind '{other}'"))),
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
        baby_growth_ticks: opt_u32_field(obj, "baby_growth_ticks", CTX, DEFAULT_BABY_GROWTH_TICKS)?,
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

// The magic sections are optional in bundle/1. Their nested shapes follow
// the public magic_primitives types, and every enum variant has an explicit
// kind so unknown variants cannot silently become defaults.
fn parse_spell(v: &JsonValue) -> Result<SpellDefinition, BundleError> {
    const CTX: &str = "spells[]";
    let o = obj_of(v, CTX)?;
    check_keys(
        o,
        &[
            "id",
            "mana_cost",
            "base_cooldown_ticks",
            "cast",
            "interrupt_on_damage",
            "interrupt_on_move",
            "steps",
        ],
        CTX,
    )?;
    let steps = field(o, "steps", CTX)?
        .as_array()
        .ok_or_else(|| verr(CTX, "steps must be an array"))?;
    bounded(steps.len(), MAX_SPELL_STEPS, CTX)?;
    let mut nodes = Vec::with_capacity(steps.len());
    for step in steps {
        let node = obj_of(step, "spells[].steps[]")?;
        check_keys(node, &["form", "effects"], "spells[].steps[]")?;
        let effects = field(node, "effects", CTX)?
            .as_array()
            .ok_or_else(|| verr(CTX, "effects must be an array"))?;
        bounded(effects.len(), MAX_EFFECTS_PER_STEP, CTX)?;
        nodes.push(SpellNode {
            form: parse_spell_form(field(node, "form", CTX)?)?,
            effects: effects
                .iter()
                .map(parse_spell_effect)
                .collect::<Result<_, _>>()?,
        });
    }
    let def = SpellDefinition {
        id: rl_field(o, "id", CTX)?,
        mana_cost: f32_field(o, "mana_cost", CTX)?,
        base_cooldown_ticks: u32_field(o, "base_cooldown_ticks", CTX)?,
        cast: parse_spell_cast(field(o, "cast", CTX)?)?,
        interrupt_on_damage: bool_field(o, "interrupt_on_damage", CTX, false)?,
        interrupt_on_move: bool_field(o, "interrupt_on_move", CTX, false)?,
        steps: nodes,
    };
    def.validate().map_err(|e| verr(CTX, format!("{e:?}")))?;
    Ok(def)
}

fn parse_spell_cast(v: &JsonValue) -> Result<SpellCastState, BundleError> {
    const CTX: &str = "spells[].cast";
    let o = obj_of(v, CTX)?;
    match str_field(o, "kind", CTX)? {
        "instant" => {
            check_keys(o, &["kind"], CTX)?;
            Ok(SpellCastState::Instant)
        }
        "cast_time" => {
            check_keys(o, &["kind", "total_ticks"], CTX)?;
            Ok(SpellCastState::cast_time(u32_field(o, "total_ticks", CTX)?))
        }
        "channeled" => {
            check_keys(
                o,
                &["kind", "interval_ticks", "max_ticks", "mana_per_pulse"],
                CTX,
            )?;
            Ok(SpellCastState::channeled(
                u32_field(o, "interval_ticks", CTX)?,
                u32_field(o, "max_ticks", CTX)?,
                f32_field(o, "mana_per_pulse", CTX)?,
            ))
        }
        kind => Err(verr(CTX, format!("unknown cast kind '{kind}'"))),
    }
}

fn parse_spell_form(v: &JsonValue) -> Result<SpellForm, BundleError> {
    const CTX: &str = "spells[].steps[].form";
    let o = obj_of(v, CTX)?;
    match str_field(o, "kind", CTX)? {
        "self_cast" => {
            check_keys(o, &["kind"], CTX)?;
            Ok(SpellForm::SelfCast)
        }
        "touch" => {
            check_keys(o, &["kind", "range"], CTX)?;
            Ok(SpellForm::Touch {
                range: f32_field(o, "range", CTX)?,
            })
        }
        "projectile" => {
            check_keys(o, &["kind", "speed", "gravity", "max_ticks"], CTX)?;
            Ok(SpellForm::Projectile {
                speed: f32_field(o, "speed", CTX)?,
                gravity: f32_field(o, "gravity", CTX)?,
                max_ticks: u32_field(o, "max_ticks", CTX)?,
            })
        }
        "raycast_beam" => {
            check_keys(o, &["kind", "range", "pierce_cap"], CTX)?;
            Ok(SpellForm::RaycastBeam {
                range: f32_field(o, "range", CTX)?,
                pierce_cap: u32_field(o, "pierce_cap", CTX)?,
            })
        }
        "aoe_zone" => {
            check_keys(
                o,
                &["kind", "radius", "duration_ticks", "pulse_interval"],
                CTX,
            )?;
            Ok(SpellForm::AoeZone {
                radius: f32_field(o, "radius", CTX)?,
                duration_ticks: u32_field(o, "duration_ticks", CTX)?,
                pulse_interval: u32_field(o, "pulse_interval", CTX)?,
            })
        }
        "chain" => {
            check_keys(o, &["kind", "max_hops", "hop_radius"], CTX)?;
            Ok(SpellForm::Chain {
                max_hops: u32_field(o, "max_hops", CTX)?,
                hop_radius: f32_field(o, "hop_radius", CTX)?,
            })
        }
        "orbit" => {
            check_keys(o, &["kind", "count", "radius", "duration_ticks"], CTX)?;
            Ok(SpellForm::Orbit {
                count: u32_field(o, "count", CTX)?,
                radius: f32_field(o, "radius", CTX)?,
                duration_ticks: u32_field(o, "duration_ticks", CTX)?,
            })
        }
        "summon" => {
            check_keys(
                o,
                &["kind", "entity_type", "duration_ticks", "max_active"],
                CTX,
            )?;
            Ok(SpellForm::Summon {
                entity_type: rl_field(o, "entity_type", CTX)?,
                duration_ticks: u32_field(o, "duration_ticks", CTX)?,
                max_active: u32_field(o, "max_active", CTX)?,
            })
        }
        kind => Err(verr(CTX, format!("unknown form kind '{kind}'"))),
    }
}

fn parse_spell_effect(v: &JsonValue) -> Result<SpellEffect, BundleError> {
    const CTX: &str = "spells[].steps[].effects[]";
    let o = obj_of(v, CTX)?;
    match str_field(o, "kind", CTX)? {
        "damage" => {
            check_keys(o, &["kind", "school", "base_amount"], CTX)?;
            Ok(SpellEffect::Damage {
                school: rl_field(o, "school", CTX)?,
                base_amount: f32_field(o, "base_amount", CTX)?,
            })
        }
        "heal" => {
            check_keys(o, &["kind", "base_amount"], CTX)?;
            Ok(SpellEffect::Heal {
                base_amount: f32_field(o, "base_amount", CTX)?,
            })
        }
        "shield" => {
            check_keys(o, &["kind", "amount", "duration_ticks"], CTX)?;
            Ok(SpellEffect::Shield {
                amount: f32_field(o, "amount", CTX)?,
                duration_ticks: u32_field(o, "duration_ticks", CTX)?,
            })
        }
        "apply_status" => {
            check_keys(
                o,
                &["kind", "status_id", "amplifier", "duration_ticks"],
                CTX,
            )?;
            Ok(SpellEffect::ApplyStatus {
                status_id: rl_field(o, "status_id", CTX)?,
                amplifier: u8_field(o, "amplifier", CTX)?,
                duration_ticks: u32_field(o, "duration_ticks", CTX)?,
            })
        }
        "knockback" => {
            check_keys(o, &["kind", "strength"], CTX)?;
            Ok(SpellEffect::Knockback {
                strength: f32_field(o, "strength", CTX)?,
            })
        }
        "blink" => {
            check_keys(o, &["kind", "max_distance"], CTX)?;
            Ok(SpellEffect::Blink {
                max_distance: f32_field(o, "max_distance", CTX)?,
            })
        }
        "conjure_light" => {
            check_keys(o, &["kind", "level", "duration_ticks"], CTX)?;
            Ok(SpellEffect::ConjureLight {
                level: u8_field(o, "level", CTX)?,
                duration_ticks: u32_field(o, "duration_ticks", CTX)?,
            })
        }
        "break_block" => {
            check_keys(o, &["kind", "max_tier"], CTX)?;
            Ok(SpellEffect::BreakBlock {
                max_tier: u8_field(o, "max_tier", CTX)?,
            })
        }
        "emit_vfx" => {
            check_keys(o, &["kind", "vfx_id"], CTX)?;
            Ok(SpellEffect::EmitVfx {
                vfx_id: rl_field(o, "vfx_id", CTX)?,
            })
        }
        kind => Err(verr(CTX, format!("unknown effect kind '{kind}'"))),
    }
}

fn parse_status(v: &JsonValue) -> Result<StatusEffectDefinition, BundleError> {
    const CTX: &str = "custom_statuses[]";
    let o = obj_of(v, CTX)?;
    check_keys(
        o,
        &[
            "id",
            "max_amplifier",
            "pulse_interval_ticks",
            "damage_per_pulse",
            "heal_per_pulse",
            "absorption_per_level",
            "frozen",
            "rooted",
            "silenced",
            "disarmed",
            "attribute_modifiers",
            "vfx_aura",
        ],
        CTX,
    )?;
    let modifiers = match o.get("attribute_modifiers") {
        None => BTreeMap::new(),
        Some(v) => {
            let entries = obj_of(v, "custom_statuses[].attribute_modifiers")?;
            bounded(entries.len(), MAX_ATTRIBUTE_MODIFIERS, CTX)?;
            entries
                .iter()
                .map(|(name, value)| {
                    check_rl(name, "custom_statuses[].attribute_modifiers")?;
                    Ok((
                        name.clone(),
                        f32_of(value, "custom_statuses[].attribute_modifiers")?,
                    ))
                })
                .collect::<Result<_, BundleError>>()?
        }
    };
    Ok(StatusEffectDefinition {
        id: rl_field(o, "id", CTX)?,
        max_amplifier: match o.get("max_amplifier") {
            None => 0,
            Some(_) => u8_field(o, "max_amplifier", CTX)?,
        },
        pulse_interval_ticks: opt_u32_field(o, "pulse_interval_ticks", CTX, 0)?,
        damage_per_pulse: opt_f32_field(o, "damage_per_pulse", CTX, 0.0)?,
        heal_per_pulse: opt_f32_field(o, "heal_per_pulse", CTX, 0.0)?,
        absorption_per_level: opt_f32_field(o, "absorption_per_level", CTX, 0.0)?,
        frozen: bool_field(o, "frozen", CTX, false)?,
        rooted: bool_field(o, "rooted", CTX, false)?,
        silenced: bool_field(o, "silenced", CTX, false)?,
        disarmed: bool_field(o, "disarmed", CTX, false)?,
        attribute_modifiers: modifiers,
        vfx_aura: opt_rl_field(o, "vfx_aura", CTX)?,
    })
}

fn parse_ritual(v: &JsonValue) -> Result<RitualRecipe, BundleError> {
    const CTX: &str = "rituals[]";
    let o = obj_of(v, CTX)?;
    check_keys(
        o,
        &[
            "id",
            "pattern",
            "reagents",
            "duration_ticks",
            "mana_per_tick",
        ],
        CTX,
    )?;
    let blocks = field(o, "pattern", CTX)?
        .as_array()
        .ok_or_else(|| verr(CTX, "pattern must be an array"))?;
    bounded(blocks.len(), MAX_RITUAL_PATTERN_BLOCKS, CTX)?;
    let mut pattern = BTreeMap::new();
    for block in blocks {
        let b = obj_of(block, "rituals[].pattern[]")?;
        check_keys(b, &["dx", "dy", "dz", "block"], "rituals[].pattern[]")?;
        let offset = (
            i32_field(b, "dx", CTX)?,
            i32_field(b, "dy", CTX)?,
            i32_field(b, "dz", CTX)?,
        );
        if [offset.0, offset.1, offset.2]
            .into_iter()
            .any(|v| !(-MAX_RITUAL_OFFSET..=MAX_RITUAL_OFFSET).contains(&v))
        {
            return Err(verr(CTX, "pattern offset out of range"));
        }
        if pattern.insert(offset, rl_field(b, "block", CTX)?).is_some() {
            return Err(verr(CTX, "duplicate pattern offset"));
        }
    }
    let items = obj_of(field(o, "reagents", CTX)?, "rituals[].reagents")?;
    bounded(items.len(), MAX_REAGENTS, CTX)?;
    let mut reagents = BTreeMap::new();
    for (item, count) in items {
        check_rl(item, CTX)?;
        reagents.insert(item.clone(), u32_of(count, CTX)?);
    }
    let recipe = RitualRecipe {
        id: rl_field(o, "id", CTX)?,
        pattern,
        reagents,
        duration_ticks: u32_field(o, "duration_ticks", CTX)?,
        mana_per_tick: f32_field(o, "mana_per_tick", CTX)?,
    };
    recipe.validate().map_err(|e| verr(CTX, format!("{e:?}")))?;
    Ok(recipe)
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
            check_keys(obj, &["kind", "swim_speed", "suffocates_in_air_ticks"], CTX)?;
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
    JsonValue::Object(pairs.into_iter().map(|(k, v)| (k.to_string(), v)).collect())
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
        ("baby_growth_ticks", jnum(f64::from(spec.baby_growth_ticks))),
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

fn spell_ast(def: &SpellDefinition) -> JsonValue {
    obj(vec![
        ("id", jstr(&def.id)),
        ("mana_cost", jnum(f64::from(def.mana_cost))),
        (
            "base_cooldown_ticks",
            jnum(f64::from(def.base_cooldown_ticks)),
        ),
        ("cast", spell_cast_ast(&def.cast)),
        ("interrupt_on_damage", jbool(def.interrupt_on_damage)),
        ("interrupt_on_move", jbool(def.interrupt_on_move)),
        (
            "steps",
            JsonValue::Array(
                def.steps
                    .iter()
                    .map(|node| {
                        obj(vec![
                            ("form", spell_form_ast(&node.form)),
                            (
                                "effects",
                                JsonValue::Array(
                                    node.effects.iter().map(spell_effect_ast).collect(),
                                ),
                            ),
                        ])
                    })
                    .collect(),
            ),
        ),
    ])
}

fn spell_cast_ast(cast: &SpellCastState) -> JsonValue {
    match cast {
        SpellCastState::Instant => obj(vec![("kind", jstr("instant"))]),
        SpellCastState::CastTime { total_ticks, .. } => obj(vec![
            ("kind", jstr("cast_time")),
            ("total_ticks", jnum(f64::from(*total_ticks))),
        ]),
        SpellCastState::Channeled {
            interval_ticks,
            max_ticks,
            mana_per_pulse,
            ..
        } => obj(vec![
            ("kind", jstr("channeled")),
            ("interval_ticks", jnum(f64::from(*interval_ticks))),
            ("max_ticks", jnum(f64::from(*max_ticks))),
            ("mana_per_pulse", jnum(f64::from(*mana_per_pulse))),
        ]),
    }
}

fn spell_form_ast(form: &SpellForm) -> JsonValue {
    match form {
        SpellForm::SelfCast => obj(vec![("kind", jstr("self_cast"))]),
        SpellForm::Touch { range } => obj(vec![
            ("kind", jstr("touch")),
            ("range", jnum(f64::from(*range))),
        ]),
        SpellForm::Projectile {
            speed,
            gravity,
            max_ticks,
        } => obj(vec![
            ("kind", jstr("projectile")),
            ("speed", jnum(f64::from(*speed))),
            ("gravity", jnum(f64::from(*gravity))),
            ("max_ticks", jnum(f64::from(*max_ticks))),
        ]),
        SpellForm::RaycastBeam { range, pierce_cap } => obj(vec![
            ("kind", jstr("raycast_beam")),
            ("range", jnum(f64::from(*range))),
            ("pierce_cap", jnum(f64::from(*pierce_cap))),
        ]),
        SpellForm::AoeZone {
            radius,
            duration_ticks,
            pulse_interval,
        } => obj(vec![
            ("kind", jstr("aoe_zone")),
            ("radius", jnum(f64::from(*radius))),
            ("duration_ticks", jnum(f64::from(*duration_ticks))),
            ("pulse_interval", jnum(f64::from(*pulse_interval))),
        ]),
        SpellForm::Chain {
            max_hops,
            hop_radius,
        } => obj(vec![
            ("kind", jstr("chain")),
            ("max_hops", jnum(f64::from(*max_hops))),
            ("hop_radius", jnum(f64::from(*hop_radius))),
        ]),
        SpellForm::Orbit {
            count,
            radius,
            duration_ticks,
        } => obj(vec![
            ("kind", jstr("orbit")),
            ("count", jnum(f64::from(*count))),
            ("radius", jnum(f64::from(*radius))),
            ("duration_ticks", jnum(f64::from(*duration_ticks))),
        ]),
        SpellForm::Summon {
            entity_type,
            duration_ticks,
            max_active,
        } => obj(vec![
            ("kind", jstr("summon")),
            ("entity_type", jstr(entity_type)),
            ("duration_ticks", jnum(f64::from(*duration_ticks))),
            ("max_active", jnum(f64::from(*max_active))),
        ]),
    }
}

fn spell_effect_ast(effect: &SpellEffect) -> JsonValue {
    match effect {
        SpellEffect::Damage {
            school,
            base_amount,
        } => obj(vec![
            ("kind", jstr("damage")),
            ("school", jstr(school)),
            ("base_amount", jnum(f64::from(*base_amount))),
        ]),
        SpellEffect::Heal { base_amount } => obj(vec![
            ("kind", jstr("heal")),
            ("base_amount", jnum(f64::from(*base_amount))),
        ]),
        SpellEffect::Shield {
            amount,
            duration_ticks,
        } => obj(vec![
            ("kind", jstr("shield")),
            ("amount", jnum(f64::from(*amount))),
            ("duration_ticks", jnum(f64::from(*duration_ticks))),
        ]),
        SpellEffect::ApplyStatus {
            status_id,
            amplifier,
            duration_ticks,
        } => obj(vec![
            ("kind", jstr("apply_status")),
            ("status_id", jstr(status_id)),
            ("amplifier", jnum(f64::from(*amplifier))),
            ("duration_ticks", jnum(f64::from(*duration_ticks))),
        ]),
        SpellEffect::Knockback { strength } => obj(vec![
            ("kind", jstr("knockback")),
            ("strength", jnum(f64::from(*strength))),
        ]),
        SpellEffect::Blink { max_distance } => obj(vec![
            ("kind", jstr("blink")),
            ("max_distance", jnum(f64::from(*max_distance))),
        ]),
        SpellEffect::ConjureLight {
            level,
            duration_ticks,
        } => obj(vec![
            ("kind", jstr("conjure_light")),
            ("level", jnum(f64::from(*level))),
            ("duration_ticks", jnum(f64::from(*duration_ticks))),
        ]),
        SpellEffect::BreakBlock { max_tier } => obj(vec![
            ("kind", jstr("break_block")),
            ("max_tier", jnum(f64::from(*max_tier))),
        ]),
        SpellEffect::EmitVfx { vfx_id } => {
            obj(vec![("kind", jstr("emit_vfx")), ("vfx_id", jstr(vfx_id))])
        }
    }
}

fn status_ast(def: &StatusEffectDefinition) -> JsonValue {
    obj(vec![
        ("id", jstr(&def.id)),
        ("max_amplifier", jnum(f64::from(def.max_amplifier))),
        (
            "pulse_interval_ticks",
            jnum(f64::from(def.pulse_interval_ticks)),
        ),
        ("damage_per_pulse", jnum(f64::from(def.damage_per_pulse))),
        ("heal_per_pulse", jnum(f64::from(def.heal_per_pulse))),
        (
            "absorption_per_level",
            jnum(f64::from(def.absorption_per_level)),
        ),
        ("frozen", jbool(def.frozen)),
        ("rooted", jbool(def.rooted)),
        ("silenced", jbool(def.silenced)),
        ("disarmed", jbool(def.disarmed)),
        (
            "attribute_modifiers",
            JsonValue::Object(
                def.attribute_modifiers
                    .iter()
                    .map(|(key, value)| (key.clone(), jnum(f64::from(*value))))
                    .collect(),
            ),
        ),
        ("vfx_aura", opt_str(&def.vfx_aura)),
    ])
}

fn ritual_ast(recipe: &RitualRecipe) -> JsonValue {
    obj(vec![
        ("id", jstr(&recipe.id)),
        (
            "pattern",
            JsonValue::Array(
                recipe
                    .pattern
                    .iter()
                    .map(|(&(dx, dy, dz), block)| {
                        obj(vec![
                            ("dx", jnum(f64::from(dx))),
                            ("dy", jnum(f64::from(dy))),
                            ("dz", jnum(f64::from(dz))),
                            ("block", jstr(block)),
                        ])
                    })
                    .collect(),
            ),
        ),
        (
            "reagents",
            JsonValue::Object(
                recipe
                    .reagents
                    .iter()
                    .map(|(item, count)| (item.clone(), jnum(f64::from(*count))))
                    .collect(),
            ),
        ),
        ("duration_ticks", jnum(f64::from(recipe.duration_ticks))),
        ("mana_per_tick", jnum(f64::from(recipe.mana_per_tick))),
    ])
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

fn obj_of<'a>(v: &'a JsonValue, ctx: &str) -> Result<&'a BTreeMap<String, JsonValue>, BundleError> {
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
    v.as_bool().ok_or_else(|| verr(ctx, "must be a boolean"))
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

fn f32_of(v: &JsonValue, ctx: &str) -> Result<f32, BundleError> {
    let number = f64_of(v, ctx)?;
    let value = number as f32;
    if !value.is_finite() || (number != 0.0 && value == 0.0) {
        return Err(verr(ctx, "number outside the f32 range"));
    }
    Ok(value)
}

fn f32_field(obj: &BTreeMap<String, JsonValue>, key: &str, ctx: &str) -> Result<f32, BundleError> {
    f32_of(field(obj, key, ctx)?, ctx)
}

fn opt_f32_field(
    obj: &BTreeMap<String, JsonValue>,
    key: &str,
    ctx: &str,
    default: f32,
) -> Result<f32, BundleError> {
    match obj.get(key) {
        Some(value) => f32_of(value, ctx),
        None => Ok(default),
    }
}

fn f64_field(obj: &BTreeMap<String, JsonValue>, key: &str, ctx: &str) -> Result<f64, BundleError> {
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

fn u32_field(obj: &BTreeMap<String, JsonValue>, key: &str, ctx: &str) -> Result<u32, BundleError> {
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

fn u8_field(obj: &BTreeMap<String, JsonValue>, key: &str, ctx: &str) -> Result<u8, BundleError> {
    let n = int_of(field(obj, key, ctx)?, ctx)?;
    if !(0.0..=f64::from(u8::MAX)).contains(&n) {
        return Err(verr(ctx, "must be a u8 integer"));
    }
    Ok(n as u8)
}

fn i32_field(obj: &BTreeMap<String, JsonValue>, key: &str, ctx: &str) -> Result<i32, BundleError> {
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
    let items = v.as_array().ok_or_else(|| verr(ctx, "must be an array"))?;
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

fn optional_section<T>(
    top: &BTreeMap<String, JsonValue>,
    key: &str,
    parse: fn(&JsonValue) -> Result<T, BundleError>,
) -> Result<Vec<T>, BundleError> {
    match top.get(key) {
        Some(items) => {
            let array = items
                .as_array()
                .ok_or_else(|| verr("bundle", format!("'{key}' must be an array")))?;
            bounded(array.len(), MAX_SECTION_ENTRIES, key)?;
            array.iter().map(parse).collect()
        }
        None => Ok(Vec::new()),
    }
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
    pub spells_installed: usize,
    pub custom_statuses_installed: usize,
    pub rituals_installed: usize,
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
    spells: BTreeMap<String, SpellDefinition>,
    custom_statuses: CustomStatusRegistry,
    rituals: BTreeMap<String, RitualRecipe>,
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
            spells: BTreeMap::new(),
            custom_statuses: CustomStatusRegistry::default(),
            rituals: BTreeMap::new(),
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

        // Validate all magic insertions against the complete installed state
        // before modifying any runtime registry, including legacy sections.
        for spell in &bundle.spells {
            if self.spells.contains_key(&spell.id) {
                return Err(verr("spells", format!("already registered '{}'", spell.id)));
            }
        }
        for ritual in &bundle.rituals {
            if self.rituals.contains_key(&ritual.id) {
                return Err(verr(
                    "rituals",
                    format!("already registered '{}'", ritual.id),
                ));
            }
        }
        let mut new_statuses = self.custom_statuses.clone();
        if new_statuses.len() + bundle.custom_statuses.len() > MAX_STATUS_DEFS {
            return Err(verr("custom_statuses", "registry capacity exceeded"));
        }
        for status in &bundle.custom_statuses {
            new_statuses
                .register(status.clone())
                .map_err(|e| verr("custom_statuses", format!("{e:?}")))?;
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
            self.creatures
                .insert(spec.entity_type.clone(), spec.clone());
        }
        for spell in &bundle.spells {
            self.spells.insert(spell.id.clone(), spell.clone());
        }
        self.custom_statuses = new_statuses;
        for ritual in &bundle.rituals {
            self.rituals.insert(ritual.id.clone(), ritual.clone());
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
            spells_installed: bundle.spells.len(),
            custom_statuses_installed: bundle.custom_statuses.len(),
            rituals_installed: bundle.rituals.len(),
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

    /// Registered spell by resource ID.
    pub fn spell(&self, id: &str) -> Option<&SpellDefinition> {
        self.spells.get(id)
    }

    pub fn spell_count(&self) -> usize {
        self.spells.len()
    }

    /// Registered custom status by resource ID.
    pub fn custom_status(&self, id: &str) -> Option<&StatusEffectDefinition> {
        self.custom_statuses.get(id)
    }

    pub fn custom_status_count(&self) -> usize {
        self.custom_statuses.len()
    }

    /// Registered ritual by resource ID.
    pub fn ritual(&self, id: &str) -> Option<&RitualRecipe> {
        self.rituals.get(id)
    }

    pub fn ritual_count(&self) -> usize {
        self.rituals.len()
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
/// expressible as pure `megu-mod-bundle/1` JSON data.
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
            spells: Vec::new(),
            custom_statuses: Vec::new(),
            rituals: Vec::new(),
        })
        .collect()
}

#[cfg(test)]
mod magic_bundle_tests {
    use super::*;

    fn document(extra: &str) -> String {
        format!(
            r#"{{"schema":"megu-mod-bundle/1","mod_id":"demo","mc_version_range":"[1.21,)","portals":[],"progression_barriers":[],"biome_regions":[],"creatures":[],"custom_channels":[]{extra}}}"#
        )
    }

    const SPELL: &str = r#"{"id":"demo:ember","mana_cost":2.5,"base_cooldown_ticks":10,"cast":{"kind":"instant"},"steps":[{"form":{"kind":"aoe_zone","radius":4,"duration_ticks":40,"pulse_interval":10},"effects":[{"kind":"damage","school":"demo:fire","base_amount":3},{"kind":"apply_status","status_id":"demo:burn","amplifier":1,"duration_ticks":60}]}]}"#;
    const STATUS: &str = r#"{"id":"demo:burn","max_amplifier":3,"pulse_interval_ticks":10,"damage_per_pulse":1.5,"heal_per_pulse":0.25,"absorption_per_level":2,"frozen":true,"rooted":true,"silenced":true,"disarmed":true,"attribute_modifiers":{"demo:power":-0.5},"vfx_aura":"demo:fire_aura"}"#;
    const RITUAL: &str = r#"{"id":"demo:altar","pattern":[{"dx":0,"dy":0,"dz":0,"block":"demo:core"},{"dx":1,"dy":0,"dz":0,"block":"minecraft:gold_block"}],"reagents":{"minecraft:diamond":2},"duration_ticks":100,"mana_per_tick":0.75}"#;

    fn with_magic() -> DeclarativeModBundle {
        DeclarativeModBundle::from_json(&document(&format!(
            r#", "spells":[{SPELL}], "custom_statuses":[{STATUS}], "rituals":[{RITUAL}]"#
        )))
        .expect("valid declarative magic bundle")
    }

    #[test]
    fn old_bundles_remain_accepted_without_magic_keys() {
        let old = DeclarativeModBundle::from_json(&document("")).unwrap();
        assert!(old.spells.is_empty() && old.custom_statuses.is_empty() && old.rituals.is_empty());
        assert!(!old.to_json().contains("\"spells\""));
        assert!(!old.to_json().contains("\"rituals\""));
        assert_eq!(
            old,
            DeclarativeModBundle::from_json(&old.to_json()).unwrap()
        );
        let explicit_empty = document(r#", "spells":[], "custom_statuses":[], "rituals":[]"#);
        assert_eq!(
            old,
            DeclarativeModBundle::from_json(&explicit_empty).unwrap()
        );
        let mut runtime = UnifiedModRuntime::default();
        let receipt = runtime
            .register_bundle(&old, McVersion::new(1, 21, 1))
            .unwrap();
        assert_eq!(
            (
                receipt.spells_installed,
                receipt.custom_statuses_installed,
                receipt.rituals_installed
            ),
            (0, 0, 0)
        );
    }

    #[test]
    fn parses_roundtrips_and_registers_all_magic_sections() {
        let bundle = with_magic();
        assert_eq!(
            bundle,
            DeclarativeModBundle::from_json(&bundle.to_json()).unwrap()
        );
        assert_eq!(
            bundle.to_json(),
            DeclarativeModBundle::from_json(&bundle.to_json())
                .unwrap()
                .to_json()
        );
        assert_eq!(bundle.spells[0].steps[0].form.max_targets(), 128);
        assert!(bundle.custom_statuses[0].silenced);
        assert_eq!(bundle.rituals[0].pattern.len(), 2);
        let mut runtime = UnifiedModRuntime::default();
        let summary = runtime
            .register_bundle(&bundle, McVersion::new(1, 21, 1))
            .unwrap();
        assert_eq!(
            (
                summary.spells_installed,
                summary.custom_statuses_installed,
                summary.rituals_installed
            ),
            (1, 1, 1)
        );
        assert_eq!(
            (
                runtime.spell_count(),
                runtime.custom_status_count(),
                runtime.ritual_count()
            ),
            (1, 1, 1)
        );
        assert_eq!(runtime.spell("demo:ember"), Some(&bundle.spells[0]));
        assert_eq!(
            runtime.custom_status("demo:burn"),
            Some(&bundle.custom_statuses[0])
        );
        assert_eq!(runtime.ritual("demo:altar"), Some(&bundle.rituals[0]));
        assert!(runtime.spell("demo:missing").is_none());
    }

    #[test]
    fn supports_every_spell_cast_form_and_effect_variant() {
        let casts = [
            r#"{"kind":"instant"}"#,
            r#"{"kind":"cast_time","total_ticks":10}"#,
            r#"{"kind":"channeled","interval_ticks":2,"max_ticks":10,"mana_per_pulse":0.5}"#,
        ];
        let forms = [
            r#"{"kind":"self_cast"}"#,
            r#"{"kind":"touch","range":3}"#,
            r#"{"kind":"projectile","speed":2,"gravity":0.05,"max_ticks":10}"#,
            r#"{"kind":"raycast_beam","range":10,"pierce_cap":2}"#,
            r#"{"kind":"aoe_zone","radius":3,"duration_ticks":10,"pulse_interval":2}"#,
            r#"{"kind":"chain","max_hops":3,"hop_radius":4}"#,
            r#"{"kind":"orbit","count":3,"radius":2,"duration_ticks":12}"#,
            r#"{"kind":"summon","entity_type":"minecraft:pig","duration_ticks":12,"max_active":2}"#,
        ];
        let effects = [
            r#"{"kind":"damage","school":"demo:arcane","base_amount":2}"#,
            r#"{"kind":"heal","base_amount":3}"#,
            r#"{"kind":"shield","amount":2,"duration_ticks":20}"#,
            r#"{"kind":"apply_status","status_id":"demo:burn","amplifier":2,"duration_ticks":20}"#,
            r#"{"kind":"knockback","strength":3}"#,
            r#"{"kind":"blink","max_distance":4}"#,
            r#"{"kind":"conjure_light","level":8,"duration_ticks":10}"#,
            r#"{"kind":"break_block","max_tier":2}"#,
            r#"{"kind":"emit_vfx","vfx_id":"demo:flash"}"#,
        ];
        for cast in casts {
            for (idx, form) in forms.iter().enumerate() {
                let effect = effects[idx % effects.len()];
                let input = document(&format!(
                    r#", "spells":[{{"id":"demo:test","mana_cost":1,"base_cooldown_ticks":1,"cast":{cast},"steps":[{{"form":{form},"effects":[{effect}]}}]}}]"#
                ));
                let bundle = DeclarativeModBundle::from_json(&input).unwrap();
                assert_eq!(
                    bundle,
                    DeclarativeModBundle::from_json(&bundle.to_json()).unwrap()
                );
            }
        }
        // Exercise the remaining effect independent of form choice.
        let input = document(&format!(
            r#", "spells":[{{"id":"demo:test","mana_cost":0,"base_cooldown_ticks":0,"cast":{{"kind":"instant"}},"steps":[{{"form":{{"kind":"self_cast"}},"effects":[{}]}}]}}]"#,
            effects[8]
        ));
        assert!(DeclarativeModBundle::from_json(&input).is_ok());
    }

    #[test]
    fn rejects_malformed_and_over_cap_magic_fail_closed() {
        let baseline = document(&format!(r#", "spells":[{SPELL}]"#));
        for (before, after) in [
            ("\"radius\":4", "\"radius\":33"),
            ("\"radius\":4", "\"radius\":1e100"),
            ("\"radius\":4", "\"radius\":null"),
            ("\"kind\":\"aoe_zone\"", "\"kind\":\"unknown_form\""),
            ("\"kind\":\"damage\"", "\"kind\":\"unknown_effect\""),
            ("\"kind\":\"instant\"", "\"kind\":\"unknown_cast\""),
            ("\"mana_cost\":2.5", "\"mana_cost\":-1"),
            ("\"mana_cost\":2.5", "\"mana_cost\":1e100"),
            ("\"mana_cost\":2.5", "\"mana_cost\":1e-100"),
            ("\"mana_cost\":2.5", "\"mana_cost\":\"NaN\""),
            ("\"base_cooldown_ticks\":10", "\"base_cooldown_ticks\":1.5"),
            ("\"base_amount\":3", "\"base_amount\":-1"),
            ("\"base_amount\":3", "\"base_amount\":2,\"extra\":1"),
        ] {
            let invalid = baseline.replacen(before, after, 1);
            assert!(
                DeclarativeModBundle::from_json(&invalid).is_err(),
                "accepted {after}"
            );
        }
        for extra in [
            r#", "spells":{}"#,
            r#", "rituals":null"#,
            r#", "custom_statuses":[{"id":"demo:burn","damage_per_pulse":1}]"#,
            r#", "custom_statuses":[{"id":"demo:burn","vfx_aura":"demo:fx","unexpected":1}]"#,
            r#", "custom_statuses":[{"id":"demo:burn","attribute_modifiers":{"INVALID:Name":1}}]"#,
            r#", "rituals":[{"id":"demo:altar","pattern":[{"dx":17,"dy":0,"dz":0,"block":"minecraft:stone"}],"reagents":{},"duration_ticks":1,"mana_per_tick":1}]"#,
            r#", "rituals":[{"id":"demo:altar","pattern":[{"dx":-2147483648,"dy":0,"dz":0,"block":"minecraft:stone"}],"reagents":{},"duration_ticks":1,"mana_per_tick":1}]"#,
            r#", "rituals":[{"id":"demo:altar","pattern":[{"dx":0,"dy":0,"dz":0,"block":"minecraft:stone"},{"dx":0,"dy":0,"dz":0,"block":"minecraft:dirt"}],"reagents":{},"duration_ticks":1,"mana_per_tick":1}]"#,
            r#", "rituals":[{"id":"demo:altar","pattern":[],"reagents":{"minecraft:diamond":0},"duration_ticks":1,"mana_per_tick":1}]"#,
            r#", "unknown_section":[]"#,
        ] {
            assert!(
                DeclarativeModBundle::from_json(&document(extra)).is_err(),
                "accepted {extra}"
            );
        }
        assert!(DeclarativeModBundle::from_json(&document(&format!(
            r#", "spells":[{SPELL},{SPELL}]"#
        )))
        .is_err());
        assert!(DeclarativeModBundle::from_json(&document(&format!(
            r#", "custom_statuses":[{STATUS},{STATUS}]"#
        )))
        .is_err());
        assert!(DeclarativeModBundle::from_json(&document(&format!(
            r#", "rituals":[{RITUAL},{RITUAL}]"#
        )))
        .is_err());
    }

    #[test]
    fn failed_registration_never_mutates_runtime() {
        let mut runtime = UnifiedModRuntime::default();
        let original = with_magic();
        runtime
            .register_bundle(&original, McVersion::new(1, 21, 1))
            .unwrap();
        for invalid in [
            original.clone(),
            {
                let mut copy = original.clone();
                copy.spells.clear();
                copy.rituals.clear();
                copy
            },
            {
                let mut copy = original.clone();
                copy.spells.clear();
                copy.custom_statuses.clear();
                copy
            },
        ] {
            assert!(runtime
                .register_bundle(&invalid, McVersion::new(1, 21, 1))
                .is_err());
            assert_eq!(
                (
                    runtime.spell_count(),
                    runtime.custom_status_count(),
                    runtime.ritual_count()
                ),
                (1, 1, 1)
            );
            assert_eq!(runtime.installed_mod_ids().len(), 1);
        }
        let mut tampered = original.clone();
        tampered.spells[0].cast = SpellCastState::CastTime {
            total_ticks: 4,
            elapsed_ticks: 1,
        };
        assert!(tampered.validate().is_err());
        tampered = original.clone();
        tampered.spells[0].mana_cost = f32::NAN;
        assert!(tampered.validate().is_err());
        tampered = original.clone();
        tampered.custom_statuses[0]
            .attribute_modifiers
            .insert("demo:speed".into(), f32::INFINITY);
        assert!(tampered.validate().is_err());
        tampered = original.clone();
        tampered.custom_statuses[0].damage_per_pulse = f32::NAN;
        assert!(tampered.validate().is_err());
        tampered = original;
        tampered.rituals[0]
            .pattern
            .insert((i32::MIN, 0, 0), "minecraft:stone".into());
        assert!(tampered.validate().is_err());
    }
}
