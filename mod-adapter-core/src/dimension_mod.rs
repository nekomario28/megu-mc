// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later

//! Universal dimension-adding mod support: zero-JVM `dimension_type` /
//! `dimension` / `worldgen` / ore-tag discovery, a declarative mod portal
//! engine covering every major dimension-mod travel pattern, per-dimension
//! environment profiles, and multi-word light masks for tall custom
//! dimensions.
//!
//! Pure-`std` and free of external crates: all JSON goes through
//! [`crate::data_loader::parse_json`], and the portal detectors are pure
//! functions over a caller-supplied block accessor so the same code validates
//! datapack fixtures, live `PalettedSection` reads, and test worlds alike.

use std::collections::{BTreeSet, VecDeque};

use crate::data_loader::{parse_json, DataLoadError, JsonValue};

/// Chunk-generator `generator.type` values the native engine can execute with
/// zero JVM (mirrors `CustomGeneratorKind` in `src/worldgen.rs`).
const NATIVE_GENERATOR_NOISE: &str = "minecraft:noise";
const NATIVE_GENERATOR_FLAT: &str = "minecraft:flat";
const NATIVE_GENERATOR_VOID: &str = "minecraft:debug";

/// Hard cap on a horizontal fluid-pool flood fill so a malformed modded ocean
/// can never produce an unbounded portal scan.
const MAX_POOL_CELLS: usize = 4096;

/// Default anti-cheat teleport grace granted after a mod-portal hop: one
/// second of server ticks, matching the teleport-acceptance window
/// `ac_expect_teleport` gives a vanilla `/execute in` transfer.
pub const DEFAULT_TELEPORT_GRACE_TICKS: u32 = 20;

// ---------------------------------------------------------------------------
// Dimension mod data discovery
// ---------------------------------------------------------------------------

/// A parsed `data/<ns>/dimension_type/*.json` entry.
#[derive(Clone, Debug, PartialEq)]
pub struct DimTypeDescriptor {
    /// Canonical `"namespace:path"` id derived from the entry path.
    pub id: String,
    pub min_y: i32,
    pub height: i32,
    pub coordinate_scale: f64,
    pub has_skylight: bool,
    pub has_ceiling: bool,
    pub ultrawarm: bool,
    pub natural: bool,
    pub bed_works: bool,
    pub respawn_anchor_works: bool,
    pub ambient_light: f64,
    pub effects: String,
}

impl DimTypeDescriptor {
    /// A descriptor with vanilla-overworld defaults for every optional field;
    /// built-in reference profiles start here and override what differs.
    pub fn defaults(id: impl Into<String>, min_y: i32, height: i32) -> Self {
        Self {
            id: id.into(),
            min_y,
            height,
            coordinate_scale: 1.0,
            has_skylight: true,
            has_ceiling: false,
            ultrawarm: false,
            natural: true,
            bed_works: true,
            respawn_anchor_works: false,
            ambient_light: 0.0,
            effects: "minecraft:overworld".to_string(),
        }
    }

    /// Number of 16-block sections this dimension's light engine tracks,
    /// including the two virtual boundary sections (`min_y - 16` .. `height`).
    /// Overworld => 26, Nether/End => 18, a 2048-block dimension => 130.
    pub fn light_sections(&self) -> usize {
        let sections = self.height / 16;
        (sections + 2).max(0) as usize
    }
}

/// The `generator.type` of a `dimension/` file, reduced to what the runtime
/// can classify.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GeneratorKind {
    /// `minecraft:noise` — runs natively on the Track 2 dynamic-dimension path.
    Noise,
    /// `minecraft:flat` — superflat generator, native.
    Flat,
    /// `minecraft:debug` or a missing generator declaration — rendered as an
    /// empty (void) world, native.
    Void,
    /// Any other generator type: a modded Java `ChunkGenerator` that cannot
    /// run natively and routes the mod to Tier 2/3 fallback.
    CustomChunkGenerator(String),
}

/// A parsed `data/<ns>/dimension/*.json` entry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DimEntryDescriptor {
    /// Canonical `"namespace:path"` id derived from the entry path.
    pub id: String,
    /// The `type` field naming the `dimension_type` this dimension uses.
    pub type_id: String,
    pub generator_kind: GeneratorKind,
}

/// Counts and ids of `data/<ns>/worldgen/*` declarations a dimension mod
/// ships, grouped by registry category.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WorldgenDataSummary {
    pub noise_settings: Vec<String>,
    pub biomes: Vec<String>,
    pub configured_features: Vec<String>,
    pub placed_features: Vec<String>,
    pub structures: Vec<String>,
}

/// Everything [`discover_dimension_mod_entries`] extracted from one mod JAR or
/// datapack directory.
#[derive(Clone, Debug, Default)]
pub struct DimensionModData {
    pub dimension_types: Vec<DimTypeDescriptor>,
    pub dimensions: Vec<DimEntryDescriptor>,
    pub worldgen: WorldgenDataSummary,
    /// Block ids declared under `data/*/tags/{block,blocks}/ores*.json`,
    /// ready to forward into the anti-xray hidden set.
    pub ore_blocks: Vec<String>,
    /// Block ids declared under `data/*/tags/{block,blocks}/chests*.json`,
    /// ready to forward into the anti-xray hidden set.
    pub chest_blocks: Vec<String>,
    /// `data/*` paths that matched a dimension-mod slot but failed to parse;
    /// kept for diagnostics instead of aborting the whole scan.
    pub skipped_malformed: Vec<String>,
}

/// Whether a discovered dimension mod can run with zero JVM.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DimensionModClassification {
    /// Every dimension uses a vanilla noise/flat/void generator: the mod is
    /// pure data-driven and classifies as Tier 1 `UniversalNative`.
    PureDataDriven,
    /// At least one dimension declares a modded Java `ChunkGenerator`; carries
    /// the offending generator type ids for the `CUSTOM_CHUNK_GENERATOR`
    /// diagnostic reason.
    CustomChunkGenerator(Vec<String>),
}

impl DimensionModData {
    /// True when the mod ships at least one `dimension_type` plus one
    /// `dimension` entry — the `tools/mod_launcher.py` dimension-mod rule.
    pub fn is_dimension_mod(&self) -> bool {
        !self.dimension_types.is_empty() && !self.dimensions.is_empty()
    }

    /// Shorthand for `classification() == PureDataDriven`.
    pub fn is_pure_data_driven(&self) -> bool {
        self.classification() == DimensionModClassification::PureDataDriven
    }

    /// Pure data-driven (Tier 1 zero-JVM) vs custom Java `ChunkGenerator`
    /// (Tier 2/3 fallback).
    pub fn classification(&self) -> DimensionModClassification {
        let customs: Vec<String> = self
            .dimensions
            .iter()
            .filter_map(|d| match &d.generator_kind {
                GeneratorKind::CustomChunkGenerator(kind) => Some(kind.clone()),
                _ => None,
            })
            .collect();
        if customs.is_empty() {
            DimensionModClassification::PureDataDriven
        } else {
            DimensionModClassification::CustomChunkGenerator(customs)
        }
    }

    /// Every custom ore/chest block id merged, deduplicated, and sorted — the
    /// direct input for `megu`'s anti-xray hidden set.
    pub fn anti_xray_hidden_blocks(&self) -> Vec<String> {
        self.ore_blocks
            .iter()
            .chain(self.chest_blocks.iter())
            .cloned()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect()
    }

