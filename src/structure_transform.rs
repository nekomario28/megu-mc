//! StructureTemplate geometry and state transforms from pinned native contracts.
type Pos = (i32, i32, i32);
#[derive(Clone, Copy, Debug)]
#[repr(u8)]
pub(crate) enum Mirror {
    None,
    LeftRight,
    FrontBack,
}
#[derive(Clone, Copy, Debug)]
#[repr(u8)]
pub(crate) enum Rotation {
    None,
    Clockwise90,
    Clockwise180,
    Counterclockwise90,
}
impl Mirror {
    /// Native's mirror **index** (what a template file stores), not its name.
    ///
    /// The enum names are swapped relative to native's: this `LeftRight` flips `z` like
    /// native's `FRONT_BACK`, and this `FrontBack` flips `x` like native's `LEFT_RIGHT`.
    /// `from_index` is what keeps the composition right — a caller that maps a name
    /// instead of an index gets the wrong mirror.
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
// ---------- pinned jar-derived data on disk ----------
//
// The 1.21.1 tables this server reads are generated from the pinned server
// jar, so the build never `include_bytes!`s them: a stripped checkout carries
// none of the files until `tools/vanilla_data/gen_vanilla_data.sh` runs.
// Resolution order matches the block-registry pack: the directory
// `MEGU_VERSION_DIR` names first (version-neutral spelling, then the
// bundled `*_1211` tag), then the checkout layout relative to the working
// directory, then the manifest directory so in-repo callers still resolve
// when the working directory is somewhere else.

/// Every location [`pinned_data_bytes`] tries for `rel`, in order, so a
/// failure can name the whole search instead of a bare "not found".
fn pinned_data_candidates(rel: &str) -> Vec<std::path::PathBuf> {
    let mut out = Vec::with_capacity(4);
    if let Some(dir) = std::env::var_os("MEGU_VERSION_DIR") {
        if !dir.is_empty() {
            let dir = std::path::PathBuf::from(dir);
            if let Some(name) = rel.strip_prefix("assets/") {
                // The version-pack spelling drops the bundled tag (e.g.
                // `feature_order.json`); the tagged name stays a fallback so
                // an `assets/` copy also serves as a version directory.
                if let Some((stem, ext)) = name.split_once('.') {
                    let stem = stem.strip_suffix("_1211").unwrap_or(stem);
                    out.push(dir.join(format!("{stem}.{ext}")));
                }
                out.push(dir.join(name));
            } else if let Some(rest) = rel.strip_prefix("datapacks/vanilla/") {
                // The pack roots its datapack content at `data/`; the full
                // checkout spelling stays a fallback for a mirrored tree.
                out.push(dir.join(rest));
                out.push(dir.join(rel));
            } else {
                out.push(dir.join(rel));
            }
        }
    }
    out.push(rel.into());
    out.push(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(rel));
    out
}

/// The bytes of one pinned data file, or `None` when no candidate exists.
pub(crate) fn pinned_data_bytes(rel: &str) -> Option<Vec<u8>> {
    pinned_data_candidates(rel)
        .iter()
        .find_map(|p| std::fs::read(p).ok())
}

/// [`pinned_data_bytes`] for a table that must exist: the panic names every
/// candidate tried so a missing pack is a readable error, not a silent none.
#[allow(dead_code)]
pub(crate) fn pinned_data_expect(rel: &str, what: &str) -> Vec<u8> {
    pinned_data_bytes(rel).unwrap_or_else(|| {
        panic!(
            "{what}: {rel} is absent (searched {})",
            pinned_data_candidates(rel)
                .iter()
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        )
    })
}

/// The per-state `StructureTemplate` transform table: 12 u32s per state,
/// generated from the pinned 1.21.1 jar. `include_bytes!` used to compile it
/// in; loading it lazily keeps stripped checkouts building while a missing
/// pack still fails closed on first use.
fn transform_table() -> Option<&'static [u8]> {
    static TABLE: std::sync::OnceLock<Option<Vec<u8>>> = std::sync::OnceLock::new();
    TABLE
        .get_or_init(|| pinned_data_bytes("assets/structure_state_transforms_1211.bin"))
        .as_deref()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(u8)]
enum Dir {
    North,
    East,
    South,
    West,
}

