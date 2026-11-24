// SPDX-License-Identifier: LGPL-3.0-or-later
//! Live Protocol 767 world cache, block-state classification, `client-port`
//! greedy meshing + vertex packing + translucent sorting, and collision lookup.

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::time::Instant;

use megu_client_mesh::ffi::{
    megu_client_mesh_pack_vertices, megu_client_mesh_sort_translucent, PackedVertex, ABI_OK,
};
use megu_client_mesh::{
    section_index, Block, CompressedSection, DirectionBuckets, FaceVisibility, Layer, Light,
    MeshCache, MissingNeighbor, Quad, ReleasedMeshMeta, RenderRegions, Section, SectionBlocks,
    SectionCoord, SectionGraph, ALL_FACES, CUTOUT, FLUID_TINTED, LAVA, LIGHT_BYTES, NO_FLUID,
    OPAQUE, SAME_STATE_CULL, SECTION_CELLS, TINTED, TRANSLUCENT, WATER,
};
use megu_kernel::skeletal_anim::{
    decode_animation_trigger, parse_animation_json, parse_geometry_json, parse_jem_model,
    parse_jpm_model, AnimationClip, AnimationController, AnimationError, BoneMask, LoopMode,
    SkeletonRig, MAX_CLIPS,
};
use megu_kernel::occlusion_table::{CoalescedLightQueue, QueuedLightEdit};
use megu_kernel::{
    Aabb, FluidCell, FluidKind, LightEngine, LightFact, LightUpdateMultiWord, PalettedSection,
    Registry, STATE_COUNT,
};

use crate::protocol::{
    bitset_get, decode_paletted_container, ChunkBiomeData, DecodedBlockEntity, DecodedChunk,
    DecodedLightData, DimensionMeta, PlayEvent, WireAttribute, WireReader,
};

// ---------------------------------------------------------------------------
// Safe Rust wrappers over client-port vertex packing and translucent sorting
// ---------------------------------------------------------------------------

pub fn pack_vertices(quads: &[Quad]) -> Result<Vec<PackedVertex>, i32> {
    if quads.is_empty() {
        return Ok(Vec::new());
    }
    let mut required = 0usize;
    let query = unsafe {
        megu_client_mesh_pack_vertices(
            quads.as_ptr(),
            quads.len(),
            std::ptr::null_mut(),
            0,
            &mut required,
        )
    };
    if query != ABI_OK {
        return Err(query);
    }
    let mut out = vec![PackedVertex::default(); required];
    let mut written = 0usize;
    let status = unsafe {
        megu_client_mesh_pack_vertices(
            quads.as_ptr(),
            quads.len(),
            out.as_mut_ptr(),
            out.len(),
            &mut written,
        )
    };
    if status != ABI_OK {
        return Err(status);
    }
    out.truncate(written);
    Ok(out)
}

fn packed_vertices_digest(vertices: &[PackedVertex]) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    for vertex in vertices {
        vertex.position.hash(&mut hasher);
        vertex.state_id.hash(&mut hasher);
        vertex.attributes.hash(&mut hasher);
        vertex.tint.hash(&mut hasher);
    }
    hasher.finish()
}

pub fn sort_translucent(quads: &[Quad], camera: [f32; 3]) -> Result<Vec<u32>, i32> {
    if quads.is_empty() {
        return Ok(Vec::new());
    }
    let mut required = 0usize;
    let query = unsafe {
        megu_client_mesh_sort_translucent(
            quads.as_ptr(),
            quads.len(),
            camera[0],
            camera[1],
            camera[2],
            std::ptr::null_mut(),
            0,
            &mut required,
        )
    };
    if query != ABI_OK {
        return Err(query);
    }
    if required == 0 {
        return Ok(Vec::new());
    }
    let mut out = vec![0u32; required];
    let mut written = 0usize;
    let status = unsafe {
        megu_client_mesh_sort_translucent(
            quads.as_ptr(),
            quads.len(),
            camera[0],
            camera[1],
            camera[2],
            out.as_mut_ptr(),
            out.len(),
            &mut written,
        )
    };
    if status != ABI_OK {
        return Err(status);
    }
    out.truncate(written);
    Ok(out)
}

// ---------------------------------------------------------------------------
// Block-state registry (loads assets/block_states_1211.json or uses fallback)
// ---------------------------------------------------------------------------

const FLAG_AIR: u8 = 1;
const FLAG_SOLID: u8 = 8;

#[derive(Clone, Copy, Debug)]
struct LocalBox {
    min: [f64; 3],
    max: [f64; 3],
}

pub struct BlockTable {
    mesh_blocks: Vec<Block>,
    shapes: Vec<LocalBox>,
    collision_shapes: Vec<Vec<u16>>,
    fluid_cells: Vec<FluidCell>,
    air_mask: Vec<bool>,
    /// Per-state registry name (`"minecraft:oak_planks"`) when the table was
    /// built from `block_states_1211.json`; empty in builtin fallback mode.
    state_names: Vec<String>,
    /// O(1) state→material table built once alongside `state_names`
    /// (empty → StoneLike fallback in builtin mode).
    acoustic_lut: crate::spatial_audio::AcousticMaterialLut,
    pub loaded_from_json: bool,
    pub source_path: Option<PathBuf>,
}

impl BlockTable {
    pub fn load(explicit_path: Option<&Path>) -> Self {
        if let Some(path) = explicit_path {
            if let Ok(table) = Self::from_json_file(path) {
                return table;
            }
        }
        let candidates = [
            PathBuf::from("assets/block_states_1211.json"),
            PathBuf::from("../assets/block_states_1211.json"),
        ];
        for candidate in &candidates {
            if candidate.is_file() {
                if let Ok(table) = Self::from_json_file(candidate) {
                    return table;
                }
            }
        }
        if let Ok(exe) = std::env::current_exe() {
            for ancestor in exe.ancestors().skip(1).take(4) {
                let candidate = ancestor.join("assets/block_states_1211.json");
                if candidate.is_file() {
                    if let Ok(table) = Self::from_json_file(&candidate) {
                        return table;
                    }
                }
            }
        }
        Self::builtin()
    }

    pub fn builtin() -> Self {
        let count = STATE_COUNT as usize;
        let shapes = vec![LocalBox {
            min: [0.0, 0.0, 0.0],
            max: [1.0, 1.0, 1.0],
        }];
        let mut mesh_blocks = Vec::with_capacity(count);
        let mut collision_shapes = Vec::with_capacity(count);
        let mut fluid_cells = Vec::with_capacity(count);
        let mut air_mask = Vec::with_capacity(count);

        for state in 0..count {
            let sid = state as u32;
            let (block, col, fcell, is_air) = classify_builtin(sid);
            mesh_blocks.push(block);
            collision_shapes.push(col);
            fluid_cells.push(fcell);
            air_mask.push(is_air);
        }

        Self {
            mesh_blocks,
            shapes,
            collision_shapes,
            fluid_cells,
            air_mask,
            state_names: Vec::new(),
            acoustic_lut: crate::spatial_audio::AcousticMaterialLut::from_state_names(&[]),
            loaded_from_json: false,
            source_path: None,
        }
    }

    pub fn from_json_file(path: &Path) -> Result<Self, String> {
        let raw =
            std::fs::read(path).map_err(|e| format!("failed to read {}: {e}", path.display()))?;
        let mut table = Self::from_json_bytes(&raw)?;
        table.source_path = Some(path.to_path_buf());
        Ok(table)
    }

    pub fn from_json_bytes(raw: &[u8]) -> Result<Self, String> {
        let parsed = parse_block_states_json(raw)?;
        let count = parsed.flags.len();
        if count == 0 || parsed.full_sides.len() != count || parsed.collision_shapes.len() != count
        {
            return Err("inconsistent column lengths in block_states_1211.json".to_string());
        }

        let mut names: Vec<&str> = vec![""; count];
        let mut owned_names: Vec<String> = vec![String::new(); count];
        let mut base_ids: Vec<u32> = vec![0; count];
        for (name, base, len) in &parsed.blocks {
            let start = *base as usize;
            let end = start.saturating_add(*len as usize).min(count);
            for idx in start..end {
                names[idx] = name.as_str();
                owned_names[idx] = name.clone();
                base_ids[idx] = *base;
            }
        }

        let mut mesh_blocks = Vec::with_capacity(count);
        let mut fluid_cells = Vec::with_capacity(count);
        let mut air_mask = Vec::with_capacity(count);

        for idx in 0..count {
            let sid = idx as u32;
            let name = names[idx];
            let short = name.strip_prefix("minecraft:").unwrap_or(name);
            let flag = parsed.flags[idx];
            let full_sides = parsed.full_sides[idx] & ALL_FACES;
            let is_air = flag & FLAG_AIR != 0;
            let has_collision = !parsed.collision_shapes[idx].is_empty();
            let is_ice = short == "ice" || short == "packed_ice" || short == "blue_ice";

            air_mask.push(is_air);

            if is_air {
                mesh_blocks.push(Block::AIR);
                fluid_cells.push(FluidCell::default());
                continue;
            }

            if short == "water" {
                let level = sid.saturating_sub(base_ids[idx]).min(15) as u8;
                let falling = level >= 8;
                let amount = if falling { 8 } else { 8 - level };
                mesh_blocks.push(Block {
                    state_id: sid,
                    fluid_kind: WATER,
                    fluid_level: level,
                    flags: FLUID_TINTED,
                    fluid_tint: 0x3f76e4,
                    ..Block::AIR
                });
                fluid_cells.push(FluidCell {
                    kind: Some(FluidKind::Water),
                    amount,
                    falling,
                    blocks_motion: false,
                    is_ice: false,
                    sturdy_faces: 0,
                });
                continue;
            }

            if short == "lava" {
                let level = sid.saturating_sub(base_ids[idx]).min(15) as u8;
                let falling = level >= 8;
                let amount = if falling { 8 } else { 8 - level };
                mesh_blocks.push(Block {
                    state_id: sid,
                    fluid_kind: LAVA,
                    fluid_level: level,
                    ..Block::AIR
                });
                fluid_cells.push(FluidCell {
                    kind: Some(FluidKind::Lava),
                    amount,
                    falling,
                    blocks_motion: false,
                    is_ice: false,
                    sturdy_faces: 0,
                });
                continue;
            }

            let is_translucent = short.contains("stained_glass")
                || short == "ice"
                || short == "frosted_ice"
                || short == "slime_block"
                || short == "honey_block"
                || short == "tinted_glass"
                || short == "nether_portal";
            let is_tinted = short == "grass_block"
                || short.ends_with("_leaves")
                || short == "short_grass"
                || short == "tall_grass"
                || short == "fern"
                || short == "large_fern"
                || short == "vine"
                || short == "sugar_cane";
            let tint = if is_tinted { 0x7cbd6b } else { 0 };
            let tint_flag = if is_tinted { TINTED } else { 0 };

            let block = if is_translucent {
                Block {
                    state_id: sid,
                    tint,
                    flags: TRANSLUCENT | SAME_STATE_CULL | tint_flag,
                    occlusion: 0,
                    fluid_kind: NO_FLUID,
                    fluid_level: 0,
                    fluid_tint: 0,
                }
            } else if full_sides == ALL_FACES && flag & FLAG_SOLID != 0 && !short.contains("glass")
            {
                Block {
                    state_id: sid,
                    tint,
                    flags: OPAQUE | tint_flag,
                    occlusion: ALL_FACES,
                    fluid_kind: NO_FLUID,
                    fluid_level: 0,
                    fluid_tint: 0,
                }
            } else {
                Block {
                    state_id: sid,
                    tint,
                    flags: CUTOUT | tint_flag,
                    occlusion: if short.contains("glass") || short.ends_with("_leaves") {
                        0
                    } else {
                        full_sides
                    },
                    fluid_kind: NO_FLUID,
                    fluid_level: 0,
                    fluid_tint: 0,
                }
            };
            mesh_blocks.push(block);
            fluid_cells.push(FluidCell {
                kind: None,
                amount: 0,
                falling: false,
                blocks_motion: has_collision,
                is_ice,
                sturdy_faces: full_sides,
            });
        }

        Ok(Self {
            mesh_blocks,
            shapes: parsed.shapes,
            collision_shapes: parsed.collision_shapes,
            fluid_cells,
            air_mask,
            acoustic_lut: crate::spatial_audio::AcousticMaterialLut::from_state_names(&owned_names),
            state_names: owned_names,
            loaded_from_json: true,
            source_path: None,
        })
    }

    pub fn mesh_block(&self, state_id: u32) -> Block {
        self.mesh_blocks
            .get(state_id as usize)
            .copied()
            .unwrap_or_else(|| Block::opaque_cube(state_id))
    }

    pub fn is_air(&self, state_id: u32) -> bool {
        self.air_mask
            .get(state_id as usize)
            .copied()
            .unwrap_or(false)
    }

    /// Registry name for a block state (`"minecraft:oak_planks"`); `None`
    /// for out-of-range states or the builtin fallback table.
    pub fn state_name(&self, state_id: u32) -> Option<&str> {
        self.state_names
            .get(state_id as usize)
            .map(String::as_str)
            .filter(|n| !n.is_empty())
    }

    /// Number of global block states this table covers.
    pub fn state_count(&self) -> usize {
        self.mesh_blocks.len()
    }

    /// Per-state registry names indexed by state id (empty in builtin
    /// fallback mode); bulk source for acoustic/material LUT construction.
    pub fn state_names(&self) -> &[String] {
        &self.state_names
    }

    /// The O(1) acoustic LUT built once when this table loaded — every
    /// ray/snapshot query is a single indexed read, never a name scan.
    pub fn acoustic_lut(&self) -> &crate::spatial_audio::AcousticMaterialLut {
        &self.acoustic_lut
    }

    pub fn fluid_cell(&self, state_id: u32) -> FluidCell {
        self.fluid_cells
            .get(state_id as usize)
            .copied()
            .unwrap_or_default()
    }

    pub fn push_collision_aabbs(
        &self,
        state_id: u32,
        bx: i32,
        by: i32,
        bz: i32,
        out: &mut Vec<Aabb>,
    ) {
        let Some(indices) = self.collision_shapes.get(state_id as usize) else {
            return;
        };
        let ox = f64::from(bx);
        let oy = f64::from(by);
        let oz = f64::from(bz);
        for &idx in indices {
            if let Some(shape) = self.shapes.get(usize::from(idx)) {
                let min = [ox + shape.min[0], oy + shape.min[1], oz + shape.min[2]];
                let max = [ox + shape.max[0], oy + shape.max[1], oz + shape.max[2]];
                if min[0] < max[0] && min[1] < max[1] && min[2] < max[2] {
                    out.push(Aabb { min, max });
                }
            }
        }
    }
}

