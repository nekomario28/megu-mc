//! Local-Minecraft font discovery for the authentic look: reads
//! `assets/minecraft/textures/font/ascii.png` (the vanilla 8×8 pixel
//! font) and the Unifont `.hex` packs (`unifont_all_no_pua`,
//! `unifont_jp_patch`) out of an installed 1.21.1 — entirely in memory,
//! nothing extracted to disk, nothing Mojang-copyrighted bundled with
//! Megu. Every failure degrades to "no external font", so offline hosts
//! and CI never notice the path exists.

use std::path::{Path, PathBuf};

use megu_client_mesh::png_decode::decode_png_rgba;
use megu_ui::font_cjk::GlyphAtlas;

use crate::live_session::SessionPaths;
use crate::vanilla_bootstrap::{default_vanilla_cache_dir, discover_local_vanilla_jar};
use crate::vanilla_jar::{read_bounded_file, read_selected_entries};

/// JAR member holding the vanilla 8×8 ASCII bitmap font.
const ASCII_PNG_ENTRY: &str = "assets/minecraft/textures/font/ascii.png";
/// Bound on an `indexes/*.json` asset index read — real indexes are a
/// few hundred KB; anything larger is not a font manifest.
const MAX_INDEX_JSON_BYTES: u64 = 32 * 1024 * 1024;
/// Bound on a single `.hex` object read — the real Unifont packs are
/// well under 8 MiB (which `GlyphAtlas::load_unifont_hex` re-checks).
const MAX_HEX_OBJECT_BYTES: usize = 16 * 1024 * 1024;
const MAX_INDEX_CANDIDATES: usize = 64;

fn safe_index_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && !id.contains(['/', '\\', '\0'])
        && id != "."
        && id != ".."
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'))
}

/// Assets-store roots scanned for `indexes/` + `objects/`, in priority
/// order: the explicit `--assetsDir`, then the game dir's own `assets/`
/// (the `.minecraft` layout), then Megu's bootstrap cache.
fn candidate_assets_roots(paths: &SessionPaths) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Some(dir) = &paths.assets_dir {
        out.push(dir.clone());
    }
    if let Some(dir) = &paths.game_dir {
        out.push(dir.join("assets"));
    }
    if let Some(cache) = default_vanilla_cache_dir() {
        out.push(cache.join("assets"));
    }
    if let Some(home) = std::env::var_os("HOME") {
        let home = PathBuf::from(home);
        out.push(home.join(".minecraft").join("assets"));
    }
    out
}

/// Parse `(asset key, sha1 hash)` pairs for every `minecraft/font/…`
/// entry in an `indexes/*.json` blob. The index format is a flat
/// `{"objects": {"<path>": {"hash": "<sha1>", "size": n}}}` map — this
/// is a bounded substring scan, not a JSON parser, and it only accepts
/// keys that end `.hex` or `.zip` so a hostile index cannot make us
/// open arbitrary object names.
fn font_asset_hashes(json: &[u8]) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut pos = 0usize;
    while let Some(off) = json[pos..]
        .windows(b"\"minecraft/font/".len())
        .position(|w| w == b"\"minecraft/font/")
    {
        let start = pos + off + 1;
        pos = start + 1;
        let Some(end) = json[start..].iter().position(|&b| b == b'"') else {
            break;
        };
        let key = &json[start..start + end];
        let Ok(key) = std::str::from_utf8(key) else {
            continue;
        };
        if !(key.starts_with("minecraft/font/") && key.contains("unifont"))
            || !(key.ends_with(".hex") || key.ends_with(".zip"))
        {
            continue;
        }
        // The `"hash"` member follows the key within its object literal;
        // bound the lookahead so a malformed index cannot skip ahead.
        let window_end = (start + end + 512).min(json.len());
        let window = &json[start + end..window_end];
        let Some(hash_off) = window
            .windows(b"\"hash\"".len())
            .position(|w| w == b"\"hash\"")
        else {
            continue;
        };
        let after = &window[hash_off + b"\"hash\"".len()..];
        let Some(colon) = after.iter().position(|&b| b == b':') else {
            continue;
        };
        let after = &after[colon + 1..];
        let Some(q0) = after.iter().position(|&b| b == b'"') else {
            continue;
        };
        let after = &after[q0 + 1..];
        let Some(q1) = after.iter().position(|&b| b == b'"') else {
            continue;
        };
        let hash = &after[..q1];
        if hash.len() == 40 && hash.iter().all(|b| b.is_ascii_hexdigit()) {
            out.push((key.to_owned(), String::from_utf8_lossy(hash).into_owned()));
        }
    }
    out
}

