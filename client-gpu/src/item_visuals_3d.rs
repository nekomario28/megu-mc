// SPDX-License-Identifier: LGPL-3.0-or-later
//! Software item-icon visual helpers.
//!
//! This module intentionally stays independent of the GPU renderer: it turns
//! already decoded vanilla sprite RGBA into inventory/hotbar icons, applies
//! the enchanted-glint texture transform in CPU memory, and adapts official
//! extra assets for platform/window use. No Mojang asset bytes are bundled.

use std::collections::BTreeMap;
use std::path::PathBuf;

use crate::vanilla_i18n_data::VanillaTextsAndFonts;
use crate::vanilla_official_extras::{VanillaOfficialExtras, WindowIconRgba};

/// Maximum dimension accepted by the software isometric rasterizer.
///
/// GUI icons are normally 16×16 or 32×32. The cap keeps a malformed caller
/// dimension from becoming an unbounded allocation while leaving high-DPI
/// resource-pack icons comfortably in range.
pub const MAX_ISOMETRIC_ICON_SIZE: u32 = 1024;

/// Directional face lighting used by Minecraft's inventory block rendering.
pub const ISO_TOP_LIGHT: f32 = 1.0;
pub const ISO_LEFT_LIGHT: f32 = 0.80;
pub const ISO_RIGHT_LIGHT: f32 = 0.60;

/// One borrowed RGBA8 sprite in tightly packed row-major order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SpriteRgba<'a> {
    /// Texel width.
    pub width: u32,
    /// Texel height.
    pub height: u32,
    /// `width * height * 4` bytes, RGBA8.
    pub rgba: &'a [u8],
}

impl<'a> SpriteRgba<'a> {
    /// Construct a borrowed sprite view.
    pub const fn new(width: u32, height: u32, rgba: &'a [u8]) -> Self {
        Self {
            width,
            height,
            rgba,
        }
    }

    /// Whether dimensions are nonzero and exactly match the byte slice.
    pub fn is_valid(self) -> bool {
        rgba_len(self.width, self.height) == Some(self.rgba.len())
    }
}

fn rgba_len(width: u32, height: u32) -> Option<usize> {
    if width == 0 || height == 0 {
        return None;
    }
    usize::try_from(width)
        .ok()?
        .checked_mul(usize::try_from(height).ok()?)?
        .checked_mul(4)
}

