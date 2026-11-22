//! Native structure-set placement primitives, independent of terrain and pieces.
//! Candidates are not accepted starts: biome, frequency and exclusion checks
//! still belong to the structure pipeline.
use crate::generation_math::Lcg;

#[derive(Clone, Copy, Debug)]
pub(crate) enum Spread {
    Linear,
    Triangular,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct RandomSpread {
    spacing: i32,
    separation: i32,
    salt: i32,
    spread: Spread,
}

fn salted(seed: u64, x: i32, z: i32, salt: i32) -> Lcg {
    Lcg::new(
        seed.wrapping_add((i64::from(x).wrapping_mul(341873128712)) as u64)
            .wrapping_add((i64::from(z).wrapping_mul(132897987541)) as u64)
            .wrapping_add(i64::from(salt) as u64),
    )
}

impl RandomSpread {
    pub(crate) fn new(
        spacing: i32,
        separation: i32,
        salt: i32,
        spread: Spread,
    ) -> Result<Self, &'static str> {
        if !(0..=4096).contains(&spacing)
            || !(0..=4096).contains(&separation)
            || spacing <= separation
        {
            return Err("spacing must exceed separation and both must be in 0..=4096");
        }
        Ok(Self {
            spacing,
            separation,
            salt,
            spread,
        })
    }

    pub(crate) fn candidate(self, seed: u64, x: i32, z: i32) -> (i32, i32) {
        let rx = x.div_euclid(self.spacing);
        let rz = z.div_euclid(self.spacing);
        let mut random = salted(seed, rx, rz, self.salt);
        let bound = self.spacing - self.separation;
        let mut draw = || match self.spread {
            Spread::Linear => random.next_bound(bound),
            Spread::Triangular => (random.next_bound(bound) + random.next_bound(bound)) / 2,
        };
        (
            rx.wrapping_mul(self.spacing).wrapping_add(draw()),
            rz.wrapping_mul(self.spacing).wrapping_add(draw()),
        )
    }
}

/// `StructurePlacement.ExclusionZone`: a structure set may not place a chunk when
/// another set places one within `chunk_count` chunks of it.
///
/// `ChunkGeneratorStructureState.hasStructureChunkInRange` scans the closed square
/// `[x-n, x+n] x [z-n, z+n]` and asks the *other* set's `isStructureChunk`, so the
/// other set's own frequency reducer and exclusion zone apply too.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ExclusionZone {
    pub(crate) chunk_count: i32,
}

impl ExclusionZone {
    pub(crate) fn forbids(
        self,
        x: i32,
        z: i32,
        mut other_admits: impl FnMut(i32, i32) -> bool,
    ) -> bool {
        let n = self.chunk_count;
        for ox in x - n..=x + n {
            for oz in z - n..=z + n {
                if other_admits(ox, oz) {
                    return true;
                }
            }
        }
        false
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum FrequencyReduction {
    Default,
    LegacyType1,
    LegacyType2,
    LegacyType3,
}

/// The cheap half of `generateRingPositions`: the candidate chunk and the child RNG
/// state for every ring, with **no** biome lookup.
///
/// `candidate` depends only on `(seed, distance, initial_spread, count)` and the child
/// state is a raw LCG state ([`Lcg::from_raw_state`] resumes it exactly). Keeping the
/// two halves in one function is what lets a caller resolve a single ring index
/// lazily without drifting from the eager [`concentric_rings`] path: the latter
/// delegates here, so both consume the identical `next_f64`/`nextLong` stream.
type RingCandidate = ((i32, i32), u64);

pub(crate) fn concentric_ring_candidates(
    seed: u64,
    distance: i32,
    initial_spread: i32,
    count: i32,
) -> Result<Vec<RingCandidate>, &'static str> {
    if !(0..=1023).contains(&distance)
        || !(0..=1023).contains(&initial_spread)
        || !(0..=4095).contains(&count)
    {
        return Err("invalid concentric ring parameters");
    }
    if count == 0 {
        return Ok(Vec::new());
    }
    let mut random = Lcg::new(seed);
    let mut angle = random.next_f64() * std::f64::consts::PI * 2.0;
    let mut spread = initial_spread;
    let mut placed = 0;
    let mut ring = 0;
    let mut result = Vec::new();
    // Java Math.round uses floor(x + 0.5), not Rust's ties-away-from-zero.
    let round = |value: f64| ((value + 0.5).floor() as i64) as i32;
    for i in 0..count {
        let radius = f64::from(4 * distance + distance * ring * 6)
            + (random.next_f64() - 0.5) * (f64::from(distance) * 2.5);
        let candidate = (round(angle.cos() * radius), round(angle.sin() * radius));
        let child = Lcg::new(random.next_i64() as u64);
        result.push((candidate, child.state48()));
        angle += (std::f64::consts::PI * 2.0) / f64::from(spread);
        placed += 1;
        if placed == spread {
            ring += 1;
            placed = 0;
            spread += 2 * spread / (ring + 1);
            spread = spread.min(count - i);
            angle += random.next_f64() * std::f64::consts::PI * 2.0;
        }
    }
    Ok(result)
}

/// ChunkGeneratorStructureState.generateRingPositions, before the caller's
/// biome source relocates a candidate. Its fork consumes a nextLong on every
/// position even when the preferred-biome search returns no match.
pub(crate) fn concentric_rings(
    seed: u64,
    distance: i32,
    initial_spread: i32,
    count: i32,
    mut find_biome: impl FnMut((i32, i32), &mut Lcg) -> Option<(i32, i32)>,
) -> Result<Vec<(i32, i32)>, &'static str> {
    let candidates = concentric_ring_candidates(seed, distance, initial_spread, count)?;
    let mut result = Vec::with_capacity(candidates.len());
    for (candidate, child_state) in candidates {
        let mut child = Lcg::from_raw_state(child_state);
        result.push(find_biome(candidate, &mut child).unwrap_or(candidate));
    }
    Ok(result)
}

