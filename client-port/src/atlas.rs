// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later

//! Texture atlas packing, alpha-weighted mipmapping, `.mcmeta` animation
//! timing tables and baked-quad UV remapping — pure `std`, deterministic and
//! fail-closed.
//!
//! [`AtlasBuilder`] packs arbitrary RGBA8 sprites (16x16 blocks, 32x32 HD
//! textures, any size a resource pack ships) into a power-of-two atlas with
//! an optional per-sprite padding border that keeps mipmapped edges from
//! bleeding neighbours into each other. Sprites are laid out by a
//! deterministic shelf packer: identical inputs always produce a
//! byte-identical atlas, which keeps texture uploads cache-stable.
//!
//! The padding border's fill is controlled by [`PaddingMode`], set builder-
//! wide with [`AtlasBuilder::set_padding_mode`] or per sprite with
//! [`AtlasBuilder::add_sprite_with_mode`]: [`PaddingMode::Transparent`] (the
//! default, the historical `(0, 0, 0, 0)` gutter), [`PaddingMode::ClampEdge`]
//! which replicates the nearest sprite edge texel outward, or
//! [`PaddingMode::WrapTile`] which wraps texels from the opposite sprite
//! edge so `fract`-wrapped shader sampling tiles seamlessly. The fill covers
//! the full `1..=padding` border on all four sides including the corners.
//! [`AtlasBuilder::for_blocks`] presets [`PaddingMode::WrapTile`] for
//! tile-safe block atlases.
//!
//! [`build_mipmaps`] downsamples a packed atlas with alpha-weighted
//! averaging: colour channels are weighted by each texel's alpha, so fully
//! transparent texels (`a == 0`) contribute nothing and cutout foliage,
//! flowers and 3D-model leaves keep their pure colours instead of picking up
//! a black fringe at lower mip levels.
//!
//! [`build_mipmaps_linear`] is the gamma-correct variant: RGB is averaged in
//! linear sRGB space through a deterministic 256-entry decode LUT and
//! encoded back to sRGB, so averaging sRGB 0 and 255 yields ~188 instead of
//! the naive ~128. Its optional `cutout_alpha_cutoff` rescales each level's
//! alpha — deterministically binary-searching a factor in `1.0..=4.0` — so
//! the fraction of texels passing the cutoff tracks the base image and
//! distant cutout foliage keeps its silhouette instead of vanishing.
//!
//! [`SpriteAnimation`] is the client-side half of the `.mcmeta` animation
//! section: it resolves a vertical strip sprite (`height = frame_count *
//! frame_width`) or a custom index/duration table into a deterministic
//! `(frame, next_frame, blend_alpha)` answer per tick, including smooth
//! `interpolate: true` blending for water, lava and portal sprites.
//!
//! [`remap_baked_quad_uvs`] (and its half-texel-inset variant) maps a
//! [`BakedModelQuad`]'s vanilla `0..16` UVs into the sprite's sub-rect of a
//! [`PackedAtlas`], [`PackedAtlas::tile_rect`] exposes the same sub-rect in
//! tile-safe form for shaders that wrap sprite UVs with `fract` and sample
//! with `textureSampleGrad`, and [`apply_biome_tint`] performs the cheap RGB
//! multiply biomes apply to foliage and grass without touching alpha.
//!
//! Every limit is fail-closed: bad sprite dimensions, bad RGBA lengths,
//! duplicate names, atlas overflow, mip chains that would drop below 1x1 and
//! out-of-range animation tables all return an error rather than guessing.
//! No Mojang or third-party texture data is referenced or bundled anywhere in
//! this file: sprites arrive as caller-supplied RGBA8 buffers.

use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;

use crate::model_baker::{BakedModelQuad, MODEL_UNITS_PER_BLOCK};

// ---------------------------------------------------------------------------
// Limits
// ---------------------------------------------------------------------------

/// Default atlas edge cap: 4096 texels per axis. Low-spec Intel iGPUs sample
/// power-of-two atlases without tiling or fallback paths, and 4096 covers
/// the full vanilla + HD texture set with headroom.
pub const DEFAULT_ATLAS_MAX_DIM: u32 = 4096;

/// Hard ceiling for [`AtlasBuilder::set_max_dim`]. A fully packed RGBA8
/// atlas at this size is ~1 GiB of texels, which is already far beyond what
/// any supported target should upload as a single texture.
pub const MAX_ATLAS_DIM: u32 = 16384;

/// Largest per-sprite padding border [`AtlasBuilder::set_padding`] accepts.
/// `0` disables padding; `1..=MAX_SPRITE_PADDING` reserves that many
/// transparent texels on every side of each sprite so mip chains cannot
/// bleed a neighbour's colours into it.
pub const MAX_SPRITE_PADDING: u32 = 8;

/// Cap on the playback frame table of a [`SpriteAnimation`]. Legitimate
/// `.mcmeta` files use a handful of frames; the cap keeps a hostile frame
/// table from growing the timing table without bound.
pub const MAX_ANIMATION_FRAMES: usize = 1024;

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Every way atlas packing, mipmapping, animation tables and quad UV
/// remapping can fail. All variants are terminal: the caller drops the
/// atlas rather than guessing a fallback.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AtlasError {
    /// [`AtlasBuilder::build`] ran with no sprites queued.
    EmptyAtlas,
    /// A sprite name was empty.
    EmptySpriteName,
    /// A sprite `width` or `height` was zero.
    ZeroSpriteDimension {
        /// Offending sprite name.
        name: String,
    },
    /// `rgba.len()` did not equal `width * height * 4`.
    RgbaLengthMismatch {
        /// Offending sprite name.
        name: String,
        /// Required byte count (`width * height * 4`).
        expected: u64,
        /// Supplied byte count.
        actual: usize,
    },
    /// Two sprites were queued under the same name.
    DuplicateSprite(String),
    /// `max_dim` was outside `1..=MAX_ATLAS_DIM`.
    InvalidMaxDimension(u32),
    /// `padding` was outside `0..=MAX_SPRITE_PADDING`.
    InvalidPadding(u32),
    /// A sprite plus its padding border cannot fit inside the power-of-two
    /// atlas limit on at least one axis.
    SpriteExceedsAtlas {
        /// Offending sprite name.
        name: String,
        /// Sprite width in texels.
        width: u32,
        /// Sprite height in texels.
        height: u32,
        /// Configured padding border in texels.
        padding: u32,
        /// Effective power-of-two atlas edge limit the sprite was checked
        /// against.
        limit: u32,
    },
    /// The shelf packer could not fit every sprite within the atlas limit.
    AtlasFull {
        /// Effective power-of-two atlas edge limit.
        limit: u32,
    },
    /// A hand-constructed [`PackedAtlas`] has dimensions or a pixel buffer
    /// length that disagree. [`AtlasBuilder::build`] never emits one.
    MalformedAtlas {
        /// Required byte count (`width * height * 4`).
        expected: u64,
        /// Supplied byte count.
        actual: usize,
    },
    /// More mip levels were requested than the atlas dimensions support
    /// (a dimension would drop below 1 texel).
    TooManyMipLevels {
        /// Requested level count.
        requested: u32,
        /// Maximum supported level count.
        max: u32,
    },
    /// A [`BakedModelQuad`]'s texture id names no sprite in the atlas.
    TextureNotInAtlas(String),
}

