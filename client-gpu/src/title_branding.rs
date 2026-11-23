// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
//! Clean-room runtime `MEGU EDITION` title-screen branding.
//!
//! When the player's *own* local vanilla client JAR is present, this
//! module reads three archive members **in memory** through
//! [`LazyZipIndex`] (zero temp files, zero Mojang assets in git):
//!
//! * `assets/minecraft/textures/gui/title/minecraft.png` — the wordmark
//! * `assets/minecraft/textures/gui/title/edition.png` — the `* EDITION`
//!   banner whose left word is patched in RGBA memory into `MEGU`
//!   (producing `MEGU EDITION`) by [`patch_edition_banner_to_megu`]
//! * `assets/minecraft/texts/splashes.txt` — the splash pool, merged with
//!   any `assets/*/texts/splashes.txt` inside loaded mod JARs plus a
//!   small original Megu set
//!
//! The patcher is resolution-agnostic: it samples the fill / highlight /
//! outline / drop-shadow palette straight from the untouched right-hand
//! `EDITION` glyphs, so standard 128×16, HD 256×32, 512×64 and custom
//! resource-pack banners all restyle correctly.
//!
//! Everything fails closed: a missing JAR, malformed archive or
//! undecodable PNG yields [`TitleBrandingAssets::fallback`] — never a
//! panic, never a partial write to disk.

use std::path::{Path, PathBuf};

use megu_client_mesh::png_decode;
use megu_core::zip_archive::LazyZipIndex;

use crate::live_session::{jar_version_ids, SessionPaths};

// ---------------------------------------------------------------------------
// Archive member names + budgets
// ---------------------------------------------------------------------------

/// Archive member carrying the main title wordmark.
const JAR_TITLE_LOGO: &str = "assets/minecraft/textures/gui/title/minecraft.png";
/// Archive member carrying the `* EDITION` banner strip.
const JAR_TITLE_EDITION: &str = "assets/minecraft/textures/gui/title/edition.png";

/// Per-entry materialization cap (same bound `vanilla_jar` enforces).
const MAX_TITLE_ENTRY_BYTES: usize = 8 * 1024 * 1024;
/// Splash text cap — a hostile `splashes.txt` cannot stall boot.
const MAX_SPLASH_BYTES: usize = 256 * 1024;
/// Merged splash-pool entry cap.
const MAX_SPLASHES: usize = 4096;
/// Single splash line cap (vanilla lines are short).
const MAX_SPLASH_LINE: usize = 256;
/// Mod JARs scanned for extra splash pools, in filename order.
const MAX_MOD_JARS: usize = 64;

/// Original clean-room Megu splashes appended to every pool — also the
/// whole pool when no client JAR exists.
pub const MEGU_SPLASHES: [&str; 6] = [
    "Pure Rust!",
    "Zero GC Pauses!",
    "Pure wgpu!",
    "20.00 TPS!",
    "Clean-room!",
    "MEGU EDITION!",
];

/// `assets/<ns>/texts/splashes.txt` under any namespace.
fn is_splashes_entry(name: &str) -> bool {
    name.starts_with("assets/") && name.ends_with("/texts/splashes.txt") && !name.contains("..")
}

/// One of the three title members (or a namespaced splash pool).
fn is_title_entry(name: &str) -> bool {
    name == JAR_TITLE_LOGO || name == JAR_TITLE_EDITION || is_splashes_entry(name)
}

// ---------------------------------------------------------------------------
// Decoded image
// ---------------------------------------------------------------------------

/// One decoded RGBA8 image in tightly-packed row-major order.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RgbaImage {
    /// Texel width.
    pub width: u32,
    /// Texel height.
    pub height: u32,
    /// `width * height * 4` bytes, RGBA8.
    pub rgba: Vec<u8>,
}

impl RgbaImage {
    /// Decode a PNG through the workspace's clean-room decoder; rejects
    /// any payload whose byte count disagrees with its header.
    fn decode_png(bytes: &[u8]) -> Result<Self, String> {
        let (width, height, rgba) =
            png_decode::decode_png_rgba(bytes).map_err(|e| e.to_string())?;
        let expected = width as usize * height as usize * 4;
        if rgba.len() != expected {
            return Err(format!(
                "decoded {width}x{height} PNG gave {} bytes, expected {expected}",
                rgba.len()
            ));
        }
        Ok(Self {
            width,
            height,
            rgba,
        })
    }
}

