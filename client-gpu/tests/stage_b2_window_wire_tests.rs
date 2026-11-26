// SPDX-License-Identifier: LGPL-3.0-or-later
//! Final window wiring checks for atlas-backed containers and local runtime assets.

use megu_client::protocol::{ClientPlayEvent, PlayEvent};
use megu_client_gpu::live_session::LiveSession;
use megu_client_gpu::renderer::{
    probe_adapters, request_gpu_device, GpuRenderer, OFFSCREEN_FORMAT,
};
use megu_client_gpu::GpuCliConfig;
use megu_client_mesh::RenderBackendPreference;
use megu_ui::container_screen::{menu_schema, ContainerScreenState};
use megu_ui::draw::{
    tessellate_draw_list_full, GuiSpriteAtlas, GuiSpriteUv, ItemIconAtlas, UiCommand, UiDrawList,
};

fn panel_uv(i: usize) -> GuiSpriteUv {
    let u0 = 0.05 + i as f32 * 0.01;
    GuiSpriteUv {
        uv_min: [u0, 0.1],
        uv_max: [u0 + 0.005, 0.15],
        width: 176,
        height: 166,
        nine_slice_border: [0; 4],
    }
}

#[test]
fn requested_vanilla_menus_emit_the_atlas_uv_quad_and_empty_atlas_falls_back() {
    let ids = [12, 14, 13, 8, 9, 19, u32::MAX];
    let panels: Vec<_> = ids
        .iter()
        .enumerate()
        .map(|(index, id)| (*id, panel_uv(index)))
        .collect();
    let atlas = GuiSpriteAtlas::from_packed_with_container_panels(
        256,
        256,
        vec![255; 256 * 256 * 4],
        Vec::new(),
        panels,
    );
    for (index, menu_id) in ids.into_iter().enumerate() {
        let schema = menu_schema(if menu_id == u32::MAX { 12 } else { menu_id }).unwrap();
        let screen = ContainerScreenState::new(schema, 1, 0, "test");
        let (w, h) = (480.0, 360.0);
        let panel = screen.panel_rect(w, h);
        let mut list = UiDrawList::new();
        screen.render_with_atlas(
            w,
            h,
            (menu_id == u32::MAX).then_some(menu_id),
            &atlas,
            &mut list,
        );
        let quad = list.commands.iter().find_map(|command| match command {
            UiCommand::ItemIcon { rect, item_id } if item_id.starts_with("@gui_uv:") => {
                Some((*rect, item_id.as_str()))
            }
            _ => None,
        });
        let (rect, _) = quad.expect("container panel must use the custom atlas UV quad");
        assert_eq!(rect, panel, "menu {menu_id}");

        let mesh =
            tessellate_draw_list_full(w as u32, h as u32, &list, &ItemIconAtlas::default(), &atlas);
        let textured_top_left = mesh
            .vertices
            .iter()
            .rev()
            .find(|vertex| vertex.position == [panel.x as f32, panel.y as f32])
            .expect("panel quad vertex");
        let expected = panel_uv(index);
        assert!((textured_top_left.uv[0] + 4.0 - expected.uv_min[0]).abs() < 1e-6);
        assert!((textured_top_left.uv[1] - expected.uv_min[1]).abs() < 1e-6);
    }

    let screen = ContainerScreenState::new(menu_schema(12).unwrap(), 1, 0, "fallback");
    let panel = screen.panel_rect(480.0, 360.0);
    let mut list = UiDrawList::new();
    screen.render_with_atlas(480.0, 360.0, None, &GuiSpriteAtlas::default(), &mut list);
    assert!(list.commands.iter().any(|command| matches!(command,
        UiCommand::FillRect { rect, .. } if *rect == panel)));
    assert!(!list.commands.iter().any(|command| matches!(command,
        UiCommand::ItemIcon { item_id, .. } if item_id.starts_with("@gui_uv:"))));
}

fn png(width: u32, height: u32, color: [u8; 4]) -> Vec<u8> {
    let mut rgba = vec![0; width as usize * height as usize * 4];
    for pixel in rgba.as_chunks_mut::<4>().0 {
        *pixel = color;
    }
    megu_ui::draw::encode_png_rgba(width, height, &rgba)
}