impl fmt::Display for AtlasError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyAtlas => write!(f, "cannot build an atlas with no sprites"),
            Self::EmptySpriteName => write!(f, "sprite name must not be empty"),
            Self::ZeroSpriteDimension { name } => {
                write!(f, "sprite {name:?} has a zero width or height")
            }
            Self::RgbaLengthMismatch {
                name,
                expected,
                actual,
            } => write!(
                f,
                "sprite {name:?} has {actual} RGBA bytes, expected {expected} (width * height * 4)"
            ),
            Self::DuplicateSprite(name) => write!(f, "duplicate sprite name {name:?}"),
            Self::InvalidMaxDimension(dim) => {
                write!(f, "atlas max dimension {dim} outside 1..={MAX_ATLAS_DIM}")
            }
            Self::InvalidPadding(padding) => {
                write!(f, "sprite padding {padding} outside 0..={MAX_SPRITE_PADDING}")
            }
            Self::SpriteExceedsAtlas {
                name,
                width,
                height,
                padding,
                limit,
            } => write!(
                f,
                "sprite {name:?} ({width}x{height} + {padding} padding) cannot fit in a {limit}x{limit} atlas"
            ),
            Self::AtlasFull { limit } => {
                write!(f, "sprites do not fit within a {limit}x{limit} atlas")
            }
            Self::MalformedAtlas { expected, actual } => write!(
                f,
                "atlas pixel buffer has {actual} bytes, expected {expected} (width * height * 4)"
            ),
            Self::TooManyMipLevels { requested, max } => write!(
                f,
                "{requested} mip levels requested but atlas supports at most {max}"
            ),
            Self::TextureNotInAtlas(texture) => {
                write!(f, "texture {texture:?} is not packed in the atlas")
            }
        }
    }
}

impl Error for AtlasError {}

/// Every way a [`SpriteAnimation`] table can fail. All variants are
/// terminal: the caller drops the animation rather than guessing a
/// fallback.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AnimationError {
    /// `frame_size` was zero.
    ZeroFrameSize,
    /// `strip_height` is not a positive multiple of `frame_size`, so the
    /// strip does not describe whole square frames.
    InvalidStrip {
        /// Requested frame edge in texels.
        frame_size: u32,
        /// Requested strip height in texels.
        strip_height: u32,
    },
    /// The playback table was empty.
    EmptyFrameTable,
    /// The playback table exceeded [`MAX_ANIMATION_FRAMES`] entries.
    TooManyFrames(usize),
    /// A playback entry names a strip slot that does not exist.
    FrameIndexOutOfRange {
        /// Position of the offending entry in the playback table.
        position: usize,
        /// Requested strip slot.
        index: u32,
        /// Number of slots the strip actually holds.
        strip_frames: u32,
    },
    /// A playback entry had a zero-tick duration.
    ZeroFrameDuration {
        /// Position of the offending entry in the playback table.
        position: usize,
    },
}

impl fmt::Display for AnimationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ZeroFrameSize => write!(f, "animation frame size must be non-zero"),
            Self::InvalidStrip {
                frame_size,
                strip_height,
            } => write!(
                f,
                "strip height {strip_height} is not a positive multiple of frame size {frame_size}"
            ),
            Self::EmptyFrameTable => write!(f, "animation frame table is empty"),
            Self::TooManyFrames(count) => {
                write!(f, "{count} animation frames exceeds limit of {MAX_ANIMATION_FRAMES}")
            }
            Self::FrameIndexOutOfRange {
                position,
                index,
                strip_frames,
            } => write!(
                f,
                "animation frame {position} references strip slot {index}, but the strip holds {strip_frames}"
            ),
            Self::ZeroFrameDuration { position } => {
                write!(f, "animation frame {position} has a zero-tick duration")
            }
        }
    }
}

impl Error for AnimationError {}

// ---------------------------------------------------------------------------
// Atlas packing
// ---------------------------------------------------------------------------

/// Largest power of two `<= dim`. `dim` must be non-zero; the builder only
/// ever passes validated `max_dim` values in `1..=MAX_ATLAS_DIM`.
fn pow2_floor(dim: u32) -> u32 {
    debug_assert!(dim >= 1);
    let next = dim.next_power_of_two();
    if next == dim {
        dim
    } else {
        next / 2
    }
}

#[derive(Clone, Debug)]
struct SpriteData {
    width: u32,
    height: u32,
    rgba: Vec<u8>,
    mode: PaddingMode,
}

/// How the padding border reserved around a packed sprite is filled.
///
/// The border exists so mipmapped edges cannot bleed a neighbouring
/// sprite's colours into this one; the mode decides what the border texels
/// contain. The fill covers the full `1..=padding` border on all four
/// sides including the four corners. See [`AtlasBuilder::set_padding_mode`]
/// and [`AtlasBuilder::add_sprite_with_mode`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum PaddingMode {
    /// Fully transparent `(0, 0, 0, 0)` gutter — the historical default.
    #[default]
    Transparent,
    /// Replicates the nearest sprite edge texel outward on every side and
    /// corner, so the border continues the sprite's edge colours and alpha.
    ClampEdge,
    /// Wraps texels from the opposite sprite edge (wrapping both axes at
    /// the corners), so `fract`-wrapped shader sampling tiles seamlessly
    /// across the sprite boundary.
    WrapTile,
}

/// Queues RGBA8 sprites and packs them into a deterministic power-of-two
/// atlas. See the [module docs](self) for the packing and fail-closed
/// contract.
#[derive(Clone, Debug)]
pub struct AtlasBuilder {
    sprites: BTreeMap<String, SpriteData>,
    max_dim: u32,
    padding: u32,
    padding_mode: PaddingMode,
}

impl AtlasBuilder {
    /// A builder with [`DEFAULT_ATLAS_MAX_DIM`], no padding and
    /// [`PaddingMode::Transparent`] gutters.
    pub fn new() -> Self {
        Self {
            sprites: BTreeMap::new(),
            max_dim: DEFAULT_ATLAS_MAX_DIM,
            padding: 0,
            padding_mode: PaddingMode::Transparent,
        }
    }

    /// A builder preset for block atlases: [`DEFAULT_ATLAS_MAX_DIM`], no
    /// padding, and [`PaddingMode::WrapTile`] gutter fills so tiled
    /// (`fract`-wrapped) UV sampling continues seamlessly across sprite
    /// edges. Call [`set_padding`](Self::set_padding) to reserve the border
    /// the mode fills.
    pub fn for_blocks() -> Self {
        Self {
            padding_mode: PaddingMode::WrapTile,
            ..Self::new()
        }
    }