// ---------------------------------------------------------------------------
// Edition patcher: `JAVA` -> `MEGU` entirely in RGBA memory
// ---------------------------------------------------------------------------

/// Pixel-space rectangle inside the banner (`w`/`h` are extents).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PatchRect {
    /// Left edge (inclusive).
    pub x: u32,
    /// Top edge (inclusive).
    pub y: u32,
    /// Extent in px.
    pub w: u32,
    /// Extent in px.
    pub h: u32,
}

/// What [`patch_edition_banner_to_megu`] did, for observability.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct EditionPatchReport {
    /// Banner width in px.
    pub width: u32,
    /// Banner height in px.
    pub height: u32,
    /// The cleared `JAVA` word bounding box.
    pub left_box: PatchRect,
    /// Pixels at or right of the word-gap boundary — never written,
    /// guaranteed byte-identical to the input.
    pub preserved_right_pixels: u64,
    /// Pixel writes the patch performed (box clear plus shadow/outline/
    /// fill/highlight rasterization; writes may revisit a pixel across
    /// layers, so this counts work, not unique positions).
    pub patched_left_pixels: u64,
}

/// Banner dimension contract: the smallest real `edition.png` is 64×8;
/// anything bigger than a 4K strip is rejected outright.
const MIN_BANNER_W: u32 = 64;
const MIN_BANNER_H: u32 = 8;
const MAX_BANNER_W: u32 = 4096;
const MAX_BANNER_H: u32 = 1024;

/// Clean-room 5×7 pixel-art font for `M E G U` — bit `c` of each row byte
/// marks glyph cell `c` (0 = leftmost of 5).
const GLYPH_ROWS: usize = 7;
const GLYPH_COLS: usize = 5;
const MEGU_GLYPHS: [[u8; GLYPH_ROWS]; 4] = [
    // M
    [
        0b10001, 0b11011, 0b10101, 0b10101, 0b10001, 0b10001, 0b10001,
    ],
    // E
    [
        0b11111, 0b00001, 0b00001, 0b01111, 0b00001, 0b00001, 0b11111,
    ],
    // G
    [
        0b11110, 0b00001, 0b00001, 0b10111, 0b10001, 0b10001, 0b01110,
    ],
    // U
    [
        0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b01110,
    ],
];
/// Advance per letter: 5 glyph columns + 1 spacing column (`MEGU` spans
/// `4 * 6 - 1 = 23` cells).
const LETTER_ADVANCE: u32 = (GLYPH_COLS as u32) + 1;
/// Total `MEGU` cell width at scale 1.
const MEGU_CELL_W: u32 = 4 * LETTER_ADVANCE - 1;

/// Quick luma for palette ranking (deterministic, no float).
fn luma(px: &[u8]) -> u32 {
    u32::from(px[0]) * 3 + u32::from(px[1]) * 6 + u32::from(px[2])
}

/// Fill/highlight/outline/shadow RGBA sampled from the untouched
/// `EDITION` glyphs.
#[derive(Clone, Copy, Debug)]
struct GlyphPalette {
    fill: [u8; 4],
    highlight: [u8; 4],
    outline: [u8; 4],
    shadow: [u8; 4],
}

