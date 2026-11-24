// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
//! Deterministic 2D `UiDrawList`, built-in 8×8 bitmap font, GPU quad
//! tessellation (with a companion WGSL shader) and a pure-`std` software
//! RGBA rasterizer.
//!
//! The layer split keeps every backend honest:
//!
//! * **Recording** — widgets and screen builders only ever emit
//!   [`UiCommand`]s into a [`UiDrawList`]. No GL/Vulkan/WGPU handles exist
//!   at this layer, so a frame is plain data: diffable in tests, replayable
//!   after a crash, identical on every platform.
//! * **GPU submission** — [`tessellate_draw_list`] bakes a draw list into
//!   [`UiVertex`]/`u32` index soup and [`font_atlas_rgba`] produces the one
//!   texture it needs. [`UI_QUAD_WGSL`] is the matching vertex+fragment
//!   shader: pixel-space positions → NDC, atlas-sampled alpha, vertex
//!   color modulation. It compiles under any WGSL front end (wgpu/naga or a
//!   hand-rolled Vulkan pipeline); the crate deliberately carries no GPU
//!   dependency so embedders pick their own.
//! * **Software raster** — [`rasterize_draw_list`] paints the same commands
//!   into an `RGBA8` buffer with src-over blending, and
//!   [`encode_png_rgba`] wraps any RGBA frame in a valid PNG (zlib stored
//!   blocks, RFC 2083/1950) so tests and crash dumpers can persist pixels
//!   without a window system.
//!
//! The font is an original clean-room pixel face authored for this crate:
//! each glyph is written as rows of `#`/`.` art (visible in [`FONT_8X8`]
//! below and in [`crate::font_cjk::EXTRA_GLYPHS`]) and compiled to bytes by
//! a `const fn`. It covers printable ASCII `0x20..=0x7E` on the 8×8 cell
//! plus the full-width CJK/kana/punctuation/Hangul and half-width extended
//! Latin/Cyrillic sets in [`crate::font_cjk`] — every code point the
//! bilingual UI can emit — on a 16×8 "double cell" canvas that paints
//! squeezed to [`FULLWIDTH_PX`] px (near-square instead of a 2:1
//! pancake). Anything still unmapped draws the
//! [`FALLBACK_GLYPH`] box (or its wide twin) so labels never silently
//! disappear, and an external [`crate::font_cjk::GlyphAtlas`] font pack can
//! fill the remaining gaps at runtime.

/// Axis-aligned rectangle in physical pixels.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct UiRect {
    pub x: i32,
    pub y: i32,
    pub w: u32,
    pub h: u32,
}

impl UiRect {
    pub const fn new(x: i32, y: i32, w: u32, h: u32) -> Self {
        Self { x, y, w, h }
    }

    /// Right edge (exclusive), in pixels.
    pub const fn right(self) -> i32 {
        self.x + self.w as i32
    }

    /// Bottom edge (exclusive), in pixels.
    pub const fn bottom(self) -> i32 {
        self.y + self.h as i32
    }

    /// Point containment test (right/bottom exclusive).
    pub const fn contains(self, px: i32, py: i32) -> bool {
        px >= self.x && px < self.right() && py >= self.y && py < self.bottom()
    }

    /// `true` when the two rectangles share at least one pixel.
    pub const fn intersects(self, other: UiRect) -> bool {
        self.x < other.right()
            && other.x < self.right()
            && self.y < other.bottom()
            && other.y < self.bottom()
    }
}

/// Straight (non-premultiplied) 8-bit RGBA color.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct UiColor {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

impl UiColor {
    pub const fn rgba(r: u8, g: u8, b: u8, a: u8) -> Self {
        Self { r, g, b, a }
    }

    pub const fn rgb(r: u8, g: u8, b: u8) -> Self {
        Self::rgba(r, g, b, 255)
    }

    /// Same RGB channels with a different alpha.
    pub const fn with_alpha(self, a: u8) -> Self {
        Self::rgba(self.r, self.g, self.b, a)
    }

    /// Linear-light `f32` copy for vertex attributes.
    pub fn as_f32(self) -> [f32; 4] {
        [
            f32::from(self.r) / 255.0,
            f32::from(self.g) / 255.0,
            f32::from(self.b) / 255.0,
            f32::from(self.a) / 255.0,
        ]
    }
}

/// Common UI palette, chosen once so every screen agrees.
pub mod palette {
    use super::UiColor;

    pub const BACKGROUND_TOP: UiColor = UiColor::rgb(24, 26, 32);
    pub const BACKGROUND_BOTTOM: UiColor = UiColor::rgb(13, 14, 18);
    pub const PANEL: UiColor = UiColor::rgba(30, 33, 42, 235);
    pub const PANEL_BORDER: UiColor = UiColor::rgb(80, 88, 110);
    pub const BUTTON: UiColor = UiColor::rgb(58, 62, 76);
    pub const BUTTON_HOVER: UiColor = UiColor::rgb(84, 96, 130);
    pub const BUTTON_DISABLED: UiColor = UiColor::rgb(38, 40, 48);
    pub const BUTTON_BORDER: UiColor = UiColor::rgb(14, 15, 20);
    pub const BUTTON_BORDER_HOVER: UiColor = UiColor::rgb(215, 225, 255);
    pub const TEXT: UiColor = UiColor::rgb(238, 238, 238);
    pub const TEXT_DIM: UiColor = UiColor::rgb(160, 165, 178);
    pub const TEXT_DARK: UiColor = UiColor::rgb(40, 42, 50);
    pub const ACCENT: UiColor = UiColor::rgb(84, 170, 90);
    pub const DANGER: UiColor = UiColor::rgb(190, 60, 55);
    pub const WARNING: UiColor = UiColor::rgb(230, 180, 60);
    pub const HEALTH: UiColor = UiColor::rgb(210, 45, 45);
    pub const ABSORPTION: UiColor = UiColor::rgb(235, 190, 60);
    pub const ARMOR: UiColor = UiColor::rgb(170, 180, 190);
    pub const HUNGER: UiColor = UiColor::rgb(150, 90, 40);
    pub const XP: UiColor = UiColor::rgb(125, 220, 45);
    pub const AIR: UiColor = UiColor::rgb(60, 150, 230);
    pub const SLOT: UiColor = UiColor::rgba(20, 20, 24, 190);
    pub const OVERLAY_DIM: UiColor = UiColor::rgba(8, 8, 12, 170);
}

/// One recorded draw operation. Text positions are the *top-left* of the
/// first glyph cell; `scale` multiplies the 8×8 cell, so `scale = 2` yields
/// 16-px-tall glyphs (full-width glyphs advance [`FULLWIDTH_PX`]×scale px).
#[derive(Clone, Debug, PartialEq)]
pub enum UiCommand {
    /// Filled rectangle.
    FillRect { rect: UiRect, color: UiColor },
    /// Rectangle outline drawn *inside* `rect`, `thickness` px wide.
    BorderRect {
        rect: UiRect,
        color: UiColor,
        thickness: u32,
    },
    /// Monospace bitmap text. `'\n'` starts a new line at the original `x`.
    Text {
        x: i32,
        y: i32,
        scale: u32,
        color: UiColor,
        text: String,
    },
    /// Horizontal progress bar: border, background, then a fill whose
    /// width is `fraction` (clamped to `0.0..=1.0`) of the interior.
    ProgressBar {
        rect: UiRect,
        fraction: f32,
        fill: UiColor,
        back: UiColor,
        border: UiColor,
    },
    /// An item icon slot. When the host supplied an [`ItemIconAtlas`]
    /// (real `assets/*/textures/item/*.png` sprites) the GPU quad path
    /// samples the atlas; every other path — and unknown ids — paints the
    /// deterministic swatch + monogram, so a slot is never invisible.
    ItemIcon { rect: UiRect, item_id: String },
}

/// An ordered list of 2D commands; painting is back-to-front.
///
/// [`clear`](Self::clear) recycles each `Text` command's `String` into a
/// private pool (content dropped, heap capacity kept), and
/// [`text_str`](Self::text_str) refills a pooled buffer instead of
/// allocating — a screen rebuilt every frame through
/// [`crate::state::UiState::render_into`] emits text with zero heap traffic
/// once the pool matches the frame's widget count.
#[derive(Clone, Debug, Default)]
pub struct UiDrawList {
    pub commands: Vec<UiCommand>,
    /// Recycled `Text` payload heaps; see the type docs. Contents are
    /// stale — entries are cleared before reuse — so the pool never
    /// affects equality or rendering.
    string_pool: Vec<String>,
}

