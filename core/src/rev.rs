// SPDX-License-Identifier: LGPL-3.0-or-later
#![forbid(unsafe_code)]
//! `megu_core::rev` — universal zero-allocation monotonic revisions and
//! the dirty-layer bus every retained renderer, HUD surface, modded
//! overlay and screen container in megu subscribes to.
//!
//! [`Revision`] is the crate-wide generation token: a monotonically
//! increasing, never-reused `u64` with `0` reserved as "never touched"
//! ([`Revision::NONE`]). [`LayerRevBus`] pins `N` of those counters in a
//! fixed-size array plus one `u64` frame dirty mask, so a bump, a
//! staleness query and a clean-mark are all `O(1)` with **zero** heap
//! allocation — the whole bus is plain data a caller can embed in its
//! own state or a shared page.
//!
//! Two complementary signals ride on the same counters:
//!
//! * **Revision comparison** — [`LayerRevBus::is_dirty_since`] compares
//!   a layer's generation against a token the consumer captured at its
//!   last bake. That is the retained-mode query:
//!   [`RetainedLayerTracker`] folds it into a [`DirtyLayers`] bitmask so
//!   a frame with no changes performs 0 rebuilds, 0 heap allocations and
//!   0 GPU uploads.
//! * **Frame dirty bits** — every [`LayerRevBus::bump`] also sets the
//!   layer's bit in the frame dirty mask ([`LayerRevBus::dirty_mask`],
//!   [`LayerRevBus::any_dirty`]) until [`LayerRevBus::mark_clean`] clears
//!   it: the "someone touched something this frame" flag for compositors
//!   that do not keep per-layer baked revisions.
//!
//! [`UiLayer`] gives the canonical 1.21.1 render-stack order on
//! [`UiLayerBus`] (`LayerRevBus<8>`): backdrop, static HUD, dynamic HUD,
//! active screen, vanilla overlay, mod HUD and mod overlay occupy bits
//! `0..=6`, leaving bit 7 as headroom for one future layer without a
//! layout break. A Fabric HUD widget, a Create goggles overlay and a
//! WASM mod panel all bump the same spine — [`UiLayer::ModHud`] /
//! [`UiLayer::ModOverlay`] exist so modded surfaces dirty *their own*
//! layer and never force the vanilla hotbar to re-tessellate.
//!
//! For `N > 64` the frame dirty mask tracks layers `0..64` exactly and
//! folds all higher layers into the shared `dirty_high` overflow flag;
//! revision comparison stays exact for every `N`.

use std::fmt;

/// Bit width of the frame dirty mask. Layers at index `>= MASK_BITS`
/// fold into the shared `dirty_high` overflow flag instead of owning a
/// dedicated bit.
const MASK_BITS: usize = u64::BITS as usize;

/// Monotonic, never-reused generation token.
///
/// `0` ([`Revision::NONE`]) is the permanent "never touched" sentinel;
/// [`LayerRevBus::bump`] skips it on wraparound so a live token never
/// aliases the sentinel even after `2^64` bumps.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Revision(pub u64);

impl Revision {
    /// `0` — the "never touched / nothing baked yet" sentinel.
    pub const NONE: Self = Self(0);

    /// The raw counter value.
    #[inline]
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }

    /// `true` when this token is [`Revision::NONE`] — i.e. the layer was
    /// never bumped since the bus started.
    #[inline]
    #[must_use]
    pub const fn is_none(self) -> bool {
        self.0 == 0
    }
}

impl fmt::Display for Revision {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "rev{}", self.0)
    }
}

/// Fixed-capacity dirty-layer revision bus: `[u64; N]` generations plus
/// one `u64` frame dirty mask. Zero heap allocation, `O(1)` for every
/// operation, and copyable state — a renderer can snapshot the bus by
/// value and diff two snapshots through [`RetainedLayerTracker`].
#[derive(Clone, Debug)]
pub struct LayerRevBus<const N: usize> {
    revs: [u64; N],
    dirty: u64,
    dirty_high: bool,
}