fn classify_builtin(sid: u32) -> (Block, Vec<u16>, FluidCell, bool) {
    // Canonical 1.21.1 state IDs for key blocks when block_states_1211.json is absent.
    match sid {
        0 => (Block::AIR, Vec::new(), FluidCell::default(), true),
        80..=95 => {
            let level = (sid - 80) as u8;
            let falling = level >= 8;
            let amount = if falling { 8 } else { 8 - level };
            (
                Block {
                    state_id: sid,
                    fluid_kind: WATER,
                    fluid_level: level,
                    flags: FLUID_TINTED,
                    fluid_tint: 0x3f76e4,
                    ..Block::AIR
                },
                Vec::new(),
                FluidCell {
                    kind: Some(FluidKind::Water),
                    amount,
                    falling,
                    blocks_motion: false,
                    is_ice: false,
                    sturdy_faces: 0,
                },
                false,
            )
        }
        96..=111 => {
            let level = (sid - 96) as u8;
            let falling = level >= 8;
            let amount = if falling { 8 } else { 8 - level };
            (
                Block {
                    state_id: sid,
                    fluid_kind: LAVA,
                    fluid_level: level,
                    ..Block::AIR
                },
                Vec::new(),
                FluidCell {
                    kind: Some(FluidKind::Lava),
                    amount,
                    falling,
                    blocks_motion: false,
                    is_ice: false,
                    sturdy_faces: 0,
                },
                false,
            )
        }
        519 => (
            Block {
                state_id: sid,
                flags: CUTOUT,
                occlusion: 0,
                ..Block::AIR
            },
            vec![0],
            FluidCell {
                blocks_motion: true,
                ..FluidCell::default()
            },
            false,
        ),
        2005 => (
            Block {
                state_id: sid,
                flags: CUTOUT | TINTED,
                tint: 0x7cbd6b,
                occlusion: 0,
                ..Block::AIR
            },
            Vec::new(),
            FluidCell::default(),
            false,
        ),
        _ => (
            Block::opaque_cube(sid),
            vec![0],
            FluidCell {
                blocks_motion: true,
                sturdy_faces: ALL_FACES,
                ..FluidCell::default()
            },
            false,
        ),
    }
}

// ---------------------------------------------------------------------------
// assets/block_states_1211.json decoding on the canonical megu_core::json
// reader (the file is machine-generated strict JSON)
// ---------------------------------------------------------------------------

struct ParsedBlockStates {
    blocks: Vec<(String, u32, u32)>,
    flags: Vec<u8>,
    full_sides: Vec<u8>,
    shapes: Vec<LocalBox>,
    collision_shapes: Vec<Vec<u16>>,
}

fn json_u64(value: &megu_core::json::JsonValue, what: &str) -> Result<u64, String> {
    match value {
        megu_core::json::JsonValue::Number(n)
            if n.fract() == 0.0 && *n >= 0.0 && *n <= u64::MAX as f64 =>
        {
            Ok(*n as u64)
        }
        _ => Err(format!(
            "block_states_1211.json: expected integer for {what}"
        )),
    }
}

fn json_string(value: &megu_core::json::JsonValue, what: &str) -> Result<String, String> {
    value
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| format!("block_states_1211.json: expected string for {what}"))
}

fn json_array<'a>(
    value: &'a megu_core::json::JsonValue,
    what: &str,
) -> Result<&'a [megu_core::json::JsonValue], String> {
    value
        .as_array()
        .ok_or_else(|| format!("block_states_1211.json: expected array for {what}"))
}

fn parse_state_block_rows(
    value: &megu_core::json::JsonValue,
) -> Result<Vec<(String, u32, u32)>, String> {
    let items = json_array(value, "blocks")?;
    let mut out = Vec::with_capacity(items.len());
    for (i, item) in items.iter().enumerate() {
        let row = json_array(item, "blocks row").map_err(|_| {
            format!("block_states_1211.json: malformed blocks row {i}")
        })?;
        if row.len() != 3 {
            return Err(format!(
                "block_states_1211.json: blocks row {i} is not a 3-tuple"
            ));
        }
        out.push((
            json_string(&row[0], "blocks row name")?,
            json_u64(&row[1], "blocks row base")? as u32,
            json_u64(&row[2], "blocks row length")? as u32,
        ));
    }
    Ok(out)
}

fn parse_u8_list(value: &megu_core::json::JsonValue, what: &str) -> Result<Vec<u8>, String> {
    json_array(value, what)?
        .iter()
        .enumerate()
        .map(|(i, v)| {
            json_u64(v, what)
                .map(|n| n as u8)
                .map_err(|_| format!("block_states_1211.json: bad {what} entry {i}"))
        })
        .collect()
}

fn parse_shape_list(
    value: &megu_core::json::JsonValue,
) -> Result<Vec<LocalBox>, String> {
    let items = json_array(value, "shapes")?;
    let mut out = Vec::with_capacity(items.len());
    for (i, item) in items.iter().enumerate() {
        if !matches!(item, megu_core::json::JsonValue::Object(_)) {
            return Err(format!(
                "block_states_1211.json: shapes entry {i} is not an object"
            ));
        }
        out.push(LocalBox {
            min: shape_axis(item, "min", i)?,
            max: shape_axis(item, "max", i)?,
        });
    }
    Ok(out)
}

fn shape_axis(
    shape: &megu_core::json::JsonValue,
    key: &str,
    index: usize,
) -> Result<[f64; 3], String> {
    let Some(axis) = shape.get(key) else {
        return Ok([0.0; 3]);
    };
    let items = json_array(axis, "shape axis").map_err(|_| {
        format!("block_states_1211.json: shapes entry {index} has a bad '{key}' axis")
    })?;
    if items.len() != 3 {
        return Err(format!(
            "block_states_1211.json: shapes entry {index} '{key}' is not a triple"
        ));
    }
    let mut out = [0.0; 3];
    for (slot, item) in out.iter_mut().zip(items.iter()) {
        *slot = item.as_f64().ok_or_else(|| {
            format!("block_states_1211.json: shapes entry {index} '{key}' is not numeric")
        })?;
    }
    Ok(out)
}

fn parse_collision_shapes(
    value: &megu_core::json::JsonValue,
) -> Result<Vec<Vec<u16>>, String> {
    let items = json_array(value, "collision_shapes")?;
    let mut out = Vec::with_capacity(items.len());
    for (i, item) in items.iter().enumerate() {
        let indices = json_array(item, "collision_shapes row").map_err(|_| {
            format!("block_states_1211.json: malformed collision_shapes row {i}")
        })?;
        let mut list = Vec::with_capacity(indices.len());
        for (j, index) in indices.iter().enumerate() {
            list.push(
                json_u64(index, "collision_shapes index")
                    .map_err(|_| {
                        format!(
                            "block_states_1211.json: bad collision_shapes[{i}][{j}]"
                        )
                    })? as u16,
            );
        }
        out.push(list);
    }
    Ok(out)
}

fn parse_block_states_json(raw: &[u8]) -> Result<ParsedBlockStates, String> {
    let text = std::str::from_utf8(raw)
        .map_err(|_| "block_states_1211.json is not valid UTF-8".to_string())?;
    // The asset holds ~26k states and ~300k values; bound depth and input but
    // leave the node budget effectively unbounded like the old scanner did.
    let limits = megu_core::json::JsonLimits {
        max_depth: 64,
        max_nodes: usize::MAX,
        max_bytes: usize::MAX,
    };
    let root = megu_core::json::parse_json_with_limits(text, limits, false)
        .map_err(|e| format!("block_states_1211.json: {e}"))?;
    let Some(entries) = root.as_object() else {
        return Err("block_states_1211.json: top level is not an object".to_string());
    };
    let mut parsed = ParsedBlockStates {
        blocks: Vec::new(),
        flags: Vec::new(),
        full_sides: Vec::new(),
        shapes: Vec::new(),
        collision_shapes: Vec::new(),
    };
    for (key, value) in entries {
        match key.as_str() {
            "blocks" => parsed.blocks = parse_state_block_rows(value)?,
            "flags" => parsed.flags = parse_u8_list(value, "flags")?,
            "full_sides" => parsed.full_sides = parse_u8_list(value, "full_sides")?,
            "shapes" => parsed.shapes = parse_shape_list(value)?,
            "collision_shapes" => parsed.collision_shapes = parse_collision_shapes(value)?,
            _ => {}
        }
    }
    Ok(parsed)
}

// ---------------------------------------------------------------------------
// Chunk Column + Live MeshCache World State
// ---------------------------------------------------------------------------

pub struct StoredSection {
    pub palette: PalettedSection,
    pub block_light: [u8; LIGHT_BYTES],
    pub sky_light: [u8; LIGHT_BYTES],
    /// Biome registry ids for the section's 4×4×4 cells (y, z, x order)
    /// — patched in place by `chunks_biomes` and seeded from
    /// `level_chunk_with_light`.
    pub biomes: [u32; 64],
    pub has_non_air: bool,
}

impl StoredSection {
    fn from_states(
        states: &[u32; SECTION_CELLS],
        block_light: [u8; LIGHT_BYTES],
        sky_light: [u8; LIGHT_BYTES],
        blocks: &BlockTable,
        state_count: usize,
    ) -> Result<Self, String> {
        let bound = u32::try_from(state_count.saturating_sub(1)).unwrap_or(u32::MAX);
        let first = states[0].min(bound);
        let mut palette = PalettedSection::new_single_bounded(first, state_count)
            .map_err(|e| format!("PalettedSection::new_single_bounded failed: {e:?}"))?;
        let mut has_non_air = !blocks.is_air(first);
        if states.iter().any(|&s| s != first) {
            for (idx, &raw_state) in states.iter().enumerate() {
                let clamped = raw_state.min(bound);
                if clamped != first {
                    palette
                        .set(idx, clamped)
                        .map_err(|e| format!("PalettedSection::set failed: {e:?}"))?;
                }
                if !blocks.is_air(clamped) {
                    has_non_air = true;
                }
            }
        }
        Ok(Self {
            palette,
            block_light,
            sky_light,
            biomes: [0; 64],
            has_non_air,
        })
    }

    fn to_mesh_section(&self, blocks: &BlockTable) -> Section {
        let mut unpacked = [0u32; SECTION_CELLS];
        self.palette.unpack_into(&mut unpacked);
        let mut mesh_blocks = Box::new([Block::AIR; SECTION_CELLS]);
        for (dst, &sid) in mesh_blocks.iter_mut().zip(unpacked.iter()) {
            *dst = blocks.mesh_block(sid);
        }
        Section {
            blocks: mesh_blocks,
            block_light: self.block_light,
            sky_light: self.sky_light,
        }
    }

    /// The meshing snapshot in [`CompressedSection`] form. A single-state
    /// palette skips the dense block array entirely, so uniform stone,
    /// deepslate, water and air sections enter `MeshCache` with zero
    /// block-storage heap bytes while staying bit-identical to
    /// [`to_mesh_section`](Self::to_mesh_section) plus the
    /// `CompressedSection::from` collapse.
    fn to_compressed_mesh_section(&self, blocks: &BlockTable) -> CompressedSection {
        if let Some(state) = self.palette.uniform_state() {
            return CompressedSection {
                blocks: SectionBlocks::Uniform(blocks.mesh_block(state)),
                block_light: self.block_light,
                sky_light: self.sky_light,
            };
        }
        CompressedSection::from(self.to_mesh_section(blocks))
    }
}

/// [`FaceVisibility::compute`] for a [`CompressedSection`] block array: a
/// uniform section resolves in O(1) to `uniform_air`/`sealed_solid` without
/// materializing the dense block array. `blocks_sight` mirrors the
/// client-port predicate — only a full-occlusion opaque cube stops sight.
fn face_visibility_of(blocks: &SectionBlocks) -> FaceVisibility {
    match blocks {
        SectionBlocks::Uniform(block) => {
            if block.flags & OPAQUE != 0 && block.occlusion & ALL_FACES == ALL_FACES {
                FaceVisibility::sealed_solid()
            } else {
                FaceVisibility::uniform_air()
            }
        }
        SectionBlocks::Dense(dense) => FaceVisibility::compute(dense),
        SectionBlocks::Palette { .. } => {
            let expanded = Box::new(std::array::from_fn(|i| blocks.cell(i)));
            FaceVisibility::compute(&expanded)
        }
    }
}

/// Identity key for one translucent quad inside a released mesh: the bit
/// pattern of its four section-local corner positions plus its face
/// direction. The greedy mesher never emits two quads covering the same
/// face of the same cells, so the key is unique per section.
type TranslucentSlotKey = ([u32; 12], u8);

fn translucent_slot_key(quad: &Quad) -> TranslucentSlotKey {
    let mut bits = [0u32; 12];
    for (dst, pos) in bits.as_chunks_mut::<3>().0.iter_mut().zip(quad.positions) {
        *dst = pos.map(f32::to_bits);
    }
    (bits, quad.direction as u8)
}

/// `client-port`'s crate-private `is_translucent`: the quads the
/// camera-driven re-sort keeps and reorders — translucent blocks and water.
fn quad_is_translucent(quad: &Quad) -> bool {
    quad.layer == Layer::Translucent || (quad.layer == Layer::Fluid && quad.fluid_kind == WATER)
}

pub struct ChunkColumn {
    pub cx: i32,
    pub cz: i32,
    pub sections: Vec<StoredSection>,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct MeshMetrics {
    pub sections_rebuilt: u64,
    pub total_mesh_us: u64,
    pub total_pack_us: u64,
    pub total_sort_us: u64,
    pub total_quads: usize,
    pub packed_vertices: usize,
    pub translucent_quads: usize,
}

impl MeshMetrics {
    pub fn avg_us_per_section(&self) -> f64 {
        if self.sections_rebuilt == 0 {
            0.0
        } else {
            self.total_mesh_us as f64 / self.sections_rebuilt as f64
        }
    }
}

/// Visibility-graph node for one loaded section: the 6x6 `FaceVisibility`
/// reachability matrix plus whether the section can emit geometry for
/// `GraphCuller`.
#[derive(Clone, Copy, Debug)]
struct SectionVisibility {
    faces: FaceVisibility,
    renderable: bool,
}

/// Validated model assets and the deterministic mapping from wire clip IDs to
/// animation names. The first clip in `register_model` is wire clip ID 0.
struct RegisteredAnimationModel {
    rig: SkeletonRig,
    clips: Vec<AnimationClip>,
    clip_names: HashMap<(u16, u8), String>,
    clip_ids: HashMap<String, u16>,
}

struct EntityAnimation {
    model_id: String,
    controller: AnimationController,
}

/// Client-side skeletal animation state. ANM1 packets target entities already
/// bound to registered models; the host never creates entities from packets.
#[derive(Default)]
pub struct WorldAnimationHost {
    models: HashMap<String, RegisteredAnimationModel>,
    entities: HashMap<i32, EntityAnimation>,
}

/// Alternative name for the world's model and entity animation registry.
pub type EntityAnimationRegistry = WorldAnimationHost;

fn loop_mode_id(mode: LoopMode) -> u8 {
    match mode {
        LoopMode::Once => 0,
        LoopMode::Loop => 1,
        LoopMode::HoldOnLastFrame => 2,
    }
}

impl WorldAnimationHost {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register clips in their wire-ID order (0 through `clips.len() - 1`).
    pub fn register_model(
        &mut self,
        model_id: impl Into<String>,
        rig: SkeletonRig,
        clips: Vec<AnimationClip>,
    ) -> Result<(), AnimationError> {
        let numbered = clips
            .into_iter()
            .enumerate()
            .map(|(id, clip)| (id as u16, clip))
            .collect();
        self.register_model_with_clip_ids(model_id, rig, numbered)
    }