impl PartialEq for UiDrawList {
    /// Draw lists compare by commands only; pooled string capacity is an
    /// allocation detail, not content.
    fn eq(&self, other: &Self) -> bool {
        self.commands == other.commands
    }
}

impl UiDrawList {
    pub fn new() -> Self {
        Self::default()
    }

    /// Drop every command while keeping `commands` capacity and recycling
    /// `Text`/`ItemIcon` payloads into the string pool for
    /// [`text_str`](Self::text_str).
    pub fn clear(&mut self) {
        for command in self.commands.drain(..) {
            match command {
                UiCommand::Text { text, .. } => self.string_pool.push(text),
                UiCommand::ItemIcon { item_id, .. } => self.string_pool.push(item_id),
                _ => {}
            }
        }
    }

    /// Number of recycled `Text` payload buffers held for reuse.
    pub fn recycled_text_buffer_count(&self) -> usize {
        self.string_pool.len()
    }

    pub fn push(&mut self, command: UiCommand) {
        self.commands.push(command);
    }

    pub fn fill_rect(&mut self, rect: UiRect, color: UiColor) {
        self.push(UiCommand::FillRect { rect, color });
    }

    pub fn border_rect(&mut self, rect: UiRect, color: UiColor, thickness: u32) {
        self.push(UiCommand::BorderRect {
            rect,
            color,
            thickness,
        });
    }

    pub fn text(&mut self, x: i32, y: i32, scale: u32, color: UiColor, text: impl Into<String>) {
        self.push(UiCommand::Text {
            x,
            y,
            scale: scale.max(1),
            color,
            text: text.into(),
        });
    }

    /// `&str` text that reuses a recycled payload buffer (see
    /// [`clear`](Self::clear)) instead of allocating a fresh `String` —
    /// zero heap traffic on a warm pool.
    pub fn text_str(&mut self, x: i32, y: i32, scale: u32, color: UiColor, text: &str) {
        let mut owned = self.string_pool.pop().unwrap_or_default();
        owned.clear();
        owned.push_str(text);
        self.push(UiCommand::Text {
            x,
            y,
            scale: scale.max(1),
            color,
            text: owned,
        });
    }

    pub fn progress_bar(
        &mut self,
        rect: UiRect,
        fraction: f32,
        fill: UiColor,
        back: UiColor,
        border: UiColor,
    ) {
        self.push(UiCommand::ProgressBar {
            rect,
            fraction,
            fill,
            back,
            border,
        });
    }

    /// Item icon slot (see [`UiCommand::ItemIcon`]). `id` is the
    /// namespaced item id (`"minecraft:iron_ingot"`).
    pub fn item_icon(&mut self, rect: UiRect, id: &str) {
        self.push(UiCommand::ItemIcon {
            rect,
            item_id: id.to_owned(),
        });
    }

    /// [`item_icon`](Self::item_icon) over a recycled payload buffer —
    /// the same zero-alloc-on-warm-pool contract as [`text_str`].
    pub fn item_icon_str(&mut self, rect: UiRect, id: &str) {
        let mut owned = self.string_pool.pop().unwrap_or_default();
        owned.clear();
        owned.push_str(id);
        self.push(UiCommand::ItemIcon {
            rect,
            item_id: owned,
        });
    }

    pub fn is_empty(&self) -> bool {
        self.commands.is_empty()
    }

    pub fn len(&self) -> usize {
        self.commands.len()
    }
}

// ---------------------------------------------------------------------------
// Built-in clean-room 8x8 bitmap font
// ---------------------------------------------------------------------------

/// Pixel width/height of one glyph cell at `scale = 1`.
pub const GLYPH_SIZE: u32 = 8;

/// Logical-pixel advance (and painted width) of a full-width glyph at
/// `scale = 1`. The glyph art lives on a 16-column "double cell" canvas
/// (see [`crate::font_cjk`]); painting it 1:1 made every CJK label read
/// as a squashed 2:1 pancake and pushed Japanese button captions past
/// their rects, so both renderers squeeze the 16 source columns into
/// [`FULLWIDTH_PX`] px — near the 8-px line height, i.e. close to the
/// square proportions East-Asian glyphs are designed for.
pub const FULLWIDTH_PX: u32 = 12;

/// Number of glyphs in [`FONT_8X8`]: printable ASCII `0x20..=0x7E`.
pub const FONT_GLYPH_COUNT: usize = 95;

/// Compile a glyph written as eight 8-char rows of `#` (set) and anything
/// else (clear) into eight row bytes, MSB = leftmost pixel.
const fn glyph(rows: [&str; 8]) -> [u8; 8] {
    let mut out = [0u8; 8];
    let mut i = 0;
    while i < 8 {
        let bytes = rows[i].as_bytes();
        let mut bits = 0u8;
        let mut j = 0;
        while j < 8 && j < bytes.len() {
            if bytes[j] == b'#' {
                bits |= 0x80 >> j;
            }
            j += 1;
        }
        out[i] = bits;
        i += 1;
    }
    out
}

/// Glyph painted for every code point outside printable ASCII (including
/// `0x7F` and all non-ASCII): a filled-box "unknown character" mark so text
/// output stays deterministic and visibly non-blank.
pub const FALLBACK_GLYPH: [u8; 8] = glyph([
    "########", "#......#", "#.####.#", "#....#.#", "#...#..#", "#..#...#", "#......#", "########",
]);

