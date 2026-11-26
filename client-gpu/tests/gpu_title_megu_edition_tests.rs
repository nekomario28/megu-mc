// SPDX-License-Identifier: LGPL-3.0-or-later
//! GPU-TITLE-MEGU-EDITION: clean-room runtime `MEGU EDITION` title
//! branding — the RGBA-memory edition patcher plus the `LazyZipIndex`
//! in-memory JAR loader. Every fixture is synthetic: in-memory RGBA
//! buffers, `encode_png_rgba` payloads and stored-ZIP archives; zero
//! Mojang assets, zero temp files.

use std::path::PathBuf;

use megu_client_gpu::title_branding::{
    load_title_branding_from_jar, patch_edition_banner_to_megu, MEGU_SPLASHES,
};
use megu_client_gpu::window::GpuApp;
use megu_client_gpu::GpuCliConfig;
use megu_ui::draw::encode_png_rgba;
use megu_ui::state::Screen;

/// Clean-room temp dir per test process (no external crates).
struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        let dir =
            std::env::temp_dir().join(format!("megu-title-megu-edition-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }

    /// Every file under the root, recursively, sorted — used to prove
    /// the loader creates zero temp files.
    fn snapshot(&self) -> Vec<PathBuf> {
        let mut out = Vec::new();
        let mut stack = vec![self.0.clone()];
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(&dir).unwrap().flatten() {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                } else {
                    out.push(path);
                }
            }
        }
        out.sort();
        out
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Minimal stored-mode ZIP writer for synthetic JAR fixtures.
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

/// Synthetic clean-room `128x16` `* EDITION` banner: left word ink in
/// columns `0..46` (solid `(200,30,30)`), a 8-column gap, right word in
/// columns `54..127` with a real palette — dark outline on the glyph
/// edge rows/cols, bright fill inside, and a semi-transparent drop
/// shadow row underneath.
const BANNER_W: u32 = 128;
const BANNER_H: u32 = 16;
const LEFT_WORD_END: u32 = 46; // exclusive
const RIGHT_WORD_START: u32 = 54;
const RIGHT_WORD_END: u32 = 127; // exclusive
const GLYPH_TOP: u32 = 3;
const GLYPH_BOTTOM: u32 = 13; // exclusive; shadow occupies row 13
const LEFT_INK: [u8; 4] = [200, 30, 30, 255];
const RIGHT_FILL: [u8; 4] = [230, 235, 240, 255];
const RIGHT_OUTLINE: [u8; 4] = [20, 20, 40, 255];
const RIGHT_SHADOW: [u8; 4] = [0, 0, 0, 120];

fn synth_edition_banner() -> Vec<u8> {
    let mut rgba = vec![0u8; (BANNER_W * BANNER_H * 4) as usize];
    let put = |rgba: &mut [u8], x: u32, y: u32, c: [u8; 4]| {
        let i = (y * BANNER_W + x) as usize * 4;
        rgba[i..i + 4].copy_from_slice(&c);
    };
    // Left word: solid block standing in for the `JAVA` glyphs.
    for y in GLYPH_TOP..GLYPH_BOTTOM {
        for x in 0..LEFT_WORD_END {
            put(&mut rgba, x, y, LEFT_INK);
        }
    }
    // Right word: outline border, fill interior, shadow one row below.
    for y in GLYPH_TOP..GLYPH_BOTTOM {
        for x in RIGHT_WORD_START..RIGHT_WORD_END {
            let edge = y == GLYPH_TOP
                || y == GLYPH_BOTTOM - 1
                || x == RIGHT_WORD_START
                || x == RIGHT_WORD_END - 1;
            put(&mut rgba, x, y, if edge { RIGHT_OUTLINE } else { RIGHT_FILL });
        }
    }
    for x in RIGHT_WORD_START + 1..RIGHT_WORD_END {
        put(&mut rgba, x, GLYPH_BOTTOM, RIGHT_SHADOW);
    }
    rgba
}

fn px(rgba: &[u8], x: u32, y: u32) -> [u8; 4] {
    let i = (y * BANNER_W + x) as usize * 4;
    [rgba[i], rgba[i + 1], rgba[i + 2], rgba[i + 3]]
}

#[test]
fn edition_patcher_replaces_left_java_region_with_megu_and_preserves_right_edition_pixels() {
    let original = synth_edition_banner();
    let mut rgba = original.clone();
    let report = patch_edition_banner_to_megu(&mut rgba, BANNER_W, BANNER_H)
        .expect("synthetic two-word banner must patch");

    assert_eq!((report.width, report.height), (BANNER_W, BANNER_H));
    assert_eq!(
        report.left_box.x, 0,
        "left word starts at the image edge"
    );
    assert_eq!(report.left_box.w, LEFT_WORD_END);
    assert!(report.patched_left_pixels > 0);
    assert!(
        report.preserved_right_pixels > 0,
        "right EDITION pixels were not preserved"
    );

    // Every pixel right of the gap boundary is byte-identical.
    for y in 0..BANNER_H {
        for x in RIGHT_WORD_START - 1..BANNER_W {
            assert_eq!(
                px(&rgba, x, y),
                px(&original, x, y),
                "right EDITION pixel ({x},{y}) changed"
            );
        }
    }

    // The left word was cleared of `LEFT_INK` and re-inked by the patch:
    // no leftover original pixel, non-trivial new coverage.
    let left_changed = (0..LEFT_WORD_END)
        .any(|x| (GLYPH_TOP..GLYPH_BOTTOM).any(|y| px(&rgba, x, y) != LEFT_INK));
    assert!(left_changed, "left word kept original JAVA pixels");
    let left_ink: usize = (0..LEFT_WORD_END)
        .flat_map(|x| (0..BANNER_H).map(move |y| (x, y)))
        .filter(|&(x, y)| px(&rgba, x, y)[3] > 0)
        .count();
    assert!(left_ink > 0, "MEGU glyphs produced no coverage");

    // The new glyphs use the palette sampled from the right word:
    // the dominant fill and the dark outline both appear in the left
    // region (the shadow's alpha may too).
    let has_fill = (0..LEFT_WORD_END)
        .flat_map(|x| (0..BANNER_H).map(move |y| (x, y)))
        .any(|(x, y)| px(&rgba, x, y) == RIGHT_FILL);
    let has_outline = (0..LEFT_WORD_END)
        .flat_map(|x| (0..BANNER_H).map(move |y| (x, y)))
        .any(|(x, y)| px(&rgba, x, y) == RIGHT_OUTLINE);
    assert!(has_fill, "MEGU missing the sampled fill palette");
    assert!(has_outline, "MEGU missing the sampled outline palette");

    // All four M-E-G-U letter columns carry glyph coverage: the font is
    // centered in the cleared 46-px word at scale 1 -> 23 px wide,
    // letters at x0 + 6*slot.
    let x0 = report.left_box.x + (report.left_box.w - 23) / 2;
    for slot in 0..4 {
        let col0 = x0 + 6 * slot;
        let coverage: usize = (col0..col0 + 5)
            .flat_map(|x| (0..BANNER_H).map(move |y| (x, y)))
            .filter(|&(x, y)| px(&rgba, x, y)[3] > 0)
            .count();
        assert!(coverage > 0, "letter slot {slot} (cols {col0}..) is empty");
    }

    // Fail-closed inputs never panic and never write.
    let mut undersized = vec![0u8; 32 * 8 * 4];
    assert!(patch_edition_banner_to_megu(&mut undersized, 32, 8).is_err());
    assert!(undersized.iter().all(|&b| b == 0));
    let mut truncated = original[..original.len() - 1].to_vec();
    assert!(patch_edition_banner_to_megu(&mut truncated, BANNER_W, BANNER_H).is_err());
    let mut oversized = vec![0u8; 64 * 8 * 4];
    assert!(patch_edition_banner_to_megu(&mut oversized, 8192, 8).is_err());
    let mut zero = Vec::new();
    assert!(patch_edition_banner_to_megu(&mut zero, 0, 0).is_err());
    // A fully-transparent banner has no glyphs to sample.
    let mut empty = vec![0u8; (BANNER_W * BANNER_H * 4) as usize];
    assert!(patch_edition_banner_to_megu(&mut empty, BANNER_W, BANNER_H).is_err());
    // A single-word banner (no interior gap) is not an edition strip.
    let mut one_word = vec![0u8; (BANNER_W * BANNER_H * 4) as usize];
    for y in GLYPH_TOP..GLYPH_BOTTOM {
        for x in 8..120 {
            let i = (y * BANNER_W + x) as usize * 4;
            one_word[i..i + 4].copy_from_slice(&RIGHT_FILL);
        }
    }
    assert!(patch_edition_banner_to_megu(&mut one_word, BANNER_W, BANNER_H).is_err());
}

#[test]
fn load_title_branding_from_synthetic_jar_loads_patched_edition_and_merged_splashes_zero_temp_files()
 {
    let tmp = TempDir::new("jar");
    let banner = synth_edition_banner();
    let edition_png = encode_png_rgba(BANNER_W, BANNER_H, &banner);
    let logo_png = encode_png_rgba(16, 8, &vec![0xAB; 16 * 8 * 4]);
    let splashes = b"Synthetic Alpha\nSynthetic Beta\n\n".as_slice();
    let jar = tmp.0.join("client.jar");
    std::fs::write(
        &jar,
        stored_zip(&[
            (
                "assets/minecraft/textures/gui/title/minecraft.png",
                logo_png.as_slice(),
            ),
            (
                "assets/minecraft/textures/gui/title/edition.png",
                edition_png.as_slice(),
            ),
            ("assets/minecraft/texts/splashes.txt", splashes),
            ("META-INF/MANIFEST.MF", b"Manifest-Version: 1.0".as_slice()),
        ]),
    )
    .unwrap();
    let mod_jar = tmp.0.join("mod.jar");
    std::fs::write(
        &mod_jar,
        stored_zip(&[(
            "assets/mymod/texts/splashes.txt",
            b"Mod Splash!".as_slice(),
        )]),
    )
    .unwrap();

    let before = tmp.snapshot();
    let assets = load_title_branding_from_jar(&jar, &[mod_jar]);
    let after = tmp.snapshot();
    assert_eq!(
        before, after,
        "title branding load must create zero temp files"
    );

    // Wordmark + patched edition strip decoded from the JAR.
    let logo = assets.logo.as_ref().expect("logo must decode");
    assert_eq!((logo.width, logo.height), (16, 8));
    let edition = assets.edition.as_ref().expect("edition must decode+patch");
    assert_eq!((edition.width, edition.height), (BANNER_W, BANNER_H));
    let patch = assets.edition_patch.expect("patch report recorded");
    assert!(patch.patched_left_pixels > 0);
    // The patched strip still preserves the right EDITION pixels.
    for y in 0..BANNER_H {
        for x in RIGHT_WORD_START..BANNER_W {
            assert_eq!(
                px(&edition.rgba, x, y),
                px(&banner, x, y),
                "loaded edition pixel ({x},{y}) was not preserved"
            );
        }
    }
    assert!(assets.has_banner());
    assert_eq!(assets.source_jar.as_deref(), Some(jar.as_path()));

    // Splash pool: JAR splashes, then mod splashes, then built-ins —
    // deterministic order, blanks dropped.
    let alpha = assets
        .splashes
        .iter()
        .position(|s| s == "Synthetic Alpha")
        .expect("JAR splash missing");
    let beta = assets
        .splashes
        .iter()
        .position(|s| s == "Synthetic Beta")
        .expect("JAR splash missing");
    let modded = assets
        .splashes
        .iter()
        .position(|s| s == "Mod Splash!")
        .expect("mod splash missing");
    let builtin = assets
        .splashes
        .iter()
        .position(|s| s == "MEGU EDITION!")
        .expect("built-in Megu splash missing");
    assert!(alpha < beta && beta < modded && modded < builtin);
    for s in MEGU_SPLASHES {
        assert!(assets.splashes.iter().any(|p| p == s), "missing {s}");
    }
    assert_eq!(assets.splashes.len(), 3 + MEGU_SPLASHES.len());
    // Deterministic selection: same seed, same line; always a pool member.
    assert_eq!(assets.select_splash(7), assets.select_splash(7));
    assert!(assets.splashes.iter().any(|s| s == assets.select_splash(42)));

    // Missing JAR -> graceful built-in fallback, never a panic.
    let missing = tmp.0.join("absent.jar");
    let fallback = load_title_branding_from_jar(&missing, &[]);
    assert!(!fallback.has_banner());
    assert!(fallback.logo.is_none() && fallback.edition.is_none());
    assert_eq!(fallback.splashes.len(), MEGU_SPLASHES.len());
    assert!(!fallback.splashes.is_empty());
    // A corrupt JAR degrades the same way.
    let corrupt = tmp.0.join("corrupt.jar");
    std::fs::write(&corrupt, b"not a zip archive at all").unwrap();
    let pre_corrupt_load = tmp.snapshot();
    let degraded = load_title_branding_from_jar(&corrupt, &[]);
    assert!(!degraded.has_banner());
    assert!(!degraded.splashes.is_empty());
    assert_eq!(
        pre_corrupt_load,
        tmp.snapshot(),
        "still zero temp files after fallback"
    );
}

#[test]
fn gpu_app_boot_loads_megu_edition_branding_and_exposes_getters() {
    let tmp = TempDir::new("app");
    let game_dir = tmp.0.join("game");
    let jar_dir = game_dir.join("versions").join("1.21.1");
    std::fs::create_dir_all(&jar_dir).unwrap();
    let banner = synth_edition_banner();
    let edition_png = encode_png_rgba(BANNER_W, BANNER_H, &banner);
    let logo_png = encode_png_rgba(16, 8, &vec![0xAB; 16 * 8 * 4]);
    std::fs::write(
        jar_dir.join("1.21.1.jar"),
        stored_zip(&[
            (
                "assets/minecraft/textures/gui/title/minecraft.png",
                logo_png.as_slice(),
            ),
            (
                "assets/minecraft/textures/gui/title/edition.png",
                edition_png.as_slice(),
            ),
            (
                "assets/minecraft/texts/splashes.txt",
                b"Boot Splash".as_slice(),
            ),
        ]),
    )
    .unwrap();

    let app = GpuApp::new(GpuCliConfig {
        game_dir: Some(game_dir),
        version_id: Some("1.21.1".to_owned()),
        ..GpuCliConfig::default()
    });
    assert_eq!(app.screen(), Screen::Title);
    assert!(
        app.title_branding().has_banner(),
        "boot did not load MEGU EDITION branding: {:?}",
        app.title_branding().notes
    );
    assert!(app.title_branding().edition_patch.is_some());
    let splash = app.active_splash();
    assert!(!splash.is_empty());
    assert!(app
        .title_branding()
        .splashes
        .iter()
        .any(|s| s == splash));

    // No game dir at all: the built-in title path stays untouched.
    let bare = GpuApp::new(GpuCliConfig::default());
    assert_eq!(bare.screen(), Screen::Title);
    assert!(!bare.title_branding().has_banner());
    assert!(!bare.active_splash().is_empty());
}
