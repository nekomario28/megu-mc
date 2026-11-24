// SPDX-License-Identifier: LGPL-3.0-or-later
// SERVER-CUSTOM-PORTAL-ENGINE: a pure, deterministic, zero-hardcoding
// custom-shaped portal solver. `detect_custom_portal_shape` runs a bounded
// 4-neighbor flood fill inside a portal plane so non-rectangular frames
// (circle, arch, heart, L-shape, modded floor portals) ignite exactly like
// the Vanilla 2x3 Nether rectangle, while `collect_connected_portal_cells`
// extinguishes every connected interior tile cleanly when any frame block
// breaks. `PortalLinkSpec` + `compute_safe_gravity_flip_landing` describe a
// generic dimension link — including one-way End-style portals whose
// gravity-flip landing is mathematically clamped so the player cannot
// overshoot the obsidian platform into the Void.
//
// Everything here is pure `std` and allocation-bounded: the world reaches in
// through caller-supplied predicates, every flood fill is capped by
// `max_area`/`max_span`, and no clock, no locks, no mod IDs are hardcoded —
// link identity is data (`PortalLinkSpec::id`, `frame_block_ids`,
// `frame_tag`).

use std::collections::{BTreeSet, HashSet, VecDeque};

/// Tuning for the custom-shaped portal solver. `enabled == false` is strict
/// Vanilla rectangle parity: the engine ignores every non-rectangular shape.
#[derive(Clone, Debug, PartialEq)]
pub struct CustomPortalConfig {
    /// Master switch. `false` disables custom-shape detection entirely.
    pub enabled: bool,
    /// Admit crying obsidian (and tag-matched equivalents) as frame blocks.
    pub allow_crying_obsidian: bool,
    /// Admit `PortalPlane::HorizontalY` floor/ceiling portals (Twilight
    /// Forest, Aether, custom `.mmod` floor portals).
    pub allow_horizontal_floor_portals: bool,
    /// Maximum bounding-box extent along either in-plane axis, clamped
    /// `3..=48` at query time.
    pub max_span: u32,
    /// Maximum interior cell count, clamped `6..=1024` at query time.
    pub max_area: usize,
    /// Minimum interior cell count, clamped `1..=64` at query time. The
    /// default `6` matches the Vanilla 2x3 = 6 interior minimum.
    pub min_area: usize,
}

impl Default for CustomPortalConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            allow_crying_obsidian: true,
            allow_horizontal_floor_portals: true,
            max_span: 23,
            max_area: 441,
            min_area: 6,
        }
    }
}

/// The 2D plane a portal shape lives in. The third axis is fixed for every
/// cell of the shape.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PortalPlane {
    /// Spans `(dx, dy)` in the XY plane at fixed `z` (Vanilla `axis=x`).
    VerticalX,
    /// Spans `(dz, dy)` in the ZY plane at fixed `x` (Vanilla `axis=z`).
    VerticalZ,
    /// Spans `(dx, dz)` in the XZ plane at fixed `y` (End / mod floor portals).
    HorizontalY,
}

impl PortalPlane {
    /// Indices into `[i32; 3]` of the two axes that vary inside the plane;
    /// the remaining index is the fixed axis.
    fn axes(self) -> (usize, usize, usize) {
        match self {
            PortalPlane::VerticalX => (0, 1, 2),
            PortalPlane::VerticalZ => (2, 1, 0),
            PortalPlane::HorizontalY => (0, 2, 1),
        }
    }

    /// The four 4-neighbor offsets inside the plane.
    fn neighbor_offsets(self) -> [[i32; 3]; 4] {
        let (a, b, _) = self.axes();
        let mut offsets = [[0i32; 3]; 4];
        offsets[0][a] = 1;
        offsets[1][a] = -1;
        offsets[2][b] = 1;
        offsets[3][b] = -1;
        offsets
    }
}

/// A closed custom portal shape found by the bounded flood fill.
#[derive(Clone, Debug, PartialEq)]
pub struct DetectedPortalShape {
    pub plane: PortalPlane,
    /// Interior (air/portal-tile) cells, deterministically sorted.
    pub interior: Vec<[i32; 3]>,
    /// Unique enclosing boundary cells, deterministically sorted.
    pub frame_cells: Vec<[i32; 3]>,
    pub min_corner: [i32; 3],
    pub max_corner: [i32; 3],
}

