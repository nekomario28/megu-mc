// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
//! Declarative `DataFixer` for the `1.21.1` (DataVersion 3955) -> `26.3`
//! (DataVersion 5023) bridge, plus the explicit opt-in
//! [`DataFixer::v26_3_to_1_21_1_downgrade`] mirror.
//!
//! A fixer is an ordered list of [`DataFixStep`]s over the format-agnostic
//! [`DataValue`] tree, so fixes are pure functions — no NBT/JSON coupling, no
//! I/O, deterministic and testable. A step runs when the document's
//! DataVersion is at or before the step's `from_dv`; running the fixer on a
//! document already at the target DataVersion is therefore a clean no-op, and
//! a document *newer* than the target is refused outright
//! ([`DataFixError::NewerThanTarget`]) instead of being silently downgraded.
//! The downgrade pipeline exists only behind its own constructor and is the
//! exact mirror: it refuses documents newer than `5023`, reverses each fix,
//! warns on 26.3-only entries it cannot represent, and stamps `3955`.

use mc_spine_core::version::TargetVersion;
use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;

/// DataVersion shipped by Minecraft 1.21.1 — the same constant
/// [`TargetVersion::V1_21_1`] carries, so the fixer and the version registry
/// can never drift apart.
pub const DV_1_21_1: i32 = TargetVersion::V1_21_1.meta().data_version;
/// DataVersion shipped by Minecraft 26.3 — [`TargetVersion::V26_3`]'s.
pub const DV_26_3: i32 = TargetVersion::V26_3.meta().data_version;

/// Minimal NBT/JSON-compatible tagged tree the fix steps operate on.
#[derive(Clone, Debug, Default, PartialEq)]
pub enum DataValue {
    #[default]
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(String),
    List(Vec<DataValue>),
    Map(BTreeMap<String, DataValue>),
}

impl DataValue {
    pub fn as_map(&self) -> Option<&BTreeMap<String, DataValue>> {
        match self {
            DataValue::Map(m) => Some(m),
            _ => None,
        }
    }

    pub fn as_map_mut(&mut self) -> Option<&mut BTreeMap<String, DataValue>> {
        match self {
            DataValue::Map(m) => Some(m),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            DataValue::Str(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_int(&self) -> Option<i64> {
        match self {
            DataValue::Int(v) => Some(*v),
            _ => None,
        }
    }
}

/// Outcome of one [`DataFixer::fix`] run.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct DataFixReport {
    /// DataVersion read from the document before any step ran.
    pub original_dv: i32,
    /// DataVersion on the document after the run.
    pub final_dv: i32,
    /// Names of the steps that transformed the document, in pipeline order.
    /// A step that ran but found its input already in output shape (a clean
    /// no-op) is not listed.
    pub applied_steps: Vec<String>,
    /// Resource-location values rewritten by the rename step.
    pub renamed_entries: usize,
    /// Non-fatal observations (e.g. a DataVersion no step covers).
    pub warnings: Vec<String>,
}

impl DataFixReport {
    fn warn(&mut self, warning: impl Into<String>) {
        self.warnings.push(warning.into());
    }
}

/// Why a fix run refused to touch the document.
#[derive(Clone, Debug, PartialEq)]
pub enum DataFixError {
    /// The document root is not a map, so it cannot carry a `DataVersion`.
    RootNotMap,
    /// No `DataVersion` key on the root map.
    MissingDataVersion,
    /// `DataVersion` exists but is not an integer.
    MalformedDataVersion,
    /// The document is newer than this fixer's target. Refused fail-closed:
    /// a forward fixer must never silently downgrade or corrupt newer data.
    NewerThanTarget { file_dv: i32, target_dv: i32 },
}

impl fmt::Display for DataFixError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DataFixError::RootNotMap => write!(f, "document root is not a map"),
            DataFixError::MissingDataVersion => write!(f, "document has no DataVersion"),
            DataFixError::MalformedDataVersion => {
                write!(f, "document DataVersion is not an integer")
            }
            DataFixError::NewerThanTarget {
                file_dv,
                target_dv,
            } => write!(
                f,
                "document DataVersion {file_dv} is newer than fixer target {target_dv}"
            ),
        }
    }
}

impl Error for DataFixError {}

/// One named migration unit. In the forward direction `apply` runs when
/// `file_dv <= from_dv`; in the downgrade direction it runs when
/// `file_dv >= from_dv`. Every step must leave the document unchanged when
/// its input shape is already the output shape (internally idempotent).
pub struct DataFixStep {
    pub name: &'static str,
    /// DataVersion the step upgrades *from* (forward) or downgrades *from*
    /// (downgrade pipeline).
    pub from_dv: i32,
    /// DataVersion the step upgrades *to* (forward) or downgrades *to*
    /// (downgrade pipeline).
    pub to_dv: i32,
    pub apply: fn(&mut DataValue, &mut DataFixReport),
}

/// Which way a pipeline walks the DataVersion range.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FixDirection {
    /// `source_dv` -> `target_dv` upgrade; refuses documents *newer* than
    /// `target_dv`.
    Forward,
    /// `source_dv` -> `target_dv` downgrade; refuses documents *newer* than
    /// `source_dv` (the format the steps were written against).
    Downgrade,
}

/// An ordered [`DataFixStep`] pipeline from `source_dv` to `target_dv`.
pub struct DataFixer {
    source_dv: i32,
    target_dv: i32,
    steps: Vec<DataFixStep>,
    direction: FixDirection,
}

impl DataFixer {
    pub fn new(source_dv: i32, target_dv: i32, mut steps: Vec<DataFixStep>) -> Self {
        steps.sort_by_key(|s| s.from_dv);
        Self {
            source_dv,
            target_dv,
            steps,
            direction: FixDirection::Forward,
        }
    }

    /// The `1.21.1` (`3955`) -> `26.3` (`5023`) pipeline: block-state key
    /// normalization, `level.dat` version metadata, chunk-section layout and
    /// palette migration, item-component upgrades, `pack_format` ->
    /// `min_format`/`max_format` migration, the resource-location rename
    /// table and the `DataVersion` stamp.
    pub fn v1_21_1_to_26_3() -> Self {
        Self::new(
            DV_1_21_1,
            DV_26_3,
            vec![
                DataFixStep {
                    name: "block_state_key_normalize",
                    from_dv: DV_1_21_1,
                    to_dv: DV_26_3,
                    apply: fix_block_state_keys,
                },
                DataFixStep {
                    name: "level_dat_version_metadata",
                    from_dv: DV_1_21_1,
                    to_dv: DV_26_3,
                    apply: fix_level_dat_26_3,
                },
                DataFixStep {
                    name: "chunk_section_layout",
                    from_dv: DV_1_21_1,
                    to_dv: DV_26_3,
                    apply: fix_chunk_sections,
                },
                DataFixStep {
                    name: "item_components_upgrade",
                    from_dv: DV_1_21_1,
                    to_dv: DV_26_3,
                    apply: fix_item_components,
                },
                DataFixStep {
                    name: "pack_format_min_max",
                    from_dv: DV_1_21_1,
                    to_dv: DV_26_3,
                    apply: fix_pack_format,
                },
                DataFixStep {
                    name: "resource_location_renames",
                    from_dv: DV_1_21_1,
                    to_dv: DV_26_3,
                    apply: fix_resource_renames,
                },
                DataFixStep {
                    name: "stamp_data_version_26_3",
                    from_dv: DV_1_21_1,
                    to_dv: DV_26_3,
                    apply: stamp_26_3,
                },
            ],
        )
    }

