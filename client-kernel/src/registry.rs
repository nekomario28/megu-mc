// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
use crate::{
    json::{jparse, J},
    light::face_occlusion_from_octants,
    Error, STATE_COUNT,
};
use std::collections::HashMap;

/// Pinned `getLightEmission`, `getLightBlock`, predicate and sturdy-face facts, six bytes per
/// state. The server registry owns the same table. The opened JSON must agree with it.
const STATE_FACTS: &[u8] = include_bytes!("../../assets/block_state_facts_1211.bin");
const FACT_BYTES: usize = 6;
const FACT_IS_AIR: u8 = 1;
const FACT_LIQUID: u8 = 1 << 1;
const FACT_BLOCKS_MOTION: u8 = 1 << 2;
const FACT_CAN_OCCLUDE: u8 = 1 << 3;
const FACT_SOLID_RENDER: u8 = 1 << 4;
const FACT_FLUID_EMPTY: u8 = 1 << 7;

/// The pinned asset's data is at most four containers deep. Bound recursion in the parser.
const MAX_JSON_DEPTH: usize = 16;

pub const FLUID_NONE: u8 = 0;
pub const FLUID_WATER: u8 = 1;
pub const FLUID_LAVA: u8 = 2;

/// Physical facts for one block state. Booleans are 0 or 1.
///
/// `sturdy_faces_mask` uses bit order down, up, north, south, west, east. `face_occlusion`
/// holds half-block quadrant masks in lighting order (west, east, north, south, down, up) and
/// can be passed to the light engine as is. Pinned tables record no separate occlusion
/// shape, so it is full for solid-render states, the collision boxes' fully covered octants
/// for other occluding states such as slabs and stairs, and empty otherwise.
/// `fluid_level` is the `level` property of water and lava blocks (0 source, 8-15 falling)
/// and 0 elsewhere. `is_waterlogged` marks water carried by a block other than `water`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BlockFacts {
    pub luminance: u8,
    pub opacity: u8,
    pub is_air: u8,
    pub is_liquid: u8,
    pub fluid_kind: u8,
    pub fluid_level: u8,
    pub is_waterlogged: u8,
    pub is_solid_render: u8,
    pub can_occlude: u8,
    pub blocks_motion: u8,
    pub sturdy_faces_mask: u8,
    pub face_occlusion: [u8; 6],
}

/// Read-only 1.21.1 block-state registry.
pub struct Registry {
    names: Vec<String>,
    first_states: Vec<u32>,
    by_name: HashMap<String, u32>,
    facts: Vec<BlockFacts>,
}

fn parse(json: &[u8]) -> Result<J, Error> {
    let (mut depth, mut in_string, mut escaped) = (0usize, false, false);
    for &byte in json {
        if in_string {
            match byte {
                _ if escaped => escaped = false,
                b'\\' => escaped = true,
                b'"' => in_string = false,
                _ => {}
            }
        } else {
            match byte {
                b'"' => in_string = true,
                b'[' | b'{' => {
                    depth += 1;
                    if depth > MAX_JSON_DEPTH {
                        return Err(Error::InvalidJson);
                    }
                }
                b']' | b'}' => depth = depth.saturating_sub(1),
                _ => {}
            }
        }
    }
    jparse(json).ok_or(Error::InvalidJson)
}

fn int(value: &J, max: u32) -> Result<u32, Error> {
    match value {
        J::Num(n) if n.fract() == 0.0 && (0.0..=f64::from(max)).contains(n) => Ok(*n as u32),
        _ => Err(Error::InvalidJson),
    }
}

fn array<'a>(value: &'a J, key: &str) -> Result<&'a Vec<J>, Error> {
    value.get(key).and_then(J::as_arr).ok_or(Error::InvalidJson)
}

fn column<'a>(states: &'a J, key: &str) -> Result<&'a Vec<J>, Error> {
    let column = array(states, key)?;
    if column.len() == STATE_COUNT as usize {
        Ok(column)
    } else {
        Err(Error::InvalidJson)
    }
}