fn sanitize_finite(v: f64) -> f64 {
    if v.is_finite() {
        v
    } else {
        0.0
    }
}

/// Bounded 4-neighbor flood fill in `plane` starting at `origin`.
///
/// Every 4-neighbor of an interior cell must be either another interior
/// replaceable cell or a valid frame cell; any other neighbor (open cave,
/// dirt wall, unclosed frame) aborts with `None` immediately. The fill is
/// capped by `max_area` interior cells and `max_span` bounding-box extent on
/// either in-plane axis, so it can never run away. Returns `None` when the
/// origin is not interior-replaceable, when the shape is unclosed, oversized,
/// or smaller than `min_area` / 2 cells on both axes.
pub fn detect_custom_portal_shape<FE, FF>(
    origin: [i32; 3],
    plane: PortalPlane,
    cfg: &CustomPortalConfig,
    mut is_interior_replaceable: FE,
    mut is_frame_block: FF,
) -> Option<DetectedPortalShape>
where
    FE: FnMut(i32, i32, i32) -> bool,
    FF: FnMut(i32, i32, i32) -> bool,
{
    if !is_interior_replaceable(origin[0], origin[1], origin[2]) {
        return None;
    }
    let max_area = cfg.max_area.clamp(6, 1024);
    let max_span = cfg.max_span.clamp(3, 48) as i32;
    let min_area = cfg.min_area.clamp(1, 64);
    let (a, b, _) = plane.axes();
    let offsets = plane.neighbor_offsets();

    let mut interior: Vec<[i32; 3]> = Vec::new();
    let mut visited: HashSet<[i32; 3]> = HashSet::new();
    let mut frame: BTreeSet<[i32; 3]> = BTreeSet::new();
    let mut queue: VecDeque<[i32; 3]> = VecDeque::new();
    let mut min = origin;
    let mut max = origin;

    visited.insert(origin);
    interior.push(origin);
    queue.push_back(origin);

    while let Some(cell) = queue.pop_front() {
        for off in offsets {
            let n = [cell[0] + off[0], cell[1] + off[1], cell[2] + off[2]];
            if visited.contains(&n) {
                continue;
            }
            if is_interior_replaceable(n[0], n[1], n[2]) {
                visited.insert(n);
                interior.push(n);
                if interior.len() > max_area {
                    return None;
                }
                for axis in [a, b] {
                    min[axis] = min[axis].min(n[axis]);
                    max[axis] = max[axis].max(n[axis]);
                }
                if max[a] - min[a] + 1 > max_span || max[b] - min[b] + 1 > max_span {
                    return None;
                }
                queue.push_back(n);
            } else if is_frame_block(n[0], n[1], n[2]) {
                frame.insert(n);
            } else {
                return None;
            }
        }
    }

    if interior.len() < min_area {
        return None;
    }
    if max[a] - min[a] + 1 < 2 || max[b] - min[b] + 1 < 2 {
        return None;
    }

    interior.sort_unstable();
    let mut min_corner = [0i32; 3];
    let mut max_corner = [0i32; 3];
    for axis in 0..3 {
        min_corner[axis] = interior.iter().map(|c| c[axis]).min().unwrap_or(0);
        max_corner[axis] = interior.iter().map(|c| c[axis]).max().unwrap_or(0);
    }
    Some(DetectedPortalShape {
        plane,
        interior,
        frame_cells: frame.into_iter().collect(),
        min_corner,
        max_corner,
    })
}

