// SPDX-License-Identifier: LGPL-3.0-or-later
//! Feature tests for `megu_client_ui::seamless_ux` — Lane
//! ALPHA4-UX-LIVE-SETTINGS-DND-IMPORT: one-click bilingual quality
//! presets, the ranked EN/JA settings search index, the translucent
//! live-preview settings backdrop, the universal drag-and-drop payload
//! classifier with mandatory consent prompts, and the existing-launcher
//! importer behind its hard secret-file denylist. All filesystem
//! fixtures are built under unique temp dirs — fully hermetic.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use megu_client_ui::seamless_ux::{
    DropConsent, DropPayloadClassifier, DropRejection, DroppedAssetKind, ExistingLauncherImporter,
    ImportError, LauncherSource, LivePreviewBackdrop, QualityPreset, SettingsSearchIndex,
};
use megu_client_ui::state::{
    CloudSetting, GpuBackend, LodMode, SettingKey, SettingsTab, UiLang, VideoPreset, VideoSettings,
};
use megu_core::hash::crc32_ieee;

static DIR_CTR: AtomicUsize = AtomicUsize::new(0);

fn fresh_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "megu-ux-{}-{tag}-{}",
        std::process::id(),
        DIR_CTR.fetch_add(1, Ordering::Relaxed)
    ));
    fs::create_dir_all(&dir).unwrap();
    dir
}

// ---------------------------------------------------------------------------
// Minimal pure-std ZIP writer (stored entries only) for classifier fixtures
// ---------------------------------------------------------------------------

fn w16(out: &mut Vec<u8>, v: u16) {
    out.extend_from_slice(&v.to_le_bytes());
}
fn w32(out: &mut Vec<u8>, v: u32) {
    out.extend_from_slice(&v.to_le_bytes());
}

struct Ent {
    name: String,
    stored: Vec<u8>,
    /// Declared uncompressed size in both headers (fixtures may lie).
    uncomp: u32,
    /// CD external-file-attributes field (Unix mode lives in bits 31..16).
    ext_attrs: u32,
}

fn ent(name: &str, stored: &[u8]) -> Ent {
    Ent {
        name: name.to_owned(),
        stored: stored.to_vec(),
        uncomp: stored.len() as u32,
        ext_attrs: 0,
    }
}

/// Write local headers + data + central directory + EOCD.
fn build_zip(entries: &[Ent]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut offs = Vec::new();
    for e in entries {
        offs.push(out.len() as u32);
        w32(&mut out, 0x0403_4b50);
        w16(&mut out, 20);
        w16(&mut out, 0);
        w16(&mut out, 0);
        w16(&mut out, 0);
        w16(&mut out, 0);
        w32(&mut out, 0);
        w32(&mut out, e.stored.len() as u32);
        w32(&mut out, e.uncomp);
        w16(&mut out, e.name.len() as u16);
        w16(&mut out, 0);
        out.extend_from_slice(e.name.as_bytes());
        out.extend_from_slice(&e.stored);
    }
    let cd_off = out.len() as u32;
    for (i, e) in entries.iter().enumerate() {
        w32(&mut out, 0x0201_4b50);
        w16(&mut out, 20);
        w16(&mut out, 20);
        w16(&mut out, 0);
        w16(&mut out, 0);
        w16(&mut out, 0);
        w16(&mut out, 0);
        w32(&mut out, 0);
        w32(&mut out, e.stored.len() as u32);
        w32(&mut out, e.uncomp);
        w16(&mut out, e.name.len() as u16);
        w16(&mut out, 0);
        w16(&mut out, 0);
        w16(&mut out, 0);
        w16(&mut out, 0);
        w32(&mut out, e.ext_attrs);
        w32(&mut out, offs[i]);
        out.extend_from_slice(e.name.as_bytes());
    }
    let cd_size = out.len() as u32 - cd_off;
    w32(&mut out, 0x0605_4b50);
    w16(&mut out, 0);
    w16(&mut out, 0);
    w16(&mut out, entries.len() as u16);
    w16(&mut out, entries.len() as u16);
    w32(&mut out, cd_size);
    w32(&mut out, cd_off);
    w16(&mut out, 0);
    out
}

fn write_file(dir: &Path, name: &str, bytes: &[u8]) -> PathBuf {
    let p = dir.join(name);
    fs::write(&p, bytes).unwrap();
    p
}

