// SPDX-License-Identifier: LGPL-3.0-or-later
//! Focused regression coverage for independent font readiness/revisions and
//! vanilla composite loot-table semantics.

use std::path::{Path, PathBuf};

use megu_client_gpu::live_session::SessionPaths;
use megu_client_gpu::recipe_ingest::ingest_mod_jars_into_ui;
use megu_client_gpu::vanilla_font::{
    ascii_loaded, font_revision, try_load_local_minecraft_fonts_detailed, unifont_loaded,
};
use megu_ui::recipe_browser::RecipeBrowserState;

struct TempDir(PathBuf);
impl TempDir {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("megu-font-loot-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }
}
impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn w16(out: &mut Vec<u8>, v: u16) {
    out.extend_from_slice(&v.to_le_bytes());
}
fn w32(out: &mut Vec<u8>, v: u32) {
    out.extend_from_slice(&v.to_le_bytes());
}

/// Minimal stored-entry ZIP writer used by both fixtures.
fn stored_zip(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut central = Vec::new();
    for (name, data) in entries {
        let crc = megu_core::hash::crc32_ieee(data);
        let lh_off = out.len() as u32;
        w32(&mut out, 0x0403_4b50);
        w16(&mut out, 20);
        w16(&mut out, 0);
        w16(&mut out, 0);
        w16(&mut out, 0);
        w16(&mut out, 0);
        w32(&mut out, crc);
        w32(&mut out, data.len() as u32);
        w32(&mut out, data.len() as u32);
        w16(&mut out, name.len() as u16);
        w16(&mut out, 0);
        out.extend_from_slice(name.as_bytes());
        out.extend_from_slice(data);

        w32(&mut central, 0x0201_4b50);
        w16(&mut central, 20);
        w16(&mut central, 20);
        w16(&mut central, 0);
        w16(&mut central, 0);
        w16(&mut central, 0);
        w16(&mut central, 0);
        w32(&mut central, crc);
        w32(&mut central, data.len() as u32);
        w32(&mut central, data.len() as u32);
        w16(&mut central, name.len() as u16);
        w16(&mut central, 0);
        w16(&mut central, 0);
        w16(&mut central, 0);
        w16(&mut central, 0);
        w32(&mut central, 0);
        w32(&mut central, lh_off);
        central.extend_from_slice(name.as_bytes());
    }
    let cd_offset = out.len() as u32;
    out.extend_from_slice(&central);
    w32(&mut out, 0x0605_4b50);
    w16(&mut out, 0);
    w16(&mut out, 0);
    w16(&mut out, entries.len() as u16);
    w16(&mut out, entries.len() as u16);
    w32(&mut out, central.len() as u32);
    w32(&mut out, cd_offset);
    w16(&mut out, 0);
    out
}

fn png_chunk(kind: &[u8; 4], data: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    out.extend_from_slice(kind);
    out.extend_from_slice(data);
    let crc = megu_core::hash::crc32_ieee_update(megu_core::hash::crc32_ieee_update(0, kind), data);
    out.extend_from_slice(&crc.to_be_bytes());
    out
}

fn raw_rgba_png(w: u32, h: u32, rgba: &[u8]) -> Vec<u8> {
    let mut ihdr = Vec::new();
    ihdr.extend_from_slice(&w.to_be_bytes());
    ihdr.extend_from_slice(&h.to_be_bytes());
    ihdr.extend_from_slice(&[8, 6, 0, 0, 0]);
    let mut raw = Vec::new();
    for y in 0..h {
        raw.push(0);
        raw.extend_from_slice(&rgba[(y * w * 4) as usize..((y + 1) * w * 4) as usize]);
    }
    let idat = megu_core::zip_archive::zlib_compress_stored(&raw);
    let mut png = vec![137, 80, 78, 71, 13, 10, 26, 10];
    png.extend(png_chunk(b"IHDR", &ihdr));
    png.extend(png_chunk(b"IDAT", &idat));
    png.extend(png_chunk(b"IEND", &[]));
    png
}

fn fake_paths(root: &Path) -> SessionPaths {
    SessionPaths {
        game_dir: Some(root.to_path_buf()),
        assets_dir: Some(root.join("assets")),
        asset_index: Some("17".to_owned()),
        version_id: Some("1.21.1".to_owned()),
        resource_pack_dir: None,
        server_mods_dir: None,
    }
}

fn write_ascii_only_install(root: &Path) {
    let mut rgba = vec![0u8; 128 * 128 * 4];
    // Light the first row of glyph 'A' (cp 0x41).
    let cp = 0x41usize;
    let (gx, gy) = ((cp % 16) * 8, (cp / 16) * 8);
    for x in gx..gx + 8 {
        rgba[((gy * 128 + x) * 4) + 3] = 0xff;
    }
    let png = raw_rgba_png(128, 128, &rgba);
    let jar = stored_zip(&[("assets/minecraft/textures/font/ascii.png", &png)]);
    let dir = root.join("versions").join("1.21.1");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("1.21.1.jar"), jar).unwrap();
}