impl Dir {
    const ALL: [Self; 4] = [Self::North, Self::East, Self::South, Self::West];

    const fn name(self) -> &'static str {
        match self {
            Self::North => "north",
            Self::East => "east",
            Self::South => "south",
            Self::West => "west",
        }
    }

    fn parse(name: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|direction| direction.name() == name)
    }
}

// Each row maps an input direction to its output. Mirror indices preserve the
// template-file/native enum contract documented by `Mirror::from_index`.
const DIRECTION_ROTATIONS: [[Dir; 4]; 4] = [
    [Dir::North, Dir::East, Dir::South, Dir::West],
    [Dir::East, Dir::South, Dir::West, Dir::North],
    [Dir::South, Dir::West, Dir::North, Dir::East],
    [Dir::West, Dir::North, Dir::East, Dir::South],
];
const DIRECTION_MIRRORS: [[Dir; 4]; 3] = [
    [Dir::North, Dir::East, Dir::South, Dir::West],
    [Dir::South, Dir::East, Dir::North, Dir::West],
    [Dir::North, Dir::West, Dir::South, Dir::East],
];
const ROTATION_STEPS_16: [i32; 4] = [0, 4, 8, 12];
const ROTATION_SWAPS_HORIZONTAL_AXIS: [bool; 4] = [false, true, false, true];

fn turn_horizontal(direction: &str, rotation: Rotation) -> &str {
    let Some(direction) = Dir::parse(direction) else {
        return direction;
    };
    DIRECTION_ROTATIONS[rotation as usize][direction as usize].name()
}

fn mirror_horizontal(direction: &str, mirror: Mirror) -> &str {
    let Some(direction) = Dir::parse(direction) else {
        return direction;
    };
    DIRECTION_MIRRORS[mirror as usize][direction as usize].name()
}

fn transform_direction(direction: &str, mirror: Mirror, rotation: Rotation) -> String {
    turn_horizontal(mirror_horizontal(direction, mirror), rotation).to_owned()
}

/// Derive a state transform from blockstate properties when the pinned binary table
/// is absent. This covers the dominant directional state contracts (facing/orientation,
/// horizontal rotation and per-direction connection properties) deterministically; the
/// binary table remains the exact native oracle when present.
fn derive_state_transform<FProps, FSet>(
    state: u32,
    mirror: Mirror,
    rotation: Rotation,
    properties_of: FProps,
    mut with_prop: FSet,
) -> Option<u32>
where
    FProps: FnOnce(u32) -> Option<Vec<(String, String)>>,
    FSet: FnMut(u32, &str, &str) -> u32,
{
    let props = properties_of(state)?;
    let mut out = state;
    let mut delayed: Vec<(String, String)> = Vec::new();
    for (key, value) in props {
        if key == "facing" || key == "orientation" {
            let mapped = transform_direction(&value, mirror, rotation);
            out = with_prop(out, &key, &mapped);
        } else if key == "rotation" {
            if let Ok(v) = value.parse::<i32>() {
                let steps = ROTATION_STEPS_16[rotation as usize];
                out = with_prop(out, &key, &((v + steps).rem_euclid(16)).to_string());
            }
        } else if key == "axis" && ROTATION_SWAPS_HORIZONTAL_AXIS[rotation as usize] {
            let mapped = match value.as_str() {
                "x" => "z",
                "z" => "x",
                _ => value.as_str(),
            };
            out = with_prop(out, &key, mapped);
        } else if matches!(key.as_str(), "north" | "east" | "south" | "west") {
            let mapped_key = transform_direction(&key, mirror, rotation);
            delayed.push((mapped_key, value));
        }
    }
    for (key, value) in delayed {
        out = with_prop(out, &key, &value);
    }
    Some(out)
}

fn state_from_table(state: u32, mirror: Mirror, rotation: Rotation) -> Option<u32> {
    let table = transform_table()?;
    let index = (state as usize)
        .checked_mul(12)?
        .checked_add(mirror as usize * 4 + rotation as usize)?
        .checked_mul(4)?;
    let end = index.checked_add(4)?;
    let bytes = table.get(index..end)?;
    Some(u32::from_be_bytes(bytes.try_into().unwrap()))
}

