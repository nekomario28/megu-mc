// SPDX-License-Identifier: LGPL-3.0-or-later
#![deny(unsafe_code)]
//! Client Tier-2 2D GUI bridge ABI: the pure-Rust `GuiGraphics -> DrawCmd`
//! contract client HUD/tooltip/screen mods draw through.
//!
//! A client mod never touches the GPU. It records one [`GuiBridgeBatch`] of
//! [`GuiBridgeDrawOp`]s per frame, the host serializes it with
//! [`encode_batch`] / validates it with [`decode_batch`] (a fail-closed
//! `GGB1` wire form suitable for crossing process/sandbox boundaries), and
//! [`lower_bridge_batch_to_draw_list`] converts the accepted batch
//! into `client-ui`'s native 2D draw list ([`DrawCmd`] =
//! [`crate::draw::UiCommand`]).
//!
//! Safety and fair-play properties, enforced in one place:
//!
//! * **Fail-closed codec** — `GGB1` magic, wire version `1`, at most
//!   [`MAX_BRIDGE_OPS`] ops per frame, strings capped at
//!   [`MAX_BRIDGE_STRING`] bytes of valid UTF-8, finite atlas UVs, and a
//!   balanced `PushScissor`/`PopScissor` discipline. Anything malformed is
//!   rejected wholesale; a bad batch never draws a partial frame.
//! * **Fair-Play slot gate** — [`GuiBridgeSlotAction`]s (container clicks
//!   a mod asks the host to perform) pass through
//!   [`GuiBridgeSlotActionGate`], which admits at most
//!   [`MAX_SLOT_ACTIONS_PER_TICK`] actions per game tick so a mod can never
//!   flood the server with click packets and trip its anti-cheat.
//! * **Read-only world view** — [`GuiBridgeHudSnapshot`] is the only game
//!   state a HUD/tooltip mod sees: a per-frame immutable copy, never a
//!   mutable borrow into the session.
//!
//! Everything here is pure `std` data and logic — no handles, no I/O, no
//! clocks — so both sides of the ABI are diffable in tests.

use std::collections::VecDeque;
use std::fmt;

use crate::draw::{palette, text_width, GuiSpriteUv, UiColor, UiCommand, UiDrawList, UiRect};

/// The native 2D draw-command type the bridge lowers into.
///
/// `client-ui`'s GPU/tessellation path already consumes
/// [`crate::draw::UiCommand`]; this alias names the same type the way the
/// ABI contract does (`GuiGraphics -> DrawCmd`) so mod-facing docs and
/// signatures read naturally.
pub type DrawCmd = UiCommand;

// ---------------------------------------------------------------------------
// Draw ops
// ---------------------------------------------------------------------------

/// One recorded 2D operation a client mod may emit per frame.
///
/// Coordinates are physical pixels, origins top-left. `rgba` colors are
/// straight (non-premultiplied) 8-bit RGBA, matching
/// [`crate::draw::UiColor`].
#[derive(Clone, Debug, PartialEq)]
pub enum GuiBridgeDrawOp {
    /// Filled rectangle.
    FillRect {
        x: i32,
        y: i32,
        w: u32,
        h: u32,
        rgba: [u8; 4],
    },
    /// Monospace bitmap text; `scale` multiplies the 8×8 glyph cell
    /// (values below `1` lower to `1`).
    DrawText {
        x: i32,
        y: i32,
        scale: u8,
        rgba: [u8; 4],
        text: String,
    },
    /// Textured quad from a named atlas texture. `(u0, v0)..(u1, v1)` are
    /// normalized texture coordinates; `tint` multiplies the sample.
    ///
    /// The clipping-correct lowering path emits this as a native atlas UV
    /// quad. The legacy command-vector compatibility wrapper still uses the
    /// quad's `tint`-colored footprint because a bare `Vec<DrawCmd>` cannot
    /// carry per-command scissor metadata.
    BlitAtlas {
        x: i32,
        y: i32,
        w: u32,
        h: u32,
        texture_id: String,
        u0: f32,
        v0: f32,
        u1: f32,
        v1: f32,
        tint: [u8; 4],
    },
    /// A vanilla 16×16 item icon plus the vanilla-style stack-count badge
    /// (badge drawn only when `count > 1`). Item pixels resolve through
    /// the host's item atlas; the bridge emits the icon footprint and the
    /// count text so layout and hit-testing stay deterministic.
    DrawItemIcon {
        x: i32,
        y: i32,
        item_id: String,
        count: u8,
    },
    /// Constrain every following op to the intersection of this rect with
    /// the current clip (and the viewport) until the matching
    /// [`PopScissor`](Self::PopScissor). Pushes nest; every push must be
    /// popped before the frame ends.
    PushScissor { x: i32, y: i32, w: u32, h: u32 },
    /// Restore the clip to the enclosing scissor (or the viewport).
    PopScissor,
}

/// Pixel size of one item-icon footprint (vanilla convention).
pub const ITEM_ICON_PX: u32 = 16;

// ---------------------------------------------------------------------------
// Read-only HUD snapshot exposed to mods
// ---------------------------------------------------------------------------

/// Immutable per-frame player/view state handed to HUD and tooltip mods.
///
/// The host fills this once per frame; mods read it, draw from it, and can
/// never mutate the session through it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct GuiBridgeHudSnapshot {
    /// Current player health (hearts × 2).
    pub hp: f32,
    /// Maximum player health.
    pub max_hp: f32,
    /// Food level `0..=20`.
    pub food: u8,
    /// Hidden saturation level (what AppleSkin-style mods visualize).
    pub saturation: f32,
    /// Armor points `0..=20` (plus toughness folded in by the host).
    pub armor: u8,
    /// Remaining air in ticks (`300` when out of water; may go negative
    /// while drowning damage ticks).
    pub air: i32,
    /// Player feet position `(x, y, z)` in world space.
    pub pos: [f64; 3],
    /// Biome identifier at the player position (`"minecraft:plains"` …).
    pub biome_id: String,
    /// Namespaced id of the block under the crosshair, if any.
    pub targeted_block_id: Option<String>,
    /// Stable id of the entity under the crosshair, if any.
    pub targeted_entity_id: Option<u64>,
    /// Namespaced id of the held item, if any.
    pub held_item_id: Option<String>,
    /// Physical framebuffer size the frame is drawn into.
    pub screen_w: u32,
    pub screen_h: u32,
}

// ---------------------------------------------------------------------------
// Screen events and fair-play container actions
// ---------------------------------------------------------------------------

