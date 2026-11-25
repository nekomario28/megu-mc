// SPDX-License-Identifier: LGPL-3.0-or-later
#![forbid(unsafe_code)]

//! Zero-alloc retained UI tessellation cache.
//!
//! The window loop used to fingerprint one monolithic [`UiDrawList`]
//! per frame and re-tessellate *everything* whenever any pixel of UI
//! changed. [`UiRetainedCache`] replaces that with per-layer retained
//! geometry built directly on `megu-core`'s revision machinery:
//!
//! * [`UiLayerBus`](megu_core::rev::UiLayerBus) — one revision counter
//!   per [`UiLayer`]; the host bumps a layer only when that layer's
//!   draw list actually changed.
//! * [`RetainedLayerTracker`] — diffs the bus against the last bake;
//!   `poll` returning [`DirtyLayers::NONE`] means the frame does
//!   **zero** tessellation work.
//! * [`IncrementalDag`] — a 9-node graph (`7 layer inputs → composite →
//!   upload`). Dirty inputs poison the composite; the composite's
//!   [`recompute_with_cutoff`](megu_core::dep_graph::IncrementalDag::recompute_with_cutoff)
//!   rescues the upload node when the merged vertex stream comes out
//!   bit-identical, so even a content-identical re-bake costs no PCIe
//!   upload.
//!
//! Steady-state contract: every buffer is a retained `Vec` cleared and
//! refilled in place — idle frames perform 0 tessellations, 0 heap
//! allocations and 0 GPU buffer re-uploads.

use megu_core::dep_graph::{IncrementalDag, NodeId};
use megu_core::rev::{DirtyLayers, RetainedLayerTracker, UiLayer, UiLayerBus};

use crate::draw::{
    tessellate_draw_list_full_into, GuiSpriteAtlas, ItemIconAtlas, UiDrawList, UiVertex,
};

/// Number of retained layers — the 7 canonical [`UiLayer`]s.
pub const RETAINED_LAYER_COUNT: usize = UiLayer::COUNT;

/// FNV-1a over the exact bytes the GPU upload would carry — vertices
/// (f32 bits) then indices — so [`IncrementalDag`]'s early-cutoff only
/// ever rescues an upload when the merged stream is truly identical.
fn mesh_digest(vertices: &[UiVertex], indices: &[u32]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    let mut mix = |b: u64| {
        hash = (hash ^ b).wrapping_mul(0x0000_0100_0000_01b3);
    };
    mix(vertices.len() as u64);
    for v in vertices {
        mix(u64::from(v.position[0].to_bits()) << 32 | u64::from(v.position[1].to_bits()));
        mix(u64::from(v.uv[0].to_bits()) << 32 | u64::from(v.uv[1].to_bits()));
        for c in v.color {
            mix(u64::from(c.to_bits()));
        }
    }
    mix(indices.len() as u64);
    for &i in indices {
        mix(u64::from(i));
    }
    hash
}

/// Per-layer retained tessellation buffers. `clear()`-then-refill
/// keeps the warm capacity forever — the zero-alloc steady state.
#[derive(Default)]
struct LayerMesh {
    vertices: Vec<UiVertex>,
    indices: Vec<u32>,
    /// Vertex count at last tessellation — diagnostic for tests.
    len_at_bake: usize,
}

/// Lifetime stats proving the retained contract.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RetainedCacheStats {
    /// `begin_frame` calls that reported at least one dirty layer.
    pub dirty_frames: u64,
    /// `begin_frame` calls with `DirtyLayers::NONE` — pure idle frames.
    pub idle_frames: u64,
    /// Per-layer re-tessellations performed.
    pub tessellations: u64,
    /// Composite merges run.
    pub composites: u64,
    /// Frames where the composite digest matched the previous bake —
    /// the merged stream was identical, so no upload is needed even
    /// though a layer re-tessellated.
    pub uploads_rescued: u64,
}