    /// Register explicit ANM1 clip IDs. Duplicate model, clip or wire IDs fail
    /// before the registry is modified.
    pub fn register_model_with_clip_ids(
        &mut self,
        model_id: impl Into<String>,
        rig: SkeletonRig,
        mut clips: Vec<(u16, AnimationClip)>,
    ) -> Result<(), AnimationError> {
        let model_id = model_id.into();
        if model_id.is_empty() || self.models.contains_key(&model_id) {
            return Err(AnimationError::InvalidRig);
        }
        if clips.len() > MAX_CLIPS {
            return Err(AnimationError::LimitExceeded);
        }
        clips.sort_by_key(|(id, _)| *id);
        let mut native = Vec::with_capacity(clips.len());
        let mut clip_names = HashMap::new();
        let mut clip_ids = HashMap::new();
        for (id, clip) in &clips {
            if *id > 4095
                || clip_ids.insert(clip.name.clone(), *id).is_some()
                || native
                    .last()
                    .is_some_and(|(previous, _): &(u16, AnimationClip)| previous == id)
            {
                return Err(AnimationError::InvalidClip);
            }
            native.push((*id, clip.clone()));
        }
        // Let the kernel validate tracks, bone names and clip contents before
        // constructing wire-facing variants.
        AnimationController::new(rig.clone(), native.iter().map(|(_, c)| c.clone()).collect())?;

        let mut controller_clips = Vec::with_capacity(MAX_CLIPS.min(clips.len() * 3));
        for (id, mut clip) in native {
            let original_mode = loop_mode_id(clip.loop_mode);
            let internal_name = format!("anm1.{id}.{original_mode}");
            clip.name = internal_name.clone();
            clip_names.insert((id, original_mode), internal_name);
            controller_clips.push(clip);
        }
        // ANM1 chooses loop mode per trigger. Populate alternate modes while
        // respecting the kernel's MAX_CLIPS bound; unavailable overrides fail
        // explicitly instead of silently playing with incorrect loop behavior.
        for (id, clip) in &clips {
            for mode in [LoopMode::Once, LoopMode::Loop, LoopMode::HoldOnLastFrame] {
                if controller_clips.len() == MAX_CLIPS {
                    break;
                }
                let mode_id = loop_mode_id(mode);
                if clip_names.contains_key(&(*id, mode_id)) {
                    continue;
                }
                let mut variant = clip.clone();
                variant.name = format!("anm1.{id}.{mode_id}");
                variant.loop_mode = mode;
                clip_names.insert((*id, mode_id), variant.name.clone());
                controller_clips.push(variant);
            }
        }
        self.models.insert(
            model_id,
            RegisteredAnimationModel {
                rig,
                clips: controller_clips,
                clip_names,
                clip_ids,
            },
        );
        Ok(())
    }

    /// Load a single Bedrock/GeckoLib geometry and animation document.
    /// Parsed clips are sorted by name for stable wire clip IDs.
    pub fn register_model_json(
        &mut self,
        model_id: impl Into<String>,
        geometry_json: &str,
        animation_json: &str,
    ) -> Result<(), AnimationError> {
        let rig = parse_geometry_json(geometry_json)?;
        let mut clips = parse_animation_json(animation_json)?;
        clips.sort_by(|a, b| a.name.cmp(&b.name));
        self.register_model(model_id, rig, clips)
    }

    /// Register an OptiFine `.jem` entity model from a resource pack's
    /// `optifine/cem/` directory. JEM files carry geometry only — the model
    /// binds with no clips and drives poses through bind/entity state.
    pub fn register_jem_model(
        &mut self,
        model_id: impl Into<String>,
        jem_json: &str,
    ) -> Result<(), AnimationError> {
        let rig = parse_jem_model(jem_json)?;
        self.register_model(model_id, rig, Vec::new())
    }

    /// Register an OptiFine `.jpm` part model the same way; the document root
    /// is the part tree itself.
    pub fn register_jpm_model(
        &mut self,
        model_id: impl Into<String>,
        jpm_json: &str,
    ) -> Result<(), AnimationError> {
        let rig = parse_jpm_model(jpm_json)?;
        self.register_model(model_id, rig, Vec::new())
    }

    /// Find the ANM1 clip ID for a model's registered animation name.
    pub fn clip_id(&self, model_id: &str, clip_name: &str) -> Option<u16> {
        self.models.get(model_id)?.clip_ids.get(clip_name).copied()
    }

    /// Bind a spawned entity to a model, with a full-body base layer.
    pub fn bind_entity(&mut self, entity_id: i32, model_id: &str) -> Result<(), AnimationError> {
        let model = self
            .models
            .get(model_id)
            .ok_or(AnimationError::InvalidRig)?;
        let mask = BoneMask::all(&model.rig);
        self.bind_entity_with_layers(entity_id, model_id, vec![(mask, 1.0)])
    }

    /// Bind an entity with ordered layers, e.g. a full-body walk layer and an
    /// upper-body-only cast layer. Binding the same entity replaces its pose.
    pub fn bind_entity_with_layers(
        &mut self,
        entity_id: i32,
        model_id: &str,
        layers: Vec<(BoneMask, f32)>,
    ) -> Result<(), AnimationError> {
        if entity_id < 0 {
            return Err(AnimationError::InvalidWire);
        }
        let model = self
            .models
            .get(model_id)
            .ok_or(AnimationError::InvalidRig)?;
        let mut controller = AnimationController::new(model.rig.clone(), model.clips.clone())?;
        for (mask, weight) in layers {
            controller.add_layer(mask, weight)?;
        }
        self.entities.insert(
            entity_id,
            EntityAnimation {
                model_id: model_id.to_owned(),
                controller,
            },
        );
        Ok(())
    }

    /// Add a masked layer without restarting any playing layers.
    pub fn add_entity_layer(
        &mut self,
        entity_id: i32,
        mask: BoneMask,
        weight: f32,
    ) -> Result<usize, AnimationError> {
        self.entities
            .get_mut(&entity_id)
            .ok_or(AnimationError::InvalidWire)?
            .controller
            .add_layer(mask, weight)
    }

    /// Ingest a complete `megu:anim_v1` (ANM1) custom-payload body.
    /// Protocol fade ticks use Minecraft's 20 ticks per second.
    pub fn ingest_anm1_payload(&mut self, bytes: &[u8]) -> Result<(), AnimationError> {
        let trigger = decode_animation_trigger(bytes)?;
        let entity = self
            .entities
            .get_mut(&trigger.entity_id)
            .ok_or(AnimationError::InvalidWire)?;
        let model = self
            .models
            .get(&entity.model_id)
            .ok_or(AnimationError::InvalidRig)?;
        let name = model
            .clip_names
            .get(&(trigger.clip_id, loop_mode_id(trigger.loop_mode)))
            .ok_or(AnimationError::UnknownClip)?;
        while entity.controller.layers().len() <= usize::from(trigger.layer) {
            entity
                .controller
                .add_layer(BoneMask::all(&model.rig), 1.0)?;
        }
        entity.controller.play(
            usize::from(trigger.layer),
            name,
            f32::from(trigger.fade_ticks) / 20.0,
            trigger.speed,
            trigger.restart,
        )
    }

    pub fn advance(&mut self, dt_sec: f32) -> Result<(), AnimationError> {
        if !dt_sec.is_finite() || dt_sec < 0.0 {
            return Err(AnimationError::InvalidTime);
        }
        for entity in self.entities.values_mut() {
            entity.controller.advance(dt_sec)?;
        }
        Ok(())
    }

    /// One column-major model-space matrix per bone, ordered as in the rig.
    /// Despawned and unbound entities return `Ok(None)`.
    pub fn evaluate_entity_pose(
        &self,
        entity_id: i32,
    ) -> Result<Option<Vec<[f32; 16]>>, AnimationError> {
        self.entities
            .get(&entity_id)
            .map(|entity| entity.controller.evaluate_world_matrices())
            .transpose()
    }

    pub fn despawn_entity(&mut self, entity_id: i32) -> bool {
        self.entities.remove(&entity_id).is_some()
    }

    pub fn clear_entities(&mut self) {
        self.entities.clear();
    }

