// SPDX-License-Identifier: LGPL-3.0-or-later
//! Integration tests for the Phase-0b core foundation expansion:
//! `megu_core::rev` (universal revision/dirty-layer bus),
//! `megu_core::registry` (universal namespaced registry spine) and
//! `megu_core::ui_contract` (canonical 1.21.1 theme + declarative UI
//! contract).

use megu_core::id::InvalidReason;
use megu_core::registry::{
    InvalidNamespacedId, NamespacedId, RegistryError, RegistryId, UniversalRegistry,
};
use megu_core::rev::{
    DirtyLayers, LayerRevBus, RetainedLayerTracker, Revision, UiLayer, UiLayerBus,
};
use megu_core::ui_contract::{
    audit_manifest, compute_gui_scale, snap_px, ModUiManifest, Rgba8, SpriteId,
    UiContractError, UiExtensionSlot, UiRect, UiWidgetKind, UiWidgetSpec,
    VanillaThemeTokens, VANILLA_1_21_1, MAX_LABEL_LEN, MAX_MOD_ID_LEN,
    MAX_WIDGETS_PER_MANIFEST,
};

// ---------------------------------------------------------------------------
// rev — Revision tokens, LayerRevBus, RetainedLayerTracker
// ---------------------------------------------------------------------------

#[test]
fn revision_none_is_the_never_touched_sentinel() {
    assert_eq!(Revision::NONE, Revision(0));
    assert_eq!(Revision::NONE.get(), 0);
    assert!(Revision::NONE.is_none());
    assert!(!Revision(1).is_none());
    assert!(Revision::NONE < Revision(1));
    assert_eq!(Revision::default(), Revision::NONE);
}

#[test]
fn bump_is_monotonic_and_never_returns_none() {
    let mut bus = LayerRevBus::<8>::new();
    let mut last = Revision::NONE;
    for expected in 1..=10u64 {
        let rev = bus.bump(0);
        assert_eq!(rev, Revision(expected));
        assert!(rev > last);
        last = rev;
    }
    assert_eq!(bus.rev(0), last);
    assert_eq!(bus.len(), 8);
}

#[test]
fn steady_state_reports_zero_dirty() {
    let bus = UiLayerBus::new();
    let mut tracker = RetainedLayerTracker::<8>::new();

    // The first poll performs the initial full bake; after that a bus
    // with no bumps must report an empty diff forever — the
    // "0 rebuilds, 0 allocs, 0 GPU uploads" fast path.
    assert_eq!(tracker.poll(&bus).count() as usize, 8);
    for _ in 0..100 {
        let dirty = tracker.poll(&bus);
        assert!(dirty.is_empty());
        assert_eq!(dirty, DirtyLayers::NONE);
        assert_eq!(dirty.mask(), 0);
        assert_eq!(dirty.iter().count(), 0);
    }
}

#[test]
fn bumping_dynamic_hud_dirties_nothing_else() {
    let mut bus = UiLayerBus::new();
    let mut tracker = RetainedLayerTracker::<8>::new();
    tracker.poll(&bus);

    UiLayer::DynamicHud.bump(&mut bus);
    let dirty = tracker.poll(&bus);

    assert!(dirty.contains(UiLayer::DynamicHud.index()));
    for layer in UiLayer::ALL {
        let should_be_dirty = layer == UiLayer::DynamicHud;
        assert_eq!(
            dirty.contains(layer.index()),
            should_be_dirty,
            "layer {layer} ({})",
            layer.as_str()
        );
    }
    // Explicitly: StaticHud, Screen and ModHud stay clean.
    assert!(!dirty.contains(UiLayer::StaticHud.index()));
    assert!(!dirty.contains(UiLayer::Screen.index()));
    assert!(!dirty.contains(UiLayer::ModHud.index()));
    assert_eq!(dirty.mask(), 1 << UiLayer::DynamicHud.index());
    assert_eq!(dirty.iter().collect::<Vec<_>>(), vec![2]);
}

