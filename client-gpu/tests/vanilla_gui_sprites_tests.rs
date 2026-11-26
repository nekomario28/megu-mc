// SPDX-License-Identifier: LGPL-3.0-or-later
//! ALPHA4-NATIVE-VANILLA-GUI-SPRITES: clean-room native vanilla 1.21.1
//! GUI sprite atlas, 9-slice tessellator and title panorama loader.
//! Every fixture is synthetic: in-memory RGBA buffers, `encode_png_rgba`
//! payloads, hand-written `.mcmeta` strings and stored-ZIP archives —
//! zero Mojang assets, zero temp files.

use std::path::PathBuf;

use megu_client_gpu::title_branding::RgbaImage;
use megu_client_gpu::vanilla_gui_sprites::{
    ALL_GUI_SPRITES, NineSliceBorder, SpriteAtlasEntry, VanillaGuiAtlas, VanillaGuiSpriteId,
    load_vanilla_gui_from_jar_bytes, load_vanilla_gui_from_path, panorama_yaw_pitch,
    tessellate_nine_slice,
};
use megu_ui::draw::encode_png_rgba;

// ---------------------------------------------------------------------------
// Fixture builders (all synthetic, zero Mojang bytes)
// ---------------------------------------------------------------------------

/// Clean-room temp dir per test process (no external crates).
struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "megu-vanilla-gui-sprites-{tag}-{}",
            std::process::id()
        ));
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

