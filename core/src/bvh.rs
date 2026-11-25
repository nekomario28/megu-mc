// SPDX-License-Identifier: LGPL-3.0-or-later
#![forbid(unsafe_code)]
//! `megu_core::bvh` — a dynamic AABB tree (bounding volume hierarchy) in
//! the classic Box2D/Bullet incremental-SAH style.
//!
//! [`DynamicBvh`] stores payloads on leaves whose bounding boxes are
//! "fattened" by a configurable margin so a moving entity, Create
//! contraption, modded hitbox, or frustum/shadow-cull volume can jitter
//! inside its leaf without paying a remove+insert rebuild. Queries answer
//! against the payload's *tight* box, so results agree exactly with a
//! brute-force O(N) scan while pruning untouched subtrees in O(log N).
//!
//! Handles ([`BvhNodeId`]) stay stable across [`DynamicBvh::update`] even
//! when the leaf is re-inserted at a different tree position, and carry a
//! generation so a stale handle after `remove` can never alias a recycled
//! node.
//!
//! Pure `std`, zero dependencies, `unsafe`-free.

/// Axis-aligned bounding box in 3D, `min <= max` per axis.
///
/// `NaN` coordinates are rejected by construction helpers where practical;
/// query methods are written so a `NaN` bound simply fails the overlap
/// test rather than panicking or matching everything.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Aabb3 {
    /// Inclusive lower corner.
    pub min: [f32; 3],
    /// Inclusive upper corner.
    pub max: [f32; 3],
}

impl Aabb3 {
    /// Box with the given corners; component order is *not* normalized —
    /// callers wanting normalization use [`Aabb3::sorted`].
    pub const fn new(min: [f32; 3], max: [f32; 3]) -> Self {
        Self { min, max }
    }

    /// Box normalized so `min <= max` on every axis.
    pub fn sorted(a: [f32; 3], b: [f32; 3]) -> Self {
        Self {
            min: [a[0].min(b[0]), a[1].min(b[1]), a[2].min(b[2])],
            max: [a[0].max(b[0]), a[1].max(b[1]), a[2].max(b[2])],
        }
    }

    /// Degenerate box containing only `p`.
    pub const fn from_point(p: [f32; 3]) -> Self {
        Self { min: p, max: p }
    }

    /// Smallest box containing both `self` and `other`.
    pub fn union(&self, other: &Aabb3) -> Aabb3 {
        Aabb3 {
            min: [
                self.min[0].min(other.min[0]),
                self.min[1].min(other.min[1]),
                self.min[2].min(other.min[2]),
            ],
            max: [
                self.max[0].max(other.max[0]),
                self.max[1].max(other.max[1]),
                self.max[2].max(other.max[2]),
            ],
        }
    }

    /// Surface area `2(xy + yz + zx)` — the SAH cost metric. Degenerate
    /// (zero-extent) boxes have area 0.
    pub fn surface_area(&self) -> f32 {
        let dx = self.max[0] - self.min[0];
        let dy = self.max[1] - self.min[1];
        let dz = self.max[2] - self.min[2];
        // NaN extents propagate to NaN; callers treat NaN cost as "never
        // chosen" via f32::min semantics, which is the safe direction.
        2.0 * (dx * dy + dy * dz + dz * dx)
    }

    /// Whether two boxes overlap on all three axes (touching counts).
    /// Any `NaN` bound makes the comparison false — NaN-safe by
    /// construction.
    pub fn intersects(&self, other: &Aabb3) -> bool {
        self.min[0] <= other.max[0]
            && other.min[0] <= self.max[0]
            && self.min[1] <= other.max[1]
            && other.min[1] <= self.max[1]
            && self.min[2] <= other.max[2]
            && other.min[2] <= self.max[2]
    }

    /// Whether `p` lies inside the box (boundary inclusive).
    pub fn contains_point(&self, p: [f32; 3]) -> bool {
        p[0] >= self.min[0]
            && p[0] <= self.max[0]
            && p[1] >= self.min[1]
            && p[1] <= self.max[1]
            && p[2] >= self.min[2]
            && p[2] <= self.max[2]
    }