fn text(value: &J) -> Result<&str, Error> {
    match value {
        J::Str(s) => Ok(s),
        _ => Err(Error::InvalidJson),
    }
}

/// Octants (x, z, y bit order) wholly inside one collision box.
fn covered_octants(boxes: &[([f64; 3], [f64; 3])]) -> u8 {
    (0..8u8)
        .filter(|&octant| {
            let low = [
                f64::from(octant & 1) * 0.5,
                f64::from(octant >> 2) * 0.5,
                f64::from((octant >> 1) & 1) * 0.5,
            ];
            boxes.iter().any(|(min, max)| {
                (0..3).all(|axis| min[axis] <= low[axis] && max[axis] >= low[axis] + 0.5)
            })
        })
        .fold(0, |mask, octant| mask | 1 << octant)
}

impl Registry {
    /// Open from `assets/block_states_1211.json` and `assets/blocks_1211.json`. Both must
    /// describe the pinned 1.21.1 state table and agree with each other.
    pub fn open_json(states_json: &[u8], blocks_json: &[u8]) -> Result<Self, Error> {
        let states = parse(states_json)?;
        let blocks = parse(blocks_json)?;
        if STATE_FACTS.len() != STATE_COUNT as usize * FACT_BYTES
            || int(states.get("format").ok_or(Error::InvalidJson)?, 1)? != 1
            || text(states.get("minecraft").ok_or(Error::InvalidJson)?)? != "1.21.1"
        {
            return Err(Error::InvalidJson);
        }
        let table = array(&states, "blocks")?;
        let details = blocks.as_arr().ok_or(Error::InvalidJson)?;
        if table.len() != details.len() || table.is_empty() {
            return Err(Error::InvalidJson);
        }
        let (mut names, mut first_states) = (vec![], vec![]);
        let mut by_name = HashMap::new();
        let mut next = 0;
        for (row, detail) in table.iter().zip(details) {
            let row = row
                .as_arr()
                .filter(|r| r.len() == 3)
                .ok_or(Error::InvalidJson)?;
            let name = text(&row[0])?;
            let first = int(&row[1], STATE_COUNT)?;
            let count = int(&row[2], STATE_COUNT)?;
            let last = first.checked_add(count).ok_or(Error::InvalidJson)?;
            let default = int(
                detail.get("defaultState").ok_or(Error::InvalidJson)?,
                STATE_COUNT,
            )?;
            let min = int(
                detail.get("minStateId").ok_or(Error::InvalidJson)?,
                STATE_COUNT,
            )?;
            let max = int(
                detail.get("maxStateId").ok_or(Error::InvalidJson)?,
                STATE_COUNT,
            )?;
            let detail_name = text(detail.get("name").ok_or(Error::InvalidJson)?)?;
            if first != next
                || count == 0
                || last > STATE_COUNT
                || (min, max) != (first, last - 1)
                || !(first..last).contains(&default)
                || name.strip_prefix("minecraft:") != Some(detail_name)
                || by_name.insert(name.to_string(), default).is_some()
            {
                return Err(Error::InvalidJson);
            }
            next = last;
            names.push(name.to_string());
            first_states.push(first);
        }
        if next != STATE_COUNT {
            return Err(Error::InvalidJson);
        }

        let shapes = array(&states, "shapes")?
            .iter()
            .map(|shape| {
                let corner = |key| -> Result<[f64; 3], Error> {
                    match shape.get(key).and_then(J::as_arr).map(|v| v.as_slice()) {
                        Some([J::Num(x), J::Num(y), J::Num(z)]) => Ok([*x, *y, *z]),
                        _ => Err(Error::InvalidJson),
                    }
                };
                Ok((corner("min")?, corner("max")?))
            })
            .collect::<Result<Vec<_>, Error>>()?;
        let (luminance, light_block) = (
            column(&states, "luminance")?,
            column(&states, "light_block")?,
        );
        let (flags, full_sides) = (column(&states, "flags")?, column(&states, "full_sides")?);
        let collision = column(&states, "collision_shapes")?;

        let mut facts = Vec::with_capacity(STATE_COUNT as usize);
        let mut block = 0;
        for state in 0..STATE_COUNT as usize {
            while block + 1 < first_states.len() && first_states[block + 1] as usize <= state {
                block += 1;
            }
            let bytes = &STATE_FACTS[state * FACT_BYTES..][..FACT_BYTES];
            let sturdy =
                (bytes[2] as u32 | (bytes[3] as u32) << 8 | (bytes[4] as u32) << 16) & 0x3f;
            let json_flags = int(&flags[state], 31)?;
            if int(&luminance[state], 15)? != u32::from(bytes[1] >> 4)
                || int(&light_block[state], 15)? != u32::from(bytes[1] & 0xf)
                || int(&full_sides[state], 63)? != sturdy
                || (json_flags & 1 != 0) != (bytes[0] & FACT_IS_AIR != 0)
                || (json_flags & 4 != 0) != (bytes[0] & FACT_LIQUID != 0)
            {
                return Err(Error::InvalidJson);
            }
            let boxes = collision[state]
                .as_arr()
                .ok_or(Error::InvalidJson)?
                .iter()
                .map(|index| {
                    shapes
                        .get(int(index, u32::MAX)? as usize)
                        .copied()
                        .ok_or(Error::InvalidJson)
                })
                .collect::<Result<Vec<_>, Error>>()?;

            let solid_render = bytes[0] & FACT_SOLID_RENDER != 0;
            let can_occlude = bytes[0] & FACT_CAN_OCCLUDE != 0;
            let octants = if solid_render {
                0xff
            } else if can_occlude {
                covered_octants(&boxes)
            } else {
                0
            };
            let name = names[block].as_str();
            let fluid_kind = match (bytes[0] & FACT_FLUID_EMPTY != 0, name) {
                (true, _) => FLUID_NONE,
                (false, "minecraft:lava") => FLUID_LAVA,
                (false, _) => FLUID_WATER,
            };
            let carrier = matches!(name, "minecraft:water" | "minecraft:lava");
            facts.push(BlockFacts {
                luminance: bytes[1] >> 4,
                opacity: bytes[1] & 0xf,
                is_air: u8::from(bytes[0] & FACT_IS_AIR != 0),
                is_liquid: u8::from(bytes[0] & FACT_LIQUID != 0),
                fluid_kind,
                fluid_level: if carrier {
                    (state as u32 - first_states[block]) as u8
                } else {
                    0
                },
                is_waterlogged: u8::from(fluid_kind == FLUID_WATER && name != "minecraft:water"),
                is_solid_render: u8::from(solid_render),
                can_occlude: u8::from(can_occlude),
                blocks_motion: u8::from(bytes[0] & FACT_BLOCKS_MOTION != 0),
                sturdy_faces_mask: sturdy as u8,
                face_occlusion: face_occlusion_from_octants(octants),
            });
        }
        Ok(Self {
            names,
            first_states,
            by_name,
            facts,
        })
    }