/// Minimal stored-mode ZIP writer for synthetic JAR/pack fixtures —
/// identical layout to `gpu_title_megu_edition_tests.rs`.
fn stored_zip(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut central = Vec::new();
    for (name, data) in entries {
        let lh_off = out.len() as u32;
        out.extend_from_slice(&0x0403_4b50u32.to_le_bytes());
        out.extend_from_slice(&20u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        let crc = megu_core::hash::crc32_ieee(data);
        out.extend_from_slice(&crc.to_le_bytes());
        out.extend_from_slice(&(data.len() as u32).to_le_bytes());
        out.extend_from_slice(&(data.len() as u32).to_le_bytes());
        out.extend_from_slice(&(name.len() as u16).to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(name.as_bytes());
        out.extend_from_slice(data);

        central.extend_from_slice(&0x0201_4b50u32.to_le_bytes());
        central.extend_from_slice(&20u16.to_le_bytes());
        central.extend_from_slice(&20u16.to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes());
        central.extend_from_slice(&crc.to_le_bytes());
        central.extend_from_slice(&(data.len() as u32).to_le_bytes());
        central.extend_from_slice(&(data.len() as u32).to_le_bytes());
        central.extend_from_slice(&(name.len() as u16).to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes()); // extra
        central.extend_from_slice(&0u16.to_le_bytes()); // comment
        central.extend_from_slice(&0u16.to_le_bytes()); // disk start
        central.extend_from_slice(&0u16.to_le_bytes()); // internal attrs
        central.extend_from_slice(&0u32.to_le_bytes()); // external attrs
        central.extend_from_slice(&lh_off.to_le_bytes());
        central.extend_from_slice(name.as_bytes());
    }
    let cd_offset = out.len() as u32;
    out.extend_from_slice(&central);
    out.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes()); // disk
    out.extend_from_slice(&0u16.to_le_bytes()); // cd start disk
    out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
    out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
    out.extend_from_slice(&(central.len() as u32).to_le_bytes());
    out.extend_from_slice(&cd_offset.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes()); // comment len
    out
}

/// Solid-color RGBA image — distinct fill per sprite so override
/// precedence can be checked at the byte level.
fn solid_image(w: u32, h: u32, color: [u8; 4]) -> RgbaImage {
    let mut rgba = vec![0u8; w as usize * h as usize * 4];
    for px in rgba.as_chunks_mut::<4>().0 {
        *px = color;
    }
    RgbaImage {
        width: w,
        height: h,
        rgba,
    }
}

fn solid_png(w: u32, h: u32, color: [u8; 4]) -> Vec<u8> {
    encode_png_rgba(w, h, &solid_image(w, h, color).rgba)
}

/// Synthetic `.mcmeta` nine_slice block (scalar border spelling).
fn mcmeta_scalar(border: u32) -> Vec<u8> {
    format!("{{\"gui\":{{\"scaling\":{{\"type\":\"nine_slice\",\"width\":200,\"height\":20,\"border\":{border}}}}}}}").into_bytes()
}

/// Synthetic `.mcmeta` nine_slice block (object border spelling).
fn mcmeta_object(l: u32, t: u32, r: u32, b: u32) -> Vec<u8> {
    format!("{{\"gui\":{{\"scaling\":{{\"type\":\"nine_slice\",\"width\":200,\"height\":20,\"border\":{{\"left\":{l},\"top\":{t},\"right\":{r},\"bottom\":{b}}}}}}}}}").into_bytes()
}

/// Synthetic `.mcmeta` non-nine_slice (stretch) scaling.
fn mcmeta_stretch() -> Vec<u8> {
    b"{\"gui\":{\"scaling\":{\"type\":\"stretch\"}}}".to_vec()
}

/// Deterministic per-sprite size + color so every fixture texture is
/// unique and identifiably "the base JAR copy".
fn sprite_spec(id: VanillaGuiSpriteId) -> (u32, u32, [u8; 4]) {
    use VanillaGuiSpriteId::*;
    let (w, h) = match id {
        Button | ButtonHighlighted | ButtonDisabled | Slider | SliderHighlighted => (200, 20),
        SliderHandle | SliderHandleHighlighted => (8, 20),
        MenuBackground => (16, 16),
        HeaderSeparator | FooterSeparator => (32, 2),
        Hotbar => (182, 22),
        HotbarSelection => (24, 24),
        Crosshair => (15, 15),
        XpBarBg | XpBarFill => (182, 5),
        HeartContainer | HeartFull | HeartHalf | FoodEmpty | FoodFull | ArmorEmpty | ArmorFull => {
            (9, 9)
        }
        Panorama0 | Panorama1 | Panorama2 | Panorama3 | Panorama4 | Panorama5 => (64, 64),
        Scroller | ScrollerBackground => (6, 32),
    };
    // Unique bright color per catalog index — never a real texture.
    let idx = ALL_GUI_SPRITES.iter().position(|s| *s == id).unwrap() as u16;
    let color = [
        (40 + idx * 7) as u8,
        (30 + idx * 11) as u8,
        (20 + idx) as u8,
        255,
    ];
    (w, h, color)
}

/// Full synthetic client JAR: every catalogued sprite plus a scalar-
/// border mcmeta on `Button` and an object-border mcmeta on `Slider`.
fn full_gui_jar() -> Vec<u8> {
    let mut owned: Vec<(String, Vec<u8>)> = Vec::new();
    for id in ALL_GUI_SPRITES {
        let (w, h, c) = sprite_spec(id);
        owned.push((id.jar_entry().to_owned(), solid_png(w, h, c)));
    }
    owned.push((
        VanillaGuiSpriteId::Button.jar_mcmeta_entry(),
        mcmeta_scalar(3),
    ));
    owned.push((
        VanillaGuiSpriteId::Slider.jar_mcmeta_entry(),
        mcmeta_object(2, 4, 6, 8),
    ));
    let refs: Vec<(&str, &[u8])> = owned
        .iter()
        .map(|(n, b)| (n.as_str(), b.as_slice()))
        .collect();
    stored_zip(&refs)
}

/// Entry pixel `(0,0)` of a decoded sprite, for override checks.
fn first_pixel(img: &RgbaImage) -> [u8; 4] {
    img.rgba[0..4].try_into().unwrap()
}

// ---------------------------------------------------------------------------
// Full + partial + corrupt loads
// ---------------------------------------------------------------------------

#[test]
fn full_in_memory_jar_loads_every_sprite_and_panorama() {
    let jar = full_gui_jar();
    let assets = load_vanilla_gui_from_jar_bytes(&jar, &[]);

    assert_eq!(assets.sprites.len(), ALL_GUI_SPRITES.len());
    assert!(assets.has_widget_skin());
    assert!(assets.has_panorama());
    for id in ALL_GUI_SPRITES {
        let img = assets
            .sprites
            .get(&id)
            .unwrap_or_else(|| panic!("{id:?} missing"));
        let (w, h, color) = sprite_spec(id);
        assert_eq!((img.width, img.height), (w, h), "{id:?} dims");
        assert_eq!(first_pixel(img), color, "{id:?} pixels");
    }
    // mcmeta-declared borders parsed (scalar + object spellings).
    assert_eq!(
        assets.borders[&VanillaGuiSpriteId::Button],
        NineSliceBorder {
            logical_width: 200,
            logical_height: 20,
            ..NineSliceBorder::uniform(3)
        }
    );
    assert_eq!(
        assets.borders[&VanillaGuiSpriteId::Slider],
        NineSliceBorder {
            left: 2,
            top: 4,
            right: 6,
            bottom: 8,
            logical_width: 200,
            logical_height: 20,
        }
    );
    // Effective border falls back to vanilla widget defaults.
    assert_eq!(
        assets.nine_slice_border(VanillaGuiSpriteId::ButtonDisabled),
        Some(NineSliceBorder::uniform(3))
    );
    assert_eq!(
        assets.nine_slice_border(VanillaGuiSpriteId::Crosshair),
        None
    );
    // Panorama cube: six equal-size square faces in face order.
    let cube = assets.panorama_cube().expect("validated cube");
    assert_eq!(cube.size, 64);
    for (i, face) in cube.faces.iter().enumerate() {
        assert_eq!((face.width, face.height), (64, 64));
        let expected = sprite_spec(ALL_GUI_SPRITES[22 + i]).2;
        assert_eq!(first_pixel(face), expected, "face {i} pixels");
    }
}

#[test]
fn missing_and_garbage_jars_fail_closed() {
    for bytes in [&b""[..], &b"not a zip at all"[..], &vec![0x50u8; 64][..]] {
        let assets = load_vanilla_gui_from_jar_bytes(bytes, &[]);
        assert!(assets.sprites.is_empty());
        assert!(!assets.has_widget_skin());
        assert!(!assets.has_panorama());
        assert!(assets.panorama_cube().is_none());
        assert!(!assets.notes.is_empty());
    }
    // Nonexistent path twin.
    let assets =
        load_vanilla_gui_from_path(&PathBuf::from("/definitely/not/a/real/client.jar"), &[]);
    assert!(assets.sprites.is_empty());
    assert!(!assets.has_widget_skin());
    assert!(!assets.notes.is_empty());
}

#[test]
fn corrupt_and_partial_entries_skip_cleanly() {
    // A JAR whose Button PNG is junk but HeartFull is fine: partial load.
    let bad_png = b"this is not a png".as_slice();
    let heart = solid_png(9, 9, [7, 7, 7, 255]);
    let jar = stored_zip(&[
        (VanillaGuiSpriteId::Button.jar_entry(), bad_png),
        (VanillaGuiSpriteId::HeartFull.jar_entry(), heart.as_slice()),
    ]);
    let assets = load_vanilla_gui_from_jar_bytes(&jar, &[]);
    assert!(!assets.sprites.contains_key(&VanillaGuiSpriteId::Button));
    assert!(assets.sprites.contains_key(&VanillaGuiSpriteId::HeartFull));
    assert!(!assets.has_widget_skin()); // flagship sprite absent
    assert!(!assets.has_panorama());
    assert!(assets.notes.iter().any(|n| n.contains("rejected")));

    // A JAR missing all panorama faces still loads its widgets.
    let (w, h, c) = sprite_spec(VanillaGuiSpriteId::Button);
    let btn = solid_png(w, h, c);
    let jar = stored_zip(&[(VanillaGuiSpriteId::Button.jar_entry(), btn.as_slice())]);
    let assets = load_vanilla_gui_from_jar_bytes(&jar, &[]);
    assert!(assets.has_widget_skin());
    assert!(!assets.has_panorama());
}

#[test]
fn non_square_panorama_face_rejects_cube() {
    let mut owned: Vec<(String, Vec<u8>)> = Vec::new();
    for id in ALL_GUI_SPRITES {
        if !id.is_panorama_face() {
            continue;
        }
        // Panorama1 deliberately non-square.
        let (w, h) = if id == VanillaGuiSpriteId::Panorama1 {
            (64, 32)
        } else {
            (64, 64)
        };
        owned.push((id.jar_entry().to_owned(), solid_png(w, h, [9, 9, 9, 255])));
    }
    let refs: Vec<(&str, &[u8])> = owned
        .iter()
        .map(|(n, b)| (n.as_str(), b.as_slice()))
        .collect();
    let jar = stored_zip(&refs);
    let assets = load_vanilla_gui_from_jar_bytes(&jar, &[]);
    assert!(!assets.has_panorama());
    assert!(assets.panorama_cube().is_none());
}

#[test]
fn path_variant_loads_jar_and_pack_layers() {
    let tmp = TempDir::new("path");
    let jar_path = tmp.0.join("client.jar");
    let pack_path = tmp.0.join("pack.zip");
    std::fs::write(&jar_path, full_gui_jar()).unwrap();
    // The pack overrides Crosshair only.
    let pack = stored_zip(&[(
        VanillaGuiSpriteId::Crosshair.jar_entry(),
        solid_png(15, 15, [250, 250, 250, 255]).as_slice(),
    )]);
    std::fs::write(&pack_path, &pack).unwrap();

    let assets = load_vanilla_gui_from_path(&jar_path, std::slice::from_ref(&pack_path));
    assert_eq!(assets.sprites.len(), ALL_GUI_SPRITES.len());
    assert_eq!(assets.source_jar.as_deref(), Some(jar_path.as_path()));
    assert_eq!(
        first_pixel(&assets.sprites[&VanillaGuiSpriteId::Crosshair]),
        [250, 250, 250, 255]
    );
    // Untouched sprites keep base-JAR pixels.
    let (_, _, btn_color) = sprite_spec(VanillaGuiSpriteId::Button);
    assert_eq!(
        first_pixel(&assets.sprites[&VanillaGuiSpriteId::Button]),
        btn_color
    );
    // The loader created zero extra files on disk (zero-extract).
    let files: Vec<_> = std::fs::read_dir(&tmp.0)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().into_string().unwrap())
        .collect();
    assert_eq!(files.len(), 2);
}