    /// The explicit `26.3` (`5023`) -> `1.21.1` (`3955`) downgrade pipeline.
    /// This is a *separate* constructor — the forward fixer never downgrades,
    /// and this fixer must be requested deliberately. It reverses the rename
    /// table and `min_format`/`max_format` -> `pack_format`, restores the
    /// `level.dat` version metadata and flat enchantment components, warns on
    /// entries that only exist in 26.3, and stamps `DataVersion` `3955`.
    pub fn v26_3_to_1_21_1_downgrade() -> Self {
        let mut fixer = Self::new(
            DV_26_3,
            DV_1_21_1,
            vec![
                DataFixStep {
                    name: "level_dat_version_metadata_restore",
                    from_dv: DV_26_3,
                    to_dv: DV_1_21_1,
                    apply: fix_level_dat_1_21_1,
                },
                DataFixStep {
                    name: "item_components_downgrade",
                    from_dv: DV_26_3,
                    to_dv: DV_1_21_1,
                    apply: downgrade_item_components,
                },
                DataFixStep {
                    name: "pack_format_restore",
                    from_dv: DV_26_3,
                    to_dv: DV_1_21_1,
                    apply: restore_pack_format,
                },
                DataFixStep {
                    name: "resource_location_renames_reverse",
                    from_dv: DV_26_3,
                    to_dv: DV_1_21_1,
                    apply: fix_resource_renames_reverse,
                },
                DataFixStep {
                    name: "report_26_3_only_entries",
                    from_dv: DV_26_3,
                    to_dv: DV_1_21_1,
                    apply: report_26_3_only_entries,
                },
                DataFixStep {
                    name: "stamp_data_version_1_21_1",
                    from_dv: DV_26_3,
                    to_dv: DV_1_21_1,
                    apply: stamp_1_21_1,
                },
            ],
        );
        fixer.direction = FixDirection::Downgrade;
        // Mirror of `new`: newest-covered version first for a downgrade run.
        fixer.steps.sort_by_key(|s| std::cmp::Reverse(s.from_dv));
        fixer
    }

    pub fn source_dv(&self) -> i32 {
        self.source_dv
    }

    pub fn target_dv(&self) -> i32 {
        self.target_dv
    }

    pub fn steps(&self) -> &[DataFixStep] {
        &self.steps
    }

    /// Migrate `root` toward `target_dv` along the fixer's direction. Forward
    /// steps are skipped when the document's DataVersion is past the version
    /// they upgrade from — the document already carries that step's output
    /// shape; downgrade steps are the mirror. Documents newer than the
    /// pipeline's ceiling (`target_dv` forward, `source_dv` downgrade) are
    /// refused.
    pub fn fix(&self, root: &mut DataValue) -> Result<DataFixReport, DataFixError> {
        let file_dv = match root.as_map() {
            None => return Err(DataFixError::RootNotMap),
            Some(map) => match map.get("DataVersion") {
                None => return Err(DataFixError::MissingDataVersion),
                // Out-of-i32-range counts as "newer": it can only refuse.
                Some(v) => v
                    .as_int()
                    .map(|dv| i32::try_from(dv).unwrap_or(i32::MAX))
                    .ok_or(DataFixError::MalformedDataVersion)?,
            },
        };
        // The fixer refuses documents written by a version it has no schema
        // for: anything past `target_dv` going forward, or past `source_dv`
        // (the newest format the steps understand) going backward.
        let ceiling = match self.direction {
            FixDirection::Forward => self.target_dv,
            FixDirection::Downgrade => self.source_dv,
        };
        if file_dv > ceiling {
            return Err(DataFixError::NewerThanTarget {
                file_dv,
                target_dv: ceiling,
            });
        }

        let mut report = DataFixReport {
            original_dv: file_dv,
            final_dv: file_dv,
            ..DataFixReport::default()
        };
        for step in &self.steps {
            let covered = match self.direction {
                FixDirection::Forward => {
                    file_dv <= step.from_dv && step.to_dv <= self.target_dv
                }
                FixDirection::Downgrade => {
                    file_dv >= step.from_dv && step.to_dv >= self.target_dv
                }
            };
            if covered {
                let before = root.clone();
                (step.apply)(root, &mut report);
                // A step is recorded only when it actually transformed the
                // document: a covered step whose input shape was already the
                // output shape is a clean no-op, not an application.
                if *root != before {
                    report.applied_steps.push(step.name.to_string());
                }
            }
        }
        if let Some(dv) = root
            .as_map()
            .and_then(|m| m.get("DataVersion"))
            .and_then(DataValue::as_int)
        {
            report.final_dv = i32::try_from(dv).unwrap_or(i32::MAX);
        }
        if report.final_dv != self.target_dv {
            // The document sits at a DataVersion no step in this pipeline
            // covers (strictly between source and target). It is left
            // untouched rather than stamped with a version it was not
            // migrated to — the warning is what records that refusal.
            report.warn(format!(
                "DataVersion {} is not covered by this fixer ({} -> {}); document left unchanged",
                file_dv, self.source_dv, self.target_dv
            ));
        }
        Ok(report)
    }
}

/// Visit every value in the tree, the visited value itself first.
fn walk_mut(value: &mut DataValue, f: &mut impl FnMut(&mut DataValue)) {
    f(value);
    match value {
        DataValue::List(items) => items.iter_mut().for_each(|v| walk_mut(v, f)),
        DataValue::Map(map) => map.values_mut().for_each(|v| walk_mut(v, f)),
        _ => {}
    }
}

/// `pack_format` numbers were rebased when `min_format`/`max_format` replaced
/// the scalar: the 1.21.1 datapack format 48 is 26.3's major 121. The offset is
/// linear across the range this pipeline bridges.
const PACK_FORMAT_REBASE: i64 = 73;

fn pack_format_tuple(pack_format: i64) -> [i64; 2] {
    [pack_format + PACK_FORMAT_REBASE, 0]
}

/// Curated resource-location renames landing between 1.21.1 and 26.3. Only
/// exact full `namespace:path` matches are rewritten, so unknown and modded
/// namespaces pass through untouched by construction.
const RENAMES: &[(&str, &str)] = &[
    ("minecraft:grass", "minecraft:short_grass"),
    ("minecraft:boat", "minecraft:oak_boat"),
    ("minecraft:chest_boat", "minecraft:oak_chest_boat"),
    ("minecraft:mooshroom", "minecraft:mushroom_cow"),
    ("minecraft:snow", "minecraft:snow_layer"),
];

fn lookup_rename(id: &str) -> Option<&'static str> {
    RENAMES
        .iter()
        .find(|(from, _)| *from == id)
        .map(|(_, to)| *to)
}

/// A map is a block-state record when it carries only `id`/`properties` (the
/// 26.3 serialization spelling) or `Name`/`Properties` (the persisted NBT
/// spelling). Item stacks and entities also have `id`, so the pure record
/// check is what keeps them out of this step.
fn is_block_state_map(map: &BTreeMap<String, DataValue>) -> bool {
    let id_str = map
        .get("id")
        .or_else(|| map.get("Name"))
        .is_some_and(|v| v.as_str().is_some());
    let pure_keys = map
        .keys()
        .all(|k| matches!(k.as_str(), "id" | "properties" | "Name" | "Properties"));
    let props_ok = map
        .get("properties")
        .or_else(|| map.get("Properties"))
        .is_none_or(|p| matches!(p, DataValue::Map(_)));
    id_str && pure_keys && props_ok
}