/// Original clean-room 8×8 pixel face covering printable ASCII
/// `0x20..=0x7E`. Index `c - 0x20` gives glyph `c`.
#[rustfmt::skip]
pub const FONT_8X8: [[u8; 8]; FONT_GLYPH_COUNT] = [
    // 0x20 ' '
    glyph(["........", "........", "........", "........", "........", "........", "........", "........"]),
    // 0x21 '!'
    glyph(["..##....", "..##....", "..##....", "..##....", "..##....", "........", "..##....", "........"]),
    // 0x22 '"'
    glyph([".##.##..", ".##.##..", "..#..#..", "........", "........", "........", "........", "........"]),
    // 0x23 '#'
    glyph(["..#..#..", "..#..#..", ".######.", "..#..#..", ".######.", "..#..#..", "..#..#..", "........"]),
    // 0x24 '$'
    glyph(["...#....", ".#####..", "#.#.....", ".###....", "....#.#.", "#####...", "...#....", "........"]),
    // 0x25 '%'
    glyph([".##..#..", ".##.#...", "...#....", "..#.....", ".#......", "#.##....", "#..##...", "........"]),
    // 0x26 '&'
    glyph([".###....", "#...#...", "#...#...", ".###....", "#..#.#..", "#...#...", ".###.#..", "........"]),
    // 0x27 '''
    glyph(["..##....", "..##....", "..#.....", "........", "........", "........", "........", "........"]),
    // 0x28 '('
    glyph(["...##...", "..#.....", ".#......", ".#......", ".#......", ".#......", "..#.....", "...##..."]),
    // 0x29 ')'
    glyph([".##.....", "...#....", "....#...", "....#...", "....#...", "....#...", "...#....", ".##....."]),
    // 0x2a '*'
    glyph(["........", "..#.#...", "...#....", ".#####..", "...#....", "..#.#...", "........", "........"]),
    // 0x2b '+'
    glyph(["........", "...#....", "...#....", ".#####..", "...#....", "...#....", "........", "........"]),
    // 0x2c ','
    glyph(["........", "........", "........", "........", "........", "..##....", "..##....", "..#....."]),
    // 0x2d '-'
    glyph(["........", "........", "........", ".#####..", "........", "........", "........", "........"]),
    // 0x2e '.'
    glyph(["........", "........", "........", "........", "........", "..##....", "..##....", "........"]),
    // 0x2f '/'
    glyph(["......#.", ".....#..", "....#...", "...#....", "..#.....", ".#......", "#.......", "........"]),
    // 0x30 '0'
    glyph([".###....", "#...#...", "#..##...", "#.#.#...", "##..#...", "#...#...", ".###....", "........"]),
    // 0x31 '1'
    glyph(["..#.....", ".##.....", "#.#.....", "..#.....", "..#.....", "..#.....", "#####...", "........"]),
    // 0x32 '2'
    glyph([".###....", "#...#...", "....#...", "...#....", "..#.....", ".#......", "#####...", "........"]),
    // 0x33 '3'
    glyph([".###....", "#...#...", "....#...", "..##....", "....#...", "#...#...", ".###....", "........"]),
    // 0x34 '4'
    glyph(["...#....", "..##....", ".#.#....", "#..#....", "#####...", "...#....", "...#....", "........"]),
    // 0x35 '5'
    glyph(["#####...", "#.......", "####....", "....#...", "....#...", "#...#...", ".###....", "........"]),
    // 0x36 '6'
    glyph([".###....", "#.......", "####....", "#...#...", "#...#...", "#...#...", ".###....", "........"]),
    // 0x37 '7'
    glyph(["#####...", "....#...", "...#....", "..#.....", ".#......", ".#......", ".#......", "........"]),
    // 0x38 '8'
    glyph([".###....", "#...#...", "#...#...", ".###....", "#...#...", "#...#...", ".###....", "........"]),
    // 0x39 '9'
    glyph([".###....", "#...#...", "#...#...", "#...#...", ".####...", "....#...", ".###....", "........"]),
    // 0x3a ':'
    glyph(["........", "..##....", "..##....", "........", "..##....", "..##....", "........", "........"]),
    // 0x3b ';'
    glyph(["........", "..##....", "..##....", "........", "..##....", "..##....", "..#.....", "........"]),
    // 0x3c '<'
    glyph(["....#...", "...#....", "..#.....", ".#......", "..#.....", "...#....", "....#...", "........"]),
    // 0x3d '='
    glyph(["........", "........", ".#####..", "........", ".#####..", "........", "........", "........"]),
    // 0x3e '>'
    glyph([".#......", "..#.....", "...#....", "....#...", "...#....", "..#.....", ".#......", "........"]),
    // 0x3f '?'
    glyph([".###....", "#...#...", "....#...", "...#....", "..#.....", "........", "..#.....", "........"]),
    // 0x40 '@'
    glyph([".#####..", "#.....#.", "#..##.#.", "#.#.#.#.", "#..###..", "#.......", ".#####..", "........"]),
    // 0x41 'A'
    glyph([".###....", "#...#...", "#...#...", "#####...", "#...#...", "#...#...", "#...#...", "........"]),
    // 0x42 'B'
    glyph(["####....", "#...#...", "#...#...", "####....", "#...#...", "#...#...", "####....", "........"]),
    // 0x43 'C'
    glyph([".####...", "#.......", "#.......", "#.......", "#.......", "#.......", ".####...", "........"]),
    // 0x44 'D'
    glyph(["####....", "#...#...", "#...#...", "#...#...", "#...#...", "#...#...", "####....", "........"]),
    // 0x45 'E'
    glyph(["#####...", "#.......", "#.......", "####....", "#.......", "#.......", "#####...", "........"]),
    // 0x46 'F'
    glyph(["#####...", "#.......", "#.......", "####....", "#.......", "#.......", "#.......", "........"]),
    // 0x47 'G'
    glyph([".####...", "#.......", "#.......", "#.###...", "#...#...", "#...#...", ".###....", "........"]),
    // 0x48 'H'
    glyph(["#...#...", "#...#...", "#...#...", "#####...", "#...#...", "#...#...", "#...#...", "........"]),
    // 0x49 'I'
    glyph(["#####...", "..#.....", "..#.....", "..#.....", "..#.....", "..#.....", "#####...", "........"]),
    // 0x4a 'J'
    glyph(["..###...", "...#....", "...#....", "...#....", "...#....", "#..#....", ".##.....", "........"]),
    // 0x4b 'K'
    glyph(["#...#...", "#..#....", "#.#.....", "##......", "#.#.....", "#..#....", "#...#...", "........"]),
    // 0x4c 'L'
    glyph(["#.......", "#.......", "#.......", "#.......", "#.......", "#.......", "#####...", "........"]),
    // 0x4d 'M'
    glyph(["#...#...", "##.##...", "#.#.#...", "#.#.#...", "#...#...", "#...#...", "#...#...", "........"]),
    // 0x4e 'N'
    glyph(["#...#...", "##..#...", "##..#...", "#.#.#...", "#..##...", "#..##...", "#...#...", "........"]),
    // 0x4f 'O'
    glyph([".###....", "#...#...", "#...#...", "#...#...", "#...#...", "#...#...", ".###....", "........"]),
    // 0x50 'P'
    glyph(["####....", "#...#...", "#...#...", "####....", "#.......", "#.......", "#.......", "........"]),
    // 0x51 'Q'
    glyph([".###....", "#...#...", "#...#...", "#...#...", "#.#.#...", "#..#....", ".##.#...", "........"]),
    // 0x52 'R'
    glyph(["####....", "#...#...", "#...#...", "####....", "#.#.....", "#..#....", "#...#...", "........"]),
    // 0x53 'S'
    glyph([".####...", "#.......", "#.......", ".###....", "....#...", "....#...", "####....", "........"]),
    // 0x54 'T'
    glyph(["#####...", "..#.....", "..#.....", "..#.....", "..#.....", "..#.....", "..#.....", "........"]),
    // 0x55 'U'
    glyph(["#...#...", "#...#...", "#...#...", "#...#...", "#...#...", "#...#...", ".###....", "........"]),
    // 0x56 'V'
    glyph(["#...#...", "#...#...", "#...#...", "#...#...", "#...#...", ".#.#....", "..#.....", "........"]),
    // 0x57 'W'
    glyph(["#...#...", "#...#...", "#...#...", "#.#.#...", "#.#.#...", "##.##...", "#...#...", "........"]),
    // 0x58 'X'
    glyph(["#...#...", "#...#...", ".#.#....", "..#.....", ".#.#....", "#...#...", "#...#...", "........"]),
    // 0x59 'Y'
    glyph(["#...#...", "#...#...", ".#.#....", "..#.....", "..#.....", "..#.....", "..#.....", "........"]),
    // 0x5a 'Z'
    glyph(["#####...", "....#...", "...#....", "..#.....", ".#......", "#.......", "#####...", "........"]),
    // 0x5b '['
    glyph([".###....", ".#......", ".#......", ".#......", ".#......", ".#......", ".#......", ".###...."]),
    // 0x5c '\'
    glyph(["#.......", ".#......", "..#.....", "...#....", "....#...", ".....#..", "......#.", "........"]),
    // 0x5d ']'
    glyph([".###....", "...#....", "...#....", "...#....", "...#....", "...#....", "...#....", ".###...."]),
    // 0x5e '^'
    glyph(["..#.....", ".#.#....", "#...#...", "........", "........", "........", "........", "........"]),
    // 0x5f '_'
    glyph(["........", "........", "........", "........", "........", "........", "........", "######.."]),
    // 0x60 '`'
    glyph([".#......", "..#.....", "........", "........", "........", "........", "........", "........"]),
    // 0x61 'a'
    glyph(["........", "........", ".###....", "....#...", ".####...", "#...#...", ".####...", "........"]),
    // 0x62 'b'
    glyph(["#.......", "#.......", "####....", "#...#...", "#...#...", "#...#...", "####....", "........"]),
    // 0x63 'c'
    glyph(["........", "........", ".####...", "#.......", "#.......", "#.......", ".####...", "........"]),
    // 0x64 'd'
    glyph(["....#...", "....#...", ".####...", "#...#...", "#...#...", "#...#...", ".####...", "........"]),
    // 0x65 'e'
    glyph(["........", "........", ".###....", "#...#...", "#####...", "#.......", ".####...", "........"]),
    // 0x66 'f'
    glyph(["..##....", ".#......", ".#......", "###.....", ".#......", ".#......", ".#......", "........"]),
    // 0x67 'g'
    glyph(["........", "........", ".####...", "#...#...", "#...#...", ".####...", "....#...", ".###...."]),
    // 0x68 'h'
    glyph(["#.......", "#.......", "####....", "#...#...", "#...#...", "#...#...", "#...#...", "........"]),
    // 0x69 'i'
    glyph(["..#.....", "........", ".##.....", "..#.....", "..#.....", "..#.....", ".###....", "........"]),
    // 0x6a 'j'
    glyph(["...#....", "........", "..##....", "...#....", "...#....", "...#....", "#..#....", ".##....."]),
    // 0x6b 'k'
    glyph([".#......", ".#......", ".#..#...", ".#.#....", ".##.....", ".#.#....", ".#..#...", "........"]),
    // 0x6c 'l'
    glyph([".##.....", "..#.....", "..#.....", "..#.....", "..#.....", "..#.....", ".###....", "........"]),
    // 0x6d 'm'
    glyph(["........", "........", "##.##...", "#.#.#...", "#.#.#...", "#...#...", "#...#...", "........"]),
    // 0x6e 'n'
    glyph(["........", "........", "####....", "#...#...", "#...#...", "#...#...", "#...#...", "........"]),
    // 0x6f 'o'
    glyph(["........", "........", ".###....", "#...#...", "#...#...", "#...#...", ".###....", "........"]),
    // 0x70 'p'
    glyph(["........", "........", "####....", "#...#...", "#...#...", "####....", "#.......", "#......."]),
    // 0x71 'q'
    glyph(["........", "........", ".####...", "#...#...", "#...#...", ".####...", "....#...", "....#..."]),
    // 0x72 'r'
    glyph(["........", "........", "#.##....", "##..#...", "#.......", "#.......", "#.......", "........"]),
    // 0x73 's'
    glyph(["........", "........", ".####...", "#.......", ".###....", "....#...", "####....", "........"]),
    // 0x74 't'
    glyph([".#......", ".#......", "###.....", ".#......", ".#......", ".#......", "..##....", "........"]),
    // 0x75 'u'
    glyph(["........", "........", "#...#...", "#...#...", "#...#...", "#...#...", ".####...", "........"]),
    // 0x76 'v'
    glyph(["........", "........", "#...#...", "#...#...", "#...#...", ".#.#....", "..#.....", "........"]),
    // 0x77 'w'
    glyph(["........", "........", "#...#...", "#...#...", "#.#.#...", "#.#.#...", ".#.#....", "........"]),
    // 0x78 'x'
    glyph(["........", "........", "#...#...", ".#.#....", "..#.....", ".#.#....", "#...#...", "........"]),
    // 0x79 'y'
    glyph(["........", "........", "#...#...", "#...#...", "#...#...", ".####...", "....#...", ".###...."]),
    // 0x7a 'z'
    glyph(["........", "........", "#####...", "...#....", "..#.....", ".#......", "#####...", "........"]),
    // 0x7b '{'
    glyph(["...##...", "..#.....", "..#.....", ".#......", "..#.....", "..#.....", "..#.....", "...##..."]),
    // 0x7c '|'
    glyph(["...#....", "...#....", "...#....", "...#....", "...#....", "...#....", "...#....", "...#...."]),
    // 0x7d '}'
    glyph([".##.....", "...#....", "...#....", "....#...", "...#....", "...#....", "...#....", ".##....."]),
    // 0x7e '~'
    glyph(["........", "........", ".##..#..", "#..##...", "........", "........", "........", "........"]),
];