    /// Merge the discovered ore/chest tag members into a
    /// [`crate::unified_tags::UnifiedTagIndex`] under the conventional
    /// `c:ores` / `c:chests` block tags — reachable through the legacy
    /// `forge:` aliases too, so mods that only shipped `forge:` files and
    /// mods that query `c:` interoperate with zero mod-specific wiring.
    pub fn populate_unified_tags(&self, index: &mut crate::unified_tags::UnifiedTagIndex) {
        index.merge_values(
            crate::unified_tags::TagDomain::Block,
            "c:ores",
            self.ore_blocks.iter(),
        );
        index.merge_values(
            crate::unified_tags::TagDomain::Block,
            "c:chests",
            self.chest_blocks.iter(),
        );
    }
}

/// Scan `(path, bytes)` entries from a mod JAR or datapack directory and
/// extract every dimension-mod datum: `dimension_type` / `dimension`
/// descriptors, `worldgen` counts and ids, and ore/chest tag members.
///
/// Malformed JSON entries are recorded in
/// [`DimensionModData::skipped_malformed`] rather than aborting the scan; a
/// structurally unreadable JSON blob (depth/size limit) still returns
/// [`DataLoadError::Json`].
pub fn discover_dimension_mod_entries<I, S, B>(
    entries: I,
) -> Result<DimensionModData, DataLoadError>
where
    I: IntoIterator<Item = (S, B)>,
    S: AsRef<str>,
    B: AsRef<[u8]>,
{
    let mut out = DimensionModData::default();
    for (path, bytes) in entries {
        let path = path.as_ref();
        let bytes = bytes.as_ref();
        let parts: Vec<&str> = path.split('/').collect();
        if parts.len() < 4 || parts[0] != "data" || !path.ends_with(".json") {
            continue;
        }
        let ns = parts[1];
        let category = parts[2];
        let stem = parts[3..].join("/");
        let stem = stem.strip_suffix(".json").unwrap_or(&stem);
        let id = format!("{ns}:{stem}");

        match category {
            "dimension_type" => match parse_dim_type(&id, bytes) {
                Ok(desc) => out.dimension_types.push(desc),
                Err(_) => out.skipped_malformed.push(path.to_string()),
            },
            "dimension" => match parse_dim_entry(&id, bytes) {
                Ok(desc) => out.dimensions.push(desc),
                Err(_) => out.skipped_malformed.push(path.to_string()),
            },
            "worldgen" if parts.len() >= 5 => {
                let wg_stem = parts[4..].join("/");
                let wg_stem = wg_stem.strip_suffix(".json").unwrap_or(&wg_stem);
                collect_worldgen(&mut out.worldgen, parts[3], format!("{ns}:{wg_stem}"));
            }
            "tags" if parts.len() >= 5 => {
                collect_hidden_tag(&mut out, parts[3], stem, bytes)?;
            }
            _ => {}
        }
    }
    out.dimension_types.sort_by(|a, b| a.id.cmp(&b.id));
    out.dimensions.sort_by(|a, b| a.id.cmp(&b.id));
    for list in [
        &mut out.worldgen.noise_settings,
        &mut out.worldgen.biomes,
        &mut out.worldgen.configured_features,
        &mut out.worldgen.placed_features,
        &mut out.worldgen.structures,
        &mut out.ore_blocks,
        &mut out.chest_blocks,
    ] {
        list.sort();
        list.dedup();
    }
    Ok(out)
}

fn parse_dim_type(id: &str, bytes: &[u8]) -> Result<DimTypeDescriptor, DataLoadError> {
    let root = parse_json(bytes)?;
    let obj = root
        .as_object()
        .ok_or_else(|| DataLoadError::Json("dimension_type root must be object".into()))?;
    let required_i32 = |key: &str| -> Result<i32, DataLoadError> {
        obj.get(key)
            .and_then(JsonValue::as_f64)
            .map(|v| v as i32)
            .ok_or_else(|| {
                DataLoadError::Json(format!("dimension_type {id} missing '{key}'"))
            })
    };
    let opt_bool = |key: &str, default: bool| {
        obj.get(key).and_then(JsonValue::as_bool).unwrap_or(default)
    };
    let mut desc = DimTypeDescriptor::defaults(id, required_i32("min_y")?, required_i32("height")?);
    desc.coordinate_scale = obj
        .get("coordinate_scale")
        .and_then(JsonValue::as_f64)
        .filter(|v| v.is_finite() && *v > 0.0)
        .unwrap_or(1.0);
    desc.has_skylight = opt_bool("has_skylight", true);
    desc.has_ceiling = opt_bool("has_ceiling", false);
    desc.ultrawarm = opt_bool("ultrawarm", false);
    desc.natural = opt_bool("natural", true);
    desc.bed_works = opt_bool("bed_works", true);
    desc.respawn_anchor_works = opt_bool("respawn_anchor_works", false);
    desc.ambient_light = obj
        .get("ambient_light")
        .and_then(JsonValue::as_f64)
        .unwrap_or(0.0);
    if let Some(effects) = obj.get("effects").and_then(JsonValue::as_str) {
        desc.effects = effects.to_string();
    }
    Ok(desc)
}

fn parse_dim_entry(id: &str, bytes: &[u8]) -> Result<DimEntryDescriptor, DataLoadError> {
    let root = parse_json(bytes)?;
    let obj = root
        .as_object()
        .ok_or_else(|| DataLoadError::Json("dimension root must be object".into()))?;
    let type_id = obj
        .get("type")
        .and_then(JsonValue::as_str)
        .unwrap_or("minecraft:overworld")
        .to_string();
    let generator_kind = match obj
        .get("generator")
        .and_then(JsonValue::as_object)
        .and_then(|g| g.get("type"))
        .and_then(JsonValue::as_str)
    {
        Some(NATIVE_GENERATOR_NOISE) => GeneratorKind::Noise,
        Some(NATIVE_GENERATOR_FLAT) => GeneratorKind::Flat,
        // `minecraft:debug`, a missing `generator`, or a generator without a
        // type are all safely renderable as an empty world — they prove no
        // Java `ChunkGenerator` is required.
        Some(NATIVE_GENERATOR_VOID) | None => GeneratorKind::Void,
        Some(other) => GeneratorKind::CustomChunkGenerator(other.to_string()),
    };
    Ok(DimEntryDescriptor {
        id: id.to_string(),
        type_id,
        generator_kind,
    })
}

fn collect_worldgen(worldgen: &mut WorldgenDataSummary, sub: &str, id: String) {
    let list = match sub {
        "noise_settings" => &mut worldgen.noise_settings,
        "biome" => &mut worldgen.biomes,
        "configured_feature" => &mut worldgen.configured_features,
        "placed_feature" => &mut worldgen.placed_features,
        "structure" => &mut worldgen.structures,
        _ => return,
    };
    list.push(id);
}

