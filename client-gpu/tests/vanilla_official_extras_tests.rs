// SPDX-License-Identifier: LGPL-3.0-or-later
use megu_client_gpu::vanilla_official_extras::{default_skin_for_uuid, VanillaOfficialExtras};

#[test]
fn uuid_skin_selection_is_deterministic_and_in_range() {
    let mut selected = std::collections::BTreeSet::new();
    for n in 0..=255u8 {
        let uuid = [n; 16];
        let first = default_skin_for_uuid(uuid);
        assert_eq!(first, default_skin_for_uuid(uuid));
        assert!(matches!(
            first.0,
            "alex" | "ari" | "efe" | "kai" | "makena" | "noor" | "steve" | "sunny" | "zuri"
        ));
        selected.insert((first.0, first.1));
    }
    assert!(selected.len() > 1);
}

#[test]
fn missing_inputs_degrade_to_an_empty_catalog() {
    let extras = VanillaOfficialExtras::load(None, None, "17");
    assert!(extras.skin_rgba("steve", false).is_none());
    assert!(extras.armor_layer_rgba("iron", 1, false).is_none());
    assert!(extras
        .entity_variant_rgba("horse/horse_white.png")
        .is_none());
    assert!(extras.gui_sprite_rgba("icon/ping_1.png").is_none());
    assert!(extras.window_icons.is_empty());
    assert!(extras.builtin_resource_packs.is_empty());
    assert!(extras.credits_lines().is_empty());
    assert!(extras.registries.raw.is_empty());
}

#[test]
fn entity_variant_keys_are_stable_and_normalized() {
    assert_eq!(
        VanillaOfficialExtras::resolve_entity_variant_key("horse", "white"),
        "horse/horse_white.png"
    );
    assert_eq!(
        VanillaOfficialExtras::resolve_entity_variant_key("wolf", "wolf_angry.png"),
        "wolf/wolf_angry.png"
    );
    assert_eq!(
        VanillaOfficialExtras::resolve_entity_variant_key("textures/entity/cat/", ""),
        "cat/cat.png"
    );
}