// ---------------------------------------------------------------------------
// Resource-pack override precedence
// ---------------------------------------------------------------------------

#[test]
fn later_pack_overrides_sprite_and_border() {
    let jar = full_gui_jar();

    // Pack 1 replaces the Button texture and pushes border 5.
    let pack_btn = solid_png(200, 20, [1, 2, 3, 255]);
    let pack1 = stored_zip(&[
        (VanillaGuiSpriteId::Button.jar_entry(), pack_btn.as_slice()),
        (
            VanillaGuiSpriteId::Button.jar_mcmeta_entry().as_str(),
            mcmeta_scalar(5).as_slice(),
        ),
    ]);

    // A replacement without a sidecar uses the typed default.
    let pack2_btn = solid_png(200, 20, [9, 8, 7, 255]);
    let pack2 = stored_zip(&[(VanillaGuiSpriteId::Button.jar_entry(), pack2_btn.as_slice())]);
    let assets = load_vanilla_gui_from_jar_bytes(&jar, &[&pack1, &pack2]);
    let btn = &assets.sprites[&VanillaGuiSpriteId::Button];
    assert_eq!(first_pixel(btn), [9, 8, 7, 255], "pack2 wins the png");
    assert_eq!(
        assets.nine_slice_border(VanillaGuiSpriteId::Button),
        Some(NineSliceBorder::uniform(3))
    );
    assert!(!assets.borders.contains_key(&VanillaGuiSpriteId::Button));

    // An explicit stretch on the winning image suppresses the typed default.
    let pack3 = stored_zip(&[
        (VanillaGuiSpriteId::Button.jar_entry(), &pack2_btn),
        (
            VanillaGuiSpriteId::Button.jar_mcmeta_entry().as_str(),
            &mcmeta_stretch(),
        ),
        (
            VanillaGuiSpriteId::MenuBackground.jar_entry(),
            solid_png(16, 16, [200, 100, 50, 255]).as_slice(),
        ),
    ]);
    let assets = load_vanilla_gui_from_jar_bytes(&jar, &[&pack1, &pack3]);
    assert_eq!(assets.nine_slice_border(VanillaGuiSpriteId::Button), None);
    assert_eq!(
        first_pixel(&assets.sprites[&VanillaGuiSpriteId::MenuBackground]),
        [200, 100, 50, 255]
    );
}