/// The retained 7-layer UI tessellation cache — see module docs.
pub struct UiRetainedCache {
    /// Baked-revision tracker over the canonical bus.
    tracker: RetainedLayerTracker<8>,
    /// `layer[i]` (input) → `composite` → `upload` graph.
    dag: IncrementalDag,
    /// One input node per [`UiLayer::index`].
    layer_nodes: [NodeId; RETAINED_LAYER_COUNT],
    /// Merge node downstream of every layer input.
    composite_node: NodeId,
    /// Upload node downstream of `composite_node`; dirty ⇒ PCIe upload.
    upload_node: NodeId,
    /// Retained per-layer meshes.
    layers: [LayerMesh; RETAINED_LAYER_COUNT],
    /// Single merged batch the GPU uploads in one call.
    merged_vertices: Vec<UiVertex>,
    merged_indices: Vec<u32>,
    /// Scratch for `dirty_order_into` — retained, zero-alloc per frame.
    order_scratch: Vec<NodeId>,
    /// Dirty set drained by the in-flight `begin_frame`/`composite`
    /// pair (cleared at the next `begin_frame`).
    pending: DirtyLayers,
    /// Merged digest at last composite — `upload`'s settled digest.
    merged_digest: u64,
    /// `composite` already ran since `begin_frame` — guards the stats.
    composited: bool,
    stats: RetainedCacheStats,
}

impl Default for UiRetainedCache {
    fn default() -> Self {
        Self::new()
    }
}

impl UiRetainedCache {
    /// Fresh cache: every layer unbaked, first frame tessellates all.
    pub fn new() -> Self {
        let mut dag = IncrementalDag::with_capacity(RETAINED_LAYER_COUNT + 2);
        let layer_nodes: [NodeId; RETAINED_LAYER_COUNT] =
            std::array::from_fn(|_| dag.add_node(0));
        let composite_node = dag.add_node(0);
        let upload_node = dag.add_node(0);
        for &input in &layer_nodes {
            dag.add_edge(input, composite_node)
                .expect("fresh dag cannot cycle");
        }
        dag.add_edge(composite_node, upload_node)
            .expect("fresh dag cannot cycle");
        Self {
            tracker: RetainedLayerTracker::new(),
            dag,
            layer_nodes,
            composite_node,
            upload_node,
            layers: std::array::from_fn(|_| LayerMesh::default()),
            merged_vertices: Vec::new(),
            merged_indices: Vec::new(),
            order_scratch: Vec::new(),
            pending: DirtyLayers::NONE,
            merged_digest: 0,
            composited: false,
            stats: RetainedCacheStats::default(),
        }
    }

    /// Diff the bus against the last bake: returns the layers whose
    /// revision moved and poisons the DAG's composite/upload chain for
    /// each. `DirtyLayers::NONE` means the frame does **zero**
    /// tessellation and — unless the previous composite is still
    /// pending — zero GPU upload.
    ///
    /// Callers then feed each dirty layer's current [`UiDrawList`] to
    /// [`update_layer`](Self::update_layer) and finish with
    /// [`composite`](Self::composite).
    pub fn begin_frame(&mut self, bus: &UiLayerBus) -> DirtyLayers {
        let dirty = self.tracker.poll(bus);
        self.pending = dirty;
        self.composited = false;
        self.order_scratch.clear();
        if dirty.is_empty() {
            self.stats.idle_frames += 1;
        } else {
            self.stats.dirty_frames += 1;
            for index in dirty.iter() {
                // Index 7 is the bus's reserved headroom — no canonical
                // `UiLayer` lives there, so it maps to no input node.
                let Some(&input) = self.layer_nodes.get(index) else {
                    continue;
                };
                let rev = bus.rev(index);
                // Input commit: marks the input clean, poisons
                // composite + upload downstream.
                let _ = self.dag.mark_input_changed(input, rev.get());
            }
        }
        // Rebuild the topo-ordered work plan from the DAG's actual dirty
        // state, not from this frame's revision diff alone. A caller that
        // deferred the previous frame's composite still finds it owed here,
        // while a completed composite leaves no stale scratch entry that
        // would merge again on every idle frame.
        self.dag.dirty_order_into(&mut self.order_scratch);
        dirty
    }

    /// Re-tessellate `layer`'s draw list into the layer's *retained*
    /// buffers — capacity is reused, so steady-state rebuilds allocate
    /// nothing. Call once per layer in [`begin_frame`](Self::begin_frame)'s
    /// dirty set; calling for a clean layer is correct (idempotent) but
    /// wasted work.
    pub fn update_layer(
        &mut self,
        layer: UiLayer,
        list: &UiDrawList,
        viewport_w: u32,
        viewport_h: u32,
        icons: &ItemIconAtlas,
        sprites: &GuiSpriteAtlas,
    ) {
        let mesh = &mut self.layers[layer.index()];
        tessellate_draw_list_full_into(
            list,
            viewport_w,
            viewport_h,
            icons,
            sprites,
            &mut mesh.vertices,
            &mut mesh.indices,
        );
        mesh.len_at_bake = mesh.vertices.len();
        self.stats.tessellations += 1;
    }

