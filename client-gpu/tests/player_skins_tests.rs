// SPDX-License-Identifier: LGPL-3.0-or-later
use std::fs;
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use megu_client_gpu::player_skins::{
    PlayerSkinCache, SkinSource, normalize_skin_to_64x64, parse_textures_property,
};

fn base64(input: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut output = String::new();
    for chunk in input.chunks(3) {
        let a = chunk[0];
        let b = *chunk.get(1).unwrap_or(&0);
        let c = *chunk.get(2).unwrap_or(&0);
        output.push(TABLE[(a >> 2) as usize] as char);
        output.push(TABLE[(((a & 3) << 4) | (b >> 4)) as usize] as char);
        output.push(if chunk.len() > 1 {
            TABLE[(((b & 15) << 2) | (c >> 6)) as usize] as char
        } else {
            '='
        });
        output.push(if chunk.len() > 2 {
            TABLE[(c & 63) as usize] as char
        } else {
            '='
        });
    }
    output
}

fn property(skin_url: &str, cape: Option<&str>, model: &str) -> String {
    let mut textures = format!(
        "{{\"textures\":{{\"SKIN\":{{\"url\":\"{skin_url}\",\"metadata\":{{\"model\":\"{model}\"}}}}"
    );
    if let Some(cape_url) = cape {
        textures.push_str(&format!(",\"CAPE\":{{\"url\":\"{cape_url}\"}}"));
    }
    textures.push_str("}}");
    base64(textures.as_bytes())
}

fn temp_dir(label: &str) -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let path = std::env::temp_dir().join(format!(
        "megu-player-skins-{label}-{}-{nonce}",
        std::process::id()
    ));
    fs::create_dir_all(&path).unwrap();
    path
}

fn await_result(cache: &mut PlayerSkinCache, uuid: [u8; 16]) {
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        cache.poll_completed();
        if cache.get_skin(uuid).is_some() {
            return;
        }
        std::thread::yield_now();
    }
    panic!("background skin request did not complete");
}

#[test]
fn parses_slim_and_classic_properties_and_upgrades_http() {
    const SKIN: &str = "http://textures.minecraft.net/texture/0123456789abcdef0123456789abcdef";
    const CAPE: &str = "https://textures.minecraft.net/texture/fedcba9876543210fedcba9876543210";
    let slim = parse_textures_property(&property(SKIN, Some(CAPE), "slim")).unwrap();
    assert!(slim.slim);
    assert_eq!(slim.skin_url, SKIN.replace("http://", "https://"));
    assert_eq!(slim.cape_url.as_deref(), Some(CAPE));
    let classic = parse_textures_property(&property(SKIN, None, "classic")).unwrap();
    assert!(!classic.slim);
    assert_eq!(classic.cape_url, None);

    let profile = format!(
        "{{\"properties\":[{{\"name\":\"textures\",\"value\":\"{}\"}}]}}",
        property(SKIN, None, "classic")
    );
    assert_eq!(
        parse_textures_property(&profile).unwrap().skin_url,
        SKIN.replace("http://", "https://")
    );
}

#[test]
fn rejects_untrusted_hosts_schemes_and_malformed_hashes() {
    for url in [
        "https://evil.example.com/texture/0123456789abcdef",
        "https://127.0.0.1/texture/0123456789abcdef",
        "file:///texture/0123456789abcdef",
        "https://textures.minecraft.net/texture/not-a-valid-hash",
        "https://textures.minecraft.net.evil.example/texture/0123456789abcdef",
    ] {
        assert!(
            parse_textures_property(&property(url, None, "classic")).is_err(),
            "accepted {url}"
        );
    }
}

#[test]
fn upgrades_legacy_layout_with_vanilla_face_mirroring() {
    let mut legacy = vec![0; 64 * 32 * 4];
    for y in 0..32usize {
        for x in 0..64usize {
            let at = (y * 64 + x) * 4;
            legacy[at..at + 4].copy_from_slice(&[x as u8, y as u8, 71, 255]);
        }
    }
    legacy[3] = 0; // transparent base-layer input must become opaque.
    let (width, height, modern) = normalize_skin_to_64x64(64, 32, &legacy).unwrap();
    assert_eq!((width, height), (64, 64));
    let pixel = |x: usize, y: usize| &modern[(y * 64 + x) * 4..(y * 64 + x) * 4 + 4];
    // Every limb rectangle is copied with a horizontal mirror. These samples
    // distinguish the Vanilla face swap from the former byte-for-byte copy.
    assert_eq!(pixel(16, 52), &[11, 20, 71, 255]);
    assert_eq!(pixel(31, 63), &[12, 31, 71, 255]);
    assert_eq!(pixel(32, 52), &[51, 20, 71, 255]);
    assert_eq!(pixel(47, 63), &[52, 31, 71, 255]);
    assert_eq!(pixel(0, 0)[3], 255);
    assert_eq!(pixel(63, 63), &[0, 0, 0, 0]);
}

