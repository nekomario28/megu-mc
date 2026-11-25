use megu_client_ui::container_screen::{
    container_skin_sprite_key, menu_schema, ContainerScreenState, ContainerSlotHotkey, UiItemStack,
};
use megu_client_ui::creative_screen::{
    CreativeClick, CreativeInventoryAction, CreativeInventoryState, CreativeItem, CreativeTab,
};
use megu_client_ui::draw::UiDrawList;
use megu_client_ui::gameplay_screens::{
    BedSleepOverlay, BookEditorClick, BookEditorState, EndCreditsState,
};
use megu_client_ui::state::{HudScreenOverlay, LiveStatusEffect, UiState};

#[test]
fn container_skins_cover_registry_menus_and_inventory() {
    assert_eq!(container_skin_sprite_key(0), "container/generic_54");
    assert_eq!(container_skin_sprite_key(5), "container/generic_54");
    assert_eq!(container_skin_sprite_key(14), "container/furnace");
    assert_eq!(container_skin_sprite_key(17), "container/lectern");
    assert_eq!(container_skin_sprite_key(u32::MAX), "container/inventory");
    assert_eq!(container_skin_sprite_key(99), "");
}

#[test]
fn container_hotkeys_and_shift_click_emit_protocol_modes() {
    let mut c = ContainerScreenState::new(menu_schema(14).unwrap(), 2, 3, "Furnace");
    c.set_slot(0, UiItemStack::new(1, 4));
    c.hover(Some(0));
    assert_eq!(c.click_with_shift(0, 0, true).unwrap().mode, 1);
    assert_eq!(
        c.handle_slot_hotkey(ContainerSlotHotkey::HotbarSwap(8))
            .unwrap()
            .mode,
        2
    );
    assert_eq!(
        c.handle_slot_hotkey(ContainerSlotHotkey::OffhandSwap)
            .unwrap()
            .button,
        40
    );
    c.set_slot(0, UiItemStack::new(1, 4));
    assert_eq!(
        c.handle_slot_hotkey(ContainerSlotHotkey::DropStack)
            .unwrap()
            .button,
        1
    );
}

#[test]
fn creative_inventory_search_tabs_actions_and_render() {
    let mut c = CreativeInventoryState::default();
    c.set_items(vec![
        CreativeItem {
            item_id: 1,
            name: "stone".into(),
            count: 64,
        },
        CreativeItem {
            item_id: 2,
            name: "dirt".into(),
            count: 64,
        },
    ]);
    c.set_search("sto");
    assert_eq!(c.filtered_items().count(), 1);
    assert_eq!(
        c.click(CreativeClick::Item(0)),
        Some(CreativeInventoryAction::SetCreativeSlot {
            slot: -1,
            item_id: 1,
            count: 64
        })
    );
    c.click(CreativeClick::Tab(CreativeTab::Search));
    let mut out = UiDrawList::new();
    c.render(640, 360, &mut out);
    assert!(!out.commands.is_empty());
}

#[test]
fn gameplay_editors_and_hud_fields_are_operable() {
    let mut book = BookEditorState::new(4);
    book.type_char('A');
    book.click(BookEditorClick::Next);
    book.type_char('B');
    assert_eq!(book.pages, vec!["A", "B"]);
    book.click(BookEditorClick::SignAndClose);
    book.type_char('T');
    assert!(
        matches!(book.submit_title(), megu_client_ui::gameplay_screens::BookEditorAction::Save { title: Some(t), .. } if t=="T")
    );
    let mut credits = EndCreditsState::default();
    credits.load_text("one", "two");
    credits.tick(1.0, true);
    assert!(credits.scroll > 0.0);
    let overlay = BedSleepOverlay { visible: true };
    assert!(overlay.click(320, 202, 640, 360));
    let mut ui = UiState::default();
    ui.hud.status_effects.push(LiveStatusEffect {
        effect_id: 1,
        name: "Speed".into(),
        amplifier: 0,
        duration_ticks: 100,
        beneficial: true,
    });
    ui.hud.screen_overlay = HudScreenOverlay {
        hurt_alpha: 0.5,
        ..Default::default()
    };
    ui.set_translations([("menu.singleplayer".to_string(), "Solo".to_string())].into());
    assert_eq!(ui.translate("menu.singleplayer", "Singleplayer"), "Solo");
}