    /// Merge the 7 retained layers into the single upload batch.
    ///
    /// Runs the DAG's composite node with the merged mesh's content
    /// digest: [`CutoffOutcome::EarlyCutoff`] (identical bytes) rescues
    /// the upload node — [`gpu_upload_needed`](Self::gpu_upload_needed)
    /// then reports `false` even though layers re-tessellated. When
    /// nothing was dirty this frame the merge is skipped entirely and
    /// the previous batch is returned.
    ///
    /// Returns `(vertices, indices)` — the buffers to upload iff
    /// `gpu_upload_needed()`.
    pub fn composite(&mut self) -> (&[UiVertex], &[u32]) {
        let merge_owed = self.order_scratch.contains(&self.composite_node);
        if !self.composited && merge_owed {
            self.merged_vertices.clear();
            self.merged_indices.clear();
            for layer in &self.layers {
                let base = self.merged_vertices.len() as u32;
                self.merged_vertices.extend_from_slice(&layer.vertices);
                self.merged_indices
                    .extend(layer.indices.iter().map(|i| i + base));
            }
            self.merged_digest = mesh_digest(&self.merged_vertices, &self.merged_indices);
            let outcome = self
                .dag
                .recompute_with_cutoff(self.composite_node, self.merged_digest);
            if outcome == megu_core::dep_graph::CutoffOutcome::EarlyCutoff {
                self.stats.uploads_rescued += 1;
            }
            self.stats.composites += 1;
            self.pending = DirtyLayers::NONE;
        }
        self.composited = true;
        (&self.merged_vertices, &self.merged_indices)
    }

    /// `true` when the merged batch differs from what the GPU was last
    /// told — the DAG's upload node is dirty. `false` on idle frames
    /// AND on identical re-bakes (early-cutoff rescue).
    pub fn gpu_upload_needed(&self) -> bool {
        self.dag.is_dirty(self.upload_node).unwrap_or(true)
    }

    /// Record the GPU upload as done — settles the upload node at the
    /// current merged digest.
    pub fn mark_uploaded(&mut self) {
        self.dag
            .recompute_with_cutoff(self.upload_node, self.merged_digest);
    }

    /// Drop every baked revision and buffer — the next `begin_frame`
    /// reports all layers dirty. For GUI-scale changes, atlas reloads
    /// and other events that invalidate every layer's geometry at once.
    pub fn reset(&mut self) {
        self.tracker.reset();
        for layer in &mut self.layers {
            layer.vertices.clear();
            layer.indices.clear();
            layer.len_at_bake = 0;
        }
        self.pending = DirtyLayers::NONE;
        self.composited = false;
        // Force composite + upload dirty so the first frame uploads.
        let _ = self
            .dag
            .mark_input_changed(self.composite_node, self.merged_digest);
    }

    /// Retained buffers for `layer` — test/diagnostic access.
    pub fn layer_mesh(&self, layer: UiLayer) -> (&[UiVertex], &[u32]) {
        let mesh = &self.layers[layer.index()];
        (&mesh.vertices, &mesh.indices)
    }

    /// Layers the in-flight frame still owes `update_layer` calls for.
    pub fn pending_dirty(&self) -> DirtyLayers {
        self.pending
    }

