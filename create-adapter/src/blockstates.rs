// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later

//! Create 1.21.1 block catalog on top of [`mod_adapter_core::blockstates::BlockRegistry`].

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

/// Build the default core Create 1.21.1 block catalog (23 blocks).
pub fn core_create_registry(base_state_id: u32) -> CreateBlockRegistry {
    BlockRegistry::build(base_state_id, core_create_block_defs())
        .expect("built-in Create catalog must be valid")
}

/// Built-in Create 1.21.1 core block catalog (23 blocks covering kinetics,
/// belts/chutes, processing, fluids, and contraption anchors).
pub fn core_create_block_defs() -> Vec<BlockDef<CreateBlockRole>> {
    let axis = || PropertyDef::new("axis", &["x", "y", "z"]);
    let facing_all =
        || PropertyDef::new("facing", &["north", "south", "east", "west", "up", "down"]);
    let facing_horiz = || PropertyDef::new("facing", &["north", "south", "east", "west"]);
    let waterlogged = || PropertyDef::new("waterlogged", &["false", "true"]);
    let powered = || PropertyDef::new("powered", &["false", "true"]);

    vec![
        BlockDef {
            id: "create:shaft".into(),
            properties: vec![axis(), waterlogged()],
            load_factor: 0.0,
            capacity_factor: 0.0,
            role: CreateBlockRole::Shaft,
        },
        BlockDef {
            id: "create:cogwheel".into(),
            properties: vec![axis(), waterlogged()],
            load_factor: 0.0,
            capacity_factor: 0.0,
            role: CreateBlockRole::Cogwheel,
        },
        BlockDef {
            id: "create:large_cogwheel".into(),
            properties: vec![axis(), waterlogged()],
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
            id: "create:water_wheel".into(),
            properties: vec![axis()],
            load_factor: 0.0,
            capacity_factor: 32.0,
            role: CreateBlockRole::Generator { driven_rpm: 8 },
        },
        BlockDef {
            id: "create:large_water_wheel".into(),
            properties: vec![axis()],
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
            id: "create:mechanical_belt".into(),
            properties: vec![
                facing_horiz(),
                PropertyDef::new("part", &["start", "middle", "end", "pulley"]),
                PropertyDef::new("slope", &["horizontal", "upward", "downward", "vertical"]),
                waterlogged(),
            ],
            load_factor: 1.0,
            capacity_factor: 0.0,
            role: CreateBlockRole::Belt,
        },
        BlockDef {
            id: "create:depot".into(),
            properties: vec![waterlogged()],
            load_factor: 0.0,
            capacity_factor: 0.0,
            role: CreateBlockRole::Transport,
        },
        BlockDef {
            id: "create:chute".into(),
            properties: vec![
                facing_horiz(),
                PropertyDef::new("shape", &["normal", "window", "intersection"]),
                waterlogged(),
            ],
            load_factor: 0.0,
            capacity_factor: 0.0,
            role: CreateBlockRole::Transport,
        },
        BlockDef {
            id: "create:andesite_funnel".into(),
            properties: vec![
                facing_all(),
                PropertyDef::new("extracting", &["false", "true"]),
                powered(),
            ],
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
                PropertyDef::new("north", &["false", "true"]),
                PropertyDef::new("south", &["false", "true"]),
                PropertyDef::new("east", &["false", "true"]),
                PropertyDef::new("west", &["false", "true"]),
                PropertyDef::new("up", &["false", "true"]),
                PropertyDef::new("down", &["false", "true"]),
                waterlogged(),
            ],
            load_factor: 0.0,
            capacity_factor: 0.0,
            role: CreateBlockRole::FluidPipe,
        },
        BlockDef {
            id: "create:mechanical_pump".into(),
            properties: vec![facing_all(), waterlogged()],
            load_factor: 4.0,
            capacity_factor: 0.0,
            role: CreateBlockRole::FluidPump,
        },
        BlockDef {
            id: "create:fluid_tank".into(),
            properties: vec![
                PropertyDef::new("top", &["false", "true"]),
                PropertyDef::new("bottom", &["false", "true"]),
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
            id: "create:stressometer".into(),
            properties: vec![facing_horiz(), axis()],
            load_factor: 0.0,
            capacity_factor: 0.0,
            role: CreateBlockRole::Sensor,
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cartesian_expansion_and_save_v2_remapping_survive_reorder() {
        let base = core_create_registry(DEFAULT_BASE_STATE_ID);
        assert_eq!(base.block_count(), 23);
        let shaft_default = base.default_state("create:shaft").unwrap();
        assert_eq!(
            base.encode_canonical(shaft_default),
            Some("create:shaft[axis=x,waterlogged=false]")
        );
        let y_shaft = base.cycle_property(shaft_default, "axis").unwrap();
        assert_eq!(
            base.encode_canonical(y_shaft),
            Some("create:shaft[axis=y,waterlogged=false]")
        );
        assert_eq!(
            base.decode_canonical("create:shaft[waterlogged=false,axis=y]"),
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
}