/// Rename a block-state record's keys to the persisted `Name`/`Properties`
/// spelling. Caller must have already confirmed [`is_block_state_map`].
fn normalize_block_state_map(map: &mut BTreeMap<String, DataValue>) {
    if let Some(id) = map.remove("id") {
        map.insert("Name".to_string(), id);
    }
    if let Some(properties) = map.remove("properties") {
        map.insert("Properties".to_string(), properties);
    }
}

/// Canonicalize block-state records to `Name`/`Properties`: `{id, properties}`
/// maps are renamed, `{Name, Properties}` maps already carry the output shape.
fn fix_block_state_keys(root: &mut DataValue, _report: &mut DataFixReport) {
    walk_mut(root, &mut |value| {
        let Some(map) = value.as_map_mut() else { return };
        if !is_block_state_map(map) {
            return;
        }
        normalize_block_state_map(map);
    });
}

/// Scalar `pack_format` -> `min_format`/`max_format` `[major, minor]` tuples on
/// every pack metadata map (e.g. `pack.mcmeta`'s `pack` object).
fn fix_pack_format(root: &mut DataValue, report: &mut DataFixReport) {
    walk_mut(root, &mut |value| {
        let Some(map) = value.as_map_mut() else { return };
        let Some(format) = map.get("pack_format").and_then(DataValue::as_int) else {
            return;
        };
        map.remove("pack_format");
        let tuple = || {
            DataValue::List(
                pack_format_tuple(format)
                    .into_iter()
                    .map(DataValue::Int)
                    .collect(),
            )
        };
        for key in ["min_format", "max_format"] {
            if map.contains_key(key) {
                report.warn(format!(
                    "pack metadata already carries {key}; dropped scalar pack_format {format}"
                ));
            } else {
                map.insert(key.to_string(), tuple());
            }
        }
    });
}

/// Apply [`RENAMES`] to every string value in the tree.
fn fix_resource_renames(root: &mut DataValue, report: &mut DataFixReport) {
    walk_mut(root, &mut |value| {
        let DataValue::Str(id) = value else { return };
        if let Some(to) = lookup_rename(id) {
            *id = to.to_string();
            report.renamed_entries += 1;
        }
    });
}

/// The stamp step for the 1.21.1 -> 26.3 pipeline.
fn stamp_26_3(root: &mut DataValue, _report: &mut DataFixReport) {
    if let Some(map) = root.as_map_mut() {
        map.insert("DataVersion".to_string(), DataValue::Int(DV_26_3 as i64));
    }
}

/// Release-name strings stamped into `Data.Version.Name`.
const VERSION_NAME_1_21_1: &str = "1.21.1";
const VERSION_NAME_26_3: &str = "26.3";

/// A `Data` compound only counts as `level.dat`-shaped when it carries the
/// version metadata real level.dat files always have. Anything else named
/// `Data` (modded payloads, custom tags) is left alone.
fn is_level_dat_data(map: &BTreeMap<String, DataValue>) -> bool {
    matches!(map.get("Version"), Some(DataValue::Map(_)))
        || matches!(map.get("DataVersion"), Some(DataValue::Int(_)))
}

/// Stamp `Data.DataVersion` and `Data.Version.{Id, Name, Snapshot}`.
fn stamp_level_dat(data: &mut BTreeMap<String, DataValue>, dv: i32, name: &str) {
    if let Some(version) = data.get_mut("Version").and_then(DataValue::as_map_mut) {
        version.insert("Id".to_string(), DataValue::Int(dv as i64));
        version.insert("Name".to_string(), DataValue::Str(name.to_string()));
        version.insert("Snapshot".to_string(), DataValue::Bool(false));
    }
    if matches!(data.get("DataVersion"), Some(DataValue::Int(_))) {
        data.insert("DataVersion".to_string(), DataValue::Int(dv as i64));
    }
}

/// `level.dat` keeps a second copy of the version inside `Data`: this step
/// migrates it. The root `DataVersion` is handled by the pipeline's stamp
/// step, so a document without a level.dat `Data` compound is untouched.
fn fix_level_dat_26_3(root: &mut DataValue, _report: &mut DataFixReport) {
    let Some(data) = root
        .as_map_mut()
        .and_then(|m| m.get_mut("Data"))
        .and_then(DataValue::as_map_mut)
    else {
        return;
    };
    if is_level_dat_data(data) {
        stamp_level_dat(data, DV_26_3, VERSION_NAME_26_3);
    }
}

/// Downgrade mirror of [`fix_level_dat_26_3`].
fn fix_level_dat_1_21_1(root: &mut DataValue, _report: &mut DataFixReport) {
    let Some(data) = root
        .as_map_mut()
        .and_then(|m| m.get_mut("Data"))
        .and_then(DataValue::as_map_mut)
    else {
        return;
    };
    if is_level_dat_data(data) {
        stamp_level_dat(data, DV_1_21_1, VERSION_NAME_1_21_1);
    }
}

/// Normalize one palette kind inside a chunk section. `block_states` palettes
/// hold block-state records (key-normalized and rename-migrated here);
/// `biomes` palettes hold plain resource-location strings and are validated
/// only — the rename table is a block/item/entity mapping, not a biome one.
fn fix_section_palette(
    section_index: usize,
    kind: &str,
    container: &mut BTreeMap<String, DataValue>,
    report: &mut DataFixReport,
) {
    let Some(DataValue::List(entries)) = container.get_mut("palette") else {
        if container.contains_key("palette") {
            report.warn(format!(
                "sections[{section_index}].{kind}.palette is not a list; left untouched"
            ));
        }
        return;
    };
    for (entry_index, entry) in entries.iter_mut().enumerate() {
        if kind == "biomes" {
            if !matches!(entry, DataValue::Str(_)) {
                report.warn(format!(
                    "sections[{section_index}].biomes.palette[{entry_index}] is not a biome id; left untouched"
                ));
            }
            continue;
        }
        let Some(entry_map) = entry.as_map_mut() else {
            report.warn(format!(
                "sections[{section_index}].block_states.palette[{entry_index}] is not a compound; left untouched"
            ));
            continue;
        };
        if !is_block_state_map(entry_map) {
            report.warn(format!(
                "sections[{section_index}].block_states.palette[{entry_index}] is not a block state; left untouched"
            ));
            continue;
        }
        normalize_block_state_map(entry_map);
        if let Some(DataValue::Str(id)) = entry_map.get_mut("Name") {
            if let Some(to) = lookup_rename(id) {
                *id = to.to_string();
                report.renamed_entries += 1;
            }
        }
    }
}

