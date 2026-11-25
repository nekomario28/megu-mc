// SPDX-License-Identifier: LGPL-3.0-or-later
//! Headless tests for local world-asset loading and clean-room fallbacks.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use megu_client_gpu::vanilla_world_assets::{
    moon_phase_uv_rect, sample_colormap_tint, DecodedRgbaTexture, VanillaColormap,
    VanillaEnvTexture, VanillaMiscOverlay, VanillaWorldAssets,
};

static NEXT_DIR: AtomicUsize = AtomicUsize::new(0);

fn temp_path(label: &str) -> PathBuf {
    let id = NEXT_DIR.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "megu-world-assets-{}-{label}-{id}",
        std::process::id()
    ))
}

fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = !0u32;
    for byte in bytes {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0xedb8_8320 & (0u32.wrapping_sub(crc & 1)));
        }
    }
    !crc
}

fn adler32(bytes: &[u8]) -> u32 {
    let (mut a, mut b) = (1u32, 0u32);
    for byte in bytes {
        a = (a + u32::from(*byte)) % 65_521;
        b = (b + a) % 65_521;
    }
    (b << 16) | a
}

fn png_chunk(out: &mut Vec<u8>, name: &[u8; 4], payload: &[u8]) {
    out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    out.extend_from_slice(name);
    out.extend_from_slice(payload);
    let crc = crc32(&out[out.len() - payload.len() - 4..]);
    out.extend_from_slice(&crc.to_be_bytes());
}

fn rgba_png(width: u32, height: u32, pixel: [u8; 4]) -> Vec<u8> {
    let row_len = width as usize * 4;
    let mut raw = Vec::with_capacity(height as usize * (row_len + 1));
    for _ in 0..height {
        raw.extend(std::iter::once(0)); // PNG filter: None
        for _ in 0..width {
            raw.extend_from_slice(&pixel);
        }
    }

    let mut zlib = vec![0x78, 0x01];
    let chunks = raw.chunks(65_535);
    let count = chunks.len();
    for (i, chunk) in chunks.enumerate() {
        zlib.push(if i + 1 == count { 1 } else { 0 });
        let len = chunk.len() as u16;
        zlib.extend_from_slice(&len.to_le_bytes());
        zlib.extend_from_slice(&(!len).to_le_bytes());
        zlib.extend_from_slice(chunk);
    }
    zlib.extend_from_slice(&adler32(&raw).to_be_bytes());

    let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
    let mut ihdr = Vec::with_capacity(13);
    ihdr.extend_from_slice(&width.to_be_bytes());
    ihdr.extend_from_slice(&height.to_be_bytes());
    ihdr.extend_from_slice(&[8, 6, 0, 0, 0]); // RGBA8, no interlace
    png_chunk(&mut png, b"IHDR", &ihdr);
    png_chunk(&mut png, b"IDAT", &zlib);
    png_chunk(&mut png, b"IEND", &[]);
    png
}