/// Input the host forwards into an open mod screen.
#[derive(Clone, Debug, PartialEq)]
pub enum GuiBridgeScreenEvent {
    /// Mouse button pressed/released at `(x, y)`.
    MouseClick { x: i32, y: i32, button: u8 },
    /// Scroll-wheel step at `(x, y)` (`delta` in lines, sign = direction).
    MouseScroll { x: i32, y: i32, delta: i32 },
    /// Drag motion while `button` is held: current position plus delta.
    MouseDrag {
        x: i32,
        y: i32,
        dx: i32,
        dy: i32,
        button: u8,
    },
    /// A key press: platform-neutral `key` code plus modifier bits.
    KeyPress {
        key: u32,
        shift: bool,
        ctrl: bool,
        alt: bool,
    },
}

/// A container interaction a mod asks the host to perform on its behalf.
///
/// These are *requests*, not direct packet sends: the host passes them
/// through [`GuiBridgeSlotActionGate`] and issues them at human-plausible
/// pacing, so a misbehaving or compromised mod cannot turn into a
/// click-flood.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GuiBridgeSlotAction {
    /// Left/right-style click on a container slot.
    ClickSlot { slot: u16, button: u8 },
    /// Shift-click quick move on a slot.
    QuickMove { slot: u16 },
    /// Swap a container slot with a hotbar slot (`0..=8`).
    SwapHotbar { slot: u16, hotbar: u8 },
}

/// Hard cap on slot actions a mod may submit per game tick. Vanilla
/// inventory interactions never exceed a handful of clicks per tick; any
/// more is anti-cheat bait on strict servers, so the gate drops them.
pub const MAX_SLOT_ACTIONS_PER_TICK: usize = 4;

/// Per-tick rate limiter for [`GuiBridgeSlotAction`]s.
///
/// The host calls [`begin_tick`](Self::begin_tick) once per game tick,
/// then [`try_submit`](Self::try_submit) for every action a mod emits.
/// Actions beyond [`MAX_SLOT_ACTIONS_PER_TICK`] within one tick are
/// refused and must be retried next tick — the gate never bursts.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GuiBridgeSlotActionGate {
    used_this_tick: usize,
    /// Refused actions kept (FIFO, bounded) so the host can flush them on
    /// the next tick instead of silently dropping the mod's request.
    pending: VecDeque<GuiBridgeSlotAction>,
}

impl GuiBridgeSlotActionGate {
    pub fn new() -> Self {
        Self::default()
    }

    /// Reset the per-tick budget. Returns the queued actions admitted
    /// this tick (up to the cap), in submit order.
    pub fn begin_tick(&mut self) -> Vec<GuiBridgeSlotAction> {
        self.used_this_tick = 0;
        let mut out = Vec::new();
        while self.used_this_tick < MAX_SLOT_ACTIONS_PER_TICK {
            let Some(action) = self.pending.pop_front() else {
                break;
            };
            self.used_this_tick += 1;
            out.push(action);
        }
        out
    }

    /// Remaining action budget this tick.
    pub fn remaining(&self) -> usize {
        MAX_SLOT_ACTIONS_PER_TICK - self.used_this_tick
    }

    /// Actions queued behind the fair-play cap.
    pub fn pending_len(&self) -> usize {
        self.pending.len()
    }

    /// Admit `action` within this tick's budget, or queue it for the next
    /// `begin_tick`. Returns `true` when the action may execute now.
    ///
    /// The queue is bounded at [`MAX_BRIDGE_OPS`] entries — a mod asking
    /// for thousands of clicks per tick is refused outright (`false`,
    /// nothing queued) rather than allowed to grow memory without limit.
    pub fn try_submit(&mut self, action: GuiBridgeSlotAction) -> bool {
        if self.used_this_tick < MAX_SLOT_ACTIONS_PER_TICK {
            self.used_this_tick += 1;
            return true;
        }
        if self.pending.len() < MAX_BRIDGE_OPS {
            self.pending.push_back(action);
        }
        false
    }
}

// ---------------------------------------------------------------------------
// Batch + fail-closed GGB1 wire codec
// ---------------------------------------------------------------------------

/// One frame of recorded draw ops exchanged with a client mod.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct GuiBridgeBatch {
    pub ops: Vec<GuiBridgeDrawOp>,
}

impl GuiBridgeBatch {
    pub fn new(ops: Vec<GuiBridgeDrawOp>) -> Self {
        Self { ops }
    }

    /// `true` when the batch would survive [`encode_batch`] — right op
    /// count, bounded strings, finite UVs, balanced scissors.
    pub fn is_valid(&self) -> bool {
        validate_batch(&self.ops).is_ok()
    }
}

/// Wire magic: `"GGB1"`.
pub const GGB1_MAGIC: [u8; 4] = *b"GGB1";
/// Only accepted wire format version.
pub const GGB1_VERSION: u8 = 1;
/// Maximum ops a single frame batch may carry.
pub const MAX_BRIDGE_OPS: usize = 2048;
/// Maximum byte length of any string field (`text`, `texture_id`,
/// `item_id`).
pub const MAX_BRIDGE_STRING: usize = 512;

/// Every way a batch or its wire form can be rejected. Decoding is
/// fail-closed: the first violation discards the entire frame.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GuiBridgeCodecError {
    /// Leading bytes were not `"GGB1"`.
    BadMagic,
    /// Header version byte is not [`GGB1_VERSION`].
    UnsupportedVersion(u8),
    /// Op count exceeds [`MAX_BRIDGE_OPS`].
    TooManyOps(usize),
    /// Input ended inside a header, op, or string.
    Truncated,
    /// A length-prefixed string exceeded [`MAX_BRIDGE_STRING`] bytes.
    StringTooLong(usize),
    /// A string field was not valid UTF-8.
    InvalidUtf8,
    /// Op tag byte matched no known op.
    UnknownOpTag(u8),
    /// `BlitAtlas` UVs contained a NaN or infinity.
    NonFiniteTexcoord,
    /// `PopScissor` arrived with nothing on the stack.
    ScissorUnderflow,
    /// Frame ended with `PushScissor`s still open.
    UnbalancedScissors,
    /// Bytes remained after the declared op count (frame smuggling).
    TrailingBytes(usize),
}

