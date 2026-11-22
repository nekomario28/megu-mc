// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
use crate::{
    json::{jparse, J},
    light::face_occlusion_from_octants,
    Error, STATE_COUNT,
};
use megu_core::id::ResourceLocation;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// The physical-facts table is loaded at runtime, never embedded: a stripped
/// public export carries no `assets/*_1211*` file, so `include_bytes!` would
/// make this crate — and every client linking it — uncompilable there. The
/// resolution order for the pinned 1.21.1 table is bytes staged through
/// [`Registry::load_state_facts_from_bytes`], then the first
/// `block_state_facts{,_1211}.bin` found on disk, then a fallback synthesized
/// from the pack's own data.
const FACT_BYTES: usize = 6;
const FACT_IS_AIR: u8 = 1;
const FACT_LIQUID: u8 = 1 << 1;
const FACT_BLOCKS_MOTION: u8 = 1 << 2;
const FACT_CAN_OCCLUDE: u8 = 1 << 3;
const FACT_SOLID_RENDER: u8 = 1 << 4;
const FACT_FLUID_EMPTY: u8 = 1 << 7;

/// The bundled pack's version tag, as in the server's `VersionAssetPaths`.
const VERSION_ASSET_TAG: &str = "1211";

/// Facts staged by [`Registry::load_state_facts_from_bytes`]. Explicit bytes
/// win over any file on disk: a caller that stages them is delivering the
/// table a live data sync provided, not pointing at a local pack.
static STAGED_FACTS: OnceLock<Vec<u8>> = OnceLock::new();

/// The first `block_state_facts` table found on disk, including the negative
/// result, so a pack that carries none is probed once rather than on every
/// registry open.
static DISCOVERED_FACTS: OnceLock<Option<Vec<u8>>> = OnceLock::new();

/// Every directory a `block_state_facts` table may live in, mirroring the
/// server's `pinned_data_candidates` order: `MEGU_VERSION_DIR` first (a
/// generated version-pack root), then the checkout's `assets/` relative to the
/// working directory and to the manifest so in-repo callers still resolve
/// when the working directory is somewhere else. Each directory is tried with
/// the version-neutral name first and the bundled `*_1211` tag second, the
/// same fallback [`version_pack_path`] uses inside a pack.
fn state_facts_candidates() -> Vec<PathBuf> {
    let mut dirs = Vec::with_capacity(4);
    if let Some(dir) = std::env::var_os("MEGU_VERSION_DIR") {
        if !dir.is_empty() {
            dirs.push(PathBuf::from(dir));
        }
    }
    dirs.push(PathBuf::from("assets"));
    dirs.push(PathBuf::from("../assets"));
    dirs.push(Path::new(env!("CARGO_MANIFEST_DIR")).join("../assets"));
    let mut out = Vec::with_capacity(dirs.len() * 2);
    for dir in dirs {
        out.push(dir.join("block_state_facts.bin"));
        out.push(dir.join(format!("block_state_facts_{VERSION_ASSET_TAG}.bin")));
    }
    out
}

/// Read the first `block_state_facts` candidate that exists, or `None`.
fn discover_state_facts() -> Option<Vec<u8>> {
    state_facts_candidates()
        .iter()
        .find_map(|path| std::fs::read(path).ok())
}

/// The staged or discovered `block_state_facts` table, or `None` when the
/// pack ships none and the caller must derive or fall back.
fn state_facts() -> Option<&'static [u8]> {
    if let Some(bytes) = STAGED_FACTS.get() {
        return Some(bytes.as_slice());
    }
    DISCOVERED_FACTS
        .get_or_init(discover_state_facts)
        .as_deref()
}

/// A derived facts table claims at most this many states. Without it a
/// hostile `blocks.json` could name a `maxStateId` near `u32::MAX` and make
/// the fallback allocation the attack instead of the table.
const MAX_DERIVED_STATES: usize = 1 << 20;

