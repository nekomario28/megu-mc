// SPDX-License-Identifier: LGPL-3.0-or-later
//! Synthetic Vanilla container skin loading and atlas bridge coverage.
//! No Mojang assets or files are used; fixtures are in-memory ZIP archives.

use megu_client_gpu::title_branding::RgbaImage;
use megu_client_gpu::vanilla_gui_sprites::{
    load_vanilla_gui_from_jar_bytes, ALL_CONTAINER_TEXTURES, VANILLA_WORKSTATION_SPRITES,
};
use megu_ui::draw::{encode_png_rgba, VanillaContainerTextureId};

fn image(width: u32, height: u32, color: [u8; 4]) -> RgbaImage {
    let mut rgba = vec![0; width as usize * height as usize * 4];
    for pixel in rgba.as_chunks_mut::<4>().0 {
        *pixel = color;
    }
    RgbaImage {
        width,
        height,
        rgba,
    }
}

fn png(width: u32, height: u32, color: [u8; 4]) -> Vec<u8> {
    encode_png_rgba(width, height, &image(width, height, color).rgba)
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
    let cd_offset = out.len() as u32;
    out.extend_from_slice(&central);
    out.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
    out.extend_from_slice(&[0; 4]);
    out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
    out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
    out.extend_from_slice(&(central.len() as u32).to_le_bytes());
    out.extend_from_slice(&cd_offset.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out
}

fn entry_path(relative: &str) -> String {
    format!("assets/minecraft/textures/gui/{relative}")
}

fn base_archive() -> Vec<u8> {
    let mut owned = Vec::<(String, Vec<u8>)>::new();
    for id in ALL_CONTAINER_TEXTURES {
        let (width, height) = if id == VanillaContainerTextureId::Villager {
            (512, 256)
        } else {
            (256, 256)
        };
        owned.push((
            entry_path(id.path()),
            png(width, height, [31, 79, 127, 255]),
        ));
    }
    for path in VANILLA_WORKSTATION_SPRITES {
        owned.push((entry_path(path), png(7, 9, [16, 32, 48, 255])));
    }
    let borrowed: Vec<(&str, &[u8])> = owned
        .iter()
        .map(|(name, bytes)| (name.as_str(), bytes.as_slice()))
        .collect();
    stored_zip(&borrowed)
}

#[test]
fn loads_crops_and_bridges_every_container_and_workstation_texture() {
    let jar = base_archive();
    let assets = load_vanilla_gui_from_jar_bytes(&jar, &[]);
    assert_eq!(
        assets.named_textures.len(),
        ALL_CONTAINER_TEXTURES.len() + VANILLA_WORKSTATION_SPRITES.len()
    );
    for path in VANILLA_WORKSTATION_SPRITES {
        let texture = assets.named_texture(path).expect("overlay sprite loaded");
        assert_eq!((texture.width, texture.height), (7, 9), "{path}");
    }

    let atlas = assets.build_atlas();
    for id in ALL_CONTAINER_TEXTURES {
        let panel = atlas
            .named_entries
            .iter()
            .find(|(name, _)| name == id.path())
            .expect("cropped container panel packed")
            .1;
        assert_eq!(
            (panel.width, panel.height),
            id.panel_size(),
            "{}",
            id.path()
        );
    }
    let atlas_again = assets.build_full_atlas();
    assert_eq!(
        (atlas.width, atlas.height),
        (atlas_again.width, atlas_again.height)
    );
    assert_eq!(
        atlas.rgba, atlas_again.rgba,
        "atlas pixels are deterministic"
    );
    assert_eq!(atlas.entries, atlas_again.entries);
    assert_eq!(atlas.named_entries, atlas_again.named_entries);
    let ui_atlas = atlas.to_gui_sprite_atlas();
    for menu_id in 0..=24 {
        assert!(
            ui_atlas.container_panel_sprite(menu_id).is_some(),
            "menu id {menu_id} resolves"
        );
    }
    assert!(ui_atlas.container_panel_sprite(u32::MAX).is_some());
    assert_eq!(
        VanillaContainerTextureId::for_menu_id(0),
        Some(VanillaContainerTextureId::Generic54)
    );
    assert_eq!(
        VanillaContainerTextureId::for_menu_id(u32::MAX),
        Some(VanillaContainerTextureId::Inventory)
    );
    assert_eq!(VanillaContainerTextureId::for_menu_id(25), None);
}

#[test]
fn crops_villager_sheet_and_two_x_resource_pack_override_proportionally() {
    let jar = base_archive();
    let villager = png(1024, 512, [201, 44, 99, 255]);
    let hd = stored_zip(&[(
        "assets/minecraft/textures/gui/container/villager.png",
        &villager,
    )]);
    let assets = load_vanilla_gui_from_jar_bytes(&jar, &[&hd]);
    assert_eq!(
        (
            assets
                .named_texture("container/villager.png")
                .unwrap()
                .width,
            assets
                .named_texture("container/villager.png")
                .unwrap()
                .height
        ),
        (1024, 512)
    );
    assert_eq!(
        assets.named_texture("container/villager.png").unwrap().rgba[..4],
        [201, 44, 99, 255]
    );
    let atlas = assets.build_full_atlas();
    let villager_entry = atlas
        .named_entries
        .iter()
        .find(|(name, _)| name == "container/villager.png")
        .unwrap()
        .1;
    assert_eq!((villager_entry.width, villager_entry.height), (552, 332));
    let bridge = atlas.to_gui_sprite_atlas();
    let menu = bridge.container_panel_sprite(19).unwrap();
    assert_eq!((menu.width, menu.height), (552, 332));
}

#[test]
fn invalid_or_missing_backgrounds_fail_closed_and_keep_prior_layer() {
    let corrupt_only = stored_zip(&[(
        "assets/minecraft/textures/gui/container/furnace.png",
        b"not a png",
    )]);
    let missing = load_vanilla_gui_from_jar_bytes(&corrupt_only, &[]);
    assert!(missing
        .named_texture(VanillaContainerTextureId::Furnace.path())
        .is_none());
    assert!(!missing.notes.is_empty());
    assert!(missing
        .build_full_atlas()
        .to_gui_sprite_atlas()
        .container_panel_sprite(14)
        .is_none());

    let base = base_archive();
    let corrupt_override = stored_zip(&[(
        "assets/minecraft/textures/gui/container/furnace.png",
        b"bad override",
    )]);
    let layered = load_vanilla_gui_from_jar_bytes(&base, &[&corrupt_override]);
    assert!(layered
        .named_texture(VanillaContainerTextureId::Furnace.path())
        .is_some());
}