/// Asset indexes to scan, best first: the explicit `--assetIndex`
/// (e.g. `17` for 1.21.1), then every other `indexes/*.json` newest
/// numeric id first.
fn index_files(root: &Path, paths: &SessionPaths) -> Vec<PathBuf> {
    let dir = root.join("indexes");
    let mut out = Vec::new();
    if let Some(id) = &paths.asset_index {
        if safe_index_id(id) {
            let f = dir.join(format!("{id}.json"));
            if f.is_file() {
                out.push(f);
            }
        }
    }
    let mut rest: Vec<(u64, PathBuf)> = std::fs::read_dir(&dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "json"))
        .map(|p| {
            let id = p
                .file_stem()
                .and_then(|s| s.to_str())
                .and_then(|s| s.parse::<u64>().ok())
                .unwrap_or(0);
            (id, p)
        })
        // Bound directory admission before sorting, then keep the newest
        // numeric candidates rather than whichever names readdir returned first.
        .take(MAX_INDEX_CANDIDATES * 16)
        .collect();
    rest.sort_by_key(|a| std::cmp::Reverse(a.0));
    rest.truncate(MAX_INDEX_CANDIDATES);
    for (_, p) in rest {
        if !out.contains(&p) {
            out.push(p);
        }
    }
    out
}

/// Read one Unifont `.hex` blob out of an `objects/<hh>/<hash>` store
/// entry — either directly (`.hex` key) or from the first `.hex` member
/// inside the pack zip, all in memory.
fn read_hex_object(path: &Path, is_zip: bool) -> Option<String> {
    if is_zip {
        let (entries, _) = read_selected_entries(path, |n| n.ends_with(".hex")).ok()?;
        let entry = entries.into_iter().next()?;
        String::from_utf8(entry.bytes).ok()
    } else {
        let bytes = read_bounded_file(path, MAX_HEX_OBJECT_BYTES).ok()?;
        String::from_utf8(bytes).ok()
    }
}

/// Patch preference for `unifont_all_no_pua` (base coverage) vs
/// `unifont_jp_patch` (glyph overrides): the base absorbs first, patches
/// last, mirroring the vanilla font provider chain.
fn unifont_pack_rank(key: &str) -> u8 {
    if key.contains("all_no_pua") {
        0
    } else if key.contains("jp_patch") {
        1
    } else {
        2
    }
}

/// Hydrate Unifont glyphs from the given assets object-store roots.
fn load_unifont_packs(
    paths: &SessionPaths,
    assets_roots: &[PathBuf],
    atlas: &mut GlyphAtlas,
) -> bool {
    let mut loaded_any = false;
    for root in assets_roots {
        for index in index_files(root, paths) {
            let Ok(json) = read_bounded_file(&index, MAX_INDEX_JSON_BYTES as usize) else {
                continue;
            };
            let mut entries = font_asset_hashes(&json);
            if entries.is_empty() {
                continue;
            }
            entries.sort_by_key(|(key, _)| unifont_pack_rank(key));
            entries.dedup_by(|a, b| a.1 == b.1);
            for (key, hash) in entries {
                let obj = root.join("objects").join(&hash[..2]).join(&hash);
                let Some(hex) = read_hex_object(&obj, key.ends_with(".zip")) else {
                    continue;
                };
                if let Ok(pack) = GlyphAtlas::load_unifont_hex(&hex) {
                    atlas.absorb(pack);
                    loaded_any = true;
                }
            }
            if loaded_any {
                return true;
            }
        }
    }
    loaded_any
}