/// Chunk-section layout normalization: the persisted section index key is
/// `Y` (a stray `y` from JSON-derived docs is folded in, a `y`/`Y` collision
/// keeps `Y` and warns), and every `block_states`/`biomes` palette entry is
/// validated, with block-state key and rename migrations applied in place.
fn fix_chunk_sections(root: &mut DataValue, report: &mut DataFixReport) {
    walk_mut(root, &mut |value| {
        let Some(map) = value.as_map_mut() else { return };
        let Some(DataValue::List(sections)) = map.get_mut("sections") else {
            if map.contains_key("sections") {
                report.warn("sections key is not a list; left untouched".to_string());
            }
            return;
        };
        for (index, section) in sections.iter_mut().enumerate() {
            let Some(section_map) = section.as_map_mut() else {
                report.warn(format!("sections[{index}] is not a compound; left untouched"));
                continue;
            };
            match (section_map.contains_key("Y"), section_map.remove("y")) {
                (false, Some(y)) => {
                    section_map.insert("Y".to_string(), y);
                }
                (true, Some(_)) => report.warn(format!(
                    "sections[{index}] carries both y and Y; dropped y"
                )),
                _ => {}
            }
            for kind in ["block_states", "biomes"] {
                let malformed = section_map.contains_key(kind)
                    && !matches!(section_map.get(kind), Some(DataValue::Map(_)));
                match section_map.get_mut(kind).and_then(DataValue::as_map_mut) {
                    Some(container) => fix_section_palette(index, kind, container, report),
                    None if malformed => report
                        .warn(format!("sections[{index}].{kind} is not a compound; left untouched")),
                    None => {}
                }
            }
        }
    });
}

/// Enchantment-bearing components stored as `{levels: {ench: lvl}}` in 26.3
/// and as a flat `{ench: lvl}` map in 1.21.1.
const ENCHANTMENT_COMPONENTS: &[&str] =
    &["minecraft:enchantments", "minecraft:stored_enchantments"];

/// A map shaped like an item-stack record: `id` plus `count` and/or a
/// `components` compound. Entity records (`id` + `Pos`) and bare block
/// states are deliberately excluded.
fn is_item_stack_map(map: &BTreeMap<String, DataValue>) -> bool {
    matches!(map.get("id"), Some(DataValue::Str(_)))
        && (map.contains_key("count") || matches!(map.get("components"), Some(DataValue::Map(_))))
}

/// Wrap a flat enchantment map into the 26.3 `{levels: ...}` form. A map
/// already carrying a `levels` compound is left alone (idempotent), and any
/// shape that is neither flat levels nor the wrapped form is reported and
/// preserved.
fn wrap_enchantment_component(key: &str, value: &mut DataValue, report: &mut DataFixReport) {
    let Some(map) = value.as_map_mut() else {
        report.warn(format!("component {key} is not a compound; left untouched"));
        return;
    };
    if matches!(map.get("levels"), Some(DataValue::Map(_))) {
        return;
    }
    if !map.values().all(|v| matches!(v, DataValue::Int(_))) {
        report.warn(format!(
            "component {key} is neither flat levels nor a levels compound; left untouched"
        ));
        return;
    }
    let flat = std::mem::take(map);
    map.insert("levels".to_string(), DataValue::Map(flat));
}

/// Downgrade mirror of [`wrap_enchantment_component`]: unwrap a pure
/// `{levels: ...}` map back to the flat 1.21.1 spelling.
fn unwrap_enchantment_component(key: &str, value: &mut DataValue, report: &mut DataFixReport) {
    let Some(map) = value.as_map_mut() else {
        return;
    };
    if !matches!(map.get("levels"), Some(DataValue::Map(_))) {
        return; // already flat — or no levels at all; nothing to unwrap
    }
    if map.len() != 1 {
        report.warn(format!(
            "component {key} has keys beside levels; left in 26.3 form"
        ));
        return;
    }
    if let Some(DataValue::Map(levels)) = map.remove("levels") {
        *map = levels;
    }
}

/// Item-stack migration: rename the stack `id` through [`RENAMES`] and wrap
/// flat enchantment components into the 26.3 `levels` form. Unknown and
/// modded component keys (`create:*`, `aether:*`, ...) are preserved by
/// construction — only the curated keys are rewritten. Nested items inside
/// components (containers, bundles) are visited by the same tree walk.
fn fix_item_components(root: &mut DataValue, report: &mut DataFixReport) {
    walk_mut(root, &mut |value| {
        let Some(map) = value.as_map_mut() else { return };
        if !is_item_stack_map(map) {
            return;
        }
        if let Some(DataValue::Str(id)) = map.get_mut("id") {
            if let Some(to) = lookup_rename(id) {
                *id = to.to_string();
                report.renamed_entries += 1;
            }
        }
        let Some(components) = map.get_mut("components").and_then(DataValue::as_map_mut) else {
            return;
        };
        for key in ENCHANTMENT_COMPONENTS {
            if let Some(component) = components.get_mut(*key) {
                wrap_enchantment_component(key, component, report);
            }
        }
    });
}

/// Downgrade mirror of [`fix_item_components`]: reverse the stack `id` rename
/// and unwrap `levels` compounds back to flat enchantment maps. Modded
/// component keys are untouched here too.
fn downgrade_item_components(root: &mut DataValue, report: &mut DataFixReport) {
    walk_mut(root, &mut |value| {
        let Some(map) = value.as_map_mut() else { return };
        if !is_item_stack_map(map) {
            return;
        }
        if let Some(DataValue::Str(id)) = map.get_mut("id") {
            if let Some(from) = reverse_lookup_rename(id) {
                *id = from.to_string();
                report.renamed_entries += 1;
            }
        }
        let Some(components) = map.get_mut("components").and_then(DataValue::as_map_mut) else {
            return;
        };
        for key in ENCHANTMENT_COMPONENTS {
            if let Some(component) = components.get_mut(*key) {
                unwrap_enchantment_component(key, component, report);
            }
        }
    });
}

/// Reverse of [`lookup_rename`] for the downgrade pipeline. [`RENAMES`] is a
/// bijection, so the table is simply read right-to-left.
fn reverse_lookup_rename(id: &str) -> Option<&'static str> {
    RENAMES
        .iter()
        .find(|(_, to)| *to == id)
        .map(|(from, _)| *from)
}

/// Apply the reversed [`RENAMES`] table to every string value in the tree.
fn fix_resource_renames_reverse(root: &mut DataValue, report: &mut DataFixReport) {
    walk_mut(root, &mut |value| {
        let DataValue::Str(id) = value else { return };
        if let Some(from) = reverse_lookup_rename(id) {
            *id = from.to_string();
            report.renamed_entries += 1;
        }
    });
}

/// `min_format`/`max_format` `[major, minor]` tuples -> the scalar
/// `pack_format` 1.21.1 understands. A `min != max` pair restores from
/// `min_format` with a warning; a non-zero minor (unrepresentable in the
/// scalar) warns and is dropped.
fn restore_pack_format(root: &mut DataValue, report: &mut DataFixReport) {
    fn as_tuple(value: &DataValue) -> Option<[i64; 2]> {
        match value {
            DataValue::List(v) if v.len() == 2 => {
                Some([v[0].as_int()?, v[1].as_int()?])
            }
            _ => None,
        }
    }
    walk_mut(root, &mut |value| {
        let Some(map) = value.as_map_mut() else { return };
        let mut bounds: Vec<[i64; 2]> = Vec::new();
        for key in ["min_format", "max_format"] {
            match map.get(key) {
                None => {}
                Some(value) => match as_tuple(value) {
                    Some(tuple) => bounds.push(tuple),
                    None => report
                        .warn(format!("{key} is not a [major, minor] pair; ignoring it")),
                },
            }
        }
        if bounds.is_empty() {
            return;
        }
        bounds.dedup();
        let [major, minor] = bounds[0];
        if bounds.len() > 1 {
            report.warn(
                "min_format and max_format disagree; restored pack_format from min_format"
                    .to_string(),
            );
        }
        if minor != 0 {
            report.warn(format!(
                "26.3 minor pack format {minor} has no 1.21.1 equivalent; dropped"
            ));
        }
        map.remove("min_format");
        map.remove("max_format");
        map.insert(
            "pack_format".to_string(),
            DataValue::Int(major - PACK_FORMAT_REBASE),
        );
    });
}