/// `BiomeSource.findBiomeHorizontal` — the quart-space spiral vanilla runs after a
/// concentric-ring candidate, and the only place a stronghold's final chunk is decided.
///
/// `biome_at` answers `getNoiseBiome(quart_x, quart_y, quart_z)` (a `&str`, or any
/// `Copy` key such as a biome leaf id); `radius` is in blocks
/// (112 for strongholds), `increment` in quarts (the seven-argument overload passes 1,
/// i.e. a 4-block step). With `find_closest` every ring is a full square and the first
/// match wins; otherwise only the ring outline is sampled and matches are reservoir
/// sampled with one `nextInt(matches + 1)` draw each, which is why the RNG is threaded in.
///
/// Returns the matching block position (`QuartPos.toBlock`), not a chunk.
#[cfg_attr(not(test), allow(dead_code))]
// Direct port of the biome search contract, including RNG and closest-match mode.
#[allow(clippy::too_many_arguments)]
pub(crate) fn find_biome_horizontal<B: Copy>(
    x: i32,
    y: i32,
    z: i32,
    radius: i32,
    increment: i32,
    mut biome_at: impl FnMut(i32, i32, i32) -> B,
    mut matches: impl FnMut(B) -> bool,
    random: &mut Lcg,
    find_closest: bool,
) -> Option<(i32, i32, i32)> {
    // `QuartPos.fromBlock`: quart coordinates are a block coordinate shifted by two.
    let center_x = x >> 2;
    let center_z = z >> 2;
    let limit = radius >> 2;
    let quart_y = y >> 2;
    let mut found: Option<(i32, i32)> = None;
    let mut matched = 0;
    let mut ring = if find_closest { 0 } else { limit };
    while ring <= limit {
        let mut dz = -ring;
        while dz <= ring {
            let on_edge_z = dz.abs() == ring;
            let mut dx = -ring;
            while dx <= ring {
                if find_closest {
                    let on_edge_x = dx.abs() == ring;
                    if !on_edge_x && !on_edge_z {
                        dx += increment;
                        continue;
                    }
                }
                let qx = center_x + dx;
                let qz = center_z + dz;
                if matches(biome_at(qx, quart_y, qz)) {
                    if found.is_none() || random.next_bound(matched + 1) == 0 {
                        found = Some((qx, qz));
                        if find_closest {
                            return Some((qx << 2, y, qz << 2));
                        }
                    }
                    matched += 1;
                }
                dx += increment;
            }
            dz += increment;
        }
        ring += increment;
    }
    found.map(|(qx, qz)| (qx << 2, y, qz << 2))
}

/// `ChunkGeneratorStructureState`'s ring-relocation callback: the candidate chunk's
/// centre (`SectionPos.sectionToBlockCoord(chunk, 8)`) is searched for a preferred
/// biome and the *chunk* of the hit is returned, or the candidate when none is found.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn concentric_rings_target<B: Copy>(
    candidate: (i32, i32),
    random: &mut Lcg,
    radius: i32,
    mut biome_at: impl FnMut(i32, i32, i32) -> B,
    matches: impl FnMut(B) -> bool,
) -> Option<(i32, i32)> {
    let (cx, cz) = candidate;
    let hit = find_biome_horizontal(
        (cx << 4) + 8,
        0,
        (cz << 4) + 8,
        radius,
        1,
        &mut biome_at,
        matches,
        random,
        false,
    )?;
    Some((hit.0 >> 4, hit.2 >> 4))
}

