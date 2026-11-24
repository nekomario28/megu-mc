// SPDX-License-Identifier: LGPL-3.0-or-later
//! The curated Modrinth catalog and installed-mod precedence stay independent
//! of the host's installed-mod list and its staged enable/disable actions.

use std::collections::BTreeSet;

use megu_client_ui::mod_menu::{
    active_builtin_qol_overrides, modrinth_project_url, modrinth_version_lookup_url,
    valid_modrinth_slug, BuiltinQolFeature, BuiltinQolResolution, ModLoaderTag, ModMenuAction,
    ModMenuEntry, ModMenuFilter, ModMenuHitRegion, ModMenuState, ModMenuWidgetId, QolSubCategory,
    CURATED_MODRINTH_QOL_MODS,
};
use megu_client_ui::state::{UiInputEvent, UiKey, UiLang};

fn recommended_ids(state: &mut ModMenuState) -> Vec<String> {
    (0..state.visible_indices().len())
        .map(|index| {
            state.activate(ModMenuWidgetId::ModRow(index));
            state
                .selected_entry()
                .expect("visible catalog row")
                .mod_id
                .clone()
        })
        .collect()
}

#[test]
fn curated_projects_have_unique_valid_slugs_and_1211_loader_metadata() {
    let expected: BTreeSet<&str> = [
        "jade",
        "mouse-tweaks",
        "inventory-profiles-next",
        "inventory-essentials",
        "appleskin",
        "shulkerboxtooltip",
        "emi",
        "jei",
        "controlling",
        "searchables",
        "betterf3",
        "continuity",
        "entity-model-features",
        "entitytexturefeatures",
        "chat-heads",
    ]
    .into_iter()
    .collect();
    let mut actual = BTreeSet::new();
    for entry in CURATED_MODRINTH_QOL_MODS {
        assert!(actual.insert(entry.slug), "duplicate {}", entry.slug);
        assert!(valid_modrinth_slug(entry.slug));
        assert!(!entry.name.is_empty());
        assert!(!entry.summary_en.is_empty());
        assert!(!entry.summary_ja.is_empty());
        assert_eq!(entry.target_version, "1.21.1");
        assert_eq!(
            entry.loaders,
            &[ModLoaderTag::Fabric, ModLoaderTag::NeoForge]
        );
        assert_eq!(
            modrinth_version_lookup_url(entry.slug).as_deref(),
            Some(
                format!(
                    "https://api.modrinth.com/v2/project/{}/version?game_versions=%5B%221.21.1%22%5D",
                    entry.slug
                )
                .as_str()
            )
        );
        assert_eq!(
            modrinth_project_url(entry.slug),
            Some(format!("https://modrinth.com/mod/{}", entry.slug))
        );
    }
    assert_eq!(actual, expected);
}

#[test]
fn version_lookup_rejects_non_slug_characters() {
    for bad in [
        "",
        "Jade",
        "mouse_tweaks",
        "../jade",
        "a?b",
        "a/b",
        "a b",
        "émi",
    ] {
        assert!(!valid_modrinth_slug(bad), "accepted {bad:?}");
        assert_eq!(modrinth_version_lookup_url(bad), None);
        assert_eq!(modrinth_project_url(bad), None);
    }
}

#[test]
fn installed_aliases_yield_to_mods_and_unknown_mods_leave_builtins_active() {
    use BuiltinQolFeature::{InventorySort, LookingAtHud, MouseTweaksGestures};

    let disabled = active_builtin_qol_overrides(&[]);
    for feature in [LookingAtHud, MouseTweaksGestures, InventorySort] {
        assert_eq!(
            disabled.resolution(feature),
            BuiltinQolResolution::UseBuiltin
        );
        assert!(disabled.use_builtin(feature));
    }

    let active = active_builtin_qol_overrides(&["jade", "mousetweaks", "inventoryprofilesnext"]);
    for (feature, mod_id) in [
        (LookingAtHud, "jade"),
        (MouseTweaksGestures, "mousetweaks"),
        (InventorySort, "inventoryprofilesnext"),
    ] {
        assert_eq!(
            active.resolution(feature),
            BuiltinQolResolution::DeferToInstalledMod {
                mod_id: mod_id.to_owned(),
            }
        );
        assert!(!active.use_builtin(feature));
    }
    for unknown in ["notjade", "jadelike", "unrelated-mod", "mouse_tweaks"] {
        assert_eq!(
            active_builtin_qol_overrides(&[unknown]).resolution(LookingAtHud),
            BuiltinQolResolution::UseBuiltin
        );
    }
}

