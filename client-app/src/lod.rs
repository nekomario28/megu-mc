// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
//! Distance-tiered far-terrain LOD column store: summarized `LodCell` grids
//! keyed by chunk column, drawing that never overlaps full-detail meshes,
//! LRU/farthest-first eviction under a strict byte budget, and the opt-in
//! `mc-spine:lod_v1` wire codec.

use std::collections::{HashMap, HashSet};

use mc_spine_client_mesh::{summarize_column, LodCell, SectionView, LOD_COARSE, LOD_FINE};

/// Distance tier for one chunk column, measured in Chebyshev chunk units
/// from the camera column.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LodTier {
    /// Inside `full_detail_radius`: rendered by the regular section meshes.
    FullDetail,
    /// Inside `fine_lod_radius`: drawn from the 16x16 per-block summary.
    Fine,
    /// Inside `coarse_lod_radius`: drawn from the 4x4 footprint summary.
    Coarse,
    /// Beyond `coarse_lod_radius`: not drawn at all.
    Culled,
}

/// One column selected for LOD drawing by [`LodManager::active_lod_columns`].
/// `tier` is always `LodTier::Fine` or `LodTier::Coarse`; columns inside the
/// full-detail radius that are not yet meshed are promoted to `Fine` so no
/// hole appears while their meshes stream in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LodDrawColumn {
    pub cx: i32,
    pub cz: i32,
    pub tier: LodTier,
}

/// Radii are Chebyshev distances in chunk columns and must satisfy
/// `full_detail_radius <= fine_lod_radius <= coarse_lod_radius`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LodConfig {
    /// Columns at or inside this distance render full-detail meshes only.
    pub full_detail_radius: i32,
    /// Columns beyond full detail up to this distance draw fine LOD cells.
    pub fine_lod_radius: i32,
    /// Columns beyond fine up to this distance draw coarse LOD cells.
    pub coarse_lod_radius: i32,
    /// Strict cap on stored summary bytes; inserts evict farthest-then-oldest
    /// columns until the total fits. A single column larger than the cap is
    /// not stored at all.
    pub max_memory_bytes: usize,
}

impl Default for LodConfig {
    fn default() -> Self {
        Self {
            full_detail_radius: 8,
            fine_lod_radius: 32,
            coarse_lod_radius: 128,
            max_memory_bytes: 64 * 1024 * 1024,
        }
    }
}

/// Snapshot of [`LodManager`] state. `fine_drawn`, `coarse_drawn` and
/// `skipped_full_detail` describe the most recent
/// [`LodManager::active_lod_columns`] call; the rest are cumulative/current.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LodStats {
    pub stored_columns: usize,
    pub memory_bytes: usize,
    pub evictions: u64,
    pub fine_drawn: usize,
    pub coarse_drawn: usize,
    pub skipped_full_detail: usize,
}

/// A decoded `mc-spine:lod_v1` frame: one column summary at `resolution`
/// (`LOD_COARSE` or `LOD_FINE`) with `resolution * resolution` cells in
/// x-fastest, z-major order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LodFrame {
    pub cx: i32,
    pub cz: i32,
    pub resolution: usize,
    pub cells: Vec<LodCell>,
}

/// Why a `lod_v1` frame failed to decode or encode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LodError {
    BadResolution,
    BadCellCount,
    Truncated,
    BadMagic,
    BadVersion,
    TrailingBytes,
}

const LOD_V1_MAGIC: &[u8; 4] = b"LOD1";
const LOD_V1_VERSION: u8 = 1;
const LOD_V1_HEADER: usize = 16;
const LOD_V1_CELL_BYTES: usize = 11;

struct LodEntry {
    /// Fine-resolution cells (`LOD_FINE * LOD_FINE`); coarse cells are
    /// derived on demand by [`LodManager::coarse_cells`].
    cells: Vec<LodCell>,
    last_tick: u64,
}