/// Synthesize the six-bytes-per-state facts table a pack did not ship, so a
/// stripped export still opens instead of failing closed. With
/// `block_states.json` the pack's own columns reproduce the checked fields
/// exactly — air, liquid, emission, opacity, sturdy faces and carried fluid
/// are the same numbers the generated table records — and the collision
/// boxes stand in for the physical bits (a non-empty collision blocks motion
/// and can occlude, a lone full-cube collision marks a solid render).
/// Without it the fallback is purely structural: `air` states pass light and
/// motion, every other state an opaque full cube. Nothing here embeds
/// Mojang-derived bytes.
fn derive_state_facts(blocks: &J, states: Option<&J>) -> Result<Vec<u8>, Error> {
    let details = blocks.as_arr().ok_or(Error::InvalidJson)?;
    let states = match states {
        Some(states) => states,
        None => {
            let mut facts = Vec::new();
            let mut rows = 0usize;
            for detail in details {
                let name = text(detail.get("name").ok_or(Error::InvalidJson)?)?;
                let min = int(
                    detail.get("minStateId").ok_or(Error::InvalidJson)?,
                    u32::MAX,
                )?;
                let max = int(
                    detail.get("maxStateId").ok_or(Error::InvalidJson)?,
                    u32::MAX,
                )?;
                let row: [u8; FACT_BYTES] = match name.rsplit(':').next() == Some("air") {
                    true => [FACT_IS_AIR | FACT_FLUID_EMPTY, 0, 0, 0, 0, 0],
                    false => [
                        FACT_SOLID_RENDER | FACT_CAN_OCCLUDE | FACT_BLOCKS_MOTION
                            | FACT_FLUID_EMPTY,
                        0x0f,
                        0x3f,
                        0,
                        0,
                        0,
                    ],
                };
                rows = rows
                    .checked_add(max.wrapping_sub(min) as usize + usize::from(max >= min))
                    .ok_or(Error::InvalidJson)?;
                if rows > MAX_DERIVED_STATES {
                    return Err(Error::InvalidJson);
                }
                for _ in min..=max {
                    facts.extend_from_slice(&row);
                }
            }
            return Ok(facts);
        }
    };
    let flags = array(states, "flags")?;
    let count = flags.len();
    if count > MAX_DERIVED_STATES {
        return Err(Error::InvalidJson);
    }
    let luminance = column(states, "luminance", count)?;
    let light_block = column(states, "light_block", count)?;
    let full_sides = column(states, "full_sides", count)?;
    let collision = column(states, "collision_shapes", count)?;
    let shapes = array(states, "shapes")?;
    let full_cube = |shape: &J| -> Result<bool, Error> {
        let corner = |key| -> Result<[f64; 3], Error> {
            match shape.get(key).and_then(J::as_arr).map(Vec::as_slice) {
                Some([J::Num(x), J::Num(y), J::Num(z)]) => Ok([*x, *y, *z]),
                _ => Err(Error::InvalidJson),
            }
        };
        Ok(corner("min")? == [0.0, 0.0, 0.0] && corner("max")? == [1.0, 1.0, 1.0])
    };
    let mut facts = Vec::with_capacity(count * FACT_BYTES);
    for state in 0..count {
        let state_flags = int(&flags[state], 31)?;
        let boxes = collision[state].as_arr().ok_or(Error::InvalidJson)?;
        let mut row = [0u8; FACT_BYTES];
        if state_flags & 1 != 0 {
            row[0] |= FACT_IS_AIR;
        }
        if state_flags & 4 != 0 {
            row[0] |= FACT_LIQUID;
        }
        // The pack's `holds a fluid` bit is the liquid carrier question; a
        // state that is no liquid and holds none has an empty fluid state.
        if state_flags & (4 | 16) == 0 {
            row[0] |= FACT_FLUID_EMPTY;
        }
        if !boxes.is_empty() {
            row[0] |= FACT_BLOCKS_MOTION | FACT_CAN_OCCLUDE;
            if boxes.len() == 1 {
                let index = int(&boxes[0], u32::MAX)? as usize;
                if full_cube(shapes.get(index).ok_or(Error::InvalidJson)?)? {
                    row[0] |= FACT_SOLID_RENDER;
                }
            }
        }
        row[1] = ((int(&luminance[state], 15)? << 4) | int(&light_block[state], 15)?) as u8;
        row[2] = int(&full_sides[state], 63)? as u8;
        facts.extend_from_slice(&row);
    }
    Ok(facts)
}

/// [`derive_state_facts`] over the pack's raw parts, for kernel-internal
/// callers (the FFI surface, tests) that hold file bytes rather than parsed
/// JSON. Test-only today.
#[cfg(test)]
pub(crate) fn derive_state_facts_from_parts(
    blocks_json: &[u8],
    block_states_json: Option<&[u8]>,
) -> Result<Vec<u8>, Error> {
    derive_state_facts(
        &parse(blocks_json)?,
        block_states_json.map(parse).transpose()?.as_ref(),
    )
}

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

/// Read-only block-state registry for one Minecraft version's pack.
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

fn column<'a>(states: &'a J, key: &str, state_count: usize) -> Result<&'a Vec<J>, Error> {
    let column = array(states, key)?;
    if column.len() == state_count {
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

/// One version pack file inside `dir`: the version-neutral name wins and the
/// bundled pack's `*_1211` spelling is the fallback, mirroring the server's
/// `VersionAssetPaths::from_dir`. When neither exists the neutral path is
/// reported so the load error names the file the pack should have provided.
fn version_pack_path(dir: &Path, neutral: &str) -> PathBuf {
    let neutral_path = dir.join(neutral);
    if neutral_path.is_file() {
        return neutral_path;
    }
    let (stem, ext) = neutral
        .split_once('.')
        .expect("version asset names carry one extension");
    let tagged = dir.join(format!("{stem}_{VERSION_ASSET_TAG}.{ext}"));
    if tagged.is_file() {
        tagged
    } else {
        neutral_path
    }
}

/// Read one version pack file, or `None` when no spelling of it exists.
fn read_pack_file(dir: &Path, neutral: &str) -> Result<Option<Vec<u8>>, Error> {
    let path = version_pack_path(dir, neutral);
    if !path.is_file() {
        return Ok(None);
    }
    std::fs::read(&path).map(Some).map_err(|_| Error::NotFound)
}

/// Per-state columns of `block_states.json`, present only when that file is.
struct StateColumns<'a> {
    luminance: &'a [J],
    light_block: &'a [J],
    flags: &'a [J],
    full_sides: &'a [J],
    collision: &'a [J],
    shapes: Vec<([f64; 3], [f64; 3])>,
}