#[test]
fn disk_cache_hit_round_trips_synthetic_png_without_network() {
    const URL: &str = "https://textures.minecraft.net/texture/0123456789abcdef0123456789abcdef";
    let root = temp_dir("disk");
    let mut cache = PlayerSkinCache::new(&root);
    let cache_file = cache.disk_cache_path(URL).unwrap();
    fs::create_dir_all(cache_file.parent().unwrap()).unwrap();
    let expected = synthetic_png(64, 64, [12, 34, 56, 255]);
    fs::write(cache_file, expected).unwrap();
    let uuid = [0x42; 16];
    cache.request_skin_for_uuid(uuid, Some(&property(URL, None, "slim")));
    await_result(&mut cache, uuid);
    let resolved = cache.get_skin(uuid).unwrap();
    assert_eq!((resolved.width, resolved.height), (64, 64));
    assert!(resolved.slim);
    assert_eq!(&resolved.skin_rgba[..4], &[12, 34, 56, 255]);
    assert_eq!(resolved.source, SkinSource::DiskCache);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn invalid_or_missing_property_falls_back_to_vanilla_uuid_skin() {
    let root = temp_dir("fallback");
    let mut cache = PlayerSkinCache::new(&root);
    let uuid = [0x91; 16];
    cache.request_skin_for_uuid(uuid, Some("not base64"));
    await_result(&mut cache, uuid);
    let resolved = cache.get_skin(uuid).unwrap();
    let (name, slim) = megu_client_gpu::vanilla_official_extras::default_skin_for_uuid(uuid);
    assert_eq!(resolved.source, SkinSource::VanillaDefault { name });
    assert!(resolved.skin_rgba.is_empty());
    assert_eq!(
        (resolved.width, resolved.height, resolved.slim),
        (0, 0, slim)
    );
    fs::remove_dir_all(root).unwrap();
}

fn synthetic_png(width: u32, height: u32, pixel: [u8; 4]) -> Vec<u8> {
    let mut row = Vec::with_capacity(width as usize * 4 + 1);
    row.push(0); // PNG filter: None
    for _ in 0..width {
        row.extend_from_slice(&pixel);
    }
    let raw = row.repeat(height as usize);
    let mut zlib = vec![0x78, 0x01];
    let mut remaining = raw.as_slice();
    while !remaining.is_empty() {
        let count = remaining.len().min(65_535);
        let final_block = count == remaining.len();
        zlib.push(u8::from(final_block));
        zlib.extend_from_slice(&(count as u16).to_le_bytes());
        zlib.extend_from_slice(&(!(count as u16)).to_le_bytes());
        zlib.extend_from_slice(&remaining[..count]);
        remaining = &remaining[count..];
    }
    let mut a = 1u32;
    let mut b = 0u32;
    for byte in &raw {
        a = (a + u32::from(*byte)) % 65_521;
        b = (b + a) % 65_521;
    }
    zlib.extend_from_slice(&((b << 16) | a).to_be_bytes());

    let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
    let mut ihdr = Vec::new();
    ihdr.extend_from_slice(&width.to_be_bytes());
    ihdr.extend_from_slice(&height.to_be_bytes());
    ihdr.extend_from_slice(&[8, 6, 0, 0, 0]);
    png_chunk(&mut png, b"IHDR", &ihdr);
    png_chunk(&mut png, b"IDAT", &zlib);
    png_chunk(&mut png, b"IEND", &[]);
    png
}

fn png_chunk(output: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    output.extend_from_slice(&(data.len() as u32).to_be_bytes());
    output.extend_from_slice(kind);
    output.extend_from_slice(data);
    let mut crc = 0xffff_ffffu32;
    for byte in kind.iter().chain(data) {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0xedb8_8320 & 0u32.wrapping_sub(crc & 1));
        }
    }
    output.extend_from_slice(&(!crc).to_be_bytes());
}