#[test]
fn mod_hud_isolation_from_vanilla_layers() {
    let mut bus = UiLayerBus::new();
    let mut tracker = RetainedLayerTracker::<8>::new();
    tracker.poll(&bus);

    // A mod HUD (Create goggles, minimap, WASM HUD) invalidates only
    // its own layer; the vanilla stack must not re-tessellate.
    UiLayer::ModHud.bump(&mut bus);
    let dirty = tracker.poll(&bus);
    assert_eq!(dirty.mask(), 1 << UiLayer::ModHud.index());
    for layer in [
        UiLayer::Backdrop,
        UiLayer::StaticHud,
        UiLayer::DynamicHud,
        UiLayer::Screen,
        UiLayer::Overlay,
        UiLayer::ModOverlay,
    ] {
        assert!(!dirty.contains(layer.index()), "{layer} falsely dirtied");
    }
}

#[test]
fn simultaneous_bumps_coalesce_into_one_mask() {
    let mut bus = UiLayerBus::new();
    let mut tracker = RetainedLayerTracker::<8>::new();
    tracker.poll(&bus);

    UiLayer::DynamicHud.bump(&mut bus);
    UiLayer::ModOverlay.bump(&mut bus);
    UiLayer::ModHud.bump(&mut bus);

    let dirty = tracker.poll(&bus);
    assert_eq!(dirty.mask(), (1 << 2) | (1 << 5) | (1 << 6));
    assert_eq!(dirty.count(), 3);
    assert_eq!(dirty.iter().collect::<Vec<_>>(), vec![2, 5, 6]);
    assert!(tracker.poll(&bus).is_empty());
}

#[test]
fn frame_dirty_bits_track_bump_and_clear() {
    let mut bus = UiLayerBus::new();
    assert!(!bus.any_dirty());
    assert_eq!(bus.dirty_mask(), 0);

    UiLayer::Screen.bump(&mut bus);
    UiLayer::Overlay.bump(&mut bus);
    assert!(bus.any_dirty());
    assert_eq!(bus.dirty_mask(), (1 << 3) | (1 << 4));
    assert!(bus.is_flagged_dirty(3));
    assert!(!bus.is_flagged_dirty(2));

    bus.mark_clean(3);
    assert_eq!(bus.dirty_mask(), 1 << 4);
    assert!(bus.any_dirty());

    bus.clear_dirty();
    assert!(!bus.any_dirty());
    assert_eq!(bus.dirty_mask(), 0);
    // Revisions are history, not flags — they survive the clear.
    assert!(bus.is_dirty_since(3, Revision::NONE));
    assert!(!bus.is_dirty_since(3, bus.rev(3)));
}

#[test]
fn is_dirty_since_compares_generations_not_flags() {
    let mut bus = UiLayerBus::new();
    let seen_at_bake = UiLayer::StaticHud.bump(&mut bus);
    bus.mark_clean(UiLayer::StaticHud.index());

    // Frame flag cleared, but the generation still differs from NONE.
    assert!(!bus.is_flagged_dirty(1));
    assert!(bus.is_dirty_since(1, Revision::NONE));
    assert!(!bus.is_dirty_since(1, seen_at_bake));

    // A later bump makes the stored token stale again.
    UiLayer::StaticHud.bump(&mut bus);
    assert!(bus.is_dirty_since(1, seen_at_bake));
    assert!(!bus.is_dirty_since(1, bus.rev(1)));
}

#[test]
fn peek_dirty_and_mark_baked_support_selective_bakes() {
    let mut bus = UiLayerBus::new();
    let mut tracker = RetainedLayerTracker::<8>::new();
    tracker.poll(&bus);

    UiLayer::DynamicHud.bump(&mut bus);
    UiLayer::Overlay.bump(&mut bus);

    // Read-only diff — the tracker is not advanced.
    let peek = tracker.peek_dirty(&bus);
    assert_eq!(peek.mask(), (1 << 2) | (1 << 4));
    // Bake only the overlay; dynamic HUD must still report dirty.
    tracker.mark_baked(4, bus.rev(4));
    assert_eq!(tracker.baked_rev(4), Some(bus.rev(4)));
    let next = tracker.peek_dirty(&bus);
    assert_eq!(next.mask(), 1 << 2);
}

#[test]
fn tracker_reset_forces_full_rebake() {
    let bus = UiLayerBus::new();
    let mut tracker = RetainedLayerTracker::<8>::new();
    tracker.poll(&bus);
    assert!(tracker.poll(&bus).is_empty());
    tracker.reset();
    let dirty = tracker.poll(&bus);
    assert_eq!(dirty.count() as usize, 8);
}