#[test]
fn all_yielding_catalog_mods_and_aliases_are_resolved() {
    use BuiltinQolFeature::*;
    for (id, feature) in [
        ("inventory-essentials", InventorySort),
        ("appleskin", HungerSaturationPreview),
        ("shulkerboxtooltip", ContainerTooltipPreview),
        ("emi", RecipeBrowser),
        ("jei", RecipeBrowser),
        ("continuity", ConnectedTextures),
        ("entity_model_features", EmfModels),
        ("entity_texture_features", EtfTextures),
        ("entity-texture-features", EtfTextures),
    ] {
        assert_eq!(
            active_builtin_qol_overrides(&[id]).resolution(feature),
            BuiltinQolResolution::DeferToInstalledMod {
                mod_id: id.to_owned(),
            }
        );
    }
    // A single built-in feature is only yielded once when two mods provide it.
    assert_eq!(
        active_builtin_qol_overrides(&["jei", "emi"]).resolution(RecipeBrowser),
        BuiltinQolResolution::DeferToInstalledMod {
            mod_id: "emi".into(),
        }
    );
}

#[test]
fn menu_deference_tracks_enabled_and_staged_changes() {
    use BuiltinQolFeature::LookingAtHud;
    let mut jade = ModMenuEntry::new("jade", "Jade", "1.0");
    jade.enabled = false;
    let mut menu = ModMenuState::new(vec![jade]);
    assert!(menu
        .active_builtin_qol_deference()
        .use_builtin(LookingAtHud));

    menu.activate(ModMenuWidgetId::ToggleEnabled);
    assert_eq!(
        menu.active_builtin_qol_deference().resolution(LookingAtHud),
        BuiltinQolResolution::DeferToInstalledMod {
            mod_id: "jade".into(),
        }
    );
    menu.activate(ModMenuWidgetId::ApplyChanges);
    assert!(!menu
        .active_builtin_qol_deference()
        .use_builtin(LookingAtHud));

    menu.activate(ModMenuWidgetId::ToggleEnabled);
    assert!(menu
        .active_builtin_qol_deference()
        .use_builtin(LookingAtHud));
}

#[test]
fn recommended_catalog_is_searchable_and_category_filterable_without_installing() {
    let mut menu = ModMenuState::new(vec![ModMenuEntry::new("my-mod", "My Mod", "1.0")]);
    assert_eq!(menu.entries.len(), 1);
    assert_eq!(menu.visible_indices().len(), 1);
    menu.activate(ModMenuWidgetId::FilterRecommendedQol);
    assert_eq!(menu.filter, ModMenuFilter::RecommendedQol);
    assert_eq!(menu.visible_indices().len(), 15);

    for slug in ["jade", "mouse-tweaks", "inventory-profiles-next"] {
        menu.search = slug.to_owned();
        assert_eq!(recommended_ids(&mut menu), [slug]);
    }
    menu.search.clear();
    menu.activate(ModMenuWidgetId::QolInventoryInput);
    assert_eq!(
        menu.recommended_category,
        Some(QolSubCategory::InventoryInput)
    );
    let inventory = recommended_ids(&mut menu);
    assert!(inventory.contains(&"mouse-tweaks".to_owned()));
    assert!(inventory.contains(&"inventory-profiles-next".to_owned()));
    assert!(!inventory.contains(&"jade".to_owned()));

    menu.activate(ModMenuWidgetId::QolRecipes);
    assert_eq!(recommended_ids(&mut menu), ["emi", "jei"]);
    menu.activate(ModMenuWidgetId::FilterRecommendedQol);
    assert_eq!(menu.visible_indices().len(), 15);

    menu.search = "jade".into();
    let result = menu.activate(ModMenuWidgetId::InstallRecommended);
    assert_eq!(
        result,
        [ModMenuAction::OpenUrl(
            "https://modrinth.com/mod/jade".into()
        )]
    );
    assert!(!menu.is_dirty());
    assert!(menu.activate(ModMenuWidgetId::ToggleEnabled).is_empty());
    menu.activate(ModMenuWidgetId::FilterAll);
    assert_eq!(menu.entries.len(), 1);
    menu.search.clear();
    assert_eq!(recommended_ids(&mut menu), ["my-mod"]);
}