/// Resource locations that exist in 26.3 but have no 1.21.1 equivalent. The
/// downgrade leaves them in place — deleting mod or future content would
/// corrupt the document — and reports each occurrence instead.
const ONLY_IN_26_3: &[&str] = &[
    "minecraft:dried_ghast",
    "minecraft:ghastling",
    "minecraft:happy_ghast",
    "minecraft:music_disc_tears",
];

/// Warn on every string that names a 26.3-only entry. Purely observational:
/// it never mutates the document, so it is never listed in `applied_steps`.
fn report_26_3_only_entries(root: &mut DataValue, report: &mut DataFixReport) {
    walk_mut(root, &mut |value| {
        let DataValue::Str(id) = value else { return };
        if ONLY_IN_26_3.contains(&id.as_str()) {
            report.warn(format!(
                "{id} only exists in 26.3 and has no 1.21.1 equivalent; left in place"
            ));
        }
    });
}

/// The stamp step for the explicit 26.3 -> 1.21.1 downgrade pipeline.
fn stamp_1_21_1(root: &mut DataValue, _report: &mut DataFixReport) {
    if let Some(map) = root.as_map_mut() {
        map.insert("DataVersion".to_string(), DataValue::Int(DV_1_21_1 as i64));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(entries: &[(&str, DataValue)]) -> DataValue {
        DataValue::Map(
            entries
                .iter()
                .map(|(k, v)| (k.to_string(), v.clone()))
                .collect(),
        )
    }

    fn str_list(values: &[&str]) -> DataValue {
        DataValue::List(values.iter().map(|s| DataValue::Str(s.to_string())).collect())
    }

    /// A representative 1.21.1-era document exercising every step.
    fn doc_3955() -> DataValue {
        map(&[
            ("DataVersion", DataValue::Int(DV_1_21_1 as i64)),
            (
                "palette",
                DataValue::List(vec![
                    map(&[("id", DataValue::Str("minecraft:stone".into()))]),
                    map(&[
                        ("id", DataValue::Str("create:brass_casing".into())),
                        (
                            "properties",
                            map(&[("axis", DataValue::Str("y".into()))]),
                        ),
                    ]),
                    map(&[("Name", DataValue::Str("minecraft:snow".into()))]),
                ]),
            ),
            (
                "pack",
                map(&[
                    ("pack_format", DataValue::Int(48)),
                    ("description", DataValue::Str("world pack".into())),
                ]),
            ),
            (
                "entities",
                DataValue::List(vec![map(&[
                    ("id", DataValue::Str("minecraft:mooshroom".into())),
                    ("Pos", str_list(&[])),
                ])]),
            ),
            // An item record: `id` + `count` must NOT become a block state.
            (
                "held",
                map(&[
                    ("id", DataValue::Str("minecraft:stone".into())),
                    ("count", DataValue::Int(3)),
                ]),
            ),
        ])
    }

    #[test]
    fn full_3955_to_5023_migration() {
        let fixer = DataFixer::v1_21_1_to_26_3();
        assert_eq!(fixer.source_dv(), DV_1_21_1);
        assert_eq!(fixer.target_dv(), DV_26_3);
        let mut doc = doc_3955();
        let report = fixer.fix(&mut doc).unwrap();

        assert_eq!(report.original_dv, 3955);
        assert_eq!(report.final_dv, 5023);
        assert_eq!(
            report.applied_steps,
            vec![
                "block_state_key_normalize",
                "pack_format_min_max",
                "resource_location_renames",
                "stamp_data_version_26_3",
            ]
        );
        assert_eq!(report.renamed_entries, 2); // snow -> snow_layer, mooshroom -> mushroom_cow
        assert!(report.warnings.is_empty());

        let root = doc.as_map().unwrap();
        assert_eq!(root["DataVersion"], DataValue::Int(5023));

        // id/properties normalized to Name/Properties, modded id preserved.
        let palette = match &root["palette"] {
            DataValue::List(v) => v,
            other => panic!("palette: {other:?}"),
        };
        assert_eq!(
            palette[0],
            map(&[("Name", DataValue::Str("minecraft:stone".into()))])
        );
        assert_eq!(
            palette[1],
            map(&[
                ("Name", DataValue::Str("create:brass_casing".into())),
                ("Properties", map(&[("axis", DataValue::Str("y".into()))])),
            ])
        );
        // Already-canonical spelling kept; the rename still applied.
        assert_eq!(
            palette[2],
            map(&[("Name", DataValue::Str("minecraft:snow_layer".into()))])
        );

        // pack_format 48 -> [121, 0] on both bounds.
        let pack = root["pack"].as_map().unwrap();
        let tuple = DataValue::List(vec![DataValue::Int(121), DataValue::Int(0)]);
        assert_eq!(pack["min_format"], tuple);
        assert_eq!(pack["max_format"], tuple);
        assert!(!pack.contains_key("pack_format"));

        // Entity rename applied under a non-block-state `id`... but the
        // record kept `id` (it has `Pos`, not a pure block-state map).
        let entity = match &root["entities"] {
            DataValue::List(v) => v[0].as_map().unwrap().clone(),
            other => panic!("entities: {other:?}"),
        };
        assert_eq!(
            entity["id"],
            DataValue::Str("minecraft:mushroom_cow".into())
        );

        // Item record untouched: `id` + `count` is not a block-state map.
        let held = root["held"].as_map().unwrap();
        assert_eq!(held["id"], DataValue::Str("minecraft:stone".into()));
        assert_eq!(held["count"], DataValue::Int(3));
    }

    #[test]
    fn second_run_is_a_clean_noop() {
        let fixer = DataFixer::v1_21_1_to_26_3();
        let mut doc = doc_3955();
        fixer.fix(&mut doc).unwrap();
        let migrated = doc.clone();

        let report = fixer.fix(&mut doc).unwrap();
        assert_eq!(report.original_dv, 5023);
        assert_eq!(report.final_dv, 5023);
        assert!(report.applied_steps.is_empty());
        assert_eq!(report.renamed_entries, 0);
        assert!(report.warnings.is_empty());
        assert_eq!(doc, migrated);
    }

    #[test]
    fn newer_document_is_refused_unchanged() {
        let fixer = DataFixer::v1_21_1_to_26_3();
        let mut doc = map(&[("DataVersion", DataValue::Int(6000))]);
        let before = doc.clone();
        assert_eq!(
            fixer.fix(&mut doc),
            Err(DataFixError::NewerThanTarget {
                file_dv: 6000,
                target_dv: 5023
            })
        );
        assert_eq!(doc, before);
    }

    #[test]
    fn missing_and_malformed_data_version_fail_closed() {
        let fixer = DataFixer::v1_21_1_to_26_3();
        let mut no_dv = map(&[("region", DataValue::Null)]);
        assert_eq!(fixer.fix(&mut no_dv), Err(DataFixError::MissingDataVersion));
        let mut bad_dv = map(&[("DataVersion", DataValue::Str("new".into()))]);
        assert_eq!(
            fixer.fix(&mut bad_dv),
            Err(DataFixError::MalformedDataVersion)
        );
        let mut not_map = DataValue::List(vec![]);
        assert_eq!(fixer.fix(&mut not_map), Err(DataFixError::RootNotMap));
    }

    #[test]
    fn uncovered_intermediate_version_warns_and_stays_put() {
        let fixer = DataFixer::v1_21_1_to_26_3();
        let mut doc = map(&[
            ("DataVersion", DataValue::Int(4500)),
            ("pack", map(&[("pack_format", DataValue::Int(48))])),
        ]);
        let before = doc.clone();
        let report = fixer.fix(&mut doc).unwrap();
        assert!(report.applied_steps.is_empty());
        assert_eq!(report.final_dv, 4500);
        assert_eq!(report.warnings.len(), 1);
        assert_eq!(doc, before);
    }

    #[test]
    fn modded_namespaces_pass_through_the_rename_table() {
        let mut root = map(&[
            ("DataVersion", DataValue::Int(DV_1_21_1 as i64)),
            ("mod_block", DataValue::Str("create:brass_casing".into())),
            (
                "mod_ids",
                str_list(&["create:shaft", "aeronautics:hull", "minecraft:snow"]),
            ),
        ]);
        let report = DataFixer::v1_21_1_to_26_3().fix(&mut root).unwrap();
        assert_eq!(report.renamed_entries, 1); // only the vanilla rename
        let root = root.as_map().unwrap();
        assert_eq!(
            root["mod_block"],
            DataValue::Str("create:brass_casing".into())
        );
        assert_eq!(
            root["mod_ids"],
            str_list(&["create:shaft", "aeronautics:hull", "minecraft:snow_layer"])
        );
    }

    /// A `level.dat`-shaped 1.21.1 document: root `DataVersion` plus the
    /// duplicate copy inside `Data`.
    fn level_dat_3955() -> DataValue {
        map(&[
            ("DataVersion", DataValue::Int(DV_1_21_1 as i64)),
            (
                "Data",
                map(&[
                    ("DataVersion", DataValue::Int(DV_1_21_1 as i64)),
                    (
                        "Version",
                        map(&[
                            ("Id", DataValue::Int(DV_1_21_1 as i64)),
                            ("Name", DataValue::Str("1.21.1".into())),
                            ("Snapshot", DataValue::Bool(false)),
                        ]),
                    ),
                    ("RandomSeed", DataValue::Int(-42)),
                ]),
            ),
        ])
    }

    #[test]
    fn level_dat_metadata_migrates_inside_the_data_compound() {
        let mut doc = level_dat_3955();
        let report = DataFixer::v1_21_1_to_26_3().fix(&mut doc).unwrap();
        assert_eq!(report.final_dv, DV_26_3);
        assert!(report.warnings.is_empty());
        assert_eq!(
            report.applied_steps,
            vec![
                "level_dat_version_metadata",
                "stamp_data_version_26_3"
            ]
        );
        let data = doc.as_map().unwrap()["Data"].as_map().unwrap();
        assert_eq!(data["DataVersion"], DataValue::Int(DV_26_3 as i64));
        assert_eq!(
            data["Version"],
            map(&[
                ("Id", DataValue::Int(DV_26_3 as i64)),
                ("Name", DataValue::Str("26.3".into())),
                ("Snapshot", DataValue::Bool(false)),
            ])
        );
        // Unrelated level.dat content is preserved exactly.
        assert_eq!(data["RandomSeed"], DataValue::Int(-42));
    }

    /// A `Data` compound without version metadata is not a level.dat and
    /// must not gain one.
    #[test]
    fn unrelated_data_compounds_are_not_stamped() {
        let mut doc = map(&[
            ("DataVersion", DataValue::Int(DV_1_21_1 as i64)),
            ("Data", map(&[("Custom", DataValue::Int(1))])),
        ]);
        let report = DataFixer::v1_21_1_to_26_3().fix(&mut doc).unwrap();
        assert_eq!(
            report.applied_steps,
            vec!["stamp_data_version_26_3"]
        );
        assert_eq!(
            doc.as_map().unwrap()["Data"],
            map(&[("Custom", DataValue::Int(1))])
        );
    }

    #[test]
    fn chunk_sections_normalize_layout_and_migrate_palettes() {
        let mut doc = map(&[
            ("DataVersion", DataValue::Int(DV_1_21_1 as i64)),
            (
                "sections",
                DataValue::List(vec![
                    map(&[
                        ("y", DataValue::Int(0)),
                        (
                            "block_states",
                            map(&[(
                                "palette",
                                DataValue::List(vec![
                                    map(&[
                                        ("id", DataValue::Str("minecraft:grass".into())),
                                        ("properties", map(&[("snowy", DataValue::Str("false".into()))])),
                                    ]),
                                    map(&[("Name", DataValue::Str("minecraft:snow".into()))]),
                                    map(&[("id", DataValue::Str("create:brass_casing".into()))]),
                                    DataValue::Int(7), // not a compound: warn, keep
                                ]),
                            )]),
                        ),
                        (
                            "biomes",
                            map(&[(
                                "palette",
                                DataValue::List(vec![
                                    DataValue::Str("minecraft:plains".into()),
                                    DataValue::Int(3), // not a biome id: warn, keep
                                ]),
                            )]),
                        ),
                    ]),
                    // A `y`/`Y` collision keeps `Y` and warns.
                    map(&[
                        ("y", DataValue::Int(4)),
                        ("Y", DataValue::Int(5)),
                    ]),
                ]),
            ),
        ]);
        let report = DataFixer::v1_21_1_to_26_3().fix(&mut doc).unwrap();
        assert_eq!(report.renamed_entries, 2); // grass + snow inside the palette
        assert_eq!(report.warnings.len(), 3); // bad block entry, bad biome, y/Y clash
        assert_eq!(
            report.applied_steps,
            vec![
                // The global key pass normalizes {id, properties} entries
                // first; the section step then does y->Y and the renames.
                "block_state_key_normalize",
                "chunk_section_layout",
                "stamp_data_version_26_3",
            ]
        );

        let sections = match &doc.as_map().unwrap()["sections"] {
            DataValue::List(v) => v,
            other => panic!("sections: {other:?}"),
        };
        let section0 = sections[0].as_map().unwrap();
        assert!(!section0.contains_key("y"));
        assert_eq!(section0["Y"], DataValue::Int(0));
        let palette = match &section0["block_states"].as_map().unwrap()["palette"] {
            DataValue::List(v) => v,
            other => panic!("palette: {other:?}"),
        };
        assert_eq!(
            palette[0],
            map(&[
                ("Name", DataValue::Str("minecraft:short_grass".into())),
                ("Properties", map(&[("snowy", DataValue::Str("false".into()))])),
            ])
        );
        assert_eq!(
            palette[1],
            map(&[("Name", DataValue::Str("minecraft:snow_layer".into()))])
        );
        // Modded palette entries normalize keys but keep their namespace.
        assert_eq!(
            palette[2],
            map(&[("Name", DataValue::Str("create:brass_casing".into()))])
        );
        assert_eq!(palette[3], DataValue::Int(7));
        assert_eq!(
            section0["biomes"].as_map().unwrap()["palette"],
            DataValue::List(vec![
                DataValue::Str("minecraft:plains".into()),
                DataValue::Int(3),
            ])
        );
        let section1 = sections[1].as_map().unwrap();
        assert_eq!(section1["Y"], DataValue::Int(5));
        assert!(!section1.contains_key("y"));
    }

    #[test]
    fn item_components_wrap_enchantments_rename_ids_and_keep_modded() {
        let mut doc = map(&[
            ("DataVersion", DataValue::Int(DV_1_21_1 as i64)),
            (
                "inventory",
                DataValue::List(vec![
                    map(&[
                        ("id", DataValue::Str("minecraft:boat".into())),
                        ("count", DataValue::Int(1)),
                        (
                            "components",
                            map(&[
                                (
                                    "minecraft:enchantments",
                                    map(&[
                                        ("minecraft:sharpness", DataValue::Int(5)),
                                        ("minecraft:unbreaking", DataValue::Int(3)),
                                    ]),
                                ),
                                ("create:schematic", map(&[("progress", DataValue::Int(4))])),
                                ("aether:cloud_buff", DataValue::Str("float".into())),
                            ]),
                        ),
                    ]),
                    map(&[
                        ("id", DataValue::Str("minecraft:chest_boat".into())),
                        ("count", DataValue::Int(2)),
                        (
                            "components",
                            map(&[(
                                "minecraft:enchantments",
                                map(&[(
                                    "levels",
                                    map(&[("minecraft:looting", DataValue::Int(2))]),
                                )]),
                            )]),
                        ),
                    ]),
                    // A nested stack inside a container component is found
                    // by the same walk.
                    map(&[
                        ("id", DataValue::Str("minecraft:shulker_box".into())),
                        ("count", DataValue::Int(1)),
                        (
                            "components",
                            map(&[(
                                "minecraft:container",
                                DataValue::List(vec![map(&[
                                    ("id", DataValue::Str("minecraft:snow".into())),
                                    ("count", DataValue::Int(8)),
                                ])]),
                            )]),
                        ),
                    ]),
                ]),
            ),
        ]);
        let report = DataFixer::v1_21_1_to_26_3().fix(&mut doc).unwrap();
        assert!(report.warnings.is_empty());
        assert_eq!(report.renamed_entries, 3); // boat, chest_boat, nested snow
        assert_eq!(
            report.applied_steps,
            vec!["item_components_upgrade", "stamp_data_version_26_3"]
        );

        let inventory = match &doc.as_map().unwrap()["inventory"] {
            DataValue::List(v) => v,
            other => panic!("inventory: {other:?}"),
        };
        let first = inventory[0].as_map().unwrap();
        assert_eq!(first["id"], DataValue::Str("minecraft:oak_boat".into()));
        assert_eq!(
            first["components"].as_map().unwrap()["minecraft:enchantments"],
            map(&[(
                "levels",
                map(&[
                    ("minecraft:sharpness", DataValue::Int(5)),
                    ("minecraft:unbreaking", DataValue::Int(3)),
                ]),
            )])
        );
        // Modded component keys pass through untouched, map or scalar.
        assert_eq!(
            first["components"].as_map().unwrap()["create:schematic"],
            map(&[("progress", DataValue::Int(4))])
        );
        assert_eq!(
            first["components"].as_map().unwrap()["aether:cloud_buff"],
            DataValue::Str("float".into())
        );
        // Already-wrapped components are not double-wrapped (idempotent).
        let second = inventory[1].as_map().unwrap();
        assert_eq!(second["id"], DataValue::Str("minecraft:oak_chest_boat".into()));
        assert_eq!(
            second["components"].as_map().unwrap()["minecraft:enchantments"],
            map(&[(
                "levels",
                map(&[("minecraft:looting", DataValue::Int(2))]),
            )])
        );
        let nested = &inventory[2].as_map().unwrap()["components"].as_map().unwrap()
            ["minecraft:container"];
        assert_eq!(
            nested,
            &DataValue::List(vec![map(&[
                ("id", DataValue::Str("minecraft:snow_layer".into())),
                ("count", DataValue::Int(8)),
            ])])
        );
    }

    /// Canonical 1.21.1 document mixing every migrated structure. Canonical
    /// inputs round-trip bit-exactly through the downgrade fixer.
    fn rich_doc_3955() -> DataValue {
        map(&[
            ("DataVersion", DataValue::Int(DV_1_21_1 as i64)),
            (
                "Data",
                map(&[
                    ("DataVersion", DataValue::Int(DV_1_21_1 as i64)),
                    (
                        "Version",
                        map(&[
                            ("Id", DataValue::Int(DV_1_21_1 as i64)),
                            ("Name", DataValue::Str("1.21.1".into())),
                            ("Snapshot", DataValue::Bool(false)),
                        ]),
                    ),
                ]),
            ),
            (
                "sections",
                DataValue::List(vec![map(&[
                    ("Y", DataValue::Int(0)),
                    (
                        "block_states",
                        map(&[(
                            "palette",
                            DataValue::List(vec![
                                map(&[("Name", DataValue::Str("minecraft:snow".into()))]),
                                map(&[("Name", DataValue::Str("create:brass_casing".into()))]),
                            ]),
                        )]),
                    ),
                    (
                        "biomes",
                        map(&[(
                            "palette",
                            str_list(&["minecraft:plains", "aether:skyroot_meadow"]),
                        )]),
                    ),
                ])]),
            ),
            (
                "inventory",
                DataValue::List(vec![map(&[
                    ("id", DataValue::Str("minecraft:boat".into())),
                    ("count", DataValue::Int(1)),
                    (
                        "components",
                        map(&[
                            (
                                "minecraft:enchantments",
                                map(&[("minecraft:sharpness", DataValue::Int(5))]),
                            ),
                            ("create:schematic", map(&[("progress", DataValue::Int(4))])),
                        ]),
                    ),
                ])]),
            ),
            (
                "pack",
                map(&[
                    ("pack_format", DataValue::Int(48)),
                    ("description", DataValue::Str("world pack".into())),
                ]),
            ),
            (
                "entities",
                DataValue::List(vec![map(&[
                    ("id", DataValue::Str("minecraft:mooshroom".into())),
                    ("Pos", str_list(&[])),
                ])]),
            ),
        ])
    }

    #[test]
    fn rich_document_migrates_then_second_run_is_a_clean_noop() {
        let fixer = DataFixer::v1_21_1_to_26_3();
        let mut doc = rich_doc_3955();
        let report = fixer.fix(&mut doc).unwrap();
        assert_eq!(report.final_dv, DV_26_3);
        assert!(report.warnings.is_empty());
        assert_eq!(report.renamed_entries, 3); // snow, boat, mooshroom
        assert_eq!(
            report.applied_steps,
            vec![
                "level_dat_version_metadata",
                "chunk_section_layout",
                "item_components_upgrade",
                "pack_format_min_max",
                "resource_location_renames",
                "stamp_data_version_26_3",
            ]
        );
        let migrated = doc.clone();
        let report = fixer.fix(&mut doc).unwrap();
        assert!(report.applied_steps.is_empty());
        assert_eq!(report.renamed_entries, 0);
        assert!(report.warnings.is_empty());
        assert_eq!(doc, migrated);
    }

    #[test]
    fn downgrade_roundtrip_is_bit_exact_on_canonical_input() {
        let up = DataFixer::v1_21_1_to_26_3();
        let down = DataFixer::v26_3_to_1_21_1_downgrade();
        assert_eq!(down.source_dv(), DV_26_3);
        assert_eq!(down.target_dv(), DV_1_21_1);

        let original = rich_doc_3955();
        let mut doc = original.clone();
        up.fix(&mut doc).unwrap();
        assert_ne!(doc, original); // the upgrade really did something

        let report = down.fix(&mut doc).unwrap();
        assert_eq!(report.original_dv, DV_26_3);
        assert_eq!(report.final_dv, DV_1_21_1);
        assert_eq!(report.renamed_entries, 3); // snow_layer, oak_boat, mushroom_cow
        assert!(report.warnings.is_empty());
        assert_eq!(doc, original);
    }

    #[test]
    fn downgrade_reports_and_preserves_26_3_only_entries() {
        let down = DataFixer::v26_3_to_1_21_1_downgrade();
        let mut doc = map(&[
            ("DataVersion", DataValue::Int(DV_26_3 as i64)),
            (
                "entities",
                DataValue::List(vec![
                    map(&[("id", DataValue::Str("minecraft:happy_ghast".into()))]),
                    map(&[
                        ("id", DataValue::Str("minecraft:mushroom_cow".into())),
                        ("Pos", str_list(&[])),
                    ]),
                ]),
            ),
            (
                "inventory",
                DataValue::List(vec![map(&[
                    ("id", DataValue::Str("minecraft:music_disc_tears".into())),
                    ("count", DataValue::Int(1)),
                ])]),
            ),
        ]);
        let report = down.fix(&mut doc).unwrap();
        assert_eq!(report.final_dv, DV_1_21_1);
        assert_eq!(report.renamed_entries, 1); // mushroom_cow -> mooshroom
        assert_eq!(report.warnings.len(), 2);
        assert!(report.warnings.iter().any(|w| w.contains("happy_ghast")));
        assert!(report
            .warnings
            .iter()
            .any(|w| w.contains("music_disc_tears")));
        // 26.3-only values are reported, never deleted.
        let entities = match &doc.as_map().unwrap()["entities"] {
            DataValue::List(v) => v,
            other => panic!("entities: {other:?}"),
        };
        assert_eq!(
            entities[0].as_map().unwrap()["id"],
            DataValue::Str("minecraft:happy_ghast".into())
        );
        assert_eq!(
            entities[1].as_map().unwrap()["id"],
            DataValue::Str("minecraft:mooshroom".into())
        );
    }

    #[test]
    fn downgrade_refuses_newer_documents_and_noops_at_target() {
        let down = DataFixer::v26_3_to_1_21_1_downgrade();
        let mut doc = map(&[("DataVersion", DataValue::Int(6000))]);
        let before = doc.clone();
        assert_eq!(
            down.fix(&mut doc),
            Err(DataFixError::NewerThanTarget {
                file_dv: 6000,
                target_dv: DV_26_3,
            })
        );
        assert_eq!(doc, before);

        let mut doc = map(&[
            ("DataVersion", DataValue::Int(DV_1_21_1 as i64)),
            ("mod_block", DataValue::Str("create:shaft".into())),
        ]);
        let before = doc.clone();
        let report = down.fix(&mut doc).unwrap();
        assert_eq!(report.final_dv, DV_1_21_1);
        assert!(report.applied_steps.is_empty());
        assert!(report.warnings.is_empty());
        assert_eq!(doc, before);

        // Between target and source no step applies: warn, leave untouched.
        let mut doc = map(&[("DataVersion", DataValue::Int(4500))]);
        let before = doc.clone();
        let report = down.fix(&mut doc).unwrap();
        assert_eq!(report.warnings.len(), 1);
        assert_eq!(doc, before);
    }

    #[test]
    fn downgrade_restores_scalar_pack_format() {
        let down = DataFixer::v26_3_to_1_21_1_downgrade();
        let mut doc = map(&[
            ("DataVersion", DataValue::Int(DV_26_3 as i64)),
            (
                "pack",
                map(&[
                    ("min_format", DataValue::List(vec![DataValue::Int(121), DataValue::Int(0)])),
                    ("max_format", DataValue::List(vec![DataValue::Int(121), DataValue::Int(0)])),
                    ("description", DataValue::Str("world pack".into())),
                ]),
            ),
            // Disagreeing bounds restore from min_format with a warning.
            (
                "split_pack",
                map(&[
                    ("min_format", DataValue::List(vec![DataValue::Int(121), DataValue::Int(0)])),
                    ("max_format", DataValue::List(vec![DataValue::Int(122), DataValue::Int(0)])),
                ]),
            ),
            // A non-zero minor is unrepresentable in 1.21.1: warn, keep major.
            (
                "minor_pack",
                map(&[
                    ("min_format", DataValue::List(vec![DataValue::Int(121), DataValue::Int(1)])),
                    ("max_format", DataValue::List(vec![DataValue::Int(121), DataValue::Int(1)])),
                ]),
            ),
        ]);
        let report = down.fix(&mut doc).unwrap();
        assert_eq!(report.final_dv, DV_1_21_1);
        assert_eq!(report.warnings.len(), 2);
        let root = doc.as_map().unwrap();
        assert_eq!(
            root["pack"],
            map(&[
                ("pack_format", DataValue::Int(48)),
                ("description", DataValue::Str("world pack".into())),
            ])
        );
        assert_eq!(
            root["split_pack"],
            map(&[("pack_format", DataValue::Int(48))])
        );
        assert_eq!(
            root["minor_pack"],
            map(&[("pack_format", DataValue::Int(48))])
        );
    }

    #[test]
    fn downgrade_unwraps_levels_and_reverses_item_ids() {
        let down = DataFixer::v26_3_to_1_21_1_downgrade();
        let mut doc = map(&[
            ("DataVersion", DataValue::Int(DV_26_3 as i64)),
            (
                "inventory",
                DataValue::List(vec![
                    map(&[
                        ("id", DataValue::Str("minecraft:oak_boat".into())),
                        ("count", DataValue::Int(1)),
                        (
                            "components",
                            map(&[
                                (
                                    "minecraft:enchantments",
                                    map(&[(
                                        "levels",
                                        map(&[("minecraft:sharpness", DataValue::Int(5))]),
                                    )]),
                                ),
                                ("create:schematic", map(&[("progress", DataValue::Int(4))])),
                            ]),
                        ),
                    ]),
                    // A component carrying keys beside `levels` warns and is
                    // left in its 26.3 form.
                    map(&[
                        ("id", DataValue::Str("minecraft:stone".into())),
                        ("count", DataValue::Int(1)),
                        (
                            "components",
                            map(&[(
                                "minecraft:enchantments",
                                map(&[
                                    ("levels", map(&[("minecraft:looting", DataValue::Int(2))])),
                                    ("show_in_tooltip", DataValue::Bool(false)),
                                ]),
                            )]),
                        ),
                    ]),
                ]),
            ),
        ]);
        let report = down.fix(&mut doc).unwrap();
        assert_eq!(report.final_dv, DV_1_21_1);
        assert_eq!(report.renamed_entries, 1); // oak_boat -> boat
        assert_eq!(report.warnings.len(), 1);
        let inventory = match &doc.as_map().unwrap()["inventory"] {
            DataValue::List(v) => v,
            other => panic!("inventory: {other:?}"),
        };
        let first = inventory[0].as_map().unwrap();
        assert_eq!(first["id"], DataValue::Str("minecraft:boat".into()));
        assert_eq!(
            first["components"].as_map().unwrap()["minecraft:enchantments"],
            map(&[("minecraft:sharpness", DataValue::Int(5))])
        );
        assert_eq!(
            first["components"].as_map().unwrap()["create:schematic"],
            map(&[("progress", DataValue::Int(4))])
        );
        let second = inventory[1].as_map().unwrap();
        assert_eq!(
            second["components"].as_map().unwrap()["minecraft:enchantments"],
            map(&[
                ("levels", map(&[("minecraft:looting", DataValue::Int(2))])),
                ("show_in_tooltip", DataValue::Bool(false)),
            ])
        );
    }
}