#[test]
fn ui_layer_indices_and_keys_are_canonical() {
    assert_eq!(UiLayer::Backdrop.index(), 0);
    assert_eq!(UiLayer::StaticHud.index(), 1);
    assert_eq!(UiLayer::DynamicHud.index(), 2);
    assert_eq!(UiLayer::Screen.index(), 3);
    assert_eq!(UiLayer::Overlay.index(), 4);
    assert_eq!(UiLayer::ModHud.index(), 5);
    assert_eq!(UiLayer::ModOverlay.index(), 6);
    assert_eq!(UiLayer::COUNT, 7);
    assert_eq!(UiLayer::ALL.len(), 7);
    for (index, layer) in UiLayer::ALL.iter().copied().enumerate() {
        assert_eq!(layer.index(), index);
        assert_eq!(UiLayer::from_index(index), Some(layer));
    }
    // Bit 7 on UiLayerBus is reserved headroom, not a defined layer.
    assert_eq!(UiLayer::from_index(7), None);
    assert_eq!(UiLayer::from_index(64), None);
    // Keys are unique and stable.
    let mut keys: Vec<&'static str> = UiLayer::ALL.iter().map(|l| l.as_str()).collect();
    keys.sort_unstable();
    keys.dedup();
    assert_eq!(keys.len(), UiLayer::COUNT);
}

#[test]
fn bus_layers_above_64_use_the_overflow_flag() {
    let mut bus = LayerRevBus::<80>::new();
    bus.bump(70);
    assert!(bus.any_dirty());
    assert!(bus.dirty_high());
    assert_eq!(bus.dirty_mask(), 0);
    assert!(bus.is_flagged_dirty(79));
    assert!(bus.is_dirty_since(70, Revision::NONE));
    bus.mark_clean(70);
    assert!(!bus.any_dirty());

    // And the retained tracker folds it into `high` — after the
    // initial full bake, only the bumped high layer reports dirty.
    let mut tracker = RetainedLayerTracker::<80>::new();
    tracker.poll(&bus);
    bus.bump(70);
    let dirty = tracker.poll(&bus);
    assert!(dirty.has_high());
    assert!(dirty.contains(70));
    assert!(!dirty.contains(3));
    assert_eq!(dirty.mask(), 0);
}

#[test]
fn dirty_layers_union_and_display() {
    let a = DirtyLayers::NONE;
    let mut bus = UiLayerBus::new();
    let mut tracker = RetainedLayerTracker::<8>::new();
    tracker.poll(&bus);
    UiLayer::Screen.bump(&mut bus);
    let b = tracker.peek_dirty(&bus);
    let union = a.union(b);
    assert_eq!(union.mask(), 1 << 3);
    assert!(!union.is_empty());
    assert!(format!("{union}").contains("0x"));
}

// ---------------------------------------------------------------------------
// registry — NamespacedId, UniversalRegistry, digest
// ---------------------------------------------------------------------------

#[test]
fn namespaced_id_applies_the_minecraft_default() {
    let id = NamespacedId::parse("stone").unwrap();
    assert_eq!(id.namespace(), "minecraft");
    assert_eq!(id.path(), "stone");
    assert_eq!(id.as_str(), "minecraft:stone");
    assert_eq!(id.to_string(), "minecraft:stone");

    let qualified = NamespacedId::parse("create:mechanical_belt").unwrap();
    assert_eq!(qualified.namespace(), "create");
    assert_eq!(qualified.path(), "mechanical_belt");
}

#[test]
fn namespaced_id_rejects_every_invalid_spelling() {
    let invalid: &[&str] = &[
        "",
        ":",
        ":path",
        "ns:",
        "a:b:c",
        "FOO:bar",
        "foo:BAR",
        "fo o:bar",
        "foo:bar baz",
        "föö:bar",
        "ns:..",
        "ns:a/../b",
        "ns:\u{7f}a", // control character
        "ns:\u{1b}[0m",
    ];
    for input in invalid {
        assert!(
            NamespacedId::parse(input).is_err(),
            "{input:?} should be rejected"
        );
    }
    let err: InvalidNamespacedId = NamespacedId::parse("a:b:c").unwrap_err();
    assert_eq!(err.reason(), InvalidReason::TooManySeparators);
}