#[test]
fn recommended_filter_and_install_button_have_visible_hit_regions() {
    let mut menu = ModMenuState::new(vec![]);
    for lang in [UiLang::En, UiLang::Ja] {
        menu.lang = lang;
        let (_, hits) = menu.render(640, 360);
        assert!(hits
            .iter()
            .any(|hit| hit.id == ModMenuWidgetId::FilterRecommendedQol));
        menu.activate(ModMenuWidgetId::FilterRecommendedQol);
        let (_, hits) = menu.render(640, 360);
        for button in [
            ModMenuWidgetId::QolHudInfo,
            ModMenuWidgetId::QolInventoryInput,
            ModMenuWidgetId::QolRecipes,
            ModMenuWidgetId::QolVisualsCompat,
            ModMenuWidgetId::InstallRecommended,
        ] {
            assert!(
                hits.iter().any(|hit| hit.id == button),
                "missing {button:?}"
            );
        }
        assert!(!hits
            .iter()
            .any(|hit| hit.id == ModMenuWidgetId::ToggleEnabled));
        menu.activate(ModMenuWidgetId::FilterAll);
    }
}

fn click(
    menu: &mut ModMenuState,
    width: u32,
    height: u32,
    id: ModMenuWidgetId,
) -> Vec<ModMenuAction> {
    let (_, hits) = menu.render(width, height);
    let rect = hits
        .iter()
        .find(|hit| hit.id == id)
        .unwrap_or_else(|| panic!("missing hit region for {id:?}"))
        .rect;
    menu.handle_input(
        UiInputEvent::Click {
            x: rect.x + rect.w as i32 / 2,
            y: rect.y + rect.h as i32 / 2,
        },
        width,
        height,
    )
}

fn assert_no_overlapping_hits(hits: &[ModMenuHitRegion]) {
    for (index, a) in hits.iter().enumerate() {
        for b in hits.iter().skip(index + 1) {
            assert!(
                !a.rect.intersects(b.rect),
                "hit regions overlap: {a:?} and {b:?}"
            );
        }
    }
}

#[test]
fn recommendation_buttons_are_clickable_and_hit_regions_do_not_overlap() {
    for lang in [UiLang::En, UiLang::Ja] {
        for (width, height) in [(640, 360), (1280, 720), (1920, 1080)] {
            let mut menu = ModMenuState::new(vec![]);
            menu.lang = lang;
            assert_no_overlapping_hits(&menu.render(width, height).1);
            assert!(click(
                &mut menu,
                width,
                height,
                ModMenuWidgetId::FilterRecommendedQol
            )
            .is_empty());
            assert_eq!(menu.filter, ModMenuFilter::RecommendedQol);
            assert_no_overlapping_hits(&menu.render(width, height).1);
            assert_eq!(
                click(
                    &mut menu,
                    width,
                    height,
                    ModMenuWidgetId::InstallRecommended,
                ),
                [ModMenuAction::OpenUrl(
                    "https://modrinth.com/mod/appleskin".into()
                )]
            );
            click(&mut menu, width, height, ModMenuWidgetId::QolRecipes);
            assert_eq!(menu.recommended_category, Some(QolSubCategory::Recipes));
            assert_no_overlapping_hits(&menu.render(width, height).1);
            click(&mut menu, width, height, ModMenuWidgetId::FilterAll);
            assert_eq!(menu.filter, ModMenuFilter::All);
        }
    }
}

#[test]
fn recommended_keyboard_activation_opens_selected_modrinth_page() {
    let mut menu = ModMenuState::new(vec![]);
    menu.activate(ModMenuWidgetId::FilterRecommendedQol);
    menu.search = "jade".into();
    assert!(menu.search_focused());
    assert!(menu
        .handle_input(UiInputEvent::KeyPressed(UiKey::Enter), 640, 360)
        .is_empty());
    assert!(!menu.search_focused());
    assert_eq!(
        menu.handle_input(UiInputEvent::KeyPressed(UiKey::Enter), 640, 360),
        [ModMenuAction::OpenUrl(
            "https://modrinth.com/mod/jade".into()
        )]
    );
    assert!(!menu.is_dirty());
}