/// Build a minimal valid PNG (signature + one real IHDR chunk with a
/// correct CRC; no pixel data — the classifier only reads the header).
fn png_bytes(w: u32, h: u32) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]);
    out.extend_from_slice(&13u32.to_be_bytes());
    let mut ihdr = Vec::new();
    ihdr.extend_from_slice(b"IHDR");
    ihdr.extend_from_slice(&w.to_be_bytes());
    ihdr.extend_from_slice(&h.to_be_bytes());
    ihdr.extend_from_slice(&[8, 6, 0, 0, 0]); // 8-bit RGBA, deflate, std filter, no interlace
    let crc = crc32_ieee(&ihdr);
    out.extend_from_slice(&ihdr);
    out.extend_from_slice(&crc.to_be_bytes());
    out
}

// ---------------------------------------------------------------------------
// Quality presets
// ---------------------------------------------------------------------------

#[test]
fn every_preset_has_distinct_bilingual_name_and_description() {
    let mut names_en = Vec::new();
    let mut names_ja = Vec::new();
    for p in QualityPreset::ALL {
        let (en, ja) = (p.name(UiLang::En), p.name(UiLang::Ja));
        assert!(!en.is_empty() && !ja.is_empty());
        assert!(!p.description(UiLang::En).is_empty());
        assert!(!p.description(UiLang::Ja).is_empty());
        names_en.push(en);
        names_ja.push(ja);
    }
    names_en.sort_unstable();
    names_en.dedup();
    names_ja.sort_unstable();
    names_ja.dedup();
    assert_eq!(names_en.len(), 4, "EN preset names must be distinct");
    assert_eq!(names_ja.len(), 4, "JA preset names must be distinct");
}

#[test]
fn preset_specs_are_concrete_and_sensibly_ordered() {
    let ultra = QualityPreset::UltraLowLaptop.spec();
    let bal = QualityPreset::Balanced.spec();
    let cin = QualityPreset::Cinematic.spec();
    let deck = QualityPreset::SteamDeck60.spec();

    assert!(ultra.render_distance_chunks < bal.render_distance_chunks);
    assert!(bal.render_distance_chunks < cin.render_distance_chunks);
    assert!(deck.render_distance_chunks < bal.render_distance_chunks);
    assert_eq!(deck.fps_cap, 60, "Steam Deck preset locks 60 fps");
    assert_eq!(ultra.fps_cap, 30);
    assert!(!ultra.shaders_enabled && ultra.shadow_map_resolution == 0);
    assert!(cin.shaders_enabled && cin.shadow_map_resolution >= 2048);
    assert!(cin.lod == LodMode::Far && ultra.lod == LodMode::Off);
    assert!(ultra.gpu_backend == GpuBackend::ClassicLowSpec);
    assert!(deck.clouds == CloudSetting::Fast && bal.clouds == CloudSetting::Fancy);
}

#[test]
fn apply_writes_all_owned_video_fields() {
    let mut v = VideoSettings::default();
    QualityPreset::SteamDeck60.apply(&mut v);
    let s = QualityPreset::SteamDeck60.spec();
    assert_eq!(v.view_distance, s.render_distance_chunks);
    assert_eq!(v.fps_cap, 60);
    assert_eq!(v.lod, s.lod);
    assert_eq!(v.gpu_backend, s.gpu_backend);
    assert_eq!(v.clouds, s.clouds);
    assert_eq!(v.entity_shadows, s.entity_shadows);
    assert_eq!(v.preset, VideoPreset::High);
}

// ---------------------------------------------------------------------------
// Bilingual settings search index
// ---------------------------------------------------------------------------

fn index() -> SettingsSearchIndex {
    SettingsSearchIndex::all_settings()
}

#[test]
fn catalog_covers_every_setting_and_tab() {
    let idx = index();
    assert_eq!(
        idx.entries().len(),
        67,
        "one search entry per SettingKey variant"
    );
    for key in [
        SettingKey::VideoPreset,
        SettingKey::FovDeg,
        SettingKey::KeyBindings,
        SettingKey::AudioMaster,
        SettingKey::ShadersEnabled,
        SettingKey::FpMinimapMode,
        SettingKey::Language,
    ] {
        assert!(
            idx.entries().iter().any(|e| e.key == key),
            "missing catalog entry for {key:?}"
        );
    }
    for tab in SettingsTab::ALL {
        assert!(
            idx.entries().iter().any(|e| e.tab == tab),
            "no entries for tab {tab:?}"
        );
    }
}

#[test]
fn search_finds_fps_fov_distance_by_english_keyword() {
    let idx = index();
    let fps = idx.search("fps");
    assert!(
        fps.iter().any(|h| h.key == SettingKey::VideoFpsCap),
        "fps must surface the FPS cap"
    );
    let fov = idx.search("fov");
    assert_eq!(
        fov.first().map(|h| h.key),
        Some(SettingKey::FovDeg),
        "fov must rank Field of View first"
    );
    let dist = idx.search("distance");
    assert!(
        dist.iter().any(|h| h.key == SettingKey::VideoViewDistance),
        "distance must surface view distance"
    );
}