/// Atlas tile index of the *first* (left or only) tile of `c`'s glyph:
/// `c - 0x20` for printable ASCII, `EXTRA_GLYPH_TILE_BASE + 2*i` for a
/// built-in extended glyph, [`crate::font_cjk::WIDE_FALLBACK_TILE`] for an
/// unmapped full-width codepoint, or [`FONT_GLYPH_COUNT`] (the fallback
/// tile) otherwise. A full-width glyph's right tile is `index + 1`.
pub fn glyph_index(c: char) -> u32 {
    let cp = c as u32;
    if (0x20..=0x7e).contains(&cp) {
        return cp - 0x20;
    }
    if let Some((idx, _)) = crate::font_cjk::lookup_extra(c) {
        return crate::font_cjk::EXTRA_GLYPH_TILE_BASE + 2 * idx as u32;
    }
    if crate::font_cjk::is_fullwidth_codepoint(cp) {
        return crate::font_cjk::WIDE_FALLBACK_TILE;
    }
    FONT_GLYPH_COUNT as u32
}

/// Glyph bitmap for `c` (eight row bytes, MSB = leftmost pixel).
///
/// For full-width glyphs this returns the *left* cell of the pair; use
/// [`glyph_bitmap`] for the complete glyph.
pub fn glyph_for(c: char) -> &'static [u8; 8] {
    let cp = c as u32;
    if (0x20..=0x7e).contains(&cp) {
        return &FONT_8X8[(cp - 0x20) as usize];
    }
    if let Some((idx, _)) = crate::font_cjk::lookup_extra(c) {
        return &crate::font_cjk::EXTRA_GLYPHS[idx].2[0];
    }
    &FALLBACK_GLYPH
}

/// Complete raster data for `c`: one or two 8×8 cells painted left-to-right.
///
/// Resolution order is deterministic: the built-in ASCII face, then the
/// built-in extended table ([`crate::font_cjk::EXTRA_GLYPHS`]), then the
/// installed external [`crate::font_cjk::GlyphAtlas`] pack, then the
/// fallback box — wide when `c` is an East-Asian full-width codepoint,
/// narrow otherwise — so every character advances without overlap.
pub fn glyph_bitmap(c: char) -> crate::font_cjk::GlyphBitmap {
    let cp = c as u32;
    if (0x20..=0x7e).contains(&cp) {
        return crate::font_cjk::GlyphBitmap {
            cells: 1,
            tiles: [FONT_8X8[(cp - 0x20) as usize], [0; 8]],
        };
    }
    if let Some((_, bitmap)) = crate::font_cjk::lookup_extra(c) {
        return bitmap;
    }
    if let Some(bitmap) = crate::font_cjk::external_glyph(c) {
        return bitmap;
    }
    if crate::font_cjk::is_fullwidth_codepoint(cp) {
        return crate::font_cjk::GlyphBitmap {
            cells: 2,
            tiles: crate::font_cjk::WIDE_FALLBACK_GLYPH,
        };
    }
    crate::font_cjk::GlyphBitmap {
        cells: 1,
        tiles: [FALLBACK_GLYPH, [0; 8]],
    }
}

/// Width class of `c` in 8-px source cells: 1 for half-width, 2 for
/// full-width. This counts the glyph's *source tiles* — the rendered
/// pen advance is [`glyph_advance`] (a full-width glyph's two 8×8 tiles
/// squeeze into [`FULLWIDTH_PX`] px).
pub fn glyph_cells(c: char) -> u32 {
    u32::from(glyph_bitmap(c).cells.max(1))
}

/// Logical px `c` advances the pen at `scale = 1`: [`GLYPH_SIZE`] for
/// half-width glyphs, [`FULLWIDTH_PX`] for full-width ones. Multiply by
/// `scale` for the rendered advance.
pub fn glyph_advance(c: char) -> u32 {
    if glyph_cells(c) >= 2 {
        FULLWIDTH_PX
    } else {
        GLYPH_SIZE
    }
}

/// `true` when `c` has a real glyph (built-in ASCII, built-in extended
/// table, or the installed external pack) — `false` when it would draw a
/// fallback "tofu" box.
pub fn glyph_is_covered(c: char) -> bool {
    let cp = c as u32;
    if (0x20..=0x7e).contains(&cp) {
        return true;
    }
    if crate::font_cjk::lookup_extra(c).is_some() {
        return true;
    }
    crate::font_cjk::external_glyph(c).is_some()
}

/// Pixel width of `text` rendered at `scale`: every glyph advances
/// [`glyph_advance`] `× scale` px (half-width 8, full-width
/// [`FULLWIDTH_PX`] at `scale = 1`), including spaces; `'\n'` resets to
/// a new line and is not counted.
pub fn text_width(text: &str, scale: u32) -> u32 {
    let mut width = 0u32;
    let mut line = 0u32;
    for c in text.chars() {
        if c == '\n' {
            width = width.max(line);
            line = 0;
        } else {
            line += glyph_advance(c) * scale;
        }
    }
    width.max(line)
}