    pub fn entity_count(&self) -> usize {
        self.entities.len()
    }
}

// ---------------------------------------------------------------------------
// Live entity / block-entity / sky / block-break tracking (Protocol 767 S2C)
// ---------------------------------------------------------------------------

/// Upper bound on simultaneously tracked live entities. Spawn packets past
/// the cap are ignored so a hostile server cannot grow the map unboundedly.
pub const MAX_TRACKED_ENTITIES: usize = 4096;
/// Upper bound on tracked block entities across every loaded column.
pub const MAX_TRACKED_BLOCK_ENTITIES: usize = 8192;
/// Upper bound on concurrent block-break crack overlays (one per breaker).
pub const MAX_BLOCK_DESTRUCTIONS: usize = 256;
/// Vanilla hurt-flash duration in ticks (`Entity#animateHurt` /
/// `hurt_animation`); `entity_event` status 2 sets the same timer.
const HURT_FLASH_TICKS: u8 = 10;
/// `VecDeltaCodec` scale: positions quantize to 1/4096 block on the wire.
const POS_Q_PER_BLOCK: f64 = 4096.0;
/// `game_event` ids for the weather family (vanilla `GameEvent` ordinals).
const GAME_EVENT_START_RAINING: u8 = 1;
const GAME_EVENT_STOP_RAINING: u8 = 2;
const GAME_EVENT_RAIN_LEVEL_CHANGE: u8 = 7;
const GAME_EVENT_THUNDER_LEVEL_CHANGE: u8 = 8;

/// Non-finite wire floats read as `0.0` — the same sanitize convention the
/// kernel's sky/fog evaluator uses — so NaN/inf can never poison tracked
/// entity or weather state.
fn clean_f64(v: f64) -> f64 {
    if v.is_finite() {
        v
    } else {
        0.0
    }
}

fn clean_f32(v: f32) -> f32 {
    if v.is_finite() {
        v
    } else {
        0.0
    }
}

/// Weather levels clamp to `0.0..=1.0`; non-finite input reads as `0.0`.
fn clean_weather_level(v: f32) -> f32 {
    clean_f32(v).clamp(0.0, 1.0)
}

/// Round a block-space value (coordinate or delta) to 1/4096-block units —
/// the same round-half-up the server's `VecDeltaCodec` move encoder uses
/// (`floor(s) + (frac >= 0.5)`), so `pos_q` mirrors the wire quantization.
/// Wire move deltas arrive as `raw / 4096.0`, so `quantize_pos_4096(delta)`
/// recovers the exact i16; absolute sets quantize the coordinate itself.
fn quantize_pos_4096(v: f64) -> i64 {
    let scaled = v * POS_Q_PER_BLOCK;
    if !scaled.is_finite() {
        return 0;
    }
    let floor = scaled.floor();
    (floor + f64::from(scaled - floor >= 0.5)) as i64
}

/// One live entity tracked from the Protocol 767 S2C entity packets
/// (`add_entity`, the `move_entity_*` family, `teleport_entity`,
/// `rotate_head`, `set_entity_motion`, `hurt_animation`, `entity_event`,
/// `set_equipment`, `remove_entities`). Only `add_entity` creates rows;
/// every other packet updates an already-tracked id or is ignored.
#[derive(Clone, Debug)]
pub struct TrackedEntity {
    /// Server entity id.
    pub eid: i32,
    /// Spawn UUID exactly as received on `add_entity`.
    pub uuid: [u8; 16],
    /// Entity-type registry id from `add_entity`.
    pub entity_type: i32,
    /// Position in 1/4096-block fixed point — the client-side `VecDeltaCodec`
    /// accumulator. Absolute sets quantize the coordinate; relative moves add
    /// the decoded delta, so accumulation is exact integer arithmetic.
    pub pos_q: [i64; 3],
    /// Position in blocks — vanilla keeps full f64 precision here.
    pub pos: [f64; 3],
    /// Velocity in blocks/tick (`set_entity_motion`, wire i16 / 8000).
    pub vel: [f64; 3],
    /// Body yaw in degrees (`byte * 360 / 256` on the wire).
    pub yaw_deg: f32,
    /// Pitch in degrees.
    pub pitch_deg: f32,
    /// Head yaw in degrees (`rotate_head`, spawn, `*_pos_rot`, teleport).
    pub head_yaw_deg: f32,
    pub on_ground: bool,
    /// Remaining hurt-flash ticks; set to [`HURT_FLASH_TICKS`] by
    /// `hurt_animation`/`entity_event` status 2 and counted down by
    /// [`WorldState::tick_entities`].
    pub hurt_ticks: u8,
    /// Most recently received vanilla entity animation id (0 main hand,
    /// 3 off hand, 4 critical, 5 magic critical); consumers can animate it.
    pub last_animation: Option<u8>,
    /// Bitmask of equipment slot ids seen in `set_equipment` (bits 0..=7;
    /// vanilla equipment slots are a small fixed enum, higher wire ids are
    /// not representable and ignored).
    pub equipment_slots: u8,
}

/// One block entity tracked from chunk payloads and `block_entity_data`
/// updates, keyed by world `[x, y, z]` in [`WorldState::block_entities`].
#[derive(Clone, Debug)]
pub struct TrackedBlockEntity {
    /// Wire block-entity type id.
    pub kind: i32,
    /// Raw anonymous-NBT payload exactly as received on the wire (empty when
    /// the packet carried the absent-NBT marker).
    pub nbt_bytes: Vec<u8>,
}

/// Crack-overlay state for one block being mined, keyed by breaker entity id
/// in [`WorldState::block_destructions`].
#[derive(Clone, Copy, Debug)]
pub struct BlockBreakProgress {
    /// Block position being mined.
    pub pos: [i32; 3],
    /// Break stage `0..=9` — higher is more cracked. The overlay clears at
    /// stage `>= 10` or the wire `-1` sentinel (255 as `u8`).
    pub stage: u8,
}

/// One `level_particles` spawn request — the vanilla particle emitter
/// parameters (drained by the particle stage, capped at 256 records).
#[derive(Clone, Copy, Debug)]
pub struct LevelParticleSpawn {
    /// Palette particle id.
    pub particle_id: i32,
    /// `true` = visible beyond the normal 32-block particle range.
    pub long_distance: bool,
    /// Spawn position.
    pub pos: [f64; 3],
    /// Per-axis spread (each `0..=` block radius).
    pub offset: [f32; 3],
    /// Particle velocity scale.
    pub max_speed: f32,
    /// Spawn count (`0` = the vanilla single directional burst).
    pub count: i32,
}

pub struct WorldState {
    pub dimension: DimensionMeta,
    pub blocks: BlockTable,
    /// Pinned Vanilla 1.21.1 block registry (luminance, opacity, fluid and
    /// occlusion facts) when `assets/block_states_1211.json` +
    /// `assets/blocks_1211.json` were available, or the `--version-dir` pack
    /// opened via `Registry::from_version_dir`; `None` in fallback mode.
    pub registry: Option<Registry>,
    /// Number of global block-state ids the active registry declares
    /// (`Registry::state_count()` or pinned `STATE_COUNT`); bounds the
    /// `PalettedSection` global palette and every incoming state id.
    pub state_count: usize,
    /// Incremental light engine fed by local block edits; only present when
    /// the registry loaded and the dimension fits the engine window.
    light_engine: Option<LightEngine>,
    /// Tick-local, last-write-wins coalescer (`occlusion_table`) for the
    /// light edits block changes queue: a `SectionBlocksUpdate` batch
    /// drains as one `apply_edits_multi_word` call per touched chunk
    /// instead of one engine call per edit.
    light_queue: CoalescedLightQueue,
    pub columns: HashMap<(i32, i32), ChunkColumn>,
    pub mesh_cache: MeshCache,
    packed_meshes: HashMap<SectionCoord, Vec<PackedVertex>>,
    packed_mesh_digests: HashMap<SectionCoord, u64>,
    /// Monotonic per-section mesh-content revision, bumped on every mesh
    /// rebuild or translucent re-sort so `client-gpu` detects changes with
    /// one `u64` compare instead of byte-comparing `Vec<PackedVertex>`
    /// slices. Entries are removed on chunk unload/evict and survive
    /// [`WorldState::release_uploaded_section_cpu_copies`] so unchanged
    /// frames keep matching the VRAM-resident copy.
    section_mesh_revisions: HashMap<SectionCoord, u64>,
    /// Source of [`section_mesh_revisions`] values. Starts at 1 so `0`
    /// stays a usable "never uploaded" sentinel for consumers, and never
    /// resets — a later section cannot inherit a stale predecessor's
    /// revision across dimension resets.
    next_mesh_revision: u64,
    translucent_orders: HashMap<SectionCoord, Vec<u32>>,
    /// Per-released-section map from a kept translucent quad's position key
    /// to its index in the grouped (packed) order the GPU uploaded. Lets a
    /// camera-driven re-sort of a released section re-derive a valid
    /// `translucent_order` over the VRAM-resident mesh without the deleted
    /// full CPU copy.
    released_translucent_slots:
        HashMap<SectionCoord, HashMap<TranslucentSlotKey, u32>>,
    quad_counts: HashMap<SectionCoord, usize>,
    /// Per-section 6x6 face-to-face visibility graph (including uniform-air
    /// sections) plus the renderable flag fed to `GraphCuller`.
    visibility: HashMap<SectionCoord, SectionVisibility>,
    /// Per-section direction-bucket ranges over the grouped packed mesh.
    direction_buckets: HashMap<SectionCoord, DirectionBuckets>,
    /// Shared 8x4x8-section arena producing batched multi-draw submissions.
    render_regions: RenderRegions,
    pub chunks_decoded: u64,
    pub chunks_unloaded: u64,
    pub light_updates: u64,
    pub block_updates: u64,
    /// Block cells relit by the local `LightEngine` after 0x09/0x49 edits.
    pub local_light_edits: u64,
    pub metrics: MeshMetrics,
    /// Skeletal animation models and live entity controllers.
    pub animations: WorldAnimationHost,
    /// Live entities tracked from S2C entity packets, bounded by
    /// [`MAX_TRACKED_ENTITIES`].
    pub entities: BTreeMap<i32, TrackedEntity>,
    /// Last server-synced attributes per tracked entity.
    pub entity_attributes: BTreeMap<i32, Vec<WireAttribute>>,
    /// Vehicle-to-passenger relationships from `set_passengers`.
    pub passengers: BTreeMap<i32, Vec<i32>>,
    /// Leash/entity attachment target by source entity id.
    pub entity_links: BTreeMap<i32, i32>,
    /// Recent block events retained for a future block-event renderer.
    pub block_events: VecDeque<([i32; 3], u8, u8, i32)>,
    /// Recent `level_event` records `(event, pos, data)` — the vanilla
    /// effect channel (block-break particles 2001, sounds, ...) kept
    /// bounded for the effect renderer like [`Self::block_events`].
    pub level_events: VecDeque<(i32, [i32; 3], i32)>,
    /// Recent `level_particles` spawn records kept bounded the
    /// same way; the particle stage drains them when it lands.
    pub level_particles: VecDeque<LevelParticleSpawn>,
    /// Recent item pickup animations retained for a future entity animation system.
    pub item_pickups: VecDeque<(i32, i32, i32)>,
    /// Block entities retained from chunk payloads and `block_entity_data`
    /// updates, keyed by world `[x, y, z]`, bounded by
    /// [`MAX_TRACKED_BLOCK_ENTITIES`].
    pub block_entities: BTreeMap<[i32; 3], TrackedBlockEntity>,
    /// World age in game ticks (`set_time`).
    pub world_age: i64,
    /// Time of day in game ticks (`set_time`); consumers reduce mod 24000.
    pub time_of_day: i64,
    /// Rain level `0.0..=1.0` driven by weather `game_event`s.
    pub rain_level: f32,
    /// Thunder level `0.0..=1.0` driven by weather `game_event`s.
    pub thunder_level: f32,
    /// Block-break crack overlays keyed by breaker entity id, bounded by
    /// [`MAX_BLOCK_DESTRUCTIONS`].
    pub block_destructions: BTreeMap<i32, BlockBreakProgress>,
}

/// Candidate directories holding the pinned `assets/` pair, mirroring
/// `BlockTable::load`'s search order.
fn kernel_asset_dirs() -> Vec<PathBuf> {
    let mut dirs = vec![PathBuf::from("assets"), PathBuf::from("../assets")];
    if let Ok(exe) = std::env::current_exe() {
        for ancestor in exe.ancestors().skip(1).take(6) {
            dirs.push(ancestor.join("assets"));
        }
    }
    dirs
}

fn try_open_registry(states: &Path, blocks: &Path) -> Option<Registry> {
    let states_json = std::fs::read(states).ok()?;
    let blocks_json = std::fs::read(blocks).ok()?;
    Registry::open_json(&states_json, &blocks_json).ok()
}

/// Load the kernel `Registry` from the asset pair that supplied the block
/// table. Returns `None` (clean fallback) when either file is absent or the
/// pair fails validation.
fn load_kernel_registry(states_path: Option<&Path>) -> Option<Registry> {
    if let Some(states) = states_path {
        if let Some(dir) = states.parent() {
            if let Some(registry) = try_open_registry(states, &dir.join("blocks_1211.json")) {
                return Some(registry);
            }
        }
    }
    for dir in kernel_asset_dirs() {
        if let Some(registry) = try_open_registry(
            &dir.join("block_states_1211.json"),
            &dir.join("blocks_1211.json"),
        ) {
            return Some(registry);
        }
    }
    None
}

fn light_engine_for(dimension: &DimensionMeta, enabled: bool) -> Option<LightEngine> {
    if !enabled {
        return None;
    }
    let height = u32::try_from(dimension.height).ok()?;
    // Windows of <=62 payload sections keep the standard u64-mask engine;
    // taller custom dimensions (up to 128 sections, e.g. min_y=-512 with
    // height=1536) need the multi-word engine so changed-section bits past
    // 63 stay representable.
    if height / 16 <= 62 {
        LightEngine::new(dimension.min_y, height, dimension.has_skylight).ok()
    } else {
        LightEngine::new_tall(dimension.min_y, height, dimension.has_skylight).ok()
    }
}

impl WorldState {
    pub fn new(dimension: DimensionMeta, blocks: BlockTable) -> Self {
        let registry = load_kernel_registry(blocks.source_path.as_deref());
        Self::with_registry(dimension, blocks, registry)
    }

    /// `new` with the kernel `Registry` opened from a `--version-dir` pack
    /// (`blocks.json` + `block_state_facts.bin`, optional
    /// `block_states.json`); the pack's `state_count()` bounds every stored
    /// section's global palette.
    pub fn new_from_version_dir(
        dimension: DimensionMeta,
        blocks: BlockTable,
        dir: &std::path::Path,
    ) -> Result<Self, String> {
        let registry = Registry::from_version_dir(dir)
            .map_err(|e| format!("cannot open version pack {}: {e:?}", dir.display()))?;
        Ok(Self::with_registry(dimension, blocks, Some(registry)))
    }

    fn with_registry(
        dimension: DimensionMeta,
        blocks: BlockTable,
        registry: Option<Registry>,
    ) -> Self {
        let state_count = registry
            .as_ref()
            .map_or(STATE_COUNT as usize, Registry::state_count);
        let light_engine = light_engine_for(&dimension, registry.is_some());
        Self {
            dimension,
            blocks,
            registry,
            state_count,
            light_engine,
            light_queue: CoalescedLightQueue::new(),
            columns: HashMap::new(),
            mesh_cache: MeshCache::new(MissingNeighbor::Air),
            packed_meshes: HashMap::new(),
            packed_mesh_digests: HashMap::new(),
            section_mesh_revisions: HashMap::new(),
            next_mesh_revision: 1,
            translucent_orders: HashMap::new(),
            released_translucent_slots: HashMap::new(),
            quad_counts: HashMap::new(),
            visibility: HashMap::new(),
            direction_buckets: HashMap::new(),
            render_regions: RenderRegions::default(),
            chunks_decoded: 0,
            chunks_unloaded: 0,
            light_updates: 0,
            block_updates: 0,
            local_light_edits: 0,
            metrics: MeshMetrics::default(),
            animations: WorldAnimationHost::new(),
            entities: BTreeMap::new(),
            entity_attributes: BTreeMap::new(),
            passengers: BTreeMap::new(),
            entity_links: BTreeMap::new(),
            block_events: VecDeque::new(),
            level_events: VecDeque::new(),
            level_particles: VecDeque::new(),
            item_pickups: VecDeque::new(),
            block_entities: BTreeMap::new(),
            world_age: 0,
            time_of_day: 0,
            rain_level: 0.0,
            thunder_level: 0.0,
            block_destructions: BTreeMap::new(),
        }
    }

    pub fn reset_dimension(&mut self, dimension: DimensionMeta) {
        self.dimension = dimension;
        self.animations.clear_entities();
        // Dimension changes invalidate live entity, block-entity and
        // block-break state; the world clock (age/time/weather) persists
        // across respawns exactly like vanilla's shared `ClientLevel` data.
        self.entities.clear();
        self.entity_attributes.clear();
        self.passengers.clear();
        self.entity_links.clear();
        self.block_events.clear();
        self.level_events.clear();
        self.level_particles.clear();
        self.item_pickups.clear();
        self.block_entities.clear();
        self.block_destructions.clear();
        self.light_engine = light_engine_for(&self.dimension, self.registry.is_some());
        // A dimension change discards any edits queued against the old
        // world — the fresh engine has no columns for them to apply to.
        self.light_queue = CoalescedLightQueue::new();
        self.columns.clear();
        self.mesh_cache = MeshCache::new(MissingNeighbor::Air);
        self.packed_meshes.clear();
        self.packed_mesh_digests.clear();
        self.section_mesh_revisions.clear();
        self.translucent_orders.clear();
        self.released_translucent_slots.clear();
        self.quad_counts.clear();
        self.visibility.clear();
        self.direction_buckets.clear();
        self.render_regions = RenderRegions::default();
        self.recompute_live_totals();
    }

    pub fn ingest_anm1_payload(&mut self, bytes: &[u8]) -> Result<(), AnimationError> {
        self.animations.ingest_anm1_payload(bytes)
    }

    pub fn advance_animations(&mut self, dt_sec: f32) -> Result<(), AnimationError> {
        self.animations.advance(dt_sec)
    }

    pub fn evaluate_entity_pose(
        &self,
        entity_id: i32,
    ) -> Result<Option<Vec<[f32; 16]>>, AnimationError> {
        self.animations.evaluate_entity_pose(entity_id)
    }

    pub fn despawn_entity(&mut self, entity_id: i32) -> bool {
        self.animations.despawn_entity(entity_id)
    }

    pub fn loaded_chunks(&self) -> usize {
        self.columns.len()
    }

    pub fn pending_mesh_sections(&self) -> usize {
        self.mesh_cache.pending()
    }

    pub fn get_block_state(&self, x: i32, y: i32, z: i32) -> Option<u32> {
        // `min_y`/`height` come from server-sent dimension_type NBT and may
        // be any i32; widen before adding so hostile values cannot overflow.
        let min_y = i64::from(self.dimension.min_y);
        let max_y = min_y + i64::from(self.dimension.height);
        let y = i64::from(y);
        if y < min_y || y >= max_y {
            return Some(0);
        }
        let cx = x.div_euclid(16);
        let cz = z.div_euclid(16);
        let col = self.columns.get(&(cx, cz))?;
        let sec_idx = ((y - min_y) / 16) as usize;
        let sec = col.sections.get(sec_idx)?;
        let idx = section_index(
            x.rem_euclid(16) as usize,
            (y - min_y).rem_euclid(16) as usize,
            z.rem_euclid(16) as usize,
        );
        sec.palette.get(idx).ok()
    }

    pub fn apply_chunk(&mut self, decoded: DecodedChunk) -> Result<(), String> {
        self.apply_chunk_borrowed(&decoded)
    }

    /// Shared chunk-ingest path behind [`Self::apply_chunk`] and the
    /// `ChunkData` arm of [`Self::apply_play_event`]; borrows the decoded
    /// column so event routing does not clone section payloads.
    fn apply_chunk_borrowed(&mut self, decoded: &DecodedChunk) -> Result<(), String> {
        let section_count = decoded.sections.len();
        let default_sky = if self.dimension.has_skylight {
            [0xffu8; LIGHT_BYTES]
        } else {
            [0u8; LIGHT_BYTES]
        };

        let min_sy = self.dimension.min_section_y();
        let mut stored_sections = Vec::with_capacity(section_count);
        let mut sky_cursor = usize::from(bitset_get(&decoded.light.sky_mask, 0));
        let mut block_cursor = usize::from(bitset_get(&decoded.light.block_mask, 0));
        for (idx, sec) in decoded.sections.iter().enumerate() {
            let (sky_update, block_update) = section_light_updates(
                &decoded.light,
                idx + 1,
                &mut sky_cursor,
                &mut block_cursor,
            );
            let sky = sky_update.unwrap_or(default_sky);
            let blk = block_update.unwrap_or([0u8; LIGHT_BYTES]);
            let mut stored =
                StoredSection::from_states(&sec.states, blk, sky, &self.blocks, self.state_count)?;
            stored.biomes = sec.biomes;
            let coord = SectionCoord(decoded.cx, min_sy + idx as i32, decoded.cz);
            if stored.has_non_air {
                let mesh_sec = stored.to_compressed_mesh_section(&self.blocks);
                self.set_section_visibility(coord, face_visibility_of(&mesh_sec.blocks), true);
                self.mesh_cache.insert_compressed(coord, mesh_sec);
            } else {
                self.remove_section_mesh(coord);
                self.set_section_visibility(coord, FaceVisibility::uniform_air(), false);
            }
            stored_sections.push(stored);
        }

        self.columns.insert(
            (decoded.cx, decoded.cz),
            ChunkColumn {
                cx: decoded.cx,
                cz: decoded.cz,
                sections: stored_sections,
            },
        );
        self.refresh_chunk_block_entities(decoded.cx, decoded.cz, &decoded.block_entities);
        self.chunks_decoded += 1;
        self.load_light_chunk(decoded.cx, decoded.cz);
        Ok(())
    }