impl fmt::Display for GuiBridgeCodecError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BadMagic => write!(f, "bad magic (expected GGB1)"),
            Self::UnsupportedVersion(v) => write!(f, "unsupported wire version {v}"),
            Self::TooManyOps(n) => write!(f, "op count {n} exceeds {MAX_BRIDGE_OPS}"),
            Self::Truncated => write!(f, "truncated batch"),
            Self::StringTooLong(n) => write!(f, "string of {n} bytes exceeds {MAX_BRIDGE_STRING}"),
            Self::InvalidUtf8 => write!(f, "string field is not UTF-8"),
            Self::UnknownOpTag(tag) => write!(f, "unknown op tag 0x{tag:02x}"),
            Self::NonFiniteTexcoord => write!(f, "non-finite atlas texcoord"),
            Self::ScissorUnderflow => write!(f, "PopScissor on an empty scissor stack"),
            Self::UnbalancedScissors => write!(f, "frame ended with open scissors"),
            Self::TrailingBytes(n) => write!(f, "{n} trailing bytes after last op"),
        }
    }
}

impl std::error::Error for GuiBridgeCodecError {}

// op tags (stable wire values — append only)
const TAG_FILL_RECT: u8 = 0x01;
const TAG_DRAW_TEXT: u8 = 0x02;
const TAG_BLIT_ATLAS: u8 = 0x03;
const TAG_DRAW_ITEM_ICON: u8 = 0x04;
const TAG_PUSH_SCISSOR: u8 = 0x05;
const TAG_POP_SCISSOR: u8 = 0x06;

fn push_i32(out: &mut Vec<u8>, v: i32) {
    out.extend_from_slice(&v.to_le_bytes());
}

fn push_u32(out: &mut Vec<u8>, v: u32) {
    out.extend_from_slice(&v.to_le_bytes());
}

fn push_u16(out: &mut Vec<u8>, v: u16) {
    out.extend_from_slice(&v.to_le_bytes());
}

fn push_f32(out: &mut Vec<u8>, v: f32) {
    out.extend_from_slice(&v.to_le_bytes());
}

fn push_str(out: &mut Vec<u8>, s: &str) {
    push_u16(out, s.len() as u16);
    out.extend_from_slice(s.as_bytes());
}

/// `Err` when any structural invariant of the batch is violated; used by
/// the encoder so only decodable batches can be produced.
fn validate_batch(ops: &[GuiBridgeDrawOp]) -> Result<(), GuiBridgeCodecError> {
    if ops.len() > MAX_BRIDGE_OPS {
        return Err(GuiBridgeCodecError::TooManyOps(ops.len()));
    }
    let mut depth = 0usize;
    for op in ops {
        match op {
            GuiBridgeDrawOp::FillRect { .. } => {}
            GuiBridgeDrawOp::DrawText { text, .. } => {
                if text.len() > MAX_BRIDGE_STRING {
                    return Err(GuiBridgeCodecError::StringTooLong(text.len()));
                }
            }
            GuiBridgeDrawOp::BlitAtlas {
                texture_id,
                u0,
                v0,
                u1,
                v1,
                ..
            } => {
                if texture_id.len() > MAX_BRIDGE_STRING {
                    return Err(GuiBridgeCodecError::StringTooLong(texture_id.len()));
                }
                if !(u0.is_finite() && v0.is_finite() && u1.is_finite() && v1.is_finite()) {
                    return Err(GuiBridgeCodecError::NonFiniteTexcoord);
                }
            }
            GuiBridgeDrawOp::DrawItemIcon { item_id, .. } => {
                if item_id.len() > MAX_BRIDGE_STRING {
                    return Err(GuiBridgeCodecError::StringTooLong(item_id.len()));
                }
            }
            GuiBridgeDrawOp::PushScissor { .. } => depth += 1,
            GuiBridgeDrawOp::PopScissor => {
                depth = depth
                    .checked_sub(1)
                    .ok_or(GuiBridgeCodecError::ScissorUnderflow)?;
            }
        }
    }
    if depth != 0 {
        return Err(GuiBridgeCodecError::UnbalancedScissors);
    }
    Ok(())
}

/// Serialize `batch` into its `GGB1` wire form. Refuses to emit a frame
/// the decoder would reject — encode and decode share the same
/// validation, so the pair is always a perfect round trip.
pub fn encode_batch(batch: &GuiBridgeBatch) -> Result<Vec<u8>, GuiBridgeCodecError> {
    validate_batch(&batch.ops)?;
    let mut out = Vec::with_capacity(8 + batch.ops.len() * 24);
    out.extend_from_slice(&GGB1_MAGIC);
    out.push(GGB1_VERSION);
    out.push(0); // flags (reserved, must be 0)
    push_u16(&mut out, batch.ops.len() as u16);
    for op in &batch.ops {
        match op {
            GuiBridgeDrawOp::FillRect { x, y, w, h, rgba } => {
                out.push(TAG_FILL_RECT);
                push_i32(&mut out, *x);
                push_i32(&mut out, *y);
                push_u32(&mut out, *w);
                push_u32(&mut out, *h);
                out.extend_from_slice(rgba);
            }
            GuiBridgeDrawOp::DrawText {
                x,
                y,
                scale,
                rgba,
                text,
            } => {
                out.push(TAG_DRAW_TEXT);
                push_i32(&mut out, *x);
                push_i32(&mut out, *y);
                out.push(*scale);
                out.extend_from_slice(rgba);
                push_str(&mut out, text);
            }
            GuiBridgeDrawOp::BlitAtlas {
                x,
                y,
                w,
                h,
                texture_id,
                u0,
                v0,
                u1,
                v1,
                tint,
            } => {
                out.push(TAG_BLIT_ATLAS);
                push_i32(&mut out, *x);
                push_i32(&mut out, *y);
                push_u32(&mut out, *w);
                push_u32(&mut out, *h);
                push_str(&mut out, texture_id);
                push_f32(&mut out, *u0);
                push_f32(&mut out, *v0);
                push_f32(&mut out, *u1);
                push_f32(&mut out, *v1);
                out.extend_from_slice(tint);
            }
            GuiBridgeDrawOp::DrawItemIcon { x, y, item_id, count } => {
                out.push(TAG_DRAW_ITEM_ICON);
                push_i32(&mut out, *x);
                push_i32(&mut out, *y);
                push_str(&mut out, item_id);
                out.push(*count);
            }
            GuiBridgeDrawOp::PushScissor { x, y, w, h } => {
                out.push(TAG_PUSH_SCISSOR);
                push_i32(&mut out, *x);
                push_i32(&mut out, *y);
                push_u32(&mut out, *w);
                push_u32(&mut out, *h);
            }
            GuiBridgeDrawOp::PopScissor => out.push(TAG_POP_SCISSOR),
        }
    }
    Ok(out)
}