fn collect_hidden_tag(
    out: &mut DimensionModData,
    tag_kind: &str,
    stem: &str,
    bytes: &[u8],
) -> Result<(), DataLoadError> {
    if tag_kind != "block" && tag_kind != "blocks" {
        return Ok(());
    }
    let leaf = stem.rsplit('/').next().unwrap_or(stem);
    let target = if leaf.starts_with("ores") {
        &mut out.ore_blocks
    } else if leaf.starts_with("chests") {
        &mut out.chest_blocks
    } else {
        return Ok(());
    };
    let root = parse_json(bytes)?;
    let Some(values) = root
        .as_object()
        .and_then(|o| o.get("values"))
        .and_then(JsonValue::as_array)
    else {
        return Ok(());
    };
    for value in values {
        let member = value.as_str().or_else(|| {
            value
                .as_object()
                .and_then(|o| o.get("id"))
                .and_then(JsonValue::as_str)
        });
        // Nested `#tag` references are expanded by the tag table, not here.
        if let Some(member) = member.filter(|m| !m.starts_with('#')) {
            target.push(member.to_string());
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Declarative mod portal engine
// ---------------------------------------------------------------------------

/// How a dimension mod's portal is triggered, declared once per portal rule.
#[derive(Clone, Debug, PartialEq)]
pub enum PortalTrigger {
    /// A vertical rectangular frame filled with air that an item ignites:
    /// The Aether (glowstone + water bucket), Deeper & Darker (reinforced
    /// deepslate), The Undergarden (stone bricks + catalyst), and vanilla's
    /// own nether portal shape.
    VerticalFrame {
        frame_block: String,
        /// Interior (open) rectangle bounds, not counting the frame ring.
        min_width: u32,
        min_height: u32,
        max_width: u32,
        max_height: u32,
        igniter_item: String,
        portal_block: String,
    },
    /// A flat pool of fluid bordered by decorative blocks that a dropped item
    /// activates: Twilight Forest's 2×2 water pool + flowers + diamond.
    HorizontalPool {
        /// Border block ids or `#namespace:tag` references the caller expands
        /// through the loaded tag table before detection.
        border_tags_or_blocks: Vec<String>,
        fluid_block: String,
        min_size: u32,
        drop_item: String,
        portal_block: String,
    },
    /// Ascending past a Y threshold while riding a vehicle / holding an item:
    /// Ad Astra and Create: Liftoff rocket ascent (`y >= 600`).
    AltitudeThreshold {
        min_y_trigger: i32,
        required_vehicle_or_item: Option<String>,
    },
    /// Using an item (or bare hand) on a specific block: teleportation pads
    /// and dimension-key blocks.
    DirectBlockInteract {
        block_id: String,
        required_item: Option<String>,
    },
}

/// One declarative portal rule: a trigger plus where it leads.
#[derive(Clone, Debug, PartialEq)]
pub struct PortalRule {
    pub name: String,
    pub trigger: PortalTrigger,
    pub target_dim: String,
    /// Ticks of anti-cheat movement grace granted to the teleported player so
    /// a mod portal can never false-flag (`ac_expect_teleport` equivalent).
    pub teleport_grace_ticks: u32,
}

impl PortalRule {
    pub fn new(name: impl Into<String>, trigger: PortalTrigger, target_dim: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            trigger,
            target_dim: target_dim.into(),
            teleport_grace_ticks: DEFAULT_TELEPORT_GRACE_TICKS,
        }
    }
}

/// A successfully validated portal activation: which rule fired and the
/// anchor block of the detected shape.
#[derive(Clone, Debug, PartialEq)]
pub struct PortalTransition {
    pub rule_name: String,
    pub target_dim: String,
    /// For a `VerticalFrame`, the minimum-corner interior (open) cell; for a
    /// `HorizontalPool`, the lexicographically smallest fluid cell.
    pub anchor: [i32; 3],
    /// Interior open cells (frame) or fluid cells (pool) in the detected
    /// shape — recorded for diagnostics and portal-block stamping.
    pub cell_count: u32,
}

/// Convert a validated transition into the teleport the server should
/// execute: the destination coordinates (coordinate-scale corrected) plus the
/// anti-cheat grace the move needs.
#[derive(Clone, Debug, PartialEq)]
pub struct TeleportIntent {
    pub target_dim: String,
    /// Destination position in the target dimension: `x/z` scaled by
    /// `source_scale / target_scale`, `y` clamped inside the target's safe
    /// band `[min_y + 4, min_y + height - 8]`.
    pub target_pos: [f64; 3],
    /// Grace the server applies via `ac_expect_teleport(eid)` +
    /// `ac_grant_grace` so the portal hop cannot false-flag movement checks.
    pub expect_teleport_grace_ticks: u32,
}

/// Ordered set of declarative portal rules evaluated per activation attempt.
#[derive(Clone, Debug, Default)]
pub struct PortalRegistry {
    rules: Vec<PortalRule>,
}

impl PortalRegistry {
    pub fn new() -> Self {
        Self { rules: Vec::new() }
    }

    /// A registry preloaded with every built-in reference profile's rules.
    pub fn with_builtin_rules() -> Self {
        let mut registry = Self::new();
        for profile in builtin_dimension_mod_profiles() {
            for rule in profile.portal_rules {
                registry.register(rule);
            }
        }
        registry
    }

    pub fn register(&mut self, rule: PortalRule) {
        self.rules.push(rule);
    }

    pub fn rules(&self) -> &[PortalRule] {
        &self.rules
    }

    pub fn find(&self, name: &str) -> Option<&PortalRule> {
        self.rules.iter().find(|r| r.name == name)
    }

    pub fn rules_to<'a>(&'a self, target_dim: &'a str) -> impl Iterator<Item = &'a PortalRule> + 'a {
        self.rules.iter().filter(move |r| r.target_dim == target_dim)
    }
}

/// A spec for the safe arrival platform the server builds (or verifies) at a
/// computed destination when no return portal exists: a solid floor under the
/// arrival rectangle plus, for frame portals, the matching portal interior.
#[derive(Clone, Debug, PartialEq)]
pub struct ArrivalPlatform {
    pub floor_block: String,
    /// Floor footprint in x/z blocks, centred under the destination.
    pub floor_width: u32,
    pub floor_depth: u32,
    /// Portal interior to stamp above the floor, if the trigger kind creates
    /// a return portal (`None` for pool/altitude/interact triggers, whose
    /// home portal stays behind).
    pub portal_block: Option<String>,
    pub portal_width: u32,
    pub portal_height: u32,
}

/// Deterministic arrival-platform spec for a rule: frame portals rebuild
/// their own kind in the destination dimension, everything else lands on a
/// minimal 3×3 stone pad.
pub fn arrival_platform(rule: &PortalRule) -> ArrivalPlatform {
    match &rule.trigger {
        PortalTrigger::VerticalFrame {
            frame_block,
            min_width,
            min_height,
            portal_block,
            ..
        } => ArrivalPlatform {
            floor_block: frame_block.clone(),
            floor_width: min_width + 2,
            floor_depth: 3,
            portal_block: Some(portal_block.clone()),
            portal_width: *min_width,
            portal_height: *min_height,
        },
        _ => ArrivalPlatform {
            floor_block: "minecraft:stone".to_string(),
            floor_width: 3,
            floor_depth: 3,
            portal_block: None,
            portal_width: 0,
            portal_height: 0,
        },
    }
}

fn is_open(id: Option<&str>, portal_block: &str) -> bool {
    matches!(
        id,
        Some("minecraft:air") | Some("minecraft:cave_air") | Some("minecraft:void_air")
    ) || id == Some(portal_block)
}

/// Validate a `VerticalFrame` portal around an ignition click at `ignite`
/// (an interior open cell). `block_at` answers the block id at a world
/// position, or `None` for unloaded space (which fails the shape closed).
///
/// Deterministic and bounded: at most `2 * max_height + 4 * max_width` cell
/// reads past the open-interior scan.
pub fn detect_vertical_portal(
    rule: &PortalRule,
    ignite: [i32; 3],
    block_at: &dyn Fn(i32, i32, i32) -> Option<String>,
) -> Option<PortalTransition> {
    let PortalTrigger::VerticalFrame {
        frame_block,
        min_width,
        min_height,
        max_width,
        max_height,
        portal_block,
        ..
    } = &rule.trigger
    else {
        return None;
    };
    let at = |x: i32, y: i32, z: i32| block_at(x, y, z);
    if !is_open(at(ignite[0], ignite[1], ignite[2]).as_deref(), portal_block) {
        return None;
    }

    // Vertical interior extent.
    let mut bottom = ignite[1];
    while is_open(at(ignite[0], bottom - 1, ignite[2]).as_deref(), portal_block) {
        bottom -= 1;
        if ignite[1] - bottom > *max_height as i32 {
            return None;
        }
    }
    let mut top = ignite[1];
    while is_open(at(ignite[0], top + 1, ignite[2]).as_deref(), portal_block) {
        top += 1;
        if top - ignite[1] > *max_height as i32 {
            return None;
        }
    }
    let height = (top - bottom + 1) as u32;
    if height < *min_height || height > *max_height {
        return None;
    }

    // Horizontal interior extent along each axis; a vertical frame portal is
    // exactly one cell thick, so the fixed coordinate is the ignite cell's.
    for axis in 0..2usize {
        let fixed = if axis == 0 { ignite[2] } else { ignite[0] };
        let cell = |a: i32, y: i32| -> Option<String> {
            if axis == 0 {
                at(a, y, fixed)
            } else {
                at(fixed, y, a)
            }
        };
        let mut low = if axis == 0 { ignite[0] } else { ignite[2] };
        while is_open(cell(low - 1, ignite[1]).as_deref(), portal_block) {
            low -= 1;
            if (if axis == 0 { ignite[0] } else { ignite[2] }) - low > *max_width as i32 {
                break;
            }
        }
        let mut high = if axis == 0 { ignite[0] } else { ignite[2] };
        while is_open(cell(high + 1, ignite[1]).as_deref(), portal_block) {
            high += 1;
            if high - (if axis == 0 { ignite[0] } else { ignite[2] }) > *max_width as i32 {
                break;
            }
        }
        let width = (high - low + 1) as u32;
        if width < *min_width || width > *max_width {
            continue;
        }

        let mut valid = true;
        'checks: {
            for y in bottom..=top {
                for a in low..=high {
                    if !is_open(cell(a, y).as_deref(), portal_block) {
                        valid = false;
                        break 'checks;
                    }
                }
                if cell(low - 1, y).as_deref() != Some(frame_block.as_str())
                    || cell(high + 1, y).as_deref() != Some(frame_block.as_str())
                {
                    valid = false;
                    break 'checks;
                }
            }
            for a in low..=high {
                if cell(a, bottom - 1).as_deref() != Some(frame_block.as_str())
                    || cell(a, top + 1).as_deref() != Some(frame_block.as_str())
                {
                    valid = false;
                    break 'checks;
                }
            }
        }
        if valid {
            let anchor = if axis == 0 {
                [low, bottom, ignite[2]]
            } else {
                [ignite[0], bottom, low]
            };
            return Some(PortalTransition {
                rule_name: rule.name.clone(),
                target_dim: rule.target_dim.clone(),
                anchor,
                cell_count: width * height,
            });
        }
    }
    None
}

