// SPDX-License-Identifier: LGPL-3.0-or-later

use megu_client_gpu::entity_appearance::RuntimeEntityAppearanceAtlas;
use megu_client_gpu::panorama_pass::vanilla_panorama_angles;
use megu_client_gpu::player_skins::{ResolvedPlayerSkin, SkinSource};
use megu_client_gpu::vanilla_official_extras::VanillaOfficialExtras;
use megu_client_gpu::window::{upgrade_ui_item_icon_atlas_with_3d_blocks, GpuApp};
use megu_client_gpu::GpuCliConfig;

fn png(width: u32, height: u32, color: [u8; 4]) -> Vec<u8> {
    let rgba = vec![color; (width * height) as usize].concat();
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

fn temp_dir(tag: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!("megu-visual-parity-{tag}-{}", std::process::id()))
}

fn write_minimal_jar(game_dir: &std::path::Path) {
    let version_dir = game_dir.join("versions/1.21.1");
    std::fs::create_dir_all(&version_dir).unwrap();
    let icon = png(64, 64, [80, 120, 200, 255]);
    let entries = [
        ("assets/minecraft/textures/entity/cow/cow.png", icon.as_slice()),
        ("assets/minecraft/textures/entity/player/wide/steve.png", icon.as_slice()),
        ("assets/minecraft/textures/gui/title/background/panorama_overlay.png", icon.as_slice()),
    ];
    std::fs::write(version_dir.join("1.21.1.jar"), stored_zip(&entries)).unwrap();
}

#[test]
fn runtime_load_constructs_official_extras_and_entity_appearance() {
    let game_dir = temp_dir("extras");
    write_minimal_jar(&game_dir);
    let mut app = GpuApp::new(GpuCliConfig {
        game_dir: Some(game_dir),
        version_id: Some("1.21.1".into()),
        ..GpuCliConfig::default()
    });
    app.load_vanilla_runtime_assets();
    assert!(app.vanilla_runtime_assets().is_some());
    assert!(app.vanilla_official_extras().is_some());
    let appearance = app.entity_appearance().expect("appearance atlas");
    assert!(appearance.is_dirty());
    assert_eq!(appearance.revision(), 0);
    assert_eq!(app.sync_official_builtin_pack_menu_entries(), 0);
}

#[test]
fn player_skin_upsert_advances_revision_once_per_real_change() {
    let game_dir = temp_dir("skins");
    write_minimal_jar(&game_dir);
    let mut app = GpuApp::new(GpuCliConfig {
        game_dir: Some(game_dir),
        version_id: Some("1.21.1".into()),
        ..GpuCliConfig::default()
    });
    app.load_vanilla_runtime_assets();
    let runtime = app.vanilla_runtime_assets().unwrap();
    let extras = VanillaOfficialExtras::default();
    let mut appearance = RuntimeEntityAppearanceAtlas::new(&runtime.entity_atlas, &extras);
    appearance.mark_uploaded();
    let skin = ResolvedPlayerSkin {
        uuid: [7; 16],
        skin_rgba: vec![255; 64 * 64 * 4],
        width: 64,
        height: 64,
        slim: false,
        cape_rgba: Some((46, 22, vec![200; 46 * 22 * 4])),
        source: SkinSource::Mojang,
    };
    assert!(appearance.upsert_player_skin(skin.uuid, &skin));
    assert_eq!(appearance.revision(), 1);
    assert!(appearance.is_dirty());
    appearance.mark_uploaded();
    assert!(!appearance.upsert_player_skin(skin.uuid, &skin));
    assert_eq!(appearance.revision(), 1);
    assert!(appearance.player_skin_uv(skin.uuid).is_some());
    assert!(appearance.player_cape_uv(skin.uuid).is_some());
}

#[test]
fn panorama_angles_are_continuous_radians() {
    let (yaw0, pitch0) = vanilla_panorama_angles(0.0);
    let (yaw1, pitch1) = vanilla_panorama_angles(0.5);
    assert!(yaw0.is_finite() && pitch0.is_finite());
    assert!(yaw1.is_finite() && pitch1.is_finite());
    assert!(yaw1 >= yaw0);
    assert!(pitch0.abs() < 1.0 && pitch1.abs() < 1.0);
}

#[test]
fn cube_item_icons_are_replaced_with_isometric_pixels() {
    let mut builder = megu_ui::draw::ItemIconAtlasBuilder::new();
    let mut sprite = vec![0u8; 16 * 16 * 4];
    for pixel in sprite.as_chunks_mut::<4>().0 {
        *pixel = [100, 100, 100, 255];
    }
    builder.add("minecraft:stone", 16, 16, &sprite);
    let mut atlas = builder.finish();
    let before = atlas.rgba.clone();
    let replaced = upgrade_ui_item_icon_atlas_with_3d_blocks(&mut atlas);
    assert!(replaced > 0, "stone must be recognized as a cube block");
    assert_ne!(atlas.rgba, before);
    assert!(atlas.rgba.as_chunks::<4>().0.iter().any(|p| p[3] == 0));
}
