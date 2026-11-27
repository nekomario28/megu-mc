// SPDX-License-Identifier: LGPL-3.0-or-later

//! Create 1.21.1 block catalog on top of [`mod_adapter_core::blockstates::BlockRegistry`].

use std::collections::BTreeMap;

use mod_adapter_core::data_loader::LoadedModData;

pub use mod_adapter_core::blockstates::{
    BlockDef, BlockRegistry, BlockStateEntry, PropertyDef, RegistryError, DEFAULT_BASE_STATE_ID,
};

/// Functional role of a Create block in the flagship native simulation adapter.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CreateBlockRole {
    Shaft,
    Cogwheel,
    LargeCogwheel,
    Gearbox,
    Clutch,
    Gearshift,
    Generator { driven_rpm: i32 },
    StressConsumer,
    Belt,
    Transport,
    Processor,
    FluidPipe,
    FluidPump,
    FluidTank,
    ContraptionAnchor,
    Sensor,
    /// Inert addon block with no simulation behaviour (decorative blocks,
    /// buoyancy envelopes, control surfaces, ore/chest placeholders).
    Decorative,
}

/// Create-specialized block registry alias backed by the universal multi-property
/// `BlockRegistry` in `mod-adapter-core`.
pub type CreateBlockRegistry = BlockRegistry<CreateBlockRole>;

/// Legacy block ids accepted as aliases of the canonical Create catalog.
/// `create:mechanical_belt` predates real Create 6.x, which registers the
/// belt block as `create:belt`; legacy MCF1/MCF2 sidecars and callers
/// written against the old id keep resolving while `encode_canonical`
/// always emits the canonical id.
pub fn core_create_block_aliases() -> Vec<(String, String)> {
    vec![(
        "create:mechanical_belt".to_string(),
        "create:belt".to_string(),
    )]
}

/// Build the default core Create 1.21.1 block catalog (31 blocks).
pub fn core_create_registry(base_state_id: u32) -> CreateBlockRegistry {
    BlockRegistry::build_with_aliases(
        base_state_id,
        core_create_block_defs(),
        &core_create_block_aliases(),
    )
    .expect("built-in Create catalog must be valid")
}

/// Build the core catalog with every `assets/*/blockstates/*.json` table
/// harvested from a loaded mod JAR ([`LoadedModData::blockstates`]) merged
/// on top: known catalog ids get the authoritative JAR property spelling,
/// unknown ids (the other ~600 Create blocks plus addon blocks) are
/// appended with an inferred role. This is the JAR-loaded counterpart of
/// [`core_create_registry`].
pub fn create_registry_with_mod_data(
    base_state_id: u32,
    data: &LoadedModData,
) -> Result<CreateBlockRegistry, RegistryError> {
    create_registry_with_blockstates(base_state_id, &data.blockstates)
}

/// [`create_registry_with_mod_data`] over a bare blockstate table — also
/// accepts [`crate::data_loader::LoadedCreateData::blockstates`].
pub fn create_registry_with_blockstates(
    base_state_id: u32,
    blockstates: &BTreeMap<String, Vec<PropertyDef>>,
) -> Result<CreateBlockRegistry, RegistryError> {
    let mut defs = core_create_block_defs();
    merge_blockstate_table(&mut defs, blockstates);
    BlockRegistry::build_with_aliases(base_state_id, defs, &core_create_block_aliases())
}

/// Merge JAR-harvested blockstate property tables into `defs`. Blocks
/// already in the catalog keep their role and stress factors but take the
/// JAR's exact property names/values; unknown ids are appended via
/// [`jar_block_profile`]. Returns the number of defs appended.
pub fn merge_blockstate_table(
    defs: &mut Vec<BlockDef<CreateBlockRole>>,
    blockstates: &BTreeMap<String, Vec<PropertyDef>>,
) -> usize {
    let mut added = 0usize;
    for (id, properties) in blockstates {
        if !mod_adapter_core::blockstates::is_valid_resource_location(id) {
            continue;
        }
        if let Some(def) = defs.iter_mut().find(|def| def.id == *id) {
            def.properties = properties.clone();
            continue;
        }
        let (role, load_factor, capacity_factor) = jar_block_profile(id);
        defs.push(BlockDef {
            id: id.clone(),
            properties: properties.clone(),
            load_factor,
            capacity_factor,
            role,
        });
        added += 1;
    }
    added
}