    /// Configured atlas edge cap (not yet rounded to a power of two).
    pub fn max_dim(&self) -> u32 {
        self.max_dim
    }

    /// Sets the atlas edge cap. Must be in `1..=MAX_ATLAS_DIM`; the
    /// effective packing limit is the largest power of two not exceeding it.
    pub fn set_max_dim(&mut self, dim: u32) -> Result<(), AtlasError> {
        if !(1..=MAX_ATLAS_DIM).contains(&dim) {
            return Err(AtlasError::InvalidMaxDimension(dim));
        }
        self.max_dim = dim;
        Ok(())
    }

    /// Configured per-sprite padding border in texels.
    pub fn padding(&self) -> u32 {
        self.padding
    }

    /// Sets the per-sprite padding border: `0` disables it, `1..=8` reserves
    /// that many texels on every side of each sprite.
    pub fn set_padding(&mut self, padding: u32) -> Result<(), AtlasError> {
        if padding > MAX_SPRITE_PADDING {
            return Err(AtlasError::InvalidPadding(padding));
        }
        self.padding = padding;
        Ok(())
    }

    /// The [`PaddingMode`] applied to sprites queued with
    /// [`add`](Self::add).
    pub fn padding_mode(&self) -> PaddingMode {
        self.padding_mode
    }

    /// Sets the default [`PaddingMode`] for subsequently queued sprites.
    /// [`add_sprite_with_mode`](Self::add_sprite_with_mode) overrides it
    /// per sprite. Has no visible effect while `padding` is 0.
    pub fn set_padding_mode(&mut self, mode: PaddingMode) {
        self.padding_mode = mode;
    }

    /// Number of queued sprites.
    pub fn len(&self) -> usize {
        self.sprites.len()
    }

    /// Whether no sprites are queued.
    pub fn is_empty(&self) -> bool {
        self.sprites.is_empty()
    }

    /// Queues one RGBA8 sprite with the builder's current
    /// [`PaddingMode`]. `rgba` must be exactly `width * height * 4`
    /// bytes in row-major order. Fails closed on an empty name, zero
    /// dimensions, a length mismatch, a duplicate name, or a sprite whose
    /// padded cell can never fit inside the atlas limit.
    pub fn add(
        &mut self,
        name: impl Into<String>,
        width: u32,
        height: u32,
        rgba: Vec<u8>,
    ) -> Result<(), AtlasError> {
        let mode = self.padding_mode;
        self.add_sprite_with_mode(name, width, height, rgba, mode)
    }

    /// [`add`](Self::add) with an explicit [`PaddingMode`] for this
    /// sprite's padding border, overriding the builder default.
    pub fn add_sprite_with_mode(
        &mut self,
        name: impl Into<String>,
        width: u32,
        height: u32,
        rgba: Vec<u8>,
        mode: PaddingMode,
    ) -> Result<(), AtlasError> {
        let name = name.into();
        if name.is_empty() {
            return Err(AtlasError::EmptySpriteName);
        }
        if self.sprites.contains_key(&name) {
            return Err(AtlasError::DuplicateSprite(name));
        }
        if width == 0 || height == 0 {
            return Err(AtlasError::ZeroSpriteDimension { name });
        }
        // Reject sprites that can never fit before touching their payload;
        // this also bounds width/height to <= MAX_ATLAS_DIM so the length
        // math below cannot overflow.
        let limit = self.atlas_limit();
        let pad = u64::from(self.padding);
        let cell_w = u64::from(width) + 2 * pad;
        let cell_h = u64::from(height) + 2 * pad;
        if cell_w > u64::from(limit) || cell_h > u64::from(limit) {
            return Err(AtlasError::SpriteExceedsAtlas {
                name,
                width,
                height,
                padding: self.padding,
                limit,
            });
        }
        let expected = u64::from(width) * u64::from(height) * 4;
        if rgba.len() as u64 != expected {
            return Err(AtlasError::RgbaLengthMismatch {
                name,
                expected,
                actual: rgba.len(),
            });
        }
        self.sprites.insert(
            name,
            SpriteData {
                width,
                height,
                rgba,
                mode,
            },
        );
        Ok(())
    }

    /// The effective power-of-two edge limit sprites pack into.
    fn atlas_limit(&self) -> u32 {
        pow2_floor(self.max_dim)
    }

    /// Packs every queued sprite and returns the finished atlas.
    ///
    /// Sprites are sorted by `(height desc, width desc, name asc)` and placed
    /// by a first-fit shelf packer, so identical input sets always produce a
    /// byte-identical atlas regardless of insertion order. Fails closed with
    /// [`AtlasError::EmptyAtlas`], [`AtlasError::SpriteExceedsAtlas`] or
    /// [`AtlasError::AtlasFull`].
    pub fn build(self) -> Result<PackedAtlas, AtlasError> {
        if self.sprites.is_empty() {
            return Err(AtlasError::EmptyAtlas);
        }
        let limit = self.atlas_limit();
        let padding = self.padding;

        let mut order: Vec<(&String, &SpriteData)> = self.sprites.iter().collect();
        order.sort_by(|a, b| {
            b.1.height
                .cmp(&a.1.height)
                .then_with(|| b.1.width.cmp(&a.1.width))
                .then_with(|| a.0.cmp(b.0))
        });

        struct Shelf {
            /// Top edge of the shelf row in cell coordinates.
            y: u32,
            /// Tallest cell height admitted into this shelf.
            height: u32,
            /// Right edge of the used cell space in this shelf.
            x_used: u32,
        }

        let mut shelves: Vec<Shelf> = Vec::new();
        let mut bottom = 0u32;
        // (name, sprite, atlas pixel x, atlas pixel y) in pack order.
        let mut placed: Vec<(&String, &SpriteData, u32, u32)> = Vec::with_capacity(order.len());

        for (name, sprite) in order {
            let cell_w = sprite.width + 2 * padding;
            let cell_h = sprite.height + 2 * padding;
            if cell_w > limit || cell_h > limit {
                return Err(AtlasError::SpriteExceedsAtlas {
                    name: name.clone(),
                    width: sprite.width,
                    height: sprite.height,
                    padding,
                    limit,
                });
            }
            // First-fit: earliest shelf whose height admits the cell and
            // whose remaining width fits it.
            let existing = shelves
                .iter_mut()
                .find(|shelf| cell_h <= shelf.height && shelf.x_used + cell_w <= limit);
            let (cell_x, cell_y) = match existing {
                Some(shelf) => {
                    let pos = (shelf.x_used, shelf.y);
                    shelf.x_used += cell_w;
                    pos
                }
                None => {
                    if bottom + cell_h > limit {
                        return Err(AtlasError::AtlasFull { limit });
                    }
                    let pos = (0, bottom);
                    shelves.push(Shelf {
                        y: bottom,
                        height: cell_h,
                        x_used: cell_w,
                    });
                    bottom += cell_h;
                    pos
                }
            };
            placed.push((name, sprite, cell_x + padding, cell_y + padding));
        }

        let used_w = shelves.iter().map(|shelf| shelf.x_used).max().unwrap_or(0);
        // `used_w`/`bottom` are <= limit (a power of two), so rounding up
        // stays inside the limit.
        let atlas_w = used_w.next_power_of_two();
        let atlas_h = bottom.next_power_of_two();

        let row_len = |width: u32| width as usize * 4;
        let mut rgba = vec![0u8; atlas_w as usize * atlas_h as usize * 4];
        let mut placements = BTreeMap::new();
        for (name, sprite, x, y) in placed {
            placements.insert(
                name.clone(),
                SpritePlacement {
                    x,
                    y,
                    width: sprite.width,
                    height: sprite.height,
                },
            );
            let src_row = row_len(sprite.width);
            for (row, src) in sprite.rgba.chunks_exact(src_row).enumerate() {
                let dst = ((y as usize + row) * atlas_w as usize + x as usize) * 4;
                rgba[dst..dst + src_row].copy_from_slice(src);
            }
            fill_padding_gutter(&mut rgba, atlas_w, sprite, x, y, padding);
        }

        Ok(PackedAtlas {
            width: atlas_w,
            height: atlas_h,
            padding,
            rgba,
            placements,
        })
    }
}

