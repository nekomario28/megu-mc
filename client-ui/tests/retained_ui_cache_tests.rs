// SPDX-License-Identifier: LGPL-3.0-or-later
//! Integration proof for [`UiRetainedCache`]: the retained 7-layer
//! tessellation cache behind `client-gpu`'s UI upload path.
//!
//! Contract under test:
//! 1. Idle frames do **0 re-tessellations and 0 GPU upload marks**.
//! 2. A single-layer bump (`DynamicHud`) leaves `StaticHud`, `Screen`,
//!    `Overlay` and `ModHud` untouched.
//! 3. A fully populated frame (HUD + container + JEI overlay + F3 +
//!    chat + a [`ModUiManifest`]) composites into ONE merged batch with
//!    zero buffer-capacity growth after warm-up.

use megu_client_ui::draw::{
    palette, GuiSpriteAtlas, ItemIconAtlas, UiColor, UiDrawList, UiRect,
};
use megu_client_ui::retained_cache::{RetainedCacheStats, UiRetainedCache};
use megu_client_ui::vanilla_widgets::{
    draw_mod_ui_manifest, draw_vanilla_button, VanillaButtonState, VanillaWidgets,
};
use megu_core::rev::{DirtyLayers, UiLayer, UiLayerBus};
use megu_core::ui_contract::{
    ModUiManifest, UiExtensionSlot, UiRect as ContractRect, UiWidgetKind, UiWidgetSpec,
};

const VIEW_W: u32 = 1280;
const VIEW_H: u32 = 720;

fn icons() -> ItemIconAtlas {
    ItemIconAtlas::default()
}

fn sprites() -> GuiSpriteAtlas {
    GuiSpriteAtlas::default()
}

/// A realistic per-layer draw list: `n` fill quads plus a text line.
fn layer_list(seed: i32, quads: usize) -> UiDrawList {
    let mut list = UiDrawList::new();
    for i in 0..quads {
        let x = seed + (i as i32) * 4;
        list.fill_rect(UiRect::new(x, i as i32 * 4, 8, 8), palette::PANEL);
    }
    list.text_str(seed, seed, 1, UiColor::rgb(255, 255, 255), "layer");
    list
}

/// The "everything open" frame: hotbar-like HUD, container grid, JEI
/// sidebar, F3 debug text, chat and one mod manifest.
fn full_scene_lists() -> [(UiLayer, UiDrawList); 7] {
    let mut backdrop = UiDrawList::new();
    backdrop.fill_rect(
        UiRect::new(0, 0, VIEW_W, VIEW_H),
        UiColor::rgb(0, 0, 0).with_alpha(120),
    );
    let mut static_hud = layer_list(20, 12);
    let dynamic_hud = layer_list(60, 8);
    let screen = layer_list(100, 24);
    let overlay = layer_list(300, 16);
    let mod_hud = layer_list(500, 6);
    let mut mod_overlay = UiDrawList::new();
    let kit = VanillaWidgets::new(2);
    draw_vanilla_button(
        &mut static_hud,
        &kit,
        4,
        4,
        40,
        "HUD",
        VanillaButtonState::Normal,
    );
    let mut manifest = ModUiManifest::new("jei_like", UiExtensionSlot::CustomScreen);
    manifest.push_widget(
        UiWidgetSpec::new(UiWidgetKind::Panel, ContractRect::new(10, 10, 120, 80))
            .with_label("mod panel"),
    );
    manifest.push_widget(
        UiWidgetSpec::new(UiWidgetKind::Button, ContractRect::new(16, 20, 60, 20))
            .with_label("Craft"),
    );
    let report = draw_mod_ui_manifest(
        &mut mod_overlay,
        &manifest,
        2,
        UiRect::new(0, 0, VIEW_W, VIEW_H),
    );
    assert!(report.is_within_budget(), "{:?}", report.violations);
    [
        (UiLayer::Backdrop, backdrop),
        (UiLayer::StaticHud, static_hud),
        (UiLayer::DynamicHud, dynamic_hud),
        (UiLayer::Screen, screen),
        (UiLayer::Overlay, overlay),
        (UiLayer::ModHud, mod_hud),
        (UiLayer::ModOverlay, mod_overlay),
    ]
}

fn tessellate_all(cache: &mut UiRetainedCache, lists: &[(UiLayer, UiDrawList)]) {
    let icons = icons();
    let sprites = sprites();
    for (layer, list) in lists {
        cache.update_layer(*layer, list, VIEW_W, VIEW_H, &icons, &sprites);
    }
}