    /// Whether `other` is fully inside `self` (boundary inclusive).
    pub fn contains_aabb(&self, other: &Aabb3) -> bool {
        self.contains_point(other.min) && self.contains_point(other.max)
    }

    /// Box expanded by `margin` on every side.
    pub fn expanded(&self, margin: f32) -> Aabb3 {
        Aabb3 {
            min: [
                self.min[0] - margin,
                self.min[1] - margin,
                self.min[2] - margin,
            ],
            max: [
                self.max[0] + margin,
                self.max[1] + margin,
                self.max[2] + margin,
            ],
        }
    }

    /// Slab-test ray intersection. `inv_dir` is the componentwise
    /// `1.0 / dir` reciprocal (pass `f32::INFINITY` for zero components —
    /// the per-axis non-finite branch handles them exactly, so the test
    /// is NaN-safe). Returns entry distance `t >= 0` for a hit within
    /// `max_t`, else `None`.
    pub fn ray_intersects(
        &self,
        origin: [f32; 3],
        inv_dir: [f32; 3],
        max_t: f32,
    ) -> Option<f32> {
        let mut t_near = 0.0f32;
        let mut t_far = max_t;
        for axis in 0..3 {
            let (lo, hi) = (self.min[axis], self.max[axis]);
            if inv_dir[axis].is_finite() {
                let t1 = (lo - origin[axis]) * inv_dir[axis];
                let t2 = (hi - origin[axis]) * inv_dir[axis];
                // NaN bounds or a NaN origin must never report a hit —
                // f32::min/max would silently absorb the NaN.
                if t1.is_nan() || t2.is_nan() {
                    return None;
                }
                let (t1, t2) = if t1 <= t2 { (t1, t2) } else { (t2, t1) };
                t_near = t_near.max(t1);
                t_far = t_far.min(t2);
            } else if !(origin[axis] >= lo && origin[axis] <= hi) {
                // Direction ~0 on this axis: the ray travels parallel to
                // the slab, so the origin must already lie inside it.
                return None;
            }
            if t_near > t_far {
                return None;
            }
        }
        Some(t_near)
    }
}

/// Stable handle to a [`DynamicBvh`] leaf. The generation pins the exact
/// lifetime of the underlying slot: a handle captured before a `remove`
/// never aliases the recycled node.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct BvhNodeId {
    /// Slot index inside the tree.
    pub index: u32,
    /// Generation the slot had when this handle was issued.
    pub generation: u32,
}

const NIL: usize = usize::MAX;

enum Entry<T> {
    /// Tight bounds + user data. The node's `aabb` field holds the
    /// *fattened* box used for tree fit metrics.
    Leaf { tight: Aabb3, payload: T },
    Internal { children: [usize; 2] },
    Free { next: usize },
}

struct Node<T> {
    /// Fat box for leaves, union of children for internal nodes.
    aabb: Aabb3,
    parent: usize,
    /// 0 for leaves; `1 + max(child heights)` for internal nodes.
    height: u32,
    /// Bumped when the slot is recycled so stale `BvhNodeId`s miss.
    generation: u32,
    entry: Entry<T>,
}

/// Incremental-SAH dynamic AABB tree with fattened leaves.
///
/// * `insert` — greedy surface-area-heuristic sibling descent, then
///   bottom-up refit + rotation balancing: amortized O(log N).
/// * `remove` — collapses the leaf's parent, refits and rebalances:
///   amortized O(log N).
/// * `update` — keeps the leaf in place while the new tight box still
///   fits inside the fat one (returns `false`); otherwise unlinks and
///   re-inserts the *same* leaf node (returns `true`), so `BvhNodeId`s
///   survive moves.
/// * `query_aabb` / `query_ray` — caller-supplied scratch `Vec`s keep hot
///   loops allocation-free; payloads are filtered by *tight* bounds so
///   results equal a brute-force scan with zero false positives or
///   negatives.
pub struct DynamicBvh<T> {
    nodes: Vec<Node<T>>,
    root: usize,
    free_head: usize,
    len: usize,
    /// Absolute margin added on every side of a leaf's tight box.
    margin: f32,
}