/// Validate a `HorizontalPool` portal: a 4-connected fluid pool containing
/// `origin`, at least `min_size` cells, every orthogonal neighbour either in
/// the pool or in the caller-resolved border set. `border` is the expansion
/// of `border_tags_or_blocks` through the loaded tag table.
pub fn detect_horizontal_pool_portal(
    rule: &PortalRule,
    origin: [i32; 3],
    border: &BTreeSet<String>,
    block_at: &dyn Fn(i32, i32, i32) -> Option<String>,
) -> Option<PortalTransition> {
    let PortalTrigger::HorizontalPool {
        fluid_block,
        min_size,
        ..
    } = &rule.trigger
    else {
        return None;
    };
    if block_at(origin[0], origin[1], origin[2]).as_deref() != Some(fluid_block.as_str()) {
        return None;
    }
    let y = origin[1];
    let mut cells: BTreeSet<(i32, i32)> = BTreeSet::new();
    let mut queue: VecDeque<(i32, i32)> = VecDeque::from([(origin[0], origin[2])]);
    let mut valid = true;
    while let Some((x, z)) = queue.pop_front() {
        if !cells.insert((x, z)) {
            continue;
        }
        if cells.len() > MAX_POOL_CELLS {
            return None;
        }
        for (nx, nz) in [(x + 1, z), (x - 1, z), (x, z + 1), (x, z - 1)] {
            match block_at(nx, y, nz) {
                Some(id) if id == *fluid_block => queue.push_back((nx, nz)),
                Some(id) if border.contains(&id) => {}
                _ => {
                    valid = false;
                }
            }
        }
    }
    if !valid || (cells.len() as u32) < *min_size {
        return None;
    }
    let (ax, az) = cells.iter().next().copied()?;
    Some(PortalTransition {
        rule_name: rule.name.clone(),
        target_dim: rule.target_dim.clone(),
        anchor: [ax, y, az],
        cell_count: cells.len() as u32,
    })
}

/// Evaluate an `AltitudeThreshold` trigger: `entity_y` at or above the
/// threshold while riding/holding the required vehicle or item (if any).
pub fn check_altitude_portal(
    rule: &PortalRule,
    entity_y: f64,
    vehicle_or_held_item: Option<&str>,
) -> bool {
    let PortalTrigger::AltitudeThreshold {
        min_y_trigger,
        required_vehicle_or_item,
    } = &rule.trigger
    else {
        return false;
    };
    if entity_y < *min_y_trigger as f64 {
        return false;
    }
    match required_vehicle_or_item {
        Some(required) => vehicle_or_held_item == Some(required.as_str()),
        None => true,
    }
}

/// Evaluate a `DirectBlockInteract` trigger: a use action on `block_id`,
/// optionally requiring `held_item`.
pub fn check_interact_portal(
    rule: &PortalRule,
    clicked_block_id: &str,
    held_item: Option<&str>,
) -> bool {
    let PortalTrigger::DirectBlockInteract {
        block_id,
        required_item,
    } = &rule.trigger
    else {
        return false;
    };
    if clicked_block_id != block_id {
        return false;
    }
    match required_item {
        Some(required) => held_item == Some(required.as_str()),
        None => true,
    }
}

