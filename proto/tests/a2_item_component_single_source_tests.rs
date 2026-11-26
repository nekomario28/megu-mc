#![allow(dead_code)]

// data_v767 is private in the library crate today, so this integration test
// compiles the owned table source directly to exercise its public API.
mod table {
    #[derive(Clone, Copy)]
    pub enum PacketPhase {
        Handshake,
        Status,
        Login,
        Configuration,
        Play,
    }
    #[derive(Clone, Copy)]
    pub enum PacketDir {
        Clientbound,
        Serverbound,
    }
}

#[path = "../src/data_v767.rs"]
mod data_v767;

use data_v767::{component_ids, component_id_by_name, component_name_by_id, item_id_by_name, item_name_by_id};
use std::collections::HashSet;

#[test]
fn every_item_round_trips_by_bare_and_namespaced_name() {
    assert_eq!(data_v767::V767_ITEMS.len(), 1333);
    let mut names = HashSet::new();
    for (id, canonical) in data_v767::V767_ITEMS.iter().enumerate() {
        assert!(names.insert(*canonical), "duplicate item name {canonical}");
        assert_eq!(item_name_by_id(id as u32), Some(*canonical));
        let bare = canonical.strip_prefix("minecraft:").unwrap();
        assert_eq!(item_id_by_name(bare), Some(id as u32), "bare {bare}");
        assert_eq!(item_id_by_name(canonical), Some(id as u32), "canonical {canonical}");
    }
    assert_eq!(item_name_by_id(1333), None);
    assert_eq!(item_id_by_name("not_an_item"), None);
}

#[test]
fn every_component_round_trips_and_matches_typed_constant() {
    let constants = [
        component_ids::CUSTOM_DATA, component_ids::MAX_STACK_SIZE, component_ids::MAX_DAMAGE,
        component_ids::DAMAGE, component_ids::UNBREAKABLE, component_ids::CUSTOM_NAME,
        component_ids::ITEM_NAME, component_ids::LORE, component_ids::RARITY,
        component_ids::ENCHANTMENTS, component_ids::CAN_PLACE_ON, component_ids::CAN_BREAK,
        component_ids::ATTRIBUTE_MODIFIERS, component_ids::CUSTOM_MODEL_DATA,
        component_ids::HIDE_ADDITIONAL_TOOLTIP, component_ids::HIDE_TOOLTIP,
        component_ids::REPAIR_COST, component_ids::CREATIVE_SLOT_LOCK,
        component_ids::ENCHANTMENT_GLINT_OVERRIDE, component_ids::INTANGIBLE_PROJECTILE,
        component_ids::FOOD, component_ids::FIRE_RESISTANT, component_ids::TOOL,
        component_ids::STORED_ENCHANTMENTS, component_ids::DYED_COLOR, component_ids::MAP_COLOR,
        component_ids::MAP_ID, component_ids::MAP_DECORATIONS, component_ids::MAP_POST_PROCESSING,
        component_ids::CHARGED_PROJECTILES, component_ids::BUNDLE_CONTENTS, component_ids::POTION_CONTENTS,
        component_ids::SUSPICIOUS_STEW_EFFECTS, component_ids::WRITABLE_BOOK_CONTENT,
        component_ids::WRITTEN_BOOK_CONTENT, component_ids::TRIM, component_ids::DEBUG_STICK_STATE,
        component_ids::ENTITY_DATA, component_ids::BUCKET_ENTITY_DATA, component_ids::BLOCK_ENTITY_DATA,
        component_ids::INSTRUMENT, component_ids::OMINOUS_BOTTLE_AMPLIFIER,
        component_ids::JUKEBOX_PLAYABLE, component_ids::RECIPES, component_ids::LODESTONE_TRACKER,
        component_ids::FIREWORK_EXPLOSION, component_ids::FIREWORKS, component_ids::PROFILE,
        component_ids::NOTE_BLOCK_SOUND, component_ids::BANNER_PATTERNS, component_ids::BASE_COLOR,
        component_ids::POT_DECORATIONS, component_ids::CONTAINER, component_ids::BLOCK_STATE,
        component_ids::BEES, component_ids::LOCK, component_ids::CONTAINER_LOOT,
    ];
    assert_eq!(data_v767::V767_COMPONENTS.len(), 57);
    assert_eq!(constants.len(), 57);
    let mut names = HashSet::new();
    for (id, canonical) in data_v767::V767_COMPONENTS.iter().enumerate() {
        assert!(names.insert(*canonical), "duplicate component name {canonical}");
        assert_eq!(constants[id], id as u32, "constant at component id {id}");
        assert_eq!(component_name_by_id(id as u32), Some(*canonical));
        let bare = canonical.strip_prefix("minecraft:").unwrap();
        assert_eq!(component_id_by_name(bare), Some(id as u32), "bare {bare}");
        assert_eq!(component_id_by_name(canonical), Some(id as u32), "canonical {canonical}");
    }
    assert_eq!(component_name_by_id(57), None);
    assert_eq!(component_id_by_name("not_a_component"), None);
}