impl<T> Default for DynamicBvh<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> DynamicBvh<T> {
    /// Empty tree with the default fattening margin (`0.05` world units).
    pub fn new() -> Self {
        Self::with_margin(0.05)
    }

    /// Empty tree with a custom fattening margin. Larger margins trade
    /// looser broad-phase culling for fewer re-insertions of jittery
    /// movers; `margin <= 0` degenerates to tight boxes (still correct).
    pub fn with_margin(margin: f32) -> Self {
        Self {
            nodes: Vec::new(),
            root: NIL,
            free_head: NIL,
            len: 0,
            margin: margin.max(0.0),
        }
    }

    /// Number of live leaves.
    pub fn len(&self) -> usize {
        self.len
    }

    /// Whether the tree holds no payloads.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Current fattening margin.
    pub fn margin(&self) -> f32 {
        self.margin
    }

    /// Insert `payload` bounded by `aabb`; returns its stable handle.
    pub fn insert(&mut self, aabb: Aabb3, payload: T) -> BvhNodeId {
        self.len += 1;
        let idx = self.alloc_node();
        let node = &mut self.nodes[idx];
        node.aabb = aabb.expanded(self.margin);
        node.height = 0;
        node.parent = NIL;
        node.entry = Entry::Leaf {
            tight: aabb,
            payload,
        };
        self.insert_leaf(idx);
        BvhNodeId {
            index: idx as u32,
            generation: self.nodes[idx].generation,
        }
    }

    /// Remove `id`, returning its payload; `None` for stale handles.
    pub fn remove(&mut self, id: BvhNodeId) -> Option<T> {
        if !self.is_live_leaf(id) {
            return None;
        }
        let idx = id.index as usize;
        self.unlink_leaf(idx);
        let entry = std::mem::replace(
            &mut self.nodes[idx].entry,
            Entry::Free {
                next: self.free_head,
            },
        );
        self.free_head = idx;
        self.nodes[idx].generation = self.nodes[idx].generation.wrapping_add(1);
        self.nodes[idx].height = u32::MAX;
        self.len -= 1;
        match entry {
            Entry::Leaf { payload, .. } => Some(payload),
            _ => unreachable!("live leaf check guarantees a leaf entry"),
        }
    }

    /// Move `id` to `new_aabb`. Returns `false` when the new box still
    /// fits the leaf's fat bounds (no tree work performed — the common
    /// case for sub-margin jitter) and `true` when the leaf was unlinked
    /// and re-inserted. `false` is also returned for stale handles;
    /// distinguish via [`DynamicBvh::get`] when it matters.
    pub fn update(&mut self, id: BvhNodeId, new_aabb: Aabb3) -> bool {
        if !self.is_live_leaf(id) {
            return false;
        }
        let idx = id.index as usize;
        if self.nodes[idx].aabb.contains_aabb(&new_aabb) {
            if let Entry::Leaf { tight, .. } = &mut self.nodes[idx].entry {
                *tight = new_aabb;
            }
            return false;
        }
        self.unlink_leaf(idx);
        self.nodes[idx].aabb = new_aabb.expanded(self.margin);
        if let Entry::Leaf { tight, .. } = &mut self.nodes[idx].entry {
            *tight = new_aabb;
        }
        self.insert_leaf(idx);
        true
    }

    /// Payload reference for a live handle.
    pub fn get(&self, id: BvhNodeId) -> Option<&T> {
        if !self.is_live_leaf(id) {
            return None;
        }
        match &self.nodes[id.index as usize].entry {
            Entry::Leaf { payload, .. } => Some(payload),
            _ => None,
        }
    }

    /// Exclusive payload reference for a live handle.
    pub fn get_mut(&mut self, id: BvhNodeId) -> Option<&mut T> {
        if !self.is_live_leaf(id) {
            return None;
        }
        match &mut self.nodes[id.index as usize].entry {
            Entry::Leaf { payload, .. } => Some(payload),
            _ => None,
        }
    }