/// Sample the right `EDITION` word: fully-opaque texels vote for `fill`
/// (mode), the brightest becomes `highlight`, the darkest `outline`;
/// semi-transparent texels (the drop shadow, alpha below opaque) vote
/// for `shadow` — falling back to a softened `outline` when the source
/// casts none.
fn sample_palette(rgba: &[u8], width: u32, box_: PatchRect) -> Result<GlyphPalette, String> {
    let mut counts: Vec<([u8; 4], u32)> = Vec::new();
    for y in box_.y..box_.y + box_.h {
        for x in box_.x..box_.x + box_.w {
            let i = (y * width + x) as usize * 4;
            let px = &rgba[i..i + 4];
            if px[3] == 0 {
                continue;
            }
            let key = [px[0], px[1], px[2], px[3]];
            match counts.iter_mut().find(|(c, _)| *c == key) {
                Some((_, n)) => *n += 1,
                None => counts.push((key, 1)),
            }
        }
    }
    let (mut fill, mut fill_votes) = ([0u8; 4], 0u32);
    let (mut highlight, mut hi_luma) = ([0u8; 4], 0u32);
    let (mut outline, mut lo_luma) = ([0u8; 4], u32::MAX);
    let (mut shadow, mut shadow_votes) = ([0u8; 4], 0u32);
    let mut opaque_px = 0u64;
    for (c, n) in counts {
        if c[3] >= 224 {
            opaque_px += u64::from(n);
            if n > fill_votes {
                fill_votes = n;
                fill = c;
            }
            let l = luma(&c);
            if l > hi_luma {
                hi_luma = l;
                highlight = c;
            }
            if l < lo_luma {
                lo_luma = l;
                outline = c;
            }
        } else if n > shadow_votes {
            shadow_votes = n;
            shadow = c;
        }
    }
    if opaque_px == 0 {
        return Err("right word has no opaque glyph pixels to sample".to_owned());
    }
    if shadow_votes == 0 {
        // No explicit drop shadow in the source: reuse the outline at a
        // softened alpha so `MEGU` still casts like `EDITION` would.
        shadow = [outline[0], outline[1], outline[2], outline[3].min(160)];
    }
    Ok(GlyphPalette {
        fill,
        highlight,
        outline,
        shadow,
    })
}

/// Bounds-checked RGBA writer counting every write it performs.
struct Painter<'a> {
    rgba: &'a mut [u8],
    width: u32,
    height: u32,
    /// Hard right bound: nothing may cross into the `EDITION` word.
    x_limit: u32,
    writes: u64,
}

impl Painter<'_> {
    fn put(&mut self, x: i32, y: i32, color: [u8; 4]) {
        if x < 0 || y < 0 || x as u32 >= self.x_limit || y as u32 >= self.height {
            return;
        }
        let i = (y as u32 * self.width + x as u32) as usize * 4;
        self.rgba[i..i + 4].copy_from_slice(&color);
        self.writes += 1;
    }

    fn fill(&mut self, x: i32, y: i32, w: i32, h: i32, color: [u8; 4]) {
        for dy in 0..h {
            for dx in 0..w {
                self.put(x + dx, y + dy, color);
            }
        }
    }
}