fn in_unit_range(value: f32) -> bool {
    const EPSILON: f32 = 1.0e-6;
    (-EPSILON..=1.0 + EPSILON).contains(&value)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum IsoFace {
    Top,
    Left,
    Right,
}

/// Classify a point in the unit-square icon projection and return the face's
/// local UV. The projection uses vertical cube edges and a 2:1 isometric top
/// diamond. Shared edges are deterministic: top first, then left, then right.
fn iso_face_uv(x: f32, y: f32) -> Option<(IsoFace, f32, f32)> {
    let top_u = 2.0 * y + x - 0.5;
    let top_v = 2.0 * y - x + 0.5;
    if in_unit_range(top_u) && in_unit_range(top_v) {
        return Some((IsoFace::Top, top_u, top_v));
    }

    let left_u = 2.0 * x;
    let left_v = 2.0 * y - 0.5 - x;
    if in_unit_range(left_u) && in_unit_range(left_v) {
        return Some((IsoFace::Left, left_u, left_v));
    }

    let right_u = 2.0 * x - 1.0;
    let right_v = 2.0 * y + x - 1.5;
    if in_unit_range(right_u) && in_unit_range(right_v) {
        return Some((IsoFace::Right, right_u, right_v));
    }

    None
}

fn sample_nearest(sprite: &SpriteRgba<'_>, u: f32, v: f32) -> [u8; 4] {
    let u = u.clamp(0.0, 1.0);
    let v = v.clamp(0.0, 1.0);
    let x = ((u * sprite.width as f32) as u32).min(sprite.width - 1);
    let y = ((v * sprite.height as f32) as u32).min(sprite.height - 1);
    let index = (y as usize * sprite.width as usize + x as usize) * 4;
    sprite.rgba[index..index + 4].try_into().unwrap_or([0; 4])
}

fn shade_channel(channel: u8, light: f32, tint_channel: u8) -> u8 {
    (f32::from(channel) * light * (f32::from(tint_channel) / 255.0))
        .round()
        .clamp(0.0, 255.0) as u8
}

fn shade_pixel(pixel: [u8; 4], light: f32, tint: Option<[u8; 3]>) -> [u8; 4] {
    let tint = tint.unwrap_or([255; 3]);
    [
        shade_channel(pixel[0], light, tint[0]),
        shade_channel(pixel[1], light, tint[1]),
        shade_channel(pixel[2], light, tint[2]),
        pixel[3],
    ]
}

/// Software-rasterize a crisp Minecraft-style isometric cube icon.
///
/// `top`, `left`, and `right` are borrowed RGBA sprite views. `out_size` is
/// both width and height of the returned image. The output is tightly packed
/// RGBA8, fully transparent outside the hexagonal cube silhouette. Texel
/// lookup is nearest-neighbor; face lighting is top `1.0`, left `0.80`, and
/// right `0.60`. `top_tint` applies biome/grass-style multiplication before
/// the unit top-face light.
///
/// Invalid sprite views, zero dimensions, and sizes above
/// [`MAX_ISOMETRIC_ICON_SIZE`] yield an empty vector rather than panicking or
/// attempting an unbounded allocation.
pub fn render_isometric_block_icon_rgba(
    top: &SpriteRgba<'_>,
    left: &SpriteRgba<'_>,
    right: &SpriteRgba<'_>,
    out_size: u32,
    top_tint: Option<[u8; 3]>,
) -> Vec<u8> {
    if out_size == 0
        || out_size > MAX_ISOMETRIC_ICON_SIZE
        || !top.is_valid()
        || !left.is_valid()
        || !right.is_valid()
    {
        return Vec::new();
    }
    let Some(out_len) = rgba_len(out_size, out_size) else {
        return Vec::new();
    };
    let mut out = vec![0u8; out_len];
    let size = out_size as f32;

    for y in 0..out_size {
        for x in 0..out_size {
            let nx = (x as f32 + 0.5) / size;
            let ny = (y as f32 + 0.5) / size;
            let Some((face, u, v)) = iso_face_uv(nx, ny) else {
                continue;
            };
            let (sprite, light, tint) = match face {
                IsoFace::Top => (top, ISO_TOP_LIGHT, top_tint),
                IsoFace::Left => (left, ISO_LEFT_LIGHT, None),
                IsoFace::Right => (right, ISO_RIGHT_LIGHT, None),
            };
            let pixel = shade_pixel(sample_nearest(sprite, u, v), light, tint);
            let out_index = (y as usize * out_size as usize + x as usize) * 4;
            out[out_index..out_index + 4].copy_from_slice(&pixel);
        }
    }
    out
}

/// Generate isometric inventory icons for a deterministic block sprite map.
///
/// The input maps a block/item name to `(top, side, optional_top_tint)`. The
/// same side sprite is used for both visible vertical faces, while the two
/// faces still receive their different directional lighting. Entries whose
/// sprites are invalid are omitted. Output keys are unchanged so callers can
/// replace only the cube-block entries in a larger item icon atlas.
pub fn upgrade_item_icon_atlas_with_3d_blocks<'a>(
    block_sprites: &BTreeMap<String, (SpriteRgba<'a>, SpriteRgba<'a>, Option<[u8; 3]>)>,
    out_size: u32,
) -> BTreeMap<String, Vec<u8>> {
    block_sprites
        .iter()
        .filter_map(|(name, (top, side, top_tint))| {
            let rgba = render_isometric_block_icon_rgba(top, side, side, out_size, *top_tint);
            (!rgba.is_empty()).then(|| (name.clone(), rgba))
        })
        .collect()
}

/// Top, left, right, optional side-overlay sprite, and optional top tint for
/// one distinct-face isometric block icon.
pub type IsometricFaceSprites<'a> = (
    SpriteRgba<'a>,
    SpriteRgba<'a>,
    SpriteRgba<'a>,
    Option<SpriteRgba<'a>>,
    Option<[u8; 3]>,
);

/// Generate isometric icons when the two visible vertical faces have distinct
/// sprites. `side_overlay` is alpha-composited over both vertical faces before
/// lighting, matching side-overlay block sprites such as grass while keeping
/// the legacy single-side entry point intact.
pub fn upgrade_item_icon_atlas_with_3d_block_faces<'a>(
    block_sprites: &BTreeMap<String, IsometricFaceSprites<'a>>,
    out_size: u32,
) -> BTreeMap<String, Vec<u8>> {
    block_sprites
        .iter()
        .filter_map(|(name, (top, left, right, side_overlay, top_tint))| {
            let rgba = if let Some(overlay) = side_overlay {
                let left_composited = composite_sprite_overlay_rgba(left, overlay);
                let right_composited = composite_sprite_overlay_rgba(right, overlay);
                let left_sprite = SpriteRgba::new(left.width, left.height, &left_composited);
                let right_sprite = SpriteRgba::new(right.width, right.height, &right_composited);
                render_isometric_block_icon_rgba(
                    top,
                    &left_sprite,
                    &right_sprite,
                    out_size,
                    *top_tint,
                )
            } else {
                render_isometric_block_icon_rgba(top, left, right, out_size, *top_tint)
            };
            (!rgba.is_empty()).then(|| (name.clone(), rgba))
        })
        .collect()
}