    /// Tight (query-visible) bounds of a live leaf.
    pub fn tight_aabb(&self, id: BvhNodeId) -> Option<Aabb3> {
        if !self.is_live_leaf(id) {
            return None;
        }
        match &self.nodes[id.index as usize].entry {
            Entry::Leaf { tight, .. } => Some(*tight),
            _ => None,
        }
    }

    /// Append `(id, &payload)` of every leaf whose *tight* box overlaps
    /// `query`. Scratch `out` is appended to, not cleared — reuse it
    /// across frames for zero allocation.
    pub fn query_aabb<'a>(
        &'a self,
        query: Aabb3,
        out: &mut Vec<(BvhNodeId, &'a T)>,
    ) {
        if self.root == NIL {
            return;
        }
        let mut stack = vec![self.root];
        while let Some(idx) = stack.pop() {
            let node = &self.nodes[idx];
            if !node.aabb.intersects(&query) {
                continue;
            }
            match &node.entry {
                Entry::Internal { children } => {
                    stack.push(children[0]);
                    stack.push(children[1]);
                }
                Entry::Leaf { tight, payload } => {
                    // Tight filter: fat bounds are fit heuristic only, so
                    // results are bit-identical to a brute-force scan.
                    if tight.intersects(&query) {
                        out.push((self.id_of(idx), payload));
                    }
                }
                Entry::Free { .. } => unreachable!("free node reachable from root"),
            }
        }
    }

    /// Append `(id, entry_t, &payload)` of every leaf whose tight box is
    /// hit by `origin + t*dir` within `max_t`, sorted by ascending `t`
    /// (ties by handle). `dir` need not be normalized; per-axis zero
    /// components are handled exactly.
    pub fn query_ray<'a>(
        &'a self,
        origin: [f32; 3],
        dir: [f32; 3],
        max_t: f32,
        out: &mut Vec<(BvhNodeId, f32, &'a T)>,
    ) {
        if self.root == NIL {
            return;
        }
        let inv_dir = [1.0 / dir[0], 1.0 / dir[1], 1.0 / dir[2]];
        let base = out.len();
        let mut stack = vec![self.root];
        while let Some(idx) = stack.pop() {
            let node = &self.nodes[idx];
            if node.aabb.ray_intersects(origin, inv_dir, max_t).is_none() {
                continue;
            }
            match &node.entry {
                Entry::Internal { children } => {
                    stack.push(children[0]);
                    stack.push(children[1]);
                }
                Entry::Leaf { tight, payload } => {
                    if let Some(t) = tight.ray_intersects(origin, inv_dir, max_t) {
                        out.push((self.id_of(idx), t, payload));
                    }
                }
                Entry::Free { .. } => unreachable!("free node reachable from root"),
            }
        }
        // Deterministic nearest-first order independent of tree shape.
        out[base..].sort_by(|a, b| {
            a.1.partial_cmp(&b.1)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(a.0.cmp(&b.0))
        });
    }

    fn id_of(&self, idx: usize) -> BvhNodeId {
        BvhNodeId {
            index: idx as u32,
            generation: self.nodes[idx].generation,
        }
    }

    fn is_live_leaf(&self, id: BvhNodeId) -> bool {
        match self.nodes.get(id.index as usize) {
            Some(node) => {
                node.generation == id.generation && matches!(node.entry, Entry::Leaf { .. })
            }
            None => false,
        }
    }

    fn alloc_node(&mut self) -> usize {
        if self.free_head != NIL {
            let idx = self.free_head;
            let next = match self.nodes[idx].entry {
                Entry::Free { next } => next,
                _ => unreachable!("free list points at live node"),
            };
            self.free_head = next;
            idx
        } else {
            let idx = self.nodes.len();
            self.nodes.push(Node {
                aabb: Aabb3::from_point([0.0; 3]),
                parent: NIL,
                height: u32::MAX,
                generation: 0,
                entry: Entry::Free { next: NIL },
            });
            idx
        }
    }