/// `(width, height)` in pixels of `text` rendered at `scale` — the pair of
/// [`text_width`] and [`text_height`] in one call.
pub fn measure_text_px(text: &str, scale: u32) -> (u32, u32) {
    (text_width(text, scale), text_height(text, scale))
}

/// Glyph cells `text` occupies (sum of [`glyph_cells`], `'\n'` excluded
/// from the running count). Cells are 8-px budget units: because a
/// full-width glyph costs 2 cells but renders [`FULLWIDTH_PX`] px,
/// `text_cells(t) * 8` is always `>= text_width(t, 1)` — a cell-count
/// clip is conservative and can never overrun its pixel budget.
pub fn text_cells(text: &str) -> usize {
    let mut cells = 0usize;
    for c in text.chars() {
        if c != '\n' {
            cells += glyph_cells(c) as usize;
        }
    }
    cells
}

/// Longest prefix of `text` that fits `limit` glyph cells (no allocation).
/// A full-width glyph that would only half-fit is dropped whole — glyphs
/// are never clipped mid-cell.
pub fn first_cells(text: &str, limit: usize) -> &str {
    let mut used = 0usize;
    for (end, c) in text.char_indices() {
        let cells = glyph_cells(c) as usize;
        if used + cells > limit {
            return &text[..end];
        }
        used += cells;
    }
    text
}

/// Longest suffix of `text` that fits `limit` glyph cells — the
/// mirror of [`first_cells`] for right-aligned scrolling text.
pub fn last_cells(text: &str, limit: usize) -> &str {
    let mut used = 0usize;
    for (start, c) in text.char_indices().rev() {
        let cells = glyph_cells(c) as usize;
        if used + cells > limit {
            return &text[start + c.len_utf8()..];
        }
        used += cells;
    }
    text
}

/// Longest prefix of `text` whose rendered advance fits `limit_px`
/// scale-1 pixels. The exact counterpart of [`first_cells`] for pixel
/// budgets: a full-width glyph costs [`FULLWIDTH_PX`] px, not two 8-px
/// cells, so pixel-budget callers get the true fit instead of
/// under-filling Japanese text by a third.
pub fn first_px(text: &str, limit_px: usize) -> &str {
    let mut used = 0usize;
    for (end, c) in text.char_indices() {
        let w = glyph_advance(c) as usize;
        if used + w > limit_px {
            return &text[..end];
        }
        used += w;
    }
    text
}

/// Longest suffix of `text` whose rendered advance fits `limit_px`
/// scale-1 pixels — the mirror of [`first_px`].
pub fn last_px(text: &str, limit_px: usize) -> &str {
    let mut used = 0usize;
    for (start, c) in text.char_indices().rev() {
        let w = glyph_advance(c) as usize;
        if used + w > limit_px {
            return &text[start + c.len_utf8()..];
        }
        used += w;
    }
    text
}

/// Pixel height of `text` rendered at `scale` (8·scale per line).
pub fn text_height(text: &str, scale: u32) -> u32 {
    let lines = text.lines().count().max(1) as u32;
    lines * GLYPH_SIZE * scale
}

// ---------------------------------------------------------------------------
// Font atlas + quad tessellation (the GPU/WGSL path)
// ---------------------------------------------------------------------------

/// Tiles per atlas row.
pub const ATLAS_COLS: u32 = 16;
/// Atlas rows: 95 ASCII tiles + the narrow fallback + the solid-white tile
/// at [`WHITE_TILE_INDEX`] + the two-tile wide fallback + two tiles per
/// built-in extended glyph ([`crate::font_cjk::EXTRA_GLYPH_COUNT`]).
/// Glyphs installed from an external pack have no atlas tile — the GPU
/// path draws the fallback box for them (the software rasterizer paints
/// them correctly); baking a pack into the atlas is an embedder choice.
pub const ATLAS_ROWS: u32 = {
    let tiles =
        crate::font_cjk::EXTRA_GLYPH_TILE_BASE + 2 * crate::font_cjk::EXTRA_GLYPH_COUNT as u32;
    tiles.div_ceil(ATLAS_COLS)
};
/// Atlas tile index of the solid white tile (`color * 1.0` passthrough).
pub const WHITE_TILE_INDEX: u32 = (FONT_GLYPH_COUNT as u32) + 1;
/// Atlas dimensions in pixels (128 px wide = sixteen 8×8 tiles per row;
/// [`ATLAS_ROWS`] × 8 px tall).
pub const FONT_ATLAS_WIDTH: u32 = ATLAS_COLS * GLYPH_SIZE;
pub const FONT_ATLAS_HEIGHT: u32 = ATLAS_ROWS * GLYPH_SIZE;

/// The one texture the WGSL path needs: `FONT_ATLAS_WIDTH`×
/// `FONT_ATLAS_HEIGHT` RGBA8. Glyph tiles are white-on-transparent (alpha =
/// ink mask) so vertex colors tint them; tile [`WHITE_TILE_INDEX`] is a
/// solid opaque white square used by every non-text quad.
pub fn font_atlas_rgba() -> Vec<u8> {
    let w = FONT_ATLAS_WIDTH as usize;
    let h = FONT_ATLAS_HEIGHT as usize;
    let mut out = vec![0u8; w * h * 4];
    let mut paint = |tile: u32, bitmap: &[u8; 8]| {
        let tile_x = (tile % ATLAS_COLS) * GLYPH_SIZE;
        let tile_y = (tile / ATLAS_COLS) * GLYPH_SIZE;
        for (row, bits) in bitmap.iter().enumerate() {
            for col in 0..8usize {
                if (bits & (0x80 >> col)) == 0 {
                    continue;
                }
                let px = (tile_x as usize) + col;
                let py = (tile_y as usize) + row;
                let o = (py * w + px) * 4;
                out[o] = 0xff;
                out[o + 1] = 0xff;
                out[o + 2] = 0xff;
                out[o + 3] = 0xff;
            }
        }
    };
    for (tile, bitmap) in FONT_8X8.iter().enumerate() {
        paint(tile as u32, bitmap);
    }
    paint(FONT_GLYPH_COUNT as u32, &FALLBACK_GLYPH);
    paint(WHITE_TILE_INDEX, &[0xff; 8]);
    paint(
        crate::font_cjk::WIDE_FALLBACK_TILE,
        &crate::font_cjk::WIDE_FALLBACK_GLYPH[0],
    );
    paint(
        crate::font_cjk::WIDE_FALLBACK_TILE + 1,
        &crate::font_cjk::WIDE_FALLBACK_GLYPH[1],
    );
    for (i, (_, _, tiles)) in crate::font_cjk::EXTRA_GLYPHS.iter().enumerate() {
        let base = crate::font_cjk::EXTRA_GLYPH_TILE_BASE + 2 * i as u32;
        paint(base, &tiles[0]);
        paint(base + 1, &tiles[1]);
    }
    out
}

/// Normalized UV rectangle of atlas `tile` (the region one quad samples).
fn tile_uv(tile: u32) -> [f32; 4] {
    let tx = (tile % ATLAS_COLS) as f32 * GLYPH_SIZE as f32;
    let ty = (tile / ATLAS_COLS) as f32 * GLYPH_SIZE as f32;
    [
        tx / FONT_ATLAS_WIDTH as f32,
        ty / FONT_ATLAS_HEIGHT as f32,
        (tx + GLYPH_SIZE as f32) / FONT_ATLAS_WIDTH as f32,
        (ty + GLYPH_SIZE as f32) / FONT_ATLAS_HEIGHT as f32,
    ]
}

// ---------------------------------------------------------------------------
// UI item-icon atlas (`assets/<ns>/textures/{item,block}/*.png` sprites)
// ---------------------------------------------------------------------------

/// The square cell every item icon is normalized into on the atlas.
pub const ITEM_ICON_CELL: u32 = 16;
/// Hard cap on distinct item ids the atlas can hold — `64×64` cells.
pub const ITEM_ICON_MAX: usize = 4096;