/// Rewrite the left `JAVA` word of an `edition.png`-shaped RGBA banner
/// as crisp blocky `MEGU`, leaving the right `EDITION` word and the gap
/// byte-identical.
///
/// Layout discovery is fully dynamic: the widest interior run of
/// empty columns is the word gap; everything left of it is cleared via
/// its tight ink bounding box, and the right word supplies the palette
/// and the baseline the new glyphs sit on.
///
/// Fails closed on dimension/buffer mismatch, an empty banner, no word
/// gap, or a right word with nothing to sample.
pub fn patch_edition_banner_to_megu(
    rgba: &mut [u8],
    width: u32,
    height: u32,
) -> Result<EditionPatchReport, String> {
    if !(MIN_BANNER_W..=MAX_BANNER_W).contains(&width)
        || !(MIN_BANNER_H..=MAX_BANNER_H).contains(&height)
    {
        return Err(format!(
            "banner dimensions {width}x{height} outside {MIN_BANNER_W}x{MIN_BANNER_H}..{MAX_BANNER_W}x{MAX_BANNER_H}"
        ));
    }
    let expected = width as usize * height as usize * 4;
    if rgba.len() != expected {
        return Err(format!(
            "RGBA buffer is {} bytes, expected {expected} for {width}x{height}",
            rgba.len()
        ));
    }

    // Column ink histogram: a column "has ink" when any pixel is
    // non-transparent (alpha > 0 — shadow texels count).
    let mut col_ink = vec![0u32; width as usize];
    for y in 0..height {
        for x in 0..width {
            if rgba[(y * width + x) as usize * 4 + 3] > 0 {
                col_ink[x as usize] += 1;
            }
        }
    }
    let first = col_ink.iter().position(|&n| n > 0);
    let last = col_ink.iter().rposition(|&n| n > 0);
    let (Some(first), Some(last)) = (first, last) else {
        return Err("banner has no non-transparent pixels".to_owned());
    };

    // Widest interior empty-column run = the word gap. It must be at
    // least two columns wide — a single empty column is letter spacing,
    // not a word break.
    let mut gap = None;
    let mut run_start = None;
    for (x, &ink) in col_ink.iter().enumerate().take(last + 1).skip(first) {
        if ink == 0 {
            run_start.get_or_insert(x);
        } else if let Some(start) = run_start.take() {
            if gap.is_none_or(|(_, _, g)| x - start > g) {
                gap = Some((start, x, x - start));
            }
        }
    }
    let Some((gap_start, gap_end, gap_w)) = gap else {
        return Err("no word gap: banner is not a two-word strip".to_owned());
    };
    if gap_w < 2 {
        return Err(format!("word gap too narrow ({gap_w}px) to split words"));
    }

    // Tight ink boxes of both words.
    let word_box = |x0: u32, x1: u32| -> PatchRect {
        let mut top = height;
        let mut bottom = 0u32;
        for y in 0..height {
            for x in x0..=x1 {
                if rgba[(y * width + x) as usize * 4 + 3] > 0 {
                    top = top.min(y);
                    bottom = y;
                }
            }
        }
        PatchRect {
            x: x0,
            y: top,
            w: x1 - x0 + 1,
            h: bottom - top + 1,
        }
    };
    let left_box = word_box(first as u32, gap_start as u32 - 1);
    let right_box = word_box(gap_end as u32, last as u32);
    let palette = sample_palette(rgba, width, right_box)?;

    // Pixel scale for the 5×7 font: the right word's ink height covers
    // roughly 7 glyph rows + outline + shadow (≈9 rows), with the
    // `+4` rounding real HD banners up instead of shrinking them.
    let mut s = ((right_box.h + 4) / 9).max(1);
    // `MEGU` must stay left of the word gap; shrink until it fits the
    // cleared word's own span (plus one letter-spacing of slack).
    let span = (gap_start - first) as u32;
    while s > 1 && MEGU_CELL_W * s > span + LETTER_ADVANCE {
        s -= 1;
    }

    let mut painter = Painter {
        rgba,
        width,
        height,
        x_limit: gap_end as u32 - 1,
        writes: 0,
    };

    // Clear the `JAVA` word's tight box — every non-transparent pixel of
    // the left word is inside it by construction.
    let clear = [0u8; 4];
    for y in left_box.y..left_box.y + left_box.h {
        for x in left_box.x..left_box.x + left_box.w {
            let i = (y * width + x) as usize * 4;
            painter.rgba[i..i + 4].copy_from_slice(&clear);
            painter.writes += 1;
        }
    }

    // Baseline harmony: the new glyph bottom sits on the right word's
    // real glyph bottom — its last ink row minus the shadow offset.
    let baseline = right_box.y + right_box.h - 1 - s.min(right_box.h - 1);
    let y0 = baseline as i32 - (GLYPH_ROWS as i32 * s as i32) + 1;
    let font_w = MEGU_CELL_W * s;
    let x0 = left_box.x + left_box.w.saturating_sub(font_w) / 2;

    // Rasterize M-E-G-U: drop shadow (+s,+s), then a one-cell dark
    // outline ring, then the fill with a top-edge highlight. A mild
    // italic slant (total `s` px, top leaning right) echoes the
    // `EDITION` word's lean.
    let slant = s as i32;
    for (li, glyph) in MEGU_GLYPHS.iter().enumerate() {
        let lx = x0 as i32 + (li as u32 * LETTER_ADVANCE * s) as i32;
        for r in 0..GLYPH_ROWS {
            let xoff = ((GLYPH_ROWS - 1 - r) as i32 * slant) / (GLYPH_ROWS - 1) as i32;
            for c in 0..GLYPH_COLS {
                if glyph[r] & (1 << c) == 0 {
                    continue;
                }
                let px = lx + xoff + (c as u32 * s) as i32;
                let py = y0 + (r as u32 * s) as i32;
                let sz = s as i32;
                painter.fill(px + sz, py + sz, sz, sz, palette.shadow);
                painter.fill(px - sz, py - sz, 3 * sz, 3 * sz, palette.outline);
                painter.fill(px, py, sz, sz, palette.fill);
                // Top-edge cells catch the sampled highlight.
                let above_set = r > 0 && glyph[r - 1] & (1 << c) != 0;
                if !above_set {
                    painter.fill(px, py, sz, s.div_ceil(2).max(1) as i32, palette.highlight);
                }
            }
        }
    }

    Ok(EditionPatchReport {
        width,
        height,
        left_box,
        preserved_right_pixels: u64::from(width - painter.x_limit) * u64::from(height),
        patched_left_pixels: painter.writes,
    })
}