#[test]
fn search_finds_settings_by_japanese_keywords() {
    let idx = index();
    let shadow = idx.search("影");
    assert!(
        shadow.iter().any(|h| h.key == SettingKey::VideoEntityShadows)
            && shadow.iter().any(|h| h.key == SettingKey::ShadersEnabled),
        "影 must surface shadow/shader settings"
    );
    let ctrl = idx.search("コントローラ");
    assert!(
        ctrl.iter().any(|h| h.key == SettingKey::KeyBindings),
        "コントローラ must surface key bindings"
    );
    let vol = idx.search("音量");
    assert!(
        vol.iter().any(|h| h.key == SettingKey::AudioMaster)
            && vol.iter().all(|h| h.tab == SettingsTab::Audio),
        "音量 must rank audio-volume settings"
    );
    let ja_name = idx.search("視野角");
    assert_eq!(ja_name.first().map(|h| h.key), Some(SettingKey::FovDeg));
}

#[test]
fn search_ranks_exact_over_partial_and_requires_all_tokens() {
    let idx = index();
    let hits = idx.search("mouse sensitivity");
    assert_eq!(hits.first().map(|h| h.key), Some(SettingKey::MouseSensitivityPct));
    // Second token "sensitivity" appears nowhere in e.g. keybinds, so
    // unrelated entries must not leak in via the first token alone.
    assert!(hits.iter().all(|h| {
        h.name_en.to_lowercase().contains("sensitivity")
            || h.name_en.to_lowercase().contains("mouse")
            || h.name_ja.contains("感度")
            || h.name_ja.contains("マウス")
    }));
    assert!(idx.search("zzzz-not-a-setting").is_empty());
    assert!(idx.search("   ").is_empty());
    assert!(idx.search("").is_empty());
}

#[test]
fn hits_carry_their_category_tab() {
    let idx = index();
    let hits = idx.search("shader pack");
    let hit = hits
        .iter()
        .find(|h| h.key == SettingKey::ExternalShaderPack)
        .expect("shader pack query must hit the pack picker");
    assert_eq!(hit.tab, SettingsTab::Shaders);
}

// ---------------------------------------------------------------------------
// Live preview backdrop
// ---------------------------------------------------------------------------

#[test]
fn wide_window_splits_panel_and_live_viewport() {
    let l = LivePreviewBackdrop::compute(1280, 720);
    assert!(l.split);
    assert!(l.preview_alpha > 128 && l.preview_alpha < 255);
    let (p, v) = (l.settings_panel_rect, l.live_world_viewport_rect);
    assert!(p.w >= 480 && p.x == 8);
    assert!(v.w > 0, "live world viewport must keep a clear region");
    assert_eq!(v.x, p.x + p.w as i32 + 8);
    assert!(v.x + v.w as i32 <= 1280 - 8);
    // No overlap: the world viewport stays fully clear of the panel.
    assert!(p.x + p.w as i32 <= v.x);
}

#[test]
fn narrow_window_falls_back_to_translucent_full_panel() {
    let l = LivePreviewBackdrop::compute(854, 480);
    assert!(!l.split);
    assert_eq!(l.live_world_viewport_rect.w, 0);
    assert!(l.preview_alpha > 200, "world still bleeds through scrim");
    assert!(l.settings_panel_rect.w > 700);
}

#[test]
fn layout_is_deterministic_and_in_bounds() {
    for (w, h) in [(640, 360), (1024, 576), (1920, 1080), (3840, 2160)] {
        let a = LivePreviewBackdrop::compute(w, h);
        let b = LivePreviewBackdrop::compute(w, h);
        assert_eq!(a, b, "layout must be deterministic");
        for r in [a.settings_panel_rect, a.live_world_viewport_rect] {
            assert!(r.x >= 0 && r.y >= 0);
            assert!(r.x + r.w as i32 <= w as i32);
            assert!(r.y + r.h as i32 <= h as i32);
        }
    }
}

// ---------------------------------------------------------------------------
// Drop payload classifier
// ---------------------------------------------------------------------------