/// Try every enabled plane at `origin` and return the first closed custom
/// portal shape: `VerticalX`, then `VerticalZ`, then `HorizontalY` (only
/// when `cfg.allow_horizontal_floor_portals`).
pub fn detect_any_custom_portal<FE, FF>(
    origin: [i32; 3],
    cfg: &CustomPortalConfig,
    is_interior_replaceable: FE,
    is_frame_block: FF,
) -> Option<DetectedPortalShape>
where
    FE: FnMut(i32, i32, i32) -> bool + Copy,
    FF: FnMut(i32, i32, i32) -> bool + Copy,
{
    let mut planes = vec![PortalPlane::VerticalX, PortalPlane::VerticalZ];
    if cfg.allow_horizontal_floor_portals {
        planes.push(PortalPlane::HorizontalY);
    }
    for plane in planes {
        if let Some(shape) =
            detect_custom_portal_shape(origin, plane, cfg, is_interior_replaceable, is_frame_block)
        {
            return Some(shape);
        }
    }
    None
}

/// Bounded 4-neighbor BFS in `plane` collecting every connected portal
/// interior cell reachable from `start`. Used when a frame block or portal
/// tile breaks so custom shapes extinguish with zero floating tiles. The
/// fill stops after `max_cells` cells and the result is deterministically
/// sorted.
pub fn collect_connected_portal_cells<FP>(
    start: [i32; 3],
    plane: PortalPlane,
    max_cells: usize,
    mut is_portal_block: FP,
) -> Vec<[i32; 3]>
where
    FP: FnMut(i32, i32, i32) -> bool,
{
    let mut cells = Vec::new();
    if !is_portal_block(start[0], start[1], start[2]) || max_cells == 0 {
        return cells;
    }
    let mut visited: HashSet<[i32; 3]> = HashSet::new();
    let mut queue: VecDeque<[i32; 3]> = VecDeque::new();
    visited.insert(start);
    queue.push_back(start);
    while let Some(cell) = queue.pop_front() {
        if cells.len() >= max_cells {
            break;
        }
        cells.push(cell);
        for off in plane.neighbor_offsets() {
            let n = [cell[0] + off[0], cell[1] + off[1], cell[2] + off[2]];
            if visited.contains(&n) {
                continue;
            }
            if is_portal_block(n[0], n[1], n[2]) {
                visited.insert(n);
                queue.push_back(n);
            }
        }
    }
    cells.sort_unstable();
    cells
}

/// How a portal pair transitions the player between dimensions.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PortalTransitionStyle {
    /// Ordinary bidirectional seam, no special handling.
    BidirectionalSeamless,
    /// One-way entry that flips gravity and lands on a safe platform.
    OneWayGravityFlipToPlatform,
    /// One-way entry mirrored across the portal plane.
    HorizontalMirror,
}

/// A universal, mod-agnostic description of a portal link. All identity is
/// data — no mod IDs are hardcoded anywhere in this crate.
#[derive(Clone, Debug, PartialEq)]
pub struct PortalLinkSpec {
    /// Link identifier, e.g. `"minecraft:nether_portal"`,
    /// `"minecraft:end_portal"`, `"mymod:sky_portal"`.
    pub id: String,
    pub target_dim_id: String,
    /// Explicit frame block ids admitted as portal frame.
    pub frame_block_ids: Vec<String>,
    /// Optional block tag whose members are also admitted as frame.
    pub frame_tag: Option<String>,
    /// Whether non-rectangular custom shapes ignite for this link.
    pub allow_custom_shapes: bool,
    /// One-way links close the rift after entry and never return.
    pub one_way: bool,
    pub transition_style: PortalTransitionStyle,
    /// Half-extent of the safe landing platform in blocks (e.g. `2.0` for
    /// the End's 5x5 obsidian platform).
    pub safe_landing_platform_half_extent: f64,
}

/// The deterministic landing plan for a gravity-flip portal entry.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GravityFlipLandingPlan {
    /// Feet position guaranteed centered and safely above the platform.
    pub landing_pos: [f64; 3],
    /// Entry velocity after sanitization and horizontal clamping, in
    /// blocks/tick. Horizontal speed can never carry the player off the
    /// platform footprint into the Void.
    pub clamped_vel: [f64; 3],
    /// Smooth 180-degree camera/gravity transition duration.
    pub flip_duration_ms: u16,
    /// Safe-landing cushion before fall damage resumes.
    pub fall_damage_immunity_ticks: u16,
    /// Air blocks cleared above the platform to prevent suffocation.
    pub clear_headroom_blocks: u8,
    /// One-way portals seal the entry rift behind the player.
    pub close_rift_after_entry: bool,
}