impl Default for AtlasBuilder {
    fn default() -> Self {
        Self::new()
    }
}

/// Fills the `padding`-texel gutter around `sprite` — placed at atlas pixel
/// `x, y` — according to `sprite.mode`, covering the full `1..=padding`
/// border on all four sides including the four corners.
///
/// The cell `[x - padding, x + width + padding)` ×
/// `[y - padding, y + height + padding)` is exclusively owned by this
/// sprite: the shelf packer never overlaps cells, so the fill cannot touch
/// a neighbour's texels. No-op for [`PaddingMode::Transparent`] (the
/// buffer is already zeroed) and for `padding == 0`.
fn fill_padding_gutter(
    rgba: &mut [u8],
    atlas_w: u32,
    sprite: &SpriteData,
    x: u32,
    y: u32,
    padding: u32,
) {
    if padding == 0 || sprite.mode == PaddingMode::Transparent {
        return;
    }
    let cell_x = x - padding;
    let cell_y = y - padding;
    let cell_right = x + sprite.width + padding;
    let cell_bottom = y + sprite.height + padding;
    for cy in cell_y..cell_bottom {
        for cx in cell_x..cell_right {
            // Skip the sprite's own texels.
            if cx >= x && cx < x + sprite.width && cy >= y && cy < y + sprite.height {
                continue;
            }
            // Offset from the sprite's top-left, in
            // `-padding..width + padding` on each axis.
            let sx = i64::from(cx) - i64::from(x);
            let sy = i64::from(cy) - i64::from(y);
            let (tx, ty) = match sprite.mode {
                PaddingMode::ClampEdge => (
                    sx.clamp(0, i64::from(sprite.width) - 1) as u32,
                    sy.clamp(0, i64::from(sprite.height) - 1) as u32,
                ),
                PaddingMode::WrapTile => (
                    sx.rem_euclid(i64::from(sprite.width)) as u32,
                    sy.rem_euclid(i64::from(sprite.height)) as u32,
                ),
                PaddingMode::Transparent => continue,
            };
            let src = (ty as usize * sprite.width as usize + tx as usize) * 4;
            let dst = (cy as usize * atlas_w as usize + cx as usize) * 4;
            rgba[dst..dst + 4].copy_from_slice(&sprite.rgba[src..src + 4]);
        }
    }
}

/// Where one sprite's pixels landed inside a [`PackedAtlas`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SpritePlacement {
    /// Left edge in atlas texels.
    pub x: u32,
    /// Top edge in atlas texels.
    pub y: u32,
    /// Sprite width in texels.
    pub width: u32,
    /// Sprite height in texels.
    pub height: u32,
}

/// Tile-safe sampling sub-rect of one packed sprite, for shaders that wrap
/// sprite UVs with `fract` (`uv - floor(uv)`) and sample with
/// `textureSampleGrad`.
///
/// The sprite's `[0, 1)` UV domain maps onto
/// `[u0, u0 + u_scale)` × `[v0, v0 + v_scale)` in normalized atlas space, so
/// every wrapped sample — including UVs far outside `0..1` — lands on this
/// sprite's texels and nowhere else. `texel_u`/`texel_v` are one atlas
/// texel in normalized UV units (`1 / width`, `1 / height`), for scaling
/// explicit sample gradients. See [`PackedAtlas::tile_rect`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AtlasTileRect {
    /// Left edge of the sprite rect in normalized atlas U.
    pub u0: f32,
    /// Top edge of the sprite rect in normalized atlas V.
    pub v0: f32,
    /// Sprite rect width in normalized atlas U (right edge minus `u0`).
    pub u_scale: f32,
    /// Sprite rect height in normalized atlas V (bottom edge minus `v0`).
    pub v_scale: f32,
    /// One atlas texel in normalized U (`1 / atlas.width`).
    pub texel_u: f32,
    /// One atlas texel in normalized V (`1 / atlas.height`).
    pub texel_v: f32,
}

/// A finished atlas: power-of-two RGBA8 pixels plus every sprite's pixel
/// rect. `padding` records the border the builder reserved around each
/// sprite (0 when disabled).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PackedAtlas {
    /// Atlas width in texels; always a power of two, `<= MAX_ATLAS_DIM`.
    pub width: u32,
    /// Atlas height in texels; always a power of two, `<= MAX_ATLAS_DIM`.
    pub height: u32,
    /// Per-sprite padding border the atlas was built with.
    pub padding: u32,
    /// Row-major RGBA8 texels, `width * height * 4` bytes. Uncovered texels
    /// are `(0, 0, 0, 0)`.
    pub rgba: Vec<u8>,
    /// Sprite name -> pixel rect inside the atlas.
    pub placements: BTreeMap<String, SpritePlacement>,
}

impl PackedAtlas {
    /// The pixel rect `name` was packed into, if present.
    pub fn placement(&self, name: &str) -> Option<&SpritePlacement> {
        self.placements.get(name)
    }

    /// Normalized `[u0, v0, u1, v1]` atlas UV rect covering `name`'s sprite,
    /// or `None` when the sprite is not packed.
    pub fn uv_rect(&self, name: &str) -> Option<[f32; 4]> {
        self.placement(name)
            .map(|placement| self.placement_uv_rect(placement))
    }