    /// Attach leaf `idx` under the best-cost sibling and rebalance upward.
    fn insert_leaf(&mut self, leaf: usize) {
        if self.root == NIL {
            self.root = leaf;
            self.nodes[leaf].parent = NIL;
            return;
        }
        let leaf_aabb = self.nodes[leaf].aabb;

        // Greedy SAH descent: walk to the node where stopping costs less
        // than continuing into either child (Box2D b2DynamicTree).
        let mut sibling = self.root;
        while let Entry::Internal { children } = self.nodes[sibling].entry {
            let sibling_area = self.nodes[sibling].aabb.surface_area();
            let combined_area = self.nodes[sibling]
                .aabb
                .union(&leaf_aabb)
                .surface_area();
            let direct_cost = 2.0 * combined_area;
            let descend_cost = 2.0 * (combined_area - sibling_area);
            let (c0, c1) = (children[0], children[1]);
            let cost0 = self.child_cost(c0, &leaf_aabb, descend_cost);
            let cost1 = self.child_cost(c1, &leaf_aabb, descend_cost);
            if direct_cost < cost0 && direct_cost < cost1 {
                break;
            }
            sibling = if cost0 < cost1 { c0 } else { c1 };
        }

        // Splice a new internal parent between `sibling` and its parent.
        let old_parent = self.nodes[sibling].parent;
        let new_parent = self.alloc_node();
        let height = self.nodes[sibling].height + 1;
        let aabb = self.nodes[sibling].aabb.union(&leaf_aabb);
        self.nodes[new_parent] = Node {
            aabb,
            parent: old_parent,
            height,
            generation: self.nodes[new_parent].generation,
            entry: Entry::Internal {
                children: [sibling, leaf],
            },
        };
        self.nodes[sibling].parent = new_parent;
        self.nodes[leaf].parent = new_parent;
        if old_parent == NIL {
            self.root = new_parent;
        } else {
            match &mut self.nodes[old_parent].entry {
                Entry::Internal { children } => {
                    if children[0] == sibling {
                        children[0] = new_parent;
                    } else {
                        children[1] = new_parent;
                    }
                }
                _ => unreachable!("parent of a tree node is internal"),
            }
        }

        // Refit bounds/heights and rotate back to balance.
        let mut cursor = self.nodes[leaf].parent;
        while cursor != NIL {
            cursor = self.balance(cursor);
            let (c0, c1) = self.children_of(cursor);
            self.nodes[cursor].aabb =
                self.nodes[c0].aabb.union(&self.nodes[c1].aabb);
            self.nodes[cursor].height =
                1 + self.nodes[c0].height.max(self.nodes[c1].height);
            cursor = self.nodes[cursor].parent;
        }
    }

    /// Cost of making `child` the sibling of `leaf_aabb`, matching the
    /// Box2D heuristic: union area for leaves, area delta for internals.
    fn child_cost(&self, child: usize, leaf_aabb: &Aabb3, descend: f32) -> f32 {
        let node = &self.nodes[child];
        let combined = node.aabb.union(leaf_aabb).surface_area();
        if matches!(node.entry, Entry::Leaf { .. }) {
            combined + descend
        } else {
            (combined - node.aabb.surface_area()) + descend
        }
    }

    /// Detach leaf `idx`: its parent's other child is adopted by the
    /// grandparent (or becomes root), then the parent is freed and the
    /// chain refit + rebalanced. The leaf node itself is left allocated
    /// so `update` can re-insert it under the same handle.
    fn unlink_leaf(&mut self, leaf: usize) {
        if leaf == self.root {
            self.root = NIL;
            self.nodes[leaf].parent = NIL;
            return;
        }
        let parent = self.nodes[leaf].parent;
        let grand = self.nodes[parent].parent;
        let sibling = {
            let (c0, c1) = self.children_of(parent);
            if c0 == leaf {
                c1
            } else {
                c0
            }
        };
        if grand == NIL {
            self.root = sibling;
            self.nodes[sibling].parent = NIL;
        } else {
            match &mut self.nodes[grand].entry {
                Entry::Internal { children } => {
                    if children[0] == parent {
                        children[0] = sibling;
                    } else {
                        children[1] = sibling;
                    }
                }
                _ => unreachable!("grandparent of a leaf is internal"),
            }
            self.nodes[sibling].parent = grand;
            // Refit + rebalance up from the grandparent.
            let mut cursor = grand;
            while cursor != NIL {
                cursor = self.balance(cursor);
                let (c0, c1) = self.children_of(cursor);
                self.nodes[cursor].aabb =
                    self.nodes[c0].aabb.union(&self.nodes[c1].aabb);
                self.nodes[cursor].height =
                    1 + self.nodes[c0].height.max(self.nodes[c1].height);
                cursor = self.nodes[cursor].parent;
            }
        }
        // Recycle the now-detached internal parent node.
        let generation = self.nodes[parent].generation.wrapping_add(1);
        self.nodes[parent] = Node {
            aabb: Aabb3::from_point([0.0; 3]),
            parent: NIL,
            height: u32::MAX,
            generation,
            entry: Entry::Free {
                next: self.free_head,
            },
        };
        self.free_head = parent;
        self.nodes[leaf].parent = NIL;
    }