#[test]
fn universal_registry_assigns_dense_ids_in_order() {
    let mut reg: UniversalRegistry<u32> = UniversalRegistry::new();
    assert!(reg.is_empty());
    let stone = reg
        .register(NamespacedId::parse("minecraft:stone").unwrap(), 1)
        .unwrap();
    let belt = reg
        .register(NamespacedId::parse("create:mechanical_belt").unwrap(), 2)
        .unwrap();
    let forest = reg
        .register(
            NamespacedId::parse("twilightforest:twilight_forest").unwrap(),
            3,
        )
        .unwrap();

    assert_eq!(stone, RegistryId(0));
    assert_eq!(belt, RegistryId(1));
    assert_eq!(forest, RegistryId(2));
    assert_eq!(reg.len(), 3);

    // O(1) both directions.
    assert_eq!(reg.get(belt), Some(&2));
    assert_eq!(reg.name_of(forest).unwrap().as_str(), "twilightforest:twilight_forest");
    assert_eq!(reg.id_of("create:mechanical_belt"), Some(belt));
    assert_eq!(reg.entry(stone).map(|(n, _)| n.path()), Some("stone"));
    assert_eq!(reg.get(RegistryId(99)), None);
    assert_eq!(reg.name_of(RegistryId(99)), None);
    assert_eq!(reg.id_of("nowhere:missing"), None);
}

#[test]
fn id_of_resolves_unqualified_names_through_minecraft() {
    let mut reg: UniversalRegistry<()> = UniversalRegistry::new();
    let stone = reg.register(NamespacedId::parse("minecraft:stone").unwrap(), ()).unwrap();
    let dirt = reg.register(NamespacedId::parse("minecraft:dirt").unwrap(), ()).unwrap();
    reg.register(NamespacedId::parse("ae2:certus_quartz").unwrap(), ()).unwrap();

    // Bare spellings hit the minecraft default namespace.
    assert_eq!(reg.id_of("stone"), Some(stone));
    assert_eq!(reg.id_of("dirt"), Some(dirt));
    // A bare spelling never crosses namespaces.
    assert_eq!(reg.id_of("certus_quartz"), None);
    // Malformed inputs fail closed.
    assert_eq!(reg.id_of("a:b:c"), None);
    assert_eq!(reg.id_of("Foo:bar"), None);
}

#[test]
fn duplicate_registration_is_rejected() {
    let mut reg: UniversalRegistry<()> = UniversalRegistry::new();
    reg.register(NamespacedId::parse("minecraft:stone").unwrap(), ())
        .unwrap();
    let err = reg
        .register(NamespacedId::parse("minecraft:stone").unwrap(), ())
        .unwrap_err();
    assert_eq!(
        err,
        RegistryError::Duplicate {
            id: NamespacedId::parse("minecraft:stone").unwrap()
        }
    );
    // The canonical spelling is stored once: "stone" collides with
    // "minecraft:stone" too.
    assert!(reg.register(NamespacedId::parse("stone").unwrap(), ()).is_err());
    assert_eq!(reg.len(), 1);
}

#[test]
fn freeze_seals_the_dense_id_space() {
    let mut reg: UniversalRegistry<()> = UniversalRegistry::new();
    reg.register(NamespacedId::parse("minecraft:stone").unwrap(), ())
        .unwrap();
    reg.freeze();
    assert!(reg.is_frozen());
    let err = reg
        .register(NamespacedId::parse("create:cogwheel").unwrap(), ())
        .unwrap_err();
    assert!(matches!(err, RegistryError::Frozen { .. }));
    assert!(err.to_string().contains("frozen"));
    // Existing lookups still work after the freeze.
    assert_eq!(reg.id_of("stone"), Some(RegistryId(0)));
    assert_eq!(reg.len(), 1);
}

#[test]
fn digest_crc32_is_deterministic_for_identical_tables() {
    let build = || {
        let mut reg: UniversalRegistry<u32> = UniversalRegistry::new();
        for (name, v) in [
            ("minecraft:air", 0u32),
            ("minecraft:stone", 1),
            ("minecraft:dirt", 2),
            ("create:mechanical_belt", 100),
            ("twilightforest:twilight_forest", 200),
        ] {
            reg.register(NamespacedId::parse(name).unwrap(), v).unwrap();
        }
        reg
    };
    let a = build();
    let b = build();
    assert_eq!(a.digest_crc32(), b.digest_crc32());

    // Order matters: permuted registration digests differently.
    let mut permuted: UniversalRegistry<u32> = UniversalRegistry::new();
    for name in [
        "create:mechanical_belt",
        "minecraft:air",
        "minecraft:stone",
        "minecraft:dirt",
        "twilightforest:twilight_forest",
    ] {
        permuted
            .register(NamespacedId::parse(name).unwrap(), 0)
            .unwrap();
    }
    assert_ne!(a.digest_crc32(), permuted.digest_crc32());

    // One extra or missing entry diverges too.
    let mut extra = build();
    extra
        .register(NamespacedId::parse("minecraft:grass_block").unwrap(), 4)
        .unwrap();
    assert_ne!(a.digest_crc32(), extra.digest_crc32());
}

