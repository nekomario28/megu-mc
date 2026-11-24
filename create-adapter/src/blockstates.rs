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

/// Build the default core Create 1.21.1 block catalog (29 blocks).
pub fn core_create_registry(base_state_id: u32) -> CreateBlockRegistry {
    BlockRegistry::build_with_aliases(
        base_state_id,
        core_create_block_defs(),
        &core_create_block_aliases(),
    )
    .expect("built-in Create catalog must be valid")
}

/// Built-in Create 1.21.1 core block catalog (29 blocks covering kinetics,
/// belts/chutes/funnels, processing, fluids, casings, and contraption
/// anchors). The list is append-only: shipped save/MCF2 sidecars and MRS1
/// frames key states by the index this order assigns, so new entries always
/// land at the end.
pub fn core_create_block_defs() -> Vec<BlockDef<CreateBlockRole>> {
    let axis = || PropertyDef::new("axis", &["x", "y", "z"]);
    let facing_all =
        || PropertyDef::new("facing", &["north", "south", "east", "west", "up", "down"]);
    let facing_horiz = || PropertyDef::new("facing", &["north", "south", "east", "west"]);
    let waterlogged = || PropertyDef::new("waterlogged", &["false", "true"]);
    let powered = || PropertyDef::new("powered", &["false", "true"]);
    let extracting = || PropertyDef::new("extracting", &["false", "true"]);

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
            // Creative-mode constant-RPM kinetic source — the standard
            // power supply in Ponder/showcase schematics.
            id: "create:creative_motor".into(),
            properties: vec![facing_all()],
            load_factor: 0.0,
            capacity_factor: 2048.0,
            role: CreateBlockRole::Generator { driven_rpm: 256 },
        },
        BlockDef {
            id: "create:belt".into(),
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
            properties: vec![facing_all(), extracting(), powered()],
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
        // Append-only tail (C2): belt funnels + brass funnel + casings —
        // Ponder/showcase schematics ship all of them, and the live item
        // flow wires every `Transport`-role duct.
        BlockDef {
            // Sits on a belt edge; `facing` is the horizontal direction the
            // funnel's mouth opens toward.
            id: "create:andesite_belt_funnel".into(),
            properties: vec![facing_horiz(), extracting(), powered()],
            load_factor: 0.0,
            capacity_factor: 0.0,
            role: CreateBlockRole::Transport,
        },
        BlockDef {
            id: "create:brass_belt_funnel".into(),
            properties: vec![facing_horiz(), extracting(), powered()],
            load_factor: 0.0,
            capacity_factor: 0.0,
            role: CreateBlockRole::Transport,
        },
        BlockDef {
            id: "create:brass_funnel".into(),
            properties: vec![facing_all(), extracting(), powered()],
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
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cartesian_expansion_and_save_v2_remapping_survive_reorder() {
        let base = core_create_registry(DEFAULT_BASE_STATE_ID);
        assert_eq!(base.block_count(), 29);
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

    #[test]
    fn belt_canonical_id_with_legacy_alias_and_creative_motor() {
        let base = core_create_registry(DEFAULT_BASE_STATE_ID);

        // Real Create 6.x registers `create:belt`; the adapter's historical
        // `create:mechanical_belt` stays a resolvable alias.
        let belt = base
            .decode_canonical(
                "create:belt[facing=north,part=middle,slope=horizontal,waterlogged=false]",
            )
            .expect("canonical belt state");
        assert_eq!(
            base.decode_canonical(
                "create:mechanical_belt[facing=north,part=middle,slope=horizontal,waterlogged=false]",
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
            Some("create:belt[facing=north,part=middle,slope=horizontal,waterlogged=false]"),
            "encode always re-emits the canonical id"
        );
        assert_eq!(
            base.block_def("create:mechanical_belt").map(|d| &d.role),
            Some(&CreateBlockRole::Belt)
        );

        // The creative motor registers as a constant-RPM stress source.
        let motor = base
            .default_state("create:creative_motor")
            .expect("creative motor registered");
        assert_eq!(
            base.encode_canonical(motor),
            Some("create:creative_motor[facing=north]")
        );
        let def = base.block_def("create:creative_motor").unwrap();
        assert_eq!(
            def.role,
            CreateBlockRole::Generator { driven_rpm: 256 }
        );
        assert!(def.capacity_factor > 0.0);
    }
}