#[test]
fn mrpack_and_modrinth_index_are_detected() {
    let dir = fresh_dir("mrpack");
    let zip = build_zip(&[
        ent("modrinth.index.json", b"{\"game\":\"minecraft\"}"),
        ent("overrides/options.txt", b"fov:0.5"),
    ]);
    let p = write_file(&dir, "cool-pack.mrpack", &zip);
    let v = DropPayloadClassifier::classify_path(&p);
    assert_eq!(v.kind, DroppedAssetKind::Mrpack);
    let prompt = v.prompt.expect("installable drop needs a prompt");
    assert_eq!(prompt.options, DropConsent::ALL);
    assert!(!prompt.title(UiLang::En).is_empty() && !prompt.title(UiLang::Ja).is_empty());
    assert!(prompt.message(UiLang::En).contains("cool-pack.mrpack"));

    // Mislabeled extension still classifies by content.
    let p2 = write_file(&dir, "renamed.zip", &zip);
    assert_eq!(DropPayloadClassifier::classify_path(&p2).kind, DroppedAssetKind::Mrpack);

    // .mrpack without the index is not a modpack.
    let bare = build_zip(&[ent("readme.txt", b"hi")]);
    let p3 = write_file(&dir, "fake.mrpack", &bare);
    assert_eq!(DropPayloadClassifier::classify_path(&p3).kind, DroppedAssetKind::Unsupported);
}

#[test]
fn java_jar_requires_mod_marker() {
    let dir = fresh_dir("jar");
    let fabric = build_zip(&[
        ent("fabric.mod.json", b"{}"),
        ent("com/example/Mod.class", b"\xca\xfe\xba\xbe"),
    ]);
    let p = write_file(&dir, "fabricmod.jar", &fabric);
    assert_eq!(DropPayloadClassifier::classify_path(&p).kind, DroppedAssetKind::LocalJavaModJar);

    let forge = build_zip(&[ent("META-INF/mods.toml", b"modLoader=\"fml\"")]);
    let p = write_file(&dir, "forgemod.jar", &forge);
    assert_eq!(DropPayloadClassifier::classify_path(&p).kind, DroppedAssetKind::LocalJavaModJar);

    let plain = build_zip(&[ent("META-INF/MANIFEST.MF", b"Manifest-Version: 1.0")]);
    let p = write_file(&dir, "library.jar", &plain);
    let v = DropPayloadClassifier::classify_path(&p);
    assert_eq!(v.kind, DroppedAssetKind::Unsupported);
    assert!(matches!(v.rejection, Some(DropRejection::MissingMarkers)));
}

#[test]
fn wasm_mod_requires_module_magic() {
    let dir = fresh_dir("wasm");
    let good = write_file(&dir, "cool.wasm", &[0x00, 0x61, 0x73, 0x6D, 0x01, 0x00, 0x00, 0x00]);
    assert_eq!(DropPayloadClassifier::classify_path(&good).kind, DroppedAssetKind::WasmMod);
    let bad = write_file(&dir, "fake.wasm", b"not a wasm module");
    assert_eq!(DropPayloadClassifier::classify_path(&bad).kind, DroppedAssetKind::Unsupported);
}

#[test]
fn skin_png_only_accepts_64x64_or_64x32() {
    let dir = fresh_dir("skin");
    let modern = write_file(&dir, "steve.png", &png_bytes(64, 64));
    let legacy = write_file(&dir, "classic.png", &png_bytes(64, 32));
    let wrong = write_file(&dir, "hd-skin.png", &png_bytes(128, 128));
    let corrupt = write_file(&dir, "corrupt.png", b"\x89PNG\r\n\x1a\njunk");
    assert_eq!(DropPayloadClassifier::classify_path(&modern).kind, DroppedAssetKind::PlayerSkinPng);
    assert_eq!(DropPayloadClassifier::classify_path(&legacy).kind, DroppedAssetKind::PlayerSkinPng);
    for p in [wrong, corrupt] {
        let v = DropPayloadClassifier::classify_path(&p);
        assert_eq!(v.kind, DroppedAssetKind::Unsupported, "{p:?}");
        assert!(matches!(v.rejection, Some(DropRejection::BadSkinPng)));
    }
    // Tampered CRC must fail too.
    let mut tampered = png_bytes(64, 64);
    *tampered.last_mut().unwrap() ^= 0xFF;
    let p = write_file(&dir, "tampered.png", &tampered);
    assert_eq!(DropPayloadClassifier::classify_path(&p).kind, DroppedAssetKind::Unsupported);
}

