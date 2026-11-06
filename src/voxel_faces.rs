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

#[cfg(test)]
mod tests {
    use super::*;
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