/// Packed RGBA8 item-icon atlas plus the `item_id → uv` lookup the
/// tessellator resolves [`UiCommand::ItemIcon`] against. Built once by
/// the host from real JAR sprites, uploaded to the GPU once, then
/// immutable — steady frames do no work here.
#[derive(Clone, Debug, Default)]
pub struct ItemIconAtlas {
    /// Atlas pixel width (`cols * ITEM_ICON_CELL`).
    pub width: u32,
    /// Atlas pixel height (`rows * ITEM_ICON_CELL`).
    pub height: u32,
    /// Tightly packed `width*height*4` RGBA8.
    pub rgba: Vec<u8>,
    /// `namespaced item id → normalized [u0, v0, u1, v1]`.
    uvs: std::collections::HashMap<String, [f32; 4]>,
}

impl ItemIconAtlas {
    /// Atlas-space uv rect for `id`, if it was added to the atlas.
    pub fn uv(&self, id: &str) -> Option<[f32; 4]> {
        self.uvs.get(id).copied()
    }

    /// Distinct item ids packed.
    pub fn len(&self) -> usize {
        self.uvs.len()
    }

    /// `true` when no sprites were packed (all icons fall back).
    pub fn is_empty(&self) -> bool {
        self.uvs.is_empty()
    }
}

/// Accumulates `(id, w, h, rgba)` sprites, normalizing each into a
/// [`ITEM_ICON_CELL`] square at `add` time; `finish` packs the grid.
/// Duplicate ids keep the first sprite (later JARs never stomp an item
/// icon — same contract as recipe dedup).
#[derive(Default)]
pub struct ItemIconAtlasBuilder {
    sprites: Vec<(String, [u8; (ITEM_ICON_CELL * ITEM_ICON_CELL * 4) as usize])>,
    seen: std::collections::HashSet<String>,
}

impl ItemIconAtlasBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Add `id`'s `w`×`h` RGBA8 sprite. Animated strips (`h` a multiple
    /// of `w` taller than one frame) contribute only their first `w×w`
    /// frame; any other aspect ratio samples the whole image. Returns
    /// `false` for malformed payloads, duplicates, or a full atlas.
    pub fn add(&mut self, id: &str, w: u32, h: u32, rgba: &[u8]) -> bool {
        if id.is_empty()
            || w == 0
            || h == 0
            || rgba.len() != w as usize * h as usize * 4
            || self.sprites.len() >= ITEM_ICON_MAX
            || !self.seen.insert(id.to_owned())
        {
            return false;
        }
        let cell = ITEM_ICON_CELL as usize;
        // Animated vertical strips: the first square frame is the icon.
        let frame_h = if h > w && h.is_multiple_of(w) { w } else { h };
        let mut out = [0u8; (ITEM_ICON_CELL * ITEM_ICON_CELL * 4) as usize];
        for cy in 0..cell {
            let sy = (cy as u32 * frame_h / ITEM_ICON_CELL) as usize;
            for cx in 0..cell {
                let sx = (cx as u32 * w / ITEM_ICON_CELL) as usize;
                let s = (sy * w as usize + sx) * 4;
                let d = (cy * cell + cx) * 4;
                out[d..d + 4].copy_from_slice(&rgba[s..s + 4]);
            }
        }
        self.sprites.push((id.to_owned(), out));
        true
    }

    /// Pack every accepted sprite into a near-square grid atlas.
    pub fn finish(self) -> ItemIconAtlas {
        let n = self.sprites.len();
        if n == 0 {
            return ItemIconAtlas::default();
        }
        let cols = (n as f64).sqrt().ceil() as u32;
        let rows = (n as u32).div_ceil(cols);
        let cell = ITEM_ICON_CELL as usize;
        let (aw, ah) = (cols as usize * cell, rows as usize * cell);
        let mut rgba = vec![0u8; aw * ah * 4];
        let mut uvs = std::collections::HashMap::with_capacity(n);
        for (i, (id, sprite)) in self.sprites.into_iter().enumerate() {
            let (cx, cy) = (i as u32 % cols, i as u32 / cols);
            let (px, py) = (cx as usize * cell, cy as usize * cell);
            for row in 0..cell {
                let s = row * cell * 4;
                let d = ((py + row) * aw + px) * 4;
                rgba[d..d + cell * 4].copy_from_slice(&sprite[s..s + cell * 4]);
            }
            let (w, h) = (aw as f32, ah as f32);
            uvs.insert(
                id,
                [
                    (px as f32 + 0.5) / w,
                    (py as f32 + 0.5) / h,
                    (px as f32 + cell as f32 - 0.5) / w,
                    (py as f32 + cell as f32 - 0.5) / h,
                ],
            );
        }
        ItemIconAtlas {
            width: aw as u32,
            height: ah as u32,
            rgba,
            uvs,
        }
    }
}

/// Deterministic swatch + monogram fallback for an `ItemIcon` with no
/// atlas entry — identical hashing to `item_color` in `recipe_browser`
/// so both code paths paint the same color for the same id.
fn icon_fallback(item_id: &str) -> (UiColor, char) {
    const RAMP: [UiColor; 16] = [
        UiColor::rgb(188, 84, 84),
        UiColor::rgb(188, 132, 84),
        UiColor::rgb(188, 172, 84),
        UiColor::rgb(140, 188, 84),
        UiColor::rgb(84, 188, 96),
        UiColor::rgb(84, 188, 150),
        UiColor::rgb(84, 178, 188),
        UiColor::rgb(84, 136, 188),
        UiColor::rgb(96, 96, 188),
        UiColor::rgb(140, 84, 188),
        UiColor::rgb(188, 84, 168),
        UiColor::rgb(188, 84, 116),
        UiColor::rgb(150, 150, 160),
        UiColor::rgb(110, 124, 84),
        UiColor::rgb(96, 110, 140),
        UiColor::rgb(176, 148, 96),
    ];
    let mut hash: u32 = 0x811c_9dc5;
    for &b in item_id.as_bytes() {
        hash = (hash ^ u32::from(b)).wrapping_mul(0x0100_0193);
    }
    let letter = item_id
        .rsplit([':', '/'])
        .next()
        .and_then(|s| s.chars().next())
        .unwrap_or('?')
        .to_ascii_uppercase();
    (RAMP[(hash >> 16) as usize & 15], letter)
}

/// One vertex of the baked UI mesh: pixel-space `position`, normalized
/// `uv` into [`font_atlas_rgba`], and straight-alpha `color`.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct UiVertex {
    pub position: [f32; 2],
    pub uv: [f32; 2],
    pub color: [f32; 4],
}

/// Triangle soup ready for a Vulkan/WGSL pipeline: `vertices` +
/// `indices` (two triangles per quad, clockwise TL→BL→BR / TL→BR→TR).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct UiMesh {
    pub vertices: Vec<UiVertex>,
    pub indices: Vec<u32>,
}

/// WGSL source of the companion shader [`tessellate_draw_list`] output is
/// built for. Bindings: `group(0)` `0` = uniforms (screen size in pixels),
/// `1` = the font atlas texture, `2` = a nearest-neighbor sampler,
/// `3` = the [`ItemIconAtlas`] texture, `4` = its sampler. Vertex inputs
/// match [`UiVertex`] field order. An icon quad marks its uv by biasing
/// `u` into `[-2.0, -1.0]` — impossible for real font-atlas uvs — so the
/// fragment stage samples the second texture instead. Alpha-blended over
/// whatever is underneath; embedders enable standard src-over blending
/// on the color target.
pub const UI_QUAD_WGSL: &str = r#"
struct UiUniforms {
    screen_size : vec2<f32>,
    pad0        : vec2<f32>,
};
@group(0) @binding(0) var<uniform> u : UiUniforms;
@group(0) @binding(1) var atlas_tex : texture_2d<f32>;
@group(0) @binding(2) var atlas_smp : sampler;
@group(0) @binding(3) var icon_tex : texture_2d<f32>;
@group(0) @binding(4) var icon_smp : sampler;

struct VsIn {
    @location(0) position : vec2<f32>,
    @location(1) uv       : vec2<f32>,
    @location(2) color    : vec4<f32>,
};
struct VsOut {
    @builtin(position) clip : vec4<f32>,
    @location(0) uv         : vec2<f32>,
    @location(1) color      : vec4<f32>,
};