    /// Feed one decoded column into the local `LightEngine` as `LightFact`
    /// cells ordered y, z, x from the window bottom. Skipped silently when the
    /// engine or registry is absent or the column does not cover the window.
    fn load_light_chunk(&mut self, cx: i32, cz: i32) {
        let (Some(engine), Some(registry), Some(col)) = (
            self.light_engine.as_mut(),
            self.registry.as_ref(),
            self.columns.get(&(cx, cz)),
        ) else {
            return;
        };
        let cells_per_chunk = engine.cells_per_chunk();
        let height_blocks = cells_per_chunk / 256;
        if col.sections.len() * SECTION_CELLS != cells_per_chunk {
            return;
        }
        let bound = u32::try_from(self.state_count.saturating_sub(1)).unwrap_or(u32::MAX);
        let mut cells = Vec::with_capacity(cells_per_chunk);
        for y_off in 0..height_blocks {
            let sy = y_off / 16;
            let ly = y_off % 16;
            let sec = &col.sections[sy];
            for lz in 0..16usize {
                for lx in 0..16usize {
                    let state = sec
                        .palette
                        .get(section_index(lx, ly, lz))
                        .unwrap_or(0)
                        .min(bound);
                    let fact = registry.facts(state).unwrap_or_default();
                    cells.push(LightFact {
                        opacity: fact.opacity,
                        emission: fact.luminance,
                        occlusion: fact.face_occlusion,
                    });
                }
            }
        }
        let _ = engine.load_chunk(cx, cz, &cells);
    }

    /// Enqueue one local block edit's light change into the tick-local
    /// [`CoalescedLightQueue`]. Unknown states are dropped here exactly as
    /// the former per-edit path dropped them; the queue drains through
    /// [`Self::flush_light_queue`] into the same engine + write-back path.
    fn queue_local_light_edit(&mut self, x: i32, y: i32, z: i32, state_id: u32) {
        if self
            .registry
            .as_ref()
            .is_some_and(|registry| registry.facts(state_id).is_ok())
        {
            self.light_queue.push(QueuedLightEdit {
                x,
                y,
                z,
                state_id,
            });
        }
    }

    /// Drain the coalesced queue into the incremental light engine and push
    /// the resulting 2048-byte sky/block arrays into the stored sections +
    /// mesh cache — one `apply_edits_multi_word` call per touched chunk via
    /// `ChunkLightBatch::to_light_edits` rather than one call per edit.
    /// No-op when the engine or registry is unavailable.
    fn flush_light_queue(&mut self) {
        // `drain` runs even without an engine so stranded edits cannot
        // accumulate across packets.
        let batches = self.light_queue.drain();
        if batches.is_empty() || self.light_engine.is_none() {
            return;
        }
        for batch in &batches {
            let edits = match self
                .registry
                .as_ref()
                .map(|registry| batch.to_light_edits(registry))
            {
                Some(Ok(edits)) => edits,
                _ => continue,
            };
            let update = {
                let engine = self.light_engine.as_mut().expect("checked above");
                match engine.apply_edits_multi_word(&edits) {
                    Ok(update) => update,
                    Err(_) => continue,
                }
            };
            self.local_light_edits += edits.len() as u64;
            self.apply_local_light_update(update);
        }
    }

    /// Write engine-produced light sections back into `StoredSection`s and
    /// reinsert changed sections into the `MeshCache`. The masks are
    /// `BitSet.toLongArray`-shaped word lists: bit `i` lives at
    /// `words[i / 64] >> (i % 64)`, so tall-dimension sections past bit 63
    /// are covered by iterating every word.
    fn apply_local_light_update(&mut self, update: LightUpdateMultiWord) {
        let min_sy = self.dimension.min_section_y();
        for chunk_update in update.chunks {
            let (cx, cz) = chunk_update.chunk;
            // Merge sky/block payloads and cleared (all-zero) sections per
            // local section index. Light-section index `i` maps to stored
            // section `i - 1` (index 0 is the guard section below min_y).
            type SectionLightChange = (usize, Option<[u8; LIGHT_BYTES]>, Option<[u8; LIGHT_BYTES]>);
            let mut changes: Vec<SectionLightChange> = Vec::new();
            let mut put =
                |sy: usize, sky: Option<[u8; LIGHT_BYTES]>, block: Option<[u8; LIGHT_BYTES]>| {
                    if let Some(entry) = changes.iter_mut().find(|entry| entry.0 == sy) {
                        if sky.is_some() {
                            entry.1 = sky;
                        }
                        if block.is_some() {
                            entry.2 = block;
                        }
                    } else {
                        changes.push((sy, sky, block));
                    }
                };
            for (light_idx, array) in chunk_update.sky_sections {
                if light_idx >= 1 {
                    put(light_idx - 1, Some(array), None);
                }
            }
            for (light_idx, array) in chunk_update.block_sections {
                if light_idx >= 1 {
                    put(light_idx - 1, None, Some(array));
                }
            }
            for (word_idx, word) in chunk_update.empty_sky_mask.iter().enumerate() {
                let mut bits = *word;
                while bits != 0 {
                    let bit = word_idx * 64 + bits.trailing_zeros() as usize;
                    bits &= bits - 1;
                    if bit >= 1 {
                        put(bit - 1, Some([0u8; LIGHT_BYTES]), None);
                    }
                }
            }
            for (word_idx, word) in chunk_update.empty_block_mask.iter().enumerate() {
                let mut bits = *word;
                while bits != 0 {
                    let bit = word_idx * 64 + bits.trailing_zeros() as usize;
                    bits &= bits - 1;
                    if bit >= 1 {
                        put(bit - 1, None, Some([0u8; LIGHT_BYTES]));
                    }
                }
            }

            let mut dirty = Vec::new();
            {
                let Some(col) = self.columns.get_mut(&(cx, cz)) else {
                    continue;
                };
                for (sy, sky, block) in changes {
                    let Some(sec) = col.sections.get_mut(sy) else {
                        continue;
                    };
                    let mut changed = false;
                    if let Some(array) = sky {
                        if sec.sky_light != array {
                            sec.sky_light = array;
                            changed = true;
                        }
                    }
                    if let Some(array) = block {
                        if sec.block_light != array {
                            sec.block_light = array;
                            changed = true;
                        }
                    }
                    if changed && sec.has_non_air {
                        dirty.push(sy);
                    }
                }
            }
            for sy in dirty {
                let coord = SectionCoord(cx, min_sy + sy as i32, cz);
                let mesh_sec = match self.columns.get(&(cx, cz)) {
                    Some(col) => match col.sections.get(sy) {
                        Some(sec) => sec.to_compressed_mesh_section(&self.blocks),
                        None => continue,
                    },
                    None => continue,
                };
                self.mesh_cache.insert_compressed(coord, mesh_sec);
            }
        }
    }

    pub fn apply_light_update(
        &mut self,
        cx: i32,
        cz: i32,
        light: &DecodedLightData,
    ) -> Result<(), String> {
        self.light_updates += 1;
        let Some(col) = self.columns.get_mut(&(cx, cz)) else {
            return Ok(());
        };
        let min_sy = self.dimension.min_section_y();
        let mut sky_cursor = usize::from(bitset_get(&light.sky_mask, 0));
        let mut block_cursor = usize::from(bitset_get(&light.block_mask, 0));

        for (idx, sec) in col.sections.iter_mut().enumerate() {
            let mut changed = false;
            let (sky_update, block_update) =
                section_light_updates(light, idx + 1, &mut sky_cursor, &mut block_cursor);
            if let Some(new_sky) = sky_update {
                if sec.sky_light != new_sky {
                    sec.sky_light = new_sky;
                    changed = true;
                }
            }
            if let Some(new_blk) = block_update {
                if sec.block_light != new_blk {
                    sec.block_light = new_blk;
                    changed = true;
                }
            }
            if changed && sec.has_non_air {
                let coord = SectionCoord(cx, min_sy + idx as i32, cz);
                let mesh_sec = sec.to_compressed_mesh_section(&self.blocks);
                self.mesh_cache.insert_compressed(coord, mesh_sec);
            }
        }
        Ok(())
    }

    /// Batched form of [`Self::apply_block_update`] for
    /// `SectionBlocksUpdate` packets: every edit lands in the coalesced
    /// light queue last-write-wins, then drains once — one engine call
    /// per touched chunk instead of one call per edit.
    fn apply_block_updates(&mut self, updates: &[([i32; 3], u32)]) -> Result<(), String> {
        let result = updates
            .iter()
            .try_for_each(|&(pos, state_id)| self.apply_block_update_inner(pos, state_id));
        self.flush_light_queue();
        result
    }

    pub fn apply_block_update(&mut self, pos: [i32; 3], state_id: u32) -> Result<(), String> {
        let result = self.apply_block_update_inner(pos, state_id);
        self.flush_light_queue();
        result
    }

    fn apply_block_update_inner(&mut self, pos: [i32; 3], state_id: u32) -> Result<(), String> {
        self.block_updates += 1;
        let [x, y, z] = pos;
        // Widen before adding: `min_y`/`height` are server-controlled i32s.
        let min_y = i64::from(self.dimension.min_y);
        let max_y = min_y + i64::from(self.dimension.height);
        let y64 = i64::from(y);
        if y64 < min_y || y64 >= max_y {
            return Ok(());
        }
        let cx = x.div_euclid(16);
        let cz = z.div_euclid(16);
        let Some(col) = self.columns.get_mut(&(cx, cz)) else {
            return Ok(());
        };
        let sec_idx = ((y64 - min_y) / 16) as usize;
        let Some(sec) = col.sections.get_mut(sec_idx) else {
            return Ok(());
        };

        let lx = x.rem_euclid(16) as usize;
        let ly = (y64 - min_y).rem_euclid(16) as usize;
        let lz = z.rem_euclid(16) as usize;
        let cell_idx = section_index(lx, ly, lz);
        let clamped = state_id
            .min(u32::try_from(self.state_count.saturating_sub(1)).unwrap_or(u32::MAX));
        let previous = sec.palette.get(cell_idx).ok();
        sec.palette
            .set(cell_idx, clamped)
            .map_err(|e| format!("PalettedSection::set failed: {e:?}"))?;
        if previous != Some(clamped) {
            // A real block-state change retires any block entity anchored at
            // the cell (chest broken, sign replaced); `block_entity_data`
            // re-adds it when the server still considers one present.
            self.block_entities.remove(&pos);
        }

        let coord = SectionCoord(cx, self.dimension.min_section_y() + sec_idx as i32, cz);
        let mesh_block = self.blocks.mesh_block(clamped);
        let light = Light::new(
            read_nibble(&sec.block_light, cell_idx),
            read_nibble(&sec.sky_light, cell_idx),
        );

        if !self.blocks.is_air(clamped) {
            if !sec.has_non_air {
                sec.has_non_air = true;
                let mesh_sec = sec.to_compressed_mesh_section(&self.blocks);
                self.set_section_visibility(coord, face_visibility_of(&mesh_sec.blocks), true);
                self.mesh_cache.insert_compressed(coord, mesh_sec);
            } else if self
                .mesh_cache
                .set_cell(coord, [lx, ly, lz], mesh_block, light)
            {
                let mesh_sec = sec.to_compressed_mesh_section(&self.blocks);
                self.set_section_visibility(coord, face_visibility_of(&mesh_sec.blocks), true);
            }
        } else if sec.has_non_air
            && self
                .mesh_cache
                .set_cell(coord, [lx, ly, lz], mesh_block, light)
        {
            let mesh_sec = sec.to_compressed_mesh_section(&self.blocks);
            self.set_section_visibility(coord, face_visibility_of(&mesh_sec.blocks), true);
        }
        self.queue_local_light_edit(x, y, z, clamped);
        Ok(())
    }

    pub fn unload_chunk(&mut self, cx: i32, cz: i32) {
        if let Some(col) = self.columns.remove(&(cx, cz)) {
            let min_sy = self.dimension.min_section_y();
            for idx in 0..col.sections.len() {
                self.remove_section_mesh(SectionCoord(cx, min_sy + idx as i32, cz));
            }
            self.chunks_unloaded += 1;
            self.recompute_live_totals();
        }
        self.forget_chunk(cx, cz);
    }

    /// Distance governor: unload every column whose Chebyshev distance from
    /// `(center_cx, center_cz)` exceeds `view_distance_chunks + 2` — the
    /// same two-chunk hysteresis margin vanilla keeps past the render
    /// border so chunks do not thrash while the player hovers near an
    /// edge. Eviction is RAM-only: the column, its section meshes,
    /// visibility/render-region artifacts and tracked block entities are
    /// dropped via [`WorldState::unload_chunk`] with zero disk writes, and
    /// `chunks_unloaded` accounts for each one. Live entities stay tracked
    /// — like vanilla, their lifecycle is server-driven (`remove_entities`),
    /// not chunk-driven. Returns how many columns were evicted; intended
    /// to run on a slow cadence (about once a second), not per frame.
    pub fn evict_far_chunks(
        &mut self,
        center_cx: i32,
        center_cz: i32,
        view_distance_chunks: i32,
    ) -> usize {
        let keep_radius = i64::from(view_distance_chunks).saturating_add(2).max(0);
        let victims: Vec<(i32, i32)> = self
            .columns
            .keys()
            .copied()
            .filter(|&(cx, cz)| {
                (i64::from(cx) - i64::from(center_cx))
                    .abs()
                    .max((i64::from(cz) - i64::from(center_cz)).abs())
                    > keep_radius
            })
            .collect();
        let evicted = victims.len();
        for (cx, cz) in victims {
            self.unload_chunk(cx, cz);
        }
        evicted
    }

    /// Drop every tracked block entity inside chunk column `(cx, cz)`. Runs on
    /// every chunk unload — even when the column itself was absent, since
    /// `block_entity_data` can anchor records to not-yet-loaded chunks — and
    /// internally before a chunk payload replaces its block-entity set, so
    /// block entities never outlive their column.
    pub fn forget_chunk(&mut self, cx: i32, cz: i32) {
        self.block_entities
            .retain(|pos, _| pos[0].div_euclid(16) != cx || pos[2].div_euclid(16) != cz);
    }