/// Coordinate-scale destination for a portal hop: `x/z` are scaled by the
/// exact `source_scale / target_scale` ratio (Overworld 1 → Nether 8 divides
/// by 8; a `coordinate_scale = 4.0` modded dimension from the Overworld
/// divides by 4), and `y` is preserved but safely clamped inside the target
/// `[min_y + 4, min_y + height - 8]` band so the arrival can never embed in
/// bedrock or fall out of the world.
///
/// Non-positive or non-finite scales are clamped to `1.0` rather than
/// producing NaN destinations.
pub fn compute_portal_destination(
    source_pos: [f64; 3],
    source_scale: f64,
    target: &DimTypeDescriptor,
) -> [f64; 3] {
    let sane = |v: f64| v.is_finite() && v > 0.0;
    let ratio = if sane(source_scale) && sane(target.coordinate_scale) {
        source_scale / target.coordinate_scale
    } else {
        1.0
    };
    let lo = target.min_y.saturating_add(4) as f64;
    let hi = target
        .min_y
        .saturating_add(target.height)
        .saturating_sub(8) as f64;
    let y = source_pos[1].clamp(lo, hi.max(lo));
    [source_pos[0] * ratio, y, source_pos[2] * ratio]
}

/// Assemble the [`TeleportIntent`] the server executes for a validated
/// transition.
pub fn build_teleport_intent(
    transition: &PortalTransition,
    source_pos: [f64; 3],
    source_scale: f64,
    target: &DimTypeDescriptor,
    grace_ticks: u32,
) -> TeleportIntent {
    TeleportIntent {
        target_dim: transition.target_dim.clone(),
        target_pos: compute_portal_destination(source_pos, source_scale, target),
        expect_teleport_grace_ticks: grace_ticks,
    }
}

// ---------------------------------------------------------------------------
// Per-dimension environment profile
// ---------------------------------------------------------------------------

/// How a dimension mod's environment changes physics and rendering for a
/// `dimension_type`, beyond what the JSON declares.
#[derive(Clone, Debug, PartialEq)]
pub struct DimensionEnvProfile {
    /// Dimension this profile applies to (`"namespace:path"`).
    pub dimension_id: String,
    /// Multiplier on the vanilla `minecraft:generic.gravity` attribute
    /// (1.21.1 default `0.08`): Moon `0.165`, Mars `0.38`, orbit `0.05`.
    pub gravity_multiplier: f64,
    /// Whether entities need oxygen (sealed suits / underwater breathing) in
    /// this dimension.
    pub requires_oxygen: bool,
    pub has_skylight: bool,
    pub has_ceiling: bool,
    /// Whether the 2-lane Underground Occlusion anti-xray lane applies:
    /// `has_skylight && !has_ceiling` — dimensions with an opaque ceiling
    /// (Nether-like) hide everything uniformly and get no benefit from the
    /// underground lane.
    pub supports_underground_occlusion: bool,
    /// `dimension_type.effects` sky/fog shader id.
    pub sky_effects: String,
}

impl DimensionEnvProfile {
    /// Derive a profile from a `dimension_type` descriptor plus the mod's
    /// declared physics overrides.
    pub fn derive(
        dim: &DimTypeDescriptor,
        gravity_multiplier: f64,
        requires_oxygen: bool,
    ) -> Self {
        Self {
            dimension_id: dim.id.clone(),
            gravity_multiplier,
            requires_oxygen,
            has_skylight: dim.has_skylight,
            has_ceiling: dim.has_ceiling,
            supports_underground_occlusion: dim.has_skylight && !dim.has_ceiling,
            sky_effects: dim.effects.clone(),
        }
    }

    /// Effective `generic.gravity` attribute value for vanilla's `0.08`
    /// default under this dimension's multiplier.
    pub fn effective_gravity(&self) -> f64 {
        0.08 * self.gravity_multiplier
    }
}

// ---------------------------------------------------------------------------
// Built-in reference profiles
// ---------------------------------------------------------------------------

/// A complete declarative profile for one well-known dimension-adding mod:
/// the `dimension_type`/`dimension` descriptors it is expected to ship, its
/// environment profiles, and its portal rules.
#[derive(Clone, Debug)]
pub struct DimensionModProfile {
    pub mod_id: &'static str,
    pub dimension_types: Vec<DimTypeDescriptor>,
    pub dimensions: Vec<DimEntryDescriptor>,
    pub env_profiles: Vec<DimensionEnvProfile>,
    pub portal_rules: Vec<PortalRule>,
}

fn frame_portal_rule(
    name: &str,
    target_dim: &str,
    frame_block: &str,
    igniter_item: &str,
    portal_block: &str,
) -> PortalRule {
    PortalRule::new(
        name,
        PortalTrigger::VerticalFrame {
            frame_block: frame_block.to_string(),
            min_width: 2,
            min_height: 3,
            max_width: 21,
            max_height: 21,
            igniter_item: igniter_item.to_string(),
            portal_block: portal_block.to_string(),
        },
        target_dim,
    )
}

