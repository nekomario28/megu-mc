//! StructureTemplate geometry and state transforms from pinned native contracts.
type Pos = (i32, i32, i32);
#[derive(Clone, Copy, Debug)]
pub(crate) enum Mirror {
    None,
    LeftRight,
    FrontBack,
}
#[derive(Clone, Copy, Debug)]
pub(crate) enum Rotation {
    None,
    Clockwise90,
    Clockwise180,
    Counterclockwise90,
}
impl Mirror {
    pub(crate) fn from_index(index: u8) -> Option<Self> {
        match index {
            0 => Some(Self::None),
            1 => Some(Self::LeftRight),
            2 => Some(Self::FrontBack),
            _ => None,
        }
    }
}
impl Rotation {
    pub(crate) fn from_index(index: u8) -> Option<Self> {
        match index {
            0 => Some(Self::None),
            1 => Some(Self::Clockwise90),
            2 => Some(Self::Clockwise180),
            3 => Some(Self::Counterclockwise90),
            _ => None,
        }
    }
}
pub(crate) fn block(
    (mut x, y, mut z): Pos,
    mirror: Mirror,
    rotation: Rotation,
    (px, _, pz): Pos,
) -> Pos {
    match mirror {
        Mirror::None => {}
        Mirror::LeftRight => z = z.wrapping_neg(),
        Mirror::FrontBack => x = x.wrapping_neg(),
    }
    match rotation {
        Rotation::None => (x, y, z),
        Rotation::Clockwise90 => (
            px.wrapping_add(pz).wrapping_sub(z),
            y,
            pz.wrapping_sub(px).wrapping_add(x),
        ),
        Rotation::Clockwise180 => (
            px.wrapping_add(px).wrapping_sub(x),
            y,
            pz.wrapping_add(pz).wrapping_sub(z),
        ),
        Rotation::Counterclockwise90 => (
            px.wrapping_sub(pz).wrapping_add(z),
            y,
            px.wrapping_add(pz).wrapping_sub(x),
        ),
    }
}
pub(crate) fn entity(
    [mut x, y, mut z]: [f64; 3],
    mirror: Mirror,
    rotation: Rotation,
    (px, _, pz): Pos,
) -> [f64; 3] {
    match mirror {
        Mirror::None => {}
        Mirror::LeftRight => z = 1.0 - z,
        Mirror::FrontBack => x = 1.0 - x,
    }
    match rotation {
        Rotation::None => [x, y, z],
        Rotation::Clockwise90 => [
            f64::from(px.wrapping_add(pz).wrapping_add(1)) - z,
            y,
            f64::from(pz.wrapping_sub(px)) + x,
        ],
        Rotation::Clockwise180 => [
            f64::from(px.wrapping_add(px).wrapping_add(1)) - x,
            y,
            f64::from(pz.wrapping_add(pz).wrapping_add(1)) - z,
        ],
        Rotation::Counterclockwise90 => [
            f64::from(px.wrapping_sub(pz)) + z,
            y,
            f64::from(px.wrapping_add(pz).wrapping_add(1)) - x,
        ],
    }
}
pub(crate) fn bounds(
    size: Pos,
    origin: Pos,
    pivot: Pos,
    mirror: Mirror,
    rotation: Rotation,
) -> [i32; 6] {
    let first = block((0, 0, 0), mirror, rotation, pivot);
    let last = block(
        (
            size.0.wrapping_sub(1),
            size.1.wrapping_sub(1),
            size.2.wrapping_sub(1),
        ),
        mirror,
        rotation,
        pivot,
    );
    [
        first.0.min(last.0).wrapping_add(origin.0),
        first.1.min(last.1).wrapping_add(origin.1),
        first.2.min(last.2).wrapping_add(origin.2),
        first.0.max(last.0).wrapping_add(origin.0),
        first.1.max(last.1).wrapping_add(origin.1),
        first.2.max(last.2).wrapping_add(origin.2),
    ]
}
pub(crate) fn state(state: u32, mirror: Mirror, rotation: Rotation) -> Option<u32> {
    let index = (state as usize)
        .checked_mul(12)?
        .checked_add(mirror as usize * 4 + rotation as usize)?
        .checked_mul(4)?;
    let end = index.checked_add(4)?;
    let bytes = include_bytes!("../assets/structure_state_transforms_1211.bin").get(index..end)?;
    Some(u32::from_be_bytes(bytes.try_into().unwrap()))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn template_coordinate_and_bounds_transforms_match_native() {
        fn pos(s: &str) -> Pos {
            let v: Vec<i32> = s.split(',').map(|v| v.parse().unwrap()).collect();
            assert_eq!(v.len(), 3);
            (v[0], v[1], v[2])
        }
        fn doubles(s: &str) -> [f64; 3] {
            let v: Vec<f64> = s
                .split(',')
                .map(|v| f64::from_bits(u64::from_str_radix(v, 16).unwrap()))
                .collect();
            v.try_into().unwrap()
        }
        let mut count = 0;
        for row in include_str!("../tests/data/structure_transform_1211.tsv").lines() {
            let f: Vec<_> = row.split('\t').collect();
            assert_eq!(f.len(), 10);
            let mirror = Mirror::from_index(f[0].parse().unwrap()).unwrap();
            let rotation = Rotation::from_index(f[1].parse().unwrap()).unwrap();
            assert_eq!(
                block(pos(f[2]), mirror, rotation, pos(f[3])),
                pos(f[5]),
                "native block: {row}"
            );
            assert_eq!(
                entity(doubles(f[4]), mirror, rotation, pos(f[3])).map(f64::to_bits),
                doubles(f[6]).map(f64::to_bits),
                "native entity: {row}"
            );
            assert_eq!(
                bounds(pos(f[7]), pos(f[8]), pos(f[3]), mirror, rotation).as_slice(),
                f[9].split(',')
                    .map(|v| v.parse::<i32>().unwrap())
                    .collect::<Vec<_>>(),
                "native bounds: {row}"
            );
            count += 1;
        }
        assert_eq!(count, 192);
    }
    #[test]
    fn every_native_state_transform_is_in_the_same_registry() {
        let count = crate::block_registry::breg().unwrap().state_count() as u32;
        assert_eq!(
            include_bytes!("../assets/structure_state_transforms_1211.bin").len(),
            count as usize * 12 * 4
        );
        for s in 0..count {
            assert_eq!(state(s, Mirror::None, Rotation::None), Some(s));
            for m in 0..3 {
                for r in 0..4 {
                    assert!(
                        state(
                            s,
                            Mirror::from_index(m).unwrap(),
                            Rotation::from_index(r).unwrap()
                        )
                        .unwrap()
                            < count
                    );
                }
            }
        }
        assert!(state(count, Mirror::None, Rotation::None).is_none());
    }
}