pub(crate) fn state_with<FProps, FSet>(
    state: u32,
    mirror: Mirror,
    rotation: Rotation,
    properties_of: FProps,
    with_prop: FSet,
) -> Option<u32>
where
    FProps: FnOnce(u32) -> Option<Vec<(String, String)>>,
    FSet: FnMut(u32, &str, &str) -> u32,
{
    if let Some(mapped) = state_from_table(state, mirror, rotation) {
        return Some(mapped);
    }
    derive_state_transform(state, mirror, rotation, properties_of, with_prop)
}

#[cfg(all(test, megu_vanilla_data))]
pub(crate) fn state(state: u32, mirror: Mirror, rotation: Rotation) -> Option<u32> {
    state_with(
        state,
        mirror,
        rotation,
        |s| crate::block_registry::breg()?.properties_of(s),
        |s, key, value| {
            crate::block_registry::breg()
                .map_or(s, |reg| reg.with_prop(s, key, value))
        },
    )
}

#[cfg(test)]
fn inverse_transform(mirror: Mirror, rotation: Rotation) -> (Mirror, Rotation) {
    let original: Vec<_> = Dir::ALL
        .into_iter()
        .map(|direction| {
            DIRECTION_ROTATIONS[rotation as usize]
                [DIRECTION_MIRRORS[mirror as usize][direction as usize] as usize]
        })
        .collect();
    for candidate_mirror in [mirror, Mirror::None, Mirror::LeftRight, Mirror::FrontBack] {
        for candidate_rotation in [
            Rotation::None,
            Rotation::Clockwise90,
            Rotation::Clockwise180,
            Rotation::Counterclockwise90,
        ] {
            let restores = original.iter().enumerate().all(|(index, direction)| {
                DIRECTION_ROTATIONS[candidate_rotation as usize]
                    [DIRECTION_MIRRORS[candidate_mirror as usize][*direction as usize] as usize]
                    == Dir::ALL[index]
            });
            if restores {
                return (candidate_mirror, candidate_rotation);
            }
        }
    }
    panic!("no inverse for {mirror:?} {rotation:?}");
}

#[cfg(test)]
#[test]
fn structure_transform_dihedral_tables_cover_all_symmetries_and_inverses() {
    let mut symmetries = std::collections::HashSet::new();
    for mirror in [Mirror::None, Mirror::FrontBack] {
        for rotation in [
            Rotation::None,
            Rotation::Clockwise90,
            Rotation::Clockwise180,
            Rotation::Counterclockwise90,
        ] {
            let mapped: Vec<_> = Dir::ALL
                .into_iter()
                .map(|direction| {
                    DIRECTION_ROTATIONS[rotation as usize]
                        [DIRECTION_MIRRORS[mirror as usize][direction as usize] as usize]
                })
                .collect();
            assert!(
                symmetries.insert(mapped.clone()),
                "duplicate {mirror:?} {rotation:?}"
            );
            let (inverse_mirror, inverse_rotation) = inverse_transform(mirror, rotation);
            for (original, transformed) in Dir::ALL.into_iter().zip(mapped) {
                let restored = DIRECTION_ROTATIONS[inverse_rotation as usize]
                    [DIRECTION_MIRRORS[inverse_mirror as usize][transformed as usize] as usize];
                assert_eq!(restored, original);
                assert_eq!(
                    transform_direction(original.name(), mirror, rotation),
                    transformed.name()
                );
            }
        }
    }
    assert_eq!(symmetries.len(), 8);
}