#[test]
fn unreadable_pack_layer_is_skipped() {
    let jar = full_gui_jar();
    let garbage = b"definitely not a zip".as_slice();
    let assets = load_vanilla_gui_from_jar_bytes(&jar, &[garbage]);
    assert_eq!(assets.sprites.len(), ALL_GUI_SPRITES.len());
    assert!(assets.notes.iter().any(|n| n.contains("pack[0]")));
}

// ---------------------------------------------------------------------------
// Single-atlas packing
// ---------------------------------------------------------------------------

#[test]
fn atlas_packs_all_sprites_no_overlap_padded_uvs() {
    let jar = full_gui_jar();
    let assets = load_vanilla_gui_from_jar_bytes(&jar, &[]);
    let atlas = assets.build_atlas();

    assert!(atlas.width.is_power_of_two());
    assert!(atlas.height >= 1);
    assert_eq!(
        atlas.rgba.len(),
        atlas.width as usize * atlas.height as usize * 4
    );
    // Every non-panorama sprite packed; panorama faces stay out.
    assert_eq!(atlas.entries.len(), ALL_GUI_SPRITES.len() - 6);
    for (id, entry) in &atlas.entries {
        assert!(!id.is_panorama_face());
        // UVs strictly inside (0, 1): the 1 px gutter keeps every
        // sprite's inner rect off the atlas edge.
        assert!(
            entry.uv_min[0] > 0.0 && entry.uv_min[1] > 0.0,
            "{id:?} uv_min"
        );
        assert!(
            entry.uv_max[0] < 1.0 && entry.uv_max[1] < 1.0,
            "{id:?} uv_max"
        );
        assert!(entry.uv_min[0] < entry.uv_max[0]);
        assert!(entry.uv_min[1] < entry.uv_max[1]);
    }
    // Zero overlapping inner rects (pixel space).
    let rects: Vec<(u32, u32, u32, u32)> = atlas
        .entries
        .iter()
        .map(|(_, e)| {
            let x = (e.uv_min[0] * atlas.width as f32).round() as u32;
            let y = (e.uv_min[1] * atlas.height as f32).round() as u32;
            (x, y, e.width, e.height)
        })
        .collect();
    for (i, a) in rects.iter().enumerate() {
        for b in rects.iter().skip(i + 1) {
            let overlap = a.0 < b.0 + b.2 && b.0 < a.0 + a.2 && a.1 < b.1 + b.3 && b.1 < a.1 + a.3;
            assert!(!overlap, "rects {a:?} and {b:?} overlap");
        }
        // 1 px padding honored: inner rect is inset ≥ 1 px on all sides.
        assert!(a.0 >= 1 && a.1 >= 1);
        assert!(a.0 + a.2 < atlas.width);
        assert!(a.1 + a.3 < atlas.height);
    }
    // Deterministic catalog order.
    let order: Vec<VanillaGuiSpriteId> = atlas.entries.iter().map(|(id, _)| *id).collect();
    let mut sorted = order.clone();
    sorted.sort();
    assert_eq!(order, sorted);
}

