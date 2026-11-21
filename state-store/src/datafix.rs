// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
//! Declarative `DataFixer` for the `1.21.1` (DataVersion 3955) -> `26.3`
//! (DataVersion 5023) bridge.
//!
//! A fixer is an ordered list of [`DataFixStep`]s over the format-agnostic
//! [`DataValue`] tree, so fixes are pure functions — no NBT/JSON coupling, no
//! I/O, deterministic and testable. A step runs when the document's
//! DataVersion is at or before the step's `from_dv`; running the fixer on a
//! document already at the target DataVersion is therefore a clean no-op, and
//! a document *newer* than the target is refused outright
//! ([`DataFixError::NewerThanTarget`]) instead of being silently downgraded.

use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;

/// DataVersion shipped by Minecraft 1.21.1.
pub const DV_1_21_1: i32 = 3955;
/// DataVersion shipped by Minecraft 26.3.
pub const DV_26_3: i32 = 5023;

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
    /// Names of the steps that ran, in pipeline order.
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

/// One named migration unit. `apply` runs when `file_dv <= from_dv` and must
/// leave the document unchanged when its input shape is already the output
/// shape (every step is internally idempotent).
pub struct DataFixStep {
    pub name: &'static str,
    /// DataVersion the step upgrades *from*: the step runs on documents at or
    /// below this version.
    pub from_dv: i32,
    /// DataVersion the step upgrades *to*.
    pub to_dv: i32,
    pub apply: fn(&mut DataValue, &mut DataFixReport),
}

/// An ordered [`DataFixStep`] pipeline from `source_dv` to `target_dv`.
pub struct DataFixer {
    source_dv: i32,
    target_dv: i32,
    steps: Vec<DataFixStep>,
}

impl DataFixer {
    pub fn new(source_dv: i32, target_dv: i32, mut steps: Vec<DataFixStep>) -> Self {
        steps.sort_by_key(|s| s.from_dv);
        Self {
            source_dv,
            target_dv,
            steps,
        }
    }

    /// The `1.21.1` (`3955`) -> `26.3` (`5023`) pipeline: block-state key
    /// normalization, `pack_format` -> `min_format`/`max_format` migration,
    /// the resource-location rename table and the `DataVersion` stamp.
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

    pub fn source_dv(&self) -> i32 {
        self.source_dv
    }

    pub fn target_dv(&self) -> i32 {
        self.target_dv
    }

    pub fn steps(&self) -> &[DataFixStep] {
        &self.steps
    }

    /// Migrate `root` toward `target_dv`. Steps whose `from_dv` is below the
    /// document's DataVersion are skipped — the document already carries that
    /// step's output shape. Documents newer than `target_dv` are refused.
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
        if file_dv > self.target_dv {
            return Err(DataFixError::NewerThanTarget {
                file_dv,
                target_dv: self.target_dv,
            });
        }

        let mut report = DataFixReport {
            original_dv: file_dv,
            final_dv: file_dv,
            ..DataFixReport::default()
        };
        for step in &self.steps {
            if file_dv <= step.from_dv && step.to_dv <= self.target_dv {
                (step.apply)(root, &mut report);
                report.applied_steps.push(step.name.to_string());
            }
        }
        if let Some(dv) = root
            .as_map()
            .and_then(|m| m.get("DataVersion"))
            .and_then(DataValue::as_int)
        {
            report.final_dv = i32::try_from(dv).unwrap_or(i32::MAX);
        }
        if report.final_dv < self.target_dv {
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
        .map_or(true, |p| matches!(p, DataValue::Map(_)));
    id_str && pure_keys && props_ok
}

/// Canonicalize block-state records to `Name`/`Properties`: `{id, properties}`
/// maps are renamed, `{Name, Properties}` maps already carry the output shape.
fn fix_block_state_keys(root: &mut DataValue, _report: &mut DataFixReport) {
    walk_mut(root, &mut |value| {
        let Some(map) = value.as_map_mut() else { return };
        if !is_block_state_map(map) {
            return;
        }
        if let Some(id) = map.remove("id") {
            map.insert("Name".to_string(), id);
        }
        if let Some(properties) = map.remove("properties") {
            map.insert("Properties".to_string(), properties);
        }
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
}
