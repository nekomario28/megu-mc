// SPDX-License-Identifier: LGPL-3.0-or-later
//! Headless coverage for the clean-room extended vanilla texture loader.

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

use megu_client_gpu::vanilla_atlas_extras::{recolor_armor_trim, VanillaAtlasExtras};

static NEXT: AtomicUsize = AtomicUsize::new(0);

struct Temp(PathBuf);
impl Temp {
    fn new() -> Self {
        let p = std::env::temp_dir().join(format!(
            "atlas-extras-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&p).expect("create fixture directory");
        Self(p)
    }
}
impl Drop for Temp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn png(color: [u8; 4]) -> Vec<u8> {
    png_pixels(1, 1, &[color])
}

fn png_pixels(width: u32, height: u32, pixels: &[[u8; 4]]) -> Vec<u8> {
    assert_eq!(pixels.len(), (width * height) as usize);
    let mut data = Vec::with_capacity((width as usize * 4 + 1) * height as usize);
    for row in pixels.chunks_exact(width as usize) {
        data.push(0);
        for pixel in row {
            data.extend_from_slice(pixel);
        }
    }
    let zlib = megu_core::zip_archive::zlib_compress_stored(&data);
    fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], bytes: &[u8]) {
        out.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
        let start = out.len();
        out.extend_from_slice(kind);
        out.extend_from_slice(bytes);
        let crc = megu_core::hash::crc32_ieee(&out[start..]);
        out.extend_from_slice(&crc.to_be_bytes());
    }
    let mut out = vec![137, 80, 78, 71, 13, 10, 26, 10];
    let mut ihdr = width.to_be_bytes().to_vec();
    ihdr.extend_from_slice(&height.to_be_bytes());
    ihdr.extend_from_slice(&[8, 6, 0, 0, 0]);
    chunk(&mut out, b"IHDR", &ihdr);
    chunk(&mut out, b"IDAT", &zlib);
    chunk(&mut out, b"IEND", &[]);
    out
}

