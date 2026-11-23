//! Exposed voxel faces in native DiscreteVoxelShape traversal order.
//! Sparse occupancy avoids scanning the empty volume between distant writes.

use std::collections::HashSet;
type Pos = (i32, i32, i32);

/// Direction indices are DOWN, UP, NORTH, SOUTH, WEST, EAST. Coordinates are
/// valid world positions. Duplicate writes occupy one voxel; cavities retain
/// their inward-facing edges. This order is observable through scheduled ticks.
pub(crate) fn exposed_faces(points: impl IntoIterator<Item = Pos>) -> Vec<(u8, Pos, Pos)> {
    let occupied: HashSet<_> = points.into_iter().collect();
    let mut faces = Vec::new();
    for &p in &occupied {
        for (direction, (dx, dy, dz)) in [
            (0, (0, -1, 0)),
            (1, (0, 1, 0)),
            (2, (0, 0, -1)),
            (3, (0, 0, 1)),
            (4, (-1, 0, 0)),
            (5, (1, 0, 0)),
        ] {
            let q = (
                p.0.wrapping_add(dx),
                p.1.wrapping_add(dy),
                p.2.wrapping_add(dz),
            );
            if !occupied.contains(&q) {
                faces.push((direction, p, q));
            }
        }
    }
    // Native scans z (outer x, inner y), y (outer z, inner x), then x
    // (outer y, inner z). A positive face is emitted at the following empty
    // voxel, not at its occupied coordinate. Sorting transitions reproduces
    // that dense scan without iterating unoccupied rows or gaps.
    faces.sort_unstable_by_key(|&(direction, p, _)| {
        let positive = i32::from(direction & 1);
        match direction {
            2 | 3 => (0, p.0, p.1, p.2.wrapping_add(positive)),
            0 | 1 => (1, p.2, p.0, p.1.wrapping_add(positive)),
            4 | 5 => (2, p.1, p.2, p.0.wrapping_add(positive)),
            _ => unreachable!(),
        }
    });
    faces
}

#[cfg(all(test, megu_vanilla_data))]
mod tests {
    use super::*;

    /// A single occupied voxel touches every direction; the native scan order
    /// for it is the z faces (negative first), then y, then x.
    #[test]
    fn a_single_cell_emits_six_faces_in_scan_order() {
        assert_eq!(
            exposed_faces([(3, 4, 5)]),
            vec![
                (2, (3, 4, 5), (3, 4, 4)),
                (3, (3, 4, 5), (3, 4, 6)),
                (0, (3, 4, 5), (3, 3, 5)),
                (1, (3, 4, 5), (3, 5, 5)),
                (4, (3, 4, 5), (2, 4, 5)),
                (5, (3, 4, 5), (4, 4, 5)),
            ]
        );
    }

    /// The occupancy set owns dedup and ordering: the same voxel written twice,
    /// or written in a different sequence, is one voxel in the shape and the
    /// emitted sequence cannot change. A template may legitimately write a cell
    /// more than once (processed layers, a block entity row rewriting a cell).
    #[test]
    fn duplicate_and_reordered_points_emit_identically() {
        let a = exposed_faces([(1, 2, 3), (1, 2, 3), (2, 2, 3), (1, 5, 3)]);
        let b = exposed_faces([(2, 2, 3), (1, 5, 3), (1, 2, 3)]);
        assert_eq!(a, b);
        assert_eq!(a.len(), 16);
    }

    /// Nothing written, nothing scanned: the traversal over an empty shape is
    /// empty rather than a zero-cell face set.
    #[test]
    fn an_empty_shape_has_no_faces() {
        assert!(exposed_faces(std::iter::empty()).is_empty());
    }