impl<const N: usize> Default for LayerRevBus<N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const N: usize> LayerRevBus<N> {
    /// Empty bus: every layer at [`Revision::NONE`], nothing dirty.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            revs: [0; N],
            dirty: 0,
            dirty_high: false,
        }
    }

    /// Number of layers this bus tracks — the const `N`.
    #[inline]
    #[must_use]
    pub const fn len(&self) -> usize {
        N
    }

    /// `true` for `N == 0` — a degenerate bus that dirty-tracks nothing.
    #[inline]
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        N == 0
    }

    /// Advance `layer` to the next generation and set its frame dirty
    /// flag. `O(1)`, zero allocation; the wraparound at `u64::MAX` skips
    /// `0` so the returned token never aliases [`Revision::NONE`].
    ///
    /// # Panics
    ///
    /// `layer` must be `< N`; an out-of-range index is a contract bug,
    /// not recoverable input.
    #[inline]
    pub fn bump(&mut self, layer: usize) -> Revision {
        assert!(layer < N, "layer {layer} out of range for LayerRevBus<{N}>");
        let next = self.revs[layer].wrapping_add(1).max(1);
        self.revs[layer] = next;
        if layer < MASK_BITS {
            self.dirty |= 1u64 << layer;
        } else {
            self.dirty_high = true;
        }
        Revision(next)
    }

    /// Current generation of `layer` — [`Revision::NONE`] until its
    /// first [`bump`](Self::bump).
    ///
    /// # Panics
    ///
    /// `layer` must be `< N`.
    #[inline]
    #[must_use]
    pub fn rev(&self, layer: usize) -> Revision {
        assert!(layer < N, "layer {layer} out of range for LayerRevBus<{N}>");
        Revision(self.revs[layer])
    }

    /// `true` when `layer`'s generation differs from `seen` — i.e. the
    /// layer was bumped after the consumer captured `seen`. Passing
    /// [`Revision::NONE`] answers "was this layer ever touched".
    ///
    /// This is a *generation* comparison independent of the frame dirty
    /// bits, so it stays correct across [`mark_clean`](Self::mark_clean)
    /// — the right query for consumers that remember their own baked
    /// revision, which is exactly what [`RetainedLayerTracker`] wraps.
    ///
    /// # Panics
    ///
    /// `layer` must be `< N`.
    #[inline]
    #[must_use]
    pub fn is_dirty_since(&self, layer: usize, seen: Revision) -> bool {
        self.rev(layer) != seen
    }

    /// `true` while `layer`'s *frame dirty flag* is set — i.e. it was
    /// bumped since the last [`mark_clean`](Self::mark_clean) or
    /// [`clear_dirty`](Self::clear_dirty). For `layer >= 64` this reports
    /// the shared overflow flag.
    ///
    /// # Panics
    ///
    /// `layer` must be `< N`.
    #[inline]
    #[must_use]
    pub fn is_flagged_dirty(&self, layer: usize) -> bool {
        assert!(layer < N, "layer {layer} out of range for LayerRevBus<{N}>");
        if layer < MASK_BITS {
            self.dirty & (1u64 << layer) != 0
        } else {
            self.dirty_high
        }
    }

    /// Clear the frame dirty flag for `layer`. Revision history is
    /// untouched — [`is_dirty_since`](Self::is_dirty_since) still sees
    /// every past bump.
    ///
    /// For `layer >= 64` this clears the shared overflow flag (the
    /// documented coarser granularity); with `N <= 64` every layer owns
    /// an exact bit.
    ///
    /// # Panics
    ///
    /// `layer` must be `< N`.
    #[inline]
    pub fn mark_clean(&mut self, layer: usize) {
        assert!(layer < N, "layer {layer} out of range for LayerRevBus<{N}>");
        if layer < MASK_BITS {
            self.dirty &= !(1u64 << layer);
        } else {
            self.dirty_high = false;
        }
    }

    /// Clear every frame dirty flag — "this frame's compositing pass
    /// consumed all changes". Revisions are untouched.
    #[inline]
    pub fn clear_dirty(&mut self) {
        self.dirty = 0;
        self.dirty_high = false;
    }

    /// `true` when any layer carries an uncleared frame dirty flag.
    #[inline]
    #[must_use]
    pub const fn any_dirty(&self) -> bool {
        self.dirty != 0 || self.dirty_high
    }

    /// The raw frame dirty bitmask for layers `0..64` — bit `i` is
    /// layer `i`. Layers `>= 64` report through
    /// [`dirty_high`](Self::dirty_high) / [`any_dirty`](Self::any_dirty).
    #[inline]
    #[must_use]
    pub const fn dirty_mask(&self) -> u64 {
        self.dirty
    }

    /// `true` while a layer `>= 64` carries an uncleared frame dirty
    /// flag. Always `false` for `N <= 64`.
    #[inline]
    #[must_use]
    pub const fn dirty_high(&self) -> bool {
        self.dirty_high
    }
}