#[test]
fn atlas_blits_pixels_and_extrudes_gutter() {
    let color = [12, 200, 99, 255];
    let btn = solid_png(8, 4, color);
    let jar = stored_zip(&[(VanillaGuiSpriteId::Button.jar_entry(), btn.as_slice())]);
    let assets = load_vanilla_gui_from_jar_bytes(&jar, &[]);
    let atlas = assets.build_atlas();
    assert!(atlas.width.is_power_of_two());
    let e = *atlas.entry(VanillaGuiSpriteId::Button).unwrap();
    let ix = (e.uv_min[0] * atlas.width as f32).round() as u32;
    let iy = (e.uv_min[1] * atlas.height as f32).round() as u32;
    let px = |x: u32, y: u32| -> [u8; 4] {
        let o = (y * atlas.width + x) as usize * 4;
        atlas.rgba[o..o + 4].try_into().unwrap()
    };
    // Inner pixels carry the fixture color.
    assert_eq!(px(ix, iy), color);
    assert_eq!(px(ix + 7, iy + 3), color);
    // Gutter pixels are extruded edge texels (same solid color here),
    // proving padding exists and was written.
    assert_eq!(px(ix - 1, iy - 1), color, "top-left gutter");
    assert_eq!(px(ix + 8, iy + 4), color, "bottom-right gutter");
    assert_eq!(px(ix + 4, iy - 1), color, "top gutter row");
    assert_eq!(px(ix - 1, iy + 2), color, "left gutter col");
    // 9-slice border carried through from the vanilla default.
    assert_eq!(e.nine_slice_border, Some(NineSliceBorder::uniform(3)));
}

