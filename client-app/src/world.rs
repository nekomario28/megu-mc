// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
//! Live Protocol 767 world cache, block-state classification, `client-port`
//! greedy meshing + vertex packing + translucent sorting, and collision lookup.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::time::Instant;

use mc_spine_client_mesh::ffi::{
    mc_spine_client_mesh_pack_vertices, mc_spine_client_mesh_sort_translucent, PackedVertex, ABI_OK,
};
use mc_spine_client_mesh::{
    section_index, Block, DirectionBuckets, FaceVisibility, Light, MeshCache, MissingNeighbor,
    Quad, RenderRegions, Section, SectionCoord, SectionGraph, ALL_FACES, CUTOUT, FLUID_TINTED,
    LAVA, LIGHT_BYTES, NO_FLUID, OPAQUE, SAME_STATE_CULL, SECTION_CELLS, TINTED, TRANSLUCENT,
    WATER,
};
use mc_spine_kernel::{
    Aabb, FluidCell, FluidKind, LightEdit, LightEngine, LightFact, LightUpdateMultiWord,
    PalettedSection, Registry, STATE_COUNT,
};

use crate::protocol::{bitset_get, DecodedChunk, DecodedLightData, DimensionMeta};

// ---------------------------------------------------------------------------
// Safe Rust wrappers over client-port vertex packing and translucent sorting
// ---------------------------------------------------------------------------