    /// Replace the tracked block-entity set of column `(cx, cz)` with the
    /// records carried by a freshly applied chunk payload. The wire format
    /// packs chunk-local X/Z into one byte (`x = packed >> 4`,
    /// `z = packed & 0x0f`) and carries absolute world Y as `i16`.
    fn refresh_chunk_block_entities(
        &mut self,
        cx: i32,
        cz: i32,
        entities: &[DecodedBlockEntity],
    ) {
        self.forget_chunk(cx, cz);
        for be in entities {
            if self.block_entities.len() >= MAX_TRACKED_BLOCK_ENTITIES {
                break;
            }
            // Saturating: `cx`/`cz` are server-controlled i32s, so `cx * 16`
            // can overflow for absurd coordinates; saturation keeps the
            // position deterministic instead of panicking in debug builds.
            let x = cx
                .saturating_mul(16)
                .saturating_add(i32::from(be.packed_xz >> 4));
            let z = cz
                .saturating_mul(16)
                .saturating_add(i32::from(be.packed_xz & 0x0f));
            self.block_entities.insert(
                [x, i32::from(be.y), z],
                TrackedBlockEntity {
                    kind: be.kind,
                    nbt_bytes: be.nbt_bytes.clone(),
                },
            );
        }
    }

    fn remove_section_mesh(&mut self, coord: SectionCoord) {
        self.mesh_cache.remove(coord);
        self.packed_meshes.remove(&coord);
        self.packed_mesh_digests.remove(&coord);
        self.section_mesh_revisions.remove(&coord);
        self.translucent_orders.remove(&coord);
        self.released_translucent_slots.remove(&coord);
        self.quad_counts.remove(&coord);
        self.direction_buckets.remove(&coord);
        self.visibility.remove(&coord);
        self.render_regions.remove(coord);
    }

    /// Record the visibility-graph node for one loaded section.
    /// `renderable` mirrors `StoredSection::has_non_air`: the section can emit
    /// geometry toward `GraphCuller`'s `visible` set.
    fn set_section_visibility(
        &mut self,
        coord: SectionCoord,
        faces: FaceVisibility,
        renderable: bool,
    ) {
        self.visibility
            .insert(coord, SectionVisibility { faces, renderable });
    }

    /// Run a bounded greedy-meshing pass on dirty sections, then pack vertices
    /// (16 bytes/vertex, 4 vertices/quad) and sort translucent quads back-to-front.
    /// Ordering falls back to pure camera distance; see
    /// [`rebuild_dirty_bounded_ordered`](Self::rebuild_dirty_bounded_ordered).
    pub fn rebuild_dirty_bounded(
        &mut self,
        limit: usize,
        camera: [f32; 3],
    ) -> Result<usize, String> {
        self.rebuild_dirty_bounded_ordered(limit, camera, &[])
    }

    /// Same as [`rebuild_dirty_bounded`](Self::rebuild_dirty_bounded) but the
    /// dirty queue is drained in `MeshCache::rebuild_dirty_ordered` order:
    /// sections inside the camera frustum first, then nearest to farthest.
    /// `planes` are the six view planes in `box_in_frustum` form
    /// (`dot(n, p) + d >= 0` inside); an empty slice degrades to pure distance
    /// ordering. Afterwards a threshold-gated `MeshCache::resort_translucent`
    /// re-derives renderer artifacts only for sections whose translucent quad
    /// order actually changed — no full re-sort while the camera stays inside
    /// `MeshCache::resort_threshold` of the last sort origin.
    pub fn rebuild_dirty_bounded_ordered(
        &mut self,
        limit: usize,
        camera: [f32; 3],
        planes: &[[f32; 4]],
    ) -> Result<usize, String> {
        let mut refresh: BTreeSet<SectionCoord> = BTreeSet::new();
        let mut rebuilt_count = 0usize;
        if limit != 0 && self.mesh_cache.pending() != 0 {
            let t_mesh = Instant::now();
            let rebuilt = self.mesh_cache.rebuild_dirty_ordered(limit, camera, planes);
            let mesh_us = t_mesh.elapsed().as_micros() as u64;
            self.metrics.total_mesh_us += mesh_us;
            rebuilt_count = rebuilt.len();
            self.metrics.sections_rebuilt += rebuilt_count as u64;
            refresh.extend(rebuilt);
        }

        // Incremental translucent re-sort, gated on camera displacement.
        // Under the resort threshold this returns an empty list and costs only
        // the distance check; dirty sections are skipped inside the cache.
        let t_sort = Instant::now();
        let resorted = self.mesh_cache.resort_translucent(camera);
        self.metrics.total_sort_us += t_sort.elapsed().as_micros() as u64;
        refresh.extend(resorted);

        for coord in refresh {
            self.derive_section_artifacts(coord, camera)?;
        }

        self.recompute_live_totals();
        Ok(rebuilt_count)
    }

    /// Refresh every renderer-facing artifact for one clean meshed section:
    /// direction-bucket ranges over the grouped quad order, the packed
    /// 16-byte vertices, the shared-arena render-region upload and the
    /// translucent sort order. Idempotent. Every call issues a new
    /// [`WorldState::section_mesh_revisions`] value — rebuilds and
    /// translucent re-sorts both change what the GPU should draw.
    fn derive_section_artifacts(
        &mut self,
        coord: SectionCoord,
        camera: [f32; 3],
    ) -> Result<(), String> {
        let revision = self.next_mesh_revision;
        self.next_mesh_revision = self.next_mesh_revision.wrapping_add(1);
        self.section_mesh_revisions.insert(coord, revision);

        if self.mesh_cache.released_mesh_meta(coord).is_some() {
            // The opaque CPU payloads are released and the VRAM copy is
            // authoritative: `mesh(coord)` now holds only the kept
            // translucent re-sort quads, so re-deriving the full artifacts
            // would resurrect a translucent-only mesh as if it were the
            // whole section. Only the translucent order may move here.
            self.refresh_released_translucent_order(coord);
            return Ok(());
        }
        self.released_translucent_slots.remove(&coord);

        let (grouped, buckets) = {
            let quads = self.mesh_cache.mesh(coord).unwrap_or(&[]);
            DirectionBuckets::group(quads)
        };
        self.quad_counts.insert(coord, grouped.len());
        self.direction_buckets.insert(coord, buckets);
        self.render_regions.upload(coord, &grouped);

        let t_pack = Instant::now();
        let packed = pack_vertices(&grouped)
            .map_err(|code| format!("pack_vertices failed with ABI code {code}"))?;
        self.metrics.total_pack_us += t_pack.elapsed().as_micros() as u64;
        debug_assert_eq!(packed.len(), grouped.len() * 4);
        self.packed_mesh_digests
            .insert(coord, packed_vertices_digest(&packed));
        self.packed_meshes.insert(coord, packed);

        let local_cam = [
            camera[0] - (coord.0 * 16) as f32,
            camera[1] - (coord.1 * 16) as f32,
            camera[2] - (coord.2 * 16) as f32,
        ];
        let t_sort = Instant::now();
        let order = sort_translucent(&grouped, local_cam)
            .map_err(|code| format!("sort_translucent failed with ABI code {code}"))?;
        self.metrics.total_sort_us += t_sort.elapsed().as_micros() as u64;
        self.translucent_orders.insert(coord, order);
        Ok(())
    }

    /// Re-derive `translucent_orders` for a section whose opaque CPU
    /// payloads were released to VRAM. `mesh_cache.mesh(coord)` already
    /// holds the kept translucent quads in the new back-to-front order; the
    /// [`WorldState::released_translucent_slots`] map translates each kept
    /// quad back to its index in the grouped mesh the GPU uploaded, so the
    /// emitted order still indexes the VRAM-resident packed mesh. When the
    /// release bypassed [`WorldState::release_uploaded_section_cpu_copies`]
    /// no slot map exists and the previous order is kept — it still indexes
    /// real translucent quads, only its ordering is stale.
    fn refresh_released_translucent_order(&mut self, coord: SectionCoord) {
        let Some(slots) = self.released_translucent_slots.get(&coord) else {
            return;
        };
        let kept = self.mesh_cache.mesh(coord).unwrap_or(&[]);
        let order = kept
            .iter()
            .map(|quad| {
                slots
                    .get(&translucent_slot_key(quad))
                    .copied()
                    .unwrap_or(u32::MAX)
            })
            .collect();
        self.translucent_orders.insert(coord, order);
    }

    /// Visibility-graph nodes for `GraphCuller::search`: every loaded
    /// section — including uniform-air sections, which propagate sight but
    /// are never renderable — with its 6x6 face-reachability mask and
    /// renderable flag.
    pub fn section_graphs(&self) -> impl Iterator<Item = (SectionCoord, SectionGraph)> + '_ {
        self.visibility.iter().map(|(&coord, &entry)| {
            (
                coord,
                SectionGraph {
                    through: entry.faces.bits(),
                    renderable: entry.renderable,
                },
            )
        })
    }

    /// Direction-bucket ranges for one section, in grouped packed-vertex
    /// order. `None` until the section's first rebuild.
    pub fn direction_buckets(&self, coord: SectionCoord) -> Option<DirectionBuckets> {
        self.direction_buckets.get(&coord).copied()
    }

    /// Shared render-region arena for multi-draw command batching.
    pub fn render_regions(&self) -> &RenderRegions {
        &self.render_regions
    }

    /// Mesh `Section`s of one loaded chunk column, bottom to top, ready for
    /// `summarize_column` LOD ingestion. `None` when the column is not
    /// loaded.
    pub fn column_sections(&self, cx: i32, cz: i32) -> Option<Vec<Section>> {
        let column = self.columns.get(&(cx, cz))?;
        Some(
            column
                .sections
                .iter()
                .map(|section| section.to_mesh_section(&self.blocks))
                .collect(),
        )
    }

    /// Packed 16-byte vertices for one section — the per-coordinate
    /// counterpart of [`packed_meshes`](Self::packed_meshes) that avoids
    /// materializing a lookup table per frame.
    pub fn packed_mesh(&self, coord: SectionCoord) -> Option<&[PackedVertex]> {
        self.packed_meshes.get(&coord).map(Vec::as_slice)
    }

    /// Digest computed at pack time for the section's packed vertex bytes.
    pub fn packed_mesh_digest(&self, coord: SectionCoord) -> Option<u64> {
        self.packed_mesh_digests.get(&coord).copied()
    }

    /// Iterate rebuilt sections as `(coord, packed 16-byte vertices)` for the
    /// software rasterizer. Four vertices per quad, corner order preserved,
    /// quads in direction-bucket grouped order.
    pub fn packed_meshes(&self) -> impl Iterator<Item = (SectionCoord, &[PackedVertex])> {
        self.packed_meshes
            .iter()
            .map(|(coord, verts)| (*coord, verts.as_slice()))
    }

    /// `sort_translucent` quad indices for one section, back-to-front over the
    /// grouped packed-vertex order.
    pub fn translucent_order(&self, coord: SectionCoord) -> Option<&[u32]> {
        self.translucent_orders.get(&coord).map(Vec::as_slice)
    }

    /// O(1) mesh-change detection for `client-gpu`: the last mesh-content
    /// revision issued for `coord`, bumped on every rebuild or translucent
    /// re-sort. `None` when the section is not loaded or was never meshed.
    /// Replaces byte-comparing `PackedVertex` slices with one `u64` compare.
    pub fn section_mesh_revision(&self, coord: SectionCoord) -> Option<u64> {
        self.section_mesh_revisions.get(&coord).copied()
    }

    /// Dense block-array bytes still retained by the mesh cache.
    pub fn mesh_cache_dense_bytes(&self) -> usize {
        self.mesh_cache.mesh_cache_dense_bytes()
    }

    /// Number of sections still holding their CPU `Vec<PackedVertex>` copy.
    /// Sections released through
    /// [`release_uploaded_section_cpu_copies`](Self::release_uploaded_section_cpu_copies)
    /// and sections not yet meshed are not counted.
    pub fn packed_mesh_count(&self) -> usize {
        self.packed_meshes.len()
    }

    /// Metadata kept for a section whose opaque CPU quads were released
    /// after VRAM upload; `None` while the full CPU mesh is still held.
    /// See [`MeshCache::released_mesh_meta`].
    pub fn released_mesh_meta(&self, coord: SectionCoord) -> Option<&ReleasedMeshMeta> {
        self.mesh_cache.released_mesh_meta(coord)
    }

    /// Post-upload zero-duplication release: once `coord`'s vertex data is
    /// registered for VRAM upload, drops both CPU copies of its opaque
    /// geometry — the `Vec<PackedVertex>` and the `MeshCache`'s opaque
    /// `Vec<Quad>`, which keeps only translucent/water quads for
    /// camera-driven re-sorts plus a [`ReleasedMeshMeta`].
    /// [`section_mesh_revisions`](Self::section_mesh_revision) stays intact
    /// so unchanged frames keep matching the VRAM-resident copy instead of
    /// re-uploading. A later block edit re-dirties the section and the next
    /// rebuild re-populates `packed_meshes` under a strictly higher
    /// revision. Returns the total CPU bytes freed.
    pub fn release_uploaded_section_cpu_copies(&mut self, coord: SectionCoord) -> usize {
        // Capture the translucent-quad -> grouped-index map only on the
        // first release, while the full mesh is still resident:
        // `mesh(coord)` shrinks to the kept translucent quads afterwards.
        if self.mesh_cache.released_mesh_meta(coord).is_none() {
            if let Some(quads) = self.mesh_cache.mesh(coord) {
                if quads.iter().any(quad_is_translucent) {
                    let (grouped, _) = DirectionBuckets::group(quads);
                    let slots: HashMap<TranslucentSlotKey, u32> = grouped
                        .iter()
                        .enumerate()
                        .filter(|(_, quad)| quad_is_translucent(quad))
                        .map(|(index, quad)| (translucent_slot_key(quad), index as u32))
                        .collect();
                    self.released_translucent_slots.insert(coord, slots);
                }
            }
        }
        let quad_bytes = self.mesh_cache.release_uploaded_opaque_cpu_mesh(coord);
        let packed_bytes = if self.mesh_cache.released_mesh_meta(coord).is_some() {
            self.packed_mesh_digests.remove(&coord);
            self.packed_meshes
                .remove(&coord)
                .map_or(0, |verts| {
                    verts.len() * std::mem::size_of::<PackedVertex>()
                })
        } else {
            0
        };
        quad_bytes + packed_bytes
    }

    fn recompute_live_totals(&mut self) {
        self.metrics.total_quads = self.quad_counts.values().sum();
        self.metrics.packed_vertices = self.packed_meshes.values().map(Vec::len).sum();
        self.metrics.translucent_quads = self.translucent_orders.values().map(Vec::len).sum();
    }