/// Canonical UI render-stack layer indices — vanilla occupies `0..=4`,
/// modded surfaces `5..=6`, bit 7 is reserved headroom on
/// [`UiLayerBus`].
///
/// Indices are ascending *bottom-to-top* draw order: `Backdrop` (0)
/// composites first, `ModOverlay` (6) composites last — so a mod modal
/// always lands above the vanilla screen and overlay layers.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum UiLayer {
    /// Panorama / menu-blur backdrop behind everything.
    Backdrop = 0,
    /// Hotbar, hearts, armor, hunger, XP — the pieces that only rebuild
    /// on a state change, never per frame.
    StaticHud = 1,
    /// Chat, actionbar, subtitles, F3 debug, bossbar.
    DynamicHud = 2,
    /// The active vanilla or modded screen / container.
    Screen = 3,
    /// JEI/EMI sidebars, tooltips, drag cursor, gamepad prompts.
    Overlay = 4,
    /// Any mod HUD slot: Create goggles, minimap, WASM mod HUD.
    ModHud = 5,
    /// Any mod modal / overlay surface above everything else.
    ModOverlay = 6,
}

impl UiLayer {
    /// All defined layers in canonical index order —
    /// `ALL[layer.index()] == layer` for every variant.
    pub const ALL: [Self; Self::COUNT] = [
        Self::Backdrop,
        Self::StaticHud,
        Self::DynamicHud,
        Self::Screen,
        Self::Overlay,
        Self::ModHud,
        Self::ModOverlay,
    ];

    /// Number of defined layers. [`UiLayerBus`] is `LayerRevBus<8>`, so
    /// bit 7 remains reserved headroom.
    pub const COUNT: usize = 7;

    /// The stable bus index / dirty-bit position `0..=6`.
    #[inline]
    #[must_use]
    pub const fn index(self) -> usize {
        self as usize
    }

    /// The `UiLayer` for bus index `0..=6`; `None` for the reserved
    /// headroom index 7 and anything above it.
    #[inline]
    #[must_use]
    pub const fn from_index(index: usize) -> Option<Self> {
        match index {
            0 => Some(Self::Backdrop),
            1 => Some(Self::StaticHud),
            2 => Some(Self::DynamicHud),
            3 => Some(Self::Screen),
            4 => Some(Self::Overlay),
            5 => Some(Self::ModHud),
            6 => Some(Self::ModOverlay),
            _ => None,
        }
    }

    /// Stable lowercase key for logs, manifests and config files.
    #[inline]
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Backdrop => "backdrop",
            Self::StaticHud => "static_hud",
            Self::DynamicHud => "dynamic_hud",
            Self::Screen => "screen",
            Self::Overlay => "overlay",
            Self::ModHud => "mod_hud",
            Self::ModOverlay => "mod_overlay",
        }
    }

    /// Bump this layer's revision on `bus`, setting its frame dirty
    /// flag and returning the new token. Works for any `N >= 7` — the
    /// canonical [`UiLayerBus`] included.
    ///
    /// # Panics
    ///
    /// Propagates [`LayerRevBus::bump`]'s panic when `N < 7`.
    #[inline]
    pub fn bump<const N: usize>(self, bus: &mut LayerRevBus<N>) -> Revision {
        bus.bump(self.index())
    }
}

impl fmt::Display for UiLayer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The canonical UI layer bus: 7 defined layers ([`UiLayer`]) plus one
/// reserved headroom bit.
pub type UiLayerBus = LayerRevBus<8>;

/// Bitmask answer to "which layers need re-tessellation this frame?".
///
/// [`is_empty`](Self::is_empty) is the steady-state fast path the whole
/// design exists for: when every layer's generation still matches its
/// last bake, the caller performs 0 rebuilds, 0 heap allocations and 0
/// GPU uploads.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DirtyLayers {
    mask: u64,
    high: bool,
}

impl DirtyLayers {
    /// The empty set — nothing needs rebuilding.
    pub const NONE: Self = Self { mask: 0, high: false };

