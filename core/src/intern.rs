// SPDX-License-Identifier: LGPL-3.0-or-later
#![forbid(unsafe_code)]
//! `megu_core::intern` — string interning and generational arenas.
//!
//! Two primitives every layer of the engine shares:
//!
//! * [`Symbol`] / [`StringInterner`] — deduplicates strings into one
//!   contiguous buffer so block/item/biome/mod registry IDs, scoreboard
//!   owners, UI labels, and command tokens compare and hash as `u32`s
//!   instead of scanning bytes.
//! * [`GenIndex`] / [`GenArena`] — a dense generational arena giving O(1)
//!   insert/remove/lookup with ABA-free handles: a stale `GenIndex` can
//!   never alias a live slot because every reuse bumps the slot's
//!   generation. Ideal for entity handles, mod objects, UI widget handles,
//!   and network session IDs — the classic "dangling ID" class of bug is
//!   eliminated by construction.
//!
//! Everything here is pure `std`, deterministic, and `unsafe`-free.

use std::collections::HashMap;

/// Opaque interned-string handle. `Copy`, `u32`-sized, and equality is an
/// integer compare: two [`Symbol`]s are equal iff they name the same
/// string in the same [`StringInterner`].
///
/// Symbols are only meaningful for the interner that produced them; mixing
/// symbols across interners is not prevented statically but resolves to
/// `None` or wrong strings — keep one interner per namespace scope.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Symbol(pub u32);

/// Deduplicating string interner backed by one contiguous `String` buffer.
///
/// Interned bytes are never re-allocated or moved: the buffer only grows,
/// and each [`Symbol`] indexes a `(offset, len)` span into it. Lookups use
/// a `hash -> candidate symbols` bucket table so no string is stored twice
/// and hash collisions resolve by exact byte comparison.
#[derive(Debug, Default)]
pub struct StringInterner {
    /// All interned bytes, appended once per unique string.
    buf: String,
    /// `spans[sym.0] = (offset, len)` of the symbol's bytes in `buf`.
    spans: Vec<(u32, u32)>,
    /// FNV-1a hash -> symbols whose bytes hash to it. Buckets stay tiny in
    /// expectation; collisions are resolved by byte equality.
    buckets: HashMap<u64, Vec<u32>>,
}

impl StringInterner {
    /// Empty interner.
    pub fn new() -> Self {
        Self::default()
    }

    /// Empty interner with room for `capacity` distinct strings.
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            buf: String::new(),
            spans: Vec::with_capacity(capacity),
            buckets: HashMap::with_capacity(capacity),
        }
    }

    /// Intern `s`, returning its symbol; equal strings map to equal
    /// symbols. The first intern of a string appends its bytes to the
    /// contiguous buffer; repeats are a hash lookup plus byte compare.
    pub fn intern(&mut self, s: &str) -> Symbol {
        let hash = crate::hash::fnv1a64(s.as_bytes());
        if let Some(sym) = self.lookup_hashed(s, hash) {
            return sym;
        }
        let offset = self.buf.len() as u32;
        self.buf.push_str(s);
        let sym = Symbol(self.spans.len() as u32);
        self.spans.push((offset, s.len() as u32));
        self.buckets.entry(hash).or_default().push(sym.0);
        sym
    }

    /// Symbol for a previously interned string, or `None`.
    pub fn get(&self, s: &str) -> Option<Symbol> {
        self.lookup_hashed(s, crate::hash::fnv1a64(s.as_bytes()))
    }

    /// The string behind `sym`, or `None` for symbols this interner never
    /// issued.
    pub fn resolve(&self, sym: Symbol) -> Option<&str> {
        let &(offset, len) = self.spans.get(sym.0 as usize)?;
        Some(&self.buf[offset as usize..offset as usize + len as usize])
    }

    /// Number of distinct interned strings.
    pub fn len(&self) -> usize {
        self.spans.len()
    }

    /// Whether nothing has been interned yet.
    pub fn is_empty(&self) -> bool {
        self.spans.is_empty()
    }

    /// Total bytes held by the contiguous string buffer.
    pub fn buffer_bytes(&self) -> usize {
        self.buf.len()
    }

    fn lookup_hashed(&self, s: &str, hash: u64) -> Option<Symbol> {
        self.buckets
            .get(&hash)?
            .iter()
            .copied()
            .find(|&raw| self.str_at(raw) == s)
            .map(Symbol)
    }

    /// Bytes of span `raw` — caller guarantees `raw < spans.len()`.
    fn str_at(&self, raw: u32) -> &str {
        let (offset, len) = self.spans[raw as usize];
        &self.buf[offset as usize..offset as usize + len as usize]
    }
}

impl std::fmt::Display for Symbol {
    /// Prints the raw index; resolve through the owning interner for text.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "sym#{}", self.0)
    }
}

// ---------------------------------------------------------------------------
// Generational arena
// ---------------------------------------------------------------------------

/// ABA-free handle into a [`GenArena`]: the `generation` field pins the
/// exact lifetime of the slot at `index`, so a handle captured before a
/// remove+insert cycle can never alias the new occupant.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct GenIndex {
    /// Slot index inside the arena.
    pub index: u32,
    /// Generation the slot had when this handle was issued.
    pub generation: u32,
}

enum SlotState<T> {
    Occupied(T),
    /// Index of the next free slot, or `u32::MAX` at the end of the chain.
    Free { next_free: u32 },
}