/// Merge every blockstate table in a [`LoadedModData`] JAR scan into `defs`
/// — see [`merge_blockstate_table`].
pub fn merge_blockstates_from_mod_data(
    defs: &mut Vec<BlockDef<CreateBlockRole>>,
    data: &LoadedModData,
) -> usize {
    merge_blockstate_table(defs, &data.blockstates)
}

/// Role and stress factors for a block that exists only in JAR blockstate
/// data. Exact `create:*` ids reuse the verified Create 6.0.x `AllBlocks`
/// constants; other namespaces get structural roles with zero stress so an
/// addon shaft still transmits rotation without inventing balance numbers.
/// Returns `(role, load_factor, capacity_factor)`.
fn jar_block_profile(id: &str) -> (CreateBlockRole, f64, f64) {
    let path = id.rsplit(':').next().unwrap_or(id);
    if let Some(suffix) = id.strip_prefix("create:") {
        match suffix {
            // Verified AllBlocks constants: SU per RPM of stress capacity
            // and the generated speed in RPM.
            "water_wheel" => return (CreateBlockRole::Generator { driven_rpm: 8 }, 0.0, 32.0),
            "large_water_wheel" => {
                return (CreateBlockRole::Generator { driven_rpm: 4 }, 0.0, 128.0)
            }
            "windmill_bearing" => {
                return (CreateBlockRole::Generator { driven_rpm: 16 }, 0.0, 512.0)
            }
            "steam_engine" => return (CreateBlockRole::Generator { driven_rpm: 64 }, 0.0, 1024.0),
            "hand_crank" => return (CreateBlockRole::Generator { driven_rpm: 32 }, 0.0, 8.0),
            "creative_motor" => {
                return (CreateBlockRole::Generator { driven_rpm: 256 }, 0.0, 16384.0)
            }
            "mechanical_press" => return (CreateBlockRole::Processor, 8.0, 0.0),
            "mechanical_mixer" => return (CreateBlockRole::Processor, 4.0, 0.0),
            "millstone" => return (CreateBlockRole::Processor, 4.0, 0.0),
            "mechanical_pump" => return (CreateBlockRole::FluidPump, 4.0, 0.0),
            "mechanical_bearing" => return (CreateBlockRole::ContraptionAnchor, 4.0, 0.0),
            "crushing_wheel" => return (CreateBlockRole::Processor, 8.0, 0.0),
            "weighted_ejector" => return (CreateBlockRole::Processor, 2.0, 0.0),
            "mechanical_crafter" => return (CreateBlockRole::Processor, 2.0, 0.0),
            "mechanical_arm" => return (CreateBlockRole::Processor, 2.0, 0.0),
            "encased_fan" => return (CreateBlockRole::Processor, 2.0, 0.0),
            "deployer" | "mechanical_drill" | "mechanical_saw" => {
                return (CreateBlockRole::Processor, 4.0, 0.0)
            }
            "rope_pulley" | "hose_pulley" | "elevator_pulley" => {
                return (CreateBlockRole::StressConsumer, 4.0, 0.0)
            }
            "turntable" => return (CreateBlockRole::ContraptionAnchor, 4.0, 0.0),
            "belt" => return (CreateBlockRole::Belt, 1.0, 0.0),
            "depot" | "chute" | "smart_chute" | "andesite_funnel" | "brass_funnel"
            | "andesite_belt_funnel" | "brass_belt_funnel" => {
                return (CreateBlockRole::Transport, 0.0, 0.0)
            }
            "fluid_pipe" | "encased_fluid_pipe" => {
                return (CreateBlockRole::FluidPipe, 0.0, 0.0)
            }
            "fluid_tank" => return (CreateBlockRole::FluidTank, 0.0, 0.0),
            "basin" | "spout" => return (CreateBlockRole::Processor, 0.0, 0.0),
            "stressometer" => return (CreateBlockRole::Sensor, 0.0, 0.0),
            _ => {}
        }
        // Remaining `create:*` ids: kinetic passives by name, then inert.
        let role = match path {
            "shaft" | "powered_shaft" | "andesite_encased_shaft" | "brass_encased_shaft"
            | "metal_girder_encased_shaft" => CreateBlockRole::Shaft,
            "cogwheel" | "andesite_encased_cogwheel" | "brass_encased_cogwheel" => {
                CreateBlockRole::Cogwheel
            }
            "large_cogwheel" | "andesite_encased_large_cogwheel"
            | "brass_encased_large_cogwheel" => CreateBlockRole::LargeCogwheel,
            "gearbox" => CreateBlockRole::Gearbox,
            "clutch" => CreateBlockRole::Clutch,
            "gearshift" | "adjustable_chain_gearshift" | "sequenced_gearshift" => {
                CreateBlockRole::Gearshift
            }
            _ => CreateBlockRole::Decorative,
        };
        return (role, 0.0, 0.0);
    }
    // Addon/other namespaces: structural roles only, never stress numbers.
    let role = if path.contains("water_wheel") {
        CreateBlockRole::Generator { driven_rpm: 8 }
    } else if path.contains("large_cogwheel") {
        CreateBlockRole::LargeCogwheel
    } else if path.contains("cogwheel") {
        CreateBlockRole::Cogwheel
    } else if path.contains("gearbox") {
        CreateBlockRole::Gearbox
    } else if path.contains("clutch") {
        CreateBlockRole::Clutch
    } else if path.contains("gearshift") {
        CreateBlockRole::Gearshift
    } else if path.contains("shaft") {
        CreateBlockRole::Shaft
    } else if path == "belt" || path.ends_with("_belt") {
        CreateBlockRole::Belt
    } else if path.contains("funnel") || path == "depot" || path == "chute" {
        CreateBlockRole::Transport
    } else if path.contains("fluid_pipe") {
        CreateBlockRole::FluidPipe
    } else if path.contains("fluid_tank") || path.contains("_tank") {
        CreateBlockRole::FluidTank
    } else if path.contains("pump") {
        CreateBlockRole::FluidPump
    } else {
        CreateBlockRole::Decorative
    };
    (role, 0.0, 0.0)
}