fn composite_sprite_overlay_rgba(base: &SpriteRgba<'_>, overlay: &SpriteRgba<'_>) -> Vec<u8> {
    if !base.is_valid() || !overlay.is_valid() {
        return base.rgba.to_vec();
    }
    let mut out = base.rgba.to_vec();
    for y in 0..base.height {
        for x in 0..base.width {
            let u = (x as f32 + 0.5) / base.width as f32;
            let v = (y as f32 + 0.5) / base.height as f32;
            let over = sample_nearest(overlay, u, v);
            let index = (y as usize * base.width as usize + x as usize) * 4;
            let alpha = f32::from(over[3]) / 255.0;
            for channel in 0..3 {
                out[index + channel] = (f32::from(over[channel]) * alpha
                    + f32::from(out[index + channel]) * (1.0 - alpha))
                    .round() as u8;
            }
            out[index + 3] = out[index + 3].max(over[3]);
        }
    }
    out
}

/// Vanilla-style enchanted-glint UV translation and rotation.
///
/// The two diagonal scroll periods match the texture-matrix periods used by
/// the vanilla 1.21.1 item/entity glint setup at its default 50% glint-speed
/// accessibility setting: the negative U phase has an effective 27.5 second
/// period and the positive V phase a 7.5 second period. The returned angle is
/// the legacy GUI glint tilt, `-50` degrees, as used for item-icon composition
/// here.
pub fn enchanted_glint_uv_transform(time_secs: f32) -> ([f32; 2], f32) {
    if !time_secs.is_finite() {
        return ([0.0, 0.0], -50.0);
    }
    const DEFAULT_TIME_SCALE: f32 = 8.0 * 0.5;
    const U_PERIOD_MS: f32 = 110_000.0;
    const V_PERIOD_MS: f32 = 30_000.0;
    let scaled_ms = time_secs * 1_000.0 * DEFAULT_TIME_SCALE;
    let u_phase = (scaled_ms / U_PERIOD_MS).rem_euclid(1.0);
    let v_phase = (scaled_ms / V_PERIOD_MS).rem_euclid(1.0);
    ([-u_phase, v_phase], -50.0)
}

fn wrap01(value: f32) -> f32 {
    value.rem_euclid(1.0)
}

const ENCHANT_GLINT_PURPLE_TINT: [u8; 3] = [128, 64, 204];

fn procedural_glint_pixel(u: f32, v: f32) -> [u8; 4] {
    // Deterministic sparse diagonal bands. This is only a fallback for a
    // missing texture; when vanilla's glint PNG is available it is sampled.
    let band = wrap01(u * 3.0 + v * 5.0);
    let fine = wrap01(u * 17.0 - v * 11.0);
    let strength = if band < 0.12 {
        0.55 + 0.45 * (1.0 - band / 0.12)
    } else if fine < 0.035 {
        0.30
    } else {
        0.0
    };
    let intensity = (255.0 * strength).round() as u8;
    [
        intensity,
        intensity,
        intensity,
        (190.0 * strength).round() as u8,
    ]
}

fn glint_pixel(glint_rgba: Option<(&[u8], u32, u32)>, u: f32, v: f32) -> [u8; 4] {
    if let Some((rgba, width, height)) = glint_rgba {
        let sprite = SpriteRgba::new(width, height, rgba);
        if sprite.is_valid() {
            return sample_nearest(&sprite, u, v);
        }
    }
    procedural_glint_pixel(u, v)
}

/// Composite a vanilla-style additive enchanted glint over an RGBA icon.
///
/// The base alpha mask is authoritative: transparent texels are copied
/// byte-for-byte and never receive shimmer. Opaque or partially transparent
/// texels keep their original alpha while RGB receives additive purple glint.
/// When `glint_rgba` is absent or malformed, a deterministic procedural purple
/// shimmer is used instead.
pub fn composite_enchanted_glint_icon(
    base_rgba: &[u8],
    width: u32,
    height: u32,
    glint_rgba: Option<(&[u8], u32, u32)>,
    time_secs: f32,
) -> Vec<u8> {
    if rgba_len(width, height) != Some(base_rgba.len()) {
        return Vec::new();
    }
    let mut out = base_rgba.to_vec();
    let ([du, dv], angle_degrees) = enchanted_glint_uv_transform(time_secs);
    let (sin, cos) = angle_degrees.to_radians().sin_cos();

    for y in 0..height {
        for x in 0..width {
            let index = (y as usize * width as usize + x as usize) * 4;
            if out[index + 3] == 0 {
                continue;
            }
            let nx = (x as f32 + 0.5) / width as f32 - 0.5;
            let ny = (y as f32 + 0.5) / height as f32 - 0.5;
            // Vanilla scales glint UVs by 8 for item icons before rotating.
            let sx = nx * 8.0;
            let sy = ny * 8.0;
            let gu = wrap01(sx * cos - sy * sin + du);
            let gv = wrap01(sx * sin + sy * cos + dv);
            let glint = glint_pixel(glint_rgba, gu, gv);
            let alpha_weight = (f32::from(glint[3]) / 255.0) * (f32::from(out[index + 3]) / 255.0);
            for channel in 0..3 {
                let purple = f32::from(ENCHANT_GLINT_PURPLE_TINT[channel]) / 255.0;
                let addition = f32::from(glint[channel]) * alpha_weight * purple;
                out[index + channel] =
                    out[index + channel].saturating_add(addition.round().clamp(0.0, 255.0) as u8);
            }
        }
    }
    out
}