    /// `true` when no layer is dirty — the zero-work fast path.
    #[inline]
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.mask == 0 && !self.high
    }

    /// The raw dirty bitmask for layers `0..64`.
    #[inline]
    #[must_use]
    pub const fn mask(self) -> u64 {
        self.mask
    }

    /// `true` when at least one layer `>= 64` is dirty. Such layers do
    /// not own dedicated mask bits and report through this shared flag
    /// instead.
    #[inline]
    #[must_use]
    pub const fn has_high(self) -> bool {
        self.high
    }

    /// `true` when `layer` is dirty. For `layer >= 64` this reports the
    /// shared [`has_high`](Self::has_high) flag.
    #[inline]
    #[must_use]
    pub const fn contains(self, layer: usize) -> bool {
        if layer < MASK_BITS {
            self.mask & (1u64 << layer) != 0
        } else {
            self.high
        }
    }

    /// Number of dirty layers — exact while every layer is `< 64`; the
    /// `>= 64` overflow counts as one.
    #[inline]
    #[must_use]
    pub fn count(self) -> u32 {
        self.mask.count_ones() + u32::from(self.high)
    }

    /// Iterate the dirty layer indices `0..64` in ascending order.
    /// Overflow layers (`>= 64`) are reported through
    /// [`has_high`](Self::has_high), never yielded individually.
    #[inline]
    pub fn iter(self) -> DirtyLayerIter {
        DirtyLayerIter { mask: self.mask }
    }

    /// Union of two dirty sets — `a | b`.
    #[inline]
    #[must_use]
    pub const fn union(self, other: Self) -> Self {
        Self {
            mask: self.mask | other.mask,
            high: self.high || other.high,
        }
    }
}

impl fmt::Display for DirtyLayers {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "DirtyLayers(0x{:016x}{})", self.mask, if self.high { "+high" } else { "" })
    }
}

/// Ascending-index iterator over [`DirtyLayers::iter`].
#[derive(Clone, Debug)]
pub struct DirtyLayerIter {
    mask: u64,
}

impl Iterator for DirtyLayerIter {
    type Item = usize;

    #[inline]
    fn next(&mut self) -> Option<usize> {
        if self.mask == 0 {
            return None;
        }
        let index = self.mask.trailing_zeros() as usize;
        self.mask &= self.mask - 1;
        Some(index)
    }

    #[inline]
    fn size_hint(&self) -> (usize, Option<usize>) {
        let n = self.mask.count_ones() as usize;
        (n, Some(n))
    }
}

impl ExactSizeIterator for DirtyLayerIter {}

/// Per-layer baked-revision shadow of a [`LayerRevBus`].
///
/// The tracker records, per layer, the [`Revision`] the renderer last
/// tessellated against. [`poll`](Self::poll) / [`peek_dirty`](Self::peek_dirty)
/// diff those records against the live bus and hand back the exact
/// [`DirtyLayers`] set that needs re-tessellation — nothing else gets
/// rebuilt, so a steady-state frame costs one array compare and zero
/// heap traffic.
#[derive(Clone, Debug)]
pub struct RetainedLayerTracker<const N: usize> {
    /// `u64::MAX` = "never baked" — a sentinel [`LayerRevBus::bump`] can
    /// never produce (it wraps `MAX` to `1`), so a fresh tracker diffs
    /// every layer as dirty once, forcing the initial full bake.
    baked: [u64; N],
}

/// `baked[]` sentinel for "never baked". Unreachable from
/// [`LayerRevBus::bump`], which skips both `0` ([`Revision::NONE`]) on
/// startup and `u64::MAX` on wraparound.
const NEVER_BAKED: u64 = u64::MAX;

impl<const N: usize> Default for RetainedLayerTracker<N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const N: usize> RetainedLayerTracker<N> {
    /// Fresh tracker: every layer counts as "never baked", so the first
    /// [`poll`](Self::poll) / [`peek_dirty`](Self::peek_dirty) reports
    /// all `N` layers dirty — the initial full tessellation. (A layer
    /// whose bus revision is still [`Revision::NONE`] would otherwise
    /// read as already clean.)
    #[must_use]
    pub const fn new() -> Self {
        Self {
            baked: [NEVER_BAKED; N],
        }
    }

    /// Read-only diff: the layers whose live bus revision differs from
    /// the recorded bake. The tracker is *not* updated — callers that
    /// bake layers selectively pair this with
    /// [`mark_baked`](Self::mark_baked) per layer.
    ///
    /// # Panics
    ///
    /// The bus must have the same `N`; mismatched arities are a
    /// contract bug.
    #[must_use]
    pub fn peek_dirty(&self, bus: &LayerRevBus<N>) -> DirtyLayers {
        let mut mask = 0u64;
        let mut high = false;
        for (index, &rev) in bus.revs.iter().enumerate() {
            if rev != self.baked[index] {
                if index < MASK_BITS {
                    mask |= 1u64 << index;
                } else {
                    high = true;
                }
            }
        }
        DirtyLayers { mask, high }
    }

    /// Record `layer` as baked at revision `rev` — the selective-bake
    /// half of [`peek_dirty`](Self::peek_dirty) for renderers that
    /// rebuild only a subset of the returned mask.
    ///
    /// # Panics
    ///
    /// `layer` must be `< N`.
    #[inline]
    pub fn mark_baked(&mut self, layer: usize, rev: Revision) {
        assert!(layer < N, "layer {layer} out of range for RetainedLayerTracker<{N}>");
        self.baked[layer] = rev.0;
    }