@vertex
fn vs_main(in : VsIn) -> VsOut {
    var out : VsOut;
    let ndc = vec2<f32>(
        in.position.x / u.screen_size.x * 2.0 - 1.0,
        1.0 - in.position.y / u.screen_size.y * 2.0,
    );
    out.clip  = vec4<f32>(ndc, 0.0, 1.0);
    out.uv    = in.uv;
    out.color = in.color;
    return out;
}

@fragment
fn fs_main(in : VsOut) -> @location(0) vec4<f32> {
    // Item-icon quads carry u = icon_u - 2.0 (see UiMesh::icon_quad);
    // font/fill quads always keep u >= 0.
    if (in.uv.x < -1.0) {
        let texel = textureSample(icon_tex, icon_smp, vec2<f32>(in.uv.x + 2.0, in.uv.y));
        return in.color * texel;
    }
    let texel = textureSample(atlas_tex, atlas_smp, in.uv);
    return in.color * texel;
}
"#;

impl UiMesh {
    fn quad(&mut self, rect: UiRect, uv: [f32; 4], color: UiColor) {
        let base = self.vertices.len() as u32;
        let (l, t) = (rect.x as f32, rect.y as f32);
        let (r, b) = (rect.right() as f32, rect.bottom() as f32);
        let c = color.as_f32();
        // TL, BL, BR, TR — matches the index order below.
        self.vertices.extend_from_slice(&[
            UiVertex {
                position: [l, t],
                uv: [uv[0], uv[1]],
                color: c,
            },
            UiVertex {
                position: [l, b],
                uv: [uv[0], uv[3]],
                color: c,
            },
            UiVertex {
                position: [r, b],
                uv: [uv[2], uv[3]],
                color: c,
            },
            UiVertex {
                position: [r, t],
                uv: [uv[2], uv[1]],
                color: c,
            },
        ]);
        self.indices
            .extend_from_slice(&[base, base + 1, base + 2, base, base + 2, base + 3]);
    }

    /// A quad sampling the item-icon texture: `u` is biased by `-2.0`
    /// into `[-2.0, -1.0]` — the range real font uvs never reach — so
    /// `fs_main` switches textures per fragment.
    fn icon_quad(&mut self, rect: UiRect, uv: [f32; 4]) {
        self.quad(
            rect,
            [uv[0] - 2.0, uv[1], uv[2] - 2.0, uv[3]],
            UiColor::rgb(255, 255, 255),
        );
    }

    fn border(&mut self, rect: UiRect, color: UiColor, thickness: u32) {
        let uv = tile_uv(WHITE_TILE_INDEX);
        let t = thickness.min(rect.w.min(rect.h)) as i32;
        if t <= 0 {
            return;
        }
        self.quad(UiRect::new(rect.x, rect.y, rect.w, t as u32), uv, color);
        self.quad(
            UiRect::new(rect.x, rect.bottom() - t, rect.w, t as u32),
            uv,
            color,
        );
        let inner_h = rect.h.saturating_sub(2 * t as u32);
        self.quad(UiRect::new(rect.x, rect.y + t, t as u32, inner_h), uv, color);
        self.quad(
            UiRect::new(rect.right() - t, rect.y + t, t as u32, inner_h),
            uv,
            color,
        );
    }
}

/// Bake a draw list into an indexed triangle mesh for the GPU path.
/// Positions stay in pixel space; [`UI_QUAD_WGSL`] maps them to NDC with
/// the `screen_size` uniform (`width`, `height` here are only used for the
/// embedder's convenience — the mesh itself is resolution-independent).
/// [`UiCommand::ItemIcon`] paints its swatch+monogram fallback; hosts
/// with a JAR-built [`ItemIconAtlas`] call
/// [`tessellate_draw_list_icons`] for real item sprites.
pub fn tessellate_draw_list(_width: u32, _height: u32, list: &UiDrawList) -> UiMesh {
    tessellate_draw_list_icons(_width, _height, list, &ItemIconAtlas::default())
}

/// [`tessellate_draw_list`] with an [`ItemIconAtlas`]: `ItemIcon`
/// commands whose id resolved a sprite emit a quad sampling the icon
/// texture (`uv.x = icon_u - 2.0` sentinel the WGSL branches on);
/// unknown ids keep the deterministic swatch + monogram fallback.
pub fn tessellate_draw_list_icons(
    _width: u32,
    _height: u32,
    list: &UiDrawList,
    icons: &ItemIconAtlas,
) -> UiMesh {
    let mut mesh = UiMesh::default();
    let white = tile_uv(WHITE_TILE_INDEX);
    for cmd in &list.commands {
        match cmd {
            UiCommand::FillRect { rect, color } => {
                mesh.quad(*rect, white, *color);
            }
            UiCommand::BorderRect {
                rect,
                color,
                thickness,
            } => {
                mesh.border(*rect, *color, *thickness);
            }
            UiCommand::Text {
                x,
                y,
                scale,
                color,
                text,
            } => {
                let cell = GLYPH_SIZE * scale;
                let (mut cx, mut cy) = (*x, *y);
                for ch in text.chars() {
                    if ch == '\n' {
                        cx = *x;
                        cy += cell as i32;
                        continue;
                    }
                    let wide = glyph_cells(ch) >= 2;
                    let adv = if wide {
                        FULLWIDTH_PX * scale
                    } else {
                        GLYPH_SIZE * scale
                    };
                    if ch != ' ' && ch != '\u{3000}' {
                        let base = glyph_index(ch);
                        if wide {
                            // Full-width art squeezes 16 -> FULLWIDTH_PX
                            // columns: each 8-px tile paints into half
                            // the advance so strokes compress, never clip.
                            let half = adv / 2;
                            mesh.quad(UiRect::new(cx, cy, half, cell), tile_uv(base), *color);
                            mesh.quad(
                                UiRect::new(cx + half as i32, cy, adv - half, cell),
                                tile_uv(base + 1),
                                *color,
                            );
                        } else {
                            mesh.quad(UiRect::new(cx, cy, cell, cell), tile_uv(base), *color);
                        }
                    }
                    cx += adv as i32;
                }
            }
            UiCommand::ProgressBar {
                rect,
                fraction,
                fill,
                back,
                border,
            } => {
                mesh.border(*rect, *border, 1);
                let inner = inset(*rect, 1);
                mesh.quad(inner, white, *back);
                let fw = (inner.w as f32 * fraction.clamp(0.0, 1.0)) as u32;
                if fw > 0 {
                    mesh.quad(UiRect::new(inner.x, inner.y, fw, inner.h), white, *fill);
                }
            }
            UiCommand::ItemIcon { rect, item_id } => {
                if let Some(uv) = icons.uv(item_id) {
                    mesh.icon_quad(*rect, uv);
                } else {
                    icon_fallback_quads(&mut mesh, *rect, item_id);
                }
            }
        }
    }
    mesh
}

/// Swatch + monogram fallback quads for an unmapped `ItemIcon` — the
/// same look the software rasterizer produces so both paths agree.
fn icon_fallback_quads(mesh: &mut UiMesh, rect: UiRect, item_id: &str) {
    let (color, letter) = icon_fallback(item_id);
    mesh.quad(rect, tile_uv(WHITE_TILE_INDEX), color);
    if letter != ' ' {
        let scale = (rect.w.min(rect.h) / GLYPH_SIZE).max(1);
        let cell = GLYPH_SIZE * scale;
        let cx = rect.x + ((rect.w as i32 - cell as i32) / 2).max(0);
        let cy = rect.y + ((rect.h as i32 - cell as i32) / 2).max(0);
        mesh.quad(
            UiRect::new(cx, cy, cell, cell),
            tile_uv(glyph_index(letter)),
            UiColor::rgb(30, 30, 34),
        );
    }
}

/// `rect` shrunk by `m` px on every side (clamped to empty).
const fn inset(rect: UiRect, m: i32) -> UiRect {
    let w = if rect.w as i32 > 2 * m {
        rect.w as i32 - 2 * m
    } else {
        0
    } as u32;
    let h = if rect.h as i32 > 2 * m {
        rect.h as i32 - 2 * m
    } else {
        0
    } as u32;
    UiRect::new(rect.x + m, rect.y + m, w, h)
}