impl FrequencyReduction {
    /// StructurePlacement.applyAdditionalChunkRestrictions. The DEFAULT reducer
    /// passes (seed, salt, chunkX, chunkZ) straight to the salted RNG in 1.21.1;
    /// keep that bytecode argument order even though it differs from candidates.
    pub(crate) fn allows(self, seed: u64, salt: i32, x: i32, z: i32, frequency: f32) -> bool {
        if frequency >= 1.0 {
            return true;
        }
        match self {
            Self::Default => salted(seed, salt, x, z).next_f32() < frequency,
            Self::LegacyType1 => {
                let region = (x >> 4) ^ (z >> 4).wrapping_shl(4);
                let mut random = Lcg::new((i64::from(region) as u64) ^ seed);
                random.next_i32();
                random.next_bound((1.0f32 / frequency) as i32) == 0
            }
            Self::LegacyType2 => salted(seed, x, z, 10387320).next_f32() < frequency,
            Self::LegacyType3 => {
                Lcg::for_large_feature(seed, x, z).next_f64() < f64::from(frequency)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rings_and_fixed_biome_relocation_match_native() {
        let mut cases = 0;
        for row in include_str!("../tests/data/concentric_rings_1211.tsv").lines() {
            let f: Vec<_> = row.split('\t').collect();
            assert_eq!(f.len(), 6);
            let relocate = f[4] == "1";
            let actual = concentric_rings(
                f[0].parse::<i64>().unwrap() as u64,
                f[1].parse().unwrap(),
                f[2].parse().unwrap(),
                f[3].parse().unwrap(),
                |(x, z), random| {
                    relocate.then(|| {
                        (
                            (x.wrapping_mul(16)
                                .wrapping_add(8)
                                .wrapping_sub(112)
                                .wrapping_add(random.next_bound(225)))
                                >> 4,
                            (z.wrapping_mul(16)
                                .wrapping_add(8)
                                .wrapping_sub(112)
                                .wrapping_add(random.next_bound(225)))
                                >> 4,
                        )
                    })
                },
            )
            .unwrap();
            let expected: Vec<(i32, i32)> = if f[5] == "EMPTY" {
                Vec::new()
            } else {
                f[5].split(';')
                    .map(|v| {
                        let (x, z) = v.split_once(',').unwrap();
                        (x.parse().unwrap(), z.parse().unwrap())
                    })
                    .collect()
            };
            assert_eq!(actual, expected, "native ring scenario {row}");
            cases += 1;
        }
        assert_eq!(cases, 84);
    }
    #[test]
    fn structure_set_candidates_and_frequency_match_native() {
        let mut count = 0;
        let mut sets = std::collections::HashSet::new();
        for row in include_str!("../tests/data/structure_placement_1211.tsv").lines() {
            let f: Vec<_> = row.split('\t').collect();
            assert_eq!(f.len(), 13);
            sets.insert(f[0]);
            let number = |i: usize| f[i].parse::<i32>().unwrap();
            let seed = f[1].parse::<i64>().unwrap() as u64;
            let spread = if number(7) == 0 {
                Spread::Linear
            } else {
                Spread::Triangular
            };
            let placement = RandomSpread::new(number(4), number(5), number(6), spread).unwrap();
            assert_eq!(
                placement.candidate(seed, number(2), number(3)),
                (number(10), number(11)),
                "{}",
                row
            );
            let reduction = match number(9) {
                0 => FrequencyReduction::Default,
                1 => FrequencyReduction::LegacyType1,
                2 => FrequencyReduction::LegacyType2,
                3 => FrequencyReduction::LegacyType3,
                _ => panic!("unknown native reduction"),
            };
            let frequency = f32::from_bits(number(8) as u32);
            assert_eq!(
                reduction.allows(seed, number(6), number(2), number(3), frequency),
                f[12] == "true",
                "{}",
                row
            );
            count += 1;
        }
        assert_eq!(sets.len(), 19);
        assert_eq!(count, 17024);
    }

    #[test]
    fn invalid_random_spread_parameters_are_rejected() {
        for (spacing, separation) in [(0, 0), (1, 1), (4, 5), (-1, 0), (4097, 0), (10, -1)] {
            assert!(RandomSpread::new(spacing, separation, 0, Spread::Linear).is_err());
        }
    }
}