#[test]
fn zip_content_markers_pick_pack_shader_and_world() {
    let dir = fresh_dir("zipkinds");
    let rp = build_zip(&[
        ent("pack.mcmeta", b"{\"pack\":{\"pack_format\":15}}"),
        ent("assets/minecraft/textures/x.png", b"..."),
    ]);
    let sp = build_zip(&[
        ent("pack.mcmeta", b"{}"),
        ent("shaders/gbuffers_terrain.vsh", b"#version 150"),
        ent("shaders/final.fsh", b"#version 150"),
    ]);
    let world = build_zip(&[
        ent("level.dat", b"\x0a"),
        ent("region/r.0.0.mca", b"chunk"),
        ent("session.lock", b""),
    ]);
    assert_eq!(
        DropPayloadClassifier::classify_path(&write_file(&dir, "pack.zip", &rp)).kind,
        DroppedAssetKind::ResourcePackZip
    );
    // shaders/ wins over pack.mcmeta — a shader pack is not a resource pack.
    assert_eq!(
        DropPayloadClassifier::classify_path(&write_file(&dir, "shaders.zip", &sp)).kind,
        DroppedAssetKind::ShaderPackZip
    );
    assert_eq!(
        DropPayloadClassifier::classify_path(&write_file(&dir, "world.zip", &world)).kind,
        DroppedAssetKind::WorldSaveDirOrZip
    );
    let empty = build_zip(&[ent("random.bin", b"??")]);
    assert_eq!(
        DropPayloadClassifier::classify_path(&write_file(&dir, "mystery.zip", &empty)).kind,
        DroppedAssetKind::Unsupported
    );
}

#[test]
fn world_save_directory_is_classified() {
    let dir = fresh_dir("save");
    let world = dir.join("MyWorld");
    fs::create_dir_all(world.join("region")).unwrap();
    fs::write(world.join("level.dat"), b"\x0a").unwrap();
    fs::write(world.join("session.lock"), b"").unwrap();
    let v = DropPayloadClassifier::classify_path(&world);
    assert_eq!(v.kind, DroppedAssetKind::WorldSaveDirOrZip);
    assert!(v.prompt.is_some());

    let plain = dir.join("JustAFolder");
    fs::create_dir_all(&plain).unwrap();
    assert_eq!(DropPayloadClassifier::classify_path(&plain).kind, DroppedAssetKind::Unsupported);
}

#[test]
fn zip_slip_and_traversal_entries_fail_closed() {
    let dir = fresh_dir("evil");
    for name in ["../escape.txt", "a/../../evil.txt", "C:\\\\win.txt", "abs//double.txt"] {
        let zip = build_zip(&[ent(name, b"x")]);
        let p = write_file(&dir, "evil.zip", &zip);
        let v = DropPayloadClassifier::classify_path(&p);
        assert_eq!(v.kind, DroppedAssetKind::Unsupported, "{name}");
        assert!(
            matches!(v.rejection, Some(DropRejection::UnsafeArchive(_))),
            "{name} must surface UnsafeArchive, got {:?}",
            v.rejection
        );
    }
}

#[test]
fn symlink_member_in_archive_is_rejected() {
    let dir = fresh_dir("lnkinzip");
    let mut e = ent("evil-link", b"/etc/passwd");
    // Unix mode S_IFLNK|0777 in the CD external-attributes high half.
    e.ext_attrs = 0xA1FF_0000;
    let zip = build_zip(&[e, ent("pack.mcmeta", b"{}"), ent("assets/x", b"y")]);
    let p = write_file(&dir, "pack.zip", &zip);
    let v = DropPayloadClassifier::classify_path(&p);
    assert_eq!(v.kind, DroppedAssetKind::Unsupported);
    assert!(matches!(v.rejection, Some(DropRejection::UnsafeArchive(_))));
}

#[test]
fn oversized_bomb_is_rejected_by_declared_size() {
    let dir = fresh_dir("bomb");
    // A tiny archive whose CD declares a 4 GiB member: the cap trips on
    // declared size, so no decompression ever happens.
    // 0xFFFF_FFFE is the largest non-ZIP64-saturated declaration — still
    // ~4 GiB over the 1 GiB single-member cap, so the cap must trip.
    let mut e = ent("pack.mcmeta", b"{}");
    e.uncomp = u32::MAX - 1;
    let zip = build_zip(&[e, ent("assets/x", b"y")]);
    let p = write_file(&dir, "bomb.zip", &zip);
    let v = DropPayloadClassifier::classify_path(&p);
    assert_eq!(v.kind, DroppedAssetKind::Unsupported);
    assert!(
        matches!(v.rejection, Some(DropRejection::ArchiveBomb { .. })),
        "got {:?}",
        v.rejection
    );

    // Each member just under the 1 GiB single cap, but ~9 GiB combined:
    // the cumulative cap must trip too.
    let mut many = ent("pack.mcmeta", b"{}");
    many.uncomp = 0x3FFF_FFFF;
    let entries: Vec<Ent> = (0..9)
        .map(|i| {
            let mut e = ent(&format!("assets/blob{i}.bin"), b"y");
            e.uncomp = 0x3FFF_FFFF;
            e
        })
        .chain(std::iter::once(many))
        .collect();
    let zip = build_zip(&entries);
    let p = write_file(&dir, "wide-bomb.zip", &zip);
    let v = DropPayloadClassifier::classify_path(&p);
    assert!(
        matches!(v.rejection, Some(DropRejection::ArchiveBomb { .. })),
        "got {:?}",
        v.rejection
    );
}

