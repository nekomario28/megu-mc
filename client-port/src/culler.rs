// SPDX-License-Identifier: LGPL-3.0-or-later

//! Graph occlusion culling: a direction-monotonic breadth-first search
//! from the camera section across per-section connectivity masks, gated by the
//! conservative six-plane frustum test from `cull.rs`.
//!
//! Each loaded section supplies a [`SectionGraph`]: a 36-bit mask where bit
//! `entry * 6 + exit` is set when sight entering the section through face
//! `entry` can reach face `exit`. Uniform air sets every pair; a fully sealed
//! solid section sets none, so it is drawn when reached but never propagates
//! further. The search never exits a section through the face it entered
//! (no backtracking), and a node is only re-queued when it gains a previously
//! unseen entry face, so the walk is monotone and bounded by six bits per
//! section.

use std::collections::{HashMap, VecDeque};

use megu_core::render_cs::SectionVisBits;

use crate::cull::box_in_frustum;
use crate::visibility::FaceVisibility;
use crate::{Direction, SectionCoord, ALL_FACES, EDGE};

/// Bit `entry * 6 + exit` of a `SectionGraph::through` mask.
pub const fn through_bit(entry: Direction, exit: Direction) -> u64 {
    1 << (entry as usize * 6 + exit as usize)
}

/// All 36 entry/exit pairs; the `through` mask of a fully open section.
pub const THROUGH_ALL: u64 = (1 << 36) - 1;

/// Per-section connectivity input to the graph culler.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SectionGraph {
    /// Sight-reachability mask: bit `entry * 6 + exit` set means a ray entering
    /// through face `entry` can leave through face `exit` without crossing an
    /// occluding cell.
    pub through: u64,
    /// Whether the section emits geometry. Non-renderable sections still
    /// propagate sight but are never listed in `CullResult::visible`.
    pub renderable: bool,
}

impl SectionGraph {
    /// Fully open, nothing drawn: the uniform-air graph.
    pub const AIR: Self = Self {
        through: THROUGH_ALL,
        renderable: false,
    };

    /// Fully sealed shell: drawn when reached, never propagates.
    pub const SOLID: Self = Self {
        through: 0,
        renderable: true,
    };

    /// Whether sight entering through `entry` can exit through `exit`.
    pub const fn can_reach(self, entry: Direction, exit: Direction) -> bool {
        self.through & through_bit(entry, exit) != 0
    }

    /// The same connectivity as a [`SectionVisBits`] — the `FaceDir`-ordered
    /// portal matrix the core BFS and draw-bucket planners consume.
    pub fn vis_bits(self) -> SectionVisBits {
        FaceVisibility::from_bits(self.through).to_vis_bits()
    }
}

/// The adjacent section one `direction` step from `coord`, or `None` when the
/// coordinate would overflow.
fn step(coord: SectionCoord, direction: Direction) -> Option<SectionCoord> {
    let [dx, dy, dz] = direction.offset([0; 3]);
    Some(SectionCoord(
        coord.0.checked_add(dx)?,
        coord.1.checked_add(dy)?,
        coord.2.checked_add(dz)?,
    ))
}

/// Result of one [`GraphCuller::search`] pass.
#[derive(Clone, Debug, Default)]
pub struct CullResult {
    /// Renderable sections reached by the search, in BFS order.
    pub visible: Vec<SectionCoord>,
    /// Renderable input sections not in `visible`: every loaded draw section
    /// rejected by the frustum planes or occluded behind sealed sections.
    pub culled: usize,
}

/// Camera-frustum occlusion culler over a loaded section set.
pub struct GraphCuller {
    camera: SectionCoord,
    planes: Vec<[f32; 4]>,
}

impl GraphCuller {
    /// `camera` is the section containing the eye; `planes` are the six
    /// frustum planes in `box_in_frustum` form (`dot(n, p) + d >= 0` inside).
    pub fn new(camera: SectionCoord, planes: &[[f32; 4]]) -> Self {
        Self {
            camera,
            planes: planes.to_vec(),
        }
    }

    fn in_frustum(&self, coord: SectionCoord) -> bool {
        let min = [
            (coord.0 * EDGE as i32) as f32,
            (coord.1 * EDGE as i32) as f32,
            (coord.2 * EDGE as i32) as f32,
        ];
        box_in_frustum(min, &self.planes)
    }