fn entry_bytes(entry: &LodEntry) -> usize {
    std::mem::size_of::<LodEntry>() + entry.cells.capacity() * std::mem::size_of::<LodCell>()
}

fn chebyshev(camera: (i32, i32), cx: i32, cz: i32) -> i64 {
    (i64::from(cx) - i64::from(camera.0))
        .abs()
        .max((i64::from(cz) - i64::from(camera.1)).abs())
}

/// Reduce a fine (16x16) summary to the coarse (4x4) footprint summary,
/// keeping the highest `top_y` cell per footprint — identical to running
/// `summarize_column` at `LOD_COARSE` because fine cells already hold the
/// per-block-column tops.
pub fn coarse_cells(fine: &[LodCell]) -> [LodCell; LOD_COARSE * LOD_COARSE] {
    debug_assert_eq!(fine.len(), LOD_FINE * LOD_FINE);
    let foot = LOD_FINE / LOD_COARSE;
    let mut out = [LodCell::EMPTY; LOD_COARSE * LOD_COARSE];
    for cz in 0..LOD_COARSE {
        for cx in 0..LOD_COARSE {
            let mut best = LodCell::EMPTY;
            for bz in 0..foot {
                for bx in 0..foot {
                    let cell = fine[(cz * foot + bz) * LOD_FINE + cx * foot + bx];
                    if cell.top_y > best.top_y {
                        best = cell;
                    }
                }
            }
            out[cz * LOD_COARSE + cx] = best;
        }
    }
    out
}

/// Store of summarized LOD columns with distance-tiered drawing and a strict
/// memory budget. Single-threaded; no interior locking.
pub struct LodManager {
    config: LodConfig,
    entries: HashMap<(i32, i32), LodEntry>,
    memory_bytes: usize,
    evictions: u64,
    tick: u64,
    camera: (i32, i32),
    fine_drawn: usize,
    coarse_drawn: usize,
    skipped_full_detail: usize,
}

impl LodManager {
    pub fn new(config: LodConfig) -> Self {
        assert!(
            0 <= config.full_detail_radius
                && config.full_detail_radius <= config.fine_lod_radius
                && config.fine_lod_radius <= config.coarse_lod_radius,
            "LOD radii must be non-negative and ordered full <= fine <= coarse"
        );
        Self {
            config,
            entries: HashMap::new(),
            memory_bytes: 0,
            evictions: 0,
            tick: 0,
            camera: (0, 0),
            fine_drawn: 0,
            coarse_drawn: 0,
            skipped_full_detail: 0,
        }
    }

    pub fn config(&self) -> LodConfig {
        self.config
    }

    pub fn stats(&self) -> LodStats {
        LodStats {
            stored_columns: self.entries.len(),
            memory_bytes: self.memory_bytes,
            evictions: self.evictions,
            fine_drawn: self.fine_drawn,
            coarse_drawn: self.coarse_drawn,
            skipped_full_detail: self.skipped_full_detail,
        }
    }

    /// Distance tier for a column, measured from the last camera position
    /// passed to [`LodManager::active_lod_columns`] (or (0, 0) before then).
    pub fn tier_for(&self, cx: i32, cz: i32) -> LodTier {
        self.tier_at(self.camera, cx, cz)
    }

    fn tier_at(&self, camera: (i32, i32), cx: i32, cz: i32) -> LodTier {
        let distance = chebyshev(camera, cx, cz);
        if distance <= i64::from(self.config.full_detail_radius) {
            LodTier::FullDetail
        } else if distance <= i64::from(self.config.fine_lod_radius) {
            LodTier::Fine
        } else if distance <= i64::from(self.config.coarse_lod_radius) {
            LodTier::Coarse
        } else {
            LodTier::Culled
        }
    }