pub fn pack_vertices(quads: &[Quad]) -> Result<Vec<PackedVertex>, i32> {
    if quads.is_empty() {
        return Ok(Vec::new());
    }
    let mut required = 0usize;
    let query = unsafe {
        mc_spine_client_mesh_pack_vertices(
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
        mc_spine_client_mesh_pack_vertices(
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

pub fn sort_translucent(quads: &[Quad], camera: [f32; 3]) -> Result<Vec<u32>, i32> {
    if quads.is_empty() {
        return Ok(Vec::new());
    }
    let mut required = 0usize;
    let query = unsafe {
        mc_spine_client_mesh_sort_translucent(
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
        mc_spine_client_mesh_sort_translucent(
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
        let mut base_ids: Vec<u32> = vec![0; count];
        for (name, base, len) in &parsed.blocks {
            let start = *base as usize;
            let end = start.saturating_add(*len as usize).min(count);
            for idx in start..end {
                names[idx] = name.as_str();
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
// Minimal zero-dependency JSON parser for assets/block_states_1211.json
// ---------------------------------------------------------------------------

struct ParsedBlockStates {
    blocks: Vec<(String, u32, u32)>,
    flags: Vec<u8>,
    full_sides: Vec<u8>,
    shapes: Vec<LocalBox>,
    collision_shapes: Vec<Vec<u16>>,
}

struct JsonScanner<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> JsonScanner<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    fn skip_ws(&mut self) {
        while let Some(&b) = self.data.get(self.pos) {
            if matches!(b, b' ' | b'\n' | b'\r' | b'\t') {
                self.pos += 1;
            } else {
                break;
            }
        }
    }

    fn peek(&mut self) -> Option<u8> {
        self.skip_ws();
        self.data.get(self.pos).copied()
    }

    fn expect(&mut self, expected: u8) -> Result<(), String> {
        self.skip_ws();
        match self.data.get(self.pos) {
            Some(&b) if b == expected => {
                self.pos += 1;
                Ok(())
            }
            other => Err(format!(
                "JSON syntax error at {}: expected '{}', got {:?}",
                self.pos, expected as char, other
            )),
        }
    }

    fn parse_string(&mut self) -> Result<String, String> {
        self.expect(b'"')?;
        let mut out = String::new();
        while let Some(&b) = self.data.get(self.pos) {
            self.pos += 1;
            match b {
                b'"' => return Ok(out),
                b'\\' => {
                    let esc = *self
                        .data
                        .get(self.pos)
                        .ok_or_else(|| "unterminated JSON string escape".to_string())?;
                    self.pos += 1;
                    match esc {
                        b'"' => out.push('"'),
                        b'\\' => out.push('\\'),
                        b'/' => out.push('/'),
                        b'b' => out.push('\x08'),
                        b'f' => out.push('\x0c'),
                        b'n' => out.push('\n'),
                        b'r' => out.push('\r'),
                        b't' => out.push('\t'),
                        b'u' => {
                            let hex = self
                                .data
                                .get(self.pos..self.pos + 4)
                                .ok_or_else(|| "truncated unicode escape".to_string())?;
                            self.pos += 4;
                            let code = u32::from_str_radix(
                                std::str::from_utf8(hex).map_err(|_| "bad unicode escape")?,
                                16,
                            )
                            .map_err(|_| "bad unicode hex")?;
                            if let Some(ch) = char::from_u32(code) {
                                out.push(ch);
                            }
                        }
                        _ => out.push(esc as char),
                    }
                }
                _ => out.push(b as char),
            }
        }
        Err("unterminated JSON string".to_string())
    }

    fn parse_u64(&mut self) -> Result<u64, String> {
        self.skip_ws();
        let start = self.pos;
        while let Some(&b) = self.data.get(self.pos) {
            if b.is_ascii_digit() {
                self.pos += 1;
            } else {
                break;
            }
        }
        if self.pos == start {
            return Err(format!("expected integer at {}", self.pos));
        }
        std::str::from_utf8(&self.data[start..self.pos])
            .map_err(|_| "invalid int UTF-8".to_string())?
            .parse::<u64>()
            .map_err(|_| "integer parse overflow".to_string())
    }

    fn parse_f64(&mut self) -> Result<f64, String> {
        self.skip_ws();
        let start = self.pos;
        while let Some(&b) = self.data.get(self.pos) {
            if b.is_ascii_digit() || matches!(b, b'-' | b'+' | b'.' | b'e' | b'E') {
                self.pos += 1;
            } else {
                break;
            }
        }
        if self.pos == start {
            return Err(format!("expected float at {}", self.pos));
        }
        std::str::from_utf8(&self.data[start..self.pos])
            .map_err(|_| "invalid float UTF-8".to_string())?
            .parse::<f64>()
            .map_err(|_| "float parse error".to_string())
    }

    fn skip_value(&mut self) -> Result<(), String> {
        match self.peek() {
            Some(b'"') => {
                let _ = self.parse_string()?;
                Ok(())
            }
            Some(b'{') => {
                self.pos += 1;
                if self.peek() == Some(b'}') {
                    self.pos += 1;
                    return Ok(());
                }
                loop {
                    let _ = self.parse_string()?;
                    self.expect(b':')?;
                    self.skip_value()?;
                    match self.peek() {
                        Some(b',') => self.pos += 1,
                        Some(b'}') => {
                            self.pos += 1;
                            return Ok(());
                        }
                        _ => return Err(format!("malformed object at {}", self.pos)),
                    }
                }
            }
            Some(b'[') => {
                self.pos += 1;
                if self.peek() == Some(b']') {
                    self.pos += 1;
                    return Ok(());
                }
                loop {
                    self.skip_value()?;
                    match self.peek() {
                        Some(b',') => self.pos += 1,
                        Some(b']') => {
                            self.pos += 1;
                            return Ok(());
                        }
                        _ => return Err(format!("malformed array at {}", self.pos)),
                    }
                }
            }
            Some(b't') => self.consume_literal(b"true"),
            Some(b'f') => self.consume_literal(b"false"),
            Some(b'n') => self.consume_literal(b"null"),
            Some(b'-' | b'0'..=b'9') => {
                let _ = self.parse_f64()?;
                Ok(())
            }
            other => Err(format!("unexpected JSON token {:?} at {}", other, self.pos)),
        }
    }

    fn consume_literal(&mut self, lit: &[u8]) -> Result<(), String> {
        self.skip_ws();
        if self.data.get(self.pos..self.pos + lit.len()) == Some(lit) {
            self.pos += lit.len();
            Ok(())
        } else {
            Err(format!("expected literal at {}", self.pos))
        }
    }

    fn parse_u8_array(&mut self) -> Result<Vec<u8>, String> {
        self.expect(b'[')?;
        let mut out = Vec::new();
        if self.peek() == Some(b']') {
            self.pos += 1;
            return Ok(out);
        }
        loop {
            out.push(self.parse_u64()? as u8);
            match self.peek() {
                Some(b',') => self.pos += 1,
                Some(b']') => {
                    self.pos += 1;
                    return Ok(out);
                }
                _ => return Err(format!("expected ',' or ']' at {}", self.pos)),
            }
        }
    }

    fn parse_f64_triple(&mut self) -> Result<[f64; 3], String> {
        self.expect(b'[')?;
        let a = self.parse_f64()?;
        self.expect(b',')?;
        let b = self.parse_f64()?;
        self.expect(b',')?;
        let c = self.parse_f64()?;
        self.expect(b']')?;
        Ok([a, b, c])
    }

    fn parse_blocks(&mut self) -> Result<Vec<(String, u32, u32)>, String> {
        self.expect(b'[')?;
        let mut out = Vec::new();
        if self.peek() == Some(b']') {
            self.pos += 1;
            return Ok(out);
        }
        loop {
            self.expect(b'[')?;
            let name = self.parse_string()?;
            self.expect(b',')?;
            let base = self.parse_u64()? as u32;
            self.expect(b',')?;
            let len = self.parse_u64()? as u32;
            self.expect(b']')?;
            out.push((name, base, len));
            match self.peek() {
                Some(b',') => self.pos += 1,
                Some(b']') => {
                    self.pos += 1;
                    return Ok(out);
                }
                _ => return Err(format!("expected ',' or ']' in blocks at {}", self.pos)),
            }
        }
    }

    fn parse_shapes(&mut self) -> Result<Vec<LocalBox>, String> {
        self.expect(b'[')?;
        let mut out = Vec::new();
        if self.peek() == Some(b']') {
            self.pos += 1;
            return Ok(out);
        }
        loop {
            self.expect(b'{')?;
            let mut min = [0.0; 3];
            let mut max = [0.0; 3];
            if self.peek() != Some(b'}') {
                loop {
                    let key = self.parse_string()?;
                    self.expect(b':')?;
                    match key.as_str() {
                        "min" => min = self.parse_f64_triple()?,
                        "max" => max = self.parse_f64_triple()?,
                        _ => self.skip_value()?,
                    }
                    match self.peek() {
                        Some(b',') => self.pos += 1,
                        Some(b'}') => break,
                        _ => return Err(format!("malformed shape object at {}", self.pos)),
                    }
                }
            }
            self.expect(b'}')?;
            out.push(LocalBox { min, max });
            match self.peek() {
                Some(b',') => self.pos += 1,
                Some(b']') => {
                    self.pos += 1;
                    return Ok(out);
                }
                _ => return Err(format!("expected ',' or ']' in shapes at {}", self.pos)),
            }
        }
    }

    fn parse_collision_shapes(&mut self) -> Result<Vec<Vec<u16>>, String> {
        self.expect(b'[')?;
        let mut out = Vec::new();
        if self.peek() == Some(b']') {
            self.pos += 1;
            return Ok(out);
        }
        loop {
            self.expect(b'[')?;
            let mut list = Vec::new();
            if self.peek() != Some(b']') {
                loop {
                    list.push(self.parse_u64()? as u16);
                    match self.peek() {
                        Some(b',') => self.pos += 1,
                        Some(b']') => break,
                        _ => return Err(format!("malformed collision list at {}", self.pos)),
                    }
                }
            }
            self.expect(b']')?;
            out.push(list);
            match self.peek() {
                Some(b',') => self.pos += 1,
                Some(b']') => {
                    self.pos += 1;
                    return Ok(out);
                }
                _ => {
                    return Err(format!(
                        "expected ',' or ']' in collision_shapes at {}",
                        self.pos
                    ))
                }
            }
        }
    }
}

fn parse_block_states_json(raw: &[u8]) -> Result<ParsedBlockStates, String> {
    let mut scanner = JsonScanner::new(raw);
    scanner.expect(b'{')?;
    let mut parsed = ParsedBlockStates {
        blocks: Vec::new(),
        flags: Vec::new(),
        full_sides: Vec::new(),
        shapes: Vec::new(),
        collision_shapes: Vec::new(),
    };
    if scanner.peek() == Some(b'}') {
        return Ok(parsed);
    }
    loop {
        let key = scanner.parse_string()?;
        scanner.expect(b':')?;
        match key.as_str() {
            "blocks" => parsed.blocks = scanner.parse_blocks()?,
            "flags" => parsed.flags = scanner.parse_u8_array()?,
            "full_sides" => parsed.full_sides = scanner.parse_u8_array()?,
            "shapes" => parsed.shapes = scanner.parse_shapes()?,
            "collision_shapes" => parsed.collision_shapes = scanner.parse_collision_shapes()?,
            _ => scanner.skip_value()?,
        }
        match scanner.peek() {
            Some(b',') => scanner.pos += 1,
            Some(b'}') => return Ok(parsed),
            _ => return Err(format!("malformed top-level JSON at {}", scanner.pos)),
        }
    }
}

// ---------------------------------------------------------------------------
// Chunk Column + Live MeshCache World State
// ---------------------------------------------------------------------------

pub struct StoredSection {
    pub palette: PalettedSection,
    pub block_light: [u8; LIGHT_BYTES],
    pub sky_light: [u8; LIGHT_BYTES],
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
    pub columns: HashMap<(i32, i32), ChunkColumn>,
    pub mesh_cache: MeshCache,
    packed_meshes: HashMap<SectionCoord, Vec<PackedVertex>>,
    translucent_orders: HashMap<SectionCoord, Vec<u32>>,
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
            columns: HashMap::new(),
            mesh_cache: MeshCache::new(MissingNeighbor::Air),
            packed_meshes: HashMap::new(),
            translucent_orders: HashMap::new(),
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
        }
    }

    pub fn reset_dimension(&mut self, dimension: DimensionMeta) {
        self.dimension = dimension;
        self.light_engine = light_engine_for(&self.dimension, self.registry.is_some());
        self.columns.clear();
        self.mesh_cache = MeshCache::new(MissingNeighbor::Air);
        self.packed_meshes.clear();
        self.translucent_orders.clear();
        self.quad_counts.clear();
        self.visibility.clear();
        self.direction_buckets.clear();
        self.render_regions = RenderRegions::default();
        self.recompute_live_totals();
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
        let section_count = decoded.sections.len();
        let default_sky = if self.dimension.has_skylight {
            [0xffu8; LIGHT_BYTES]
        } else {
            [0u8; LIGHT_BYTES]
        };

        let (sky_per_section, block_per_section) =
            unpack_section_lights(&decoded.light, section_count, default_sky);

        let min_sy = self.dimension.min_section_y();
        let mut stored_sections = Vec::with_capacity(section_count);
        for (idx, sec) in decoded.sections.iter().enumerate() {
            let sky = sky_per_section[idx].unwrap_or(default_sky);
            let blk = block_per_section[idx].unwrap_or([0u8; LIGHT_BYTES]);
            let stored =
                StoredSection::from_states(&sec.states, blk, sky, &self.blocks, self.state_count)?;
            let coord = SectionCoord(decoded.cx, min_sy + idx as i32, decoded.cz);
            if stored.has_non_air {
                let mesh_sec = stored.to_mesh_section(&self.blocks);
                self.set_section_visibility(coord, FaceVisibility::compute(&mesh_sec.blocks), true);
                self.mesh_cache.insert(coord, mesh_sec);
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

    /// Apply one local block edit to the incremental light engine and push the
    /// resulting 2048-byte sky/block arrays into the stored sections + mesh
    /// cache. No-op when the engine or registry is unavailable or the edited
    /// chunk was never loaded into the engine.
    fn apply_local_light_edit(&mut self, x: i32, y: i32, z: i32, state_id: u32) {
        let fact = match self
            .registry
            .as_ref()
            .and_then(|registry| registry.facts(state_id).ok())
        {
            Some(fact) => fact,
            None => return,
        };
        let edit = LightEdit {
            x,
            y,
            z,
            fact: LightFact {
                opacity: fact.opacity,
                emission: fact.luminance,
                occlusion: fact.face_occlusion,
            },
        };
        let update = {
            let Some(engine) = self.light_engine.as_mut() else {
                return;
            };
            match engine.apply_edits_multi_word(&[edit]) {
                Ok(update) => update,
                Err(_) => return,
            }
        };
        self.local_light_edits += 1;
        self.apply_local_light_update(update);
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
                        Some(sec) => sec.to_mesh_section(&self.blocks),
                        None => continue,
                    },
                    None => continue,
                };
                self.mesh_cache.insert(coord, mesh_sec);
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
        let section_count = col.sections.len();
        let default_sky = if self.dimension.has_skylight {
            [0xffu8; LIGHT_BYTES]
        } else {
            [0u8; LIGHT_BYTES]
        };
        let (sky_updates, block_updates) = unpack_section_lights(light, section_count, default_sky);
        let min_sy = self.dimension.min_section_y();

        for (idx, sec) in col.sections.iter_mut().enumerate() {
            let mut changed = false;
            if let Some(new_sky) = sky_updates[idx] {
                if sec.sky_light != new_sky {
                    sec.sky_light = new_sky;
                    changed = true;
                }
            }
            if let Some(new_blk) = block_updates[idx] {
                if sec.block_light != new_blk {
                    sec.block_light = new_blk;
                    changed = true;
                }
            }
            if changed && sec.has_non_air {
                let coord = SectionCoord(cx, min_sy + idx as i32, cz);
                let mesh_sec = sec.to_mesh_section(&self.blocks);
                self.mesh_cache.insert(coord, mesh_sec);
            }
        }
        Ok(())
    }

    pub fn apply_block_update(&mut self, pos: [i32; 3], state_id: u32) -> Result<(), String> {
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
        sec.palette
            .set(cell_idx, clamped)
            .map_err(|e| format!("PalettedSection::set failed: {e:?}"))?;

        let coord = SectionCoord(cx, self.dimension.min_section_y() + sec_idx as i32, cz);
        let mesh_block = self.blocks.mesh_block(clamped);
        let light = Light::new(
            read_nibble(&sec.block_light, cell_idx),
            read_nibble(&sec.sky_light, cell_idx),
        );

        if !self.blocks.is_air(clamped) {
            if !sec.has_non_air {
                sec.has_non_air = true;
                let mesh_sec = sec.to_mesh_section(&self.blocks);
                self.set_section_visibility(coord, FaceVisibility::compute(&mesh_sec.blocks), true);
                self.mesh_cache.insert(coord, mesh_sec);
            } else if self
                .mesh_cache
                .set_cell(coord, [lx, ly, lz], mesh_block, light)
            {
                let mesh_sec = sec.to_mesh_section(&self.blocks);
                self.set_section_visibility(coord, FaceVisibility::compute(&mesh_sec.blocks), true);
            }
        } else if sec.has_non_air
            && self
                .mesh_cache
                .set_cell(coord, [lx, ly, lz], mesh_block, light)
        {
            let mesh_sec = sec.to_mesh_section(&self.blocks);
            self.set_section_visibility(coord, FaceVisibility::compute(&mesh_sec.blocks), true);
        }
        self.apply_local_light_edit(x, y, z, clamped);
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
    }

    fn remove_section_mesh(&mut self, coord: SectionCoord) {
        self.mesh_cache.remove(coord);
        self.packed_meshes.remove(&coord);
        self.translucent_orders.remove(&coord);
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
    /// translucent sort order. Idempotent.
    fn derive_section_artifacts(
        &mut self,
        coord: SectionCoord,
        camera: [f32; 3],
    ) -> Result<(), String> {
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
}

fn read_nibble(bytes: &[u8; LIGHT_BYTES], idx: usize) -> u8 {
    (bytes[idx / 2] >> ((idx & 1) * 4)) & 0x0f
}

type SectionLightPlanes = (
    Vec<Option<[u8; LIGHT_BYTES]>>,
    Vec<Option<[u8; LIGHT_BYTES]>>,
);

fn unpack_section_lights(
    light: &DecodedLightData,
    section_count: usize,
    _default_sky: [u8; LIGHT_BYTES],
) -> SectionLightPlanes {
    let mut sky_out = vec![None; section_count];
    let mut block_out = vec![None; section_count];

    let light_sections = section_count + 2;
    let mut sky_cursor = 0usize;
    let mut block_cursor = 0usize;

    for ls in 0..light_sections {
        let sky_val = if bitset_get(&light.sky_mask, ls) {
            let arr = light.sky_arrays.get(sky_cursor).copied();
            sky_cursor += 1;
            arr
        } else if bitset_get(&light.empty_sky_mask, ls) {
            Some([0u8; LIGHT_BYTES])
        } else {
            None
        };

        let block_val = if bitset_get(&light.block_mask, ls) {
            let arr = light.block_arrays.get(block_cursor).copied();
            block_cursor += 1;
            arr
        } else if bitset_get(&light.empty_block_mask, ls) {
            Some([0u8; LIGHT_BYTES])
        } else {
            None
        };

        // Light section 0 is below min_y and section_count + 1 is above max_y.
        if ls >= 1 && ls <= section_count {
            sky_out[ls - 1] = sky_val;
            block_out[ls - 1] = block_val;
        }
    }
    (sky_out, block_out)
}