/// Bounds-checked little-endian cursor over a wire frame.
struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    fn remaining(&self) -> usize {
        self.buf.len() - self.pos
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], GuiBridgeCodecError> {
        if self.remaining() < n {
            return Err(GuiBridgeCodecError::Truncated);
        }
        let s = &self.buf[self.pos..self.pos + n];
        self.pos += n;
        Ok(s)
    }

    fn u8(&mut self) -> Result<u8, GuiBridgeCodecError> {
        Ok(self.take(1)?[0])
    }

    fn u16(&mut self) -> Result<u16, GuiBridgeCodecError> {
        Ok(u16::from_le_bytes(self.take(2)?.try_into().expect("2 bytes")))
    }

    fn u32(&mut self) -> Result<u32, GuiBridgeCodecError> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().expect("4 bytes")))
    }

    fn i32(&mut self) -> Result<i32, GuiBridgeCodecError> {
        Ok(i32::from_le_bytes(self.take(4)?.try_into().expect("4 bytes")))
    }

    fn f32(&mut self) -> Result<f32, GuiBridgeCodecError> {
        Ok(f32::from_le_bytes(self.take(4)?.try_into().expect("4 bytes")))
    }

    fn rgba(&mut self) -> Result<[u8; 4], GuiBridgeCodecError> {
        Ok(self.take(4)?.try_into().expect("4 bytes"))
    }

    /// Length-prefixed UTF-8 string, bounded to [`MAX_BRIDGE_STRING`].
    fn string(&mut self) -> Result<String, GuiBridgeCodecError> {
        let len = usize::from(self.u16()?);
        if len > MAX_BRIDGE_STRING {
            return Err(GuiBridgeCodecError::StringTooLong(len));
        }
        let bytes = self.take(len)?;
        String::from_utf8(bytes.to_vec()).map_err(|_| GuiBridgeCodecError::InvalidUtf8)
    }
}

/// Decode a `GGB1` frame into a batch. Any structural violation —
/// wrong magic, unknown version, overrun, overlong string, non-UTF-8
/// text, non-finite UVs, scissor imbalance, or trailing bytes — rejects
/// the whole frame.
pub fn decode_batch(bytes: &[u8]) -> Result<GuiBridgeBatch, GuiBridgeCodecError> {
    let mut r = Reader::new(bytes);
    if r.take(4)? != GGB1_MAGIC {
        return Err(GuiBridgeCodecError::BadMagic);
    }
    let version = r.u8()?;
    if version != GGB1_VERSION {
        return Err(GuiBridgeCodecError::UnsupportedVersion(version));
    }
    let _reserved_flags = r.u8()?;
    let op_count = usize::from(r.u16()?);
    if op_count > MAX_BRIDGE_OPS {
        return Err(GuiBridgeCodecError::TooManyOps(op_count));
    }
    let mut ops = Vec::with_capacity(op_count.min(256));
    for _ in 0..op_count {
        let tag = r.u8()?;
        ops.push(match tag {
            TAG_FILL_RECT => GuiBridgeDrawOp::FillRect {
                x: r.i32()?,
                y: r.i32()?,
                w: r.u32()?,
                h: r.u32()?,
                rgba: r.rgba()?,
            },
            TAG_DRAW_TEXT => GuiBridgeDrawOp::DrawText {
                x: r.i32()?,
                y: r.i32()?,
                scale: r.u8()?,
                rgba: r.rgba()?,
                text: r.string()?,
            },
            TAG_BLIT_ATLAS => {
                let (x, y, w, h) = (r.i32()?, r.i32()?, r.u32()?, r.u32()?);
                let texture_id = r.string()?;
                let (u0, v0, u1, v1) = (r.f32()?, r.f32()?, r.f32()?, r.f32()?);
                let tint = r.rgba()?;
                GuiBridgeDrawOp::BlitAtlas {
                    x,
                    y,
                    w,
                    h,
                    texture_id,
                    u0,
                    v0,
                    u1,
                    v1,
                    tint,
                }
            }
            TAG_DRAW_ITEM_ICON => GuiBridgeDrawOp::DrawItemIcon {
                x: r.i32()?,
                y: r.i32()?,
                item_id: r.string()?,
                count: r.u8()?,
            },
            TAG_PUSH_SCISSOR => GuiBridgeDrawOp::PushScissor {
                x: r.i32()?,
                y: r.i32()?,
                w: r.u32()?,
                h: r.u32()?,
            },
            TAG_POP_SCISSOR => GuiBridgeDrawOp::PopScissor,
            other => return Err(GuiBridgeCodecError::UnknownOpTag(other)),
        });
    }
    if r.remaining() != 0 {
        return Err(GuiBridgeCodecError::TrailingBytes(r.remaining()));
    }
    // Balance/limits already hold on the encode path; re-check so a
    // hand-crafted frame meets the same invariant.
    validate_batch(&ops)?;
    Ok(GuiBridgeBatch { ops })
}

// ---------------------------------------------------------------------------
// Lowering: bridge ops -> native 2D draw list
// ---------------------------------------------------------------------------

fn color_of(rgba: [u8; 4]) -> UiColor {
    UiColor::rgba(rgba[0], rgba[1], rgba[2], rgba[3])
}

fn effective_bridge_clip(viewport: UiRect, scissors: &[UiRect]) -> Option<UiRect> {
    let mut clip = viewport;
    for scissor in scissors {
        clip = clip.intersection(*scissor)?;
    }
    Some(clip)
}

fn push_bridge_scissor(scissors: &mut Vec<UiRect>, viewport: UiRect, rect: UiRect) {
    // A child scissor can only ever shrink the clip. A disjoint child is a
    // real empty scissor, not an absence of clipping, so retain a zero-sized
    // sentinel until the matching pop.
    let nested = effective_bridge_clip(viewport, scissors)
        .and_then(|clip| clip.intersection(rect))
        .unwrap_or(UiRect::new(0, 0, 0, 0));
    scissors.push(nested);
}