fn stored_zip(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut central = Vec::new();
    for (name, data) in entries {
        let offset = out.len() as u32;
        out.extend_from_slice(&0x0403_4b50u32.to_le_bytes());
        out.extend_from_slice(&20u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&[0; 4]);
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(&(data.len() as u32).to_le_bytes());
        out.extend_from_slice(&(data.len() as u32).to_le_bytes());
        out.extend_from_slice(&(name.len() as u16).to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(name.as_bytes());
        out.extend_from_slice(data);
        central.extend_from_slice(&0x0201_4b50u32.to_le_bytes());
        central.extend_from_slice(&20u16.to_le_bytes());
        central.extend_from_slice(&20u16.to_le_bytes());
        central.extend_from_slice(&[0; 8]);
        central.extend_from_slice(&0u32.to_le_bytes());
        central.extend_from_slice(&(data.len() as u32).to_le_bytes());
        central.extend_from_slice(&(data.len() as u32).to_le_bytes());
        central.extend_from_slice(&(name.len() as u16).to_le_bytes());
        central.extend_from_slice(&[0; 6]);
        central.extend_from_slice(&0u16.to_le_bytes());
        central.extend_from_slice(&0u32.to_le_bytes());
        central.extend_from_slice(&offset.to_le_bytes());
        central.extend_from_slice(name.as_bytes());
    }
    let offset = out.len() as u32;
    out.extend_from_slice(&central);
    out.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
    out.extend_from_slice(&[0; 4]);
    out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
    out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
    out.extend_from_slice(&(central.len() as u32).to_le_bytes());
    out.extend_from_slice(&offset.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out
}

fn temp_game_dir() -> std::path::PathBuf {
    static NEXT_TEMP_DIR: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let nonce = NEXT_TEMP_DIR.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    std::env::temp_dir().join(format!("megu-stage-b2-{}-{nonce}", std::process::id()))
}

#[test]
fn app_loads_real_runtime_bundle_translates_menu_title_and_uploads_it_headlessly() {
    let game_dir = temp_game_dir();
    let version_dir = game_dir.join("versions/1.21.1");
    std::fs::create_dir_all(&version_dir).unwrap();
    let mut owned = Vec::<(String, Vec<u8>)>::new();
    for id in megu_client_gpu::vanilla_gui_sprites::ALL_CONTAINER_TEXTURES {
        owned.push((
            format!("assets/minecraft/textures/gui/{}", id.path()),
            png(256, 256, [25, 55, 85, 255]),
        ));
    }
    owned.extend([
        (
            "assets/minecraft/textures/entity/cow/cow.png".into(),
            png(64, 32, [180, 120, 75, 255]),
        ),
        (
            "assets/minecraft/textures/particle/generic_0.png".into(),
            png(8, 8, [240, 220, 180, 255]),
        ),
        (
            "assets/minecraft/textures/environment/sun.png".into(),
            png(32, 32, [255, 220, 80, 255]),
        ),
        (
            "assets/minecraft/textures/colormap/grass.png".into(),
            png(256, 256, [90, 170, 60, 255]),
        ),
    ]);
    owned.push((
        "assets/minecraft/lang/en_us.json".into(),
        br#"{"container.crafting":"Crafting table","container.furnace":"Furnace"}"#.to_vec(),
    ));
    owned.push((
        "assets/minecraft/lang/ja_jp.json".into(),
        r#"{"container.crafting":"作業台","container.furnace":"かまど"}"#
            .as_bytes()
            .to_vec(),
    ));
    let borrowed: Vec<(&str, &[u8])> = owned
        .iter()
        .map(|(name, bytes)| (name.as_str(), bytes.as_slice()))
        .collect();
    std::fs::write(version_dir.join("1.21.1.jar"), stored_zip(&borrowed)).unwrap();

    let pack_root = game_dir.join("resourcepacks");
    std::fs::create_dir_all(&pack_root).unwrap();
    std::fs::write(
        game_dir.join("options.txt"),
        "resourcePacks:[\"file/override.zip\"]\n",
    )
    .unwrap();
    let pack_entries = [(
        "assets/minecraft/lang/en_us.json",
        br#"{"container.crafting":"Pack Crafting table"}"#.as_slice(),
    )];
    std::fs::write(pack_root.join("override.zip"), stored_zip(&pack_entries)).unwrap();

    let mut app = megu_client_gpu::window::GpuApp::new(GpuCliConfig {
        game_dir: Some(game_dir.clone()),
        version_id: Some("1.21.1".into()),
        resource_pack_dir: Some(pack_root),
        ..GpuCliConfig::default()
    });
    app.load_vanilla_runtime_assets();
    let summary = app
        .vanilla_runtime_assets_summary()
        .expect("runtime bundle");
    assert!(
        summary.gui_sprites >= 22,
        "all container sprites loaded: {summary:?}"
    );
    assert!(summary.gui_atlas_size[0] > 0 && summary.gui_atlas_size[1] > 0);
    assert!(summary.entity_atlas_size[0] > 0 && summary.entity_atlas_size[1] > 0);
    assert!(summary.particle_atlas_size[0] > 0 && summary.particle_atlas_size[1] > 0);
    assert!(summary.world_textures > 0);
    assert_eq!(app.translated_container_title(12), "Pack Crafting table");
    let assets = app.vanilla_runtime_assets().expect("retained bundle");
    assert!(assets.gui_atlas.container_panel_sprite(12).is_some());
    assert!(assets.gui_atlas.container_panel_sprite(u32::MAX).is_some());

    if !probe_adapters(RenderBackendPreference::Auto).is_empty() {
        let context = request_gpu_device(RenderBackendPreference::Auto, None, None).unwrap();
        let mut renderer = GpuRenderer::new(context, OFFSCREEN_FORMAT);
        megu_client_gpu::window::GpuApp::upload_runtime_assets_to_renderer(assets, &mut renderer);
    }
}

#[test]
fn live_mining_item_use_hotkeys_creative_sleep_book_and_credits_dispatch() {
    let mut live = LiveSession::offline_with_null_audio();
    live.begin_mining([1, 64, -2], 1);
    assert_eq!(live.active_block_break_overlay(), Some(([1, 64, -2], 0)));
    live.tick_mining(0.1, Some(([1, 64, -2], 1)));
    assert!(live.active_block_break_overlay().is_some());
    live.cancel_mining();
    assert_eq!(live.active_block_break_overlay(), None);
    live.begin_item_use(0, false);
    live.tick_item_use(0.1);
    assert!(live.first_person_viewmodel(true, (0.0, 0.0)).use_progress > 0.0);
    live.release_item_use();

    let mut app = megu_client_gpu::window::GpuApp::new(GpuCliConfig::default());
    live.set_sleeping(true);
    app.attach_live(live);
    app.open_player_inventory();
    app.set_container_hover(Some(0));
    for index in 0..=8 {
        assert!(app.handle_container_slot_hotkey(
            megu_ui::container_screen::ContainerSlotHotkey::HotbarSwap(index)
        ));
    }
    for key in [
        megu_ui::container_screen::ContainerSlotHotkey::OffhandSwap,
        megu_ui::container_screen::ContainerSlotHotkey::DropOne,
        megu_ui::container_screen::ContainerSlotHotkey::DropStack,
    ] {
        assert!(app.handle_container_slot_hotkey(key));
    }
    app.dispatch_creative_action(
        megu_ui::creative_screen::CreativeInventoryAction::SetCreativeSlot {
            slot: 36,
            item_id: 1,
            count: 64,
        },
    );
    assert!(app.live_is_sleeping());
    assert!(app.leave_bed());
    assert!(!app.live_is_sleeping());
    app.open_book_editor(0, vec!["Page one".into()]);
    app.dispatch_book_action(megu_ui::gameplay_screens::BookEditorAction::Save {
        slot: 0,
        pages: vec!["Signed page".into()],
        title: Some("Title".into()),
    });

    let table = megu_proto::ProtocolTable::v767_builtin();
    let writable_book_id = table
        .registry_id("minecraft:item", "minecraft:writable_book")
        .unwrap() as i32;
    let mut book_live = LiveSession::offline_with_null_audio();
    book_live.player_inventory.insert(
        36,
        megu_client::protocol::ItemStackComponents {
            count: 1,
            item_id: writable_book_id,
            added: Vec::new(),
            removed: Vec::new(),
        },
    );
    let mut book_app = megu_client_gpu::window::GpuApp::new(GpuCliConfig::default());
    book_app.attach_live(book_live);
    assert!(book_app.open_held_writable_book());

    let mut credit_live = LiveSession::offline_with_null_audio();
    credit_live
        .apply_event(ClientPlayEvent::Play(PlayEvent::GameEvent {
            event: 4,
            value: 1.0,
        }))
        .unwrap();
    let mut credits_app = megu_client_gpu::window::GpuApp::new(GpuCliConfig::default());
    credits_app.attach_live(credit_live);
    assert!(credits_app.activate_pending_credits());
    assert!(credits_app.credits_screen_open());
    assert!(!credits_app.activate_pending_credits());
}