#[test]
fn not_a_zip_and_missing_paths_are_unsupported() {
    let dir = fresh_dir("notzip");
    let p = write_file(&dir, "pack.zip", b"this is not a zip file at all");
    assert_eq!(DropPayloadClassifier::classify_path(&p).kind, DroppedAssetKind::Unsupported);
    let missing = dir.join("nope.mrpack");
    let v = DropPayloadClassifier::classify_path(&missing);
    assert_eq!(v.kind, DroppedAssetKind::Unsupported);
    assert!(matches!(v.rejection, Some(DropRejection::Io(_))));
}

#[test]
fn secret_named_drop_is_never_inspected() {
    let dir = fresh_dir("secretdrop");
    // A file on the denylist must be rejected before any content read.
    let p = write_file(&dir, "accounts.json", b"{\"token\":\"x\"}");
    let v = DropPayloadClassifier::classify_path(&p);
    assert_eq!(v.kind, DroppedAssetKind::Unsupported);
    assert_eq!(v.rejection, Some(DropRejection::ForbiddenSecretName));
    let p2 = write_file(&dir, "SESSION.TOKEN", b"xyz");
    assert_eq!(
        DropPayloadClassifier::classify_path(&p2).rejection,
        Some(DropRejection::ForbiddenSecretName)
    );
}

#[cfg(unix)]
#[test]
fn symlinked_drop_path_is_never_followed() {
    let dir = fresh_dir("symlinkdrop");
    let real = write_file(&dir, "real.png", &png_bytes(64, 64));
    let link = dir.join("sneaky.png");
    std::os::unix::fs::symlink(&real, &link).unwrap();
    let v = DropPayloadClassifier::classify_path(&link);
    assert_eq!(v.kind, DroppedAssetKind::Unsupported);
    assert_eq!(v.rejection, Some(DropRejection::SymlinkPath));
}

#[cfg(unix)]
#[test]
fn save_dir_with_symlinked_child_is_rejected() {
    let dir = fresh_dir("savelink");
    let outside = fresh_dir("savelink-out");
    let world = dir.join("LinkedWorld");
    fs::create_dir_all(&world).unwrap();
    fs::write(world.join("level.dat"), b"\x0a").unwrap();
    std::os::unix::fs::symlink(&outside, world.join("region")).unwrap();
    let v = DropPayloadClassifier::classify_path(&world);
    assert_eq!(v.kind, DroppedAssetKind::Unsupported);
    assert_eq!(v.rejection, Some(DropRejection::SymlinkPath));
}

#[test]
fn extensionless_payloads_classify_by_magic_bytes() {
    let dir = fresh_dir("magic");
    let zip = build_zip(&[ent("pack.mcmeta", b"{}"), ent("assets/x", b"y")]);
    let p = write_file(&dir, "no-extension", &zip);
    assert_eq!(DropPayloadClassifier::classify_path(&p).kind, DroppedAssetKind::ResourcePackZip);
    let p2 = write_file(&dir, "blob", &png_bytes(64, 64));
    assert_eq!(DropPayloadClassifier::classify_path(&p2).kind, DroppedAssetKind::PlayerSkinPng);
}

#[test]
fn every_installable_drop_carries_mandatory_consent_prompt() {
    let dir = fresh_dir("consent");
    let cases: Vec<(PathBuf, DroppedAssetKind)> = vec![
        (
            write_file(&dir, "a.mrpack", &build_zip(&[ent("modrinth.index.json", b"{}")])),
            DroppedAssetKind::Mrpack,
        ),
        (
            write_file(&dir, "b.wasm", &[0x00, 0x61, 0x73, 0x6D, 1, 0, 0, 0]),
            DroppedAssetKind::WasmMod,
        ),
        (
            write_file(&dir, "c.jar", &build_zip(&[ent("fabric.mod.json", b"{}")])),
            DroppedAssetKind::LocalJavaModJar,
        ),
        (
            write_file(&dir, "d.zip", &build_zip(&[ent("pack.mcmeta", b"{}"), ent("assets/x", b"y")])),
            DroppedAssetKind::ResourcePackZip,
        ),
        (
            write_file(&dir, "e.png", &png_bytes(64, 64)),
            DroppedAssetKind::PlayerSkinPng,
        ),
    ];
    for (path, kind) in cases {
        let v = DropPayloadClassifier::classify_path(&path);
        assert_eq!(v.kind, kind);
        let prompt = v.prompt.unwrap_or_else(|| panic!("{kind:?} needs a consent prompt"));
        assert_eq!(prompt.options, [DropConsent::ApplyNow, DropConsent::StageForNextWorld, DropConsent::Cancel]);
        for lang in [UiLang::En, UiLang::Ja] {
            assert!(!prompt.title(lang).is_empty());
            assert!(!prompt.message(lang).is_empty());
        }
    }
    for c in DropConsent::ALL {
        assert!(!c.label(UiLang::En).is_empty() && !c.label(UiLang::Ja).is_empty());
    }
}