fn add_unifont_assets(root: &Path) {
    let hex = "30AB:FFFF80018001800180018001800180018001800180018001800180018001FFFF\n";
    let hex_zip = stored_zip(&[("unifont_all_no_pua-0.0.hex", hex.as_bytes())]);
    let hash = "0123456789abcdef0123456789abcdef01234567";
    let object_dir = root.join("assets").join("objects").join(&hash[..2]);
    std::fs::create_dir_all(&object_dir).unwrap();
    std::fs::write(object_dir.join(hash), hex_zip).unwrap();
    let index = format!(
        "{{\"objects\":{{\"minecraft/font/unifont_all_no_pua-0.0.zip\":{{\"hash\":\"{hash}\",\"size\":1}}}}}}"
    );
    let index_dir = root.join("assets").join("indexes");
    std::fs::create_dir_all(&index_dir).unwrap();
    std::fs::write(index_dir.join("17.json"), index).unwrap();
}

#[test]
fn ascii_only_load_does_not_mark_unifont_and_late_unifont_bumps_revision() {
    let tmp = TempDir::new("font-seq");
    write_ascii_only_install(&tmp.0);
    let paths = fake_paths(&tmp.0);

    let before = font_revision();
    let ascii_only = try_load_local_minecraft_fonts_detailed(&paths).expect("ascii-only load");
    assert!(ascii_only.ascii_ready());
    assert!(!ascii_only.unifont_ready());
    assert!(ascii_loaded());
    assert!(
        !unifont_loaded(),
        "ASCII-only readiness must not masquerade as Unifont readiness"
    );
    assert!(ascii_only.revision() > before);
    let after_ascii = font_revision();

    add_unifont_assets(&tmp.0);
    let with_unifont = try_load_local_minecraft_fonts_detailed(&paths).expect("unifont load");
    assert!(with_unifont.ascii_ready());
    assert!(with_unifont.unifont_ready());
    assert!(unifont_loaded());
    assert!(
        with_unifont.revision() > after_ascii,
        "late-arriving Unifont must advance the monotonic mesh-invalidation revision"
    );
}

fn chance_of(browser: &RecipeBrowserState, item: &str) -> f32 {
    browser
        .index
        .mob_drops
        .iter()
        .find(|row| row.item_id == item)
        .unwrap_or_else(|| panic!("missing row for {item}"))
        .chance
}

#[test]
fn loot_composites_preserve_group_sequence_alternatives_and_unknowns() {
    let loot = r#"{
      "pools": [
        {"rolls": 1, "entries": [{"type":"minecraft:group","children":[
          {"type":"minecraft:item","name":"test:group_a","weight":99},
          {"type":"minecraft:item","name":"test:group_b","weight":1}
        ]}]},
        {"rolls": 1, "entries": [{"type":"minecraft:sequence","children":[
          {"type":"minecraft:item","name":"test:seq_first","conditions":[{"condition":"minecraft:random_chance","chance":0.5}]},
          {"type":"minecraft:item","name":"test:seq_second"}
        ]}]},
        {"rolls": 1, "entries": [{"type":"minecraft:alternatives","children":[
          {"type":"minecraft:item","name":"test:alt_first","conditions":[{"condition":"minecraft:random_chance","chance":0.25}]},
          {"type":"minecraft:item","name":"test:alt_second"},
          {"type":"minecraft:item","name":"test:alt_shadowed"}
        ]}]},
        {"rolls": 1, "entries": [{"type":"minecraft:alternatives","children":[
          {"type":"minecraft:item","name":"test:dynamic_first","conditions":[{"condition":"minecraft:killed_by_player"}]},
          {"type":"minecraft:item","name":"test:dynamic_fallback"}
        ]}]},
        {"rolls": 1, "entries": [{"type":"minecraft:loot_table","name":"test:referenced"}]}
      ]
    }"#;
    let tmp = TempDir::new("loot");
    let jar_path = tmp.0.join("loot.jar");
    let zip = stored_zip(&[(
        "data/test/loot_table/entities/composite.json",
        loot.as_bytes(),
    )]);
    std::fs::write(&jar_path, zip).unwrap();

    let mut browser = RecipeBrowserState::default();
    let report = ingest_mod_jars_into_ui(&[jar_path], &mut browser);
    assert_eq!(report.json_failed, 0, "notes: {:?}", report.notes);

    // Group children all execute; their weights do not become a fake 99/1 split.
    assert!((chance_of(&browser, "test:group_a") - 1.0).abs() < 1e-6);
    assert!((chance_of(&browser, "test:group_b") - 1.0).abs() < 1e-6);

    // Sequence: the second row is reached exactly when the first 50% condition succeeds.
    assert!((chance_of(&browser, "test:seq_first") - 0.5).abs() < 1e-6);
    assert!((chance_of(&browser, "test:seq_second") - 0.5).abs() < 1e-6);

    // Alternatives: the fallback probability is the product of predecessor failures,
    // and the unconditional fallback shadows every later alternative.
    assert!((chance_of(&browser, "test:alt_first") - 0.25).abs() < 1e-6);
    assert!((chance_of(&browser, "test:alt_second") - 0.75).abs() < 1e-6);
    assert!(
        !browser
            .index
            .mob_drops
            .iter()
            .any(|row| row.item_id == "test:alt_shadowed"),
        "an unconditional alternative must shadow later children"
    );

    // Runtime-only branch predicates are dynamic: NaN is the honest unknown
    // representation, never a fabricated weighted percentage.
    assert!(chance_of(&browser, "test:dynamic_first").is_nan());
    assert!(chance_of(&browser, "test:dynamic_fallback").is_nan());
    assert!(
        report
            .notes
            .iter()
            .any(|note| note.contains("test:referenced") && note.contains("unknown probability")),
        "referenced loot tables must be explicitly audited as unknown: {:?}",
        report.notes
    );
}