#[test]
fn digest_covers_identity_not_payload() {
    let mut a: UniversalRegistry<u32> = UniversalRegistry::new();
    a.register(NamespacedId::parse("minecraft:stone").unwrap(), 1).unwrap();
    let mut b: UniversalRegistry<u32> = UniversalRegistry::new();
    b.register(NamespacedId::parse("minecraft:stone").unwrap(), 9999).unwrap();
    // Values are payload, not identity — the digest agrees.
    assert_eq!(a.digest_crc32(), b.digest_crc32());

    // Length framing keeps namespace/path boundaries unambiguous:
    // ("minecraf","tab") must not digest like ("minecraft","ab").
    let mut x: UniversalRegistry<()> = UniversalRegistry::new();
    x.register(NamespacedId::new("minecraft", "ab").unwrap(), ()).unwrap();
    let mut y: UniversalRegistry<()> = UniversalRegistry::new();
    y.register(NamespacedId::new("minecraf", "tab").unwrap(), ()).unwrap();
    assert_ne!(x.digest_crc32(), y.digest_crc32());
}

#[test]
fn registry_is_generic_over_payload_types() {
    // Blocks, items, menus, channels, recipes — one spine, any T.
    struct BlockState(u32);
    struct MenuType(&'static str);
    let mut blocks: UniversalRegistry<BlockState> = UniversalRegistry::new();
    let mut menus: UniversalRegistry<MenuType> = UniversalRegistry::with_capacity(4);
    let bid = blocks
        .register(NamespacedId::parse("minecraft:stone").unwrap(), BlockState(7))
        .unwrap();
    let mid = menus
        .register(NamespacedId::parse("minecraft:chest").unwrap(), MenuType("chest"))
        .unwrap();
    assert_eq!(blocks.get(bid).map(|b| b.0), Some(7));
    assert_eq!(menus.get(mid).map(|m| m.0), Some("chest"));

    // Iteration is insertion-ordered (id, name, value) triples.
    let seen: Vec<String> = blocks
        .iter()
        .map(|(id, name, _)| format!("{id}={name}"))
        .collect();
    assert_eq!(seen, ["#0=minecraft:stone"]);
}

// ---------------------------------------------------------------------------
// ui_contract — Rgba8, VANILLA_1_21_1, gui scale, declarative tree
// ---------------------------------------------------------------------------

#[test]
fn rgba8_constructs_and_converts() {
    let c = Rgba8::rgb(0xC6, 0xC6, 0xC6);
    assert_eq!(c.a, 255);
    assert_eq!(Rgba8::rgba(1, 2, 3, 4).a, 4);
    assert_eq!(c.with_alpha(10).a, 10);
    let f = Rgba8::rgba(255, 0, 128, 255).to_f32();
    assert_eq!(f[0], 1.0);
    assert_eq!(f[1], 0.0);
    assert!((f[2] - 128.0 / 255.0).abs() < 1e-6);
    assert_eq!(f[3], 1.0);
    assert_eq!(Rgba8::TRANSPARENT, Rgba8::rgba(0, 0, 0, 0));
}

#[test]
fn vanilla_tokens_are_the_measured_1_21_1_hex_values() {
    let t: VanillaThemeTokens = VANILLA_1_21_1;

    // Panel chrome.
    assert_eq!(t.panel_face, Rgba8::rgb(0xC6, 0xC6, 0xC6));
    assert_eq!(t.panel_bevel_light, Rgba8::rgb(0xFF, 0xFF, 0xFF));
    assert_eq!(t.panel_bevel_dark, Rgba8::rgb(0x55, 0x55, 0x55));
    assert_eq!(t.panel_outline, Rgba8::rgb(0x00, 0x00, 0x00));

    // Stone button states.
    assert_eq!(t.button_face, Rgba8::rgb(0x6F, 0x6F, 0x6F));
    assert_eq!(t.button_bevel_light, Rgba8::rgb(0xA6, 0xA6, 0xA6));
    assert_eq!(t.button_bevel_dark, Rgba8::rgb(0x48, 0x48, 0x48));
    assert_eq!(t.button_hover_face, Rgba8::rgb(0x7E, 0x88, 0xA8));
    assert_eq!(t.button_highlight_border, Rgba8::rgb(0xFF, 0xFF, 0xFF));
    assert_eq!(t.button_disabled_face, Rgba8::rgb(0x3A, 0x3A, 0x3A));
    assert_eq!(t.button_disabled_text, Rgba8::rgb(0xA0, 0xA0, 0xA0));

    // Recessed slot.
    assert_eq!(t.slot_face, Rgba8::rgb(0x8B, 0x8B, 0x8B));
    assert_eq!(t.slot_inner_bevel_dark, Rgba8::rgb(0x37, 0x37, 0x37));
    assert_eq!(t.slot_inner_bevel_light, Rgba8::rgb(0xFF, 0xFF, 0xFF));
    assert_eq!(t.slot_hover_highlight, Rgba8::rgba(255, 255, 255, 128));

    // Classic tooltip.
    assert_eq!(t.tooltip_background, Rgba8::rgba(16, 0, 16, 240));
    assert_eq!(t.tooltip_border_top, Rgba8::rgba(80, 0, 255, 80));
    assert_eq!(t.tooltip_border_bottom, Rgba8::rgba(40, 0, 127, 80));

    // Text roles.
    assert_eq!(t.text_light, Rgba8::rgb(0xFF, 0xFF, 0xFF));
    assert_eq!(t.text_shadow, Rgba8::rgb(0x3F, 0x3F, 0x3F));
    assert_eq!(t.text_focused, Rgba8::rgb(0xFF, 0xFF, 0xA0));
    assert_eq!(t.text_dark, Rgba8::rgb(0x40, 0x40, 0x40));
    assert_eq!(t.text_muted, Rgba8::rgb(0xA0, 0xA0, 0xA0));
    assert_eq!(t.text_danger, Rgba8::rgb(0xFF, 0x55, 0x55));
    assert_eq!(t.text_positive, Rgba8::rgb(0x55, 0xFF, 0x55));
    assert_eq!(t.text_warning, Rgba8::rgb(0xFF, 0xFF, 0x55));

    assert_eq!(t.scrim, Rgba8::rgba(16, 16, 16, 192));

    // Metrics.
    assert_eq!(VanillaThemeTokens::BUTTON_HEIGHT, 20);
    assert_eq!(VanillaThemeTokens::BUTTON_WIDTH_DEFAULT, 200);
    assert_eq!(VanillaThemeTokens::SLOT_SIZE, 18);
    assert_eq!(VanillaThemeTokens::SLOT_ICON_SIZE, 16);
    assert_eq!(VanillaThemeTokens::HOTBAR_SLOTS, 9);
    assert_eq!(VanillaThemeTokens::TEXT_SHADOW_OFFSET_PX, 1);
    assert_eq!(VanillaThemeTokens::TOOLTIP_PADDING_PX, 4);
}

#[test]
fn sprite_ids_cover_the_canonical_nine_slice_table() {
    assert_eq!(SpriteId::COUNT, 15);
    assert_eq!(SpriteId::ALL.len(), 15);
    let mut keys: Vec<&'static str> = SpriteId::ALL.iter().map(|s| s.as_str()).collect();
    keys.sort_unstable();
    keys.dedup();
    assert_eq!(keys.len(), SpriteId::COUNT, "sprite keys must be unique");
    for expected in [
        "button", "button_highlighted", "button_disabled", "slot", "panel",
        "scroller", "scroller_background", "text_field", "text_field_highlighted",
        "tooltip_background", "tooltip_frame", "tab_selected", "tab_unselected",
        "header_separator", "footer_separator",
    ] {
        assert!(keys.contains(&expected), "missing sprite key {expected}");
    }
}

#[test]
fn compute_gui_scale_matches_vanilla_1_21_1() {
    // Auto (requested=0): largest integer s with w/s >= 320, h/s >= 240 —
    // the exact values vanilla's Window.calculateScale produces.
    let auto_cases: &[((u32, u32), u32)] = &[
        ((640, 480), 2),
        ((1280, 720), 3),
        ((1920, 1080), 4),
        ((2560, 1440), 6),
        ((3840, 2160), 9),
        ((320, 240), 1),  // smallest legal viewport
        ((300, 200), 1),  // below minimum — still clamps to 1
    ];
    for &((w, h), want) in auto_cases {
        assert_eq!(compute_gui_scale(w, h, 0), want, "{w}x{h} auto");
    }
    // Requested caps the result; the size limit can pull it lower.
    assert_eq!(compute_gui_scale(3840, 2160, 2), 2);
    assert_eq!(compute_gui_scale(3840, 2160, 4), 4);
    assert_eq!(compute_gui_scale(1920, 1080, 8), 4);
    assert_eq!(compute_gui_scale(1920, 1080, 1), 1);
    // The result is always an integer >= 1.
    assert!(compute_gui_scale(1, 1, 0) >= 1);
}

#[test]
fn snap_px_lands_on_the_device_grid() {
    assert_eq!(snap_px(10, 2), 20);
    assert_eq!(snap_px(-7, 3), -21);
    assert_eq!(snap_px(0, 4), 0);
    assert_eq!(snap_px(VanillaThemeTokens::TEXT_SHADOW_OFFSET_PX, 4), 4);
    // Saturates rather than wrapping on absurd inputs.
    assert_eq!(snap_px(i32::MAX, u32::MAX), i32::MAX);
}

#[test]
fn widget_specs_enforce_text_and_rect_bounds() {
    let ok = UiWidgetSpec::new(UiWidgetKind::Button, UiRect::new(10, 10, 200, 20))
        .with_label("Done");
    assert!(ok.validate().is_ok());

    // Oversized label.
    let long_label = "x".repeat(MAX_LABEL_LEN + 1);
    let bad = UiWidgetSpec::new(UiWidgetKind::Label, UiRect::new(0, 0, 10, 10))
        .with_label(long_label);
    assert!(matches!(
        bad.validate(),
        Err(UiContractError::TextTooLong { field: "label", .. })
    ));

    // Zero-size and out-of-range rects.
    for rect in [
        UiRect::new(0, 0, 0, 10),
        UiRect::new(0, 0, 10, 0),
        UiRect::new(0, 0, 8_193, 10),
        UiRect::new(20_000, 0, 10, 10),
        UiRect::new(0, -20_000, 10, 10),
    ] {
        let w = UiWidgetSpec::new(UiWidgetKind::Panel, rect);
        assert!(
            matches!(w.validate(), Err(UiContractError::RectOutOfBounds { .. })),
            "rect {rect:?} should fail"
        );
    }

    // Slider range checks only apply to value-bearing kinds.
    let slider = UiWidgetSpec::new(UiWidgetKind::Slider, UiRect::new(0, 0, 150, 20))
        .with_range(0, 100, 50);
    assert!(slider.validate().is_ok());
    let inverted = UiWidgetSpec::new(UiWidgetKind::Slider, UiRect::new(0, 0, 150, 20))
        .with_range(100, 0, 50);
    assert!(matches!(
        inverted.validate(),
        Err(UiContractError::RangeInverted { .. })
    ));
    let out = UiWidgetSpec::new(UiWidgetKind::ProgressBar, UiRect::new(0, 0, 150, 5))
        .with_range(0, 100, 150);
    assert!(matches!(
        out.validate(),
        Err(UiContractError::ValueOutOfRange { .. })
    ));
    // A button ignores range fields entirely — same range, still valid.
    let button = UiWidgetSpec::new(UiWidgetKind::Button, UiRect::new(0, 0, 150, 20))
        .with_range(0, 100, 150);
    assert!(button.validate().is_ok());
}

#[test]
fn manifest_bounds_keep_rogue_mods_bounded() {
    // A healthy manifest validates.
    let mut good = ModUiManifest::new("create", UiExtensionSlot::HudBottomRight);
    good.push_widget(
        UiWidgetSpec::new(UiWidgetKind::Panel, UiRect::new(4, 4, 180, 60))
            .with_label("Goggles"),
    );
    good.push_widget(
        UiWidgetSpec::new(UiWidgetKind::ProgressBar, UiRect::new(8, 40, 160, 5))
            .with_range(0, 100, 42),
    );
    assert!(good.validate().is_ok());

    // Widget-count cap: a mod cannot emit unbounded UI.
    let mut flood = ModUiManifest::new("greedy", UiExtensionSlot::CustomScreen);
    for i in 0..(MAX_WIDGETS_PER_MANIFEST + 1) {
        flood.push_widget(UiWidgetSpec::new(
            UiWidgetKind::Label,
            UiRect::new(0, i as i32, 10, 10),
        ));
    }
    assert!(matches!(
        flood.validate(),
        Err(UiContractError::TooManyWidgets { .. })
    ));

    // mod_id hygiene: empty, too long, uppercase/invalid chars.
    assert!(matches!(
        ModUiManifest::new("", UiExtensionSlot::HudTopLeft).validate(),
        Err(UiContractError::EmptyModId)
    ));
    assert!(matches!(
        ModUiManifest::new("x".repeat(MAX_MOD_ID_LEN + 1), UiExtensionSlot::HudTopLeft)
            .validate(),
        Err(UiContractError::ModIdTooLong { .. })
    ));
    for bad_id in ["Create", "my mod", "mod:name", "möd", "a/b"] {
        assert!(
            matches!(
                ModUiManifest::new(bad_id, UiExtensionSlot::HudTopLeft).validate(),
                Err(UiContractError::BadModIdChar { .. })
            ),
            "mod_id {bad_id:?} should be rejected"
        );
    }
}

#[test]
fn audit_manifest_reports_scale_and_every_violation() {
    let mut manifest = ModUiManifest::new("BadMod", UiExtensionSlot::HudTopRight);
    manifest.push_widget(
        UiWidgetSpec::new(UiWidgetKind::Label, UiRect::new(0, 0, 0, 0))
            .with_label("oops"),
    );
    let report = audit_manifest(&manifest, 1920, 1080, 0);

    assert_eq!(report.gui_scale, 4);
    assert_eq!(report.widget_count, 1);
    assert_eq!(report.max_widgets, MAX_WIDGETS_PER_MANIFEST);
    assert_eq!(report.max_text_len, MAX_LABEL_LEN);
    assert!(!report.is_within_budget());
    // Both the bad mod id and the bad rect are reported.
    assert!(report
        .violations
        .iter()
        .any(|v| matches!(v, UiContractError::BadModIdChar { .. })));
    assert!(report
        .violations
        .iter()
        .any(|v| matches!(v, UiContractError::RectOutOfBounds { .. })));

    let clean = ModUiManifest::new("ae2", UiExtensionSlot::ContainerRightPanel);
    let clean_report = audit_manifest(&clean, 2560, 1440, 2);
    assert!(clean_report.is_within_budget());
    assert_eq!(clean_report.gui_scale, 2);
    assert_eq!(clean_report.widget_count, 0);
}

#[test]
fn title_screen_purity_is_an_architectural_invariant() {
    // The slot enum simply cannot express "draw on the title screen":
    // every variant reports it cannot overlay, and no key names one.
    assert_eq!(UiExtensionSlot::COUNT, 9);
    for slot in UiExtensionSlot::ALL {
        assert!(!slot.overlays_title_screen(), "{slot} must not overlay title");
        assert!(
            !slot.as_str().contains("title"),
            "slot key {:?} must not reference the title screen",
            slot.as_str()
        );
    }
    // A manifest on every legal slot stays valid — purity is not a
    // validator false-positive.
    for slot in UiExtensionSlot::ALL {
        let m = ModUiManifest::new("some_mod", slot);
        assert!(m.validate().is_ok(), "slot {slot} must stay attachable");
    }
}

#[test]
fn widget_kinds_and_rect_helpers_behave() {
    assert_eq!(UiWidgetKind::COUNT, 11);
    let mut keys: Vec<&'static str> = UiWidgetKind::ALL.iter().map(|k| k.as_str()).collect();
    keys.sort_unstable();
    keys.dedup();
    assert_eq!(keys.len(), UiWidgetKind::COUNT);

    let rect = UiRect::new(10, 20, 18, 18);
    assert!(!rect.is_empty());
    assert_eq!(rect.right(), 28);
    assert_eq!(rect.bottom(), 38);
    assert!(rect.contains(10, 20));
    assert!(rect.contains(27, 37));
    assert!(!rect.contains(28, 20));
    assert!(!rect.contains(10, 38));
    assert!(UiRect::new(0, 0, 0, 5).is_empty());
}
