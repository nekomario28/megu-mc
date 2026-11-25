// SPDX-License-Identifier: LGPL-3.0-or-later

use megu_client::protocol::{
    encode_container_button_click, encode_container_click, encode_container_slot_state_changed,
    encode_rename_item, encode_select_trade, encode_set_beacon,
};
use megu_client_mesh::ber_instancing::{
    BerChestKind, VanillaBerExtendedState, VanillaBerFacing, VanillaParticlePayload,
    VanillaParticleWorld, build_extended_ber_mesh,
};
use megu_client_mesh::entity_rig::{
    EntityPose, EntityRigPose, build_entity_rig_mesh_with_pose, classify_entity_rig,
};
use megu_ui::container_screen::{
    menu_schema, ContainerScreenState, MerchantOfferView, WorkstationAction,
};

fn screen(menu_id: u32) -> ContainerScreenState {
    ContainerScreenState::new(
        menu_schema(menu_id).expect("vanilla menu schema"),
        1,
        0,
        "test",
    )
}

#[test]
fn workstation_packets_encode_protocol_767_actions() {
    assert_eq!(
        encode_rename_item("Sharp axe"),
        ("rename_item", b"\x09Sharp axe".to_vec())
    );
    assert_eq!(encode_select_trade(2), ("select_trade", vec![2]));
    assert_eq!(
        encode_set_beacon(Some(5), Some(12)),
        ("set_beacon", vec![1, 5, 1, 12])
    );
    assert_eq!(
        encode_container_slot_state_changed(8, 4, true),
        ("container_slot_state_changed", vec![8, 4, 1]),
    );
    assert_eq!(encode_container_button_click(7, 2), ("container_button_click", vec![7, 2]));
}

#[test]
fn container_click_modes_encode_packet_bodies() {
    for (button, mode) in [
        (0, 0), // left click
        (1, 0), // right click
        (0, 1), // shift click
        (0, 2), // number-key swap
        (0, 4), // drop
        (0, 5), // drag
        (0, 6), // double-click collect
    ] {
        let (name, payload) = encode_container_click(3, 11, 5, button, mode, &[], &[0]);
        assert_eq!(name, "container_click");
        assert!(!payload.is_empty(), "mode {mode} must produce a packet body");
    }
}

#[test]
fn anvil_text_input_emits_a_rename_action() {
    let mut anvil = screen(8);
    let rect = anvil.rename_input_rect(800.0, 600.0).unwrap();
    anvil.click_at(rect.x + 1, rect.y + 1, 800.0, 600.0);
    let action = anvil.handle_rename_char('M').unwrap();
    assert_eq!(action, WorkstationAction::RenameItem("M".to_owned()));
}

#[test]
fn beacon_confirm_emits_the_selected_effects() {
    let mut beacon = screen(9);
    beacon.set_property(0, 4);
    let panel = beacon.panel_rect(800.0, 600.0);
    beacon.click_at(panel.x + 4 + 24 + 1, panel.y + 4 + 54 + 1, 800.0, 600.0);
    let action = beacon
        .click_at(panel.x + 4 + 158 + 1, panel.y + 4 + 72 + 1, 800.0, 600.0)
        .unwrap();
    assert_eq!(
        action,
        WorkstationAction::SetBeacon {
            primary: Some(1),
            secondary: None,
        }
    );
}

#[test]
fn merchant_row_click_emits_a_trade_selection() {
    let mut merchant = screen(19);
    merchant.set_merchant_offers(vec![MerchantOfferView {
        input1: "minecraft:emerald".to_owned(),
        input2: None,
        output: "minecraft:bread".to_owned(),
        original_cost: 4,
        effective_cost: 3,
        uses: 0,
        max_uses: 12,
        out_of_stock: false,
    }]);
    let panel = merchant.panel_rect(800.0, 600.0);
    let action = merchant
        .click_at(panel.x + 4 + 9, panel.y + 4 + 19, 800.0, 600.0)
        .unwrap();
    assert_eq!(action, WorkstationAction::SelectTrade(0));
}

#[test]
fn articulated_rig_pose_reaches_the_mesh_builder() {
    let spec = classify_entity_rig(42).expect("known vanilla rig");
    let pose = EntityRigPose {
        base: EntityPose {
            baby: true,
            head_yaw_deg: 24.0,
            hurt_flash: true,
            ..EntityPose::default()
        },
        sleeping: true,
        ..EntityRigPose::default()
    };
    let mesh = build_entity_rig_mesh_with_pose(spec, pose);
    assert!(!mesh.quads.is_empty());
}

#[test]
fn particle_pool_and_animated_chest_state_produce_mesh_data() {
    let mut particles = VanillaParticleWorld::new();
    assert!(particles.spawn_protocol(
        13,
        [1.0, 2.0, 3.0],
        [0.0, 0.1, 0.0],
        7,
        VanillaParticlePayload::Dust {
            rgb: [0.2, 0.8, 0.4],
            scale: 1.0,
        },
    ));
    particles.step(0.05);
    assert!(
        !particles
            .billboard_quads([1.0, 0.0, 0.0], [0.0, 1.0, 0.0])
            .is_empty()
    );

    let state = VanillaBerExtendedState::Chest {
        lid_open: 0.6,
        double: false,
        facing: VanillaBerFacing::South,
        chest: BerChestKind::Normal,
    };
    let mesh = build_extended_ber_mesh(state.kind(), state);
    assert!(!mesh.quads.is_empty());
}