    /// Cache lifetime counters.
    pub fn stats(&self) -> RetainedCacheStats {
        self.stats
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::draw::{UiColor, UiRect};

    fn list_with_rect(x: i32) -> UiDrawList {
        let mut list = UiDrawList::new();
        list.fill_rect(UiRect::new(x, 0, 10, 10), UiColor::rgb(255, 255, 255));
        list
    }

    fn empty_icons() -> ItemIconAtlas {
        ItemIconAtlas::default()
    }

    fn empty_sprites() -> GuiSpriteAtlas {
        GuiSpriteAtlas::default()
    }

    #[test]
    fn deferred_composite_survives_an_idle_begin_frame_once() {
        let bus = UiLayerBus::new();
        let mut cache = UiRetainedCache::new();
        let icons = empty_icons();
        let sprites = empty_sprites();
        let dirty = cache.begin_frame(&bus);
        assert!(!dirty.is_empty());
        for layer in UiLayer::ALL {
            cache.update_layer(
                layer,
                &list_with_rect(layer.index() as i32),
                100,
                100,
                &icons,
                &sprites,
            );
        }

        // The next poll has no new revision diff, but the first frame's
        // composite is still genuinely owed by the DAG and must remain
        // reachable exactly once.
        assert_eq!(cache.begin_frame(&bus), DirtyLayers::NONE);
        let before = cache.stats().composites;
        let (vertices, indices) = cache.composite();
        assert!(!vertices.is_empty() && !indices.is_empty());
        assert_eq!(cache.stats().composites, before + 1);
        cache.mark_uploaded();

        assert_eq!(cache.begin_frame(&bus), DirtyLayers::NONE);
        cache.composite();
        assert_eq!(cache.stats().composites, before + 1);
    }

    #[test]
    fn idle_frame_tessellates_nothing_and_needs_no_upload() {
        let mut bus = UiLayerBus::new();
        let mut cache = UiRetainedCache::new();
        let icons = empty_icons();
        let sprites = empty_sprites();
        // First frame: everything dirty (never baked).
        let dirty = cache.begin_frame(&bus);
        assert!(!dirty.is_empty());
        for layer in UiLayer::ALL {
            cache.update_layer(layer, &list_with_rect(layer.index() as i32), 100, 100, &icons, &sprites);
        }
        let (v, i) = cache.composite();
        assert!(!v.is_empty() && !i.is_empty());
        assert!(cache.gpu_upload_needed());
        cache.mark_uploaded();
        assert!(!cache.gpu_upload_needed());
        // Idle frames: nothing bumped → nothing tessellated, no upload.
        for _ in 0..60 {
            let dirty = cache.begin_frame(&bus);
            assert_eq!(dirty, DirtyLayers::NONE);
            cache.composite();
            assert!(!cache.gpu_upload_needed());
        }
        let stats = cache.stats();
        assert_eq!(stats.tessellations, 7);
        let _ = &mut bus;
    }

    #[test]
    fn single_layer_bump_retessellates_only_that_layer() {
        let bus_rev = |bus: &UiLayerBus, l: UiLayer| bus.rev(l.index());
        let mut bus = UiLayerBus::new();
        let mut cache = UiRetainedCache::new();
        let icons = empty_icons();
        let sprites = empty_sprites();
        for layer in UiLayer::ALL {
            cache.update_layer(layer, &list_with_rect(0), 10, 10, &icons, &sprites);
        }
        cache.begin_frame(&bus);
        for layer in UiLayer::ALL {
            cache.update_layer(layer, &list_with_rect(1), 10, 10, &icons, &sprites);
        }
        cache.composite();
        cache.mark_uploaded();
        let before = cache.stats().tessellations;
        // Bump ONLY DynamicHud.
        UiLayer::DynamicHud.bump(&mut bus);
        let dirty = cache.begin_frame(&bus);
        assert!(dirty.contains(UiLayer::DynamicHud.index()));
        assert!(!dirty.contains(UiLayer::StaticHud.index()));
        assert!(!dirty.contains(UiLayer::Screen.index()));
        assert!(!dirty.contains(UiLayer::Overlay.index()));
        assert!(!dirty.contains(UiLayer::ModHud.index()));
        cache.update_layer(
            UiLayer::DynamicHud,
            &list_with_rect(5),
            10,
            10,
            &icons,
            &sprites,
        );
        cache.composite();
        assert_eq!(cache.stats().tessellations, before + 1);
        let _ = bus_rev;
    }

    #[test]
    fn identical_rebake_rescues_the_upload() {
        let mut bus = UiLayerBus::new();
        let mut cache = UiRetainedCache::new();
        let icons = empty_icons();
        let sprites = empty_sprites();
        let list = list_with_rect(3);
        for layer in UiLayer::ALL {
            cache.update_layer(layer, &list, 10, 10, &icons, &sprites);
        }
        cache.begin_frame(&bus);
        for layer in UiLayer::ALL {
            cache.update_layer(layer, &list, 10, 10, &icons, &sprites);
        }
        cache.composite();
        cache.mark_uploaded();
        // Bump a layer but rebake identical geometry → EarlyCutoff
        // rescues the upload node: no PCIe traffic for a no-op dirty.
        UiLayer::Overlay.bump(&mut bus);
        cache.begin_frame(&bus);
        cache.update_layer(UiLayer::Overlay, &list, 10, 10, &icons, &sprites);
        cache.composite();
        assert!(!cache.gpu_upload_needed());
        assert_eq!(cache.stats().uploads_rescued, 1);
    }
}