    /// Tile-safe sampling rect for `name`, or `None` when the sprite is not
    /// packed. Resolves `name` like the baked-quad remappers: exact sprite
    /// name, then the path part of a `namespace:path` id, then a
    /// `minecraft:`-prefixed guess for namespace-less ids.
    ///
    /// The shader contract is `fract`-wrapped UVs: with
    /// `f = uv - floor(uv)` (GLSL `fract`), the atlas coordinate
    /// `u0 + f_u * u_scale`, `v0 + f_v * v_scale` always lands inside
    /// `[u0, u0 + u_scale)` × `[v0, v0 + v_scale)` — the sprite's own
    /// texels — for any input UV, so tiled block faces never sample a
    /// neighbour. `texel_u`/`texel_v` carry the atlas texel size for
    /// `textureSampleGrad` derivative scaling.
    pub fn tile_rect(&self, name: &str) -> Option<AtlasTileRect> {
        let placement = find_placement(self, name)?;
        let rect = self.placement_uv_rect(placement);
        Some(AtlasTileRect {
            u0: rect[0],
            v0: rect[1],
            u_scale: rect[2] - rect[0],
            v_scale: rect[3] - rect[1],
            texel_u: 1.0 / self.width as f32,
            texel_v: 1.0 / self.height as f32,
        })
    }

    /// Maximum mip chain depth: the number of times the smaller atlas
    /// dimension can be halved before reaching 1 texel.
    pub fn max_mip_levels(&self) -> u32 {
        self.width
            .trailing_zeros()
            .min(self.height.trailing_zeros())
    }

    /// Normalized `[u0, v0, u1, v1]` rect of one placement inside the atlas.
    fn placement_uv_rect(&self, placement: &SpritePlacement) -> [f32; 4] {
        [
            placement.x as f32 / self.width as f32,
            placement.y as f32 / self.height as f32,
            (placement.x + placement.width) as f32 / self.width as f32,
            (placement.y + placement.height) as f32 / self.height as f32,
        ]
    }
}

// ---------------------------------------------------------------------------
// Alpha-weighted mipmaps
// ---------------------------------------------------------------------------

/// One downsampled mip level of a [`PackedAtlas`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AtlasMip {
    /// Mip level index: `1` is half resolution, `levels` is the last.
    pub level: u32,
    /// Level width in texels.
    pub width: u32,
    /// Level height in texels.
    pub height: u32,
    /// Row-major RGBA8 texels, `width * height * 4` bytes.
    pub rgba: Vec<u8>,
}

/// `floor((num + den/2) / den)` clamped to `u8`: round-to-nearest for the
/// non-negative accumulators the mip kernel produces.
fn div_round(num: u64, den: u64) -> u8 {
    debug_assert!(den > 0);
    ((num + den / 2) / den).min(255) as u8
}

/// One 2x2 -> 1x1 alpha-weighted downsample step. Each source texel
/// contributes its colour premultiplied by its alpha; the output colour is
/// the alpha-weighted mean and the output alpha is the footprint mean, so
/// `a == 0` texels carry no colour at all.
fn mip_half(src_w: u32, src_h: u32, src: &[u8]) -> (u32, u32, Vec<u8>) {
    let dst_w = src_w / 2;
    let dst_h = src_h / 2;
    let mut dst = vec![0u8; dst_w as usize * dst_h as usize * 4];
    for dy in 0..dst_h {
        for dx in 0..dst_w {
            let mut alpha_sum = 0u64;
            let mut color_sum = [0u64; 3];
            for sy in 0..2u32 {
                for sx in 0..2u32 {
                    let px = dx * 2 + sx;
                    let py = dy * 2 + sy;
                    if px >= src_w || py >= src_h {
                        continue;
                    }
                    let i = ((py * src_w + px) * 4) as usize;
                    let a = u64::from(src[i + 3]);
                    alpha_sum += a;
                    for (channel, sum) in color_sum.iter_mut().enumerate() {
                        *sum += u64::from(src[i + channel]) * a;
                    }
                }
            }
            let out = ((dy * dst_w + dx) * 4) as usize;
            if alpha_sum == 0 {
                // dst is already zeroed: (0, 0, 0, 0).
                continue;
            }
            for (channel, sum) in color_sum.iter().enumerate() {
                dst[out + channel] = div_round(*sum, alpha_sum);
            }
            dst[out + 3] = div_round(alpha_sum, 4);
        }
    }
    (dst_w, dst_h, dst)
}

/// Validates an atlas's pixel buffer and the requested mip depth, shared
/// by [`build_mipmaps`] and [`build_mipmaps_linear`].
fn validate_mip_request(atlas: &PackedAtlas, levels: u32) -> Result<(), AtlasError> {
    // u128 math: `PackedAtlas` fields are public, so a hand-built atlas may
    // carry dimensions whose byte count overflows u64; the comparison must
    // never wrap or panic.
    let expected = u128::from(atlas.width) * u128::from(atlas.height) * 4;
    if atlas.width == 0 || atlas.height == 0 || atlas.rgba.len() as u128 != expected {
        return Err(AtlasError::MalformedAtlas {
            expected: u64::try_from(expected).unwrap_or(u64::MAX),
            actual: atlas.rgba.len(),
        });
    }
    let max = atlas.max_mip_levels();
    if levels > max {
        return Err(AtlasError::TooManyMipLevels {
            requested: levels,
            max,
        });
    }
    Ok(())
}

/// Generates `levels` downsampled mip levels below the atlas base image
/// (level 1 is half resolution; the base image itself is not returned).
///
/// Every step is alpha-weighted: transparent texels do not bleed black into
/// cutout sprites. Fails closed with [`AtlasError::MalformedAtlas`] on a
/// hand-damaged [`PackedAtlas`] or [`AtlasError::TooManyMipLevels`] when the
/// chain would shrink a dimension below 1 texel.
pub fn build_mipmaps(atlas: &PackedAtlas, levels: u32) -> Result<Vec<AtlasMip>, AtlasError> {
    validate_mip_request(atlas, levels)?;
    let mut mips: Vec<AtlasMip> = Vec::with_capacity(levels as usize);
    for level in 1..=levels {
        // Level 1 downsamples the base atlas; level k downsamples level k-1.
        let (src_w, src_h, src) = if level == 1 {
            (atlas.width, atlas.height, atlas.rgba.as_slice())
        } else {
            let prev = &mips[(level - 2) as usize];
            (prev.width, prev.height, prev.rgba.as_slice())
        };
        let (width, height, rgba) = mip_half(src_w, src_h, src);
        mips.push(AtlasMip {
            level,
            width,
            height,
            rgba,
        });
    }
    Ok(mips)
}

// ---------------------------------------------------------------------------
// Gamma-correct (linear sRGB) mipmaps
// ---------------------------------------------------------------------------

/// Deterministic 256-entry sRGB → linear-light decode table (IEC 61966-2-1
/// EOTF). Index `i` decodes sRGB byte `i` to a linear `f32` in `0.0..=1.0`.
fn srgb_to_linear_lut() -> [f32; 256] {
    let mut lut = [0f32; 256];
    for (i, entry) in lut.iter_mut().enumerate() {
        let c = i as f32 / 255.0;
        *entry = if c <= 0.04045 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        };
    }
    lut
}