fn zip(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut central = Vec::new();
    for (name, bytes) in entries {
        let offset = out.len() as u32;
        let crc = megu_core::hash::crc32_ieee(bytes);
        out.extend_from_slice(&0x04034b50u32.to_le_bytes());
        out.extend_from_slice(&20u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&crc.to_le_bytes());
        out.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
        out.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
        out.extend_from_slice(&(name.len() as u16).to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(name.as_bytes());
        out.extend_from_slice(bytes);
        central.extend_from_slice(&0x02014b50u32.to_le_bytes());
        central.extend_from_slice(&20u16.to_le_bytes());
        central.extend_from_slice(&20u16.to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes());
        central.extend_from_slice(&crc.to_le_bytes());
        central.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
        central.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
        central.extend_from_slice(&(name.len() as u16).to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes());
        central.extend_from_slice(&0u32.to_le_bytes());
        central.extend_from_slice(&offset.to_le_bytes());
        central.extend_from_slice(name.as_bytes());
    }
    let central_offset = out.len() as u32;
    out.extend_from_slice(&central);
    out.extend_from_slice(&0x06054b50u32.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
    out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
    out.extend_from_slice(&(central.len() as u32).to_le_bytes());
    out.extend_from_slice(&central_offset.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out
}

fn write_zip(path: &std::path::Path, entries: &[(&str, &[u8])]) {
    std::fs::write(path, zip(entries)).expect("write fixture zip");
}

#[test]
fn procedural_fallbacks_cover_all_extended_categories() {
    let a = VanillaAtlasExtras::load(None, &[]);
    assert_eq!(a.from_jar_count(), 0);
    assert!(a.fallback_count() >= 150);
    for id in [
        "speed",
        "slowness",
        "haste",
        "mining_fatigue",
        "strength",
        "instant_health",
        "instant_damage",
        "jump_boost",
        "nausea",
        "regeneration",
        "resistance",
        "fire_resistance",
        "water_breathing",
        "invisibility",
        "blindness",
        "night_vision",
        "hunger",
        "weakness",
        "poison",
        "wither",
        "health_boost",
        "absorption",
        "saturation",
        "glowing",
        "levitation",
        "luck",
        "unluck",
        "slow_falling",
        "conduit_power",
        "dolphins_grace",
        "bad_omen",
        "hero_of_the_village",
        "darkness",
        "trial_omen",
        "raid_omen",
        "wind_charged",
        "weaving",
        "oozing",
        "infested",
    ] {
        assert!(a.mob_effect(id).is_some(), "{id}");
    }
    assert!(a.painting("meditative").is_some());
    assert!(a.map_background().is_some());
    assert!(a.map_decoration("player").is_some());
    assert!(a.banner_pattern("flow").is_some());
    assert!(a.shield_pattern("guster").is_some());
    assert!(a.armor_trim("silence", false).is_some());
    assert!(a.trim_palette("amethyst").is_some());
    assert!(a.boss_bar("notched_20", true).is_some());
    for color in ["pink", "blue", "red", "green", "yellow", "purple", "white"] {
        assert!(a.boss_bar(color, false).is_some(), "{color}");
        assert!(a.boss_bar(color, true).is_some(), "{color}");
    }
    for count in ["6", "10", "12", "20"] {
        assert!(a.boss_bar(&format!("notched_{count}"), false).is_some());
        assert!(a.boss_bar(&format!("notched_{count}"), true).is_some());
    }
    assert!(a.hud_extra("hud/heart/frozen_half").is_some());
    assert!(a.hud_extra("toast/tutorial").is_some());
    assert!(a.empty_slot_sprite("shield").is_some());
}

#[test]
fn zip_assets_and_palette_recolor_are_loaded_in_memory() {
    let dir = Temp::new();
    let jar = dir.0.join("client.jar");
    let texture = png([70, 30, 20, 255]);
    let trim = png_pixels(2, 1, &[[2, 2, 2, 255], [8, 8, 8, 128]]);
    let base_palette = png_pixels(
        8,
        1,
        &[
            [1, 1, 1, 255],
            [2, 2, 2, 255],
            [3, 3, 3, 255],
            [4, 4, 4, 255],
            [5, 5, 5, 255],
            [6, 6, 6, 255],
            [7, 7, 7, 255],
            [8, 8, 8, 255],
        ],
    );
    let material_palette = png_pixels(
        8,
        1,
        &[
            [10, 10, 10, 255],
            [11, 11, 11, 255],
            [12, 12, 12, 255],
            [13, 13, 13, 255],
            [14, 14, 14, 255],
            [15, 15, 15, 255],
            [16, 16, 16, 255],
            [17, 17, 17, 255],
        ],
    );
    let entries = [
        (
            "assets/minecraft/textures/painting/back.png",
            texture.as_slice(),
        ),
        (
            "assets/minecraft/textures/mob_effect/wind_charged.png",
            texture.as_slice(),
        ),
        (
            "assets/minecraft/textures/mob_effect/raid_omen.png",
            texture.as_slice(),
        ),
        (
            "assets/minecraft/textures/map/map_background.png",
            texture.as_slice(),
        ),
        (
            "assets/minecraft/textures/map/decorations/player.png",
            texture.as_slice(),
        ),
        (
            "assets/minecraft/textures/trims/models/armor/flow.png",
            trim.as_slice(),
        ),
        (
            "assets/minecraft/textures/trims/color_palettes/trim_palette.png",
            base_palette.as_slice(),
        ),
        (
            "assets/minecraft/textures/trims/color_palettes/amethyst.png",
            material_palette.as_slice(),
        ),
        (
            "assets/minecraft/textures/entity/banner/flow.png",
            texture.as_slice(),
        ),
        (
            "assets/minecraft/textures/entity/shield/guster.png",
            texture.as_slice(),
        ),
        (
            "assets/minecraft/textures/gui/sprites/boss_bar/blue_progress.png",
            texture.as_slice(),
        ),
        (
            "assets/minecraft/textures/gui/sprites/hud/heart/frozen_half.png",
            texture.as_slice(),
        ),
        (
            "assets/minecraft/textures/gui/sprites/toast/tutorial.png",
            texture.as_slice(),
        ),
        (
            "assets/minecraft/textures/item/empty_armor_slot_shield.png",
            texture.as_slice(),
        ),
    ];
    write_zip(&jar, &entries);
    let a = VanillaAtlasExtras::load(Some(&jar), &[]);
    assert_eq!(a.from_jar_count(), entries.len());
    assert_eq!(a.mob_effect("wind_charged").unwrap().rgba[0], 70);
    assert_eq!(a.banner_pattern("flow").unwrap().rgba[0], 70);
    assert_eq!(a.shield_pattern("guster").unwrap().rgba[0], 70);
    assert_eq!(a.boss_bar("blue", true).unwrap().rgba[0], 70);
    assert_eq!(a.hud_extra("toast/tutorial").unwrap().rgba[0], 70);
    let base = &a.trim_palette("trim_palette").unwrap().rgba;
    let material = &a.trim_palette("amethyst").unwrap().rgba;
    let trim = &a.armor_trim("flow", false).unwrap().rgba;
    let result = recolor_armor_trim(trim, base, material).unwrap();
    assert_eq!(&result[0..4], &[11, 11, 11, 255]);
    assert_eq!(&result[4..8], &[17, 17, 17, 128]);
    assert!(recolor_armor_trim(trim, &[0; 8], &[0; 8]).is_none());
}

#[test]
fn later_resource_pack_overrides_jar_and_directory_pack_is_supported() {
    let dir = Temp::new();
    let jar = dir.0.join("client.jar");
    let pack = dir.0.join("pack");
    let old = png([10, 10, 10, 255]);
    let new = png([200, 10, 10, 255]);
    write_zip(
        &jar,
        &[("assets/minecraft/textures/painting/kebab.png", &old)],
    );
    let target = pack.join("assets/minecraft/textures/painting/kebab.png");
    std::fs::create_dir_all(target.parent().unwrap()).unwrap();
    std::fs::write(target, &new).unwrap();
    let a = VanillaAtlasExtras::load(Some(&jar), &[pack]);
    assert_eq!(a.painting("kebab").unwrap().rgba[0], 200);
    assert_eq!(a.from_jar_count(), 1);
}

#[test]
fn corrupt_oversized_and_traversal_entries_fail_closed() {
    let dir = Temp::new();
    let jar = dir.0.join("bad.zip");
    let corrupt = [1u8, 2, 3, 4];
    let oversized = vec![0u8; 8 * 1024 * 1024 + 1];
    write_zip(
        &jar,
        &[
            ("assets/minecraft/textures/painting/kebab.png", &corrupt),
            ("assets/minecraft/textures/mob_effect/speed.png", &oversized),
        ],
    );
    let a = VanillaAtlasExtras::load(Some(&jar), &[]);
    assert_eq!(a.from_jar_count(), 0);
    assert_eq!(a.painting("kebab").unwrap().width, 16);
    assert_eq!(a.mob_effect("speed").unwrap().width, 16);

    write_zip(
        &jar,
        &[("assets/minecraft/textures/../painting/aztec.png", &corrupt)],
    );
    let traversal = VanillaAtlasExtras::load(Some(&jar), &[]);
    assert_eq!(traversal.from_jar_count(), 0);
    assert!(traversal.painting("aztec").is_some());
}