// ---------------------------------------------------------------------------
// Existing-launcher importer
// ---------------------------------------------------------------------------

/// Build a fake `.minecraft` tree including files that must be ignored.
fn populate_minecraft(root: &Path) {
    fs::create_dir_all(root.join("saves/Castle")).unwrap();
    fs::create_dir_all(root.join("saves/session_backup")).unwrap();
    fs::create_dir_all(root.join("resourcepacks")).unwrap();
    fs::write(root.join("servers.dat"), b"\x0a").unwrap();
    fs::write(
        root.join("options.txt"),
        "fov:0.75\ngamma:0.5\nlang:ja_jp\nkey_key.forward:key.keyboard.w\n",
    )
    .unwrap();
    fs::write(root.join("saves/Castle/level.dat"), b"\x0a").unwrap();
    fs::write(root.join("saves/session_backup/level.dat"), b"\x0a").unwrap();
    fs::write(root.join("resourcepacks/Fancy.zip"), b"PK").unwrap();
    // Planted secrets that must never surface.
    fs::write(root.join("launcher_accounts.json"), b"{}").unwrap();
    fs::write(root.join("microsoft_cached.token"), b"t").unwrap();
    fs::write(root.join("resourcepacks/session_store.zip"), b"PK").unwrap();
}

#[test]
fn vanilla_and_flatpak_profiles_are_discovered() {
    let home = fresh_dir("home");
    populate_minecraft(&home.join(".minecraft"));
    populate_minecraft(&home.join(".var/app/com.mojang.Minecraft/.minecraft"));
    let found = ExistingLauncherImporter::with_default_candidates(&home).discover();
    assert_eq!(found.len(), 2);
    let vanilla = found
        .iter()
        .find(|p| p.source == LauncherSource::VanillaDotMinecraft)
        .expect("vanilla profile");
    assert!(vanilla.servers_dat.is_some() && vanilla.options_txt.is_some());
    assert_eq!(vanilla.saves.len(), 1, "session_backup save is denylisted");
    assert!(vanilla.saves[0].ends_with("Castle"));
    assert_eq!(vanilla.resource_packs.len(), 1, "session_store.zip pack is denylisted");
    let flat = found
        .iter()
        .find(|p| p.source == LauncherSource::VanillaFlatpak)
        .expect("flatpak profile");
    assert!(flat.options_txt.is_some());
}

#[test]
fn prism_instances_and_modrinth_profiles_expand() {
    let home = fresh_dir("launchers");
    populate_minecraft(&home.join(".local/share/PrismLauncher/instances/FabPack/.minecraft"));
    populate_minecraft(&home.join(".local/share/PrismLauncher/instances/Vanilla121/minecraft"));
    populate_minecraft(&home.join(".local/share/com.modrinth.theseus/profiles/MyModded"));
    let found = ExistingLauncherImporter::with_default_candidates(&home).discover();
    let prism: Vec<_> = found
        .iter()
        .filter(|p| p.source == LauncherSource::PrismLauncher)
        .collect();
    assert_eq!(prism.len(), 2, "both Prism instances discovered");
    assert!(prism.iter().any(|p| p.root.ends_with("FabPack/.minecraft")));
    assert!(prism.iter().any(|p| p.root.ends_with("Vanilla121/minecraft")));
    let modrinth: Vec<_> = found
        .iter()
        .filter(|p| p.source == LauncherSource::ModrinthApp)
        .collect();
    assert_eq!(modrinth.len(), 1);
    assert!(modrinth[0].root.ends_with("MyModded"));
}