/// Declarative reference profiles for the major dimension-adding mods: The
/// Aether, Twilight Forest, Ad Astra (Moon / Mars / Earth orbit), The
/// Undergarden, and Deeper & Darker. Values are safe defaults matching the
/// mods' shipped datapack JSON; discovered `dimension_type` JSON always wins
/// when it disagrees.
pub fn builtin_dimension_mod_profiles() -> Vec<DimensionModProfile> {
    let mut profiles = Vec::new();

    // --- The Aether -------------------------------------------------------
    let aether_type = DimTypeDescriptor {
        bed_works: false,
        respawn_anchor_works: true,
        ambient_light: 0.5,
        ..DimTypeDescriptor::defaults("aether:the_aether", 0, 384)
    };
    profiles.push(DimensionModProfile {
        mod_id: "aether",
        env_profiles: vec![DimensionEnvProfile::derive(&aether_type, 1.0, false)],
        dimension_types: vec![aether_type],
        dimensions: vec![DimEntryDescriptor {
            id: "aether:the_aether".to_string(),
            type_id: "aether:the_aether".to_string(),
            generator_kind: GeneratorKind::Noise,
        }],
        portal_rules: vec![frame_portal_rule(
            "aether:the_aether_portal",
            "aether:the_aether",
            "minecraft:glowstone",
            "minecraft:water_bucket",
            "aether:aether_portal",
        )],
    });

    // --- Twilight Forest --------------------------------------------------
    let tf_type = DimTypeDescriptor {
        bed_works: false,
        ambient_light: 0.1,
        ..DimTypeDescriptor::defaults("twilightforest:twilight_forest", -32, 288)
    };
    profiles.push(DimensionModProfile {
        mod_id: "twilightforest",
        env_profiles: vec![DimensionEnvProfile::derive(&tf_type, 1.0, false)],
        dimension_types: vec![tf_type],
        dimensions: vec![DimEntryDescriptor {
            id: "twilightforest:twilight_forest".to_string(),
            type_id: "twilightforest:twilight_forest".to_string(),
            generator_kind: GeneratorKind::Noise,
        }],
        portal_rules: vec![PortalRule::new(
            "twilightforest:twilight_portal",
            PortalTrigger::HorizontalPool {
                border_tags_or_blocks: vec![
                    "#minecraft:flowers".to_string(),
                    "#minecraft:saplings".to_string(),
                    "minecraft:grass_block".to_string(),
                    "minecraft:podzol".to_string(),
                    "minecraft:mycelium".to_string(),
                ],
                fluid_block: "minecraft:water".to_string(),
                min_size: 4,
                drop_item: "minecraft:diamond".to_string(),
                portal_block: "twilightforest:twilight_portal".to_string(),
            },
            "twilightforest:twilight_forest",
        )],
    });

    // --- Ad Astra: Moon / Mars / Earth orbit ------------------------------
    let ad_astra_dims = [
        ("ad_astra:moon", 0.165, -64, 384, "ad_astra:tier_1_rocket"),
        ("ad_astra:mars", 0.38, -64, 384, "ad_astra:tier_2_rocket"),
        ("ad_astra:earth_orbit", 0.05, -64, 384, "ad_astra:tier_1_rocket"),
    ];
    let mut types = Vec::new();
    let mut dims = Vec::new();
    let mut envs = Vec::new();
    let mut rules = Vec::new();
    for (id, gravity, min_y, height, rocket) in ad_astra_dims {
        let desc = DimTypeDescriptor {
            bed_works: false,
            respawn_anchor_works: false,
            effects: "minecraft:the_end".to_string(),
            ..DimTypeDescriptor::defaults(id, min_y, height)
        };
        envs.push(DimensionEnvProfile::derive(&desc, gravity, true));
        dims.push(DimEntryDescriptor {
            id: id.to_string(),
            type_id: id.to_string(),
            generator_kind: GeneratorKind::Noise,
        });
        types.push(desc);
        rules.push(PortalRule::new(
            format!("{id}:rocket_ascent"),
            PortalTrigger::AltitudeThreshold {
                min_y_trigger: 600,
                required_vehicle_or_item: Some(rocket.to_string()),
            },
            id,
        ));
    }
    profiles.push(DimensionModProfile {
        mod_id: "ad_astra",
        dimension_types: types,
        dimensions: dims,
        env_profiles: envs,
        portal_rules: rules,
    });

    // --- The Undergarden --------------------------------------------------
    let ug_type = DimTypeDescriptor {
        has_skylight: false,
        has_ceiling: true,
        bed_works: false,
        respawn_anchor_works: true,
        ambient_light: 0.1,
        effects: "minecraft:the_nether".to_string(),
        ..DimTypeDescriptor::defaults("undergarden:undergarden", 0, 256)
    };
    profiles.push(DimensionModProfile {
        mod_id: "undergarden",
        env_profiles: vec![DimensionEnvProfile::derive(&ug_type, 1.0, false)],
        dimension_types: vec![ug_type],
        dimensions: vec![DimEntryDescriptor {
            id: "undergarden:undergarden".to_string(),
            type_id: "undergarden:undergarden".to_string(),
            generator_kind: GeneratorKind::Noise,
        }],
        portal_rules: vec![frame_portal_rule(
            "undergarden:undergarden_portal",
            "undergarden:undergarden",
            "minecraft:stone_bricks",
            "undergarden:catalyst",
            "undergarden:undergarden_portal",
        )],
    });

    // --- Deeper & Darker ---------------------------------------------------
    let dd_type = DimTypeDescriptor {
        has_skylight: false,
        has_ceiling: false,
        bed_works: false,
        ambient_light: 0.05,
        effects: "minecraft:the_end".to_string(),
        ..DimTypeDescriptor::defaults("deeperdarker:otherside", -64, 384)
    };
    profiles.push(DimensionModProfile {
        mod_id: "deeperdarker",
        env_profiles: vec![DimensionEnvProfile::derive(&dd_type, 1.0, false)],
        dimension_types: vec![dd_type],
        dimensions: vec![DimEntryDescriptor {
            id: "deeperdarker:otherside".to_string(),
            type_id: "deeperdarker:otherside".to_string(),
            generator_kind: GeneratorKind::Noise,
        }],
        portal_rules: vec![frame_portal_rule(
            "deeperdarker:otherside_portal",
            "deeperdarker:otherside",
            "minecraft:reinforced_deepslate",
            "deeperdarker:heart_of_the_deep",
            "deeperdarker:otherside_portal",
        )],
    });

    profiles
}

// ---------------------------------------------------------------------------
// Multi-word (>64 section) light masks
// ---------------------------------------------------------------------------

/// Pack per-section presence flags into the little-endian `i64` word list the
/// protocol bitset carries for `0x27` light masks. Supports arbitrary
/// dimension heights: 26 light sections (Overworld) produce one word, a
/// 128-section modded dimension (2,048 blocks tall) produces two — matching
/// `DimId::light_mask_longs` in `src/worldgen.rs`.
pub fn pack_light_mask_longs(section_flags: &[bool]) -> Vec<i64> {
    let mut words = vec![0i64; section_flags.len().div_ceil(64)];
    for (i, &flag) in section_flags.iter().enumerate() {
        if flag {
            words[i / 64] |= 1i64 << (i % 64);
        }
    }
    words
}