    pub fn collect_collision_aabbs(&self, sweep: Aabb) -> Vec<Aabb> {
        // `as i32` saturates at the extremes; saturating ±1 keeps the bound
        // arithmetic from overflowing (and wrapping the range empty in
        // release while panicking in debug).
        let min_x = (sweep.min[0].floor() as i32).saturating_sub(1);
        let max_x = (sweep.max[0].floor() as i32).saturating_add(1);
        let min_y = (sweep.min[1].floor() as i32).saturating_sub(1);
        let max_y = (sweep.max[1].floor() as i32).saturating_add(1);
        let min_z = (sweep.min[2].floor() as i32).saturating_sub(1);
        let max_z = (sweep.max[2].floor() as i32).saturating_add(1);

        let mut shapes = Vec::new();
        for bx in min_x..=max_x {
            for bz in min_z..=max_z {
                for by in min_y..=max_y {
                    if let Some(state) = self.get_block_state(bx, by, bz) {
                        if state != 0 {
                            self.blocks
                                .push_collision_aabbs(state, bx, by, bz, &mut shapes);
                        }
                    }
                }
            }
        }
        shapes
    }

    pub fn fluid_cell_at(&self, x: i32, y: i32, z: i32) -> FluidCell {
        self.get_block_state(x, y, z)
            .map(|s| self.blocks.fluid_cell(s))
            .unwrap_or_default()
    }

    /// Unified ingestion for decoded Protocol 767 play events: routes every
    /// world-relevant family — chunks, block/light updates, entities, block
    /// entities, world time/weather, and block-break overlays — into the live
    /// [`WorldState`] deterministically. Player/session concerns (teleport,
    /// health, chat, containers, HUD, particles) and unrecognized payloads
    /// are acknowledged and ignored; the caller keeps owning those.
    pub fn apply_play_event(&mut self, event: &PlayEvent) -> Result<(), String> {
        match event {
            PlayEvent::JoinGame { dimension, .. } | PlayEvent::Respawn { dimension } => {
                self.reset_dimension(dimension.clone());
            }
            PlayEvent::ChunkData(chunk) => self.apply_chunk_borrowed(chunk)?,
            PlayEvent::UpdateLight { cx, cz, light } => {
                self.apply_light_update(*cx, *cz, light)?;
            }
            PlayEvent::BlockUpdate { pos, state_id } => {
                self.apply_block_update(*pos, *state_id)?;
            }
            PlayEvent::SectionBlocksUpdate(updates) => {
                self.apply_block_updates(updates)?;
            }
            PlayEvent::UnloadChunk { cx, cz } => self.unload_chunk(*cx, *cz),
            PlayEvent::AddEntity {
                eid,
                uuid,
                entity_type,
                x,
                y,
                z,
                pitch,
                yaw,
                head_yaw,
                vx,
                vy,
                vz,
                ..
            } => {
                // Bounded: a full table ignores excess spawns; re-spawning a
                // tracked id replaces its row without growing the map.
                if self.entities.contains_key(eid)
                    || self.entities.len() < MAX_TRACKED_ENTITIES
                {
                    let pos = [clean_f64(*x), clean_f64(*y), clean_f64(*z)];
                    self.entities.insert(
                        *eid,
                        TrackedEntity {
                            eid: *eid,
                            uuid: *uuid,
                            entity_type: *entity_type,
                            pos_q: pos.map(quantize_pos_4096),
                            pos,
                            vel: [clean_f64(*vx), clean_f64(*vy), clean_f64(*vz)],
                            yaw_deg: clean_f32(*yaw),
                            pitch_deg: clean_f32(*pitch),
                            head_yaw_deg: clean_f32(*head_yaw),
                            on_ground: false,
                            hurt_ticks: 0,
                            last_animation: None,
                            equipment_slots: 0,
                        },
                    );
                }
            }
            PlayEvent::RemoveEntities { eids } => {
                for &eid in eids {
                    self.entities.remove(&eid);
                    self.entity_attributes.remove(&eid);
                    self.passengers.remove(&eid);
                    self.entity_links.remove(&eid);
                    for passengers in self.passengers.values_mut() {
                        passengers.retain(|&passenger| passenger != eid);
                    }
                    // A removed breaker stops cracking blocks.
                    self.block_destructions.remove(&eid);
                }
            }
            PlayEvent::MoveEntityPos {
                eid,
                dx,
                dy,
                dz,
                on_ground,
            } => self.move_tracked_entity(*eid, [*dx, *dy, *dz], None, *on_ground),
            PlayEvent::MoveEntityPosRot {
                eid,
                dx,
                dy,
                dz,
                yaw,
                pitch,
                on_ground,
            } => self.move_tracked_entity(
                *eid,
                [*dx, *dy, *dz],
                Some((*yaw, *pitch)),
                *on_ground,
            ),
            PlayEvent::MoveEntityRot {
                eid,
                yaw,
                pitch,
                on_ground,
            } => self.move_tracked_entity(*eid, [0.0; 3], Some((*yaw, *pitch)), *on_ground),
            PlayEvent::TeleportEntity {
                eid,
                x,
                y,
                z,
                yaw,
                pitch,
                on_ground,
            } => self.teleport_tracked_entity(*eid, [*x, *y, *z], *yaw, *pitch, *on_ground),
            PlayEvent::RotateHead { eid, head_yaw } => {
                if let Some(entity) = self.entities.get_mut(eid) {
                    entity.head_yaw_deg = clean_f32(*head_yaw);
                }
            }
            PlayEvent::SetEntityMotion { eid, vx, vy, vz } => {
                if let Some(entity) = self.entities.get_mut(eid) {
                    entity.vel = [clean_f64(*vx), clean_f64(*vy), clean_f64(*vz)];
                }
            }
            // `entity_event` status 2 is the vanilla hurt flash.
            PlayEvent::EntityEvent { eid, status } if *status == 2 => {
                if let Some(entity) = self.entities.get_mut(eid) {
                    entity.hurt_ticks = HURT_FLASH_TICKS;
                }
            }
            PlayEvent::HurtAnimation { eid, .. } => {
                if let Some(entity) = self.entities.get_mut(eid) {
                    entity.hurt_ticks = HURT_FLASH_TICKS;
                }
            }
            // `damage_event` (1.19.4+) reports the same hurt the legacy
            // `hurt_animation` carried — flash the victim identically.
            PlayEvent::DamageEvent { eid, .. } => {
                if let Some(entity) = self.entities.get_mut(eid) {
                    entity.hurt_ticks = HURT_FLASH_TICKS;
                }
            }
            PlayEvent::Animate { eid, animation } => {
                if let Some(entity) = self.entities.get_mut(eid) {
                    entity.last_animation = Some(*animation);
                }
            }
            PlayEvent::UpdateAttributes { eid, attributes } => {
                if self.entity_attributes.contains_key(eid)
                    || self.entity_attributes.len() < MAX_TRACKED_ENTITIES
                {
                    self.entity_attributes.insert(*eid, attributes.clone());
                }
            }
            PlayEvent::SetPassengers {
                vehicle,
                passengers,
            } => {
                if self.passengers.contains_key(vehicle)
                    || self.passengers.len() < MAX_TRACKED_ENTITIES
                {
                    self.passengers.insert(*vehicle, passengers.clone());
                }
            }
            PlayEvent::SetEntityLink {
                source,
                destination,
            } => {
                if self.entity_links.contains_key(source)
                    || self.entity_links.len() < MAX_TRACKED_ENTITIES
                {
                    self.entity_links.insert(*source, *destination);
                }
            }
            PlayEvent::BlockEvent {
                pos,
                action,
                parameter,
                block_type,
            } => {
                if self.block_events.len() == 64 {
                    self.block_events.pop_front();
                }
                self.block_events
                    .push_back((*pos, *action, *parameter, *block_type));
            }
            PlayEvent::TakeItemEntity {
                item_entity,
                player,
                amount,
            } => {
                if self.item_pickups.len() == 64 {
                    self.item_pickups.pop_front();
                }
                self.item_pickups
                    .push_back((*item_entity, *player, *amount));
            }
            PlayEvent::SetEquipment { eid, entries } => {
                if let Some(entity) = self.entities.get_mut(eid) {
                    for entry in entries {
                        if entry.slot < 8 {
                            entity.equipment_slots |= 1u8 << entry.slot;
                        }
                    }
                }
            }
            PlayEvent::SetTime {
                world_age,
                time_of_day,
            } => {
                self.world_age = *world_age;
                self.time_of_day = *time_of_day;
            }
            PlayEvent::GameEvent { event, value } => {
                self.apply_game_event(*event, *value);
            }
            PlayEvent::BlockDestruction {
                breaker_eid,
                pos,
                stage,
            } => self.apply_block_destruction(*breaker_eid, *pos, *stage),
            PlayEvent::Explosion {
                x, y, z, records, ..
            } => self.apply_explosion_removals(*x, *y, *z, records)?,
            PlayEvent::BlockEntityData {
                pos,
                kind,
                nbt_bytes,
            } => self.apply_block_entity_data(*pos, *kind, nbt_bytes),
            PlayEvent::LevelEvent {
                event, pos, data, ..
            } => {
                if self.level_events.len() == 64 {
                    self.level_events.pop_front();
                }
                self.level_events.push_back((*event, *pos, *data));
            }
            PlayEvent::LevelParticles {
                particle_id,
                long_distance,
                x,
                y,
                z,
                offset_x,
                offset_y,
                offset_z,
                max_speed,
                count,
                ..
            } => {
                if self.level_particles.len() == 256 {
                    self.level_particles.pop_front();
                }
                self.level_particles.push_back(LevelParticleSpawn {
                    particle_id: *particle_id,
                    long_distance: *long_distance,
                    pos: [*x, *y, *z],
                    offset: [*offset_x, *offset_y, *offset_z],
                    max_speed: *max_speed,
                    count: *count,
                });
            }
            // `chunks_biomes` — per-column biome paletted containers
            // patched into the stored sections; columns that left the
            // view distance simply have nothing to patch.
            PlayEvent::ChunksBiomes { chunks } => self.apply_chunks_biomes(chunks),
            _ => {}
        }
        Ok(())
    }

    /// `chunks_biomes`: decode each column's stream of 64-cell biome
    /// paletted containers (one per stored section, bottom-up) and patch
    /// the matching `StoredSection::biomes` in place. A malformed or
    /// truncated payload stops at the last good section — the packet is
    /// a cosmetic color hint, never worth a desync.
    fn apply_chunks_biomes(&mut self, chunks: &[ChunkBiomeData]) {
        // v767 negotiates a 64-entry vanilla biome registry; the direct
        // palette width follows the same ceil-log2 rule the chunk codec
        // uses (`decode_chunk_packet_profiled`).
        let direct_bits = (usize::BITS - 63usize.leading_zeros()) as u8;
        for chunk in chunks {
            let Some(column) = self.columns.get_mut(&(chunk.cx, chunk.cz)) else {
                continue;
            };
            let mut reader = WireReader::new(&chunk.data);
            for section in &mut column.sections {
                if reader.remaining() == 0 {
                    break;
                }
                let Ok(biomes) =
                    decode_paletted_container(&mut reader, 64, 3, 1, direct_bits)
                else {
                    break;
                };
                section.biomes.copy_from_slice(&biomes[..64]);
            }
        }
    }

    /// Relative-move family (`move_entity_pos`, `move_entity_pos_rot`,
    /// `move_entity_rot`): accumulate the decoded block delta onto the f64
    /// position and the 1/4096 fixed-point `pos_q` mirror — the client side
    /// of vanilla's `VecDeltaCodec`. Unknown ids are ignored; only
    /// `add_entity` creates rows.
    fn move_tracked_entity(
        &mut self,
        eid: i32,
        delta: [f64; 3],
        rot: Option<(f32, f32)>,
        on_ground: bool,
    ) {
        let Some(entity) = self.entities.get_mut(&eid) else {
            return;
        };
        for ((pos, pos_q), &d) in entity
            .pos
            .iter_mut()
            .zip(entity.pos_q.iter_mut())
            .zip(delta.iter())
        {
            let d = clean_f64(d);
            *pos = clean_f64(*pos + d);
            *pos_q = pos_q.saturating_add(quantize_pos_4096(d));
        }
        if let Some((yaw, pitch)) = rot {
            entity.yaw_deg = clean_f32(yaw);
            entity.pitch_deg = clean_f32(pitch);
        }
        entity.on_ground = on_ground;
    }

    /// `teleport_entity`: absolute overwrite of position (f64 + fixed-point
    /// mirror), yaw/pitch, and on-ground. Unknown ids are ignored.
    fn teleport_tracked_entity(
        &mut self,
        eid: i32,
        pos_in: [f64; 3],
        yaw: f32,
        pitch: f32,
        on_ground: bool,
    ) {
        let Some(entity) = self.entities.get_mut(&eid) else {
            return;
        };
        let pos = [
            clean_f64(pos_in[0]),
            clean_f64(pos_in[1]),
            clean_f64(pos_in[2]),
        ];
        entity.pos_q = pos.map(quantize_pos_4096);
        entity.pos = pos;
        entity.yaw_deg = clean_f32(yaw);
        entity.pitch_deg = clean_f32(pitch);
        entity.on_ground = on_ground;
    }

    /// Weather `game_event`s: start/stop rain set the level to 1.0/0.0;
    /// rain/thunder level changes carry the level as a float clamped
    /// NaN-safe to `0.0..=1.0`. All other game events are ignored.
    fn apply_game_event(&mut self, event: u8, value: f32) {
        match event {
            GAME_EVENT_START_RAINING => self.rain_level = 1.0,
            GAME_EVENT_STOP_RAINING => self.rain_level = 0.0,
            GAME_EVENT_RAIN_LEVEL_CHANGE => self.rain_level = clean_weather_level(value),
            GAME_EVENT_THUNDER_LEVEL_CHANGE => self.thunder_level = clean_weather_level(value),
            _ => {}
        }
    }

    /// `block_destruction`: stage `0..=9` records/updates the crack overlay;
    /// `>= 10` or the wire `-1` sentinel (255 as `u8`) clears it. Bounded —
    /// new breakers past [`MAX_BLOCK_DESTRUCTIONS`] are ignored while
    /// updates to already-tracked breakers always land.
    fn apply_block_destruction(&mut self, breaker_eid: i32, pos: [i32; 3], stage: i8) {
        let stage = stage as u8;
        if stage > 9 {
            self.block_destructions.remove(&breaker_eid);
            return;
        }
        if !self.block_destructions.contains_key(&breaker_eid)
            && self.block_destructions.len() >= MAX_BLOCK_DESTRUCTIONS
        {
            return;
        }
        self.block_destructions
            .insert(breaker_eid, BlockBreakProgress { pos, stage });
    }