impl Registry {
    /// Open from `assets/block_states_1211.json` and `assets/blocks_1211.json`. Both must
    /// describe the pinned 1.21.1 state table and agree with each other.
    ///
    /// The facts table the opened JSON is cross-checked against is resolved at
    /// runtime — staged bytes first, then the first `block_state_facts` file
    /// found on disk — and when the pack ships none the table is derived from
    /// the `block_states.json` columns themselves, so a stripped public export
    /// still opens.
    pub fn open_json(states_json: &[u8], blocks_json: &[u8]) -> Result<Self, Error> {
        let states = parse(states_json)?;
        let blocks = parse(blocks_json)?;
        if int(states.get("format").ok_or(Error::InvalidJson)?, 1)? != 1
            || text(states.get("minecraft").ok_or(Error::InvalidJson)?)? != "1.21.1"
        {
            return Err(Error::InvalidJson);
        }
        let facts = state_facts();
        // A staged or discovered table that does not span the pinned state
        // space belongs to a different version's pack, not to this JSON.
        if facts.is_some_and(|bytes| bytes.len() != STATE_COUNT as usize * FACT_BYTES) {
            return Err(Error::InvalidJson);
        }
        Self::from_parsed(&blocks, Some(&states), facts)
    }

    /// Stage the pinned pack's `block_state_facts` bytes — six per state — as
    /// the table [`Registry::open_json`] and [`Registry::from_version_dir`]
    /// consult before any file on disk, for the table a client synced from its
    /// server's Configuration Phase rather than found in a local `assets/`
    /// copy. The table must span the pinned state space exactly. The first
    /// call wins: a second call fails closed with `Error::InvalidArgument` so
    /// a partial loader cannot silently swap facts mid-run.
    pub fn load_state_facts_from_bytes(bytes: &[u8]) -> Result<(), Error> {
        if bytes.len() != STATE_COUNT as usize * FACT_BYTES {
            return Err(Error::InvalidJson);
        }
        STAGED_FACTS
            .set(bytes.to_vec())
            .map_err(|_| Error::InvalidArgument)
    }

    /// Open a version pack from its raw parts. `blocks_json` is the pack's
    /// `blocks.json` (per-block names and state-id ranges), `state_facts_bin` its
    /// `block_state_facts.bin` (six bytes per state), and `block_states_json` its
    /// optional `block_states.json` per-state table. The state count derives from
    /// `state_facts_bin.len() / FACT_BYTES` and must agree with `blocks_json`
    /// exactly; a misaligned, truncated or empty facts table fails closed.
    ///
    /// Without `block_states_json` the per-state cross-check and exact occlusion
    /// shapes are unavailable: every occluding state is then treated as a full
    /// cube. Pass the file whenever lighting parity matters.
    pub fn from_parts(
        blocks_json: &[u8],
        block_states_json: Option<&[u8]>,
        state_facts_bin: &[u8],
    ) -> Result<Self, Error> {
        let blocks = parse(blocks_json)?;
        let states = block_states_json.map(parse).transpose()?;
        if let Some(states) = &states {
            if int(states.get("format").ok_or(Error::InvalidJson)?, 1)? != 1 {
                return Err(Error::InvalidJson);
            }
        }
        Self::from_parsed(&blocks, states.as_ref(), Some(state_facts_bin))
    }

    /// Open a version pack directory: `blocks.json` is required,
    /// `block_state_facts.bin` and `block_states.json` are optional. For each
    /// file the version-neutral name wins and the bundled pack's `*_1211`
    /// spelling is the fallback, so the pinned `assets/` directory also serves
    /// as a version dir. A missing required file reports `Error::NotFound`.
    ///
    /// A pack that carries no `block_state_facts` falls back to the staged or
    /// discovered pinned table, then to facts derived from its own
    /// `block_states.json` columns or — with neither — a pure-structural
    /// table (`air` passable, every other state an opaque full cube) instead
    /// of failing, so a stripped public export still opens.
    pub fn from_version_dir(dir: &Path) -> Result<Self, Error> {
        let required = |name: &str| read_pack_file(dir, name)?.ok_or(Error::NotFound);
        let blocks = parse(&required("blocks.json")?)?;
        let states = read_pack_file(dir, "block_states.json")?
            .map(|bytes| parse(&bytes))
            .transpose()?;
        let facts = read_pack_file(dir, "block_state_facts.bin")?;
        let facts = match &facts {
            Some(bytes) => Some(bytes.as_slice()),
            None => state_facts(),
        };
        Self::from_parsed(&blocks, states.as_ref(), facts)
    }