/// Encodes a linear-light channel (`0.0..=1.0`, clamped) back to an sRGB
/// byte with round-to-nearest — the inverse of [`srgb_to_linear_lut`].
fn linear_to_srgb_u8(linear: f64) -> u8 {
    let l = linear.clamp(0.0, 1.0);
    let encoded = if l <= 0.003_130_8 {
        l * 12.92
    } else {
        1.055 * l.powf(1.0 / 2.4) - 0.055
    };
    (encoded * 255.0).round().clamp(0.0, 255.0) as u8
}

/// [`mip_half`] in linear sRGB space: each source texel's RGB is decoded
/// through `srgb_to_linear`, alpha-weighted averaged, and encoded back.
/// Alpha handling is identical to [`mip_half`].
fn mip_half_linear(
    src_w: u32,
    src_h: u32,
    src: &[u8],
    srgb_to_linear: &[f32; 256],
) -> (u32, u32, Vec<u8>) {
    let dst_w = src_w / 2;
    let dst_h = src_h / 2;
    let mut dst = vec![0u8; dst_w as usize * dst_h as usize * 4];
    for dy in 0..dst_h {
        for dx in 0..dst_w {
            let mut alpha_sum = 0u64;
            let mut color_sum = [0f64; 3];
            for sy in 0..2u32 {
                for sx in 0..2u32 {
                    let px = dx * 2 + sx;
                    let py = dy * 2 + sy;
                    if px >= src_w || py >= src_h {
                        continue;
                    }
                    let i = ((py * src_w + px) * 4) as usize;
                    let a = u64::from(src[i + 3]);
                    alpha_sum += a;
                    for (channel, sum) in color_sum.iter_mut().enumerate() {
                        *sum += f64::from(srgb_to_linear[usize::from(src[i + channel])])
                            * a as f64;
                    }
                }
            }
            let out = ((dy * dst_w + dx) * 4) as usize;
            if alpha_sum == 0 {
                // dst is already zeroed: (0, 0, 0, 0).
                continue;
            }
            let alpha_f = alpha_sum as f64;
            for (channel, sum) in color_sum.iter().enumerate() {
                dst[out + channel] = linear_to_srgb_u8(*sum / alpha_f);
            }
            dst[out + 3] = div_round(alpha_sum, 4);
        }
    }
    (dst_w, dst_h, dst)
}

/// Fraction of texels whose alpha — optionally pre-scaled by `scale`
/// (round-to-nearest, clamped to `u8`) — is `>= cutoff`.
fn alpha_cutoff_fraction(rgba: &[u8], cutoff: u8, scale: f64) -> f64 {
    let mut passing = 0u64;
    let mut total = 0u64;
    for px in rgba.as_chunks::<4>().0 {
        total += 1;
        let boosted = (f64::from(px[3]) * scale).round().min(255.0);
        if boosted >= f64::from(cutoff) {
            passing += 1;
        }
    }
    if total == 0 {
        return 0.0;
    }
    passing as f64 / total as f64
}

/// Upper bound of the cutout-coverage alpha scale search: the boost never
/// drops below 1.0 (no dimming) nor exceeds 4.0.
const CUTOUT_ALPHA_SCALE_MAX: f64 = 4.0;

/// Fixed iteration count for the deterministic coverage binary search.
const CUTOUT_SEARCH_STEPS: usize = 32;

/// Rescales `rgba`'s alpha so the fraction of texels with `a >= cutoff`
/// keeps tracking `base_fraction` (the fraction measured on the base
/// image). Returns immediately when the level already passes at least
/// `base_fraction`; otherwise binary-searches the smallest scale in
/// `1.0..=4.0` that reaches it — best effort, so 4.0 when even that falls
/// short — and applies it round-to-nearest.
fn preserve_cutout_coverage(rgba: &mut [u8], base_fraction: f64, cutoff: u8) {
    if alpha_cutoff_fraction(rgba, cutoff, 1.0) >= base_fraction {
        return;
    }
    let mut lo = 1.0f64;
    let mut hi = CUTOUT_ALPHA_SCALE_MAX;
    for _ in 0..CUTOUT_SEARCH_STEPS {
        let mid = (lo + hi) * 0.5;
        if alpha_cutoff_fraction(rgba, cutoff, mid) >= base_fraction {
            hi = mid;
        } else {
            lo = mid;
        }
    }
    for px in rgba.as_chunks_mut::<4>().0 {
        px[3] = (f64::from(px[3]) * hi).round().min(255.0) as u8;
    }
}

/// [`build_mipmaps`] in gamma-correct linear sRGB space.
///
/// RGB channels are decoded through a deterministic 256-entry sRGB→linear
/// LUT, alpha-weighted averaged, and encoded back to sRGB, so averaging
/// sRGB 0 and 255 yields ~188 instead of the naive ~128 and mid-tone mip
/// colours stay perceptually correct. Alpha handling is identical to
/// [`build_mipmaps`].
///
/// When `cutout_alpha_cutoff` is `Some(cutoff)`, each level's alpha is
/// additionally rescaled so the fraction of texels with `a >= cutoff`
/// keeps tracking the base image — distant cutout foliage and crops retain
/// their silhouette coverage instead of alpha-diluting below the cutoff
/// and vanishing.
///
/// Shares [`build_mipmaps`]'s fail-closed contract:
/// [`AtlasError::MalformedAtlas`] on a hand-damaged [`PackedAtlas`] and
/// [`AtlasError::TooManyMipLevels`] when the chain would shrink a dimension
/// below 1 texel.
pub fn build_mipmaps_linear(
    atlas: &PackedAtlas,
    levels: u32,
    cutout_alpha_cutoff: Option<u8>,
) -> Result<Vec<AtlasMip>, AtlasError> {
    validate_mip_request(atlas, levels)?;
    let srgb_to_linear = srgb_to_linear_lut();
    let base_fraction = cutout_alpha_cutoff.map_or(0.0, |cutoff| {
        alpha_cutoff_fraction(&atlas.rgba, cutoff, 1.0)
    });
    let mut mips: Vec<AtlasMip> = Vec::with_capacity(levels as usize);
    for level in 1..=levels {
        // Level 1 downsamples the base atlas; level k downsamples level k-1.
        let (src_w, src_h, src) = if level == 1 {
            (atlas.width, atlas.height, atlas.rgba.as_slice())
        } else {
            let prev = &mips[(level - 2) as usize];
            (prev.width, prev.height, prev.rgba.as_slice())
        };
        let (width, height, mut rgba) = mip_half_linear(src_w, src_h, src, &srgb_to_linear);
        if let Some(cutoff) = cutout_alpha_cutoff {
            preserve_cutout_coverage(&mut rgba, base_fraction, cutoff);
        }
        mips.push(AtlasMip {
            level,
            width,
            height,
            rgba,
        });
    }
    Ok(mips)
}