    /// `explode`: the packet's destruction records are byte offsets from
    /// `floor(center)` — the authoritative client-side block removals,
    /// all written as air (global palette state `0`). The record list is
    /// bounded by the decoder's
    /// [`MAX_EXPLOSION_RECORDS`](crate::protocol::MAX_EXPLOSION_RECORDS)
    /// cap; every
    /// write flows through the incremental light queue like
    /// `block_update` does.
    fn apply_explosion_removals(
        &mut self,
        x: f64,
        y: f64,
        z: f64,
        records: &[[i8; 3]],
    ) -> Result<(), String> {
        if records.is_empty() {
            return Ok(());
        }
        let base = [
            x.floor() as i32,
            y.floor() as i32,
            z.floor() as i32,
        ];
        let updates: Vec<([i32; 3], u32)> = records
            .iter()
            .map(|r| {
                (
                    [
                        base[0] + i32::from(r[0]),
                        base[1] + i32::from(r[1]),
                        base[2] + i32::from(r[2]),
                    ],
                    0,
                )
            })
            .collect();
        self.apply_block_updates(&updates)
    }

    /// `block_entity_data`: upsert the block entity at `pos` — the live
    /// update path for signs, chests, spawners, etc., complementing the
    /// chunk-payload population in [`Self::refresh_chunk_block_entities`].
    /// Bounded by [`MAX_TRACKED_BLOCK_ENTITIES`].
    fn apply_block_entity_data(&mut self, pos: [i32; 3], kind: i32, nbt_bytes: &[u8]) {
        match self.block_entities.get_mut(&pos) {
            Some(entry) => {
                entry.kind = kind;
                entry.nbt_bytes.clear();
                entry.nbt_bytes.extend_from_slice(nbt_bytes);
            }
            None => {
                if self.block_entities.len() >= MAX_TRACKED_BLOCK_ENTITIES {
                    return;
                }
                self.block_entities.insert(
                    pos,
                    TrackedBlockEntity {
                        kind,
                        nbt_bytes: nbt_bytes.to_vec(),
                    },
                );
            }
        }
    }

    /// Advance per-tick entity bookkeeping once per client tick: the
    /// hurt-flash timer counts down on every tracked entity.
    pub fn tick_entities(&mut self) {
        for entity in self.entities.values_mut() {
            entity.hurt_ticks = entity.hurt_ticks.saturating_sub(1);
        }
    }
}

fn read_nibble(bytes: &[u8; LIGHT_BYTES], idx: usize) -> u8 {
    (bytes[idx / 2] >> ((idx & 1) * 4)) & 0x0f
}

fn section_light_updates(
    light: &DecodedLightData,
    light_section: usize,
    sky_cursor: &mut usize,
    block_cursor: &mut usize,
) -> (Option<[u8; LIGHT_BYTES]>, Option<[u8; LIGHT_BYTES]>) {
    let sky_val = if bitset_get(&light.sky_mask, light_section) {
        let arr = light.sky_arrays.get(*sky_cursor).copied();
        *sky_cursor += 1;
        arr
    } else if bitset_get(&light.empty_sky_mask, light_section) {
        Some([0u8; LIGHT_BYTES])
    } else {
        None
    };

    let block_val = if bitset_get(&light.block_mask, light_section) {
        let arr = light.block_arrays.get(*block_cursor).copied();
        *block_cursor += 1;
        arr
    } else if bitset_get(&light.empty_block_mask, light_section) {
        Some([0u8; LIGHT_BYTES])
    } else {
        None
    };
    (sky_val, block_val)
}

// ---------------------------------------------------------------------------
// Vanilla 1.21.1 name-tag easter eggs (entity visuals)
// ---------------------------------------------------------------------------

/// Visual overrides a 1.21.1 client derives from an entity's exact custom
/// name: `LivingEntityRenderer.isEntityUpsideDown` flips "Dinnerbone"/"Grumm"
/// models, `Sheep` fades a "jeb_" sheep's wool through the dye palette, and a
/// "Toast" rabbit swaps to the memorial black-and-white texture.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct EntityEasterEggVisual {
    /// Render the entity upside down ("Dinnerbone" or "Grumm").
    pub upside_down: bool,
    /// Per-frame wool tint for a "jeb_" sheep, `None` for normal dye color.
    pub rainbow_sheep_rgb: Option<[f32; 3]>,
    /// Use the Toast rabbit texture ("Toast" on a rabbit only).
    pub toast_rabbit: bool,
}

/// `DyeColor` texture-diffuse colors in declaration order — the exact wool
/// palette `Sheep.getFleeceColor` cycles for a jeb_ sheep (0xRRGGBB).
const SHEEP_WOOL_RGB: [[u8; 3]; 16] = [
    [0xf9, 0xff, 0xfe], // white
    [0xf9, 0x80, 0x1d], // orange
    [0xc7, 0x4e, 0xbd], // magenta
    [0x3a, 0xb3, 0xda], // light_blue
    [0xfe, 0xd8, 0x3d], // yellow
    [0x80, 0xc7, 0x1f], // lime
    [0xf3, 0x8b, 0xaa], // pink
    [0x47, 0x4f, 0x52], // gray
    [0x9d, 0x9d, 0x97], // light_gray
    [0x16, 0x9c, 0x9c], // cyan
    [0x89, 0x32, 0xb8], // purple
    [0x3c, 0x44, 0xaa], // blue
    [0x83, 0x54, 0x32], // brown
    [0x5e, 0x7c, 0x16], // green
    [0xb0, 0x2e, 0x26], // red
    [0x1d, 0x1d, 0x21], // black
];

/// One 25-tick color per palette slot, lerping into the next — the vanilla
/// `getFleeceColor` fade `(tickCount % 25 + partialTick) / 25`.
fn jeb_wool_rgb(world_tick: u64) -> [f32; 3] {
    let phase = ((world_tick / 25) % 16) as usize;
    let next = (phase + 1) % SHEEP_WOOL_RGB.len();
    let fade = (world_tick % 25) as f32 / 25.0;
    let from = SHEEP_WOOL_RGB[phase].map(f32::from);
    let to = SHEEP_WOOL_RGB[next].map(f32::from);
    [
        (from[0] * (1.0 - fade) + to[0] * fade) / 255.0,
        (from[1] * (1.0 - fade) + to[1] * fade) / 255.0,
        (from[2] * (1.0 - fade) + to[2] * fade) / 255.0,
    ]
}

/// Resolve the vanilla name-tag easter eggs for one entity. `entity_type`
/// accepts `minecraft:name` or the bare name, and `custom_name` is the exact
/// name-tag string: every vanilla comparison is case-sensitive, so
/// "dinnerbone"/"JEB_"/`None` produce no visual.
pub fn evaluate_entity_easter_egg(
    entity_type: &str,
    custom_name: Option<&str>,
    world_tick: u64,
) -> EntityEasterEggVisual {
    let mut visual = EntityEasterEggVisual::default();
    let Some(name) = custom_name else {
        return visual;
    };
    // isEntityUpsideDown ignores the entity type entirely.
    if name == "Dinnerbone" || name == "Grumm" {
        visual.upside_down = true;
    }
    match (
        entity_type
            .strip_prefix("minecraft:")
            .unwrap_or(entity_type),
        name,
    ) {
        ("sheep", "jeb_") => visual.rainbow_sheep_rgb = Some(jeb_wool_rgb(world_tick)),
        ("rabbit", "Toast") => visual.toast_rabbit = true,
        _ => {}
    }
    visual
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dinnerbone_and_grumm_flip_any_entity_type() {
        for name in ["Dinnerbone", "Grumm"] {
            for ty in [
                "minecraft:sheep",
                "sheep",
                "minecraft:rabbit",
                "minecraft:vindicator",
                "zombie",
                "minecraft:player",
            ] {
                let visual = evaluate_entity_easter_egg(ty, Some(name), 0);
                assert!(visual.upside_down, "{ty} named {name}");
                assert!(visual.rainbow_sheep_rgb.is_none());
                assert!(!visual.toast_rabbit);
            }
        }
    }

    #[test]
    fn jeb_sheep_fades_smoothly_through_the_wool_palette() {
        let first = evaluate_entity_easter_egg("minecraft:sheep", Some("jeb_"), 0)
            .rainbow_sheep_rgb
            .expect("jeb_ sheep tints its wool");
        let white = [
            f32::from(0xf9u8) / 255.0,
            f32::from(0xffu8) / 255.0,
            f32::from(0xfeu8) / 255.0,
        ];
        for (got, want) in first.iter().zip(white) {
            assert!((got - want).abs() < 1e-6);
        }
        // At each 25-tick boundary the fade lands exactly on a dye color.
        let orange = [
            f32::from(0xf9u8) / 255.0,
            f32::from(0x80u8) / 255.0,
            f32::from(0x1du8) / 255.0,
        ];
        let tick25 = evaluate_entity_easter_egg("sheep", Some("jeb_"), 25)
            .rainbow_sheep_rgb
            .unwrap();
        for (got, want) in tick25.iter().zip(orange) {
            assert!((got - want).abs() < 1e-6);
        }
        // Two full 16-color cycles: finite RGB inside [0, 1] at every tick.
        let mut last = first;
        let mut changed = 0u32;
        for tick in 0..800 {
            let rgb = evaluate_entity_easter_egg("minecraft:sheep", Some("jeb_"), tick)
                .rainbow_sheep_rgb
                .unwrap();
            assert!(rgb.iter().all(|v| v.is_finite() && (0.0..=1.0).contains(v)));
            if rgb != last {
                changed += 1;
                last = rgb;
            }
        }
        assert!(changed > 0, "the jeb_ tint cycles over time");
        // Mid-cycle ticks interpolate instead of hard-switching colors.
        let mid = evaluate_entity_easter_egg("sheep", Some("jeb_"), 12)
            .rainbow_sheep_rgb
            .unwrap();
        assert_ne!(mid, first);
        assert_ne!(mid, tick25);
    }

    #[test]
    fn toast_texture_is_rabbit_only() {
        for ty in ["minecraft:rabbit", "rabbit"] {
            let visual = evaluate_entity_easter_egg(ty, Some("Toast"), 0);
            assert!(visual.toast_rabbit);
            assert!(!visual.upside_down);
            assert!(visual.rainbow_sheep_rgb.is_none());
        }
        assert!(!evaluate_entity_easter_egg("minecraft:sheep", Some("Toast"), 0).toast_rabbit);
        assert!(!evaluate_entity_easter_egg("minecraft:chicken", Some("Toast"), 0).toast_rabbit);
    }

    #[test]
    fn case_sensitive_names_and_mismatched_types_give_no_visual() {
        for (ty, name) in [
            ("minecraft:sheep", "JEB_"),
            ("minecraft:sheep", "jeb"),
            ("minecraft:sheep", "dinnerbone"),
            ("minecraft:sheep", " Dinnerbone"),
            ("minecraft:sheep", "Dinnerbone "),
            ("minecraft:rabbit", "toast"),
            ("minecraft:zombie", "jeb_"),
            ("minecraft:rabbit", "jeb_"),
            ("other:sheep", "jeb_"),
            ("minecraft:sheep", ""),
        ] {
            let visual = evaluate_entity_easter_egg(ty, Some(name), 0);
            assert!(!visual.upside_down, "{ty} named {name}");
            assert!(visual.rainbow_sheep_rgb.is_none());
            assert!(!visual.toast_rabbit);
        }
        let visual = evaluate_entity_easter_egg("minecraft:sheep", None, 0);
        assert!(!visual.upside_down);
        assert!(visual.rainbow_sheep_rgb.is_none());
        assert!(!visual.toast_rabbit);
    }

    /// `explode` destruction records are byte offsets from
    /// `floor(center)`: each resolves to an absolute position and is
    /// written as air through the same incremental path `block_update`
    /// uses — a loaded chunk really loses the blocks.
    #[test]
    fn explosion_writes_air_into_loaded_chunks() {
        use crate::protocol::DecodedSection;
        let mut world = WorldState::new(
            DimensionMeta {
                name: "minecraft:overworld".to_string(),
                min_y: 0,
                height: 64,
                has_skylight: true,
            },
            BlockTable::builtin(),
        );
        world
            .apply_chunk(DecodedChunk {
                cx: 0,
                cz: 0,
                sections: (0..4)
                    .map(|_| DecodedSection {
                        non_air: SECTION_CELLS as i16,
                        states: Box::new([1; SECTION_CELLS]),
                        biomes: [0; 64],
                    })
                    .collect(),
                block_entities: Vec::new(),
                light: DecodedLightData {
                    sky_mask: Vec::new(),
                    block_mask: Vec::new(),
                    empty_sky_mask: Vec::new(),
                    empty_block_mask: Vec::new(),
                    sky_arrays: Vec::new(),
                    block_arrays: Vec::new(),
                },
            })
            .unwrap();
        assert_eq!(world.get_block_state(3, 4, 5), Some(1));

        world
            .apply_play_event(&PlayEvent::Explosion {
                x: 4.6,
                y: 4.2,
                z: 4.8,
                power: 4.0,
                // `floor(center)` = (4, 4, 4): records land on
                // (4, 4, 4) and (3, 4, 5).
                records: vec![[0, 0, 0], [-1, 0, 1]],
                knockback: [0.0; 3],
                block_interaction: 1,
                small_particle_id: 0,
                large_particle_id: 0,
                sound_name: None,
                sound_id: None,
            })
            .unwrap();
        assert_eq!(world.get_block_state(4, 4, 4), Some(0));
        assert_eq!(world.get_block_state(3, 4, 5), Some(0));
        assert_eq!(world.get_block_state(1, 1, 1), Some(1));
        assert_eq!(world.block_updates, 2);
    }

    #[test]
    fn twenty_four_section_column_releases_dense_mesh_snapshots_after_meshing() {
        use crate::protocol::DecodedSection;

        let blocks = BlockTable::builtin();
        assert_ne!(blocks.mesh_block(1), blocks.mesh_block(2));
        let mut world = WorldState::new(
            DimensionMeta {
                name: "minecraft:overworld".to_string(),
                min_y: -64,
                height: 384,
                has_skylight: true,
            },
            blocks,
        );
        let sections = (0..24)
            .map(|_| {
                let mut states = Box::new([1; SECTION_CELLS]);
                states[0] = 2;
                DecodedSection {
                    non_air: SECTION_CELLS as i16,
                    states,
                    biomes: [0; 64],
                }
            })
            .collect();
        world
            .apply_chunk(DecodedChunk {
                cx: 0,
                cz: 0,
                sections,
                light: DecodedLightData {
                    sky_mask: Vec::new(),
                    block_mask: Vec::new(),
                    empty_sky_mask: Vec::new(),
                    empty_block_mask: Vec::new(),
                    sky_arrays: Vec::new(),
                    block_arrays: Vec::new(),
                },
                block_entities: Vec::new(),
            })
            .unwrap();

        let before = world.mesh_cache_dense_bytes();
        assert_eq!(before, 24 * 65_536);
        assert_eq!(world.mesh_cache.rebuild_dirty(usize::MAX).len(), 24);
        let after = world.mesh_cache.mesh_cache_blocks_heap_bytes();
        assert_eq!(world.mesh_cache_dense_bytes(), 0);
        assert_eq!(after, 24 * (4096 * 2 + 2 * std::mem::size_of::<Block>()));
        assert_eq!(world.get_block_state(0, -64, 0), Some(2));
        assert_eq!(world.get_block_state(1, -64, 0), Some(1));
    }
}