struct Slot<T> {
    /// Bumped on every removal so stale `GenIndex`es miss the new occupant.
    generation: u32,
    state: SlotState<T>,
}

/// Dense generational arena with O(1) `insert`, `remove`, `get`/`get_mut`
/// and free-list slot recycling.
///
/// Storage is a single `Vec<Slot<T>>`; removed slots join an intrusive
/// free list and are handed out in LIFO order with a bumped generation, so
/// any [`GenIndex`] minted before the removal fails lookup afterwards.
/// Iteration yields only live `(GenIndex, &T)` pairs — tombstones are
/// skipped without exposing them.
pub struct GenArena<T> {
    slots: Vec<Slot<T>>,
    /// Head of the free-slot chain (`u32::MAX` when empty).
    free_head: u32,
    /// Number of `Occupied` slots; `len` is O(1).
    live: usize,
}

const FREE_NIL: u32 = u32::MAX;

impl<T> Default for GenArena<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> GenArena<T> {
    /// Empty arena.
    pub fn new() -> Self {
        Self {
            slots: Vec::new(),
            free_head: FREE_NIL,
            live: 0,
        }
    }

    /// Empty arena with room for `capacity` live values.
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            slots: Vec::with_capacity(capacity),
            free_head: FREE_NIL,
            live: 0,
        }
    }

    /// Insert `value`, returning its handle. Reuses a freed slot (with a
    /// bumped generation) when one is available; otherwise grows the dense
    /// slot array.
    pub fn insert(&mut self, value: T) -> GenIndex {
        self.live += 1;
        if self.free_head != FREE_NIL {
            let idx = self.free_head as usize;
            let slot = &mut self.slots[idx];
            let next = match slot.state {
                SlotState::Free { next_free } => next_free,
                SlotState::Occupied(_) => unreachable!("free list points at live slot"),
            };
            slot.state = SlotState::Occupied(value);
            self.free_head = next;
            GenIndex {
                index: idx as u32,
                generation: slot.generation,
            }
        } else {
            let idx = self.slots.len();
            self.slots.push(Slot {
                generation: 0,
                state: SlotState::Occupied(value),
            });
            GenIndex {
                index: idx as u32,
                generation: 0,
            }
        }
    }

    /// Remove `id`, returning its value when the handle is live and
    /// `None` for stale or foreign handles. The slot's generation is
    /// bumped before it joins the free list, so the retired handle — and
    /// any copy of it — misses permanently.
    pub fn remove(&mut self, id: GenIndex) -> Option<T> {
        let slot = self.slots.get_mut(id.index as usize)?;
        if slot.generation != id.generation {
            return None;
        }
        let state = std::mem::replace(&mut slot.state, SlotState::Free { next_free: 0 });
        let value = match state {
            SlotState::Occupied(v) => v,
            SlotState::Free { .. } => {
                slot.state = state;
                return None;
            }
        };
        // Wrap is unreachable in practice (2^32 removals on one slot) but
        // wrapping_add keeps release builds panic-free even there.
        slot.generation = slot.generation.wrapping_add(1);
        slot.state = SlotState::Free {
            next_free: self.free_head,
        };
        self.free_head = id.index;
        self.live -= 1;
        Some(value)
    }

    /// Shared reference for a live handle, `None` for stale/foreign ones.
    pub fn get(&self, id: GenIndex) -> Option<&T> {
        let slot = self.slots.get(id.index as usize)?;
        if slot.generation != id.generation {
            return None;
        }
        match &slot.state {
            SlotState::Occupied(v) => Some(v),
            SlotState::Free { .. } => None,
        }
    }

    /// Exclusive reference for a live handle.
    pub fn get_mut(&mut self, id: GenIndex) -> Option<&mut T> {
        let slot = self.slots.get_mut(id.index as usize)?;
        if slot.generation != id.generation {
            return None;
        }
        match &mut slot.state {
            SlotState::Occupied(v) => Some(v),
            SlotState::Free { .. } => None,
        }
    }

    /// Whether `id` currently resolves to a live value.
    pub fn contains(&self, id: GenIndex) -> bool {
        self.get(id).is_some()
    }

    /// Number of live values.
    pub fn len(&self) -> usize {
        self.live
    }

    /// Whether the arena holds no live values.
    pub fn is_empty(&self) -> bool {
        self.live == 0
    }

    /// Allocated slot count (live + recycled); grows monotonically.
    pub fn capacity_slots(&self) -> usize {
        self.slots.len()
    }

    /// Iterate all live `(GenIndex, &T)` pairs in slot order.
    pub fn iter(&self) -> impl Iterator<Item = (GenIndex, &T)> {
        self.slots.iter().enumerate().filter_map(|(i, slot)| {
            if let SlotState::Occupied(v) = &slot.state {
                Some((
                    GenIndex {
                        index: i as u32,
                        generation: slot.generation,
                    },
                    v,
                ))
            } else {
                None
            }
        })
    }

    /// Iterate all live `(GenIndex, &mut T)` pairs in slot order.
    pub fn iter_mut(&mut self) -> impl Iterator<Item = (GenIndex, &mut T)> {
        self.slots.iter_mut().enumerate().filter_map(|(i, slot)| {
            if let SlotState::Occupied(v) = &mut slot.state {
                Some((
                    GenIndex {
                        index: i as u32,
                        generation: slot.generation,
                    },
                    v,
                ))
            } else {
                None
            }
        })
    }
}