// ---------------------------------------------------------------------------
// Software RGBA rasterizer
// ---------------------------------------------------------------------------

/// Src-over blend of `color` onto the 4-byte pixel at `dst`.
fn blend_px(dst: &mut [u8], color: UiColor) {
    let sa = u32::from(color.a);
    if sa == 0 {
        return;
    }
    if sa == 255 {
        dst.copy_from_slice(&[color.r, color.g, color.b, 255]);
        return;
    }
    let inv = 255 - sa;
    dst[0] = ((u32::from(color.r) * sa + u32::from(dst[0]) * inv + 127) / 255) as u8;
    dst[1] = ((u32::from(color.g) * sa + u32::from(dst[1]) * inv + 127) / 255) as u8;
    dst[2] = ((u32::from(color.b) * sa + u32::from(dst[2]) * inv + 127) / 255) as u8;
    dst[3] = (sa + u32::from(dst[3]) * inv / 255) as u8;
}

struct Frame<'a> {
    w: i32,
    h: i32,
    px: &'a mut [u8],
}

impl Frame<'_> {
    fn fill(&mut self, rect: UiRect, color: UiColor) {
        if color.a == 0 {
            return;
        }
        let x0 = rect.x.max(0);
        let y0 = rect.y.max(0);
        let x1 = rect.right().min(self.w);
        let y1 = rect.bottom().min(self.h);
        if x1 <= x0 || y1 <= y0 {
            return;
        }
        for y in y0..y1 {
            for x in x0..x1 {
                let o = ((y * self.w + x) * 4) as usize;
                blend_px(&mut self.px[o..o + 4], color);
            }
        }
    }

    fn border(&mut self, rect: UiRect, color: UiColor, thickness: u32) {
        let t = thickness.min(rect.w.min(rect.h));
        if t == 0 {
            return;
        }
        self.fill(UiRect::new(rect.x, rect.y, rect.w, t), color);
        self.fill(
            UiRect::new(rect.x, rect.bottom() - t as i32, rect.w, t),
            color,
        );
        let inner_h = rect.h.saturating_sub(2 * t);
        self.fill(
            UiRect::new(rect.x, rect.y + t as i32, t, inner_h),
            color,
        );
        self.fill(
            UiRect::new(rect.right() - t as i32, rect.y + t as i32, t, inner_h),
            color,
        );
    }

    fn glyph_cell(&mut self, ch: char, x: i32, y: i32, scale: u32, color: UiColor) {
        let bitmap = glyph_bitmap(ch);
        let wide = bitmap.cells >= 2;
        for (tile_idx, tile) in bitmap.tiles.iter().enumerate() {
            if tile_idx >= bitmap.cells as usize {
                break;
            }
            for (row, bits) in tile.iter().enumerate() {
                for col in 0..8u32 {
                    if (bits & (0x80 >> col)) == 0 {
                        continue;
                    }
                    let src_x = tile_idx as i32 * 8 + col as i32;
                    // Full-width art squeezes 16 -> FULLWIDTH_PX columns:
                    // the floor map merges adjacent source columns so no
                    // stroke ever disappears (the GPU path samples both
                    // tiles into the same squeezed quad pair).
                    let dst_x = if wide {
                        src_x * FULLWIDTH_PX as i32 / 16
                    } else {
                        src_x
                    };
                    self.fill(
                        UiRect::new(
                            x + dst_x * scale as i32,
                            y + (row as i32) * scale as i32,
                            scale,
                            scale,
                        ),
                        color,
                    );
                }
            }
        }
    }

    fn text(&mut self, x: i32, y: i32, scale: u32, color: UiColor, text: &str) {
        let cell = (GLYPH_SIZE * scale) as i32;
        let (mut cx, mut cy) = (x, y);
        for ch in text.chars() {
            if ch == '\n' {
                cx = x;
                cy += cell;
                continue;
            }
            if ch != ' ' && ch != '\u{3000}' {
                self.glyph_cell(ch, cx, cy, scale, color);
            }
            cx += glyph_advance(ch) as i32 * scale as i32;
        }
    }
}

/// Paint `list` into a fresh `width`×`height` RGBA8 buffer (transparent
/// black start, commands applied in order with src-over blending).
pub fn rasterize_draw_list(width: u32, height: u32, list: &UiDrawList) -> Vec<u8> {
    let mut px = vec![0u8; width as usize * height as usize * 4];
    {
        let mut frame = Frame {
            w: width as i32,
            h: height as i32,
            px: &mut px,
        };
        for cmd in &list.commands {
            match cmd {
                UiCommand::FillRect { rect, color } => frame.fill(*rect, *color),
                UiCommand::BorderRect {
                    rect,
                    color,
                    thickness,
                } => frame.border(*rect, *color, *thickness),
                UiCommand::Text {
                    x,
                    y,
                    scale,
                    color,
                    text,
                } => frame.text(*x, *y, *scale, *color, text),
                UiCommand::ProgressBar {
                    rect,
                    fraction,
                    fill,
                    back,
                    border,
                } => {
                    frame.border(*rect, *border, 1);
                    let inner = inset(*rect, 1);
                    frame.fill(inner, *back);
                    let fw = (inner.w as f32 * fraction.clamp(0.0, 1.0)) as u32;
                    if fw > 0 {
                        frame.fill(UiRect::new(inner.x, inner.y, fw, inner.h), *fill);
                    }
                }
                UiCommand::ItemIcon { rect, item_id } => {
                    // Software path has no atlas: deterministic swatch +
                    // monogram, mirroring `icon_fallback_quads`.
                    let (color, letter) = icon_fallback(item_id);
                    frame.fill(*rect, color);
                    if letter != ' ' {
                        let scale = (rect.w.min(rect.h) / GLYPH_SIZE).max(1);
                        let cell = (GLYPH_SIZE * scale) as i32;
                        let cx = rect.x + ((rect.w as i32 - cell) / 2).max(0);
                        let cy = rect.y + ((rect.h as i32 - cell) / 2).max(0);
                        frame.text(cx, cy, scale, UiColor::rgb(30, 30, 34), &letter.to_string());
                    }
                }
            }
        }
    }
    px
}

// ---------------------------------------------------------------------------
// PNG encoding (RFC 2083, 8-bit RGBA, zlib stored blocks via RFC 1950)
// ---------------------------------------------------------------------------

fn png_chunk(tag: &[u8; 4], data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(12 + data.len());
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    out.extend_from_slice(tag);
    out.extend_from_slice(data);
    let mut crc_input = Vec::with_capacity(4 + data.len());
    crc_input.extend_from_slice(tag);
    crc_input.extend_from_slice(data);
    out.extend_from_slice(&megu_core::hash::crc32_ieee(&crc_input).to_be_bytes());
    out
}

/// Encode tightly packed RGBA8 pixels as a PNG (color type 6, no
/// interlace). `rgba.len()` must be `width*height*4`; a mismatched buffer
/// yields an empty `Vec` rather than a corrupt file.
pub fn encode_png_rgba(width: u32, height: u32, rgba: &[u8]) -> Vec<u8> {
    let stride = width as usize * 4;
    if rgba.len() != stride * height as usize {
        return Vec::new();
    }
    let mut raw = Vec::with_capacity((stride + 1) * height as usize);
    for row in rgba.chunks_exact(stride) {
        raw.push(0); // filter type 0: None
        raw.extend_from_slice(row);
    }
    let idat = megu_core::zip_archive::zlib_compress_stored(&raw);

    let mut out = Vec::with_capacity(8 + 25 + 12 + idat.len() + 12);
    out.extend_from_slice(b"\x89PNG\r\n\x1a\n");
    let mut ihdr = Vec::with_capacity(13);
    ihdr.extend_from_slice(&width.to_be_bytes());
    ihdr.extend_from_slice(&height.to_be_bytes());
    ihdr.extend_from_slice(&[8, 6, 0, 0, 0]); // 8-bit, truecolor + alpha
    out.extend_from_slice(&png_chunk(b"IHDR", &ihdr));
    out.extend_from_slice(&png_chunk(b"IDAT", &idat));
    out.extend_from_slice(&png_chunk(b"IEND", &[]));
    out
}