    fn children_of(&self, idx: usize) -> (usize, usize) {
        match &self.nodes[idx].entry {
            Entry::Internal { children } => (children[0], children[1]),
            _ => unreachable!("internal entry expected"),
        }
    }

    /// Rotate `a`'s subtree back toward balance; returns the node index
    /// now occupying `a`'s slot in the parent chain (Box2D `Balance`).
    fn balance(&mut self, a: usize) -> usize {
        if matches!(self.nodes[a].entry, Entry::Leaf { .. })
            || self.nodes[a].height < 2
        {
            return a;
        }
        let (b, c) = self.children_of(a);
        let diff = self.nodes[c].height as i64 - self.nodes[b].height as i64;
        if diff > 1 {
            self.rotate(a, c, b)
        } else if diff < -1 {
            self.rotate(a, b, c)
        } else {
            a
        }
    }

    /// Lift `tall` (the deeper child of `a`) above `a`, re-pairing `a`
    /// with whichever of `tall`'s children yields the smaller combined
    /// surface area. `other` is `a`'s shallower child.
    fn rotate(&mut self, a: usize, tall: usize, other: usize) -> usize {
        let (d, e) = self.children_of(tall);
        let a_parent = self.nodes[a].parent;

        // Rewire `tall` to take `a`'s place.
        self.nodes[tall].parent = a_parent;
        if a_parent == NIL {
            self.root = tall;
        } else {
            match &mut self.nodes[a_parent].entry {
                Entry::Internal { children } => {
                    if children[0] == a {
                        children[0] = tall;
                    } else {
                        children[1] = tall;
                    }
                }
                _ => unreachable!("parent of a tree node is internal"),
            }
        }

        // Pair by height exactly as b2DynamicTree does: the taller
        // grandchild stays under `tall`, the shorter one moves under `a`.
        // This is what bounds the post-rotation height difference at 1.
        let moved = if self.nodes[d].height >= self.nodes[e].height {
            e
        } else {
            d
        };

        match &mut self.nodes[tall].entry {
            Entry::Internal { children } => {
                // `a` inherits the slot `moved` vacated; `keep` is
                // untouched.
                if moved == children[0] {
                    children[0] = a;
                } else {
                    children[1] = a;
                }
            }
            _ => unreachable!("tall child of an internal node is internal"),
        }
        self.nodes[a].entry = Entry::Internal {
            children: [other, moved],
        };
        self.nodes[other].parent = a;
        self.nodes[moved].parent = a;
        self.nodes[a].parent = tall;

        // Refit both subtrees bottom-up.
        let (ac0, ac1) = self.children_of(a);
        self.nodes[a].aabb = self.nodes[ac0].aabb.union(&self.nodes[ac1].aabb);
        self.nodes[a].height = 1 + self.nodes[ac0].height.max(self.nodes[ac1].height);
        let (tc0, tc1) = self.children_of(tall);
        self.nodes[tall].aabb = self.nodes[tc0].aabb.union(&self.nodes[tc1].aabb);
        self.nodes[tall].height = 1 + self.nodes[tc0].height.max(self.nodes[tc1].height);
        tall
    }
}