/// Built-in Create 1.21.1 core block catalog (31 blocks covering kinetics,
/// belts/chutes/funnels, processing, fluids, casings, and contraption
/// anchors). Property tables mirror the Create 6.0.9/6.0.10 JAR
/// `assets/create/blockstates/*.json` spellings exactly (verified against
/// `create-1.21.1-6.0.10.jar`); when a real JAR is loaded,
/// [`merge_blockstate_table`] replaces them with the harvested tables.
/// The list is append-only: shipped save/MCF2 sidecars and MRS1 frames key
/// states by the index this order assigns, so new entries always land at
/// the end.
pub fn core_create_block_defs() -> Vec<BlockDef<CreateBlockRole>> {
    let axis = || PropertyDef::new("axis", &["x", "y", "z"]);
    // Value lists are alphabetically sorted to match the `BTreeSet` ordering
    // `mod_adapter_core` produces when harvesting JAR blockstates, keeping
    // cartesian-expansion order identical between fallback and JAR catalogs.
    let facing_all =
        || PropertyDef::new("facing", &["down", "east", "north", "south", "up", "west"]);
    let facing_horiz = || PropertyDef::new("facing", &["east", "north", "south", "west"]);
    let bool_values = || ["false", "true"];
    let waterlogged = || PropertyDef::new("waterlogged", &bool_values());
    let powered = || PropertyDef::new("powered", &bool_values());
    let extracting = || PropertyDef::new("extracting", &bool_values());

    vec![
        BlockDef {
            id: "create:shaft".into(),
            properties: vec![axis()],
            load_factor: 0.0,
            capacity_factor: 0.0,
            role: CreateBlockRole::Shaft,
        },
        BlockDef {
            id: "create:cogwheel".into(),
            properties: vec![axis()],
            load_factor: 0.0,
            capacity_factor: 0.0,
            role: CreateBlockRole::Cogwheel,
        },
        BlockDef {
            id: "create:large_cogwheel".into(),
            properties: vec![axis()],
            load_factor: 0.0,
            capacity_factor: 0.0,
            role: CreateBlockRole::LargeCogwheel,
        },
        BlockDef {
            id: "create:gearbox".into(),
            properties: vec![axis()],
            load_factor: 0.0,
            capacity_factor: 0.0,
            role: CreateBlockRole::Gearbox,
        },
        BlockDef {
            id: "create:clutch".into(),
            properties: vec![axis(), powered()],
            load_factor: 0.0,
            capacity_factor: 0.0,
            role: CreateBlockRole::Clutch,
        },
        BlockDef {
            id: "create:gearshift".into(),
            properties: vec![axis(), powered()],
            load_factor: 0.0,
            capacity_factor: 0.0,
            role: CreateBlockRole::Gearshift,
        },
        BlockDef {
            // Create 6.x water wheels declare `facing` (all six directions),
            // not `axis` — the rotation axis derives from the facing.
            id: "create:water_wheel".into(),
            properties: vec![facing_all()],
            load_factor: 0.0,
            capacity_factor: 32.0,
            role: CreateBlockRole::Generator { driven_rpm: 8 },
        },
        BlockDef {
            id: "create:large_water_wheel".into(),
            properties: vec![
                axis(),
                PropertyDef::new("extension", &bool_values()),
            ],
            load_factor: 0.0,
            capacity_factor: 128.0,
            role: CreateBlockRole::Generator { driven_rpm: 4 },
        },
        BlockDef {
            id: "create:windmill_bearing".into(),
            properties: vec![facing_all()],
            load_factor: 0.0,
            capacity_factor: 512.0,
            role: CreateBlockRole::Generator { driven_rpm: 16 },
        },
        BlockDef {
            // Creative-mode constant-RPM kinetic source — the standard
            // power supply in Ponder/showcase schematics.
            id: "create:creative_motor".into(),
            properties: vec![facing_all()],
            load_factor: 0.0,
            capacity_factor: 16384.0,
            role: CreateBlockRole::Generator { driven_rpm: 256 },
        },
        BlockDef {
            // Create 6.x belt: casing x facing x part x slope x waterlogged
            // = 320 variants (`part` includes `pulley`, `slope` includes
            // `sideways`/`vertical`).
            id: "create:belt".into(),
            properties: vec![
                PropertyDef::new("casing", &bool_values()),
                facing_horiz(),
                PropertyDef::new("part", &["end", "middle", "pulley", "start"]),
                PropertyDef::new(
                    "slope",
                    &["downward", "horizontal", "sideways", "upward", "vertical"],
                ),
                waterlogged(),
            ],
            load_factor: 1.0,
            capacity_factor: 0.0,
            role: CreateBlockRole::Belt,
        },
        BlockDef {
            // Create 6.x depot declares no blockstate properties at all.
            id: "create:depot".into(),
            properties: vec![],
            load_factor: 0.0,
            capacity_factor: 0.0,
            role: CreateBlockRole::Transport,
        },
        BlockDef {
            // Chute `facing` has no `up` value; `shape` gains `encased`.
            id: "create:chute".into(),
            properties: vec![
                PropertyDef::new("facing", &["down", "east", "north", "south", "west"]),
                PropertyDef::new("shape", &["encased", "intersection", "normal", "window"]),
                waterlogged(),
            ],
            load_factor: 0.0,
            capacity_factor: 0.0,
            role: CreateBlockRole::Transport,
        },
        BlockDef {
            id: "create:andesite_funnel".into(),
            properties: vec![extracting(), facing_all(), powered(), waterlogged()],
            load_factor: 0.0,
            capacity_factor: 0.0,
            role: CreateBlockRole::Transport,
        },
        BlockDef {
            id: "create:mechanical_press".into(),
            properties: vec![facing_horiz()],
            load_factor: 8.0,
            capacity_factor: 0.0,
            role: CreateBlockRole::Processor,
        },
        BlockDef {
            id: "create:mechanical_mixer".into(),
            properties: vec![],
            load_factor: 4.0,
            capacity_factor: 0.0,
            role: CreateBlockRole::Processor,
        },
        BlockDef {
            id: "create:basin".into(),
            properties: vec![PropertyDef::new(
                "facing",
                &["down", "north", "south", "east", "west"],
            )],
            load_factor: 0.0,
            capacity_factor: 0.0,
            role: CreateBlockRole::Processor,
        },
        BlockDef {
            id: "create:millstone".into(),
            properties: vec![],
            load_factor: 4.0,
            capacity_factor: 0.0,
            role: CreateBlockRole::Processor,
        },
        BlockDef {
            id: "create:fluid_pipe".into(),
            properties: vec![
                PropertyDef::new("north", &bool_values()),
                PropertyDef::new("south", &bool_values()),
                PropertyDef::new("east", &bool_values()),
                PropertyDef::new("west", &bool_values()),
                PropertyDef::new("up", &bool_values()),
                PropertyDef::new("down", &bool_values()),
                waterlogged(),
            ],
            load_factor: 0.0,
            capacity_factor: 0.0,
            role: CreateBlockRole::FluidPipe,
        },
        BlockDef {
            // Create 6.x mechanical pump declares `facing` only — no
            // `waterlogged` property.
            id: "create:mechanical_pump".into(),
            properties: vec![facing_all()],
            load_factor: 4.0,
            capacity_factor: 0.0,
            role: CreateBlockRole::FluidPump,
        },
        BlockDef {
            id: "create:fluid_tank".into(),
            properties: vec![
                PropertyDef::new("top", &bool_values()),
                PropertyDef::new("bottom", &bool_values()),
                PropertyDef::new(
                    "shape",
                    &[
                        "plain",
                        "window",
                        "window_nw",
                        "window_sw",
                        "window_ne",
                        "window_se",
                    ],
                ),
            ],
            load_factor: 0.0,
            capacity_factor: 0.0,
            role: CreateBlockRole::FluidTank,
        },
        BlockDef {
            id: "create:spout".into(),
            properties: vec![],
            load_factor: 0.0,
            capacity_factor: 0.0,
            role: CreateBlockRole::Processor,
        },
        BlockDef {
            id: "create:mechanical_bearing".into(),
            properties: vec![facing_all()],
            load_factor: 4.0,
            capacity_factor: 0.0,
            role: CreateBlockRole::ContraptionAnchor,
        },
        BlockDef {
            // Create 6.x stressometer: `axis_along_first` x `facing`, no
            // `axis`/`waterlogged`.
            id: "create:stressometer".into(),
            properties: vec![
                PropertyDef::new("axis_along_first", &bool_values()),
                facing_all(),
            ],
            load_factor: 0.0,
            capacity_factor: 0.0,
            role: CreateBlockRole::Sensor,
        },
        // Append-only tail (C2): belt funnels + brass funnel + casings —
        // Ponder/showcase schematics ship all of them, and the live item
        // flow wires every `Transport`-role duct.
        BlockDef {
            // Sits on a belt edge; `facing` is the horizontal direction the
            // funnel's mouth opens toward. Create 6.x belt funnels use
            // `shape` (extended/pulling/pushing/retracted), not `extracting`.
            id: "create:andesite_belt_funnel".into(),
            properties: vec![
                facing_horiz(),
                powered(),
                PropertyDef::new("shape", &["extended", "pulling", "pushing", "retracted"]),
                waterlogged(),
            ],
            load_factor: 0.0,
            capacity_factor: 0.0,
            role: CreateBlockRole::Transport,
        },
        BlockDef {
            id: "create:brass_belt_funnel".into(),
            properties: vec![
                facing_horiz(),
                powered(),
                PropertyDef::new("shape", &["extended", "pulling", "pushing", "retracted"]),
                waterlogged(),
            ],
            load_factor: 0.0,
            capacity_factor: 0.0,
            role: CreateBlockRole::Transport,
        },
        BlockDef {
            id: "create:brass_funnel".into(),
            properties: vec![extracting(), facing_all(), powered(), waterlogged()],
            load_factor: 0.0,
            capacity_factor: 0.0,
            role: CreateBlockRole::Transport,
        },
        BlockDef {
            id: "create:andesite_casing".into(),
            properties: vec![],
            load_factor: 0.0,
            capacity_factor: 0.0,
            role: CreateBlockRole::Decorative,
        },
        BlockDef {
            id: "create:brass_casing".into(),
            properties: vec![],
            load_factor: 0.0,
            capacity_factor: 0.0,
            role: CreateBlockRole::Decorative,
        },
        // Append-only tail: core Create 6.x generators missing from the
        // original catalog.
        BlockDef {
            // Steam engine: `face` (mount side) x horizontal `facing` x
            // `waterlogged`; 1024 SU per RPM at 64 RPM.
            id: "create:steam_engine".into(),
            properties: vec![
                PropertyDef::new("face", &["ceiling", "floor", "wall"]),
                facing_horiz(),
                waterlogged(),
            ],
            load_factor: 0.0,
            capacity_factor: 1024.0,
            role: CreateBlockRole::Generator { driven_rpm: 64 },
        },
        BlockDef {
            // Hand crank: `facing` x `waterlogged`; 8 SU per RPM at 32 RPM.
            id: "create:hand_crank".into(),
            properties: vec![facing_all(), waterlogged()],
            load_factor: 0.0,
            capacity_factor: 8.0,
            role: CreateBlockRole::Generator { driven_rpm: 32 },
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Number of expanded blockstates for `block_id` in `registry`.
    fn state_count(registry: &CreateBlockRegistry, block_id: &str) -> usize {
        let def = registry.block_def(block_id).expect(block_id);
        let mut count = 1usize;
        for prop in &def.properties {
            count *= prop.values.len();
        }
        count
    }

    #[test]
    fn fallback_catalog_matches_create_6_0_10_jar_state_counts() {
        let registry = core_create_registry(DEFAULT_BASE_STATE_ID);
        assert_eq!(registry.block_count(), 31);
        // Counts verified against create-1.21.1-6.0.10.jar blockstates.
        for (id, expected) in [
            ("create:water_wheel", 6),
            ("create:belt", 320),
            ("create:shaft", 3),
            ("create:cogwheel", 3),
            ("create:large_cogwheel", 3),
            ("create:large_water_wheel", 6),
            ("create:stressometer", 12),
            ("create:andesite_belt_funnel", 64),
            ("create:brass_belt_funnel", 64),
            ("create:depot", 1),
            ("create:mechanical_pump", 6),
            ("create:chute", 40),
            ("create:andesite_funnel", 48),
            ("create:brass_funnel", 48),
            ("create:steam_engine", 24),
            ("create:hand_crank", 12),
        ] {
            assert_eq!(state_count(&registry, id), expected, "{id}");
        }
        // Phantom properties removed: shafts/cogs have no `waterlogged`,
        // the pump has no `waterlogged`, funnels have no `extracting`,
        // depot has no properties at all.
        for id in ["create:shaft", "create:cogwheel", "create:large_cogwheel"] {
            let def = registry.block_def(id).unwrap();
            assert_eq!(def.properties.len(), 1, "{id}");
            assert_eq!(def.properties[0].name, "axis");
        }
        let pump = registry.block_def("create:mechanical_pump").unwrap();
        assert_eq!(pump.properties.len(), 1);
        assert_eq!(pump.properties[0].name, "facing");
        let depot = registry.block_def("create:depot").unwrap();
        assert!(depot.properties.is_empty());
        let belt_funnel = registry
            .block_def("create:andesite_belt_funnel")
            .unwrap();
        assert!(
            belt_funnel
                .properties
                .iter()
                .all(|p| p.name != "extracting")
        );
    }

    #[test]
    fn jar_blockstate_merge_registers_harvested_tables() {
        // Synthetic `assets/<ns>/blockstates/*.json` harvest, as produced by
        // `mod_adapter_core::data_loader::load_from_stored_zip`.
        let mut table = BTreeMap::new();
        table.insert(
            "create:water_wheel".to_string(),
            vec![PropertyDef {
                name: "facing".into(),
                values: ["down", "east", "north", "south", "up", "west"]
                    .iter()
                    .map(|s| s.to_string())
                    .collect(),
            }],
        );
        // A JAR-only Create block and an addon-namespace block.
        table.insert(
            "create:encased_fan".to_string(),
            vec![PropertyDef {
                name: "facing".into(),
                values: ["east", "north", "south", "west"]
                    .iter()
                    .map(|s| s.to_string())
                    .collect(),
            }],
        );
        table.insert("my_mod:alloy_gearbox".to_string(), vec![]);
        table.insert("bad id!!".to_string(), vec![]);

        let mut defs = core_create_block_defs();
        assert_eq!(merge_blockstate_table(&mut defs, &table), 2);
        let registry =
            BlockRegistry::build_with_aliases(DEFAULT_BASE_STATE_ID, defs, &core_create_block_aliases())
                .unwrap();

        // Known id kept its role/factors and took the JAR table verbatim.
        let wheel = registry.block_def("create:water_wheel").unwrap();
        assert_eq!(
            wheel.role,
            CreateBlockRole::Generator { driven_rpm: 8 }
        );
        assert_eq!(wheel.properties.len(), 1);
        assert_eq!(wheel.properties[0].name, "facing");
        assert_eq!(wheel.properties[0].values.len(), 6);

        // JAR-only create block got the verified AllBlocks stress constants.
        let fan = registry.block_def("create:encased_fan").unwrap();
        assert_eq!(fan.role, CreateBlockRole::Processor);
        assert_eq!(fan.load_factor, 2.0);

        // Addon block registered as a structural/Decorative entry.
        assert!(registry.block_def("my_mod:alloy_gearbox").is_some());
        // Invalid ids never poison the registry build.
        assert!(registry.block_def("bad id!!").is_none());
    }

    #[test]
    fn cartesian_expansion_and_save_v2_remapping_survive_reorder() {
        let base = core_create_registry(DEFAULT_BASE_STATE_ID);
        assert_eq!(base.block_count(), 31);
        let shaft_default = base.default_state("create:shaft").unwrap();
        assert_eq!(
            base.encode_canonical(shaft_default),
            Some("create:shaft[axis=x]")
        );
        let y_shaft = base.cycle_property(shaft_default, "axis").unwrap();
        assert_eq!(
            base.encode_canonical(y_shaft),
            Some("create:shaft[axis=y]")
        );
        assert_eq!(
            base.decode_canonical("create:shaft[axis=y]"),
            Some(y_shaft)
        );

        let mut extended_defs = core_create_block_defs();
        extended_defs.push(BlockDef {
            id: "create:aa_addon_axle".into(),
            properties: vec![PropertyDef::new("axis", &["x", "y", "z"])],
            load_factor: 0.0,
            capacity_factor: 0.0,
            role: CreateBlockRole::Shaft,
        });
        let extended = BlockRegistry::build(DEFAULT_BASE_STATE_ID, extended_defs).unwrap();
        let saved = base.encode_canonical(y_shaft).unwrap();
        let remapped = extended.decode_canonical(saved).unwrap();
        assert_ne!(remapped, y_shaft);
        assert_eq!(extended.encode_canonical(remapped), Some(saved));
    }

    #[test]
    fn belt_canonical_id_with_legacy_alias_and_creative_motor() {
        let base = core_create_registry(DEFAULT_BASE_STATE_ID);

        // Real Create 6.x registers `create:belt`; the adapter's historical
        // `create:mechanical_belt` stays a resolvable alias.
        let belt = base
            .decode_canonical(
                "create:belt[casing=false,facing=east,part=end,slope=downward,waterlogged=false]",
            )
            .expect("canonical belt state");
        assert_eq!(
            base.decode_canonical(
                "create:mechanical_belt[casing=false,facing=east,part=end,slope=downward,waterlogged=false]",
            ),
            Some(belt),
            "legacy sidecar spellings must land on the canonical belt state"
        );
        assert_eq!(
            base.default_state("create:mechanical_belt"),
            base.default_state("create:belt")
        );
        assert_eq!(
            base.encode_canonical(belt),
            Some("create:belt[casing=false,facing=east,part=end,slope=downward,waterlogged=false]"),
            "encode always re-emits the canonical id"
        );
        assert_eq!(
            base.block_def("create:mechanical_belt").map(|d| &d.role),
            Some(&CreateBlockRole::Belt)
        );

        // The creative motor registers as a constant-RPM stress source with
        // Create's 16384 SU/RPM capacity.
        let motor = base
            .default_state("create:creative_motor")
            .expect("creative motor registered");
        assert_eq!(
            base.encode_canonical(motor),
            Some("create:creative_motor[facing=down]")
        );
        let def = base.block_def("create:creative_motor").unwrap();
        assert_eq!(
            def.role,
            CreateBlockRole::Generator { driven_rpm: 256 }
        );
        assert_eq!(def.capacity_factor, 16384.0);
    }
}