    /// Direction-monotonic BFS. `sections` is the full loaded set; it both
    /// bounds the traversal (only listed sections exist) and provides the
    /// connectivity masks. The camera section is always a node even when it
    /// is not listed, in which case it is treated as `SectionGraph::AIR`.
    ///
    /// A candidate neighbor joins the frontier only when its section box
    /// passes the frustum test: off-frustum nodes are culled, not traversed.
    /// Within the frustum a node propagates only through exits reachable from
    /// one of its accumulated entry faces, never back out an entry face, so
    /// sections hidden behind sealed geometry stay unvisited and are counted
    /// in `CullResult::culled`.
    pub fn search(
        &self,
        sections: impl IntoIterator<Item = (SectionCoord, SectionGraph)>,
    ) -> CullResult {
        let map: HashMap<SectionCoord, SectionGraph> = sections.into_iter().collect();
        let mut result = CullResult::default();
        // Accumulated entry faces per visited node plus a processed flag in
        // bit 6; the camera starts saturated so no neighbor can re-queue it.
        // A node is enqueued once per newly gained entry face, so the flag
        // keeps `visible` free of duplicates on later pops.
        const PROCESSED: u8 = 1 << 6;
        let mut entries: HashMap<SectionCoord, u8> = HashMap::with_capacity(map.len());
        let mut queue = VecDeque::new();
        entries.insert(self.camera, ALL_FACES);
        queue.push_back(self.camera);
        while let Some(coord) = queue.pop_front() {
            let graph = map.get(&coord).copied().unwrap_or(SectionGraph::AIR);
            let entry = entries.get_mut(&coord).expect("queued node");
            if *entry & PROCESSED == 0 {
                *entry |= PROCESSED;
                if graph.renderable && self.in_frustum(coord) {
                    result.visible.push(coord);
                }
            }
            let entry_mask = *entry;
            let mut out = 0;
            for entry in Direction::ALL {
                if entry_mask & entry.bit() == 0 {
                    continue;
                }
                for exit in Direction::ALL {
                    if exit != entry && graph.can_reach(entry, exit) {
                        out |= exit.bit();
                    }
                }
            }
            if coord != self.camera {
                // Never backtrack out an entry face; the parent that supplied
                // it is already visited.
                out &= !entry_mask;
            }
            for exit in Direction::ALL {
                if out & exit.bit() == 0 {
                    continue;
                }
                let Some(next) = step(coord, exit) else {
                    continue;
                };
                let face = exit.opposite().bit();
                if !map.contains_key(&next)
                    || entries.get(&next).is_some_and(|mask| mask & face != 0)
                    || !self.in_frustum(next)
                {
                    continue;
                }
                *entries.entry(next).or_default() |= face;
                queue.push_back(next);
            }
        }
        result.culled = map
            .values()
            .filter(|graph| graph.renderable)
            .count()
            .saturating_sub(result.visible.len());
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Axis-aligned box frustum: inside `min ..= max` in world blocks.
    fn box_planes(min: [f32; 3], max: [f32; 3]) -> [[f32; 4]; 6] {
        [
            [1.0, 0.0, 0.0, -min[0]],
            [-1.0, 0.0, 0.0, max[0]],
            [0.0, 1.0, 0.0, -min[1]],
            [0.0, -1.0, 0.0, max[1]],
            [0.0, 0.0, 1.0, -min[2]],
            [0.0, 0.0, -1.0, max[2]],
        ]
    }

    fn open() -> SectionGraph {
        SectionGraph::AIR
    }

    /// Synthetic terrain + cave fixture: an 8x4x8 world (256 sections) with a
    /// three-section-thick solid floor carved by a small cave pocket, under an
    /// all-air top layer.
    fn terrain(cave: &[[i32; 3]]) -> HashMap<SectionCoord, SectionGraph> {
        let mut map = HashMap::new();
        for x in 0..8 {
            for z in 0..8 {
                for y in 0..4 {
                    let graph = match y {
                        3 => open(),
                        _ if cave.contains(&[x, y, z]) => open(),
                        _ => SectionGraph::SOLID,
                    };
                    map.insert(SectionCoord(x, y, z), graph);
                }
            }
        }
        map
    }

    #[test]
    fn culls_sixty_percent_of_synthetic_terrain_and_cave() {
        // A sealed 3x1x3 pocket inside the solid layer at y = 1.
        let mut cave = Vec::new();
        for x in 3..=5 {
            for z in 3..=5 {
                cave.push([x, 1, z]);
            }
        }
        let map = terrain(&cave);
        let camera = SectionCoord(4, 1, 4);
        // Half-space frustum: everything with z >= 2 sections is inside.
        let planes = box_planes(
            [-1024.0, -1024.0, 2.0 * EDGE as f32],
            [1024.0, 1024.0, 1024.0],
        );
        let result = GraphCuller::new(camera, &planes).search(map.iter().map(|(&c, &g)| (c, g)));

        // Exactly the 30 wall sections around the 3x1x3 pocket are drawn:
        // 9 below (y=0), 9 above (y=2), and 6+6 along the x/z sides.
        assert_eq!(result.visible.len(), 30);
        let renderable = map.values().filter(|g| g.renderable).count();
        assert_eq!(renderable, 183);
        assert_eq!(result.culled, renderable - result.visible.len());
        assert!(
            result.culled * 5 >= renderable * 3,
            "culled {}/{} < 60%",
            result.culled,
            renderable
        );
        // Pocket wall sections are drawn; deep solid is occluded.
        assert!(result.visible.contains(&SectionCoord(4, 0, 4)));
        assert!(result.visible.contains(&SectionCoord(4, 2, 4)));
        assert!(result.visible.contains(&SectionCoord(2, 1, 4)));
        assert!(!result.visible.contains(&SectionCoord(0, 0, 0)));
        // Diagonal corner solids touch the pocket only at an edge: occluded.
        assert!(!result.visible.contains(&SectionCoord(2, 1, 2)));
        // Air sections propagate but are never drawn.
        assert!(!result.visible.contains(&SectionCoord(4, 1, 4)));
        // The sealed air layer above the solid roof stays unreachable.
        assert!(!result.visible.iter().any(|c| c.1 == 3));
    }

    #[test]
    fn connectivity_mask_gates_exit_directions() {
        // North-south corridor: bits only for N->S and S->N travel.
        let mut corridor = SectionGraph::AIR;
        corridor.through = through_bit(Direction::North, Direction::South)
            | through_bit(Direction::South, Direction::North);
        let camera = SectionCoord(0, 0, 0);
        let map = [
            (camera, corridor),
            (SectionCoord(0, 0, 1), corridor),
            (SectionCoord(0, 0, 2), corridor),
            // In-frustum solid east of the corridor: reachable only if the
            // corridor leaked sideways.
            (SectionCoord(1, 0, 1), SectionGraph::SOLID),
            (SectionCoord(0, 0, 3), SectionGraph::SOLID),
        ]
        .into_iter()
        .collect::<HashMap<_, _>>();
        let planes = box_planes([-1024.0; 3], [1024.0; 3]);
        let result = GraphCuller::new(camera, &planes).search(map.iter().map(|(&c, &g)| (c, g)));
        assert!(result.visible.contains(&SectionCoord(0, 0, 3)));
        assert!(!result.visible.contains(&SectionCoord(1, 0, 1)));
        assert_eq!(result.culled, 1);
    }

    #[test]
    fn off_frustum_sections_are_not_traversed() {
        // Two arms from the camera; the z < 0 arm is entirely outside the
        // frustum, so the solid behind it is occluded as well as frustum-fail.
        let camera = SectionCoord(0, 0, 0);
        let map = [
            (camera, SectionGraph::AIR),
            (SectionCoord(0, 0, -1), SectionGraph::AIR),
            (SectionCoord(0, 0, -2), SectionGraph::SOLID),
            (SectionCoord(0, 0, 1), SectionGraph::SOLID),
        ]
        .into_iter()
        .collect::<HashMap<_, _>>();
        let planes = box_planes([-1024.0, -1024.0, 0.0], [1024.0; 3]);
        let result = GraphCuller::new(camera, &planes).search(map.iter().map(|(&c, &g)| (c, g)));
        assert_eq!(result.visible, [SectionCoord(0, 0, 1)]);
        assert_eq!(result.culled, 1);
    }

    #[test]
    fn multi_entry_sections_are_listed_once() {
        // (1,0,0) and (2,0,0) are open; the solid at (2,0,1) is reached
        // through both its West and North faces, so it gains two entry bits
        // and is enqueued twice — but must appear once in `visible`.
        let camera = SectionCoord(0, 0, 0);
        let solid = SectionCoord(2, 0, 1);
        let map = [
            (camera, SectionGraph::AIR),
            (SectionCoord(1, 0, 0), SectionGraph::AIR),
            (SectionCoord(2, 0, 0), SectionGraph::AIR),
            (solid, SectionGraph::SOLID),
        ]
        .into_iter()
        .collect::<HashMap<_, _>>();
        let planes = box_planes([-1024.0; 3], [1024.0; 3]);
        let result = GraphCuller::new(camera, &planes).search(map.iter().map(|(&c, &g)| (c, g)));
        assert_eq!(result.visible, [solid]);
        assert_eq!(result.culled, 0);
    }

    #[test]
    fn camera_inside_sealed_section_sees_nothing() {
        let camera = SectionCoord(0, 0, 0);
        let map = [
            (camera, SectionGraph::SOLID),
            (SectionCoord(1, 0, 0), SectionGraph::SOLID),
        ]
        .into_iter()
        .collect::<HashMap<_, _>>();
        let planes = box_planes([-1024.0; 3], [1024.0; 3]);
        let result = GraphCuller::new(camera, &planes).search(map.iter().map(|(&c, &g)| (c, g)));
        assert_eq!(result.visible, [camera]);
        assert_eq!(result.culled, 1);
    }
}