    /// A cell fully enclosed by written cells emits no faces through the box
    /// walls, but the cavity itself is a boundary: every wall of the hole keeps
    /// its inward-facing face, since native scans transitions, not surfaces.
    #[test]
    fn a_cavity_keeps_its_inward_faces() {
        let mut cells = Vec::new();
        for x in 0..3 {
            for y in 0..3 {
                for z in 0..3 {
                    if (x, y, z) != (1, 1, 1) {
                        cells.push((x, y, z));
                    }
                }
            }
        }
        let faces = exposed_faces(cells);
        let cavity_faces: Vec<_> = faces.iter().filter(|f| f.2 == (1, 1, 1)).collect();
        assert_eq!(
            cavity_faces,
            vec![
                &(3, (1, 1, 0), (1, 1, 1)),
                &(2, (1, 1, 2), (1, 1, 1)),
                &(1, (1, 0, 1), (1, 1, 1)),
                &(0, (1, 2, 1), (1, 1, 1)),
                &(5, (0, 1, 1), (1, 1, 1)),
                &(4, (2, 1, 1), (1, 1, 1)),
            ],
            "the cavity wall reads as six faces, one per direction, in scan order"
        );
        assert_eq!(faces.len(), 54 + 6, "54 outer faces plus the 6 cavity walls");
    }

    /// Disconnected blobs emit independently in scan order: the z scan sees
    /// every z-face of both blobs (ordered by x, then y, then the empty side's
    /// z) before the y scan starts.
    #[test]
    fn disconnected_cells_interleave_only_by_scan_order() {
        let faces = exposed_faces([(0, 0, 0), (5, 5, 5)]);
        let z_faces: Vec<u8> = faces
            .iter()
            .take_while(|f| matches!(f.0, 2 | 3))
            .map(|f| f.0)
            .collect();
        assert_eq!(z_faces, vec![2, 3, 2, 3], "both cells' z faces precede y faces");
        assert_eq!(faces.len(), 12);
    }

    /// Face direction indices stay on the documented wiring: direction `d` of a
    /// face always points from the occupied cell `p` to the empty cell `q`.
    #[test]
    fn every_emitted_face_is_axis_consistent() {
        let cells = [(0, 0, 0), (1, 0, 0), (0, 1, 0), (0, 0, 1), (1, 1, 1)];
        let deltas = [
            (0, -1, 0),
            (0, 1, 0),
            (0, 0, -1),
            (0, 0, 1),
            (-1, 0, 0),
            (1, 0, 0),
        ];
        for &(d, p, q) in &exposed_faces(cells) {
            let off = deltas[d as usize];
            assert_eq!(q, (p.0 + off.0, p.1 + off.1, p.2 + off.2), "direction {d}");
        }
    }

    #[test]
    fn exposed_faces_match_actual_native_sequence() {
        let cases = crate::json::jparse(include_bytes!("../tests/data/voxel_face_cases_1211.json"))
            .unwrap();
        let cases = cases.as_arr().unwrap();
        let rows: Vec<_> = include_str!("../tests/data/voxel_faces_1211.tsv")
            .lines()
            .collect();
        assert!(!cases.is_empty());
        assert_eq!(cases.len(), rows.len());
        for (case, row) in cases.iter().zip(rows) {
            let fields: Vec<_> = row.split('\t').collect();
            assert_eq!(fields.len(), 3);
            assert_eq!(case.get("id").unwrap().str(), fields[0]);
            let point = |value: &crate::json::J| {
                let a = value.as_arr().unwrap();
                (a[0].num() as i32, a[1].num() as i32, a[2].num() as i32)
            };
            let o = point(case.get("origin").unwrap());
            let points = case
                .get("filled")
                .unwrap()
                .as_arr()
                .unwrap()
                .iter()
                .map(|v| {
                    let p = point(v);
                    (p.0 + o.0, p.1 + o.1, p.2 + o.2)
                });
            let faces = exposed_faces(points);
            let text: String = faces
                .iter()
                .map(|&(d, p, q)| {
                    let off = [
                        (0, -1, 0),
                        (0, 1, 0),
                        (0, 0, -1),
                        (0, 0, 1),
                        (-1, 0, 0),
                        (1, 0, 0),
                    ][d as usize];
                    assert_eq!(q, (p.0 + off.0, p.1 + off.1, p.2 + off.2));
                    format!("{d} {},{},{}\n", p.0, p.1, p.2)
                })
                .collect();
            let hash: String = crate::generation_math::sha256(text.as_bytes())
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect();
            assert_eq!(
                faces.len(),
                fields[1].parse::<usize>().unwrap(),
                "{}",
                fields[0]
            );
            assert_eq!(hash, fields[2], "native ordered faces: {}", fields[0]);
        }
    }
}