/// Build the vanilla `ascii.png` cell table: a square png divided into
/// a 16×16 grid of glyph cells (vanilla ships 128×128 → 8×8 cells; HD
/// scales like 256×256 also load — each 8×8 dst pixel lights when any
/// covered source pixel is opaque).
fn ascii_cells_from_png(w: u32, h: u32, rgba: &[u8]) -> Option<Box<[[u8; 8]; 128]>> {
    if w == 0 || w != h || !w.is_multiple_of(16) {
        return None;
    }
    if rgba.len() != (w * h * 4) as usize {
        return None;
    }
    let cell = (w / 16) as usize;
    let mut out = Box::new([[0u8; 8]; 128]);
    for (cp, glyph) in out.iter_mut().enumerate() {
        let gx = (cp % 16) * cell;
        let gy = (cp / 16) * cell;
        for (dy, row) in glyph.iter_mut().enumerate() {
            let mut bits = 0u8;
            for dx in 0..8usize {
                let x0 = gx + dx * cell / 8;
                let x1 = gx + (dx + 1) * cell / 8;
                let y0 = gy + dy * cell / 8;
                let y1 = gy + (dy + 1) * cell / 8;
                let lit = (y0..y1).any(|y| {
                    (x0..x1).any(|x| {
                        let o = ((y as u32 * w + x as u32) * 4) as usize;
                        rgba[o + 3] >= 0x80
                    })
                });
                if lit {
                    bits |= 0x80 >> dx;
                }
            }
            *row = bits;
        }
    }
    Some(out)
}

/// Pull `ascii.png` out of one local 1.21.1 client JAR.
fn load_ascii_bitmap(jar: &Path) -> Option<Box<[[u8; 8]; 128]>> {
    let (entries, _) = read_selected_entries(jar, |n| n == ASCII_PNG_ENTRY).ok()?;
    let entry = entries.into_iter().find(|e| e.name == ASCII_PNG_ENTRY)?;
    let (w, h, rgba) = decode_png_rgba(&entry.bytes).ok()?;
    ascii_cells_from_png(w, h, &rgba)
}

/// Load fonts from explicit sources — the testable core of
/// [`try_load_local_minecraft_fonts`] with discovery already resolved.
fn try_load_fonts_from(
    paths: &SessionPaths,
    assets_roots: &[PathBuf],
    jar: Option<&Path>,
) -> Option<GlyphAtlas> {
    let mut atlas = GlyphAtlas::default();
    let mut any = load_unifont_packs(paths, assets_roots, &mut atlas);
    if let Some(cells) = jar.and_then(load_ascii_bitmap) {
        atlas.set_ascii_bitmap(cells);
        any = true;
    }
    any.then_some(atlas)
}

