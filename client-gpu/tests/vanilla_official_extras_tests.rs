// SPDX-License-Identifier: LGPL-3.0-or-later
use megu_client_gpu::vanilla_official_extras::{VanillaOfficialExtras, default_skin_for_uuid};

#[test]
fn uuid_skin_selection_is_deterministic_and_in_range() {
    let mut selected = std::collections::BTreeSet::new();
    for n in 0..=255u8 {
        let mut uuid = [0u8; 16];
        uuid[15] = n;
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

fn uuid_with_java_hash(hash: i32) -> [u8; 16] {
    let mut uuid = [0u8; 16];
    uuid[..4].copy_from_slice(&hash.to_be_bytes());
    uuid
}

#[test]
fn default_skin_slots_match_vanilla_eighteen_slot_order() {
    const NAMES: [&str; 9] = [
        "alex", "ari", "efe", "kai", "makena", "noor", "steve", "sunny", "zuri",
    ];
    for slot in 0..18i32 {
        let (name, slim) = default_skin_for_uuid(uuid_with_java_hash(slot));
        assert_eq!(name, NAMES[slot as usize % 9], "slot {slot}");
        assert_eq!(slim, slot < 9, "slot {slot}");
    }
    // floorMod semantics: -1 selects slot 17 (wide zuri), while -18 wraps to
    // slot 0 (slim alex), matching Java's UUID.hashCode indexing.
    assert_eq!(
        default_skin_for_uuid(uuid_with_java_hash(-1)),
        ("zuri", false)
    );
    assert_eq!(
        default_skin_for_uuid(uuid_with_java_hash(-18)),
        ("alex", true)
    );
}

#[test]
fn missing_inputs_degrade_to_an_empty_catalog() {
    let extras = VanillaOfficialExtras::load(None, None, "17");
    assert!(extras.skin_rgba("steve", false).is_none());
    assert!(extras.armor_layer_rgba("iron", 1, false).is_none());
    assert!(
        extras
            .entity_variant_rgba("horse/horse_white.png")
            .is_none()
    );
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
