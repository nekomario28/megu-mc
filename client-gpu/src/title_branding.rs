// SPDX-License-Identifier: LGPL-3.0-or-later
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

use crate::live_session::SessionPaths;
use crate::vanilla_bootstrap::{
    discover_local_assets_dir, parse_asset_objects, AssetObjectSpec,
    VanillaAssetResolver, MAX_ASSET_INDEX_BYTES,
};
use crate::vanilla_gui_sprites::{load_gui_stack, VanillaGuiAssets, ALL_GUI_SPRITES};
use crate::vanilla_jar::{check_png_budget, read_bounded_file, MAX_DECODED_RGBA_BYTES};

// ---------------------------------------------------------------------------
// Archive member names + budgets
// ---------------------------------------------------------------------------

/// Archive member carrying the main title wordmark.
const JAR_TITLE_LOGO: &str = "assets/minecraft/textures/gui/title/minecraft.png";
/// Archive member carrying the `* EDITION` banner strip.
const JAR_TITLE_EDITION: &str = "assets/minecraft/textures/gui/title/edition.png";
const LOGO_SIZE: (u32, u32) = (256, 44);
const EDITION_SIZE: (u32, u32) = (128, 14);

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

    /// Vanilla blits only the top rows of the padded logo textures.
    fn crop_title_padding(&mut self, (width, height): (u32, u32), padded_height: u32) {
        if self.width.is_multiple_of(width) && self.height == self.width / width * padded_height {
            self.height = self.width / width * height;
            self.rgba.truncate((self.width * self.height * 4) as usize);
        }
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

/// Continuous normalized (`u, v in [0.0, 1.0]`) vector-stroke membership test
/// for clean-room `M`, `E` (fallback), `G`, `U` matching `EDITION`'s exact
/// stroke weight (`sv ≈ 0.25`, `sh ≈ 0.23`) so 4×4 supersampling produces
/// smooth anti-aliased slanted edges indistinguishable from `EDITION`.
fn glyph_face_contains(letter_idx: usize, u: f32, v: f32) -> bool {
    if !(0.0..=1.0).contains(&u) || !(0.0..=1.0).contains(&v) {
        return false;
    }
    const SV: f32 = 0.25; // vertical stem thickness (matches EDITION's ~10/42 px)
    const SH: f32 = 0.23; // horizontal bar thickness (matches EDITION's ~9/39 px)
    match letter_idx {
        // M: vertical outer legs + blocky squared stepped center matching `N` on the right
        0 => {
            if u <= SV || u >= 1.0 - SV {
                return true;
            }
            // Squared stepped bridge echoing EDITION's blocky `N` & the main logo's `M`
            let shoulder = ((0.22..=0.40).contains(&u) || (0.60..=0.78).contains(&u))
                && (0.14..=0.40).contains(&v);
            let center_block = (0.37..=0.63).contains(&u) && (0.34..=0.60).contains(&v);
            shoulder || center_block
        }
        // E (fallback when EDITION's first letter cannot be isolated): exact EDITION E proportions
        1 => {
            u <= SV
                || v <= SH
                || v >= 1.0 - SH
                || ((0.5 - SH * 0.5)..=(0.5 + SH * 0.5)).contains(&v) && u <= 0.88
        }
        // G: exact EDITION squared outer frame with clean upper-right gap + bold inner hook
        2 => {
            u <= SV
                || v <= SH
                || v >= 1.0 - SH
                || (u >= 1.0 - SV && v >= 0.45)
                || (u >= 0.48 && (0.45..=(0.45 + SH)).contains(&v))
        }
        // U: exact EDITION squared outer legs + bottom bar
        3 => u <= SV || u >= 1.0 - SV || v >= 1.0 - SH,
        _ => false,
    }
}

/// Bilinearly sample an RGBA image at floating-point coordinates (clamped transparent outside).
fn sample_rgba_bilinear(rgba: &[u8], width: u32, height: u32, x: f32, y: f32) -> [f32; 4] {
    let x0 = x.floor() as i32;
    let y0 = y.floor() as i32;
    let fx = x - x0 as f32;
    let fy = y - y0 as f32;
    let fetch = |ix: i32, iy: i32| -> [f32; 4] {
        if ix < 0 || iy < 0 || ix >= width as i32 || iy >= height as i32 {
            return [0.0; 4];
        }
        let i = (iy as u32 * width + ix as u32) as usize * 4;
        let a = f32::from(rgba[i + 3]) / 255.0;
        // Premultiplied alpha avoids dark fringes when interpolating near transparent edges.
        [
            f32::from(rgba[i]) * a,
            f32::from(rgba[i + 1]) * a,
            f32::from(rgba[i + 2]) * a,
            a,
        ]
    };
    let c00 = fetch(x0, y0);
    let c10 = fetch(x0 + 1, y0);
    let c01 = fetch(x0, y0 + 1);
    let c11 = fetch(x0 + 1, y0 + 1);
    let mut out = [0.0f32; 4];
    for k in 0..4 {
        let top = c00[k] * (1.0 - fx) + c10[k] * fx;
        let bot = c01[k] * (1.0 - fx) + c11[k] * fx;
        out[k] = top * (1.0 - fy) + bot * fy;
    }
    out
}

/// Locate the first isolated letter (`E`) inside `EDITION` (`right_box`) using
/// bright-face columns so we can reuse the real anti-aliased `E` glyph verbatim.
fn find_first_right_letter_span(
    rgba: &[u8],
    width: u32,
    right_box: PatchRect,
) -> Option<(u32, u32)> {
    let mut x0 = None;
    let mut x1 = None;
    for x in right_box.x..right_box.x + right_box.w {
        let has_bright = (right_box.y..right_box.y + right_box.h).any(|y| {
            let i = (y * width + x) as usize * 4;
            rgba[i + 3] >= 200 && luma(&rgba[i..i + 4]) >= 1000
        });
        if has_bright {
            x0.get_or_insert(x);
            x1 = Some(x);
        } else if x0.is_some() {
            break;
        }
    }
    let (a, b) = (x0?, x1?);
    let letter_w = b - a + 1;
    // Must be a genuine single letter (not the entire right_box solid bar).
    if letter_w >= 8 && letter_w * 3 < right_box.w * 2 {
        Some((a, b))
    } else {
        None
    }
}
/// Quick luma for palette ranking (deterministic, no float, range 0..=2550).
fn luma(px: &[u8]) -> u32 {
    u32::from(px[0]) * 3 + u32::from(px[1]) * 6 + u32::from(px[2])
}

/// Fill/highlight/bevel/outline RGBA sampled from the untouched `EDITION`
/// glyphs by luma band so black outlines can never steal `fill`.
#[derive(Clone, Copy, Debug)]
struct GlyphPalette {
    fill: [u8; 4],
    highlight: [u8; 4],
    bevel: [u8; 4],
    outline: [u8; 4],
}

/// Sample the right `EDITION` word across four luma bands:
/// * `outline`: darkest opaque pixels (`luma < 250`, e.g. `#000000`)
/// * `bevel`: mid-dark 3D stone underside (`250 <= luma < 1200`, e.g. `#3D3938`)
/// * `fill`: mode of bright stone face pixels (`luma >= 1200`, e.g. `#D0C5C0`)
/// * `highlight`: brightest stone rim pixel (falling back to slightly lifted `fill`)
fn sample_palette(rgba: &[u8], width: u32, box_: PatchRect) -> Result<GlyphPalette, String> {
    let mut counts: Vec<([u8; 4], u32)> = Vec::new();
    for y in box_.y..box_.y + box_.h {
        for x in box_.x..box_.x + box_.w {
            let i = (y * width + x) as usize * 4;
            let px = &rgba[i..i + 4];
            if px[3] < 200 {
                continue;
            }
            let key = [px[0], px[1], px[2], 255];
            match counts.iter_mut().find(|(c, _)| *c == key) {
                Some((_, n)) => *n += 1,
                None => counts.push((key, 1)),
            }
        }
    }
    if counts.is_empty() {
        return Err("right word has no opaque glyph pixels to sample".to_owned());
    }

    let mut outline = [0u8, 0, 0, 255];
    let mut lo_luma = u32::MAX;
    let mut highlight = [255u8; 4];
    let mut hi_luma = 0u32;
    let mut fill_best = ([208u8, 197, 192, 255], 0u32);
    let mut bevel_best = ([61u8, 57, 56, 255], 0u32);
    let mut overall_bright = ([208u8, 197, 192, 255], 0u32);

    for (c, n) in counts {
        let l = luma(&c);
        if l < lo_luma {
            lo_luma = l;
            outline = c;
        }
        if l > hi_luma {
            hi_luma = l;
            highlight = c;
        }
        if l >= 1200 {
            if n > fill_best.1 {
                fill_best = (c, n);
            }
        } else if l >= 250 && n > bevel_best.1 {
            bevel_best = (c, n);
        }
        if l > 250 && n > overall_bright.1 {
            overall_bright = (c, n);
        }
    }

    let fill = if fill_best.1 > 0 {
        fill_best.0
    } else if overall_bright.1 > 0 {
        overall_bright.0
    } else {
        highlight
    };
    let bevel = if bevel_best.1 > 0 {
        bevel_best.0
    } else {
        // Synthetic two-tone fixture without explicit 3D underside: derive a
        // clean darker stone shade (~35% of face brightness).
        [
            (u16::from(fill[0]) * 35 / 100) as u8,
            (u16::from(fill[1]) * 35 / 100) as u8,
            (u16::from(fill[2]) * 35 / 100) as u8,
            255,
        ]
    };

    Ok(GlyphPalette {
        fill,
        highlight,
        bevel,
        outline,
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
    let reused_e_span = find_first_right_letter_span(rgba, width, right_box);

    // Snapshot untouched source RGBA (used purely read-only to copy/warp the
    // real `E` from `EDITION` into slot #1 of `MEGU`).
    let src_snapshot = if reused_e_span.is_some() {
        Some(rgba.to_vec())
    } else {
        None
    };

    let x_limit = gap_end as u32 - 1;
    let mut painter = Painter {
        rgba,
        width,
        height,
        x_limit,
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

    // Exact 3D stone proportions measured directly from `right_box.h`:
    let outline_f = ((right_box.h as f32) / 14.5).max(1.0);
    let outline_px = outline_f.round() as i32;
    let bevel_f = ((right_box.h as f32) * 0.145).max(1.0);
    let face_y0 = right_box.y as f32 + outline_f;
    let face_h = (right_box.h as f32 - 2.0 * outline_f - bevel_f).max(6.0);

    // Match `EDITION`'s exact letter width (`~42 px` at 512x64) and inter-letter gap.
    let avail_w = (left_box.w as f32 - 2.0 * outline_f).max(16.0);
    let letter_gap = ((right_box.h as f32) * 0.11).clamp(1.5, avail_w * 0.08);
    let glyph_w = ((avail_w - 3.0 * letter_gap) / 4.0).max(4.0);
    let total_w = 4.0 * glyph_w + 3.0 * letter_gap;
    let base_x = left_box.x as f32 + ((left_box.w as f32 - total_w) * 0.5).max(outline_f);

    // Central vanishing-point perspective with quadratic center falloff (`u²`):
    // far-left `M` mirrors far-right `N` cleanly, while `E` gets only a gentle,
    // natural lean and `G`/`U` stand nearly upright beside `EDITION`'s `E`.
    let cx_f = (width as f32) * 0.5;
    let max_edge_lean = face_h * 0.16;
    let lean_at = |raw_x: f32, y_frac: f32| -> f32 {
        let u = ((cx_f - raw_x) / cx_f).clamp(0.0, 1.0);
        u * u * max_edge_lean * y_frac
    };
    let invert_lean = |sx: f32, y_frac: f32| -> f32 {
        // Two fixed-point iterations invert `sx = raw_x + lean_at(raw_x, y_frac)` to < 0.01 px.
        let mut rx = sx - lean_at(sx, y_frac);
        rx = sx - lean_at(rx, y_frac);
        rx
    };

    // Blend helper for crisp stone lip shading matching EDITION.
    let blend = |a: [u8; 4], b: [u8; 4], num: u16, den: u16| -> [u8; 4] {
        [
            ((u16::from(a[0]) * (den - num) + u16::from(b[0]) * num) / den) as u8,
            ((u16::from(a[1]) * (den - num) + u16::from(b[1]) * num) / den) as u8,
            ((u16::from(a[2]) * (den - num) + u16::from(b[2]) * num) / den) as u8,
            255,
        ]
    };
    let top_edge_shade = blend(palette.fill, palette.bevel, 14, 100);
    let bevel_lip_shade = blend(palette.fill, palette.bevel, 44, 100);

    // 4×4 supersampled smooth anti-aliased rasterizer for M, (E fallback), G, U!
    // Working on a 4x grid gives smooth slanted edges, crisp squared outlines,
    // exact 3D inward extrusion, and anti-aliased transitions matching EDITION.
    const SS: usize = 4;
    let ss_f = SS as f32;
    let mw = x_limit as usize;
    let mh = height as usize;
    let hi_w = mw * SS;
    let hi_h = mh * SS;
    let mut hi_face = vec![false; hi_w * hi_h];

    let skip_e = reused_e_span.is_some();
    for li in 0..4usize {
        if li == 1 && skip_e {
            continue;
        }
        let lx0 = base_x + li as f32 * (glyph_w + letter_gap);
        // Bounding hi-res box with generous lean padding
        let y_min = ((face_y0 - 1.0).max(0.0) * ss_f) as usize;
        let y_max = (((face_y0 + face_h + 1.0) * ss_f).ceil() as usize).min(hi_h);
        let x_min = ((lx0 - 2.0).max(0.0) * ss_f) as usize;
        let x_max = (((lx0 + glyph_w + max_edge_lean + 2.0) * ss_f).ceil() as usize).min(hi_w);
        for hy in y_min..y_max {
            let sy = (hy as f32 + 0.5) / ss_f;
            let v = (sy - face_y0) / face_h;
            if !(0.0..=1.0).contains(&v) {
                continue;
            }
            let y_frac = 1.0 - v;
            for hx in x_min..x_max {
                let sx = (hx as f32 + 0.5) / ss_f;
                let raw_x = invert_lean(sx, y_frac);
                let u = (raw_x - lx0) / glyph_w;
                if glyph_face_contains(li, u, v) {
                    hi_face[hy * hi_w + hx] = true;
                }
            }
        }
    }

    // Pass 1 (4× grid): 3D extruded stone underside pulling down + inward toward `cx`.
    let hi_bevel_steps = (bevel_f * ss_f).round().max(1.0) as usize;
    let mut hi_bevel = vec![false; hi_w * hi_h];
    for hy in 0..hi_h {
        for hx in 0..hi_w {
            if !hi_face[hy * hi_w + hx] {
                continue;
            }
            let sx = (hx as f32 + 0.5) / ss_f;
            let u_center = ((cx_f - sx) / cx_f).clamp(0.0, 1.0);
            for step in 1..=hi_bevel_steps {
                let by = hy + step;
                if by >= hi_h {
                    break;
                }
                let bx = hx + (u_center * 0.36 * step as f32).round() as usize;
                if bx < hi_w && !hi_face[by * hi_w + bx] {
                    hi_bevel[by * hi_w + bx] = true;
                }
                if !hi_face[by * hi_w + hx] {
                    hi_bevel[by * hi_w + hx] = true;
                }
            }
        }
    }

    // Pass 2 (4× grid): Crisp squared outer border via separable 1D box dilation (O(W*H)!).
    let hi_rad = (outline_f * ss_f).round().max(1.0) as isize;
    let mut hi_horiz = vec![false; hi_w * hi_h];
    for hy in 0..hi_h {
        let row_off = hy * hi_w;
        let mut count = 0i32;
        for hx in 0..hi_w {
            let add_x = hx as isize + hi_rad;
            if add_x < hi_w as isize {
                let idx = row_off + add_x as usize;
                if hi_face[idx] || hi_bevel[idx] {
                    count += 1;
                }
            }
            if hx == 0 {
                for init_x in 0..=(hi_rad.min(hi_w as isize - 1)) {
                    let idx = row_off + init_x as usize;
                    if hi_face[idx] || hi_bevel[idx] {
                        count += 1;
                    }
                }
                if add_x < hi_w as isize {
                    let idx = row_off + add_x as usize;
                    if hi_face[idx] || hi_bevel[idx] {
                        count -= 1;
                    }
                }
            }
            let rem_x = hx as isize - hi_rad - 1;
            if rem_x >= 0 {
                let idx = row_off + rem_x as usize;
                if hi_face[idx] || hi_bevel[idx] {
                    count -= 1;
                }
            }
            hi_horiz[row_off + hx] = count > 0;
        }
    }
    let mut hi_outline = vec![false; hi_w * hi_h];
    for hx in 0..hi_w {
        let mut count = 0i32;
        for hy in 0..hi_h {
            if hy == 0 {
                for init_y in 0..=(hi_rad.min(hi_h as isize - 1)) {
                    if hi_horiz[init_y as usize * hi_w + hx] {
                        count += 1;
                    }
                }
            } else {
                let add_y = hy as isize + hi_rad;
                if add_y < hi_h as isize && hi_horiz[add_y as usize * hi_w + hx] {
                    count += 1;
                }
                let rem_y = hy as isize - hi_rad - 1;
                if rem_y >= 0 && hi_horiz[rem_y as usize * hi_w + hx] {
                    count -= 1;
                }
            }
            hi_outline[hy * hi_w + hx] = count > 0;
        }
    }

    // Pass 3: Downsample 4×4 subpixels with premultiplied RGBA averaging -> butter-smooth edges!
    let lip_thresh = SS;
    for y in 0..mh {
        for x in 0..mw {
            let mut acc_r = 0.0f32;
            let mut acc_g = 0.0f32;
            let mut acc_b = 0.0f32;
            let mut acc_a = 0.0f32;
            for sy in 0..SS {
                let hy = y * SS + sy;
                for sx in 0..SS {
                    let hx = x * SS + sx;
                    let idx = hy * hi_w + hx;
                    let col = if hi_face[idx] {
                        let above_bevel =
                            hy + lip_thresh < hi_h && hi_bevel[(hy + lip_thresh) * hi_w + hx];
                        let near_top = hy < lip_thresh || !hi_face[(hy - lip_thresh) * hi_w + hx];
                        if above_bevel {
                            Some(palette.highlight)
                        } else if near_top && bevel_f >= 3.5 {
                            Some(top_edge_shade)
                        } else {
                            Some(palette.fill)
                        }
                    } else if hi_bevel[idx] {
                        let just_below_face =
                            hy >= lip_thresh && hi_face[(hy - lip_thresh) * hi_w + hx];
                        if just_below_face && bevel_f >= 3.5 {
                            Some(bevel_lip_shade)
                        } else {
                            Some(palette.bevel)
                        }
                    } else if hi_outline[idx] {
                        Some(palette.outline)
                    } else {
                        None
                    };
                    if let Some(c) = col {
                        acc_r += f32::from(c[0]);
                        acc_g += f32::from(c[1]);
                        acc_b += f32::from(c[2]);
                        acc_a += 1.0;
                    }
                }
            }
            if acc_a > 0.0 {
                let samples = (SS * SS) as f32;
                let a_out = ((acc_a / samples) * 255.0).round().clamp(0.0, 255.0) as u8;
                if a_out > 0 {
                    let r_out = (acc_r / acc_a).round().clamp(0.0, 255.0) as u8;
                    let g_out = (acc_g / acc_a).round().clamp(0.0, 255.0) as u8;
                    let b_out = (acc_b / acc_a).round().clamp(0.0, 255.0) as u8;
                    painter.put(x as i32, y as i32, [r_out, g_out, b_out, a_out]);
                }
            }
        }
    }

    // Pass 4: Reuse the REAL anti-aliased stone `E` directly from `EDITION`
    // into slot #1 of `MEGU`, bilinearly perspective-warped to slot #1's exact
    // central vanishing-point angle!
    if let (Some((e_x0, e_x1)), Some(src)) = (reused_e_span, src_snapshot.as_deref()) {
        let e_src_w = (e_x1 - e_x0 + 1) as f32;
        let pad = outline_f.ceil() as i32;
        let src_min_x = (e_x0 as i32 - pad).max(0) as f32;
        let src_max_x = (e_x1 as i32 + pad).min(width as i32 - 1) as f32;
        let dst_lx0 = base_x + 1.0 * (glyph_w + letter_gap);
        let scale_x = e_src_w / glyph_w;
        let y_start = right_box.y as i32;
        let y_end = (right_box.y + right_box.h) as i32;
        let x_start = ((dst_lx0 - outline_f - 2.0).floor() as i32).max(0);
        let x_end = ((dst_lx0 + glyph_w + max_edge_lean + outline_f + 2.0).ceil() as i32)
            .min(x_limit as i32);
        for y in y_start..y_end {
            let v = ((y as f32 - face_y0) / face_h).clamp(0.0, 1.0);
            let y_frac = 1.0 - v;
            for x in x_start..x_end {
                let raw_x = invert_lean(x as f32, y_frac);
                let src_x = e_x0 as f32 + (raw_x - dst_lx0) * scale_x;
                if src_x < src_min_x || src_x > src_max_x {
                    continue;
                }
                let s = sample_rgba_bilinear(src, width, height, src_x, y as f32);
                if s[3] > 0.02 {
                    let a_u8 = (s[3] * 255.0).round().clamp(0.0, 255.0) as u8;
                    let r_u8 = (s[0] / s[3]).round().clamp(0.0, 255.0) as u8;
                    let g_u8 = (s[1] / s[3]).round().clamp(0.0, 255.0) as u8;
                    let b_u8 = (s[2] / s[3]).round().clamp(0.0, 255.0) as u8;
                    // Alpha-over onto painter buffer so soft outer AA blends cleanly.
                    let i = (y as u32 * width + x as u32) as usize * 4;
                    let dst_a = f32::from(painter.rgba[i + 3]) / 255.0;
                    let out_a = s[3] + dst_a * (1.0 - s[3]);
                    if out_a > 0.001 {
                        let cr = (s[0] + f32::from(painter.rgba[i]) * dst_a * (1.0 - s[3])) / out_a;
                        let cg =
                            (s[1] + f32::from(painter.rgba[i + 1]) * dst_a * (1.0 - s[3])) / out_a;
                        let cb =
                            (s[2] + f32::from(painter.rgba[i + 2]) * dst_a * (1.0 - s[3])) / out_a;
                        let oa = (out_a * 255.0).round().clamp(0.0, 255.0) as u8;
                        painter.put(
                            x,
                            y,
                            [
                                cr.round().clamp(0.0, 255.0) as u8,
                                cg.round().clamp(0.0, 255.0) as u8,
                                cb.round().clamp(0.0, 255.0) as u8,
                                oa.max(a_u8),
                            ],
                        );
                    } else {
                        painter.put(x, y, [r_u8, g_u8, b_u8, a_u8]);
                    }
                }
            }
        }
    }
    let _ = outline_px;

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
    let custom_branding = custom_title_branding();
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

    // Mod JAR splash overlays are custom branding and therefore opt-in.
    for jar in mod_jars.iter().take(MAX_MOD_JARS).filter(|_| custom_branding) {
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

    // Custom Megu splashes are opt-in; they remain the fallback only when no
    // official splash pool was available at all.
    if custom_branding || out.splashes.is_empty() {
        for s in MEGU_SPLASHES {
            let s = s.to_owned();
            if !out.splashes.contains(&s) {
                out.splashes.push(s);
            }
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
                        Ok(mut img) => {
                            img.crop_title_padding(LOGO_SIZE, 64);
                            out.logo = Some(img);
                        }
                        Err(reason) => out
                            .notes
                            .push(format!("{}: logo PNG rejected: {reason}", meta.name)),
                    }
                } else if meta.name == JAR_TITLE_EDITION {
                    match RgbaImage::decode_png(&bytes) {
                        Ok(mut img) => {
                            if custom_title_branding() {
                                match patch_edition_banner_to_megu(
                                    &mut img.rgba,
                                    img.width,
                                    img.height,
                                ) {
                                    Ok(report) => {
                                        img.crop_title_padding(EDITION_SIZE, 16);
                                        out.edition_patch = Some(report);
                                        out.edition = Some(img);
                                    }
                                    Err(reason) => out.notes.push(format!(
                                        "{}: edition patch failed closed: {reason}",
                                        meta.name
                                    )),
                                }
                            } else {
                                img.crop_title_padding(EDITION_SIZE, 16);
                                out.edition = Some(img);
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

/// Resolve a client JAR and the mod JARs that can contribute title splashes.
pub fn title_branding_sources(paths: &SessionPaths) -> (Option<PathBuf>, Vec<PathBuf>) {
    (resolve_title_jar(paths), title_mod_jars(paths))
}

pub(crate) fn configured_title_jar(paths: &SessionPaths) -> Option<PathBuf> {
    VanillaAssetResolver::resolve_session(paths)
}

pub(crate) fn resolve_title_jar(paths: &SessionPaths) -> Option<PathBuf> {
    VanillaAssetResolver::resolve_session(paths)
}

/// Custom Megu title branding is opt-in. By default the player's official
/// vanilla edition strip and splash pool remain byte/pixel faithful.
fn custom_title_branding() -> bool {
    std::env::var("MEGU_CUSTOM_BRANDING").is_ok_and(|value| value == "1")
}

pub(crate) fn title_mod_jars(paths: &SessionPaths) -> Vec<PathBuf> {
    let Some(game_dir) = paths.game_dir.as_deref() else {
        return Vec::new();
    };
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
    mods
}

/// The six cubemap faces first come from resource packs, then from the
/// launcher asset index, then from a JAR with full-size textures.
pub fn load_title_panorama_assets(
    paths: &SessionPaths,
    jar: Option<&Path>,
    packs: &[PathBuf],
) -> VanillaGuiAssets {
    let mut assets = load_gui_stack(None, packs);
    if assets.has_panorama() {
        return assets;
    }
    if let Some((root, specs)) = panorama_asset_index(paths) {
        let mut remaining = MAX_DECODED_RGBA_BYTES;
        for id in ALL_GUI_SPRITES.into_iter().filter(|id| id.is_panorama_face()) {
            if assets.sprites.contains_key(&id) {
                continue;
            }
            let key = id.jar_entry().trim_start_matches("assets/");
            if let Some(image) = indexed_title_image(&root, &specs, key, remaining) {
                remaining -= image.rgba.len();
                assets.sprites.insert(id, image);
            }
        }
    }
    if !assets.has_panorama() {
        let jar_assets = load_gui_stack(jar, &[]);
        for (id, image) in jar_assets.sprites {
            // The version JAR may contain single-pixel placeholders for
            // cubemap faces stored as indexed assets (including 1.21.1).
            if id.is_panorama_face() && image.width > 1 {
                assets.sprites.entry(id).or_insert(image);
            }
        }
    }
    assets
}

fn panorama_asset_index(paths: &SessionPaths) -> Option<(PathBuf, Vec<AssetObjectSpec>)> {
    let index_id = paths.asset_index.as_deref().unwrap_or("17");
    let root = discover_local_assets_dir(index_id, paths)?;
    let bytes = read_bounded_file(
        &root.join("indexes").join(format!("{index_id}.json")),
        MAX_ASSET_INDEX_BYTES as usize,
    )
    .ok()?;
    let specs = parse_asset_objects(std::str::from_utf8(&bytes).ok()?).ok()?;
    Some((root, specs))
}

fn indexed_title_image(
    root: &Path,
    specs: &[AssetObjectSpec],
    key: &str,
    remaining: usize,
) -> Option<RgbaImage> {
    let spec = specs.iter().find(|spec| spec.logical_path == key)?;
    if spec.size > MAX_TITLE_ENTRY_BYTES as u64 {
        return None;
    }
    let object = root.join("objects").join(&spec.hash[..2]).join(&spec.hash);
    let bytes = read_bounded_file(&object, spec.size as usize).ok()?;
    if bytes.len() as u64 != spec.size || megu_core::crypto::sha1_hex(&bytes) != spec.hash {
        return None;
    }
    check_png_budget(&bytes, remaining).ok()?;
    RgbaImage::decode_png(&bytes).ok()
}

/// Screen-space layout for the title branding set in physical pixels.
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

/// Center the Vanilla-sized wordmark and edition strip in the same
/// logical header band as the title menu, shrinking both on small windows.
pub fn title_brand_layout(screen_w: u32, screen_h: u32, gui_scale: u32) -> TitleBrandLayout {
    let gui_scale = gui_scale.max(1);
    let gui_px = gui_scale as f32;
    let top = 14.0 * gui_px;
    let buttons_top = megu_ui::layout::title_buttons_top(screen_h / gui_scale) as f32 * gui_px;
    let available_w = (screen_w as f32 - 8.0 * gui_px).max(0.0);
    let available_h = (buttons_top - 10.0 * gui_px).min(screen_h as f32) - top;
    let gap = 2.0;
    let height = (LOGO_SIZE.1 + EDITION_SIZE.1) as f32 + gap;
    let scale = gui_px
        .min(available_w / LOGO_SIZE.0 as f32)
        .min((available_h / height).max(0.0));
    let logo_w = LOGO_SIZE.0 as f32 * scale;
    let logo_h = LOGO_SIZE.1 as f32 * scale;
    let ed_w = EDITION_SIZE.0 as f32 * scale;
    let ed_h = EDITION_SIZE.1 as f32 * scale;
    let logo_x = (screen_w as f32 - logo_w) / 2.0;
    let ed_x = (screen_w as f32 - ed_w) / 2.0;
    let ed_y = top + logo_h + gap * scale;
    TitleBrandLayout {
        logo_rect: [logo_x, top, logo_w, logo_h],
        edition_rect: [ed_x, ed_y, ed_w, ed_h],
        splash_anchor_x: (logo_x + logo_w - 4.0 * gui_px).max(0.0) as i32,
        splash_y: (ed_y - 9.0 * gui_px).max(0.0) as i32,
    }
}