    /// Summarize `views` (bottom to top `SectionView`s of one chunk column)
    /// and store the result at fine resolution, reusing the entry's buffer
    /// when the column is already present. Returns whether the column is
    /// stored afterwards; a column that alone exceeds `max_memory_bytes` is
    /// evicted again by the budget pass and reports `false`.
    pub fn insert_column(&mut self, cx: i32, cz: i32, views: &[SectionView<'_>]) -> bool {
        self.tick += 1;
        let tick = self.tick;
        if let Some(entry) = self.entries.get_mut(&(cx, cz)) {
            entry.cells.clear();
            entry.cells.resize(LOD_FINE * LOD_FINE, LodCell::EMPTY);
            summarize_column(views, LOD_FINE, &mut entry.cells);
            entry.last_tick = tick;
        } else {
            let mut cells = vec![LodCell::EMPTY; LOD_FINE * LOD_FINE];
            summarize_column(views, LOD_FINE, &mut cells);
            let entry = LodEntry {
                cells,
                last_tick: tick,
            };
            self.memory_bytes += entry_bytes(&entry);
            self.entries.insert((cx, cz), entry);
        }
        self.evict_over_budget();
        self.entries.contains_key(&(cx, cz))
    }

    /// Drop a stored column, e.g. when its chunk unloads. Returns whether an
    /// entry was removed.
    pub fn remove_column(&mut self, cx: i32, cz: i32) -> bool {
        match self.entries.remove(&(cx, cz)) {
            Some(entry) => {
                self.memory_bytes -= entry_bytes(&entry);
                true
            }
            None => false,
        }
    }

    pub fn contains(&self, cx: i32, cz: i32) -> bool {
        self.entries.contains_key(&(cx, cz))
    }

    /// Fine-resolution cells for a stored column.
    pub fn fine_cells(&self, cx: i32, cz: i32) -> Option<&[LodCell]> {
        self.entries.get(&(cx, cz)).map(|e| e.cells.as_slice())
    }

    /// Coarse-resolution cells for a stored column, derived from the fine
    /// summary on demand.
    pub fn coarse_cells(&self, cx: i32, cz: i32) -> Option<[LodCell; LOD_COARSE * LOD_COARSE]> {
        self.entries.get(&(cx, cz)).map(|e| coarse_cells(&e.cells))
    }

    /// Columns that should be drawn as LOD this frame, sorted by ascending
    /// distance then coordinates for deterministic output. `loaded_full`
    /// holds the chunk columns currently rendered at full detail; stored
    /// columns inside `full_detail_radius` that appear there are skipped
    /// (counted in `skipped_full_detail`) so no column is ever drawn twice.
    /// Stored columns inside the full-detail radius but not yet meshed are
    /// emitted as `Fine` to avoid a hole while their meshes stream in.
    pub fn active_lod_columns(
        &mut self,
        camera: (i32, i32),
        loaded_full: &HashSet<(i32, i32)>,
    ) -> Vec<LodDrawColumn> {
        self.camera = camera;
        self.fine_drawn = 0;
        self.coarse_drawn = 0;
        self.skipped_full_detail = 0;
        let mut out = Vec::new();
        for &(cx, cz) in self.entries.keys() {
            match self.tier_at(camera, cx, cz) {
                LodTier::Culled => {}
                LodTier::FullDetail => {
                    if loaded_full.contains(&(cx, cz)) {
                        self.skipped_full_detail += 1;
                    } else {
                        self.fine_drawn += 1;
                        out.push(LodDrawColumn {
                            cx,
                            cz,
                            tier: LodTier::Fine,
                        });
                    }
                }
                LodTier::Fine => {
                    self.fine_drawn += 1;
                    out.push(LodDrawColumn {
                        cx,
                        cz,
                        tier: LodTier::Fine,
                    });
                }
                LodTier::Coarse => {
                    self.coarse_drawn += 1;
                    out.push(LodDrawColumn {
                        cx,
                        cz,
                        tier: LodTier::Coarse,
                    });
                }
            }
        }
        out.sort_by_key(|c| (chebyshev(camera, c.cx, c.cz), c.cx, c.cz));
        out
    }

    /// Evict farthest-then-oldest columns until the stored bytes fit
    /// `max_memory_bytes`. The camera position used for "farthest" is the
    /// last one passed to [`LodManager::active_lod_columns`].
    fn evict_over_budget(&mut self) {
        while self.memory_bytes > self.config.max_memory_bytes {
            let mut victim: Option<((i32, i32), i64, u64)> = None;
            for (&key, entry) in &self.entries {
                let distance = chebyshev(self.camera, key.0, key.1);
                let better = match victim {
                    None => true,
                    Some((_, best_distance, best_tick)) => {
                        distance > best_distance
                            || (distance == best_distance && entry.last_tick < best_tick)
                    }
                };
                if better {
                    victim = Some((key, distance, entry.last_tick));
                }
            }
            let Some((key, _, _)) = victim else {
                break;
            };
            if let Some(entry) = self.entries.remove(&key) {
                self.memory_bytes -= entry_bytes(&entry);
                self.evictions += 1;
            }
        }
    }
}

/// Encode one column summary as an `mc-spine:lod_v1` frame.
///
/// Layout (all integers little-endian):
/// `"LOD1"` magic, version `1`, resolution byte, cell count u16, `cx`/`cz`
/// i32, then `resolution * resolution` cells of 11 bytes each
/// (`state_id` u32, `tint` u32, `top_y` i16, `light` u8).
pub fn encode_lod_v1(
    cx: i32,
    cz: i32,
    resolution: usize,
    cells: &[LodCell],
) -> Result<Vec<u8>, LodError> {
    if !matches!(resolution, LOD_COARSE | LOD_FINE) {
        return Err(LodError::BadResolution);
    }
    if cells.len() != resolution * resolution {
        return Err(LodError::BadCellCount);
    }
    let mut frame = Vec::with_capacity(LOD_V1_HEADER + cells.len() * LOD_V1_CELL_BYTES);
    frame.extend_from_slice(LOD_V1_MAGIC);
    frame.push(LOD_V1_VERSION);
    frame.push(resolution as u8);
    frame.extend_from_slice(&(cells.len() as u16).to_le_bytes());
    frame.extend_from_slice(&cx.to_le_bytes());
    frame.extend_from_slice(&cz.to_le_bytes());
    for cell in cells {
        frame.extend_from_slice(&cell.state_id.to_le_bytes());
        frame.extend_from_slice(&cell.tint.to_le_bytes());
        frame.extend_from_slice(&cell.top_y.to_le_bytes());
        frame.push(cell.light);
    }
    Ok(frame)
}

/// Decode an `mc-spine:lod_v1` frame. Rejects truncated, oversized,
/// wrong-magic, wrong-version, bad-resolution and count-mismatched input.
pub fn decode_lod_v1(frame: &[u8]) -> Result<LodFrame, LodError> {
    if frame.len() < LOD_V1_HEADER {
        return Err(LodError::Truncated);
    }
    if !frame.starts_with(LOD_V1_MAGIC) {
        return Err(LodError::BadMagic);
    }
    if frame[4] != LOD_V1_VERSION {
        return Err(LodError::BadVersion);
    }
    let resolution = match frame[5] {
        x if x as usize == LOD_COARSE => LOD_COARSE,
        x if x as usize == LOD_FINE => LOD_FINE,
        _ => return Err(LodError::BadResolution),
    };
    let count = u16::from_le_bytes([frame[6], frame[7]]) as usize;
    if count != resolution * resolution {
        return Err(LodError::BadCellCount);
    }
    let expected = LOD_V1_HEADER + count * LOD_V1_CELL_BYTES;
    if frame.len() < expected {
        return Err(LodError::Truncated);
    }
    if frame.len() > expected {
        return Err(LodError::TrailingBytes);
    }
    let cx = i32::from_le_bytes(frame[8..12].try_into().expect("header cx"));
    let cz = i32::from_le_bytes(frame[12..16].try_into().expect("header cz"));
    let mut cells = Vec::with_capacity(count);
    let mut cursor = LOD_V1_HEADER;
    for _ in 0..count {
        let mut cell = LodCell::EMPTY;
        cell.state_id =
            u32::from_le_bytes(frame[cursor..cursor + 4].try_into().expect("cell state_id"));
        cell.tint =
            u32::from_le_bytes(frame[cursor + 4..cursor + 8].try_into().expect("cell tint"));
        cell.top_y = i16::from_le_bytes(
            frame[cursor + 8..cursor + 10]
                .try_into()
                .expect("cell top_y"),
        );
        cell.light = frame[cursor + 10];
        cells.push(cell);
        cursor += LOD_V1_CELL_BYTES;
    }
    Ok(LodFrame {
        cx,
        cz,
        resolution,
        cells,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use mc_spine_client_mesh::{Block, Light, Section};

    fn column_with_top(state_id: u32, y: usize) -> Section {
        let mut section = Section::filled(Block::AIR, Light::new(0, 15));
        section.set(3, y, 5, Block::opaque_cube(state_id), Light::new(2, 11));
        section
    }

    #[test]
    fn tier_boundaries_and_full_detail_handoff() {
        let config = LodConfig {
            full_detail_radius: 2,
            fine_lod_radius: 4,
            coarse_lod_radius: 8,
            max_memory_bytes: usize::MAX,
        };
        let mut lod = LodManager::new(config);
        let section = column_with_top(7, 9);
        let views = [section.view()];
        for (cx, cz) in [(0, 0), (1, 0), (3, 0), (6, 0), (20, 0)] {
            assert!(lod.insert_column(cx, cz, &views));
        }

        let mut loaded = HashSet::new();
        loaded.insert((0, 0));
        loaded.insert((1, 0));
        let draw = lod.active_lod_columns((0, 0), &loaded);
        let by_coord: HashMap<_, _> = draw.iter().map(|c| ((c.cx, c.cz), c.tier)).collect();

        // Full-detail columns are never emitted as LOD: zero overlap.
        assert!(!by_coord.contains_key(&(0, 0)));
        assert!(!by_coord.contains_key(&(1, 0)));
        assert_eq!(by_coord.get(&(3, 0)), Some(&LodTier::Fine));
        assert_eq!(by_coord.get(&(6, 0)), Some(&LodTier::Coarse));
        assert!(!by_coord.contains_key(&(20, 0))); // Culled

        let stats = lod.stats();
        assert_eq!(stats.stored_columns, 5);
        assert_eq!(stats.fine_drawn, 1);
        assert_eq!(stats.coarse_drawn, 1);
        assert_eq!(stats.skipped_full_detail, 2);

        // Unloading the full-detail mesh makes its column reappear as LOD.
        loaded.clear();
        let draw = lod.active_lod_columns((0, 0), &loaded);
        let by_coord: HashMap<_, _> = draw.iter().map(|c| ((c.cx, c.cz), c.tier)).collect();
        assert_eq!(by_coord.get(&(0, 0)), Some(&LodTier::Fine));
        assert_eq!(lod.stats().skipped_full_detail, 0);
        assert_eq!(lod.stats().fine_drawn, 3);
    }

    #[test]
    fn coarse_cells_match_summarize_column() {
        let mut section = Section::filled(Block::AIR, Light::new(0, 15));
        section.set(1, 2, 1, Block::opaque_cube(1), Light::new(0, 0));
        let tall = Block::opaque_cube(42);
        section.set(3, 9, 2, tall, Light::new(0, 0));
        section.set(0, 5, 0, Block::opaque_cube(3), Light::new(0, 0));
        let views = [section.view()];

        let config = LodConfig {
            full_detail_radius: 0,
            fine_lod_radius: 100,
            coarse_lod_radius: 200,
            max_memory_bytes: usize::MAX,
        };
        let mut lod = LodManager::new(config);
        assert!(lod.insert_column(4, -2, &views));

        let mut expected = [LodCell::EMPTY; LOD_COARSE * LOD_COARSE];
        summarize_column(&views, LOD_COARSE, &mut expected);
        let got = lod.coarse_cells(4, -2).unwrap();
        assert_eq!(got, expected);
        assert_eq!(got[0].state_id, tall.state_id);
        assert_eq!(got[0].top_y, 9);

        // Fine cells agree with a direct fine summary too.
        let mut fine_expected = vec![LodCell::EMPTY; LOD_FINE * LOD_FINE];
        summarize_column(&views, LOD_FINE, &mut fine_expected);
        assert_eq!(lod.fine_cells(4, -2).unwrap(), fine_expected.as_slice());
    }

    #[test]
    fn memory_cap_evicts_farthest_then_oldest() {
        // Measure one entry's footprint, then cap at exactly two entries.
        let section = column_with_top(1, 4);
        let views = [section.view()];
        let mut lod = LodManager::new(LodConfig {
            full_detail_radius: 0,
            fine_lod_radius: 1_000,
            coarse_lod_radius: 2_000,
            max_memory_bytes: usize::MAX,
        });
        lod.insert_column(0, 0, &views);
        let one_entry = lod.stats().memory_bytes;
        let cap = one_entry * 2;

        let mut lod = LodManager::new(LodConfig {
            full_detail_radius: 0,
            fine_lod_radius: 1_000,
            coarse_lod_radius: 2_000,
            max_memory_bytes: cap,
        });
        // Camera defaults to (0,0): (50,0) is farthest, (10,0) mid, (1,0) near.
        assert!(lod.insert_column(1, 0, &views));
        assert!(lod.insert_column(50, 0, &views));
        assert!(lod.insert_column(10, 0, &views));
        let stats = lod.stats();
        assert!(stats.memory_bytes <= cap);
        assert_eq!(stats.stored_columns, 2);
        assert_eq!(stats.evictions, 1);
        assert!(lod.contains(1, 0));
        assert!(lod.contains(10, 0));
        assert!(!lod.contains(50, 0));

        // A column that alone exceeds the cap is not stored.
        let mut tiny = LodManager::new(LodConfig {
            full_detail_radius: 0,
            fine_lod_radius: 1_000,
            coarse_lod_radius: 2_000,
            max_memory_bytes: one_entry - 1,
        });
        assert!(!tiny.insert_column(0, 0, &views));
        assert_eq!(tiny.stats().stored_columns, 0);
        assert!(tiny.stats().memory_bytes < one_entry);
    }

    #[test]
    fn lod_v1_round_trip_and_malformed_rejection() {
        let mut section = Section::filled(Block::AIR, Light::new(0, 15));
        section.set(3, 9, 5, Block::opaque_cube(77), Light::new(2, 11));
        let views = [section.view()];
        let mut lod = LodManager::new(LodConfig::default());
        lod.insert_column(-3, 8, &views);
        let fine = lod.fine_cells(-3, 8).unwrap().to_vec();
        let coarse = lod.coarse_cells(-3, 8).unwrap().to_vec();

        for (resolution, cells) in [(LOD_FINE, &fine), (LOD_COARSE, &coarse)] {
            let frame = encode_lod_v1(-3, 8, resolution, cells).unwrap();
            assert_eq!(frame.len(), 16 + resolution * resolution * 11);
            let decoded = decode_lod_v1(&frame).unwrap();
            assert_eq!(decoded.cx, -3);
            assert_eq!(decoded.cz, 8);
            assert_eq!(decoded.resolution, resolution);
            assert_eq!(decoded.cells, *cells);
            // Re-encoding a decoded frame reproduces the exact bytes.
            assert_eq!(
                encode_lod_v1(decoded.cx, decoded.cz, decoded.resolution, &decoded.cells).unwrap(),
                frame
            );
        }

        // Encode-side validation.
        assert_eq!(
            encode_lod_v1(0, 0, 8, &fine[..64]),
            Err(LodError::BadResolution)
        );
        assert_eq!(
            encode_lod_v1(0, 0, LOD_COARSE, &fine[..15]),
            Err(LodError::BadCellCount)
        );

        let good = encode_lod_v1(1, 2, LOD_COARSE, &coarse).unwrap();
        // Truncated at every prefix length.
        for len in 0..good.len() {
            assert!(matches!(
                decode_lod_v1(&good[..len]),
                Err(LodError::Truncated)
                    | Err(LodError::BadMagic)
                    | Err(LodError::BadVersion)
                    | Err(LodError::BadResolution)
                    | Err(LodError::BadCellCount)
            ));
        }
        // Corrupted fields.
        let mut bad = good.clone();
        bad[0] = b'X';
        assert_eq!(decode_lod_v1(&bad), Err(LodError::BadMagic));
        let mut bad = good.clone();
        bad[4] = 2;
        assert_eq!(decode_lod_v1(&bad), Err(LodError::BadVersion));
        let mut bad = good.clone();
        bad[5] = 8;
        assert_eq!(decode_lod_v1(&bad), Err(LodError::BadResolution));
        let mut bad = good.clone();
        bad[6] = 15; // count 15 != 16
        assert_eq!(decode_lod_v1(&bad), Err(LodError::BadCellCount));
        // Oversized frame.
        let mut bad = good.clone();
        bad.push(0);
        assert_eq!(decode_lod_v1(&bad), Err(LodError::TrailingBytes));
    }

    /// The live-path bridge: a loaded `WorldState` column exports mesh
    /// `Section`s, whose views feed `LodManager` exactly like the sim's
    /// chunk-arrival handler.
    #[test]
    fn world_column_sections_feed_lod_manager() {
        use crate::protocol::{DecodedChunk, DecodedLightData, DecodedSection, DimensionMeta};
        use crate::world::{BlockTable, WorldState};
        use mc_spine_client_mesh::SECTION_CELLS;

        let dimension = DimensionMeta {
            name: "minecraft:overworld".to_string(),
            min_y: 0,
            height: 64,
            has_skylight: true,
        };
        let mut world = WorldState::new(dimension, BlockTable::builtin());
        assert!(world.column_sections(0, 0).is_none());

        let mut states = [0u32; SECTION_CELLS];
        states[5 * 16 * 16 + 3 * 16 + 9] = 7;
        let decoded = DecodedChunk {
            cx: 0,
            cz: 0,
            sections: vec![
                DecodedSection {
                    non_air: 1,
                    states: Box::new(states),
                    biomes: [0; 64],
                },
                DecodedSection {
                    non_air: 0,
                    states: Box::new([0; SECTION_CELLS]),
                    biomes: [0; 64],
                },
                DecodedSection {
                    non_air: 0,
                    states: Box::new([0; SECTION_CELLS]),
                    biomes: [0; 64],
                },
                DecodedSection {
                    non_air: 0,
                    states: Box::new([0; SECTION_CELLS]),
                    biomes: [0; 64],
                },
            ],
            light: DecodedLightData {
                sky_mask: Vec::new(),
                block_mask: Vec::new(),
                empty_sky_mask: Vec::new(),
                empty_block_mask: Vec::new(),
                sky_arrays: Vec::new(),
                block_arrays: Vec::new(),
            },
        };
        world.apply_chunk(decoded).unwrap();

        let sections = world.column_sections(0, 0).unwrap();
        assert_eq!(sections.len(), 4);
        let views: Vec<_> = sections.iter().map(|s| s.view()).collect();
        let mut lod = LodManager::new(LodConfig::default());
        assert!(lod.insert_column(0, 0, &views));
        assert!(lod.contains(0, 0));
        // The lone block at (9, 5, 3) surfaces in the fine summary.
        let fine = lod.fine_cells(0, 0).unwrap();
        assert!(fine.iter().any(|c| c.state_id == 7));

        // Unload removes it from the LOD store too, as in the sim path.
        world.unload_chunk(0, 0);
        assert!(world.column_sections(0, 0).is_none());
        assert!(lod.remove_column(0, 0));
    }
}