// ---------------------------------------------------------------------------
// Splash pool + title assets
// ---------------------------------------------------------------------------

/// Everything the title screen needs from the local client JAR plus mod
/// JARs — always clean-room: decoded at runtime, never stored in git.
#[derive(Clone, Debug, Default)]
pub struct TitleBrandingAssets {
    /// `minecraft.png` wordmark, decoded RGBA. `None` without a JAR or
    /// on any decode failure.
    pub logo: Option<RgbaImage>,
    /// `edition.png` after [`patch_edition_banner_to_megu`]. `None`
    /// without a JAR or when the patch failed closed.
    pub edition: Option<RgbaImage>,
    /// Patching metrics for observability/tests.
    pub edition_patch: Option<EditionPatchReport>,
    /// Merged splash pool: client-JAR splashes, then mod-JAR splashes,
    /// then [`MEGU_SPLASHES`]. Never empty (built-ins always present).
    pub splashes: Vec<String>,
    /// The client JAR the assets came from, when one was read.
    pub source_jar: Option<PathBuf>,
    /// Every skipped/malformed source — a dropped entry can never look
    /// like a clean negative result.
    pub notes: Vec<String>,
}

impl TitleBrandingAssets {
    /// The no-JAR state: Megu built-in splashes only, no banner textures
    /// — the built-in `MEGU` title stays in charge.
    pub fn fallback() -> Self {
        Self {
            splashes: MEGU_SPLASHES.iter().map(|s| (*s).to_owned()).collect(),
            ..Self::default()
        }
    }

    /// `true` when both banner textures decoded/patched successfully.
    pub fn has_banner(&self) -> bool {
        self.logo.is_some() && self.edition.is_some()
    }

    /// Deterministic splash pick for one session seed. The pool is never
    /// empty (`fallback` and the loader both guarantee built-ins), but
    /// the accessor still fails closed.
    pub fn select_splash(&self, seed: u64) -> &str {
        self.splashes
            .get(seed as usize % self.splashes.len().max(1))
            .map_or(MEGU_SPLASHES[0], String::as_str)
    }
}

/// Parse one `splashes.txt` payload: one splash per line, whitespace
/// trimmed, empties dropped, line count and length bounded.
fn parse_splashes(bytes: &[u8], into: &mut Vec<String>, notes: &mut Vec<String>, source: &str) {
    let text = String::from_utf8_lossy(bytes);
    for line in text.lines() {
        if into.len() >= MAX_SPLASHES {
            notes.push(format!("{source}: splash pool cap {MAX_SPLASHES} reached"));
            break;
        }
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let line: String = line.chars().take(MAX_SPLASH_LINE).collect();
        if !into.contains(&line) {
            into.push(line);
        }
    }
}