    /// Diff-and-record in one pass: returns the [`DirtyLayers`] set
    /// needing re-tessellation *and* marks every layer baked at its
    /// current bus revision — the common "drain the whole diff" flow.
    /// The next `poll` reports nothing until the bus is bumped again.
    ///
    /// # Panics
    ///
    /// The bus must have the same `N`.
    #[inline]
    pub fn poll(&mut self, bus: &LayerRevBus<N>) -> DirtyLayers {
        let dirty = self.peek_dirty(bus);
        self.baked.copy_from_slice(&bus.revs);
        dirty
    }

    /// The [`Revision`] this tracker last baked `layer` at — `None`
    /// before the first bake or after [`reset`](Self::reset).
    ///
    /// # Panics
    ///
    /// `layer` must be `< N`.
    #[inline]
    #[must_use]
    pub fn baked_rev(&self, layer: usize) -> Option<Revision> {
        assert!(layer < N, "layer {layer} out of range for RetainedLayerTracker<{N}>");
        (self.baked[layer] != NEVER_BAKED).then(|| Revision(self.baked[layer]))
    }

    /// Forget every baked revision — the next diff reports all layers
    /// dirty. For full-surface reloads (resource pack switch, GUI scale
    /// change) where every layer must re-tessellate anyway.
    #[inline]
    pub fn reset(&mut self) {
        self.baked = [NEVER_BAKED; N];
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn revision_none_is_zero_and_never_emitted() {
        assert_eq!(Revision::NONE.get(), 0);
        assert!(Revision::NONE.is_none());
        let mut bus = LayerRevBus::<8>::new();
        let first = bus.bump(3);
        assert_eq!(first.get(), 1);
        assert!(!first.is_none());
    }

    #[test]
    fn bump_marks_only_the_bumped_layer() {
        let mut bus = UiLayerBus::new();
        UiLayer::DynamicHud.bump(&mut bus);
        assert_eq!(bus.dirty_mask(), 1 << 2);
        assert!(bus.is_flagged_dirty(2));
        assert!(!bus.is_flagged_dirty(1));
        assert!(!bus.is_flagged_dirty(3));
        assert!(!bus.is_flagged_dirty(5));
    }

    #[test]
    fn mark_clean_clears_only_the_frame_flag() {
        let mut bus = UiLayerBus::new();
        let seen = UiLayer::Screen.bump(&mut bus);
        assert!(bus.any_dirty());
        bus.mark_clean(UiLayer::Screen.index());
        assert!(!bus.any_dirty());
        // Generation history survives mark_clean.
        assert!(bus.is_dirty_since(3, Revision::NONE));
        assert!(!bus.is_dirty_since(3, seen));
    }

    #[test]
    fn retained_tracker_isolates_mod_hud() {
        let mut bus = UiLayerBus::new();
        let mut tracker = RetainedLayerTracker::<8>::new();
        let initial = tracker.poll(&bus);
        assert_eq!(initial.count() as usize, 8, "first poll = full bake");
        UiLayer::ModHud.bump(&mut bus);
        let dirty = tracker.poll(&bus);
        assert_eq!(dirty.mask(), 1 << 5);
        assert!(dirty.contains(UiLayer::ModHud.index()));
        assert!(!dirty.contains(UiLayer::StaticHud.index()));
        assert!(!dirty.contains(UiLayer::Screen.index()));
        assert!(!dirty.contains(UiLayer::ModOverlay.index()));
        assert!(tracker.poll(&bus).is_empty());
    }

    #[test]
    fn ui_layer_indices_are_canonical() {
        for (index, layer) in UiLayer::ALL.iter().copied().enumerate() {
            assert_eq!(layer.index(), index);
            assert_eq!(UiLayer::from_index(index), Some(layer));
        }
        assert_eq!(UiLayer::from_index(7), None);
        assert_eq!(UiLayer::ModHud as u8, 5);
        assert_eq!(UiLayer::ModOverlay as u8, 6);
    }

    #[test]
    fn layers_above_64_fold_into_the_overflow_flag() {
        let mut bus = LayerRevBus::<80>::new();
        bus.bump(70);
        assert!(bus.any_dirty());
        assert_eq!(bus.dirty_mask(), 0);
        assert!(bus.dirty_high());
        assert!(bus.is_flagged_dirty(79));
        bus.mark_clean(70);
        assert!(!bus.any_dirty());
        assert!(bus.is_dirty_since(70, Revision::NONE));
    }
}