fn zip_archive(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut central = Vec::new();
    for (name, bytes) in entries {
        let name_bytes = name.as_bytes();
        let crc = crc32(bytes);
        let offset = out.len() as u32;
        out.extend_from_slice(&0x0403_4b50u32.to_le_bytes());
        out.extend_from_slice(&20u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&crc.to_le_bytes());
        out.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
        out.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
        out.extend_from_slice(&(name_bytes.len() as u16).to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(name_bytes);
        out.extend_from_slice(bytes);

        central.extend_from_slice(&0x0201_4b50u32.to_le_bytes());
        central.extend_from_slice(&20u16.to_le_bytes());
        central.extend_from_slice(&20u16.to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes());
        central.extend_from_slice(&crc.to_le_bytes());
        central.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
        central.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
        central.extend_from_slice(&(name_bytes.len() as u16).to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes());
        central.extend_from_slice(&0u32.to_le_bytes());
        central.extend_from_slice(&offset.to_le_bytes());
        central.extend_from_slice(name_bytes);
    }
    let central_offset = out.len() as u32;
    out.extend_from_slice(&central);
    let central_size = central.len() as u32;
    out.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
    out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
    out.extend_from_slice(&central_size.to_le_bytes());
    out.extend_from_slice(&central_offset.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out
}

fn write_zip(path: &Path, entries: &[(&str, &[u8])]) {
    fs::write(path, zip_archive(entries)).expect("write synthetic ZIP");
}

fn every_texture_is_well_formed(texture: &DecodedRgbaTexture) {
    assert!(texture.width > 0 && texture.height > 0);
    assert_eq!(
        texture.rgba.len(),
        texture.width as usize * texture.height as usize * 4
    );
}

#[test]
fn procedural_fallbacks_cover_the_entire_catalog() {
    let assets = VanillaWorldAssets::load_from_jar_and_packs(None, &[]);
    assert_eq!(assets.env_textures.len(), VanillaEnvTexture::ALL.len());
    assert_eq!(assets.colormaps.len(), VanillaColormap::ALL.len());
    assert_eq!(assets.misc_overlays.len(), VanillaMiscOverlay::ALL.len());
    for id in VanillaEnvTexture::ALL {
        let texture = assets.env_texture(id);
        every_texture_is_well_formed(texture);
        assert!(!texture.from_jar);
    }
    for id in VanillaColormap::ALL {
        let texture = assets.colormap(id);
        assert_eq!((texture.width, texture.height), (256, 256));
        every_texture_is_well_formed(texture);
        assert!(!texture.from_jar);
    }
    for id in VanillaMiscOverlay::ALL {
        let texture = assets.misc_overlay(id);
        every_texture_is_well_formed(texture);
        assert!(!texture.from_jar);
    }
    for stage in 0..10 {
        every_texture_is_well_formed(assets.destroy_stage(stage));
        assert!(!assets.destroy_stage(stage).from_jar);
    }
    assert_eq!(assets.destroy_stage(255).rgba, assets.destroy_stage(9).rgba);
}

#[test]
fn synthetic_client_jar_loads_exact_pixels_for_every_asset() {
    let zip = temp_path("client.jar");
    let png = rgba_png(2, 3, [17, 34, 51, 68]);
    let wide = rgba_png(256, 256, [91, 102, 113, 255]);
    let entries: Vec<(&str, &[u8])> = VanillaEnvTexture::ALL
        .iter()
        .map(|id| (id.path(), png.as_slice()))
        .chain(
            VanillaColormap::ALL
                .iter()
                .map(|id| (id.path(), wide.as_slice())),
        )
        .chain(
            VanillaMiscOverlay::ALL
                .iter()
                .map(|id| (id.path(), png.as_slice())),
        )
        .chain((0..10).map(|stage| {
            let path = match stage {
                0 => "assets/minecraft/textures/block/destroy_stage_0.png",
                1 => "assets/minecraft/textures/block/destroy_stage_1.png",
                2 => "assets/minecraft/textures/block/destroy_stage_2.png",
                3 => "assets/minecraft/textures/block/destroy_stage_3.png",
                4 => "assets/minecraft/textures/block/destroy_stage_4.png",
                5 => "assets/minecraft/textures/block/destroy_stage_5.png",
                6 => "assets/minecraft/textures/block/destroy_stage_6.png",
                7 => "assets/minecraft/textures/block/destroy_stage_7.png",
                8 => "assets/minecraft/textures/block/destroy_stage_8.png",
                _ => "assets/minecraft/textures/block/destroy_stage_9.png",
            };
            (path, png.as_slice())
        }))
        .collect();
    write_zip(&zip, &entries);

    let assets = VanillaWorldAssets::load_from_jar_and_packs(Some(&zip), &[]);
    for texture in assets
        .env_textures
        .values()
        .chain(assets.misc_overlays.values())
    {
        assert_eq!((texture.width, texture.height), (2, 3));
        assert_eq!(texture.rgba, [17, 34, 51, 68].repeat(6));
        assert!(texture.from_jar);
    }
    for texture in assets.colormaps.values() {
        assert_eq!(texture.rgba, [91, 102, 113, 255].repeat(256 * 256));
        assert!(texture.from_jar);
    }
    for stage in 0..10 {
        assert_eq!(assets.destroy_stage(stage).rgba, [17, 34, 51, 68].repeat(6));
        assert!(assets.destroy_stage(stage).from_jar);
    }
}

#[test]
fn directory_and_zip_packs_override_the_jar_in_order() {
    let jar = temp_path("base.jar");
    let directory = temp_path("directory-pack");
    let zip = temp_path("override.zip");
    let base = rgba_png(1, 1, [1, 2, 3, 4]);
    let middle = rgba_png(1, 1, [5, 6, 7, 8]);
    let top = rgba_png(1, 1, [9, 10, 11, 12]);
    write_zip(&jar, &[(VanillaEnvTexture::Sun.path(), &base)]);
    let file = directory.join(VanillaEnvTexture::Sun.path());
    fs::create_dir_all(file.parent().expect("texture parent")).expect("create pack");
    fs::write(file, &middle).expect("write directory override");
    write_zip(&zip, &[(VanillaEnvTexture::Sun.path(), &top)]);

    let directory_assets =
        VanillaWorldAssets::load_from_jar_and_packs(Some(&jar), std::slice::from_ref(&directory));
    let directory_sun = directory_assets.env_texture(VanillaEnvTexture::Sun);
    assert_eq!(directory_sun.rgba, [5, 6, 7, 8]);
    assert!(!directory_sun.from_jar);

    let zip_assets = VanillaWorldAssets::load_from_jar_and_packs(Some(&jar), &[zip]);
    let zip_sun = zip_assets.env_texture(VanillaEnvTexture::Sun);
    assert_eq!(zip_sun.rgba, [9, 10, 11, 12]);
    assert!(!zip_sun.from_jar);
}

#[test]
fn vanilla_biome_coordinates_and_moon_cells_are_exact() {
    let mut pixels = vec![0; 256 * 256 * 4];
    for y in 0..256usize {
        for x in 0..256usize {
            let at = (y * 256 + x) * 4;
            pixels[at..at + 4].copy_from_slice(&[x as u8, y as u8, 77, 255]);
        }
    }
    let colormap = DecodedRgbaTexture {
        width: 256,
        height: 256,
        rgba: pixels,
        from_jar: false,
    };
    for (temperature, downfall, expected) in [
        (1.0, 1.0, [0, 0, 77]),
        (0.0, 1.0, [255, 255, 77]),
        (0.5, 0.5, [127, 191, 77]),
        (2.0, -1.0, [0, 255, 77]),
    ] {
        assert_eq!(
            sample_colormap_tint(&colormap, temperature, downfall),
            expected
        );
    }
    let mut cells = Vec::new();
    for phase in 0..8 {
        let [u0, v0, u1, v1] = moon_phase_uv_rect(phase);
        assert_eq!(u1 - u0, 0.25);
        assert_eq!(v1 - v0, 0.5);
        cells.push((u0, v0));
    }
    cells.sort_by(|a, b| a.partial_cmp(b).expect("finite UVs"));
    cells.dedup();
    assert_eq!(cells.len(), 8);
}

#[test]
fn corrupt_png_and_traversal_entry_are_ignored() {
    let zip = temp_path("bad-entries.zip");
    let corrupt = b"not a PNG";
    let traversal = rgba_png(1, 1, [250, 0, 0, 255]);
    write_zip(
        &zip,
        &[
            (VanillaEnvTexture::Sun.path(), corrupt),
            (
                "../assets/minecraft/textures/environment/sun.png",
                &traversal,
            ),
        ],
    );
    let assets = VanillaWorldAssets::load_from_jar_and_packs(Some(&zip), &[]);
    assert!(!assets.env_texture(VanillaEnvTexture::Sun).from_jar);
    assert_ne!(
        assets.env_texture(VanillaEnvTexture::Sun).rgba,
        [250, 0, 0, 255]
    );
    assert!(!assets.notes.is_empty());
}