/// Load the runtime `MEGU EDITION` branding set from a local vanilla
/// client JAR plus any loaded mod JARs.
///
/// The client JAR contributes `minecraft.png`, `edition.png` (patched in
/// memory to `MEGU EDITION`) and `splashes.txt`; each mod JAR
/// contributes its `assets/*/texts/splashes.txt`. All reads go through
/// [`LazyZipIndex`] — only the EOCD tail and Central Directory are
/// indexed, selected entries inflate in memory, and **zero temporary
/// files** are created.
///
/// Fail-closed: a missing/unreadable JAR, malformed entries or
/// undecodable PNGs degrade to [`TitleBrandingAssets::fallback`] pieces
/// (a partial load keeps whatever decoded), never a panic.
pub fn load_title_branding_from_jar(
    jar_path: &Path,
    mod_jars: &[PathBuf],
) -> TitleBrandingAssets {
    let mut out = TitleBrandingAssets::default();
    if !jar_path.is_file() {
        out.notes.push(format!(
            "client JAR {} missing; using built-in title",
            jar_path.display()
        ));
    } else {
        out.source_jar = Some(jar_path.to_path_buf());
        match LazyZipIndex::open_filtered(jar_path, is_title_entry) {
            Ok(index) => load_client_jar_entries(&index, &mut out),
            Err(err) => out.notes.push(format!(
                "client JAR {} unreadable: {err}; using built-in title",
                jar_path.display()
            )),
        }
    }

    // Mod JARs: merge every `assets/*/texts/splashes.txt`, filename order.
    for jar in mod_jars.iter().take(MAX_MOD_JARS) {
        if !jar.is_file() {
            continue;
        }
        let mod_index = match LazyZipIndex::open_filtered(jar, is_splashes_entry) {
            Ok(index) => index,
            Err(err) => {
                out.notes
                    .push(format!("mod JAR {} unreadable: {err}", jar.display()));
                continue;
            }
        };
        for meta in mod_index.entries() {
            match mod_index.read_entry(&meta.name, MAX_SPLASH_BYTES) {
                Ok(bytes) => parse_splashes(
                    &bytes,
                    &mut out.splashes,
                    &mut out.notes,
                    &format!("{}:{}", jar.display(), meta.name),
                ),
                Err(err) => out.notes.push(format!(
                    "{}:{}: splash entry unreadable: {err}",
                    jar.display(),
                    meta.name
                )),
            }
        }
    }

    // The original Megu set always joins the pool, so `splashes` is
    // never empty.
    for s in MEGU_SPLASHES {
        let s = s.to_owned();
        if !out.splashes.contains(&s) {
            out.splashes.push(s);
        }
    }
    out
}

/// Decode and fold the client JAR's indexed title entries into `out`:
/// the wordmark, the `edition.png` banner patched in memory to
/// `MEGU EDITION`, and every namespaced `splashes.txt`.
fn load_client_jar_entries(index: &LazyZipIndex, out: &mut TitleBrandingAssets) {
    for meta in index.entries() {
        let cap = if is_splashes_entry(&meta.name) {
            MAX_SPLASH_BYTES
        } else {
            MAX_TITLE_ENTRY_BYTES
        };
        match index.read_entry(&meta.name, cap) {
            Ok(bytes) => {
                if meta.name == JAR_TITLE_LOGO {
                    match RgbaImage::decode_png(&bytes) {
                        Ok(img) => out.logo = Some(img),
                        Err(reason) => out
                            .notes
                            .push(format!("{}: logo PNG rejected: {reason}", meta.name)),
                    }
                } else if meta.name == JAR_TITLE_EDITION {
                    match RgbaImage::decode_png(&bytes) {
                        Ok(mut img) => {
                            match patch_edition_banner_to_megu(
                                &mut img.rgba,
                                img.width,
                                img.height,
                            ) {
                                Ok(report) => {
                                    out.edition_patch = Some(report);
                                    out.edition = Some(img);
                                }
                                Err(reason) => out.notes.push(format!(
                                    "{}: edition patch failed closed: {reason}",
                                    meta.name
                                )),
                            }
                        }
                        Err(reason) => out
                            .notes
                            .push(format!("{}: edition PNG rejected: {reason}", meta.name)),
                    }
                } else {
                    parse_splashes(&bytes, &mut out.splashes, &mut out.notes, &meta.name);
                }
            }
            Err(err) => out
                .notes
                .push(format!("{}: entry unreadable: {err}", meta.name)),
        }
    }
}