/// Compute a void-proof landing plan for a gravity-flip portal entry.
///
/// Non-finite inputs (`NaN`/`Inf`) sanitize to `0.0`. `landing_pos` is always
/// `[platform_center.x, platform_center.y + 2.25, platform_center.z]`.
/// Horizontal speed is clamped to at most
/// `(platform_half_extent * 0.35).clamp(0.05, 0.45)` blocks/tick preserving
/// direction, and vertical velocity becomes a gentle `-0.12` descent, so the
/// landing trajectory cannot exit the platform's safe footprint.
pub fn compute_safe_gravity_flip_landing(
    entry_vel: [f64; 3],
    platform_center: [f64; 3],
    platform_half_extent: f64,
    one_way: bool,
) -> GravityFlipLandingPlan {
    let vx = sanitize_finite(entry_vel[0]);
    let vy = sanitize_finite(entry_vel[1]);
    let vz = sanitize_finite(entry_vel[2]);
    let cx = sanitize_finite(platform_center[0]);
    let cy = sanitize_finite(platform_center[1]);
    let cz = sanitize_finite(platform_center[2]);
    let half = if platform_half_extent.is_finite() && platform_half_extent > 0.0 {
        platform_half_extent
    } else {
        1.0
    };

    let speed = vx.hypot(vz);
    let max_speed = (half * 0.35).clamp(0.05, 0.45);
    let scale = if speed > max_speed && speed > 0.0 {
        max_speed / speed
    } else {
        1.0
    };
    let clamped_vx = vx * scale;
    let clamped_vz = vz * scale;
    let clamped_vy = if vy < -0.12 { -0.12 } else { vy.min(0.0) };

    GravityFlipLandingPlan {
        landing_pos: [cx, cy + 2.25, cz],
        clamped_vel: [clamped_vx, clamped_vy, clamped_vz],
        flip_duration_ms: 450,
        fall_damage_immunity_ticks: 20,
        clear_headroom_blocks: 3,
        close_rift_after_entry: one_way,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[derive(Clone, Copy, PartialEq, Eq)]
    enum TestBlock {
        Air,
        Obsidian,
        CryingObsidian,
        Dirt,
    }

    /// A sparse block world; every unlisted cell reads as `Dirt` (solid,
    /// neither interior-replaceable nor frame) so floods fail closed.
    struct TestWorld {
        blocks: HashMap<[i32; 3], TestBlock>,
    }

    impl TestWorld {
        fn new() -> Self {
            Self {
                blocks: HashMap::new(),
            }
        }

        fn set(&mut self, x: i32, y: i32, z: i32, b: TestBlock) {
            self.blocks.insert([x, y, z], b);
        }

        fn interior(&self, x: i32, y: i32, z: i32) -> bool {
            self.blocks.get(&[x, y, z]) == Some(&TestBlock::Air)
        }

        fn frame(&self, x: i32, y: i32, z: i32) -> bool {
            matches!(
                self.blocks.get(&[x, y, z]),
                Some(&TestBlock::Obsidian) | Some(&TestBlock::CryingObsidian)
            )
        }

        fn is_replaceable(&self, x: i32, y: i32, z: i32) -> bool {
            self.interior(x, y, z)
        }

        fn is_frame(&self, x: i32, y: i32, z: i32) -> bool {
            self.frame(x, y, z)
        }
    }

    /// Map a list of 2D `(u, v)` interior cells into 3D on `plane` with the
    /// fixed axis at `fixed_coord`, then ring the interior with obsidian
    /// (every 4-neighbor in the plane that is not interior). `cry_every`
    /// optionally upgrades every Nth frame cell to crying obsidian.
    fn build_framed_shape(
        plane: PortalPlane,
        cells2d: &[(i32, i32)],
        fixed_coord: i32,
        world: &mut TestWorld,
        cry_every: usize,
    ) {
        let (a, b, fixed) = plane.axes();
        let interior: HashSet<(i32, i32)> = cells2d.iter().copied().collect();
        let mut frame2d: BTreeSet<(i32, i32)> = BTreeSet::new();
        for &(u, v) in cells2d {
            for (du, dv) in [(1, 0), (-1, 0), (0, 1), (0, -1)] {
                let n = (u + du, v + dv);
                if !interior.contains(&n) {
                    frame2d.insert(n);
                }
            }
        }
        let mut idx = 0usize;
        for &(u, v) in cells2d {
            let mut c = [0i32; 3];
            c[a] = u;
            c[b] = v;
            c[fixed] = fixed_coord;
            world.set(c[0], c[1], c[2], TestBlock::Air);
        }
        for (u, v) in frame2d {
            let mut c = [0i32; 3];
            c[a] = u;
            c[b] = v;
            c[fixed] = fixed_coord;
            let b = if cry_every > 0 && idx.is_multiple_of(cry_every) {
                TestBlock::CryingObsidian
            } else {
                TestBlock::Obsidian
            };
            idx += 1;
            world.set(c[0], c[1], c[2], b);
        }
    }

    fn vanilla_config() -> CustomPortalConfig {
        CustomPortalConfig {
            enabled: true,
            ..CustomPortalConfig::default()
        }
    }

    fn rect_cells(w: i32, h: i32) -> Vec<(i32, i32)> {
        let mut cells = Vec::new();
        for x in 0..w {
            for y in 0..h {
                cells.push((x, y));
            }
        }
        cells
    }

    #[test]
    fn vanilla_2x3_rectangle_ignites() {
        let mut world = TestWorld::new();
        build_framed_shape(
            PortalPlane::VerticalX,
            &rect_cells(2, 3),
            0,
            &mut world,
            0,
        );
        let origin = [0, 0, 0];
        let shape = detect_custom_portal_shape(
            origin,
            PortalPlane::VerticalX,
            &vanilla_config(),
            |x, y, z| world.is_replaceable(x, y, z),
            |x, y, z| world.is_frame(x, y, z),
        )
        .expect("vanilla rectangle must ignite");
        assert_eq!(shape.plane, PortalPlane::VerticalX);
        assert_eq!(shape.interior.len(), 6);
        assert_eq!(shape.min_corner, [0, 0, 0]);
        assert_eq!(shape.max_corner, [1, 2, 0]);
        assert!(!shape.frame_cells.is_empty());
        // Deterministic ordering.
        let mut sorted = shape.interior.clone();
        sorted.sort_unstable();
        assert_eq!(shape.interior, sorted);
    }

    #[test]
    fn custom_shapes_circle_arch_heart_lshape_ignite() {
        let cfg = vanilla_config();
        type ShapeCase = (PortalPlane, Vec<(i32, i32)>, i32);
        let shapes: Vec<ShapeCase> = vec![
            // Circle-ish disc, radius^2 <= 6, on VerticalZ.
            (
                PortalPlane::VerticalZ,
                {
                    let mut c = Vec::new();
                    for x in -2..=2 {
                        for y in -2..=2 {
                            if x * x + y * y <= 6 {
                                c.push((x, y));
                            }
                        }
                    }
                    c
                },
                4,
            ),
            // Arch: two pillars joined by a lintel.
            (
                PortalPlane::VerticalX,
                vec![
                    (0, 0),
                    (0, 1),
                    (0, 2),
                    (1, 2),
                    (2, 2),
                    (3, 2),
                    (4, 2),
                    (4, 0),
                    (4, 1),
                ],
                -2,
            ),
            // Heart on VerticalX with mixed crying obsidian frame.
            (
                PortalPlane::VerticalX,
                vec![
                    (1, 0),
                    (2, 0),
                    (4, 0),
                    (5, 0),
                    (0, 1),
                    (1, 1),
                    (2, 1),
                    (3, 1),
                    (4, 1),
                    (5, 1),
                    (6, 1),
                    (0, 2),
                    (1, 2),
                    (2, 2),
                    (3, 2),
                    (4, 2),
                    (5, 2),
                    (6, 2),
                    (1, 3),
                    (2, 3),
                    (3, 3),
                    (4, 3),
                    (5, 3),
                    (2, 4),
                    (3, 4),
                    (4, 4),
                    (3, 5),
                ],
                7,
            ),
            // L-shape on VerticalZ (7 cells, meets min_area 6).
            (
                PortalPlane::VerticalZ,
                vec![
                    (0, 0),
                    (0, 1),
                    (0, 2),
                    (0, 3),
                    (1, 0),
                    (2, 0),
                    (3, 0),
                ],
                9,
            ),
        ];
        let expected_counts = [21usize, 9, 27, 7];
        for (idx, (plane, cells, fixed)) in shapes.into_iter().enumerate() {
            let mut world = TestWorld::new();
            build_framed_shape(plane, &cells, fixed, &mut world, 3);
            let origin2d = cells[0];
            let mut origin = [0i32; 3];
            let (a, b, f) = plane.axes();
            origin[a] = origin2d.0;
            origin[b] = origin2d.1;
            origin[f] = fixed;
            let shape = detect_custom_portal_shape(
                origin,
                plane,
                &cfg,
                |x, y, z| world.is_replaceable(x, y, z),
                |x, y, z| world.is_frame(x, y, z),
            )
            .unwrap_or_else(|| panic!("shape {idx} must ignite"));
            assert_eq!(shape.interior.len(), expected_counts[idx]);
            assert_eq!(shape.plane, plane);
        }
    }

    #[test]
    fn open_or_broken_frame_returns_none() {
        let cfg = vanilla_config();
        let mut world = TestWorld::new();
        build_framed_shape(
            PortalPlane::VerticalX,
            &rect_cells(2, 3),
            0,
            &mut world,
            0,
        );
        // Punch a hole: an edge frame cell becomes dirt (solid, non-frame).
        world.set(-1, 0, 0, TestBlock::Dirt);
        let result = detect_custom_portal_shape(
            [0, 0, 0],
            PortalPlane::VerticalX,
            &cfg,
            |x, y, z| world.is_replaceable(x, y, z),
            |x, y, z| world.is_frame(x, y, z),
        );
        assert!(result.is_none(), "unclosed frame must never flood");
    }

    #[test]
    fn oversized_arena_returns_none_without_runaway() {
        // 25x25 interior exceeds both max_span (23) and max_area (441).
        let mut world = TestWorld::new();
        build_framed_shape(
            PortalPlane::VerticalX,
            &rect_cells(25, 25),
            0,
            &mut world,
            0,
        );
        let cfg = vanilla_config();
        let result = detect_custom_portal_shape(
            [0, 0, 0],
            PortalPlane::VerticalX,
            &cfg,
            |x, y, z| world.is_replaceable(x, y, z),
            |x, y, z| world.is_frame(x, y, z),
        );
        assert!(result.is_none());

        // A shape under the span limit but over a tightened max_area.
        let tight = CustomPortalConfig {
            max_area: 100,
            max_span: 48,
            ..vanilla_config()
        };
        let mut world2 = TestWorld::new();
        build_framed_shape(
            PortalPlane::VerticalX,
            &rect_cells(12, 12),
            0,
            &mut world2,
            0,
        );
        let result2 = detect_custom_portal_shape(
            [0, 0, 0],
            PortalPlane::VerticalX,
            &tight,
            |x, y, z| world2.is_replaceable(x, y, z),
            |x, y, z| world2.is_frame(x, y, z),
        );
        assert!(
            result2.is_none(),
            "144 cells must exceed max_area 100"
        );
    }

    #[test]
    fn horizontal_floor_portal_gated_by_config() {
        // Twilight-Forest-style floor portal: 3x3 interior in the XZ plane,
        // embedded in dirt so the vertical-plane probes fail fast.
        let cells: Vec<(i32, i32)> = {
            let mut c = Vec::new();
            for x in -1..=1 {
                for z in -1..=1 {
                    c.push((x, z));
                }
            }
            c
        };
        let mut world = TestWorld::new();
        build_framed_shape(PortalPlane::HorizontalY, &cells, 5, &mut world, 0);
        // Solid floor/ceiling around the slab so VerticalX/Z probes hit dirt.
        for &(u, v) in &cells {
            for dy in [-1i32, 1] {
                world.set(u, 5 + dy, v, TestBlock::Dirt);
            }
        }

        let cfg_on = vanilla_config();
        let shape = detect_any_custom_portal(
            [0, 5, 0],
            &cfg_on,
            |x, y, z| world.is_replaceable(x, y, z),
            |x, y, z| world.is_frame(x, y, z),
        )
        .expect("floor portal must ignite when enabled");
        assert_eq!(shape.plane, PortalPlane::HorizontalY);
        assert_eq!(shape.interior.len(), 9);

        let cfg_off = CustomPortalConfig {
            allow_horizontal_floor_portals: false,
            ..vanilla_config()
        };
        let none = detect_any_custom_portal(
            [0, 5, 0],
            &cfg_off,
            |x, y, z| world.is_replaceable(x, y, z),
            |x, y, z| world.is_frame(x, y, z),
        );
        assert!(
            none.is_none(),
            "floor portal must not ignite when disabled"
        );
    }

    #[test]
    fn connected_break_solver_collects_all_tiles() {
        // L-shaped interior on VerticalX.
        let cells = [
            (0, 0),
            (0, 1),
            (0, 2),
            (0, 3),
            (1, 0),
            (2, 0),
            (3, 0),
        ];
        let interior: HashSet<[i32; 3]> = cells
            .iter()
            .map(|&(u, v)| [u, v, 0])
            .collect();
        let is_portal = |x: i32, y: i32, z: i32| interior.contains(&[x, y, z]);
        let mut collected = collect_connected_portal_cells([0, 0, 0], PortalPlane::VerticalX, 64, is_portal);
        collected.sort_unstable();
        let mut expected: Vec<[i32; 3]> = interior.iter().copied().collect();
        expected.sort_unstable();
        assert_eq!(collected, expected);

        // Bounded collection truncates deterministically.
        let truncated =
            collect_connected_portal_cells([0, 0, 0], PortalPlane::VerticalX, 3, is_portal);
        assert_eq!(truncated.len(), 3);

        // Non-portal start yields empty.
        let empty =
            collect_connected_portal_cells([9, 9, 9], PortalPlane::VerticalX, 64, is_portal);
        assert!(empty.is_empty());
    }

    #[test]
    fn gravity_flip_landing_is_void_proof() {
        let center = [100.0, 64.0, -32.0];
        let plan = compute_safe_gravity_flip_landing(
            [50.0, -30.0, -40.0],
            center,
            2.0,
            true,
        );
        assert!(plan.landing_pos.iter().all(|v| v.is_finite()));
        assert_eq!(plan.landing_pos, [100.0, 66.25, -32.0]);
        let speed = plan.clamped_vel[0].hypot(plan.clamped_vel[2]);
        let max_speed: f64 = (2.0f64 * 0.35).clamp(0.05, 0.45);
        assert!(speed <= max_speed + f64::EPSILON);
        // Gentle descent only; never upward.
        assert!(plan.clamped_vel[1] <= 0.0 && plan.clamped_vel[1] >= -0.12);
        // At <=0.45 blocks/tick the drift over the first 4 descent ticks is
        // strictly inside the 5x5 (half extent 2.0) platform footprint.
        assert!(speed * 4.0 < 2.0);
        assert!(plan.close_rift_after_entry);
        assert_eq!(plan.fall_damage_immunity_ticks, 20);
        assert_eq!(plan.flip_duration_ms, 450);
        assert_eq!(plan.clear_headroom_blocks, 3);

        // NaN / Inf entry velocity sanitizes to a safe plan.
        let nan_plan = compute_safe_gravity_flip_landing(
            [f64::NAN, f64::INFINITY, f64::NEG_INFINITY],
            [f64::NAN, 64.0, -32.0],
            f64::NAN,
            false,
        );
        assert!(nan_plan.landing_pos.iter().all(|v| v.is_finite()));
        assert!(nan_plan.clamped_vel.iter().all(|v| v.is_finite()));
        assert_eq!(nan_plan.landing_pos, [0.0, 66.25, -32.0]);
        assert!(!nan_plan.close_rift_after_entry);
    }
}