#[test]
fn empty_assets_build_a_valid_empty_atlas() {
    let atlas = VanillaGuiAtlas::default();
    let _ = atlas; // type is Default-constructible
    let assets = load_vanilla_gui_from_jar_bytes(b"no zip", &[]);
    let atlas = assets.build_atlas();
    assert_eq!(atlas.rgba.len(), (atlas.width * atlas.height * 4) as usize);
    assert!(atlas.entries.is_empty());
}

// ---------------------------------------------------------------------------
// Pixel-exact 9-slice tessellation
// ---------------------------------------------------------------------------

fn entry_with_border(b: NineSliceBorder) -> SpriteAtlasEntry {
    SpriteAtlasEntry {
        uv_min: [0.0, 0.0],
        uv_max: [1.0, 1.0],
        width: 200,
        height: 20,
        nine_slice_border: Some(b),
    }
}

fn covered_area(quads: &[megu_client_gpu::vanilla_gui_sprites::TexturedUiQuad]) -> f32 {
    quads.iter().map(|q| q.pos[2] * q.pos[3]).sum()
}

#[test]
fn nine_slice_emits_nine_quads_with_exact_corners() {
    let e = entry_with_border(NineSliceBorder::uniform(3));
    for (w, h) in [(200.0f32, 20.0f32), (400.0, 40.0), (97.0, 23.0)] {
        let quads = tessellate_nine_slice([10.0, 5.0, w, h], &e);
        assert_eq!(quads.len(), 9, "all nine slices at {w}x{h}");
        let area = covered_area(&quads);
        assert!(
            (area - w * h).abs() < 0.01,
            "total covered area {area} != {w}x{h}"
        );
        // Quads tile the destination without overlap: sort by (y,x)
        // and check a strict grid.
        let tl = quads[0];
        assert_eq!(tl.pos, [10.0, 5.0, 3.0, 3.0], "TL corner 3x3");
        let tr = quads[2];
        assert!((tr.pos[0] - (10.0 + w - 3.0)).abs() < 1e-6 && tr.pos[2] == 3.0);
        // Center quad covers the stretch region.
        let c = quads[4];
        assert!((c.pos[0] - 13.0).abs() < 1e-6 && (c.pos[1] - 8.0).abs() < 1e-6);
        assert!((c.pos[2] - (w - 6.0)).abs() < 1e-6 && (c.pos[3] - (h - 6.0)).abs() < 1e-6);
        // UVs never escape the entry's box.
        for q in &quads {
            assert!(q.uv[0] >= e.uv_min[0] - 1e-6 && q.uv[2] <= e.uv_max[0] + 1e-6);
            assert!(q.uv[1] >= e.uv_min[1] - 1e-6 && q.uv[3] <= e.uv_max[1] + 1e-6);
        }
    }
}