fn valid_window_icon(icon: &WindowIconRgba) -> bool {
    rgba_len(icon.width, icon.height) == Some(icon.rgba.len())
}

/// Pick the preferred official window icon and build a `winit` icon.
///
/// Square 32×32, 48×48, then 128×128 assets are preferred in that order
/// (covering the common taskbar, alt-tab, and high-DPI cases). If none is
/// available, the largest valid decoded icon is used. Malformed byte lengths
/// are skipped and a total absence yields `None`.
pub fn build_winit_window_icon(extras: &VanillaOfficialExtras) -> Option<winit::window::Icon> {
    for size in [32, 48, 128] {
        if let Some(icon) = extras
            .window_icons
            .iter()
            .filter(|icon| valid_window_icon(icon))
            .find(|icon| icon.width == size && icon.height == size)
        {
            if let Ok(built) =
                winit::window::Icon::from_rgba(icon.rgba.clone(), icon.width, icon.height)
            {
                return Some(built);
            }
        }
    }
    extras
        .window_icons
        .iter()
        .filter(|icon| valid_window_icon(icon))
        .max_by_key(|icon| u64::from(icon.width) * u64::from(icon.height))
        .and_then(|icon| {
            winit::window::Icon::from_rgba(icon.rgba.clone(), icon.width, icon.height).ok()
        })
}

/// Return canonical tuples for Vanilla's official built-in resource packs.
///
/// `programmer_art` is emitted before `high_contrast` when both are present;
/// absent entries are omitted. The path is the already hash-resolved object in
/// the user's assets store retained by [`VanillaOfficialExtras`].
pub fn discover_official_builtin_packs(
    extras: &VanillaOfficialExtras,
) -> Vec<(String, String, PathBuf)> {
    ["programmer_art", "high_contrast"]
        .into_iter()
        .filter_map(|wanted| {
            extras
                .builtin_resource_packs
                .iter()
                .find(|pack| pack.id.strip_prefix("minecraft:").unwrap_or(&pack.id) == wanted)
                .map(|pack| {
                    (
                        pack.id.clone(),
                        pack.display_name.clone(),
                        pack.object_path.clone(),
                    )
                })
        })
        .collect()
}

fn normalized_text_lines(text: &str) -> Vec<String> {
    let mut lines = text.lines().map(str::to_owned).collect::<Vec<_>>();
    while lines.first().is_some_and(|line| line.trim().is_empty()) {
        lines.remove(0);
    }
    while lines.last().is_some_and(|line| line.trim().is_empty()) {
        lines.pop();
    }
    lines
}

fn append_credit_block(lines: &mut Vec<String>, mut block: Vec<String>) {
    while block.first().is_some_and(|line| line.trim().is_empty()) {
        block.remove(0);
    }
    while block.last().is_some_and(|line| line.trim().is_empty()) {
        block.pop();
    }
    if block.is_empty() {
        return;
    }
    if !lines.is_empty() && lines.last().is_some_and(|line| !line.is_empty()) {
        lines.push(String::new());
    }
    lines.extend(block);
}

/// Merge the end poem, structured `credits.json`, and post-credits text into
/// the line stream consumed by `EndCreditsState`.
///
/// Each nonempty block appears in Vanilla order: `end.txt`, structured credit
/// sections/disciplines/names, then `postcredits.txt`. Outer blank lines are
/// normalized and exactly one separator is inserted between blocks so missing
/// or resource-pack-supplied text cannot concatenate unrelated lines.
pub fn merged_end_credits_lines(
    texts: &VanillaTextsAndFonts,
    extras: &VanillaOfficialExtras,
) -> Vec<String> {
    let mut lines = Vec::new();
    if let Some(end) = texts.end_text.as_deref() {
        append_credit_block(&mut lines, normalized_text_lines(end));
    }
    append_credit_block(&mut lines, extras.credits_lines());
    if let Some(postcredits) = texts.postcredits_text.as_deref() {
        append_credit_block(&mut lines, normalized_text_lines(postcredits));
    }
    lines
}