#[cfg(all(test, megu_vanilla_data))]
mod tests {
    use super::*;
    #[test]
    fn directional_structure_states_round_trip_for_all_symmetries() {
        let reg = crate::block_registry::breg().unwrap();
        let mut seen = [false; 3];
        let mut checked_horizontal_axis = [false; 2];
        for state_id in 0..reg.state_count() as u32 {
            let name = reg.name_of(state_id);
            let family = if name.contains("stairs") {
                Some(0)
            } else if name.contains("rail") {
                Some(1)
            } else if name.contains("jigsaw") {
                Some(2)
            } else {
                None
            };
            let Some(props) = reg.properties_of(state_id) else {
                continue;
            };
            if let Some((_, axis)) = props.iter().find(|(key, _)| key == "axis") {
                if matches!(axis.as_str(), "x" | "z") {
                    for mirror in [Mirror::None, Mirror::FrontBack] {
                        for rotation in [
                            Rotation::None,
                            Rotation::Clockwise90,
                            Rotation::Clockwise180,
                            Rotation::Counterclockwise90,
                        ] {
                            let transformed = state(state_id, mirror, rotation).unwrap();
                            let expected = if ROTATION_SWAPS_HORIZONTAL_AXIS[rotation as usize] {
                                if axis == "x" {
                                    "z"
                                } else {
                                    "x"
                                }
                            } else {
                                axis
                            };
                            assert_eq!(
                                reg.get_prop(transformed, "axis").as_deref(),
                                Some(expected)
                            );
                        }
                    }
                    checked_horizontal_axis[usize::from(axis == "z")] = true;
                }
            }
            let Some(family) = family else { continue };
            if !props.iter().any(|(key, _)| {
                matches!(
                    key.as_str(),
                    "facing" | "orientation" | "axis" | "rotation" | "shape"
                )
            }) {
                continue;
            }
            seen[family] = true;
            for mirror in [Mirror::None, Mirror::FrontBack] {
                for rotation in [
                    Rotation::None,
                    Rotation::Clockwise90,
                    Rotation::Clockwise180,
                    Rotation::Counterclockwise90,
                ] {
                    let transformed = state(state_id, mirror, rotation).unwrap();
                    let (inverse_mirror, inverse_rotation) = inverse_transform(mirror, rotation);
                    let restored = state(transformed, inverse_mirror, inverse_rotation).unwrap();
                    // Vanilla 1.21.1 `StairBlock.mirror` only flips inner/outer `shape`
                    // when `facing` lies on the mirror's axis, so composing `FrontBack`
                    // with a 90° rotation flips non-straight stair shapes across the
                    // involution while still restoring `facing` identically.
                    if family == 0
                        && matches!(mirror, Mirror::FrontBack)
                        && matches!(rotation, Rotation::Clockwise90 | Rotation::Counterclockwise90)
                        && reg.get_prop(state_id, "shape").as_deref() != Some("straight")
                    {
                        assert_eq!(
                            reg.get_prop(restored, "facing"),
                            reg.get_prop(state_id, "facing"),
                            "{name} state={state_id} {mirror:?} {rotation:?}"
                        );
                    } else {
                        assert_eq!(
                            restored,
                            state_id,
                            "{name} state={state_id} {mirror:?} {rotation:?}"
                        );
                    }
                }
            }
        }
        assert!(
            seen.into_iter().all(|family| family),
            "missing stairs/rail/jigsaw states: {seen:?}"
        );
        assert!(
            checked_horizontal_axis.into_iter().all(|axis| axis),
            "registry did not exercise both horizontal axes: {checked_horizontal_axis:?}"
        );
    }

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
    /// The captured native entity transform, every row.
    ///
    /// `entity` existed before this corpus but had no native comparison; the fixture is
    /// `StructureTemplate.transform(Vec3, Mirror, Rotation, BlockPos)` over two pivots,
    /// three mirrors, four rotations and nine offsets, so a formula that ignores the
    /// pivot or the half-block offset cannot pass.
    #[test]
    fn entity_offset_transform_matches_native() {
        let pivots = [(10, 64, -4), (0, 0, 0)];
        let mut rows = 0;
        for line in include_str!("../tests/data/template_entity_1211.tsv").lines() {
            let f: Vec<&str> = line.split('\t').collect();
            assert_eq!(f.len(), 11, "{line}");
            let pivot = pivots[f[0].parse::<usize>().unwrap()];
            // Indices, not names: the port's mirror names are swapped (see `from_index`).
            let mirror = Mirror::from_index(f[1].parse().unwrap()).unwrap();
            let rotation = Rotation::from_index(f[2].parse().unwrap()).unwrap();
            let at = |i: usize| f[i].parse::<f64>().unwrap();
            let got = entity([at(5), at(6), at(7)], mirror, rotation, pivot);
            let want = [at(8), at(9), at(10)];
            for axis in 0..3 {
                assert!(
                    (got[axis] - want[axis]).abs() < 1e-9,
                    "{line}: got {got:?} want {want:?}"
                );
            }
            rows += 1;
        }
        assert_eq!(rows, 216, "every captured row is checked");
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
