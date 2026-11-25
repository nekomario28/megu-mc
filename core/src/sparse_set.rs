// SPDX-License-Identifier: LGPL-3.0-or-later
#![forbid(unsafe_code)]
//! Briggs & Torczon sparse set with cache-contiguous dense values.
//!
//! The sparse array validates integer ids in O(1), while all live values sit in
//! one contiguous dense vector.  Removal is swap-remove, so iteration order is
//! insertion-order-like but not stable; callers that need a stable order should
//! sort [`SparseSet::ids`] or store their own ordering key in `T`.
//!
//! As with every sparse-set implementation, sparse storage grows to the largest
//! id ever inserted.  Use compact entity/block/widget id spaces rather than
//! sparse attacker-controlled or globally random ids.

const NIL: u32 = u32::MAX;

/// Dense cache-friendly sparse set mapping `u32` ids to values.
#[derive(Clone, Debug)]
pub struct SparseSet<T> {
    sparse: Vec<u32>,
    dense: Vec<u32>,
    values: Vec<T>,
}

impl<T> SparseSet<T> {
    /// Create an empty set.
    pub fn new() -> Self {
        Self {
            sparse: Vec::new(),
            dense: Vec::new(),
            values: Vec::new(),
        }
    }

    /// Create an empty set with dense capacity for `capacity` live entries.
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            sparse: Vec::new(),
            dense: Vec::with_capacity(capacity),
            values: Vec::with_capacity(capacity),
        }
    }

    /// Insert or replace `id`'s value, returning the old value when present.
    pub fn insert(&mut self, id: u32, value: T) -> Option<T> {
        if let Some(dense_index) = self.dense_index(id) {
            return Some(std::mem::replace(&mut self.values[dense_index], value));
        }
        assert!(
            self.dense.len() < NIL as usize,
            "sparse-set dense indices are u32 and reserve u32::MAX"
        );
        let sparse_index = usize::try_from(id).expect("u32 sparse id must fit usize");
        if sparse_index >= self.sparse.len() {
            let sparse_len = sparse_index
                .checked_add(1)
                .expect("sparse index length overflowed usize");
            self.sparse.resize(sparse_len, NIL);
        }
        let dense_index = self.dense.len() as u32;
        self.sparse[sparse_index] = dense_index;
        self.dense.push(id);
        self.values.push(value);
        None
    }

    /// Remove `id` in O(1) using swap-remove, returning its value when present.
    pub fn remove(&mut self, id: u32) -> Option<T> {
        let dense_index = self.dense_index(id)?;
        let last_id = *self.dense.last()?;
        self.sparse[id as usize] = NIL;
        self.dense.swap_remove(dense_index);
        let value = self.values.swap_remove(dense_index);
        if dense_index < self.dense.len() {
            self.sparse[last_id as usize] = dense_index as u32;
        }
        Some(value)
    }

    /// Borrow the value for `id`.
    pub fn get(&self, id: u32) -> Option<&T> {
        self.dense_index(id).map(|index| &self.values[index])
    }

    /// Mutably borrow the value for `id`.
    pub fn get_mut(&mut self, id: u32) -> Option<&mut T> {
        self.dense_index(id)
            .map(move |index| &mut self.values[index])
    }

    /// Whether `id` is live.
    pub fn contains(&self, id: u32) -> bool {
        self.dense_index(id).is_some()
    }

    /// Number of live entries.
    pub fn len(&self) -> usize {
        self.dense.len()
    }

    /// Whether there are no live entries.
    pub fn is_empty(&self) -> bool {
        self.dense.is_empty()
    }

    /// Remove every entry.  Dense capacity is retained for reuse.
    pub fn clear(&mut self) {
        self.sparse.clear();
        self.dense.clear();
        self.values.clear();
    }

    /// Borrow live values as one cache-contiguous slice.
    pub fn as_slice(&self) -> &[T] {
        &self.values
    }

    /// Mutably borrow live values as one cache-contiguous slice.
    pub fn as_mut_slice(&mut self) -> &mut [T] {
        &mut self.values
    }

    /// Borrow the dense id array parallel to [`Self::as_slice`].
    pub fn ids(&self) -> &[u32] {
        &self.dense
    }

    /// Iterate `(id, value)` pairs in dense storage order.
    pub fn iter(&self) -> impl Iterator<Item = (u32, &T)> {
        self.dense.iter().copied().zip(self.values.iter())
    }

    /// Mutably iterate `(id, value)` pairs in dense storage order.
    pub fn iter_mut(&mut self) -> impl Iterator<Item = (u32, &mut T)> {
        self.dense.iter().copied().zip(self.values.iter_mut())
    }

    fn dense_index(&self, id: u32) -> Option<usize> {
        let sparse_index = id as usize;
        let stored = *self.sparse.get(sparse_index)?;
        if stored == NIL {
            return None;
        }
        let dense_index = stored as usize;
        if dense_index >= self.dense.len() || self.dense[dense_index] != id {
            return None;
        }
        Some(dense_index)
    }
}

impl<T> Default for SparseSet<T> {
    fn default() -> Self {
        Self::new()
    }
}