/// Inverse of [`pack_light_mask_longs`]: reconstruct exactly `section_count`
/// flags from a word list. Missing trailing words read as `false`; bits past
/// `section_count` are dropped.
pub fn unpack_light_mask_longs(longs: &[i64], section_count: usize) -> Vec<bool> {
    (0..section_count)
        .map(|i| {
            longs
                .get(i / 64)
                .is_some_and(|word| word & (1i64 << (i % 64)) != 0)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap as Map;

    fn world() -> Map<(i32, i32, i32), String> {
        Map::new()
    }

    fn set(w: &mut Map<(i32, i32, i32), String>, x: i32, y: i32, z: i32, id: &str) {
        w.insert((x, y, z), id.to_string());
    }

    fn block_at(
        w: &Map<(i32, i32, i32), String>,
    ) -> impl Fn(i32, i32, i32) -> Option<String> + '_ {
        move |x, y, z| {
            Some(
                w.get(&(x, y, z))
                    .cloned()
                    .unwrap_or_else(|| "minecraft:air".to_string()),
            )
        }
    }

    /// Build a complete vertical frame portal: interior `w`×`h` open cells
    /// starting at `corner` in the plane normal to `normal_axis` (0 => plane
    /// varies along x, constant z; 2 => varies along z, constant x).
    fn build_frame(
        w: &mut Map<(i32, i32, i32), String>,
        corner: [i32; 3],
        inner_w: i32,
        inner_h: i32,
        normal_axis: usize,
        frame: &str,
    ) {
        for dy in -1..=inner_h {
            for da in -1..=inner_w {
                let on_frame = dy == -1 || dy == inner_h || da == -1 || da == inner_w;
                let (x, y, z) = if normal_axis == 0 {
                    (corner[0] + da, corner[1] + dy, corner[2])
                } else {
                    (corner[0], corner[1] + dy, corner[2] + da)
                };
                set(w, x, y, z, if on_frame { frame } else { "minecraft:air" });
            }
        }
    }

    #[test]
    fn discovery_parses_dimension_mod_data() {
        let entries = vec![
            (
                "data/aether/dimension_type/the_aether.json".to_string(),
                br#"{
                    "min_y": 0, "height": 384, "coordinate_scale": 1.0,
                    "has_skylight": true, "has_ceiling": false,
                    "ultrawarm": false, "natural": true,
                    "bed_works": false, "respawn_anchor_works": true,
                    "ambient_light": 0.5, "effects": "aether:aether"
                }"#
                .to_vec(),
            ),
            (
                "data/aether/dimension/the_aether.json".to_string(),
                br#"{"type": "aether:the_aether",
                    "generator": {"type": "minecraft:noise",
                                  "settings": "aether:aether_noise"}}"#
                    .to_vec(),
            ),
            (
                "data/aether/worldgen/noise_settings/aether_noise.json".to_string(),
                b"{}".to_vec(),
            ),
            (
                "data/aether/worldgen/biome/skyroot_forest.json".to_string(),
                b"{}".to_vec(),
            ),
            (
                "data/aether/worldgen/configured_feature/skyroot_tree.json".to_string(),
                b"{}".to_vec(),
            ),
            (
                "data/aether/worldgen/placed_feature/skyroot_tree.json".to_string(),
                b"{}".to_vec(),
            ),
            (
                "data/aether/worldgen/structure/bronze_dungeon.json".to_string(),
                b"{}".to_vec(),
            ),
            (
                "data/aether/tags/block/ores.json".to_string(),
                br##"{"values": ["aether:ambrosium_ore", "aether:zanite_ore",
                                 "#forge:ores"]}"##
                .to_vec(),
            ),
            (
                "data/c/tags/blocks/chests.json".to_string(),
                br#"{"values": [{"id": "aether:skyroot_chest", "required": false}]}"#
                    .to_vec(),
            ),
            (
                "data/aether/recipes/enchanted_dart.json".to_string(),
                b"{}".to_vec(),
            ),
        ];
        let data = discover_dimension_mod_entries(entries).unwrap();
        assert!(data.is_dimension_mod());
        assert_eq!(
            data.classification(),
            DimensionModClassification::PureDataDriven
        );

        let dt = &data.dimension_types[0];
        assert_eq!(dt.id, "aether:the_aether");
        assert_eq!(dt.min_y, 0);
        assert_eq!(dt.height, 384);
        assert!(!dt.bed_works && dt.respawn_anchor_works);
        assert_eq!(dt.effects, "aether:aether");

        let dim = &data.dimensions[0];
        assert_eq!(dim.type_id, "aether:the_aether");
        assert_eq!(dim.generator_kind, GeneratorKind::Noise);

        assert_eq!(data.worldgen.noise_settings, ["aether:aether_noise"]);
        assert_eq!(data.worldgen.biomes, ["aether:skyroot_forest"]);
        assert_eq!(data.worldgen.configured_features.len(), 1);
        assert_eq!(data.worldgen.placed_features.len(), 1);
        assert_eq!(data.worldgen.structures, ["aether:bronze_dungeon"]);

        // `#forge:ores` nested tags stay unexpanded; the recipe file is ignored.
        assert_eq!(
            data.ore_blocks,
            ["aether:ambrosium_ore", "aether:zanite_ore"]
        );
        assert_eq!(data.chest_blocks, ["aether:skyroot_chest"]);
        assert_eq!(data.anti_xray_hidden_blocks().len(), 3);
    }

    #[test]
    fn discovery_flags_custom_chunk_generators() {
        let entries = vec![
            (
                "data/deep/dimension_type/otherside.json".to_string(),
                br#"{"min_y": -64, "height": 384}"#.to_vec(),
            ),
            (
                "data/deep/dimension/otherside.json".to_string(),
                br#"{"type": "deep:otherside",
                     "generator": {"type": "deep:echoing_void_gen"}}"#
                    .to_vec(),
            ),
        ];
        let data = discover_dimension_mod_entries(entries).unwrap();
        match data.classification() {
            DimensionModClassification::CustomChunkGenerator(kinds) => {
                assert_eq!(kinds, ["deep:echoing_void_gen"]);
            }
            other => panic!("expected CustomChunkGenerator, got {other:?}"),
        }
        assert!(!data.is_pure_data_driven());
    }

    #[test]
    fn discovery_defaults_and_void_generator() {
        let entries = vec![(
            "data/flat/dimension/flat.json".to_string(),
            br#"{"type": "flat:flat", "generator": {"type": "minecraft:flat"}}"#
                .to_vec(),
        )];
        let data = discover_dimension_mod_entries(entries).unwrap();
        assert!(!data.is_dimension_mod()); // no dimension_type entry
        assert_eq!(data.dimensions[0].generator_kind, GeneratorKind::Flat);
    }

    #[test]
    fn vertical_frame_portal_detects_both_orientations() {
        let profiles = builtin_dimension_mod_profiles();
        let rule = profiles[0]
            .portal_rules
            .iter()
            .find(|r| r.name == "aether:the_aether_portal")
            .unwrap()
            .clone();

        // 4x5 glowstone frame (2x3 interior) in the x plane at z=7.
        let mut w = world();
        build_frame(&mut w, [3, 64, 7], 2, 3, 0, "minecraft:glowstone");
        let at = block_at(&w);
        let t = detect_vertical_portal(&rule, [4, 65, 7], &at).expect("x-plane portal");
        assert_eq!(t.target_dim, "aether:the_aether");
        assert_eq!(t.anchor, [3, 64, 7]);
        assert_eq!(t.cell_count, 6);

        // Same frame rotated into the z plane.
        let mut w = world();
        build_frame(&mut w, [10, 64, 20], 2, 3, 2, "minecraft:glowstone");
        let at = block_at(&w);
        let t = detect_vertical_portal(&rule, [10, 65, 21], &at).expect("z-plane portal");
        assert_eq!(t.anchor, [10, 64, 20]);
    }

    #[test]
    fn vertical_frame_portal_rejects_broken_and_undersized_frames() {
        let profiles = builtin_dimension_mod_profiles();
        let rule = profiles[0].portal_rules[0].clone();

        // A missing frame block in the side wall fails closed.
        let mut w = world();
        build_frame(&mut w, [3, 64, 7], 2, 3, 0, "minecraft:glowstone");
        set(&mut w, 2, 65, 7, "minecraft:air");
        let at = block_at(&w);
        assert!(detect_vertical_portal(&rule, [4, 65, 7], &at).is_none());

        // 1-wide interior is below min_width even if fully framed.
        let mut w = world();
        build_frame(&mut w, [3, 64, 7], 1, 3, 0, "minecraft:glowstone");
        let at = block_at(&w);
        assert!(detect_vertical_portal(&rule, [3, 65, 7], &at).is_none());
    }

    #[test]
    fn horizontal_pool_portal_detects_twilight_shape() {
        let profiles = builtin_dimension_mod_profiles();
        let rule = profiles
            .iter()
            .flat_map(|p| p.portal_rules.iter())
            .find(|r| r.name == "twilightforest:twilight_portal")
            .unwrap()
            .clone();

        // 2x2 water pool at y=63 bordered by flowers/grass.
        let mut w = world();
        for x in 3..=6 {
            for z in 3..=6 {
                let id = if (4..=5).contains(&x) && (4..=5).contains(&z) {
                    "minecraft:water"
                } else {
                    "minecraft:grass_block"
                };
                set(&mut w, x, 63, z, id);
            }
        }
        set(&mut w, 3, 64, 4, "minecraft:poppy");
        let border: BTreeSet<String> = ["minecraft:grass_block"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let at = block_at(&w);
        let t = detect_horizontal_pool_portal(&rule, [4, 63, 4], &border, &at)
            .expect("2x2 pool");
        assert_eq!(t.target_dim, "twilightforest:twilight_forest");
        assert_eq!(t.cell_count, 4);
        assert_eq!(t.anchor, [4, 63, 4]);

        // A lone water cell under min_size never activates.
        assert!(detect_horizontal_pool_portal(&rule, [4, 63, 4], &BTreeSet::new(), &at)
            .is_none());
    }

    #[test]
    fn altitude_and_interact_triggers() {
        let profiles = builtin_dimension_mod_profiles();
        let ad_astra = profiles.iter().find(|p| p.mod_id == "ad_astra").unwrap();
        let moon_rule = ad_astra
            .portal_rules
            .iter()
            .find(|r| r.target_dim == "ad_astra:moon")
            .unwrap();
        assert!(check_altitude_portal(moon_rule, 600.0, Some("ad_astra:tier_1_rocket")));
        assert!(!check_altitude_portal(moon_rule, 599.9, Some("ad_astra:tier_1_rocket")));
        assert!(!check_altitude_portal(moon_rule, 650.0, Some("minecraft:elytra")));
        assert!(!check_altitude_portal(moon_rule, 650.0, None));

        let pad = PortalRule::new(
            "test:pad",
            PortalTrigger::DirectBlockInteract {
                block_id: "test:teleport_pad".to_string(),
                required_item: Some("test:dim_key".to_string()),
            },
            "test:deep",
        );
        assert!(check_interact_portal(&pad, "test:teleport_pad", Some("test:dim_key")));
        assert!(!check_interact_portal(&pad, "test:teleport_pad", None));
        assert!(!check_interact_portal(&pad, "minecraft:stone", Some("test:dim_key")));
    }

    #[test]
    fn destination_uses_exact_scale_ratio_and_safe_clamp() {
        // Nether-style: source 1.0 -> target 8.0 divides coordinates by 8.
        let netherish = DimTypeDescriptor {
            coordinate_scale: 8.0,
            ..DimTypeDescriptor::defaults("test:netherish", 0, 256)
        };
        let dst = compute_portal_destination([80.0, 70.0, -160.0], 1.0, &netherish);
        assert_eq!(dst, [10.0, 70.0, -20.0]);

        // Return trip multiplies by 8.
        let overworldish = DimTypeDescriptor::defaults("test:over", -64, 384);
        let dst = compute_portal_destination([10.0, 70.0, -20.0], 8.0, &overworldish);
        assert_eq!(dst, [80.0, 70.0, -160.0]);

        // y clamps into [min_y + 4, min_y + height - 8].
        let low = DimTypeDescriptor::defaults("test:shallow", 0, 64);
        let dst = compute_portal_destination([0.0, 500.0, 0.0], 1.0, &low);
        assert_eq!(dst[1], 56.0); // 0 + 64 - 8
        let dst = compute_portal_destination([0.0, -50.0, 0.0], 1.0, &low);
        assert_eq!(dst[1], 4.0); // 0 + 4

        // Degenerate scales fall back to 1:1 rather than NaN.
        let bad = DimTypeDescriptor {
            coordinate_scale: 0.0,
            ..DimTypeDescriptor::defaults("test:bad", 0, 64)
        };
        let dst = compute_portal_destination([5.0, 10.0, 5.0], 1.0, &bad);
        assert_eq!(dst, [5.0, 10.0, 5.0]);
    }

    #[test]
    fn teleport_intent_carries_anticheat_grace() {
        let transition = PortalTransition {
            rule_name: "aether:the_aether_portal".to_string(),
            target_dim: "aether:the_aether".to_string(),
            anchor: [3, 64, 7],
            cell_count: 6,
        };
        let target = DimTypeDescriptor::defaults("aether:the_aether", 0, 384);
        let intent = build_teleport_intent(
            &transition,
            [32.0, 70.0, 56.0],
            1.0,
            &target,
            DEFAULT_TELEPORT_GRACE_TICKS,
        );
        assert_eq!(intent.target_dim, "aether:the_aether");
        assert_eq!(intent.target_pos, [32.0, 70.0, 56.0]);
        assert_eq!(intent.expect_teleport_grace_ticks, 20);
    }

    #[test]
    fn arrival_platform_specs_match_trigger_kind() {
        let profiles = builtin_dimension_mod_profiles();
        let aether = &profiles[0].portal_rules[0];
        let platform = arrival_platform(aether);
        assert_eq!(platform.floor_block, "minecraft:glowstone");
        assert_eq!(platform.portal_block.as_deref(), Some("aether:aether_portal"));
        assert_eq!((platform.portal_width, platform.portal_height), (2, 3));

        let ad_astra = profiles.iter().find(|p| p.mod_id == "ad_astra").unwrap();
        let pad = arrival_platform(&ad_astra.portal_rules[0]);
        assert_eq!(pad.floor_block, "minecraft:stone");
        assert_eq!(pad.portal_block, None);
    }

    #[test]
    fn env_profile_derives_occlusion_and_gravity() {
        let profiles = builtin_dimension_mod_profiles();
        let ad_astra = profiles.iter().find(|p| p.mod_id == "ad_astra").unwrap();
        let moon = ad_astra
            .env_profiles
            .iter()
            .find(|e| e.dimension_id == "ad_astra:moon")
            .unwrap();
        assert_eq!(moon.gravity_multiplier, 0.165);
        assert!(moon.requires_oxygen);
        assert!(moon.supports_underground_occlusion); // skylight, no ceiling
        assert!((moon.effective_gravity() - 0.0132).abs() < 1e-9);

        let undergarden = profiles
            .iter()
            .find(|p| p.mod_id == "undergarden")
            .unwrap();
        let ug = &undergarden.env_profiles[0];
        assert!(!ug.has_skylight && ug.has_ceiling);
        assert!(!ug.supports_underground_occlusion);
    }

    #[test]
    fn builtin_profiles_cover_required_mods() {
        let profiles = builtin_dimension_mod_profiles();
        let ids: Vec<&str> = profiles.iter().map(|p| p.mod_id).collect();
        for expected in ["aether", "twilightforest", "ad_astra", "undergarden", "deeperdarker"] {
            assert!(ids.contains(&expected), "missing builtin profile {expected}");
        }
        let registry = PortalRegistry::with_builtin_rules();
        assert!(registry.find("aether:the_aether_portal").is_some());
        assert_eq!(
            registry
                .rules_to("ad_astra:moon")
                .next()
                .map(|r| r.target_dim.as_str()),
            Some("ad_astra:moon")
        );
    }

    #[test]
    fn light_mask_round_trip_beyond_64_sections() {
        // 128 sections = 2048-block-tall dimension: 2+ words.
        let flags: Vec<bool> = (0..128).map(|i| i % 3 == 0).collect();
        let packed = pack_light_mask_longs(&flags);
        assert_eq!(packed.len(), 2);
        assert_eq!(packed[0] as u64 & 1, 1);
        let unpacked = unpack_light_mask_longs(&packed, 128);
        assert_eq!(unpacked, flags);

        // Empty input and exact word boundaries.
        assert!(pack_light_mask_longs(&[]).is_empty());
        let exact = vec![true; 64];
        assert_eq!(pack_light_mask_longs(&exact), vec![-1i64]);
        assert_eq!(unpack_light_mask_longs(&[-1i64], 64), exact);

        // Truncated read: missing words answer false, extra bits are dropped.
        assert_eq!(unpack_light_mask_longs(&[], 3), vec![false; 3]);
        assert_eq!(unpack_light_mask_longs(&[-1i64], 4).len(), 4);
    }
}