/// Convert an accepted [`GuiBridgeBatch`] into a native [`UiDrawList`] with
/// per-command scissor metadata preserved.
///
/// This is the clipping-correct lowering path: `FillRect`, text runs,
/// textured atlas quads, and item-icon footprints all keep their original
/// geometry while the draw list records the effective intersection of the
/// viewport and every nested bridge scissor. Tessellation and the software
/// rasterizer can therefore clip partial rectangles, crop textured UVs in
/// proportion to the discarded edge pixels, and cut individual glyphs at
/// the scissor boundary instead of applying the legacy origin-only text
/// test.
///
/// `BlitAtlas` lowers to the native GUI-atlas UV quad encoding, including
/// its color-multiplying `tint`. `texture_id` names the source atlas for
/// bridge validation/diagnostics; the current native renderer has one GUI
/// atlas binding, so the normalized UV sample is the renderable payload.
///
/// `PushScissor`/`PopScissor` emit no commands. Defensive extra pops are
/// ignored; malformed wire batches are already rejected by the codec.
pub fn lower_bridge_batch_to_draw_list(
    batch: &GuiBridgeBatch,
    viewport_w: u32,
    viewport_h: u32,
) -> UiDrawList {
    let viewport = UiRect::new(0, 0, viewport_w, viewport_h);
    let mut scissors: Vec<UiRect> = Vec::new();

    let mut out = UiDrawList::new();
    for op in &batch.ops {
        match op {
            GuiBridgeDrawOp::FillRect { x, y, w, h, rgba } => {
                if let Some(clip) = effective_bridge_clip(viewport, &scissors) {
                    out.push_with_clip(
                        UiCommand::FillRect {
                            rect: UiRect::new(*x, *y, *w, *h),
                            color: color_of(*rgba),
                        },
                        Some(clip),
                    );
                }
            }
            GuiBridgeDrawOp::DrawText {
                x,
                y,
                scale,
                rgba,
                text,
            } => {
                if let Some(clip) = effective_bridge_clip(viewport, &scissors) {
                    out.push_with_clip(
                        UiCommand::Text {
                            x: *x,
                            y: *y,
                            scale: u32::from(*scale).max(1),
                            color: color_of(*rgba),
                            text: text.clone(),
                        },
                        Some(clip),
                    );
                }
            }
            GuiBridgeDrawOp::BlitAtlas {
                x,
                y,
                w,
                h,
                u0,
                v0,
                u1,
                v1,
                tint,
                ..
            } => {
                if let Some(clip) = effective_bridge_clip(viewport, &scissors) {
                    let rect = UiRect::new(*x, *y, *w, *h);
                    let ordered_unit_uv = [*u0, *v0, *u1, *v1]
                        .iter()
                        .all(|v| (0.0..=1.0).contains(v))
                        && u0 < u1
                        && v0 < v1;
                    if ordered_unit_uv {
                        let uv = GuiSpriteUv {
                            uv_min: [*u0, *v0],
                            uv_max: [*u1, *v1],
                            width: *w,
                            height: *h,
                            nine_slice_border: [0; 4],
                        };
                        out.push_with_clip(
                            UiDrawList::texture_uv_tinted_command(rect, uv, color_of(*tint)),
                            Some(clip),
                        );
                    } else {
                        // Reversed/tiling UVs cannot round-trip through the
                        // compact GUI-atlas encoding. Keep the legacy visible
                        // footprint rather than falling through to the item-icon
                        // fallback swatch for an otherwise valid bridge quad.
                        out.push_with_clip(
                            UiCommand::FillRect {
                                rect,
                                color: color_of(*tint),
                            },
                            Some(clip),
                        );
                    }
                }
            }
            GuiBridgeDrawOp::DrawItemIcon {
                x,
                y,
                item_id,
                count,
            } => {
                if let Some(clip) = effective_bridge_clip(viewport, &scissors) {
                    out.push_with_clip(
                        UiCommand::ItemIcon {
                            rect: UiRect::new(*x, *y, ITEM_ICON_PX, ITEM_ICON_PX),
                            item_id: item_id.clone(),
                        },
                        Some(clip),
                    );
                    if *count > 1 {
                        let label = count.to_string();
                        let tx = *x + ITEM_ICON_PX as i32 + 1 - text_width(&label, 1) as i32;
                        let ty = *y + ITEM_ICON_PX as i32 - 8;
                        out.push_with_clip(
                            UiCommand::Text {
                                x: tx,
                                y: ty,
                                scale: 1,
                                color: palette::TEXT,
                                text: label,
                            },
                            Some(clip),
                        );
                    }
                }
            }
            GuiBridgeDrawOp::PushScissor { x, y, w, h } => {
                push_bridge_scissor(&mut scissors, viewport, UiRect::new(*x, *y, *w, *h));
            }
            GuiBridgeDrawOp::PopScissor => {
                let _ = scissors.pop();
            }
        }
    }
    out
}