// ---------------------------------------------------------------------------
// `.mcmeta` animation tables
// ---------------------------------------------------------------------------

/// One entry of a [`SpriteAnimation`] playback table: which strip slot to
/// sample and for how long. This is the resolved form of a `.mcmeta`
/// `frames` entry — an integer frame index plus its `time` in ticks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AnimFrame {
    /// Strip slot to sample: which `frame_size`-square frame of the vertical
    /// strip this step uses (0-based, must be `< strip_height / frame_size`).
    pub index: u32,
    /// How many ticks this step stays active. Must be >= 1.
    pub ticks: u32,
}

/// A resolved `.mcmeta` animation: a vertical strip sprite plus a playback
/// table of [`AnimFrame`]s. See [`SpriteAnimation::active_frame_at_tick`]
/// for the per-tick query contract.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SpriteAnimation {
    /// Square frame edge in texels; the strip is `frame_size` texels wide.
    frame_size: u32,
    /// Total strip height in texels; always `frame_size * strip_frames`.
    strip_height: u32,
    /// Number of square frames the strip holds.
    strip_frames: u32,
    /// Playback table: which strip slot each step samples and its duration.
    frames: Vec<AnimFrame>,
    /// Whether consecutive frames blend smoothly (`interpolate: true`).
    interpolate: bool,
    /// Sum of all frame durations; the animation loops every this many
    /// ticks.
    total_ticks: u64,
}

/// Validates the strip geometry shared by both constructors and returns the
/// slot count.
fn strip_slots(frame_size: u32, strip_height: u32) -> Result<u32, AnimationError> {
    if frame_size == 0 {
        return Err(AnimationError::ZeroFrameSize);
    }
    if strip_height == 0 || !strip_height.is_multiple_of(frame_size) {
        return Err(AnimationError::InvalidStrip {
            frame_size,
            strip_height,
        });
    }
    Ok(strip_height / frame_size)
}

impl SpriteAnimation {
    /// The default `.mcmeta` animation: every strip slot in order, each
    /// shown for `frametime` ticks. `strip_height` must be a positive
    /// multiple of `frame_size` (the vanilla vertical-strip layout
    /// `height = frame_count * frame_width`).
    pub fn strip(
        frame_size: u32,
        strip_height: u32,
        frametime: u32,
        interpolate: bool,
    ) -> Result<Self, AnimationError> {
        let strip_frames = strip_slots(frame_size, strip_height)?;
        // Build the implicit table only after the slot count is known to be
        // inside the frame cap, so a hostile strip height cannot allocate
        // first and fail later.
        if strip_frames as usize > MAX_ANIMATION_FRAMES {
            return Err(AnimationError::TooManyFrames(strip_frames as usize));
        }
        if frametime == 0 {
            return Err(AnimationError::ZeroFrameDuration { position: 0 });
        }
        let frames = (0..strip_frames)
            .map(|index| AnimFrame {
                index,
                ticks: frametime,
            })
            .collect();
        Self::from_parts(frame_size, strip_height, strip_frames, frames, interpolate)
    }

    /// A custom `.mcmeta` `frames` table: explicit strip slots with
    /// per-frame tick durations (vanilla `{"index": n, "time": t}` entries;
    /// plain integer entries become `time = frametime`, which callers
    /// resolve before calling this).
    pub fn with_frames(
        frame_size: u32,
        strip_height: u32,
        frames: Vec<AnimFrame>,
        interpolate: bool,
    ) -> Result<Self, AnimationError> {
        let strip_frames = strip_slots(frame_size, strip_height)?;
        Self::from_parts(frame_size, strip_height, strip_frames, frames, interpolate)
    }

    fn from_parts(
        frame_size: u32,
        strip_height: u32,
        strip_frames: u32,
        frames: Vec<AnimFrame>,
        interpolate: bool,
    ) -> Result<Self, AnimationError> {
        // Both constructors share this one fail-closed validation path.
        Self::validate_table(&frames, strip_frames)?;
        let total_ticks = frames.iter().map(|frame| u64::from(frame.ticks)).sum();
        Ok(Self {
            frame_size,
            strip_height,
            strip_frames,
            frames,
            interpolate,
            total_ticks,
        })
    }

    fn validate_table(frames: &[AnimFrame], strip_frames: u32) -> Result<(), AnimationError> {
        if frames.is_empty() {
            return Err(AnimationError::EmptyFrameTable);
        }
        if frames.len() > MAX_ANIMATION_FRAMES {
            return Err(AnimationError::TooManyFrames(frames.len()));
        }
        for (position, frame) in frames.iter().enumerate() {
            if frame.ticks == 0 {
                return Err(AnimationError::ZeroFrameDuration { position });
            }
            if frame.index >= strip_frames {
                return Err(AnimationError::FrameIndexOutOfRange {
                    position,
                    index: frame.index,
                    strip_frames,
                });
            }
        }
        Ok(())
    }

    /// Square frame edge in texels.
    pub fn frame_size(&self) -> u32 {
        self.frame_size
    }

    /// Total strip height in texels.
    pub fn strip_height(&self) -> u32 {
        self.strip_height
    }

    /// Number of square frames the strip holds.
    pub fn strip_frames(&self) -> u32 {
        self.strip_frames
    }

    /// The playback table: strip slot + duration per step.
    pub fn frames(&self) -> &[AnimFrame] {
        &self.frames
    }

    /// Whether consecutive frames blend (`interpolate: true`).
    pub fn interpolate(&self) -> bool {
        self.interpolate
    }

    /// Playback steps in the loop.
    pub fn frame_count(&self) -> usize {
        self.frames.len()
    }

    /// Loop length in ticks.
    pub fn total_ticks(&self) -> u64 {
        self.total_ticks
    }

    /// Normalized `[u0, v0, u1, v1]` UV rect of strip `slot` inside the
    /// sprite image: the sprite is `frame_size` wide and `strip_height`
    /// tall, frames stack top to bottom. `None` for an out-of-range slot.
    pub fn slot_rect(&self, slot: u32) -> Option<[f32; 4]> {
        if slot >= self.strip_frames {
            return None;
        }
        let size = self.frame_size as f32;
        let strip_h = self.strip_height as f32;
        Some([
            0.0,
            slot as f32 * size / strip_h,
            1.0,
            (slot + 1) as f32 * size / strip_h,
        ])
    }

