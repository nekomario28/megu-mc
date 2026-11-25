// SPDX-License-Identifier: LGPL-3.0-or-later
#![forbid(unsafe_code)]
//! Tarjan disjoint-set union with path halving and union by rank.
//!
//! This is the shared connectivity primitive for kinetic networks, pipe and
//! cable graphs, multiblock structures, contiguous claims, and spatial-audio
//! rooms: all operations are amortized inverse-Ackermann, with no hashing or
//! external dependencies.

/// Disjoint-set union over dense `u32` element ids.
#[derive(Clone, Debug)]
pub struct DisjointSet {
    parent: Vec<u32>,
    rank: Vec<u8>,
    size: Vec<u32>,
    components: usize,
}

impl DisjointSet {
    /// Create `n` singleton elements numbered `0..n`.
    pub fn new(n: usize) -> Self {
        assert!(n <= u32::MAX as usize, "disjoint-set ids are u32");
        let n32 = n as u32;
        Self {
            parent: (0..n32).collect(),
            rank: vec![0; n],
            size: vec![1; n],
            components: n,
        }
    }

    /// Number of elements, including singleton components.
    pub fn len(&self) -> usize {
        self.parent.len()
    }

    /// Whether there are no elements.
    pub fn is_empty(&self) -> bool {
        self.parent.is_empty()
    }

    /// Append one singleton element and return its id.
    pub fn push(&mut self) -> u32 {
        let id = u32::try_from(self.parent.len()).expect("disjoint-set ids are u32");
        self.parent.push(id);
        self.rank.push(0);
        self.size.push(1);
        self.components += 1;
        id
    }

    /// Return the representative of `x`, halving the path as it goes.
    pub fn find(&mut self, x: u32) -> u32 {
        let mut x = usize::try_from(x).expect("element id fits usize");
        assert!(x < self.parent.len(), "disjoint-set id out of range");
        while self.parent[x] as usize != x {
            let parent = self.parent[x] as usize;
            let grandparent = self.parent[parent];
            self.parent[x] = grandparent;
            x = grandparent as usize;
        }
        x as u32
    }

    /// Merge the components containing `a` and `b`.
    ///
    /// Returns `true` when two previously distinct components were merged and
    /// `false` when they were already connected.
    pub fn union(&mut self, a: u32, b: u32) -> bool {
        let mut ra = self.find(a);
        let mut rb = self.find(b);
        if ra == rb {
            return false;
        }
        if self.rank[ra as usize] < self.rank[rb as usize] {
            std::mem::swap(&mut ra, &mut rb);
        }
        self.parent[rb as usize] = ra;
        self.size[ra as usize] += self.size[rb as usize];
        if self.rank[ra as usize] == self.rank[rb as usize] {
            self.rank[ra as usize] += 1;
        }
        self.components -= 1;
        true
    }

    /// Whether `a` and `b` currently belong to the same component.
    pub fn connected(&mut self, a: u32, b: u32) -> bool {
        self.find(a) == self.find(b)
    }

    /// Number of elements in the component containing `x`.
    pub fn component_size(&mut self, x: u32) -> u32 {
        let root = self.find(x);
        self.size[root as usize]
    }

    /// Number of live connected components.
    pub fn component_count(&self) -> usize {
        self.components
    }

    /// Restore every element to a singleton while retaining allocation capacity.
    pub fn reset(&mut self) {
        for (index, parent) in self.parent.iter_mut().enumerate() {
            *parent = index as u32;
        }
        self.rank.fill(0);
        self.size.fill(1);
        self.components = self.parent.len();
    }
}

impl Default for DisjointSet {
    fn default() -> Self {
        Self::new(0)
    }
}