// ---------------------------------------------------------------------------
// Source discovery + screen layout
// ---------------------------------------------------------------------------

/// Resolve the branding inputs from launcher paths: the vanilla client
/// JAR (`<gameDir>/versions/<id>/<id>.jar`, `--version` id first with
/// `1.21.1` fallback) plus `<gameDir>/mods/*.jar` in deterministic
/// filename order — the same discovery rules `recipe_ingest` uses.
pub fn title_branding_sources(paths: &SessionPaths) -> (Option<PathBuf>, Vec<PathBuf>) {
    let Some(game_dir) = paths.game_dir.as_deref() else {
        return (None, Vec::new());
    };
    let jar = jar_version_ids(paths)
        .into_iter()
        .map(|id| game_dir.join("versions").join(&id).join(format!("{id}.jar")))
        .find(|p| p.is_file());
    let mut mods: Vec<PathBuf> = std::fs::read_dir(game_dir.join("mods"))
        .map(|read| {
            read.flatten()
                .map(|e| e.path())
                .filter(|p| {
                    p.is_file()
                        && p.extension()
                            .is_some_and(|ext| ext.eq_ignore_ascii_case("jar"))
                })
                .collect()
        })
        .unwrap_or_default();
    mods.sort_by(|a, b| {
        a.file_name()
            .map(|n| n.to_string_lossy().to_lowercase())
            .cmp(&b.file_name().map(|n| n.to_string_lossy().to_lowercase()))
    });
    mods.truncate(MAX_MOD_JARS);
    (jar, mods)
}

/// Screen-space layout for the title branding set (logical UI px — the
/// same pixel space `tessellate_draw_list` emits and `UI_QUAD_WGSL`
/// consumes).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct TitleBrandLayout {
    /// `(x, y, w, h)` destination rect of the wordmark.
    pub logo_rect: [f32; 4],
    /// `(x, y, w, h)` destination rect of the patched `MEGU EDITION`
    /// banner.
    pub edition_rect: [f32; 4],
    /// Right-aligned splash anchor x — the caller subtracts the splash's
    /// text width so the line ends here (vanilla's splash leans on the
    /// logo's right edge).
    pub splash_anchor_x: i32,
    /// Splash baseline top, tucked against the banner's lower-right.
    pub splash_y: i32,
}

/// Center the branding set in the title band above the menu buttons
/// (which start at y≈92 in `client-ui`'s title layout), preserving the
/// textures' own aspect: the wordmark scales to fit ~55% of the window
/// width and ~30% of its height, and the edition strip follows at the
/// same scale, right under it.
pub fn title_brand_layout(
    screen_w: u32,
    screen_h: u32,
    logo: (u32, u32),
    edition: (u32, u32),
) -> TitleBrandLayout {
    let lw = logo.0.max(1) as f32;
    let lh = logo.1.max(1) as f32;
    let ew = edition.0.max(1) as f32;
    let eh = edition.1.max(1) as f32;
    let sw = screen_w.max(1) as f32;
    let sh = screen_h.max(1) as f32;
    let scale = ((sw * 0.55).max(64.0) / lw)
        .min((sh * 0.30).max(24.0) / lh)
        .clamp(0.05, 2.0);
    let logo_w = (lw * scale).round().max(1.0);
    let logo_h = (lh * scale).round().max(1.0);
    let ed_w = (ew * scale).round().max(1.0);
    let ed_h = (eh * scale).round().max(1.0);
    let total_h = logo_h + 2.0 + ed_h;
    let top = ((96.0 - total_h) / 2.0).max(4.0);
    let logo_x = (sw - logo_w) / 2.0;
    let ed_x = (sw - ed_w) / 2.0;
    let ed_y = top + logo_h + 2.0;
    TitleBrandLayout {
        logo_rect: [logo_x, top, logo_w, logo_h],
        edition_rect: [ed_x, ed_y, ed_w, ed_h],
        splash_anchor_x: (logo_x + logo_w - 4.0) as i32,
        splash_y: (ed_y - 9.0).max(2.0) as i32,
    }
}