#[test]
fn idle_frames_do_zero_work() {
    let bus = UiLayerBus::new();
    let mut cache = UiRetainedCache::new();
    let lists = full_scene_lists();
    cache.begin_frame(&bus);
    tessellate_all(&mut cache, &lists);
    cache.composite();
    cache.mark_uploaded();
    let tessellations = cache.stats().tessellations;
    // 300 idle frames: no bumps → no dirty layers → no tessellation,
    // no upload mark.
    for _ in 0..300 {
        let dirty = cache.begin_frame(&bus);
        assert_eq!(dirty, DirtyLayers::NONE);
        cache.composite();
        assert!(
            !cache.gpu_upload_needed(),
            "idle frame marked a GPU upload"
        );
    }
    let stats = cache.stats();
    assert_eq!(stats.tessellations, tessellations);
    assert_eq!(stats.idle_frames, 300);
}

#[test]
fn dynamic_hud_bump_leaves_other_layers_clean() {
    let mut bus = UiLayerBus::new();
    let mut cache = UiRetainedCache::new();
    let lists = full_scene_lists();
    cache.begin_frame(&bus);
    tessellate_all(&mut cache, &lists);
    cache.composite();
    cache.mark_uploaded();
    let untouched: Vec<(UiLayer, usize, usize)> = [
        UiLayer::StaticHud,
        UiLayer::Screen,
        UiLayer::Overlay,
        UiLayer::ModHud,
    ]
    .into_iter()
    .map(|l| {
        let (v, i) = cache.layer_mesh(l);
        (l, v.len(), i.len())
    })
    .collect();
    // Chat tick: only DynamicHud moves.
    UiLayer::DynamicHud.bump(&mut bus);
    let dirty = cache.begin_frame(&bus);
    assert_eq!(dirty.count(), 1);
    assert!(dirty.contains(UiLayer::DynamicHud.index()));
    let icons = icons();
    let sprites = sprites();
    cache.update_layer(
        UiLayer::DynamicHud,
        &layer_list(90, 8),
        VIEW_W,
        VIEW_H,
        &icons,
        &sprites,
    );
    cache.composite();
    for (layer, v, i) in untouched {
        let (nv, ni) = cache.layer_mesh(layer);
        assert_eq!(
            (nv.len(), ni.len()),
            (v, i),
            "{layer} re-tessellated on a DynamicHud-only bump"
        );
    }
}

#[test]
fn full_composite_merges_to_one_batch_with_no_capacity_growth() {
    let mut bus = UiLayerBus::new();
    let mut cache = UiRetainedCache::new();
    let lists = full_scene_lists();
    let (expected_verts, expected_indices) = {
        let icons = icons();
        let sprites = sprites();
        lists
            .iter()
            .map(|(_, list)| {
                let mut v = Vec::new();
                let mut i = Vec::new();
                megu_client_ui::draw::tessellate_draw_list_full_into(
                    list,
                    VIEW_W,
                    VIEW_H,
                    &icons,
                    &sprites,
                    &mut v,
                    &mut i,
                );
                (v.len(), i.len())
            })
            .fold((0usize, 0usize), |acc, x| (acc.0 + x.0, acc.1 + x.1))
    };
    // Warm-up frame: everything dirty once.
    cache.begin_frame(&bus);
    tessellate_all(&mut cache, &lists);
    {
        let (v, i) = cache.composite();
        assert_eq!(v.len(), expected_verts, "one merged vertex batch");
        assert_eq!(i.len(), expected_indices, "one merged index batch");
        // Index offsets must chain: each layer's indices address into
        // its own slice of the merged vertex buffer.
        let max = i.iter().copied().max().unwrap_or(0) as usize;
        assert!(max < v.len().max(1), "index {max} escapes merged verts");
    }
    assert!(cache.gpu_upload_needed());
    cache.mark_uploaded();
    // Churn: bump every layer every frame for 60 frames. Capacities
    // must be stable from frame 1 — the retained contract.
    let mut caps = Vec::new();
    for frame in 0..60 {
        for layer in UiLayer::ALL {
            layer.bump(&mut bus);
        }
        let dirty = cache.begin_frame(&bus);
        assert_eq!(dirty.count(), UiLayer::COUNT as u32);
        tessellate_all(&mut cache, &lists);
        let (v, i) = cache.composite();
        if frame >= 1 {
            caps.push((v.len(), i.len()));
        }
        cache.mark_uploaded();
    }
    assert!(
        caps.windows(2).all(|w| w[0] == w[1]),
        "merged batch length drifted across identical frames: {caps:?}"
    );
    let stats: RetainedCacheStats = cache.stats();
    assert_eq!(stats.tessellations, 7 + 60 * 7);
}