    /// Resolves `tick` (wrapping every [`total_ticks`](Self::total_ticks))
    /// to `(current, next, blend_alpha)`:
    ///
    /// * `current`/`next` are indices into [`frames`](Self::frames); the
    ///   strip slot each samples is `frames[i].index`.
    /// * `next` is the cyclic successor, so interpolated loops blend the
    ///   last frame back into the first.
    /// * `blend_alpha` is the fraction of the current frame's duration
    ///   already elapsed, in `[0, 1)`. It is always `0.0` when the
    ///   animation was built without `interpolate`.
    ///
    /// The function is a pure function of `tick`: identical ticks always
    /// produce identical results.
    pub fn active_frame_at_tick(&self, tick: u64) -> (usize, usize, f32) {
        let t = tick % self.total_ticks;
        let mut elapsed_base = 0u64;
        for (i, frame) in self.frames.iter().enumerate() {
            let duration = u64::from(frame.ticks);
            if t < elapsed_base + duration {
                let next = (i + 1) % self.frames.len();
                let alpha = if self.interpolate {
                    ((t - elapsed_base) as f64 / duration as f64) as f32
                } else {
                    0.0
                };
                return (i, next, alpha);
            }
            elapsed_base += duration;
        }
        // Unreachable: `t < total_ticks` by construction and the durations
        // sum to `total_ticks`. Return the last frame rather than panicking.
        let last = self.frames.len() - 1;
        (last, 0, 0.0)
    }

    /// The `atlas_rects` `SpriteFrame` pair that samples this animation at
    /// `tick`, derived from `tile` (the packed sprite's whole-strip
    /// [`AtlasTileRect`]):
    ///
    /// * `rect` — `(u0, v0, u_scale, v_scale)` of the active strip slot in
    ///   normalized atlas space. `v_scale` is `tile.v_scale /
    ///   strip_frames`, so `rect.xy + fract(uv) * rect.zw` samples exactly
    ///   one `frame_size`-tall slot while tiled faces still wrap inside it.
    /// * `aux` — `(v0 of the next slot, blend_alpha, 0, 0)` for shaders
    ///   implementing `interpolate`: `mix(cur, next, aux.y)`. `blend_alpha`
    ///   is always `0.0` for non-interpolated animations, which callers may
    ///   use to skip the second sample.
    pub fn rect_pair_at_tick(&self, tile: &AtlasTileRect, tick: u64) -> ([f32; 4], [f32; 4]) {
        let (cur, next, blend) = self.active_frame_at_tick(tick);
        // Slots stack top-to-bottom, so slot `i`'s window starts
        // `i` frame-heights below the strip's top edge.
        let frame_v = tile.v_scale / self.strip_frames as f32;
        let slot_top = |i: usize| tile.v0 + self.frames[i].index as f32 * frame_v;
        (
            [tile.u0, slot_top(cur), tile.u_scale, frame_v],
            [slot_top(next), blend, 0.0, 0.0],
        )
    }
}

// ---------------------------------------------------------------------------
// Baked-quad UV remapping + biome tint
// ---------------------------------------------------------------------------

/// Finds the atlas placement for a baked quad's texture id. Exact sprite
/// name first, then the path part of a `namespace:path` id (so a sprite
/// registered as `block/stone` still resolves a quad textured
/// `minecraft:block/stone`), then a `minecraft:`-prefixed guess for
/// namespace-less ids.
fn find_placement<'a>(atlas: &'a PackedAtlas, texture: &str) -> Option<&'a SpritePlacement> {
    if let Some(placement) = atlas.placement(texture) {
        return Some(placement);
    }
    if let Some((_, path)) = texture.split_once(':') {
        if let Some(placement) = atlas.placement(path) {
            return Some(placement);
        }
    }
    if !texture.contains(':') {
        return atlas.placement(&format!("minecraft:{texture}"));
    }
    None
}

fn remap_quad_uvs(
    atlas: &PackedAtlas,
    quad: &BakedModelQuad,
    half_texel_inset: bool,
) -> Result<[[f32; 2]; 4], AtlasError> {
    let placement = find_placement(atlas, &quad.texture)
        .ok_or_else(|| AtlasError::TextureNotInAtlas(quad.texture.clone()))?;
    let mut rect = atlas.placement_uv_rect(placement);
    if half_texel_inset {
        // Pull each edge in by half a texel so bilinear samples land on
        // texel centres instead of straddling the sprite border.
        let w = atlas.width as f32;
        let h = atlas.height as f32;
        rect[0] = (placement.x as f32 + 0.5) / w;
        rect[1] = (placement.y as f32 + 0.5) / h;
        rect[2] = ((placement.x + placement.width) as f32 - 0.5) / w;
        rect[3] = ((placement.y + placement.height) as f32 - 0.5) / h;
    }
    let mut out = [[0.0f32; 2]; 4];
    for (i, uv) in quad.uv.iter().enumerate() {
        // Baked quads carry vanilla 0..16 texture units; normalise to
        // 0..1 before mapping into the sprite sub-rect. Values outside
        // 0..16 (Vanilla wrap UVs) pass through linearly, matching the
        // bake contract.
        let u = uv[0] / MODEL_UNITS_PER_BLOCK;
        let v = uv[1] / MODEL_UNITS_PER_BLOCK;
        out[i] = [
            rect[0] + u * (rect[2] - rect[0]),
            rect[1] + v * (rect[3] - rect[1]),
        ];
    }
    Ok(out)
}

/// Maps a [`BakedModelQuad`]'s UVs into the exact sub-rect its texture
/// occupies inside `atlas`. Quad UVs are vanilla `0..16` texture units
/// (see [`BakedModelQuad::uv`]); each vertex maps linearly onto the sprite
/// rect, so a full-sprite quad lands exactly on the rect corners.
///
/// Fails closed with [`AtlasError::TextureNotInAtlas`] when the quad's
/// texture id (or its namespace-stripped path) names no packed sprite.
/// For seam-safe sampling use [`remap_baked_quad_uvs_inset`].
pub fn remap_baked_quad_uvs(
    atlas: &PackedAtlas,
    quad: &BakedModelQuad,
) -> Result<[[f32; 2]; 4], AtlasError> {
    remap_quad_uvs(atlas, quad, false)
}

/// [`remap_baked_quad_uvs`] with a half-texel inset: the sprite rect is
/// shrunk by half a texel on every side first, so bilinear filtering at the
/// quad's UV extremes samples texel centres and cannot bleed neighbouring
/// atlas sprites through the seam.
pub fn remap_baked_quad_uvs_inset(
    atlas: &PackedAtlas,
    quad: &BakedModelQuad,
) -> Result<[[f32; 2]; 4], AtlasError> {
    remap_quad_uvs(atlas, quad, true)
}

/// Multiplies `rgba`'s RGB channels by `tint_rgb` (a biome colour such as
/// foliage or grass green) with round-to-nearest scaling, preserving alpha
/// exactly. `[255, 255, 255]` is the identity tint.
#[must_use]
pub fn apply_biome_tint(rgba: [u8; 4], tint_rgb: [u8; 3]) -> [u8; 4] {
    let mut out = rgba;
    for (channel, tint) in out.iter_mut().zip(tint_rgb.iter()) {
        *channel = ((u16::from(*channel) * u16::from(*tint) + 127) / 255) as u8;
    }
    out
}