    /// Shared loader: `blocks` is the parsed `blocks.json` array, `states` the
    /// parsed `block_states.json` object when the pack provides it, and
    /// `facts_bin` the pack's six-bytes-per-state facts table — or `None` when
    /// the pack carries none, in which case [`derive_state_facts`]
    /// synthesizes it from the pack's own data.
    fn from_parsed(
        blocks: &J,
        states: Option<&J>,
        facts_bin: Option<&[u8]>,
    ) -> Result<Self, Error> {
        let derived;
        let facts_bin = match facts_bin {
            Some(bytes) => bytes,
            None => {
                derived = derive_state_facts(blocks, states)?;
                derived.as_slice()
            }
        };
        if facts_bin.is_empty() || !facts_bin.len().is_multiple_of(FACT_BYTES) {
            return Err(Error::InvalidJson);
        }
        let state_count = facts_bin.len() / FACT_BYTES;
        let state_count_u32 = u32::try_from(state_count).map_err(|_| Error::InvalidJson)?;
        let details = blocks.as_arr().ok_or(Error::InvalidJson)?;
        if details.is_empty() {
            return Err(Error::InvalidJson);
        }
        let table = states.map(|s| array(s, "blocks")).transpose()?;
        if let Some(table) = table {
            if table.len() != details.len() {
                return Err(Error::InvalidJson);
            }
        }

        let (mut names, mut first_states) = (vec![], vec![]);
        let mut by_name = HashMap::new();
        let mut next = 0;
        for (index, detail) in details.iter().enumerate() {
            let detail_name = text(detail.get("name").ok_or(Error::InvalidJson)?)?;
            let default = int(
                detail.get("defaultState").ok_or(Error::InvalidJson)?,
                state_count_u32,
            )?;
            let min = int(
                detail.get("minStateId").ok_or(Error::InvalidJson)?,
                state_count_u32,
            )?;
            let max = int(
                detail.get("maxStateId").ok_or(Error::InvalidJson)?,
                state_count_u32,
            )?;
            let (name, first, last) = match table {
                Some(table) => {
                    let row = table[index]
                        .as_arr()
                        .filter(|r| r.len() == 3)
                        .ok_or(Error::InvalidJson)?;
                    let name = text(&row[0])?;
                    let first = int(&row[1], state_count_u32)?;
                    let count = int(&row[2], state_count_u32)?;
                    let last = first.checked_add(count).ok_or(Error::InvalidJson)?;
                    if count == 0
                        || last > state_count_u32
                        || (min, max) != (first, last - 1)
                        || name.strip_prefix("minecraft:") != Some(detail_name)
                    {
                        return Err(Error::InvalidJson);
                    }
                    (name.to_string(), first, last)
                }
                None => {
                    if max < min {
                        return Err(Error::InvalidJson);
                    }
                    let last = max.checked_add(1).ok_or(Error::InvalidJson)?;
                    let name = match detail_name.contains(':') {
                        true => detail_name.to_string(),
                        false => format!("minecraft:{detail_name}"),
                    };
                    (name, min, last)
                }
            };
            if first != next
                || !(first..last).contains(&default)
                || by_name.insert(name.clone(), default).is_some()
            {
                return Err(Error::InvalidJson);
            }
            next = last;
            names.push(name);
            first_states.push(first);
        }
        if next != state_count_u32 {
            return Err(Error::InvalidJson);
        }

        let columns = states
            .map(|states| {
                Ok(StateColumns {
                    luminance: column(states, "luminance", state_count)?,
                    light_block: column(states, "light_block", state_count)?,
                    flags: column(states, "flags", state_count)?,
                    full_sides: column(states, "full_sides", state_count)?,
                    collision: column(states, "collision_shapes", state_count)?,
                    shapes: array(states, "shapes")?
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
                        .collect::<Result<Vec<_>, Error>>()?,
                })
            })
            .transpose()?;

        let mut facts = Vec::with_capacity(state_count);
        let mut block = 0;
        for state in 0..state_count {
            while block + 1 < first_states.len() && first_states[block + 1] as usize <= state {
                block += 1;
            }
            let bytes = &facts_bin[state * FACT_BYTES..][..FACT_BYTES];
            let sturdy =
                (bytes[2] as u32 | (bytes[3] as u32) << 8 | (bytes[4] as u32) << 16) & 0x3f;
            let solid_render = bytes[0] & FACT_SOLID_RENDER != 0;
            let can_occlude = bytes[0] & FACT_CAN_OCCLUDE != 0;
            let octants = match &columns {
                Some(columns) => {
                    let json_flags = int(&columns.flags[state], 31)?;
                    if int(&columns.luminance[state], 15)? != u32::from(bytes[1] >> 4)
                        || int(&columns.light_block[state], 15)? != u32::from(bytes[1] & 0xf)
                        || int(&columns.full_sides[state], 63)? != sturdy
                        || (json_flags & 1 != 0) != (bytes[0] & FACT_IS_AIR != 0)
                        || (json_flags & 4 != 0) != (bytes[0] & FACT_LIQUID != 0)
                    {
                        return Err(Error::InvalidJson);
                    }
                    let boxes = columns.collision[state]
                        .as_arr()
                        .ok_or(Error::InvalidJson)?
                        .iter()
                        .map(|index| {
                            columns
                                .shapes
                                .get(int(index, u32::MAX)? as usize)
                                .copied()
                                .ok_or(Error::InvalidJson)
                        })
                        .collect::<Result<Vec<_>, Error>>()?;
                    if solid_render {
                        0xff
                    } else if can_occlude {
                        covered_octants(&boxes)
                    } else {
                        0
                    }
                }
                // No per-state collision table: treat every occluding state as a
                // full cube rather than guessing a partial shape.
                None => u8::from(can_occlude || solid_render) * 0xff,
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

    /// Number of block states in this registry's pack: `26_684` for 1.21.1,
    /// whatever the pack's facts table holds for any other version.
    pub fn state_count(&self) -> usize {
        self.facts.len()
    }

    /// The namespaced block name of a state, such as `minecraft:oak_slab`.
    pub fn state_name(&self, state: u32) -> Result<&str, Error> {
        if state as usize >= self.facts.len() {
            return Err(Error::InvalidState);
        }
        let block = self.first_states.partition_point(|&first| first <= state) - 1;
        Ok(&self.names[block])
    }

    /// The default state of a namespaced block name.
    pub fn default_state(&self, name: &str) -> Result<u32, Error> {
        self.by_name.get(name).copied().ok_or(Error::NotFound)
    }

    /// The default state of a validated [`ResourceLocation`], or `None` when
    /// the pack does not define the block. `id` is already proven to parse by
    /// the `megu-core` identifier rules, so the only failure mode left is
    /// a name absent from this registry — reported as `None`, never silently
    /// repaired.
    pub fn lookup_resource(&self, id: &ResourceLocation) -> Option<u32> {
        self.by_name.get(id.as_str()).copied()
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
        assert_eq!(registry.state_count(), STATE_COUNT as usize);
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

    // ---------- synthetic multi-version packs ----------

    /// A four-block, six-state pack: air [0], stone [1], water [2..4],
    /// bottom-slab-like occluder [5]. `N != STATE_COUNT` everywhere it matters.
    const SMALL_STATES: usize = 6;

    fn small_blocks_json() -> Vec<u8> {
        br#"[
            {"name": "air", "defaultState": 0, "minStateId": 0, "maxStateId": 0},
            {"name": "stone", "defaultState": 1, "minStateId": 1, "maxStateId": 1},
            {"name": "water", "defaultState": 2, "minStateId": 2, "maxStateId": 4},
            {"name": "half_slab", "defaultState": 5, "minStateId": 5, "maxStateId": 5}
        ]"#
        .to_vec()
    }

    fn fact_row(flags: u8, emission: u8, opacity: u8, sturdy: u32) -> [u8; FACT_BYTES] {
        let sturdy = sturdy & 0x3f;
        [
            flags,
            emission << 4 | opacity,
            sturdy as u8,
            (sturdy >> 8) as u8,
            (sturdy >> 16) as u8,
            0,
        ]
    }

    fn small_facts_bin() -> Vec<u8> {
        [
            fact_row(FACT_IS_AIR | FACT_FLUID_EMPTY, 0, 0, 0),
            fact_row(
                FACT_SOLID_RENDER | FACT_CAN_OCCLUDE | FACT_BLOCKS_MOTION,
                0,
                15,
                0x3f,
            ),
            fact_row(FACT_LIQUID, 0, 0, 0x3e),
            fact_row(FACT_LIQUID, 0, 0, 0x3e),
            fact_row(FACT_LIQUID, 0, 0, 0x3e),
            fact_row(FACT_CAN_OCCLUDE | FACT_BLOCKS_MOTION, 0, 15, 0x3f),
        ]
        .concat()
    }

    /// `block_states.json` agreeing with `small_facts_bin`: the slab gets a
    /// bottom-half collision box (shape 0) so its occlusion shape is partial,
    /// while stone gets the full cube (shape 1).
    fn small_states_json() -> Vec<u8> {
        r#"{
            "format": 1,
            "minecraft": "9.99.test",
            "blocks": [
                ["minecraft:air", 0, 1],
                ["minecraft:stone", 1, 1],
                ["minecraft:water", 2, 3],
                ["minecraft:half_slab", 5, 1]
            ],
            "shapes": [
                {"min": [0.0, 0.0, 0.0], "max": [1.0, 0.5, 1.0]},
                {"min": [0.0, 0.0, 0.0], "max": [1.0, 1.0, 1.0]}
            ],
            "luminance": [0, 0, 0, 0, 0, 0],
            "light_block": [0, 15, 0, 0, 0, 15],
            "flags": [1, 8, 4, 4, 4, 8],
            "full_sides": [0, 63, 62, 62, 62, 63],
            "collision_shapes": [[], [1], [], [], [], [0]]
        }"#
        .as_bytes()
        .to_vec()
    }

    #[test]
    fn lookup_resource_resolves_validated_ids_and_fails_closed() {
        let registry = open();
        let stone = registry.default_state("minecraft:stone").unwrap();
        // An explicit namespace resolves like `default_state`.
        assert_eq!(
            registry.lookup_resource(&ResourceLocation::parse("minecraft:stone").unwrap()),
            Some(stone)
        );
        // An unqualified id defaults to `minecraft:` exactly as vanilla.
        assert_eq!(
            registry.lookup_resource(&ResourceLocation::parse("stone").unwrap()),
            Some(stone)
        );
        // A well-formed name absent from the pack reports `None`.
        assert_eq!(
            registry.lookup_resource(&ResourceLocation::parse("custom_mod:unobtainium").unwrap()),
            None
        );
        // The validated type keeps malformed spellings out entirely.
        assert!(ResourceLocation::parse("a:b:c").is_err());
        assert!(ResourceLocation::parse("minecraft:../escape").is_err());
    }

    #[test]
    fn target_version_pins_the_kernel_protocol() {
        assert_eq!(crate::TARGET_VERSION, crate::TargetVersion::V1_21_1);
        assert_eq!(crate::TARGET_VERSION.protocol() as u32, crate::PROTOCOL);
        let meta = crate::version_meta();
        assert_eq!(
            (meta.id, meta.protocol, meta.data_version),
            ("1.21.1", 767, 3955)
        );
    }

    #[test]
    fn from_parts_loads_a_small_non_pinned_pack() {
        for states in [None, Some(small_states_json().as_slice())] {
            let registry =
                Registry::from_parts(&small_blocks_json(), states, &small_facts_bin()).unwrap();
            assert_eq!(registry.state_count(), SMALL_STATES);
            assert_eq!(registry.block_count(), 4);
            assert_eq!(registry.default_state("minecraft:stone").unwrap(), 1);
            assert_eq!(registry.state_name(4).unwrap(), "minecraft:water");
            let water = registry.facts(4).unwrap();
            assert_eq!(
                (water.fluid_kind, water.fluid_level, water.is_liquid),
                (FLUID_WATER, 2, 1)
            );
            let stone = registry.facts(1).unwrap();
            assert_eq!((stone.opacity, stone.sturdy_faces_mask), (15, 63));
            assert_eq!(registry.facts(0).unwrap().is_air, 1);
            // Every lookup outside the pack's own state count fails closed.
            assert_eq!(registry.facts(6), Err(Error::InvalidState));
            assert_eq!(registry.state_name(6), Err(Error::InvalidState));
            assert_eq!(registry.facts(STATE_COUNT), Err(Error::InvalidState));
        }
    }

    #[test]
    fn from_parts_uses_exact_occlusion_only_with_the_states_table() {
        let exact = Registry::from_parts(
            &small_blocks_json(),
            Some(small_states_json().as_slice()),
            &small_facts_bin(),
        )
        .unwrap();
        let slab = exact.facts(5).unwrap();
        assert_eq!(slab.can_occlude, 1);
        assert_eq!(
            slab.face_occlusion,
            FaceOcclusion::slab(Half::Bottom).quadrants()
        );
        // Without the table the same occluding state is conservatively a cube.
        let coarse = Registry::from_parts(&small_blocks_json(), None, &small_facts_bin()).unwrap();
        assert_eq!(coarse.facts(5).unwrap().face_occlusion, [0xf; 6]);
    }

    #[test]
    fn from_parts_loads_a_pack_larger_than_the_pinned_state_count() {
        // A two-block pack with 27,000 states: more than the pinned 1.21.1 table.
        let large_count = 27_000usize;
        let blocks = br#"[
            {"name": "air", "defaultState": 0, "minStateId": 0, "maxStateId": 0},
            {"name": "bulk", "defaultState": 1, "minStateId": 1, "maxStateId": 26999}
        ]"#
        .to_vec();
        let mut facts = vec![0u8; large_count * FACT_BYTES];
        facts[0] = FACT_IS_AIR | FACT_FLUID_EMPTY;
        for state in 1..large_count {
            facts[state * FACT_BYTES] = FACT_SOLID_RENDER | FACT_CAN_OCCLUDE;
        }
        let registry = Registry::from_parts(&blocks, None, &facts).unwrap();
        assert_eq!(registry.state_count(), large_count);
        assert_eq!(registry.state_name(26_999).unwrap(), "minecraft:bulk");
        assert_eq!(registry.facts(26_999).unwrap().is_solid_render, 1);
        assert_eq!(registry.facts(27_000), Err(Error::InvalidState));
        assert_eq!(registry.state_name(27_000), Err(Error::InvalidState));
        // A section bounded by this registry accepts states beyond the pin.
        crate::PalettedSection::new_single_bounded(26_999, registry.state_count()).unwrap();
        assert!(matches!(
            crate::PalettedSection::new_single(26_999),
            Err(Error::InvalidState)
        ));
    }

    #[test]
    fn from_parts_rejects_misaligned_truncated_and_mismatched_packs() {
        let blocks = small_blocks_json();
        let facts = small_facts_bin();
        // A facts table that is not a whole number of six-byte rows.
        assert!(matches!(
            Registry::from_parts(&blocks, None, &facts[..facts.len() - 1]),
            Err(Error::InvalidJson)
        ));
        // An empty table.
        assert!(matches!(
            Registry::from_parts(&blocks, None, &[]),
            Err(Error::InvalidJson)
        ));
        // A facts table with one state too many.
        let mut longer = facts.clone();
        longer.extend_from_slice(&fact_row(0, 0, 0, 0));
        assert!(matches!(
            Registry::from_parts(&blocks, None, &longer),
            Err(Error::InvalidJson)
        ));
        // A facts table with one state missing.
        assert!(matches!(
            Registry::from_parts(&blocks, None, &facts[..facts.len() - FACT_BYTES]),
            Err(Error::InvalidJson)
        ));
        // An empty block table.
        assert!(matches!(
            Registry::from_parts(b"[]", None, &facts),
            Err(Error::InvalidJson)
        ));
        // A states table whose per-state columns disagree with the facts table.
        let mut bad_states = small_states_json();
        let at = bad_states
            .windows(b"\"light_block\"".len())
            .position(|w| w == b"\"light_block\"")
            .unwrap();
        let colon = bad_states[at..].iter().position(|&b| b == b':').unwrap() + at;
        let open = bad_states[colon..].iter().position(|&b| b == b'[').unwrap() + colon;
        bad_states[open + 1] = b'9'; // luminance of state 0 no longer matches.
        assert!(matches!(
            Registry::from_parts(&blocks, Some(&bad_states), &facts),
            Err(Error::InvalidJson)
        ));
        // Column counts that disagree with the derived state count.
        let short_columns = br#"{
            "format": 1,
            "blocks": [["minecraft:air", 0, 1], ["minecraft:stone", 1, 1],
                       ["minecraft:water", 2, 3], ["minecraft:half_slab", 5, 1]],
            "shapes": [], "luminance": [0], "light_block": [0], "flags": [0],
            "full_sides": [0], "collision_shapes": [[]]
        }"#;
        assert!(matches!(
            Registry::from_parts(&blocks, Some(short_columns), &facts),
            Err(Error::InvalidJson)
        ));
        // A non-format-1 states table is a different schema, not a bad pack.
        let other_format = br#"{
            "format": 2,
            "blocks": [["minecraft:air", 0, 1], ["minecraft:stone", 1, 1],
                       ["minecraft:water", 2, 3], ["minecraft:half_slab", 5, 1]],
            "shapes": [], "luminance": [0,0,0,0,0,0], "light_block": [0,0,0,0,0,0],
            "flags": [0,0,0,0,0,0], "full_sides": [0,0,0,0,0,0],
            "collision_shapes": [[],[],[],[],[],[]]
        }"#;
        assert!(matches!(
            Registry::from_parts(&blocks, Some(other_format), &facts),
            Err(Error::InvalidJson)
        ));
    }

    #[test]
    fn open_json_still_rejects_a_non_pinned_pack() {
        // The 1.21.1 entry point keeps its version pin even though from_parts
        // accepts the same bytes as a generic pack.
        assert!(matches!(
            Registry::open_json(&small_states_json(), &small_blocks_json()),
            Err(Error::InvalidJson)
        ));
    }

    fn version_dir(tagged: bool, with_states: bool) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "megu-kernel-registry-{}-{}-{}",
            std::process::id(),
            tagged,
            with_states
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let name = |neutral: &str| {
            if tagged {
                let (stem, ext) = neutral.split_once('.').unwrap();
                format!("{stem}_{VERSION_ASSET_TAG}.{ext}")
            } else {
                neutral.to_string()
            }
        };
        std::fs::write(dir.join(name("blocks.json")), small_blocks_json()).unwrap();
        std::fs::write(dir.join(name("block_state_facts.bin")), small_facts_bin()).unwrap();
        if with_states {
            std::fs::write(dir.join(name("block_states.json")), small_states_json()).unwrap();
        }
        dir
    }

    #[test]
    fn from_version_dir_resolves_neutral_tagged_and_optional_files() {
        for tagged in [false, true] {
            for with_states in [false, true] {
                let dir = version_dir(tagged, with_states);
                let registry = Registry::from_version_dir(&dir).unwrap();
                assert_eq!(registry.state_count(), SMALL_STATES);
                assert_eq!(registry.default_state("minecraft:water").unwrap(), 2);
                let slab = registry.facts(5).unwrap().face_occlusion;
                if with_states {
                    assert_eq!(slab, FaceOcclusion::slab(Half::Bottom).quadrants());
                } else {
                    assert_eq!(slab, [0xf; 6]);
                }
                std::fs::remove_dir_all(&dir).unwrap();
            }
        }
        // A missing directory and a directory missing required files fail closed.
        let missing = std::env::temp_dir().join(format!(
            "megu-kernel-registry-{}-absent",
            std::process::id()
        ));
        assert!(matches!(
            Registry::from_version_dir(&missing),
            Err(Error::NotFound)
        ));
        let empty = version_dir(false, false);
        std::fs::remove_file(empty.join("blocks.json")).unwrap();
        assert!(matches!(
            Registry::from_version_dir(&empty),
            Err(Error::NotFound)
        ));
        std::fs::remove_dir_all(&empty).unwrap();
    }

    #[test]
    fn from_version_dir_opens_the_pinned_assets_as_a_pack() {
        let registry = Registry::from_version_dir(Path::new(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../assets"
        )))
        .unwrap();
        assert_eq!(registry.state_count(), STATE_COUNT as usize);
        let pinned = open();
        let stone = pinned.default_state("minecraft:stone").unwrap();
        assert_eq!(registry.facts(stone).unwrap(), pinned.facts(stone).unwrap());
        let slab = pinned.default_state("minecraft:oak_slab").unwrap();
        assert_eq!(registry.facts(slab).unwrap(), pinned.facts(slab).unwrap());
    }

    #[test]
    fn pinned_pack_without_states_json_still_loads() {
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../assets/");
        let read = |name| std::fs::read(format!("{dir}{name}")).unwrap();
        // The generated table is preferred; on a stripped export it is absent
        // and the runtime fallback derives the same coarse registry from
        // `blocks.json` alone.
        let blocks = read("blocks_1211.json");
        let facts = std::fs::read(format!("{dir}block_state_facts_1211.bin"))
            .unwrap_or_else(|_| derive_state_facts(&parse(&blocks).unwrap(), None).unwrap());
        let registry = Registry::from_parts(&blocks, None, &facts).unwrap();
        assert_eq!(registry.state_count(), STATE_COUNT as usize);
        let stone = registry
            .facts(registry.default_state("minecraft:stone").unwrap())
            .unwrap();
        assert_eq!(
            (stone.opacity, stone.face_occlusion, stone.sturdy_faces_mask),
            (15, [0xf; 6], 63)
        );
        // A slab keeps its partial occlusion only via block_states.json.
        let slab = registry
            .facts(registry.default_state("minecraft:oak_slab").unwrap())
            .unwrap();
        assert_eq!(slab.face_occlusion, [0xf; 6]);
    }

    #[test]
    fn staged_facts_are_the_first_table_open_json_consults() {
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../assets/");
        let bytes = match std::fs::read(format!("{dir}block_state_facts_1211.bin")) {
            Ok(bytes) => bytes,
            // A stripped export has nothing to stage; the derive path covers it.
            Err(_) => return,
        };
        Registry::load_state_facts_from_bytes(&bytes).unwrap();
        // The first table wins: restaging fails closed, and a table that does
        // not span the pinned state space is never staged.
        assert_eq!(
            Registry::load_state_facts_from_bytes(&bytes),
            Err(Error::InvalidArgument)
        );
        assert_eq!(
            Registry::load_state_facts_from_bytes(&[0u8; FACT_BYTES]),
            Err(Error::InvalidJson)
        );
        let registry = open();
        assert_eq!(registry.state_count(), STATE_COUNT as usize);
        assert_eq!(registry.facts(0).unwrap().is_air, 1);
    }

    #[test]
    fn derived_facts_reproduce_the_pinned_table_columns() {
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../assets/");
        let read = |name| std::fs::read(format!("{dir}{name}")).unwrap();
        let (blocks, states) = (read("blocks_1211.json"), read("block_states_1211.json"));
        let derived = derive_state_facts(
            &parse(&blocks).unwrap(),
            Some(&parse(&states).unwrap()),
        )
        .unwrap();
        let fallback = Registry::from_parts(&blocks, Some(&states), &derived).unwrap();
        let pinned = open();
        // Every kernel-visible field must agree for the structural basics the
        // fallback commits to: air, full cubes, slabs, stairs, fluids and
        // partial occluders with a lone non-full collision box.
        for name in [
            "minecraft:air",
            "minecraft:stone",
            "minecraft:oak_slab",
            "minecraft:oak_stairs",
            "minecraft:water",
            "minecraft:lava",
            "minecraft:glowstone",
            "minecraft:torch",
            "minecraft:kelp",
            "minecraft:dirt_path",
        ] {
            let state = pinned.default_state(name).unwrap();
            assert_eq!(
                fallback.facts(state).unwrap(),
                pinned.facts(state).unwrap(),
                "{name}"
            );
        }
        // The purely structural table marks `air` passable and every other
        // state an opaque full cube.
        let structural = derive_state_facts(&parse(&blocks).unwrap(), None).unwrap();
        let coarse = Registry::from_parts(&blocks, None, &structural).unwrap();
        assert_eq!(coarse.state_count(), STATE_COUNT as usize);
        let air = coarse.facts(0).unwrap();
        assert_eq!((air.is_air, air.blocks_motion, air.can_occlude), (1, 0, 0));
        let stone = coarse
            .facts(coarse.default_state("minecraft:stone").unwrap())
            .unwrap();
        assert_eq!(
            (
                stone.opacity,
                stone.is_solid_render,
                stone.sturdy_faces_mask,
                stone.face_occlusion
            ),
            (15, 1, 63, [0xf; 6])
        );
    }

    #[test]
    fn dynamic_registry_facts_feed_light_and_physics() {
        let registry = Registry::from_parts(
            &small_blocks_json(),
            Some(small_states_json().as_slice()),
            &small_facts_bin(),
        )
        .unwrap();
        // Lighting consumes the pack's fact table directly.
        let fact_of = |state| {
            let facts = registry.facts(state).unwrap();
            crate::LightFact {
                opacity: facts.opacity,
                emission: facts.luminance,
                occlusion: facts.face_occlusion,
            }
        };
        let mut engine = crate::LightEngine::new(0, 16, true).unwrap();
        let mut cells = vec![fact_of(0); 16 * 256];
        cells[8 << 8] = fact_of(1);
        engine.load_chunk(0, 0, &cells).unwrap();
        // Physics consumes the same table's fluid facts.
        let impulse = crate::fluid_impulse(
            crate::Aabb {
                min: [0.25, 0.25, 0.25],
                max: [0.75, 0.75, 0.75],
            },
            crate::FluidQuery {
                kind: crate::FluidKind::Water,
                ultra_warm: false,
                pushed_by_fluid: true,
                is_player: true,
                velocity: [0.0; 3],
            },
            |_, _, _| {
                let facts = registry.facts(2).unwrap();
                Ok(crate::FluidCell {
                    kind: Some(crate::FluidKind::Water),
                    amount: 8 - facts.fluid_level.min(7),
                    falling: facts.fluid_level >= 8,
                    blocks_motion: facts.blocks_motion != 0,
                    is_ice: false,
                    sturdy_faces: facts.sturdy_faces_mask,
                })
            },
        )
        .unwrap();
        assert!(impulse.in_fluid);
    }
}