#[test]
fn denylist_matches_case_insensitively_at_any_depth() {
    let base = fresh_dir("deny");
    let positives = [
        "launcher_accounts.json",
        "LAUNCHER_ACCOUNTS.JSON",
        "accounts.json",
        "launcher_msa_credentials.bin",
        "MicrosoftAccountStore",
        "oauth.token",
        "private.key",
        "credentials",
        "session.lock",
        "auth_cache",
        "cert.pem",
        "bundle.PFX",
    ];
    for name in positives {
        assert!(
            ExistingLauncherImporter::is_forbidden_secret_path(&base.join(name)),
            "{name} must be denylisted"
        );
        let nested = base.join("deep/nested/dir").join(name);
        assert!(
            ExistingLauncherImporter::is_forbidden_secret_path(&nested),
            "nested {name} must be denylisted"
        );
    }
    for name in [
        "options.txt",
        "servers.dat",
        "level.dat",
        "Fancy.zip",
        "MyWorld",
        "keyboard_shortcuts.txt",
    ] {
        assert!(
            !ExistingLauncherImporter::is_forbidden_secret_path(&base.join(name)),
            "{name} must NOT be denylisted"
        );
    }
}

#[test]
fn importer_never_lists_secret_files() {
    let home = fresh_dir("nosecrets");
    let mc = home.join(".minecraft");
    populate_minecraft(&mc);
    // Extra secrets inside whitelisted directories.
    fs::write(mc.join("saves/Castle/credentials.txt"), b"no").unwrap();
    let profiles = ExistingLauncherImporter::with_default_candidates(&home).discover();
    assert_eq!(profiles.len(), 1);
    let p = &profiles[0];
    let all: Vec<PathBuf> = [
        p.servers_dat.iter().cloned().collect::<Vec<_>>(),
        p.options_txt.iter().cloned().collect(),
        p.saves.clone(),
        p.resource_packs.clone(),
        vec![p.root.clone()],
    ]
    .concat();
    for path in &all {
        assert!(
            !ExistingLauncherImporter::is_forbidden_secret_path(path),
            "forbidden path leaked into discovery: {}",
            path.display()
        );
    }
    let names: Vec<String> = all
        .iter()
        .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
        .collect();
    for bad in [
        "launcher_accounts.json",
        "microsoft_cached.token",
        "session_store.zip",
        "session_backup",
        "credentials.txt",
    ] {
        assert!(!names.iter().any(|n| n == bad), "{bad} leaked");
    }
}

#[test]
fn options_txt_parses_fov_gamma_lang_and_keybinds() {
    let dir = fresh_dir("opts");
    let p = write_file(
        &dir,
        "options.txt",
        b"# comment\nfov:0.75\ngamma:0.5\nlang:ja_jp\nkey_key.forward:key.keyboard.w\nkey_key.jump:key.keyboard.space\nrenderDistance:12\n",
    );
    let o = ExistingLauncherImporter::read_options_txt(&p).unwrap();
    assert_eq!(o.fov_deg, Some(90));
    assert_eq!(o.gamma_pct, Some(50));
    assert_eq!(o.lang.as_deref(), Some("ja_jp"));
    assert_eq!(o.keybinds.len(), 2);
    assert!(o.keybinds.iter().any(|(a, _)| a == "forward"));
    assert_eq!(o.pairs.get("renderDistance").map(String::as_str), Some("12"));
}

#[test]
fn forbidden_options_path_is_refused_before_any_read() {
    let dir = fresh_dir("forbiddenread");
    // The file exists and is perfectly readable — the denylist must
    // still refuse to open it.
    let secret = write_file(&dir, "launcher_accounts.json", b"fov:1.0\n");
    match ExistingLauncherImporter::read_options_txt(&secret) {
        Err(ImportError::ForbiddenSecret(_)) => {}
        other => panic!("expected ForbiddenSecret, got {other:?}"),
    }
    let nested = dir.join("auth_stuff");
    fs::create_dir_all(&nested).unwrap();
    let under_forbidden_dir = write_file(&nested, "options.txt", b"fov:1.0\n");
    assert!(matches!(
        ExistingLauncherImporter::read_options_txt(&under_forbidden_dir),
        Err(ImportError::ForbiddenSecret(_))
    ));
}

#[cfg(unix)]
#[test]
fn symlink_to_secret_is_refused_via_canonical_target() {
    let dir = fresh_dir("cansymlink");
    let secret = write_file(&dir, "accounts.json", b"fov:1.0\n");
    let innocent = dir.join("innocent_options.txt");
    std::os::unix::fs::symlink(&secret, &innocent).unwrap();
    assert!(ExistingLauncherImporter::is_forbidden_secret_path(&innocent));
    assert!(matches!(
        ExistingLauncherImporter::read_options_txt(&innocent),
        Err(ImportError::ForbiddenSecret(_))
    ));
}