/// Best-effort load of the local Minecraft 1.21.1 font set: the vanilla
/// `ascii.png` 8×8 ASCII table and the Unifont `.hex` packs, merged into
/// one [`GlyphAtlas`] ready for
/// [`megu_ui::font_cjk::install_external_font`]. Returns `None` when no
/// local Minecraft install yields anything — callers treat `None` as
/// "keep the built-in fonts", never an error.
pub fn try_load_local_minecraft_fonts(paths: &SessionPaths) -> Option<GlyphAtlas> {
    try_load_fonts_from(
        paths,
        &candidate_assets_roots(paths),
        discover_local_vanilla_jar(paths).as_deref(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Minimal ZIP writer for unit tests — stored entries only, real
    /// CRC32s so strict index readers accept the archive.
    fn stored_zip(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut central = Vec::new();
        for (name, data) in entries {
            let crc = megu_core::hash::crc32_ieee_update(0, data);
            let lh_off = out.len() as u32;
            out.extend_from_slice(&0x0403_4b50u32.to_le_bytes());
            out.extend_from_slice(&20u16.to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes());
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
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u32.to_le_bytes());
            central.extend_from_slice(&lh_off.to_le_bytes());
            central.extend_from_slice(name.as_bytes());
        }
        let cd_offset = out.len() as u32;
        out.extend_from_slice(&central);
        out.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
        out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
        out.extend_from_slice(&(central.len() as u32).to_le_bytes());
        out.extend_from_slice(&cd_offset.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out
    }

    /// Build a raw RGBA buffer the PNG decoder accepts via a real PNG
    /// encoder on a tiny helper: reuses `zlib_compress_stored` like the
    /// other fixtures in this crate — no Mojang bytes anywhere.
    fn raw_rgba_png(w: u32, h: u32, rgba: &[u8]) -> Vec<u8> {
        fn chunk(kind: &[u8; 4], data: &[u8]) -> Vec<u8> {
            let mut out = Vec::new();
            out.extend_from_slice(&(data.len() as u32).to_be_bytes());
            out.extend_from_slice(kind);
            out.extend_from_slice(data);
            let crc = megu_core::hash::crc32_ieee_update(
                megu_core::hash::crc32_ieee_update(0, kind),
                data,
            );
            out.extend_from_slice(&crc.to_be_bytes());
            out
        }
        let mut ihdr = Vec::new();
        ihdr.extend_from_slice(&w.to_be_bytes());
        ihdr.extend_from_slice(&h.to_be_bytes());
        ihdr.extend_from_slice(&[8, 6, 0, 0, 0]);
        let mut raw = Vec::with_capacity((w * h * 4 + h) as usize);
        for y in 0..h {
            raw.push(0u8); // filter: none
            let row = &rgba[(y * w * 4) as usize..((y + 1) * w * 4) as usize];
            raw.extend_from_slice(row);
        }
        let idat = megu_core::zip_archive::zlib_compress_stored(&raw);
        let mut png = vec![137, 80, 78, 71, 13, 10, 26, 10];
        png.extend(chunk(b"IHDR", &ihdr));
        png.extend(chunk(b"IDAT", &idat));
        png.extend(chunk(b"IEND", &[]));
        png
    }

    /// A tempdir that cleans itself up.
    struct TempDir(PathBuf);
    impl TempDir {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "megu-vanilla-font-test-{}-{}",
                std::process::id(),
                tag
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

    #[test]
    fn ascii_cells_map_16x16_grid_to_codepoints() {
        // 128×128 png: paint 'A' (cp 0x41) cell's first pixel row solid.
        let w = 128u32;
        let mut rgba = vec![0u8; (w * w * 4) as usize];
        let cp = 0x41usize;
        let gx = (cp % 16) * 8;
        let gy = (cp / 16) * 8;
        for x in gx..gx + 8 {
            let o = ((gy as u32 * w + x as u32) * 4) as usize;
            rgba[o + 3] = 0xff;
        }
        let cells = ascii_cells_from_png(w, w, &rgba).unwrap();
        assert_eq!(cells[0x41][0], 0xff);
        assert_eq!(cells[0x41][1], 0);
        assert_eq!(cells[0x42][0], 0, "neighbour cell must stay empty");
    }

    #[test]
    fn ascii_cells_accept_hd_scale_multiples() {
        // 256×256 → 16×16 px cells; light 'B' everywhere.
        let w = 256u32;
        let mut rgba = vec![0u8; (w * w * 4) as usize];
        let cp = 0x42usize;
        for y in (cp / 16) * 16..(cp / 16 + 1) * 16 {
            for x in (cp % 16) * 16..(cp % 16 + 1) * 16 {
                let o = ((y as u32 * w + x as u32) * 4) as usize;
                rgba[o + 3] = 0xff;
            }
        }
        let cells = ascii_cells_from_png(w, w, &rgba).unwrap();
        assert_eq!(cells[0x42], [0xff; 8]);
        assert!(ascii_cells_from_png(100, 100, &rgba).is_none());
        assert!(ascii_cells_from_png(0, 0, &[]).is_none());
    }

    #[test]
    fn font_asset_hashes_find_unifont_entries_only() {
        let json = br#"{"objects":{
            "minecraft/font/unifont_all_no_pua-15.1.05.zip":{"hash":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","size":1},
            "minecraft/font/unifont_jp_patch-15.1.05.hex":{"hash":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb","size":1},
            "minecraft/font/ascii.png":{"hash":"cccccccccccccccccccccccccccccccccccccccc","size":1},
            "minecraft/sounds/x.ogg":{"hash":"dddddddddddddddddddddddddddddddddddddddd","size":1}
        }}"#;
        let found = font_asset_hashes(json);
        assert_eq!(found.len(), 2);
        assert!(found.iter().any(|(k, _)| k.contains("all_no_pua")));
        assert!(found.iter().any(|(k, _)| k.contains("jp_patch")));
    }

    #[test]
    fn loads_ascii_and_unifont_from_synthetic_install() {
        let tmp = TempDir::new("full");
        let root = &tmp.0;
        // Fake 1.21.1 client JAR with only ascii.png inside.
        let mut rgba = vec![0u8; 128 * 128 * 4];
        let cp = 0x30usize; // '0'
        let (gx, gy) = ((cp % 16) * 8, (cp / 16) * 8);
        for x in gx..gx + 8 {
            rgba[((gy * 128 + x) * 4) + 3] = 0xff;
        }
        let png = raw_rgba_png(128, 128, &rgba);
        let jar = stored_zip(&[(ASCII_PNG_ENTRY, &png)]);
        let jar_dir = root.join("versions").join("1.21.1");
        std::fs::create_dir_all(&jar_dir).unwrap();
        std::fs::write(jar_dir.join("1.21.1.jar"), &jar).unwrap();
        // Assets index + objects for one unifont .hex zip.
        let hex = "30AB:FFFF80018001800180018001800180018001800180018001800180018001FFFF\n";
        let hex_zip = stored_zip(&[("unifont_all_no_pua-0.0.hex", hex.as_bytes())]);
        let hash = "0123456789abcdef0123456789abcdef01234567";
        let obj_dir = root.join("assets").join("objects").join(&hash[..2]);
        std::fs::create_dir_all(&obj_dir).unwrap();
        std::fs::write(obj_dir.join(hash), &hex_zip).unwrap();
        let index = format!(
            "{{\"objects\":{{\"minecraft/font/unifont_all_no_pua-0.0.zip\":{{\"hash\":\"{hash}\",\"size\":1}}}}}}"
        );
        let idx_dir = root.join("assets").join("indexes");
        std::fs::create_dir_all(&idx_dir).unwrap();
        std::fs::write(idx_dir.join("17.json"), index).unwrap();

        let atlas = try_load_fonts_from(
            &fake_paths(root),
            &[root.join("assets")],
            Some(&jar_dir.join("1.21.1.jar")),
        )
        .expect("synthetic install should produce an atlas");
        assert!(atlas.has_ascii_bitmap());
        assert_eq!(atlas.ascii_glyph('0').unwrap()[0], 0xff);
        let g = atlas.unifont_glyph('\u{30ab}').expect("katakana glyph");
        assert_eq!(g.cells, 2);
        assert_eq!(g.rows[0], 0xffff);
    }

    #[test]
    fn missing_install_returns_none_silently() {
        let tmp = TempDir::new("empty");
        // Scoped to the empty tempdir — the real host's own Minecraft
        // install (when present) must never leak into this check.
        assert!(try_load_fonts_from(&fake_paths(&tmp.0), &[tmp.0.join("assets")], None).is_none());
    }
}