    pub fn state_count(&self) -> u32 {
        STATE_COUNT
    }

    /// The namespaced block name of a state, such as `minecraft:oak_slab`.
    pub fn state_name(&self, state: u32) -> Result<&str, Error> {
        if state >= STATE_COUNT {
            return Err(Error::InvalidState);
        }
        let block = self.first_states.partition_point(|&first| first <= state) - 1;
        Ok(&self.names[block])
    }

    /// The default state of a namespaced block name.
    pub fn default_state(&self, name: &str) -> Result<u32, Error> {
        self.by_name.get(name).copied().ok_or(Error::NotFound)
    }

    pub fn facts(&self, state: u32) -> Result<BlockFacts, Error> {
        self.facts
            .get(state as usize)
            .copied()
            .ok_or(Error::InvalidState)
    }

    #[cfg(test)]
    fn block_count(&self) -> usize {
        self.by_name.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::light::owner::{FaceOcclusion, Half};

    fn open() -> Registry {
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../assets/");
        let read = |name| std::fs::read(format!("{dir}{name}")).unwrap();
        Registry::open_json(&read("block_states_1211.json"), &read("blocks_1211.json")).unwrap()
    }

    #[test]
    fn pinned_assets_open_and_expose_light_facts() {
        let registry = open();
        assert_eq!(registry.block_count(), 1060);
        let stone = registry.default_state("minecraft:stone").unwrap();
        assert_eq!(registry.state_name(stone).unwrap(), "minecraft:stone");
        let facts = registry.facts(stone).unwrap();
        assert_eq!(
            (
                facts.opacity,
                facts.is_solid_render,
                facts.sturdy_faces_mask
            ),
            (15, 1, 63)
        );
        assert_eq!(facts.face_occlusion, [0xf; 6]);
        let torch = registry
            .facts(registry.default_state("minecraft:torch").unwrap())
            .unwrap();
        assert_eq!(
            (torch.luminance, torch.opacity, torch.blocks_motion),
            (14, 0, 0)
        );
        assert_eq!(registry.facts(0).unwrap().is_air, 1);
    }

    #[test]
    fn fluid_and_slab_facts_follow_the_pinned_states() {
        let registry = open();
        let water = registry.facts(80).unwrap();
        assert_eq!(
            (water.fluid_kind, water.fluid_level, water.is_liquid),
            (FLUID_WATER, 0, 1)
        );
        assert_eq!(registry.facts(83).unwrap().fluid_level, 3);
        let lava = registry.facts(96 + 8).unwrap();
        assert_eq!(
            (lava.fluid_kind, lava.fluid_level, lava.is_waterlogged),
            (FLUID_LAVA, 8, 0)
        );
        assert_eq!(registry.facts(0).unwrap().fluid_kind, FLUID_NONE);

        let slab = registry.default_state("minecraft:oak_slab").unwrap();
        assert_eq!(slab, 11165);
        let facts = |state| registry.facts(state).unwrap();
        assert_eq!(facts(11164).is_waterlogged, 1);
        assert_eq!(facts(11164).fluid_kind, FLUID_WATER);
        assert_eq!(facts(11165).is_waterlogged, 0);
        assert_eq!(
            facts(11165).face_occlusion,
            FaceOcclusion::slab(Half::Bottom).quadrants()
        );
        assert_eq!(
            facts(11163).face_occlusion,
            FaceOcclusion::slab(Half::Top).quadrants()
        );
        assert_eq!(facts(11167).face_occlusion, [0xf; 6]);
        let kelp = registry.default_state("minecraft:kelp").unwrap();
        assert_eq!(
            (facts(kelp).fluid_kind, facts(kelp).is_waterlogged),
            (FLUID_WATER, 1)
        );
    }

    #[test]
    fn invalid_lookups_and_inputs_fail_closed() {
        let registry = open();
        assert_eq!(registry.state_name(STATE_COUNT), Err(Error::InvalidState));
        assert_eq!(registry.facts(STATE_COUNT), Err(Error::InvalidState));
        assert_eq!(registry.default_state("stone"), Err(Error::NotFound));
        assert!(matches!(
            Registry::open_json(b"", b""),
            Err(Error::InvalidJson)
        ));
        assert!(matches!(
            Registry::open_json(b"{}", b"[]"),
            Err(Error::InvalidJson)
        ));
        let deep = [b'['; 4096];
        assert!(matches!(
            Registry::open_json(&deep, b"[]"),
            Err(Error::InvalidJson)
        ));
    }
}