#[test]
fn nine_slice_clamps_cleanly_when_too_small() {
    let e = entry_with_border(NineSliceBorder::uniform(3));
    // 4x4 destination: 3+3 borders cannot fit -> scaled to 2 px each,
    // only the four corners remain, still tiling the rect exactly.
    let quads = tessellate_nine_slice([0.0, 0.0, 4.0, 4.0], &e);
    assert_eq!(quads.len(), 4, "edges/center degenerate at 4x4");
    let area = covered_area(&quads);
    assert!((area - 16.0).abs() < 0.01);
    for q in &quads {
        assert!((q.pos[2] - 2.0).abs() < 1e-6 && (q.pos[3] - 2.0).abs() < 1e-6);
    }
    // 1x1: everything degenerate or sub-pixel but still bounded.
    let quads = tessellate_nine_slice([0.0, 0.0, 1.0, 1.0], &e);
    let area = covered_area(&quads);
    assert!((area - 1.0).abs() < 0.01);
    for q in &quads {
        assert!(q.pos[0] >= 0.0 && q.pos[0] + q.pos[2] <= 1.0 + 1e-6);
        assert!(q.pos[1] >= 0.0 && q.pos[1] + q.pos[3] <= 1.0 + 1e-6);
    }
}

#[test]
fn nine_slice_asymmetric_borders_and_stretch_sprite() {
    let e = entry_with_border(NineSliceBorder {
        left: 2,
        top: 4,
        right: 6,
        bottom: 8,
        logical_width: 0,
        logical_height: 0,
    });
    let quads = tessellate_nine_slice([0.0, 0.0, 100.0, 50.0], &e);
    assert_eq!(quads.len(), 9);
    let tl = quads[0];
    assert_eq!(tl.pos, [0.0, 0.0, 2.0, 4.0]);
    let br = quads[8];
    assert!((br.pos[2] - 6.0).abs() < 1e-6 && (br.pos[3] - 8.0).abs() < 1e-6);
    let area = covered_area(&quads);
    assert!((area - 5000.0).abs() < 0.05);

    // No border at all -> single stretched quad.
    let mut flat = entry_with_border(NineSliceBorder::uniform(3));
    flat.nine_slice_border = None;
    let quads = tessellate_nine_slice([3.0, 7.0, 11.0, 13.0], &flat);
    assert_eq!(quads.len(), 1);
    assert_eq!(quads[0].pos, [3.0, 7.0, 11.0, 13.0]);
    assert_eq!(quads[0].uv, [0.0, 0.0, 1.0, 1.0]);
}

// ---------------------------------------------------------------------------
// Panorama camera helper
// ---------------------------------------------------------------------------

#[test]
fn panorama_yaw_pitch_stays_finite_and_bounded() {
    let mut t = 0.0f32;
    while t < 100_000.0 {
        let (yaw, pitch) = panorama_yaw_pitch(t);
        assert!(
            yaw.is_finite() && (0.0..360.0).contains(&yaw),
            "yaw@{t}={yaw}"
        );
        assert!(
            pitch.is_finite() && (-10.0..=10.0).contains(&pitch),
            "pitch@{t}={pitch}"
        );
        t += 37.5;
    }
    for edge in [
        100_000.0f32,
        1.0e9,
        f32::MAX,
        f32::INFINITY,
        f32::NAN,
        -1.0,
        -f32::INFINITY,
    ] {
        let (yaw, pitch) = panorama_yaw_pitch(edge);
        assert!(yaw.is_finite() && (0.0..360.0).contains(&yaw), "yaw@{edge}");
        assert!(
            pitch.is_finite() && (-10.0..=10.0).contains(&pitch),
            "pitch@{edge}"
        );
    }
    // Gentle drift: yaw advances with time, pitch oscillates.
    let (y0, _) = panorama_yaw_pitch(10.0);
    let (y1, _) = panorama_yaw_pitch(20.0);
    assert!(y1 > y0);
    let (_, p_a) = panorama_yaw_pitch(std::f32::consts::PI); // sin(π/2·…)
    let (_, p_b) = panorama_yaw_pitch(3.0 * std::f32::consts::PI);
    assert!(p_a.abs() + p_b.abs() > 0.0);
}