/// Convert an accepted [`GuiBridgeBatch`] into `client-ui`'s native 2D
/// draw commands, clipped to the `viewport_w`×`viewport_h` framebuffer
/// and to the batch's own scissor stack:
///
/// * `FillRect` → `FillRect` intersected with the effective clip (dropped
///   when fully clipped away).
/// * `DrawText` → `Text`, emitted only while its origin cell sits inside
///   the effective clip. A bare command vector cannot carry per-command
///   scissor metadata, so this compatibility path keeps text whole or drops
///   it; use [`lower_bridge_batch_to_draw_list`] for partial glyph clipping.
/// * `BlitAtlas` → the quad footprint filled with `tint` (see the op's
///   docs — the native list carries no texture binding).
/// * `DrawItemIcon` → the 16×16 icon footprint (slot-toned fill) plus a
///   right-bottom-aligned stack count when `count > 1`.
/// * `PushScissor`/`PopScissor` → maintained as a clip stack; they emit
///   no commands. A defensive mismatch (more pops than pushes) is ignored
///   here — the codec is the enforcement layer.
///
/// The function is total: it never fails, because
/// [`decode_batch`]/[`encode_batch`] already rejected malformed frames;
/// lowering an invalid in-memory batch simply yields the clipped
/// intersection of whatever is drawable.
pub fn lower_bridge_batch_to_draw_cmds(
    batch: &GuiBridgeBatch,
    viewport_w: u32,
    viewport_h: u32,
) -> Vec<DrawCmd> {
    let viewport = UiRect::new(0, 0, viewport_w, viewport_h);
    let mut scissors: Vec<UiRect> = Vec::new();

    let mut out = Vec::with_capacity(batch.ops.len());
    for op in &batch.ops {
        match op {
            GuiBridgeDrawOp::FillRect { x, y, w, h, rgba } => {
                let rect = UiRect::new(*x, *y, *w, *h);
                if let Some(rect) = effective_bridge_clip(viewport, &scissors)
                    .and_then(|clip| clip.intersection(rect))
                {
                    out.push(UiCommand::FillRect {
                        rect,
                        color: color_of(*rgba),
                    });
                }
            }
            GuiBridgeDrawOp::DrawText {
                x,
                y,
                scale,
                rgba,
                text,
            } => {
                let inside = effective_bridge_clip(viewport, &scissors)
                    .is_some_and(|clip| clip.contains(*x, *y));
                if inside {
                    out.push(UiCommand::Text {
                        x: *x,
                        y: *y,
                        scale: u32::from(*scale).max(1),
                        color: color_of(*rgba),
                        text: text.clone(),
                    });
                }
            }
            GuiBridgeDrawOp::BlitAtlas {
                x, y, w, h, tint, ..
            } => {
                let rect = UiRect::new(*x, *y, *w, *h);
                if let Some(rect) = effective_bridge_clip(viewport, &scissors)
                    .and_then(|clip| clip.intersection(rect))
                {
                    out.push(UiCommand::FillRect {
                        rect,
                        color: color_of(*tint),
                    });
                }
            }
            GuiBridgeDrawOp::DrawItemIcon { x, y, count, .. } => {
                let footprint = UiRect::new(*x, *y, ITEM_ICON_PX, ITEM_ICON_PX);
                if let Some(rect) = effective_bridge_clip(viewport, &scissors)
                    .and_then(|clip| clip.intersection(footprint))
                {
                    out.push(UiCommand::FillRect {
                        rect,
                        color: palette::SLOT,
                    });
                }
                if *count > 1 {
                    let label = count.to_string();
                    let tx = *x + ITEM_ICON_PX as i32 + 1 - text_width(&label, 1) as i32;
                    let ty = *y + ITEM_ICON_PX as i32 - 8;
                    let inside = effective_bridge_clip(viewport, &scissors)
                        .is_some_and(|clip| clip.contains(tx, ty));
                    if inside {
                        out.push(UiCommand::Text {
                            x: tx,
                            y: ty,
                            scale: 1,
                            color: palette::TEXT,
                            text: label,
                        });
                    }
                }
            }
            GuiBridgeDrawOp::PushScissor { x, y, w, h } => {
                push_bridge_scissor(&mut scissors, viewport, UiRect::new(*x, *y, *w, *h));
            }
            GuiBridgeDrawOp::PopScissor => {
                let _ = scissors.pop();
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_batch() -> GuiBridgeBatch {
        GuiBridgeBatch::new(vec![
            GuiBridgeDrawOp::FillRect {
                x: 4,
                y: 6,
                w: 100,
                h: 40,
                rgba: [10, 20, 30, 200],
            },
            GuiBridgeDrawOp::PushScissor {
                x: 0,
                y: 0,
                w: 50,
                h: 50,
            },
            GuiBridgeDrawOp::DrawText {
                x: 8,
                y: 8,
                scale: 2,
                rgba: [255, 255, 255, 255],
                text: "hello".to_owned(),
            },
            GuiBridgeDrawOp::BlitAtlas {
                x: 10,
                y: 12,
                w: 16,
                h: 16,
                texture_id: "megu:widgets".to_owned(),
                u0: 0.0,
                v0: 0.25,
                u1: 0.5,
                v1: 1.0,
                tint: [255, 255, 255, 255],
            },
            GuiBridgeDrawOp::DrawItemIcon {
                x: 20,
                y: 20,
                item_id: "minecraft:diamond".to_owned(),
                count: 42,
            },
            GuiBridgeDrawOp::PopScissor,
        ])
    }

    #[test]
    fn ggb1_round_trip_preserves_every_op() {
        let batch = sample_batch();
        let wire = encode_batch(&batch).expect("encodable");
        assert_eq!(&wire[..4], b"GGB1");
        assert_eq!(wire[4], GGB1_VERSION);
        let back = decode_batch(&wire).expect("decodable");
        assert_eq!(back, batch);
        assert!(back.is_valid());
    }

    #[test]
    fn empty_batch_round_trips() {
        let batch = GuiBridgeBatch::default();
        let wire = encode_batch(&batch).unwrap();
        assert_eq!(wire.len(), 8);
        assert_eq!(decode_batch(&wire).unwrap(), batch);
    }

    #[test]
    fn decode_rejects_corrupt_headers() {
        assert_eq!(
            decode_batch(b""),
            Err(GuiBridgeCodecError::Truncated)
        );
        assert_eq!(
            decode_batch(b"XXXX\x01\x00\x00\x00"),
            Err(GuiBridgeCodecError::BadMagic)
        );
        assert_eq!(
            decode_batch(b"GGB1\x02\x00\x00\x00"),
            Err(GuiBridgeCodecError::UnsupportedVersion(2))
        );
        // Truncated inside the ops region.
        let wire = encode_batch(&sample_batch()).unwrap();
        let short = &wire[..wire.len() - 3];
        assert!(matches!(
            decode_batch(short),
            Err(GuiBridgeCodecError::Truncated)
        ));
        // Trailing garbage after the last op.
        let mut long = wire.clone();
        long.extend_from_slice(&[0, 0]);
        assert_eq!(
            decode_batch(&long),
            Err(GuiBridgeCodecError::TrailingBytes(2))
        );
    }

    #[test]
    fn decode_rejects_oversized_and_unknown_op() {
        // op_count header claims more than the cap.
        let mut wire = Vec::new();
        wire.extend_from_slice(&GGB1_MAGIC);
        wire.push(GGB1_VERSION);
        wire.push(0);
        wire.extend_from_slice(&(MAX_BRIDGE_OPS as u16 + 1).to_le_bytes());
        assert_eq!(
            decode_batch(&wire),
            Err(GuiBridgeCodecError::TooManyOps(MAX_BRIDGE_OPS + 1))
        );

        // Unknown op tag.
        let mut wire = Vec::new();
        wire.extend_from_slice(&GGB1_MAGIC);
        wire.push(GGB1_VERSION);
        wire.push(0);
        wire.extend_from_slice(&1u16.to_le_bytes());
        wire.push(0x7f);
        assert_eq!(
            decode_batch(&wire),
            Err(GuiBridgeCodecError::UnknownOpTag(0x7f))
        );

        // Length-prefixed string beyond the cap (DrawText).
        let mut wire = Vec::new();
        wire.extend_from_slice(&GGB1_MAGIC);
        wire.push(GGB1_VERSION);
        wire.push(0);
        wire.extend_from_slice(&1u16.to_le_bytes());
        wire.push(TAG_DRAW_TEXT);
        wire.extend_from_slice(&0i32.to_le_bytes());
        wire.extend_from_slice(&0i32.to_le_bytes());
        wire.push(1);
        wire.extend_from_slice(&[255, 255, 255, 255]);
        wire.extend_from_slice(&(MAX_BRIDGE_STRING as u16 + 1).to_le_bytes());
        assert!(matches!(
            decode_batch(&wire),
            Err(GuiBridgeCodecError::StringTooLong(n)) if n == MAX_BRIDGE_STRING + 1
        ));

        // Invalid UTF-8 in a text payload.
        let mut wire = Vec::new();
        wire.extend_from_slice(&GGB1_MAGIC);
        wire.push(GGB1_VERSION);
        wire.push(0);
        wire.extend_from_slice(&1u16.to_le_bytes());
        wire.push(TAG_DRAW_TEXT);
        wire.extend_from_slice(&0i32.to_le_bytes());
        wire.extend_from_slice(&0i32.to_le_bytes());
        wire.push(1);
        wire.extend_from_slice(&[255, 255, 255, 255]);
        wire.extend_from_slice(&1u16.to_le_bytes());
        wire.push(0xff);
        assert_eq!(
            decode_batch(&wire),
            Err(GuiBridgeCodecError::InvalidUtf8)
        );

        // NaN texcoord on BlitAtlas.
        let mut wire = Vec::new();
        wire.extend_from_slice(&GGB1_MAGIC);
        wire.push(GGB1_VERSION);
        wire.push(0);
        wire.extend_from_slice(&1u16.to_le_bytes());
        wire.push(TAG_BLIT_ATLAS);
        wire.extend_from_slice(&0i32.to_le_bytes());
        wire.extend_from_slice(&0i32.to_le_bytes());
        wire.extend_from_slice(&16u32.to_le_bytes());
        wire.extend_from_slice(&16u32.to_le_bytes());
        wire.extend_from_slice(&1u16.to_le_bytes());
        wire.push(b'a');
        wire.extend_from_slice(&f32::NAN.to_le_bytes());
        wire.extend_from_slice(&0f32.to_le_bytes());
        wire.extend_from_slice(&1f32.to_le_bytes());
        wire.extend_from_slice(&1f32.to_le_bytes());
        wire.extend_from_slice(&[255, 255, 255, 255]);
        assert_eq!(
            decode_batch(&wire),
            Err(GuiBridgeCodecError::NonFiniteTexcoord)
        );
    }

    #[test]
    fn encode_and_decode_reject_unbalanced_scissors() {
        // Unclosed push.
        let open = GuiBridgeBatch::new(vec![GuiBridgeDrawOp::PushScissor {
            x: 0,
            y: 0,
            w: 8,
            h: 8,
        }]);
        assert_eq!(
            encode_batch(&open),
            Err(GuiBridgeCodecError::UnbalancedScissors)
        );
        assert!(!open.is_valid());

        // Pop on an empty stack (encode path).
        let underflow = GuiBridgeBatch::new(vec![GuiBridgeDrawOp::PopScissor]);
        assert_eq!(
            encode_batch(&underflow),
            Err(GuiBridgeCodecError::ScissorUnderflow)
        );

        // Same violation hand-encoded — the decoder must catch it too.
        let mut wire = Vec::new();
        wire.extend_from_slice(&GGB1_MAGIC);
        wire.push(GGB1_VERSION);
        wire.push(0);
        wire.extend_from_slice(&1u16.to_le_bytes());
        wire.push(TAG_POP_SCISSOR);
        assert_eq!(
            decode_batch(&wire),
            Err(GuiBridgeCodecError::ScissorUnderflow)
        );

        // Two pushes, one pop: still open at frame end.
        let mut wire = Vec::new();
        wire.extend_from_slice(&GGB1_MAGIC);
        wire.push(GGB1_VERSION);
        wire.push(0);
        wire.extend_from_slice(&3u16.to_le_bytes());
        for _ in 0..2 {
            wire.push(TAG_PUSH_SCISSOR);
            wire.extend_from_slice(&0i32.to_le_bytes());
            wire.extend_from_slice(&0i32.to_le_bytes());
            wire.extend_from_slice(&8u32.to_le_bytes());
            wire.extend_from_slice(&8u32.to_le_bytes());
        }
        wire.push(TAG_POP_SCISSOR);
        assert_eq!(
            decode_batch(&wire),
            Err(GuiBridgeCodecError::UnbalancedScissors)
        );
    }

    #[test]
    fn encoder_rejects_oversized_batches_and_strings() {
        let big = GuiBridgeBatch::new(vec![
            GuiBridgeDrawOp::FillRect { x: 0, y: 0, w: 1, h: 1, rgba: [0; 4] };
            MAX_BRIDGE_OPS + 1
        ]);
        assert_eq!(
            encode_batch(&big),
            Err(GuiBridgeCodecError::TooManyOps(MAX_BRIDGE_OPS + 1))
        );

        let long_text = GuiBridgeBatch::new(vec![GuiBridgeDrawOp::DrawText {
            x: 0,
            y: 0,
            scale: 1,
            rgba: [255; 4],
            text: "a".repeat(MAX_BRIDGE_STRING + 1),
        }]);
        assert!(matches!(
            encode_batch(&long_text),
            Err(GuiBridgeCodecError::StringTooLong(n)) if n == MAX_BRIDGE_STRING + 1
        ));

        // Exactly at the caps still passes.
        let at_cap = GuiBridgeBatch::new(vec![GuiBridgeDrawOp::DrawText {
            x: 0,
            y: 0,
            scale: 1,
            rgba: [255; 4],
            text: "a".repeat(MAX_BRIDGE_STRING),
        }]);
        assert!(encode_batch(&at_cap).is_ok());
    }

    #[test]
    fn slot_action_gate_enforces_fair_play_cap() {
        let mut gate = GuiBridgeSlotActionGate::new();
        for _ in 0..MAX_SLOT_ACTIONS_PER_TICK {
            assert!(gate.try_submit(GuiBridgeSlotAction::ClickSlot { slot: 0, button: 0 }));
        }
        assert_eq!(gate.remaining(), 0);
        // The fifth action in one tick is refused and queued — it can
        // never become an anti-cheat-tripping burst.
        assert!(!gate.try_submit(GuiBridgeSlotAction::QuickMove { slot: 1 }));
        assert!(!gate.try_submit(GuiBridgeSlotAction::SwapHotbar {
            slot: 2,
            hotbar: 3
        }));
        assert_eq!(gate.pending_len(), 2);

        // Next tick drains the queue inside the same cap.
        let drained = gate.begin_tick();
        assert_eq!(
            drained,
            vec![
                GuiBridgeSlotAction::QuickMove { slot: 1 },
                GuiBridgeSlotAction::SwapHotbar { slot: 2, hotbar: 3 },
            ]
        );
        assert_eq!(gate.remaining(), MAX_SLOT_ACTIONS_PER_TICK - 2);
    }

    #[test]
    fn lowering_clips_to_viewport_and_scissors() {
        let batch = GuiBridgeBatch::new(vec![
            // Partially off-screen: clipped to the viewport.
            GuiBridgeDrawOp::FillRect {
                x: -10,
                y: -5,
                w: 40,
                h: 30,
                rgba: [1, 2, 3, 255],
            },
            // Entirely outside: dropped.
            GuiBridgeDrawOp::FillRect {
                x: 500,
                y: 500,
                w: 10,
                h: 10,
                rgba: [0; 4],
            },
            GuiBridgeDrawOp::PushScissor {
                x: 0,
                y: 0,
                w: 20,
                h: 20,
            },
            // Extends past the scissor: intersected down.
            GuiBridgeDrawOp::FillRect {
                x: 10,
                y: 10,
                w: 50,
                h: 50,
                rgba: [9, 9, 9, 255],
            },
            // Text origin outside the clip: dropped whole.
            GuiBridgeDrawOp::DrawText {
                x: 40,
                y: 40,
                scale: 0,
                rgba: [255; 4],
                text: "gone".to_owned(),
            },
            // Text origin inside: kept, scale clamped to >= 1.
            GuiBridgeDrawOp::DrawText {
                x: 4,
                y: 4,
                scale: 0,
                rgba: [255; 4],
                text: "kept".to_owned(),
            },
            GuiBridgeDrawOp::PopScissor,
            // After the pop the clip is the viewport again.
            GuiBridgeDrawOp::FillRect {
                x: 30,
                y: 30,
                w: 10,
                h: 10,
                rgba: [7; 4],
            },
        ]);
        let cmds = lower_bridge_batch_to_draw_cmds(&batch, 100, 100);
        assert_eq!(cmds.len(), 4);
        match &cmds[0] {
            UiCommand::FillRect { rect, color } => {
                assert_eq!(*rect, UiRect::new(0, 0, 30, 25));
                assert_eq!(*color, UiColor::rgba(1, 2, 3, 255));
            }
            other => panic!("expected FillRect, got {other:?}"),
        }
        match &cmds[1] {
            UiCommand::FillRect { rect, .. } => {
                assert_eq!(*rect, UiRect::new(10, 10, 10, 10));
            }
            other => panic!("expected scissored FillRect, got {other:?}"),
        }
        match &cmds[2] {
            UiCommand::Text { text, scale, .. } => {
                assert_eq!(text, "kept");
                assert_eq!(*scale, 1);
            }
            other => panic!("expected kept Text, got {other:?}"),
        }
        match &cmds[3] {
            UiCommand::FillRect { rect, .. } => {
                assert_eq!(*rect, UiRect::new(30, 30, 10, 10));
            }
            other => panic!("expected post-pop FillRect, got {other:?}"),
        }
    }

    #[test]
    fn lowering_nested_scissors_intersect_and_item_icon_badges() {
        let batch = GuiBridgeBatch::new(vec![
            GuiBridgeDrawOp::PushScissor {
                x: 0,
                y: 0,
                w: 40,
                h: 40,
            },
            GuiBridgeDrawOp::PushScissor {
                x: 20,
                y: 0,
                w: 40,
                h: 40,
            },
            // Effective clip is x∈[20,40): the child narrows the parent.
            GuiBridgeDrawOp::FillRect {
                x: 0,
                y: 0,
                w: 100,
                h: 10,
                rgba: [5; 4],
            },
            GuiBridgeDrawOp::DrawItemIcon {
                x: 24,
                y: 12,
                item_id: "minecraft:apple".to_owned(),
                count: 7,
            },
            GuiBridgeDrawOp::PopScissor,
            GuiBridgeDrawOp::PopScissor,
        ]);
        let cmds = lower_bridge_batch_to_draw_cmds(&batch, 64, 64);
        // fill + icon footprint + count badge
        assert_eq!(cmds.len(), 3);
        match &cmds[0] {
            UiCommand::FillRect { rect, .. } => {
                assert_eq!(*rect, UiRect::new(20, 0, 20, 10));
            }
            other => panic!("expected nested-scissor FillRect, got {other:?}"),
        }
        match &cmds[1] {
            UiCommand::FillRect { rect, color } => {
                assert_eq!(*rect, UiRect::new(24, 12, 16, 16));
                assert_eq!(*color, palette::SLOT);
            }
            other => panic!("expected item icon footprint, got {other:?}"),
        }
        match &cmds[2] {
            UiCommand::Text { text, x, y, .. } => {
                assert_eq!(text, "7");
                // Right-bottom aligned inside the 16px icon box.
                assert_eq!(*x + text_width(text, 1) as i32, 24 + ITEM_ICON_PX as i32 + 1);
                assert_eq!(*y, 12 + ITEM_ICON_PX as i32 - 8);
            }
            other => panic!("expected count badge, got {other:?}"),
        }
    }

    #[test]
    fn lowering_atlas_emits_tinted_footprint_and_empty_batch_is_empty() {
        let batch = GuiBridgeBatch::new(vec![GuiBridgeDrawOp::BlitAtlas {
            x: 2,
            y: 3,
            w: 9,
            h: 7,
            texture_id: "megu:atlas".to_owned(),
            u0: 0.0,
            v0: 0.0,
            u1: 1.0,
            v1: 1.0,
            tint: [11, 22, 33, 44],
        }]);
        let cmds = lower_bridge_batch_to_draw_cmds(&batch, 32, 32);
        assert_eq!(
            cmds,
            vec![UiCommand::FillRect {
                rect: UiRect::new(2, 3, 9, 7),
                color: UiColor::rgba(11, 22, 33, 44),
            }]
        );
        assert!(lower_bridge_batch_to_draw_cmds(&GuiBridgeBatch::default(), 8, 8).is_empty());
    }

    #[test]
    fn hud_snapshot_is_plain_data() {
        let snap = GuiBridgeHudSnapshot {
            hp: 18.5,
            max_hp: 20.0,
            food: 17,
            saturation: 4.5,
            armor: 15,
            air: 300,
            pos: [10.5, 64.0, -3.25],
            biome_id: "minecraft:plains".to_owned(),
            targeted_block_id: Some("minecraft:grass_block".to_owned()),
            targeted_entity_id: None,
            held_item_id: Some("minecraft:diamond_pickaxe".to_owned()),
            screen_w: 1280,
            screen_h: 720,
        };
        let cloned = snap.clone();
        assert_eq!(snap, cloned);
    }
}
