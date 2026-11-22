// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
// ----- Vanilla Anvil (.mca region + level.dat) import/export, 1.21.1 + 26.3 -----
//
// P3-A: a second persistence codec next to the Save v2 format (src/save_v2.rs).
// This reads a vanilla world directory (level.dat + region/*.mca, plus the
// DIM-1/DIM1 companions) into this server's overlay world state, and writes the
// same directory shape back out so a vanilla client/server can open the result.
// Both first-class targets share the layout: 1.21.1 (`DataVersion` 3955, the
// native write format) and latest vanilla 26.3 (`DataVersion` 5023).
// All binary NBT goes through the shared Java codec (src/nbt.rs) and flate2 — no
// new dependencies, no new modules (this file is `include!`d into main).

// The NBT model this codec needs is the shared one (src/nbt.rs covers all 13
// vanilla tag types): kept under a local name so the file reads self-contained.
type AnvilNbt = crate::nbt::Tag;

// The `version` field vanilla writes for this release, the DataVersion integer,
// and one sector's size in bytes.
const ANVIL_LEVEL_VERSION: i32 = 19133;
const ANVIL_DATA_VERSION: i32 = 3955;
const ANVIL_SECTOR: usize = 4096;
// Decompression budgets: one chunk payload is at most 255 sectors (~1 MiB) on
// disk but can inflate far past that; level.dat is unbounded on disk. Both are
// far above anything a legitimate 1.21.1 file contains.
const ANVIL_CHUNK_NBT_BUDGET: usize = 64 << 20;
const ANVIL_LEVEL_NBT_BUDGET: usize = 256 << 20;

/// Capture the on-disk Anvil world before importing or migrating it. The
/// caller supplies the timestamp so snapshot identities are deterministic.
#[cfg(feature = "mod-suite")]
#[allow(dead_code)]
fn anvil_take_world_snapshot(
    world_root: &std::path::Path,
    backup_root: &std::path::Path,
    label: &str,
    timestamp_secs: u64,
) -> Result<String, String> {
    use megu_state_store::world_snapshot::{SnapshotReason, WorldSnapshotStore};

    let store = WorldSnapshotStore::new(world_root, backup_root).map_err(|e| e.to_string())?;
    // A damaged or incomplete world must still be recoverable from a snapshot.
    // Record an unknown version instead of attributing it to this server's
    // default Anvil release when level.dat cannot be read.
    let mc_version = read_level_dat_path(&world_root.join("level.dat"))
        .map(|level| {
            AnvilSchemaVersion::from_data_version(level.data_version())
                .release_name()
                .to_string()
        })
        .unwrap_or_else(|_| "unknown".to_string());
    let manifest = store
        .create_snapshot(
            SnapshotReason::PreModChange {
                summary: label.to_string(),
            },
            &mc_version,
            timestamp_secs,
        )
        .map_err(|e| e.to_string())?;
    Ok(manifest.snapshot_id)
}

/// Validate the stored content hashes before restoring the complete world.
#[cfg(feature = "mod-suite")]
#[allow(dead_code)]
fn anvil_verify_and_restore_world_snapshot(
    world_root: &std::path::Path,
    backup_root: &std::path::Path,
    snapshot_id: &str,
    timestamp_secs: u64,
) -> Result<(), String> {
    use megu_state_store::world_snapshot::WorldSnapshotStore;

    let store = WorldSnapshotStore::new(world_root, backup_root).map_err(|e| e.to_string())?;
    store
        .verify_snapshot(snapshot_id)
        .map_err(|e| e.to_string())?;
    store
        .restore_snapshot(snapshot_id, timestamp_secs)
        .map_err(|e| e.to_string())?;
    Ok(())
}

#[cfg(not(feature = "mod-suite"))]
#[allow(dead_code)]
fn anvil_take_world_snapshot(
    _world_root: &std::path::Path,
    _backup_root: &std::path::Path,
    _label: &str,
    _timestamp_secs: u64,
) -> Result<String, String> {
    Err("world snapshots require --features mod-suite".to_string())
}

#[cfg(not(feature = "mod-suite"))]
#[allow(dead_code)]
fn anvil_verify_and_restore_world_snapshot(
    _world_root: &std::path::Path,
    _backup_root: &std::path::Path,
    _snapshot_id: &str,
    _timestamp_secs: u64,
) -> Result<(), String> {
    Err("world snapshots require --features mod-suite".to_string())
}

// ---------- DataVersion schema table ----------
//
// Vanilla stamps every `level.dat` and chunk record with a `DataVersion`
// integer that names the release's save schema. The codec writes 1.21.1
// natively and reads the releases whose section layout is identical — the
// 1.20.4-1.21.5 line plus latest vanilla 26.3, the second first-class
// target: all store `sections[].Y`, `block_states`/`biomes` palettes and the
// same packed `data` arrays. The table below maps each known milestone and
// classifies everything around it.

/// A Minecraft Java release line the anvil codec can name, keyed by the
/// `DataVersion` integer vanilla writes for it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AnvilSchemaVersion {
    V1_20_4,
    V1_20_6,
    V1_21,
    /// 1.21.1 (`DataVersion` 3955): the codec's native format.
    V1_21_1,
    V1_21_3,
    V1_21_4,
    V1_21_5,
    /// 26.3 (`DataVersion` 5023): the latest-vanilla release line, a
    /// supported target next to native 1.21.1.
    V26_3,
    /// A `DataVersion` this table does not list — a snapshot between
    /// milestones, an older save, or a release newer than the table.
    Unknown(i32),
}

impl AnvilSchemaVersion {
    fn data_version(self) -> i32 {
        match self {
            Self::V1_20_4 => 3700,
            Self::V1_20_6 => 3839,
            Self::V1_21 => 3953,
            Self::V1_21_1 => 3955,
            Self::V1_21_3 => 4082,
            Self::V1_21_4 => 4189,
            Self::V1_21_5 => 4325,
            Self::V26_3 => 5023,
            Self::Unknown(dv) => dv,
        }
    }

    /// The `Version.Name` vanilla writes for the milestone; a custom target
    /// has no release name to claim.
    fn release_name(self) -> &'static str {
        match self {
            Self::V1_20_4 => "1.20.4",
            Self::V1_20_6 => "1.20.6",
            Self::V1_21 => "1.21",
            Self::V1_21_1 => "1.21.1",
            Self::V1_21_3 => "1.21.3",
            Self::V1_21_4 => "1.21.4",
            Self::V1_21_5 => "1.21.5",
            Self::V26_3 => "26.3",
            Self::Unknown(_) => "custom",
        }
    }

    fn from_data_version(dv: i32) -> AnvilSchemaVersion {
        match dv {
            3700 => Self::V1_20_4,
            3839 => Self::V1_20_6,
            3953 => Self::V1_21,
            3955 => Self::V1_21_1,
            4082 => Self::V1_21_3,
            4189 => Self::V1_21_4,
            4325 => Self::V1_21_5,
            5023 => Self::V26_3,
            other => Self::Unknown(other),
        }
    }
}

/// How one observed `DataVersion` relates to what this codec reads and writes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AnvilVersionSupport {
    /// `ANVIL_DATA_VERSION` itself: the codec's native 1.21.1 format.
    Native,
    /// A known milestone or a snapshot `DataVersion` between milestones: the
    /// section/palette schema this codec implements has not changed across the
    /// table, so decode is meaningful.
    Compatible,
    /// Older than the table's first milestone (or a missing/corrupt value):
    /// the save predates the section schema this codec assumes.
    LegacyUnsupported,
    /// Newer than the table's last milestone: an unlisted release whose schema
    /// cannot be assumed.
    FutureUnknown,
}

/// Classify an observed chunk or level.dat `DataVersion` against the schema
/// table. A value that is absent or corrupt decodes as 0 upstream and lands in
/// `LegacyUnsupported` here — recorded, never silently upgraded to a known one.
fn classify_anvil_data_version(dv: i32) -> AnvilVersionSupport {
    match AnvilSchemaVersion::from_data_version(dv) {
        AnvilSchemaVersion::V1_21_1 => AnvilVersionSupport::Native,
        AnvilSchemaVersion::Unknown(d)
            if d <= 0 || d < AnvilSchemaVersion::V1_20_4.data_version() =>
        {
            AnvilVersionSupport::LegacyUnsupported
        }
        AnvilSchemaVersion::Unknown(d)
            if d > AnvilSchemaVersion::V26_3.data_version() =>
        {
            AnvilVersionSupport::FutureUnknown
        }
        _ => AnvilVersionSupport::Compatible,
    }
}

// ---------- small NBT accessors (AnvilNbt covers all 13 vanilla tags) ----------

fn anvil_str_of(t: &AnvilNbt) -> Option<String> {
    t.string()
        .map(|s| String::from_utf16_lossy(&s.0))
}
fn anvil_i8(t: &AnvilNbt, key: &str) -> Option<i8> {
    match t.get(key) {
        Some(AnvilNbt::Byte(v)) => Some(*v),
        _ => None,
    }
}
fn anvil_i32(t: &AnvilNbt, key: &str) -> Option<i32> {
    t.get(key).and_then(AnvilNbt::int)
}
fn anvil_i64(t: &AnvilNbt, key: &str) -> Option<i64> {
    match t.get(key) {
        Some(AnvilNbt::Long(v)) => Some(*v),
        Some(AnvilNbt::Int(v)) => Some(i64::from(*v)),
        _ => None,
    }
}
fn anvil_f64(t: &AnvilNbt, key: &str) -> Option<f64> {
    match t.get(key) {
        Some(AnvilNbt::Double(v)) => Some(f64::from_bits(*v)),
        Some(AnvilNbt::Float(v)) => Some(f64::from(f32::from_bits(*v))),
        _ => None,
    }
}
fn anvil_f32(t: &AnvilNbt, key: &str) -> Option<f32> {
    match t.get(key) {
        Some(AnvilNbt::Float(v)) => Some(f32::from_bits(*v)),
        Some(AnvilNbt::Double(v)) => Some(f64::from_bits(*v) as f32),
        _ => None,
    }
}
fn anvil_str(t: &AnvilNbt, key: &str) -> Option<String> {
    t.get(key).and_then(anvil_str_of)
}
fn anvil_s(v: &str) -> AnvilNbt {
    AnvilNbt::String(crate::nbt::NbtString::text(v))
}
fn anvil_obj(pairs: &[(&str, AnvilNbt)]) -> AnvilNbt {
    AnvilNbt::Compound(
        pairs
            .iter()
            .map(|(k, v)| (crate::nbt::NbtString::text(k), v.clone()))
            .collect(),
    )
}
fn anvil_list(kind_items: Vec<AnvilNbt>) -> AnvilNbt {
    let kind = kind_items.first().map_or(0, AnvilNbt::id);
    AnvilNbt::List(kind, kind_items)
}

// ---------- bit-packed LongArray (1.16+ non-spanning layout) ----------
//
// Vanilla's SimpleBitStorage: `values_per_long = 64 / bits`, entries never cross
// a 64-bit boundary, so a width that does not divide 64 wastes the tail bits of
// every long (bits=9 -> 7 entries, 1 wasted bit). The palette-size -> width rule
// is `max(min_bits, ceillog2(len))`: 4 for blockstates, 1 for biomes.
fn anvil_bits_for(len: usize, min_bits: u8) -> u8 {
    let len = len.max(2);
    let bits = (usize::BITS - (len - 1).leading_zeros()) as u8;
    bits.max(min_bits)
}
fn anvil_pack_longs(values: &[u64], bits: u8) -> Vec<i64> {
    debug_assert!((1..=32).contains(&bits));
    let bits = bits as usize;
    let per = 64 / bits;
    let mask = (1u64 << bits) - 1;
    let mut words = vec![0u64; values.len().div_ceil(per)];
    for (i, &v) in values.iter().enumerate() {
        words[i / per] |= (v & mask) << ((i % per) * bits);
    }
    words.into_iter().map(|w| w as i64).collect()
}
fn anvil_unpack_longs(data: &[i64], bits: u8, count: usize) -> Result<Vec<u64>, String> {
    if !(1..=32).contains(&bits) {
        return Err(format!("invalid packed-array width {bits}"));
    }
    let bits = bits as usize;
    let per = 64 / bits;
    if data.len() < count.div_ceil(per) {
        return Err(format!(
            "packed array truncated: {} longs for {count} {bits}-bit entries",
            data.len()
        ));
    }
    let mask = (1u64 << bits) - 1;
    Ok((0..count)
        .map(|i| ((data[i / per] as u64) >> ((i % per) * bits)) & mask)
        .collect())
}

// ---------- blockstate <-> breg() state ids ----------

/// One palette entry -> this registry's state id. Unknown blocks, properties or
/// values (mods, newer saves) count toward `unknown` and decode as air — the
// chunk still imports; only the unresolvable cells are lost.
fn anvil_state_id(entry: &AnvilNbt, unknown: &mut usize) -> u32 {
    let Some(name) = anvil_str(entry, "Name") else {
        *unknown += 1;
        return 0;
    };
    let mut props = Vec::new();
    if let Some(AnvilNbt::Compound(fields)) = entry.get("Properties") {
        for (k, v) in fields {
            if let Some(v) = anvil_str_of(v) {
                props.push((String::from_utf16_lossy(&k.0), v));
            }
        }
    }
    match breg().and_then(|reg| reg.checked_state(&name, &props)) {
        Some(st) => st,
        None => {
            *unknown += 1;
            0
        }
    }
}

/// This registry's state id -> the `{Name, Properties?}` palette entry vanilla
/// writes. `Properties` is omitted for blocks with none, matching vanilla.
fn anvil_state_entry(st: u32) -> AnvilNbt {
    let (name, props) = match breg() {
        Some(reg) => (
            reg.name_of(st).to_string(),
            reg.properties_of(st).unwrap_or_default(),
        ),
        None => ("minecraft:air".to_string(), Vec::new()),
    };
    let mut entry = vec![("Name", anvil_s(&name))];
    if !props.is_empty() {
        entry.push((
            "Properties",
            anvil_obj(
                &props
                    .iter()
                    .map(|(k, v)| (k.as_str(), anvil_s(v)))
                    .collect::<Vec<_>>(),
            ),
        ));
    }
    anvil_obj(&entry)
}

// ---------- .mca region file ----------

/// One stored chunk: the NBT root exactly as decoded (minus decompression), the
/// timestamp table entry, and the compression flavour to write it back with.
/// Keeping the raw compound is what makes the region round-trip lossless for
/// fields this reader does not interpret (heightmaps, entities, tick queues).
#[derive(Clone, Debug, PartialEq)]
struct AnvilChunkRecord {
    timestamp: u32,
    compression: u8, // 1 = Gzip, 2 = Zlib, 3 = uncompressed
    nbt: AnvilNbt,
}

/// The chunk slots of one `r.<rx>.<rz>.mca` file, keyed by world chunk coords.
#[derive(Clone, Debug, Default, PartialEq)]
struct AnvilRegion {
    chunks: std::collections::BTreeMap<(i32, i32), AnvilChunkRecord>,
}

/// `r.<rx>.<rz>.mca` -> (rx, rz); negative coords parse as written.
fn anvil_region_coords(path: &std::path::Path) -> Result<(i32, i32), String> {
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .ok_or_else(|| format!("region path has no file name: {}", path.display()))?;
    let parts: Vec<&str> = stem.split('.').collect();
    if parts.len() != 3 || parts[0] != "r" {
        return Err(format!("not a region file name: {}", path.display()));
    }
    let rx = parts[1]
        .parse::<i32>()
        .map_err(|_| format!("bad region x in {}", path.display()))?;
    let rz = parts[2]
        .parse::<i32>()
        .map_err(|_| format!("bad region z in {}", path.display()))?;
    Ok((rx, rz))
}

fn anvil_decompress(compression: u8, payload: &[u8]) -> Result<Vec<u8>, String> {
    let limit = (ANVIL_CHUNK_NBT_BUDGET as u64) + 1;
    let mut out = Vec::new();
    match compression {
        1 => {
            flate2::read::GzDecoder::new(payload)
                .take(limit)
                .read_to_end(&mut out)
                .map_err(|e| format!("invalid gzip chunk payload: {e}"))?;
        }
        2 => {
            flate2::read::ZlibDecoder::new(payload)
                .take(limit)
                .read_to_end(&mut out)
                .map_err(|e| format!("invalid zlib chunk payload: {e}"))?;
        }
        3 => out.extend_from_slice(payload),
        other => return Err(format!("unknown chunk compression type {other}")),
    }
    if out.len() > ANVIL_CHUNK_NBT_BUDGET {
        return Err("chunk NBT exceeds size budget".to_string());
    }
    Ok(out)
}

fn anvil_compress(compression: u8, nbt: &[u8]) -> Result<Vec<u8>, String> {
    match compression {
        1 => {
            let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
            e.write_all(nbt)
                .and_then(|_| e.finish())
                .map_err(|e| format!("gzip chunk encode failed: {e}"))
        }
        2 => {
            let mut e =
                flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
            e.write_all(nbt)
                .and_then(|_| e.finish())
                .map_err(|e| format!("zlib chunk encode failed: {e}"))
        }
        3 => Ok(nbt.to_vec()),
        other => Err(format!("unknown chunk compression type {other}")),
    }
}

/// Read one `r.<rx>.<rz>.mca`: the 4 KiB location table, the 4 KiB timestamp
/// table, then each live slot's `len | compression | payload` record. Strict:
/// the first corrupt slot aborts the read — the merge-write path depends on
/// never rewriting a file it could not fully parse. The slot loop lives in
/// `src/region.rs` (`read_region_file_impl`), shared with the tolerant import
/// reader.
#[allow(dead_code)] // The console/import command is a later phase; tests drive it today.
fn read_region_file(path: &std::path::Path) -> Result<AnvilRegion, String> {
    read_region_file_impl(path, false).map(|(region, _)| region)
}

/// `path.tmp` -> `path`, fsynced before the rename — the same discipline
/// `write_world_file` uses for the v2 save, minus its backup generations.
fn anvil_atomic_write(path: &std::path::Path, bytes: &[u8]) -> Result<(), String> {
    let mut tmp_name = path.as_os_str().to_owned();
    tmp_name.push(".tmp");
    let tmp = std::path::PathBuf::from(tmp_name);
    {
        let mut f = std::fs::File::create(&tmp)
            .map_err(|e| format!("cannot create {}: {e}", tmp.display()))?;
        if let Err(e) = f.write_all(bytes).and_then(|_| f.sync_all()) {
            drop(f);
            let _ = std::fs::remove_file(&tmp);
            return Err(format!("cannot write {}: {e}", tmp.display()));
        }
    }
    std::fs::rename(&tmp, path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        format!("cannot rename {} over {}: {e}", tmp.display(), path.display())
    })?;
    // Pin the directory entry too: without it a crash can roll the rename back
    // even though every staged byte was fsynced first.
    anvil_sync_parent_dir(path);
    Ok(())
}

/// Write `region` to `path`, which must be named `r.<rx>.<rz>.mca` for the chunk
/// coordinates it holds. Sectors are packed back to back from sector 2 and every
/// payload is zero-padded to a 4 KiB boundary; timestamps come from the records.
#[allow(dead_code)] // Used by export_anvil_world and the tests.
fn write_region_file(path: &std::path::Path, region: &AnvilRegion) -> Result<(), String> {
    let (rx, rz) = anvil_region_coords(path)?;
    let mut locations = [0u8; ANVIL_SECTOR];
    let mut timestamps = [0u8; ANVIL_SECTOR];
    let mut body = Vec::new();
    for (&(cx, cz), record) in &region.chunks {
        if cx.div_euclid(32) != rx || cz.div_euclid(32) != rz {
            return Err(format!(
                "chunk ({cx}, {cz}) does not belong in {}",
                path.display()
            ));
        }
        let nbt = crate::nbt::write_named(&crate::nbt::NbtString(Vec::new()), &record.nbt)
            .map_err(|e| format!("cannot encode chunk NBT for {path:?}: {e}"))?;
        let compressed = anvil_compress(record.compression, &nbt)?;
        let len = compressed.len() + 1;
        let sectors = (4 + len).div_ceil(ANVIL_SECTOR);
        if sectors > 255 {
            return Err(format!("chunk ({cx}, {cz}) exceeds 255 sectors"));
        }
        let offset = 2 + body.len() / ANVIL_SECTOR;
        body.extend_from_slice(&(len as u32).to_be_bytes());
        body.push(record.compression);
        body.extend_from_slice(&compressed);
        body.resize(body.len().div_ceil(ANVIL_SECTOR) * ANVIL_SECTOR, 0);
        let slot = ((cx & 31) + (cz & 31) * 32) as usize;
        locations[slot * 4] = ((offset >> 16) & 0xff) as u8;
        locations[slot * 4 + 1] = ((offset >> 8) & 0xff) as u8;
        locations[slot * 4 + 2] = (offset & 0xff) as u8;
        locations[slot * 4 + 3] = sectors as u8;
        timestamps[slot * 4..slot * 4 + 4]
            .copy_from_slice(&record.timestamp.to_be_bytes());
    }
    let mut file = Vec::with_capacity(2 * ANVIL_SECTOR + body.len());
    file.extend_from_slice(&locations);
    file.extend_from_slice(&timestamps);
    file.extend_from_slice(&body);
    anvil_atomic_write(path, &file)
}

// PERF-DISK-SAVE: the content-hash skip, incremental save queue, dir-sync and
// staging-recovery machinery, and the shared strict/tolerant region-slot
// reader live in the sibling file (same translation unit as this one).
include!("region.rs");

// ---------- chunk NBT <-> megu section/state model ----------

/// One 16x16x16 section decoded to this registry's state ids. `biomes` keeps the
/// canonical names (import does not apply them; export writes them back out).
#[derive(Clone, Debug, PartialEq)]
struct AnvilSection {
    y: i8,
    blocks: Box<[u32; 4096]>,
    biomes: Option<Box<[String; 64]>>,
    block_light: Option<Box<[u8; 2048]>>,
    sky_light: Option<Box<[u8; 2048]>>,
}

/// A decoded vanilla chunk: everything `world()` needs plus the fields this
/// server preserves but does not consume.
#[derive(Clone, Debug, PartialEq)]
struct AnvilChunk {
    cx: i32,
    cz: i32,
    data_version: i32,
    status: String,
    sections: Vec<AnvilSection>,
    block_entities: Vec<AnvilNbt>,
    /// Palette entries that resolved to no known state — informational only.
    unknown_states: usize,
    /// Section entries dropped because they carried no `Y` index at all.
    dropped_sections: usize,
}

fn anvil_light(sec: &AnvilNbt, key: &str) -> Option<Box<[u8; 2048]>> {
    match sec.get(key) {
        Some(AnvilNbt::ByteArray(v)) if v.len() == 2048 => {
            Some(Box::new(v[..2048].try_into().unwrap()))
        }
        _ => None,
    }
}

/// One section entry -> `Some(section)`, or `None` when the entry carries no
/// section index (`Y` — or the lowercase `y` spelling some 1.21.2+ toolchains
/// emit) and so cannot be placed. An unplaceable entry is dropped and counted,
/// not a chunk-wide decode failure; the fields below stay optional exactly as
/// they are on the wire (`block_states`, `biomes`, both light arrays), and any
/// real structural error inside them still fails the chunk.
fn anvil_section_from_nbt(
    sec: &AnvilNbt,
    unknown: &mut usize,
) -> Result<Option<AnvilSection>, String> {
    let Some(y) = anvil_i8(sec, "Y").or_else(|| anvil_i8(sec, "y")) else {
        return Ok(None);
    };
    let mut blocks = Box::new([0u32; 4096]);
    if let Some(bs) = sec.get("block_states") {
        let palette_tag = bs
            .get("palette")
            .and_then(AnvilNbt::list)
            .ok_or("block_states palette missing")?;
        let mut palette = Vec::with_capacity(palette_tag.len());
        for entry in palette_tag {
            palette.push(anvil_state_id(entry, unknown));
        }
        match bs.get("data") {
            Some(AnvilNbt::LongArray(data)) => {
                let bits = anvil_bits_for(palette.len(), 4);
                for (i, p) in anvil_unpack_longs(data, bits, 4096)?.iter().enumerate() {
                    blocks[i] = *palette
                        .get(*p as usize)
                        .ok_or("block_states palette index out of range")?;
                }
            }
            Some(_) => return Err("block_states data is not a long array".to_string()),
            None => {
                if palette.len() != 1 {
                    return Err("block_states data missing".to_string());
                }
                blocks.fill(palette[0]);
            }
        }
    }
    let mut biomes = None;
    if let Some(bi) = sec.get("biomes") {
        let palette_tag = bi
            .get("palette")
            .and_then(AnvilNbt::list)
            .ok_or("biomes palette missing")?;
        let mut names = Vec::with_capacity(palette_tag.len());
        for entry in palette_tag {
            names.push(anvil_str_of(entry).ok_or("biome palette entry is not a string")?);
        }
        let mut cells = Box::new(std::array::from_fn::<String, 64, _>(|_| String::new()));
        match bi.get("data") {
            Some(AnvilNbt::LongArray(data)) => {
                let bits = anvil_bits_for(names.len(), 1);
                for (i, p) in anvil_unpack_longs(data, bits, 64)?.iter().enumerate() {
                    cells[i] = names
                        .get(*p as usize)
                        .ok_or("biomes palette index out of range")?
                        .clone();
                }
            }
            Some(_) => return Err("biomes data is not a long array".to_string()),
            None => {
                if names.len() != 1 {
                    return Err("biomes data missing".to_string());
                }
                cells.fill(names[0].clone());
            }
        }
        biomes = Some(cells);
    }
    Ok(Some(AnvilSection {
        y,
        blocks,
        biomes,
        block_light: anvil_light(sec, "BlockLight"),
        sky_light: anvil_light(sec, "SkyLight"),
    }))
}

/// Decode one chunk record's root compound. `fallback` supplies the position
/// when the (optional) `xPos`/`zPos` fields are absent.
fn anvil_chunk_from_nbt(
    nbt: &AnvilNbt,
    fallback: (i32, i32),
) -> Result<AnvilChunk, String> {
    if !matches!(nbt, AnvilNbt::Compound(_)) {
        return Err("chunk root is not a compound".to_string());
    }
    let mut unknown = 0usize;
    let mut sections = Vec::new();
    let mut dropped_sections = 0usize;
    if let Some(list) = nbt.get("sections").and_then(AnvilNbt::list) {
        for sec in list {
            match anvil_section_from_nbt(sec, &mut unknown)? {
                Some(section) => sections.push(section),
                None => dropped_sections += 1,
            }
        }
    }
    let mut block_entities = Vec::new();
    if let Some(list) = nbt.get("block_entities").and_then(AnvilNbt::list) {
        for entity in list {
            if matches!(entity, AnvilNbt::Compound(_)) {
                block_entities.push(entity.clone());
            }
        }
    }
    Ok(AnvilChunk {
        cx: anvil_i32(nbt, "xPos").unwrap_or(fallback.0),
        cz: anvil_i32(nbt, "zPos").unwrap_or(fallback.1),
        data_version: anvil_i32(nbt, "DataVersion").unwrap_or(0),
        status: anvil_str(nbt, "Status").unwrap_or_default(),
        sections,
        block_entities,
        unknown_states: unknown,
        dropped_sections,
    })
}

/// The `data` long array for one section's 4096 palette indices, or `None`
/// when a single-entry palette makes it implicit (the exact 1.21.1 omission).
fn anvil_pack_section(
    cells: &[u32; 4096],
    palette: &mut Vec<u32>,
) -> Option<Vec<i64>> {
    let mut index_of = HashMap::with_capacity(64);
    let mut indices = Vec::with_capacity(4096);
    for &st in cells {
        let next = palette.len();
        let idx = *index_of.entry(st).or_insert_with(|| {
            palette.push(st);
            next
        });
        indices.push(idx as u64);
    }
    if palette.len() == 1 {
        return None;
    }
    Some(anvil_pack_longs(
        &indices,
        anvil_bits_for(palette.len(), 4),
    ))
}

/// Assemble one chunk's `sections` list entry: `Y`, `block_states`, `biomes`,
/// and the two light arrays when they exist.
fn anvil_section_to_nbt(sec: &AnvilSection) -> AnvilNbt {
    let mut palette = Vec::new();
    let data = anvil_pack_section(&sec.blocks, &mut palette);
    let mut block_states = vec![(
        "palette",
        anvil_list(palette.iter().map(|&st| anvil_state_entry(st)).collect()),
    )];
    if let Some(data) = data {
        block_states.push(("data", AnvilNbt::LongArray(data)));
    }
    let mut fields = vec![
        ("Y", AnvilNbt::Byte(sec.y)),
        ("block_states", anvil_obj(&block_states)),
    ];
    let names: &[String] = sec
        .biomes
        .as_deref()
        .map(|b| b.as_slice())
        .unwrap_or(&[]);
    let mut bpal: Vec<&str> = Vec::new();
    let mut bindices = Vec::with_capacity(64);
    for name in names.iter().map(String::as_str) {
        let idx = match bpal.iter().position(|&n| n == name) {
            Some(i) => i,
            None => {
                bpal.push(name);
                bpal.len() - 1
            }
        };
        bindices.push(idx as u64);
    }
    if bpal.is_empty() {
        bpal.push("minecraft:plains");
        bindices = vec![0; 64];
    }
    let mut biomes = vec![(
        "palette",
        anvil_list(bpal.iter().map(|n| anvil_s(n)).collect()),
    )];
    if bpal.len() != 1 {
        biomes.push((
            "data",
            AnvilNbt::LongArray(anvil_pack_longs(
                &bindices,
                anvil_bits_for(bpal.len(), 1),
            )),
        ));
    }
    fields.push(("biomes", anvil_obj(&biomes)));
    if let Some(light) = &sec.block_light {
        fields.push(("BlockLight", AnvilNbt::ByteArray(light.to_vec())));
    }
    if let Some(light) = &sec.sky_light {
        fields.push(("SkyLight", AnvilNbt::ByteArray(light.to_vec())));
    }
    anvil_obj(&fields)
}

/// Encode a decoded chunk to the 1.21.1 root compound: `DataVersion`,
/// `xPos`/`zPos`/`yPos`, `Status`, `sections`, `block_entities`.
fn anvil_chunk_to_nbt(chunk: &AnvilChunk, y_pos: i32) -> AnvilNbt {
    let sections: Vec<AnvilNbt> =
        chunk.sections.iter().map(anvil_section_to_nbt).collect();
    anvil_obj(&[
        ("DataVersion", AnvilNbt::Int(chunk.data_version)),
        ("xPos", AnvilNbt::Int(chunk.cx)),
        ("zPos", AnvilNbt::Int(chunk.cz)),
        ("yPos", AnvilNbt::Int(y_pos)),
        ("Status", anvil_s(if chunk.status.is_empty() { "minecraft:full" } else { &chunk.status })),
        ("sections", anvil_list(sections)),
        (
            "block_entities",
            anvil_list(chunk.block_entities.clone()),
        ),
    ])
}

/// A decoded chunk's blocks as `world()` overlay cells for `dim`. Cells outside
/// the dimension's y window (a corrupt or foreign section list) are skipped.
fn anvil_chunk_edits(chunk: &AnvilChunk, dim: DimId) -> HashMap<u32, u32> {
    let mut edits = HashMap::new();
    let min_y = dim.min_y();
    let top_y = min_y + dim.height();
    for sec in &chunk.sections {
        let sy = i32::from(sec.y) * 16;
        if sy + 16 <= min_y || sy >= top_y {
            continue;
        }
        for lz in 0..16i32 {
            for ly in 0..16i32 {
                for lx in 0..16i32 {
                    let y = sy + ly;
                    if y < min_y || y >= top_y {
                        continue;
                    }
                    let st = sec.blocks[((ly << 8) | (lz << 4) | lx) as usize];
                    edits.insert(
                        local_idx(chunk.cx * 16 + lx, y, chunk.cz * 16 + lz),
                        st,
                    );
                }
            }
        }
    }
    edits
}

// ---------- level.dat (Gzip NBT, root { Data: {...} }) ----------

/// A parsed `level.dat`: the whole `Data` compound is kept verbatim, so fields
/// this server does not model (Player, DataPacks, enabled feature flags, custom
/// generator settings) survive an import -> export cycle byte-for-byte.
#[derive(Clone, Debug, PartialEq)]
struct AnvilLevelDat {
    data: AnvilNbt,
}

impl AnvilLevelDat {
    fn data(&self) -> &AnvilNbt {
        &self.data
    }
    fn data_mut(&mut self) -> &mut AnvilNbt {
        &mut self.data
    }

    #[cfg_attr(not(test), allow(dead_code))]
    fn data_version(&self) -> i32 {
        anvil_i32(self.data(), "DataVersion").unwrap_or(0)
    }
    #[cfg_attr(not(test), allow(dead_code))]
    fn version_id(&self) -> i32 {
        anvil_i32(self.data(), "version").unwrap_or(0)
    }
    #[cfg_attr(not(test), allow(dead_code))]
    fn level_name(&self) -> String {
        anvil_str(self.data(), "LevelName").unwrap_or_default()
    }
    fn seed(&self) -> i64 {
        self.data()
            .get("WorldGenSettings")
            .and_then(|w| anvil_i64(w, "seed"))
            .unwrap_or(0)
    }
    /// (SpawnX, SpawnY, SpawnZ, SpawnAngle).
    fn spawn(&self) -> (i32, i32, i32, f32) {
        (
            anvil_i32(self.data(), "SpawnX").unwrap_or(0),
            anvil_i32(self.data(), "SpawnY").unwrap_or(0),
            anvil_i32(self.data(), "SpawnZ").unwrap_or(0),
            anvil_f32(self.data(), "SpawnAngle").unwrap_or(0.0),
        )
    }
    fn time(&self) -> i64 {
        anvil_i64(self.data(), "Time").unwrap_or(0)
    }
    fn day_time(&self) -> i64 {
        anvil_i64(self.data(), "DayTime").unwrap_or(0)
    }
    fn difficulty(&self) -> i8 {
        anvil_i8(self.data(), "Difficulty").unwrap_or(2)
    }
    #[cfg_attr(not(test), allow(dead_code))]
    fn difficulty_locked(&self) -> bool {
        anvil_i8(self.data(), "DifficultyLocked").unwrap_or(0) != 0
    }
    #[cfg_attr(not(test), allow(dead_code))]
    fn game_type(&self) -> i32 {
        anvil_i32(self.data(), "GameType").unwrap_or(0)
    }
    /// (raining, rainTime, thundering, thunderTime).
    fn weather(&self) -> (bool, i32, bool, i32) {
        (
            anvil_i8(self.data(), "raining").unwrap_or(0) != 0,
            anvil_i32(self.data(), "rainTime").unwrap_or(0),
            anvil_i8(self.data(), "thundering").unwrap_or(0) != 0,
            anvil_i32(self.data(), "thunderTime").unwrap_or(0),
        )
    }
    /// (center_x, center_z, size, warning_blocks, warning_time, damage/block, safe_zone).
    fn border(&self) -> (f64, f64, f64, f64, f64, f64, f64) {
        (
            anvil_f64(self.data(), "BorderCenterX").unwrap_or(0.0),
            anvil_f64(self.data(), "BorderCenterZ").unwrap_or(0.0),
            anvil_f64(self.data(), "BorderSize").unwrap_or(WORLD_BORDER_MAX_SIZE),
            anvil_f64(self.data(), "BorderWarningBlocks").unwrap_or(5.0),
            anvil_f64(self.data(), "BorderWarningTime").unwrap_or(15.0),
            anvil_f64(self.data(), "BorderDamagePerBlock").unwrap_or(0.2),
            anvil_f64(self.data(), "BorderSafeZone").unwrap_or(5.0),
        )
    }
    /// name -> the string-encoded value vanilla stores ("true"/"false"/digits).
    fn game_rules(&self) -> std::collections::BTreeMap<String, String> {
        let mut rules = std::collections::BTreeMap::new();
        if let Some(AnvilNbt::Compound(values)) = self.data().get("GameRules") {
            for (k, v) in values {
                if let Some(v) = anvil_str_of(v) {
                    rules.insert(String::from_utf16_lossy(&k.0), v);
                }
            }
        }
        rules
    }

    /// One field write into the `Data` compound.
    fn set(&mut self, key: &str, value: AnvilNbt) {
        if let AnvilNbt::Compound(values) = self.data_mut() {
            values.insert(crate::nbt::NbtString::text(key), value);
        }
    }
}

#[allow(dead_code)] // Driven by import_anvil_world and the tests.
fn read_level_dat(bytes: &[u8]) -> Result<AnvilLevelDat, String> {
    let (_, root) = crate::nbt::read_gzip(bytes, ANVIL_LEVEL_NBT_BUDGET)
        .map_err(|e| format!("invalid level.dat NBT: {e}"))?;
    match root.get("Data") {
        Some(data @ AnvilNbt::Compound(_)) => Ok(AnvilLevelDat {
            data: data.clone(),
        }),
        _ => Err("level.dat has no Data compound".to_string()),
    }
}

#[allow(dead_code)] // Driven by import/export and the tests.
fn write_level_dat(level: &AnvilLevelDat) -> Result<Vec<u8>, String> {
    let root = anvil_obj(&[("Data", level.data().clone())]);
    let nbt = crate::nbt::write_named(&crate::nbt::NbtString(Vec::new()), &root)
        .map_err(|e| format!("cannot encode level.dat: {e}"))?;
    let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    e.write_all(&nbt)
        .and_then(|_| e.finish())
        .map_err(|e| format!("cannot gzip level.dat: {e}"))
}

#[allow(dead_code)] // Driven by import/export and the tests.
fn read_level_dat_path(path: &std::path::Path) -> Result<AnvilLevelDat, String> {
    let bytes =
        std::fs::read(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    read_level_dat(&bytes).map_err(|e| format!("{}: {e}", path.display()))
}

#[allow(dead_code)] // Driven by export_anvil_world and the tests.
fn write_level_dat_path(path: &std::path::Path, level: &AnvilLevelDat) -> Result<(), String> {
    let bytes = write_level_dat(level)?;
    anvil_atomic_write(path, &bytes)
}

/// Every gamerule value as vanilla's `GameRules` compound stores it: booleans as
/// "true"/"false", integers as decimal strings.
fn anvil_gamerule_string(is_bool: bool, value: i32) -> String {
    if is_bool {
        if value != 0 { "true" } else { "false" }.to_string()
    } else {
        value.to_string()
    }
}

/// Live `GameRules` compound from `command_gamerules()` + the eight atomic rules
/// `gamerule_value` treats as authoritative.
fn anvil_gamerules_nbt() -> AnvilNbt {
    let rules = command_gamerules().lock().unwrap_or_else(|e| e.into_inner());
    anvil_obj(
        &COMMAND_GAMERULE_DEFAULTS
            .iter()
            .map(|&(name, is_bool, _)| {
                (
                    name,
                    anvil_s(&anvil_gamerule_string(
                        is_bool,
                        gamerule_value(name, &rules),
                    )),
                )
            })
            .collect::<Vec<_>>(),
    )
}

/// Apply a parsed `GameRules` compound: known rules update both the command
/// registry and the eight live atomics; unknown names are ignored.
fn anvil_apply_gamerules(rules: &std::collections::BTreeMap<String, String>) {
    let mut map = command_gamerules().lock().unwrap_or_else(|e| e.into_inner());
    for &(name, is_bool, _) in COMMAND_GAMERULE_DEFAULTS {
        let Some(text) = rules.get(name) else {
            continue;
        };
        let value = if is_bool {
            match text.as_str() {
                "true" => 1,
                "false" => 0,
                _ => continue,
            }
        } else {
            match text.parse::<i32>() {
                Ok(v) => v,
                Err(_) => continue,
            }
        };
        map.insert(name, value);
        gamerule_atomic_store(name, value);
    }
}

/// The `WorldGenSettings.dimensions` entries for the three vanilla dims. `type`
/// is a dimension-type file reference resolved against the always-on vanilla
/// datapack; `generator` names the vanilla noise-settings/preset files the same
/// way, so the written level.dat is loadable without any datapack in the world.
fn anvil_level_dimensions() -> AnvilNbt {
    let noise = |settings: &str, biome_source: AnvilNbt| {
        anvil_obj(&[
            ("type", anvil_s("minecraft:noise")),
            ("settings", anvil_s(settings)),
            ("biome_source", biome_source),
        ])
    };
    let multi_noise = |preset: &str| {
        anvil_obj(&[
            ("type", anvil_s("minecraft:multi_noise")),
            ("preset", anvil_s(preset)),
        ])
    };
    anvil_obj(&[
        (
            "minecraft:overworld",
            anvil_obj(&[
                ("type", anvil_s("minecraft:overworld")),
                ("generator", noise("minecraft:overworld", multi_noise("minecraft:overworld"))),
            ]),
        ),
        (
            "minecraft:the_nether",
            anvil_obj(&[
                ("type", anvil_s("minecraft:the_nether")),
                ("generator", noise("minecraft:nether", multi_noise("minecraft:nether"))),
            ]),
        ),
        (
            "minecraft:the_end",
            anvil_obj(&[
                ("type", anvil_s("minecraft:the_end")),
                (
                    "generator",
                    noise(
                        "minecraft:end",
                        anvil_obj(&[
                            ("type", anvil_s("minecraft:the_end")),
                            ("seed", AnvilNbt::Long(0)),
                        ]),
                    ),
                ),
            ]),
        ),
    ])
}

/// Snapshot the live world state into a `Data` compound: identity fields, spawn,
/// time, difficulty, weather, border, gamemode, and all 51 gamerules. The
/// `DataVersion` stamp follows `data_version`, so `Version.Name` reports the
/// release that version names.
fn anvil_level_dat_from_state(level_name: &str, data_version: i32) -> AnvilLevelDat {
    let schema = AnvilSchemaVersion::from_data_version(data_version);
    let spawn = *WORLD_SPAWN.lock().unwrap_or_else(|e| e.into_inner());
    let now_ms = border_now_ms();
    let border = *world_border().lock().unwrap_or_else(|e| e.into_inner());
    let raining = RAINING.load(Ordering::Relaxed);
    let thundering = THUNDERING.load(Ordering::Relaxed);
    let seed = WORLD_SEED.load(Ordering::Relaxed) as i64;
    let mut level = AnvilLevelDat {
        data: anvil_obj(&[]),
    };
    let mut put = |key: &str, value: AnvilNbt| level.set(key, value);
    put("DataVersion", AnvilNbt::Int(schema.data_version()));
    put("version", AnvilNbt::Int(ANVIL_LEVEL_VERSION));
    put(
        "Version",
        anvil_obj(&[
            ("Id", AnvilNbt::Int(schema.data_version())),
            ("Name", anvil_s(schema.release_name())),
            ("Series", anvil_s("main")),
            ("Snapshot", AnvilNbt::Byte(0)),
        ]),
    );
    put("LevelName", anvil_s(level_name));
    put(
        "WorldGenSettings",
        anvil_obj(&[
            ("seed", AnvilNbt::Long(seed)),
            ("generate_features", AnvilNbt::Byte(1)),
            ("bonus_chest", AnvilNbt::Byte(0)),
            ("dimensions", anvil_level_dimensions()),
        ]),
    );
    put("SpawnX", AnvilNbt::Int(spawn.0.floor() as i32));
    put("SpawnY", AnvilNbt::Int(spawn.1.floor() as i32));
    put("SpawnZ", AnvilNbt::Int(spawn.2.floor() as i32));
    put("SpawnAngle", AnvilNbt::Float(spawn.3.to_bits()));
    let time = WORLD_TIME.load(Ordering::Relaxed) as i64;
    put("Time", AnvilNbt::Long(time));
    put("DayTime", AnvilNbt::Long(time));
    put(
        "Difficulty",
        AnvilNbt::Byte(DIFFICULTY.load(Ordering::Relaxed) as i8),
    );
    put("DifficultyLocked", AnvilNbt::Byte(0));
    put("GameType", AnvilNbt::Int(i32::from(DEFAULT_GAME_MODE.load(Ordering::Relaxed))));
    put("allowCommands", AnvilNbt::Byte(1));
    put("hardcore", AnvilNbt::Byte(0));
    put("raining", AnvilNbt::Byte((raining > 0) as i8));
    put("rainTime", AnvilNbt::Int(raining.min(i32::MAX as u64) as i32));
    put("thundering", AnvilNbt::Byte((thundering > 0) as i8));
    put(
        "thunderTime",
        AnvilNbt::Int(thundering.min(i32::MAX as u64) as i32),
    );
    put("BorderCenterX", AnvilNbt::Double(border.center_x.to_bits()));
    put("BorderCenterZ", AnvilNbt::Double(border.center_z.to_bits()));
    put(
        "BorderSize",
        AnvilNbt::Double(border.current_size(now_ms).to_bits()),
    );
    put(
        "BorderSizeLerpTarget",
        AnvilNbt::Double(border.new_size.to_bits()),
    );
    put(
        "BorderSizeLerpTime",
        AnvilNbt::Long(border.lerp_remaining_ms(now_ms).max(0)),
    );
    put(
        "BorderWarningBlocks",
        AnvilNbt::Double(f64::from(border.warning_blocks).to_bits()),
    );
    put(
        "BorderWarningTime",
        AnvilNbt::Double(f64::from(border.warning_time_secs).to_bits()),
    );
    put(
        "BorderDamagePerBlock",
        AnvilNbt::Double(border.damage_per_block.to_bits()),
    );
    put(
        "BorderSafeZone",
        AnvilNbt::Double(border.damage_safe_zone.to_bits()),
    );
    put("GameRules", anvil_gamerules_nbt());
    level
}

/// Apply a parsed `Data` compound to the live world state.
fn anvil_apply_level_dat(level: &AnvilLevelDat) {
    WORLD_SEED.store(level.seed() as u64, Ordering::Relaxed);
    let (sx, sy, sz, angle) = level.spawn();
    *WORLD_SPAWN.lock().unwrap_or_else(|e| e.into_inner()) = (
        f64::from(sx) + 0.5,
        f64::from(sy),
        f64::from(sz) + 0.5,
        angle,
    );
    let day = level.day_time().max(0) as u64;
    WORLD_TIME.store(if day == 0 { level.time().max(0) as u64 } else { day }, Ordering::Relaxed);
    DIFFICULTY.store(level.difficulty().clamp(0, 3) as u8, Ordering::Relaxed);
    let (raining, rain_time, thundering, thunder_time) = level.weather();
    RAINING.store(if raining { rain_time.max(0) as u64 } else { 0 }, Ordering::Relaxed);
    THUNDERING.store(
        if thundering { thunder_time.max(0) as u64 } else { 0 },
        Ordering::Relaxed,
    );
    let (cx, cz, size, warn_blocks, warn_time, damage, safe) = level.border();
    {
        let mut border = world_border().lock().unwrap_or_else(|e| e.into_inner());
        border.center_x = cx;
        border.center_z = cz;
        border.old_size = size.clamp(1.0, WORLD_BORDER_MAX_SIZE);
        border.new_size = border.old_size;
        border.lerp_start_ms = 0;
        border.lerp_duration_ms = 0;
        border.damage_per_block = damage;
        border.damage_safe_zone = safe;
        border.warning_blocks = warn_blocks as i32;
        border.warning_time_secs = warn_time as i32;
    }
    let rules = level.game_rules();
    if !rules.is_empty() {
        anvil_apply_gamerules(&rules);
    }
}

// ---------- world import / export ----------

/// One chunk's parsed contents, ready to apply: `edits` is in `world()`'s
/// `(idx -> state)` shape; block entities stay as raw compounds.
#[derive(Clone, Debug)]
struct AnvilImportedChunk {
    dim: DimId,
    cx: i32,
    cz: i32,
    edits: HashMap<u32, u32>,
    block_entities: Vec<AnvilNbt>,
    /// Palette entries that resolved to no known state — reported, not applied.
    #[cfg_attr(not(test), allow(dead_code))]
    unknown_states: usize,
    /// Section entries dropped for carrying no `Y` index at all.
    #[cfg_attr(not(test), allow(dead_code))]
    dropped_sections: usize,
}

/// What `import_anvil_world` parsed out of a vanilla world directory.
#[derive(Clone, Debug, Default)]
struct AnvilImportReport {
    level: Option<AnvilLevelDat>,
    chunks: Vec<AnvilImportedChunk>,
    region_files: usize,
    /// Region files that read but had chunks which did not decode.
    chunk_errors: Vec<String>,
    /// Observed chunk `DataVersion` -> decoded chunk count. The multi-version
    /// intake diagnostic: which releases a directory actually contains.
    data_version_counts: std::collections::BTreeMap<i32, usize>,
    /// Chunks whose `DataVersion` classified outside `Native`/`Compatible` —
    /// corrupt (missing) values land here beside unlisted future releases.
    unsupported_chunks: usize,
    /// Section entries dropped for carrying no `Y` index at all.
    dropped_sections: usize,
}

#[derive(Clone, Debug, Default)]
struct AnvilExportReport {
    level_written: bool,
    region_files: usize,
    chunks_written: usize,
}

/// Every `.mca` in `dir`, parsed and decoded. Region files are read in sorted
/// order so the report is deterministic.
fn anvil_import_region_dir(
    dir: &std::path::Path,
    dim: DimId,
    report: &mut AnvilImportReport,
) -> Result<(), String> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Ok(()); // no region dir: that dimension simply has no save data
    };
    let mut files: Vec<std::path::PathBuf> = entries
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|e| e == "mca"))
        .collect();
    files.sort();
    for file in files {
        // Tolerant read: one corrupt slot degrades to a chunk error and its
        // valid neighbors still import — a single torn record must not take a
        // whole 1024-chunk file's good data down with it. The merge-write
        // paths keep the strict reader for the opposite reason.
        let (region, slot_errors) = read_region_file_tolerant(&file).map_err(|e| {
            format!("{}: {e}", file.display())
        })?;
        report
            .chunk_errors
            .extend(slot_errors.iter().map(|e| format!("{}: {e}", file.display())));
        report.region_files += 1;
        for (&slot, record) in &region.chunks {
            match anvil_chunk_from_nbt(&record.nbt, slot) {
                Ok(chunk) => {
                    *report
                        .data_version_counts
                        .entry(chunk.data_version)
                        .or_insert(0) += 1;
                    if !matches!(
                        classify_anvil_data_version(chunk.data_version),
                        AnvilVersionSupport::Native | AnvilVersionSupport::Compatible
                    ) {
                        report.unsupported_chunks += 1;
                    }
                    report.dropped_sections += chunk.dropped_sections;
                    report.chunks.push(AnvilImportedChunk {
                        dim,
                        cx: chunk.cx,
                        cz: chunk.cz,
                        edits: anvil_chunk_edits(&chunk, dim),
                        block_entities: chunk.block_entities.clone(),
                        unknown_states: chunk.unknown_states,
                        dropped_sections: chunk.dropped_sections,
                    });
                }
                Err(e) => report
                    .chunk_errors
                    .push(format!("{} ({},{}): {e}", file.display(), slot.0, slot.1)),
            }
        }
    }
    Ok(())
}

/// Read `level.dat` + `region/*.mca` (+ `DIM-1/region`, `DIM1/region`) from a
/// vanilla world directory. The report carries the parsed level.dat and every
/// chunk as overlay edits; nothing is applied yet — see `apply_anvil_import`.
#[allow(dead_code)] // The console import command is a later phase.
fn import_anvil_world(vanilla_world_dir: &std::path::Path) -> Result<AnvilImportReport, String> {
    let mut report = AnvilImportReport::default();
    let level_path = vanilla_world_dir.join("level.dat");
    if level_path.is_file() {
        report.level = Some(read_level_dat_path(&level_path)?);
    }
    anvil_import_region_dir(&vanilla_world_dir.join("region"), DimId::Overworld, &mut report)?;
    anvil_import_region_dir(
        &vanilla_world_dir.join("DIM-1").join("region"),
        DimId::Nether,
        &mut report,
    )?;
    anvil_import_region_dir(
        &vanilla_world_dir.join("DIM1").join("region"),
        DimId::End,
        &mut report,
    )?;
    Ok(report)
}

/// Block entities an import read but no live registry owns. Export re-emits
/// them at their recorded positions, so an import -> export cycle keeps them.
static ANVIL_BLOCK_ENTITIES: Registry<DimBlockPos, AnvilNbt> = OnceLock::new();
fn anvil_block_entities() -> &'static LockedMap<DimBlockPos, AnvilNbt> {
    ANVIL_BLOCK_ENTITIES.get_or_init(|| Mutex::new(HashMap::new()))
}

/// The sign entity's `id` for the block at a position (hanging signs share the
/// front/back text schema but carry their own id).
fn anvil_sign_id(dim: DimId, x: i32, y: i32, z: i32) -> &'static str {
    let hanging = breg().is_some_and(|reg| {
        reg.name_of(block_at_dim(dim, x, y, z)).ends_with("hanging_sign")
    });
    if hanging { "minecraft:hanging_sign" } else { "minecraft:sign" }
}

/// `SignText`-shaped compound for one side of a sign, matching the shape
/// `sign_side_nbt` writes for the wire: color, glow flag, both message lists,
/// each message a JSON string component.
fn anvil_sign_side(lines: &[String; 4], filled: bool) -> AnvilNbt {
    let to_list = |values: Vec<String>| {
        AnvilNbt::List(
            8,
            values.iter().map(|v| anvil_s(v)).collect(),
        )
    };
    anvil_obj(&[
        ("color", anvil_s("white")),
        ("has_glowing_text", AnvilNbt::Byte(0)),
        (
            "filtered_messages",
            to_list(vec![String::new(), String::new(), String::new(), String::new()]),
        ),
        (
            "messages",
            to_list(if filled {
                lines
                    .iter()
                    .map(|l| {
                        let mut s = String::from("\"");
                        for c in l.chars() {
                            match c {
                                '"' => s.push_str("\\\""),
                                '\\' => s.push_str("\\\\"),
                                c if (c as u32) < 0x20 => {
                                    s.push_str(&format!("\\u{:04x}", c as u32))
                                }
                                c => s.push(c),
                            }
                        }
                        s.push('"');
                        s
                    })
                    .collect()
            } else {
                vec![String::new(), String::new(), String::new(), String::new()]
            }),
        ),
    ])
}

/// The four raw text lines on one side of a stored sign entity, best-effort:
/// vanilla stores each message as a JSON string component, so a quoted payload
/// unescapes; anything else is taken verbatim.
fn anvil_sign_lines(side: Option<&AnvilNbt>) -> SignLines {
    let mut lines: SignLines = [String::new(), String::new(), String::new(), String::new()];
    let Some(AnvilNbt::List(_, messages)) = side.and_then(|s| s.get("messages"))
    else {
        return lines;
    };
    for (i, m) in messages.iter().take(4).enumerate() {
        if let Some(raw) = anvil_str_of(m) {
            lines[i] = match crate::json::jparse(raw.as_bytes()) {
                Some(crate::json::J::Str(s)) => s,
                _ => raw,
            };
        }
    }
    lines
}

/// Apply a parsed report: level.dat fields to their statics, chunk blocks into
/// `world()`, block entities into `ANVIL_BLOCK_ENTITIES` (and sign text into
/// `signs()` where the entity parses).
#[allow(dead_code)] // The console import command is a later phase.
fn apply_anvil_import(report: &AnvilImportReport) {
    if let Some(level) = &report.level {
        anvil_apply_level_dat(level);
    }
    {
        let mut w = world().lock().unwrap_or_else(|e| e.into_inner());
        for chunk in &report.chunks {
            w.entry((chunk.dim, chunk.cx, chunk.cz))
                .or_default()
                .extend(chunk.edits.iter().map(|(&k, &v)| (k, v)));
        }
    }
    {
        let mut entities = anvil_block_entities().lock().unwrap_or_else(|e| e.into_inner());
        let mut sign_map = signs().lock().unwrap_or_else(|e| e.into_inner());
        for chunk in &report.chunks {
            for entity in &chunk.block_entities {
                let (Some(x), Some(y), Some(z)) = (
                    anvil_i32(entity, "x"),
                    anvil_i32(entity, "y"),
                    anvil_i32(entity, "z"),
                ) else {
                    continue;
                };
                entities.insert((chunk.dim, x, y, z), entity.clone());
                if anvil_str(entity, "id").is_some_and(|id| {
                    id.ends_with(":sign") || id.ends_with(":hanging_sign")
                }) {
                    let front = anvil_sign_lines(entity.get("front_text"));
                    if front.iter().any(|l| !l.is_empty()) {
                        sign_map.insert((chunk.dim, x, y, z, 1), front);
                    }
                    let back = anvil_sign_lines(entity.get("back_text"));
                    if back.iter().any(|l| !l.is_empty()) {
                        sign_map.insert((chunk.dim, x, y, z, 0), back);
                    }
                }
            }
        }
    }
}

/// The region subdirectory a dimension exports to, relative to the world root.
fn anvil_dim_region_dir(dim: DimId) -> Option<&'static str> {
    match dim {
        DimId::Overworld => Some("region"),
        DimId::Nether => Some("DIM-1/region"),
        DimId::End => Some("DIM1/region"),
        _ => None, // custom dimensions have no vanilla directory shape
    }
}

/// One chunk's sections as generated terrain plus its overlay edits. With no
/// generator installed (tests, `MEGU_WG=old`, custom dims) the base is air;
/// the overlay still lands on top.
fn anvil_export_sections(
    dim: DimId,
    cx: i32,
    cz: i32,
    overlay: &HashMap<u32, u32>,
) -> Vec<AnvilSection> {
    let min_y = dim.min_y();
    let mut sections = Vec::new();
    for sec in 0..dim.sections() {
        let sy = min_y + sec * 16;
        let mut cells = Box::new([0u32; 4096]);
        if let Some(base) = worldgen::terrain_section_dim(dim, cx, sy, cz, crate::feature::populate)
        {
            cells = base;
        }
        for ly in 0..16i32 {
            for lz in 0..16i32 {
                for lx in 0..16i32 {
                    let idx = local_idx(cx * 16 + lx, sy + ly, cz * 16 + lz);
                    if let Some(&st) = overlay.get(&idx) {
                        cells[((ly << 8) | (lz << 4) | lx) as usize] = st;
                    }
                }
            }
        }
        sections.push(AnvilSection {
            y: (sy >> 4) as i8,
            blocks: cells,
            biomes: anvil_export_biomes(dim, cx, sy, cz),
            block_light: None,
            sky_light: None,
        });
    }
    sections
}

/// The 4x4x4 biome names for one section through the dimension's own generator
/// (`biome_quart` + `biome_name_of_leaf`), or a per-dimension default when no
/// generator is installed.
fn anvil_export_biomes(dim: DimId, cx: i32, sy: i32, cz: i32) -> Option<Box<[String; 64]>> {
    let fallback = match dim {
        DimId::Overworld => "minecraft:plains",
        DimId::Nether => "minecraft:nether_wastes",
        _ => "minecraft:the_end",
    };
    let names = worldgen::with_gen_dim(dim, |g| {
        let mut out = Box::new(std::array::from_fn::<String, 64, _>(|_| String::new()));
        let (qx0, qy0, qz0) = (cx * 4, sy >> 2, cz * 4);
        let mut last = None;
        for ly in 0..4 {
            for lz in 0..4 {
                for lx in 0..4 {
                    let name = g
                        .biome_quart(qx0 + lx, qy0 + ly, qz0 + lz, last)
                        .and_then(|(_, leaf)| {
                            last = Some(leaf);
                            g.biome_name_of_leaf(leaf)
                        })
                        .unwrap_or_else(|| fallback.to_string());
                    out[((ly << 4) | (lz << 2) | lx) as usize] = name;
                }
            }
        }
        out
    });
    Some(names.unwrap_or_else(|| Box::new(std::array::from_fn(|_| fallback.to_string()))))
}

/// The `block_entities` for one exported chunk: imported entities that no live
/// registry owns, the sign text registries, and generated spawners. Position
/// dedup prefers the live registry over an imported record.
fn anvil_export_block_entities(
    dim: DimId,
    cx: i32,
    cz: i32,
) -> Vec<AnvilNbt> {
    let in_chunk = |x: i32, z: i32| x >> 4 == cx && z >> 4 == cz;
    let mut by_pos: std::collections::BTreeMap<(i32, i32, i32), AnvilNbt> =
        Default::default();
    for (&(d, x, y, z), tag) in anvil_block_entities().lock().unwrap_or_else(|e| e.into_inner()).iter() {
        if d == dim && in_chunk(x, z) {
            by_pos.insert((x, y, z), tag.clone());
        }
    }
    // Spawners are Overworld-only generated content; their stored tag is
    // already the full entity compound including x/y/z.
    if dim == DimId::Overworld {
        for (&(x, y, z), tag) in generated_content().lock().unwrap_or_else(|e| e.into_inner()).spawners.iter() {
            if in_chunk(x, z) {
                by_pos.insert((x, y, z), tag.clone());
            }
        }
    }
    for ((sdim, x, y, z, front), lines) in signs_in_chunk(dim, cx, cz) {
        debug_assert_eq!(sdim, dim);
        let empty = || [String::new(), String::new(), String::new(), String::new()];
        let (fl, bl) = if front != 0 { (lines, empty()) } else { (empty(), lines) };
        by_pos.insert(
            (x, y, z),
            anvil_obj(&[
                ("id", anvil_s(anvil_sign_id(dim, x, y, z))),
                ("x", AnvilNbt::Int(x)),
                ("y", AnvilNbt::Int(y)),
                ("z", AnvilNbt::Int(z)),
                ("front_text", anvil_sign_side(&fl, true)),
                ("back_text", anvil_sign_side(&bl, true)),
                ("is_waxed", AnvilNbt::Byte(0)),
            ]),
        );
    }
    by_pos.into_values().collect()
}

/// Export-time knobs: which `DataVersion` the written world claims. Everything
/// else (section layout, palettes, region-file shape) stays the codec's own —
/// the option stamps a target version onto the schema this codec writes.
#[derive(Clone, Copy, Debug)]
struct AnvilExportOptions {
    /// The `DataVersion` integer stamped into `level.dat` and every chunk.
    target_data_version: i32,
}

impl Default for AnvilExportOptions {
    fn default() -> AnvilExportOptions {
        AnvilExportOptions {
            target_data_version: ANVIL_DATA_VERSION,
        }
    }
}

/// Write a complete vanilla 1.21.1 world directory for the live world state:
/// `level.dat`, then `region/r.<rx>.<rz>.mca` per dimension that has overlay
/// chunks (`DIM-1`/`DIM1` only when those dimensions have edits).
#[allow(dead_code)] // The console export command is a later phase.
fn export_anvil_world(out_dir: &std::path::Path) -> Result<AnvilExportReport, String> {
    export_anvil_world_with_options(out_dir, &AnvilExportOptions::default())
}

/// `export_anvil_world` with an explicit `DataVersion` target, so a future
/// release can be written by re-stamping rather than by forking the codec.
#[allow(dead_code)] // The console export command is a later phase.
fn export_anvil_world_with_options(
    out_dir: &std::path::Path,
    options: &AnvilExportOptions,
) -> Result<AnvilExportReport, String> {
    let mut report = AnvilExportReport::default();
    std::fs::create_dir_all(out_dir)
        .map_err(|e| format!("cannot create {}: {e}", out_dir.display()))?;
    let level_name = out_dir
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("world");
    write_level_dat_path(
        &out_dir.join("level.dat"),
        &anvil_level_dat_from_state(level_name, options.target_data_version),
    )?;
    report.level_written = true;
    let timestamp = (border_now_ms() / 1000).min(u64::from(u32::MAX)) as u32;
    // Snapshot the overlay once: no `world()` borrow crosses a worldgen call.
    let snapshot: HashMap<DimChunkKey, HashMap<u32, u32>> =
        world().lock().unwrap_or_else(|e| e.into_inner()).clone();
    let mut by_region: std::collections::BTreeMap<(DimId, i32, i32), Vec<(i32, i32)>> =
        Default::default();
    for &(dim, cx, cz) in snapshot.keys() {
        if anvil_dim_region_dir(dim).is_some() {
            by_region
                .entry((dim, cx.div_euclid(32), cz.div_euclid(32)))
                .or_default()
                .push((cx, cz));
        }
    }
    for ((dim, rx, rz), chunks) in by_region {
        let rel = anvil_dim_region_dir(dim).expect("vanilla dims only");
        let region_dir = out_dir.join(rel);
        std::fs::create_dir_all(&region_dir)
            .map_err(|e| format!("cannot create {}: {e}", region_dir.display()))?;
        let mut region = AnvilRegion::default();
        for (cx, cz) in chunks {
            let overlay = snapshot
                .get(&(dim, cx, cz))
                .cloned()
                .unwrap_or_default();
            let chunk = AnvilChunk {
                cx,
                cz,
                data_version: options.target_data_version,
                status: "minecraft:full".to_string(),
                sections: anvil_export_sections(dim, cx, cz, &overlay),
                block_entities: anvil_export_block_entities(dim, cx, cz),
                unknown_states: 0,
                dropped_sections: 0,
            };
            region.chunks.insert(
                (cx, cz),
                AnvilChunkRecord {
                    timestamp,
                    compression: 2,
                    nbt: anvil_chunk_to_nbt(&chunk, dim.min_y() >> 4),
                },
            );
            report.chunks_written += 1;
        }
        write_region_file(&region_dir.join(format!("r.{rx}.{rz}.mca")), &region)?;
        report.region_files += 1;
    }
    Ok(report)
}

// ---------- live persistence bridge ----------
//
// `--import-anvil`/`--export-anvil` are one-shot helpers; this bridge aims the
// same codec at a directory the running server owns. `live_anvil_load_dir`
// opens it at startup (or stamps a fresh world into an empty one) and
// `live_anvil_save_dir` is the autosave target: `level.dat` rotates to
// `level.dat_old` exactly as vanilla rotates it, every `.mca` write is staged
// in its own directory and renamed over the live file so a crash never leaves
// a half-written region, and region files merge with the on-disk contents so
// chunks with no edits are never rewritten — or lost.

/// Whether native Anvil storage is armed for this run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[allow(dead_code)] // Consumed by the runtime startup/save hooks.
enum AnvilStorageMode {
    /// No `MEGU_ANVIL_WORLD_DIR`: saves stay on the v2 overlay files.
    Disabled,
    /// A world directory is configured: load it at startup, flush to it on
    /// every save.
    Live,
}

/// `MEGU_ANVIL_WORLD_DIR`: when set to a non-empty path the runtime treats
/// it as the live world store — loaded at startup, flushed on every autosave.
#[allow(dead_code)]
fn anvil_world_dir_from_env() -> Option<std::path::PathBuf> {
    let raw = std::env::var("MEGU_ANVIL_WORLD_DIR").ok()?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(std::path::PathBuf::from(trimmed))
    }
}

/// The storage mode the environment selects.
#[allow(dead_code)]
fn anvil_storage_mode() -> AnvilStorageMode {
    if anvil_world_dir_from_env().is_some() {
        AnvilStorageMode::Live
    } else {
        AnvilStorageMode::Disabled
    }
}

/// What `live_anvil_load_dir` did with a world directory.
#[derive(Clone, Debug, Default)]
#[allow(dead_code)]
struct AnvilLoadSummary {
    /// The directory held no Anvil data: a fresh `level.dat` was stamped into
    /// it so even a crash before the first autosave leaves a valid world.
    initialized: bool,
    /// `level.dat` existed and parsed.
    level_loaded: bool,
    region_files: usize,
    chunks_loaded: usize,
    /// Chunks that failed to decode, in words — the rest still loaded.
    chunk_errors: Vec<String>,
    /// Chunks whose `DataVersion` sits outside the readable schema range.
    unsupported_chunks: usize,
    /// Section entries dropped for carrying no `Y` index at all.
    dropped_sections: usize,
    /// `playerdata/*.dat` records restored into `saved_players`.
    players_loaded: usize,
}

/// What `live_anvil_save_dir` wrote.
#[derive(Clone, Debug, Default)]
#[allow(dead_code)]
struct AnvilSaveSummary {
    level_written: bool,
    /// A prior `level.dat` was rotated to `level.dat_old` before the new one
    /// took the live name — vanilla's crash-recovery backup.
    backup_rotated: bool,
    region_files: usize,
    chunks_written: usize,
    /// Dirty chunks and playerdata records whose serialized payload matched
    /// `anvil_persisted_hashes`/`anvil_playerdata_hashes` exactly: no zlib
    /// re-encode, no sector rewrite, no `.tmp` staging — the whole point of
    /// the content-hash skip.
    skipped_unchanged_writes: usize,
    /// Total `.mca` bytes committed through `anvil_atomic_write` this save.
    region_bytes_written: u64,
    /// `playerdata/<uuid>.dat` files written under the world directory.
    players_written: usize,
}

/// Does `dir` hold anything shaped like an Anvil world — a `level.dat` or at
/// least one `.mca` in any of the three vanilla region directories.
fn anvil_dir_has_data(dir: &std::path::Path) -> bool {
    if dir.join("level.dat").is_file() {
        return true;
    }
    ["region", "DIM-1/region", "DIM1/region"].iter().any(|rel| {
        std::fs::read_dir(dir.join(rel)).is_ok_and(|mut entries| {
            entries.any(|e| {
                e.is_ok_and(|e| e.path().extension().is_some_and(|x| x == "mca"))
            })
        })
    })
}

/// `level.dat` with vanilla's own rotation: the new contents are staged as
/// `level.dat_new` (fsynced), any existing `level.dat` becomes `level.dat_old`,
/// then the staged file takes the live name. A crash at any step leaves a
/// complete file under one of the three names; a failed final rename tries to
/// put the previous file back. Returns whether a backup was rotated.
#[allow(dead_code)]
fn write_level_dat_rotating(
    path: &std::path::Path,
    level: &AnvilLevelDat,
) -> Result<bool, String> {
    let bytes = write_level_dat(level)?;
    let mut new_name = path.as_os_str().to_owned();
    new_name.push("_new");
    let staged = std::path::PathBuf::from(new_name);
    {
        let mut f = std::fs::File::create(&staged)
            .map_err(|e| format!("cannot create {}: {e}", staged.display()))?;
        if let Err(e) = f.write_all(&bytes).and_then(|_| f.sync_all()) {
            drop(f);
            let _ = std::fs::remove_file(&staged);
            return Err(format!("cannot write {}: {e}", staged.display()));
        }
    }
    let mut old_name = path.as_os_str().to_owned();
    old_name.push("_old");
    let backup = std::path::PathBuf::from(old_name);
    let mut rotated = false;
    if path.exists() {
        std::fs::rename(path, &backup)
            .map_err(|e| {
                let _ = std::fs::remove_file(&staged);
                format!(
                    "cannot rotate {} to {}: {e}",
                    path.display(),
                    backup.display()
                )
            })?;
        rotated = true;
    }
    match std::fs::rename(&staged, path) {
        Ok(()) => {
            anvil_sync_parent_dir(path);
            Ok(rotated)
        }
        Err(e) => {
            let _ = std::fs::remove_file(&staged);
            // The rotation already moved the old file aside; put it back so a
            // failed save leaves the world it found rather than none at all.
            if rotated {
                let _ = std::fs::rename(&backup, path);
            }
            Err(format!(
                "cannot rename {} over {}: {e}",
                staged.display(),
                path.display()
            ))
        }
    }
}

// ---------- playerdata/<uuid>.dat (vanilla per-player NBT) ----------
//
// Vanilla keeps one gzipped NBT file per player under
// `<world>/playerdata/<dashed-uuid>.dat`. These records round-trip a
// `PlayerSave` verbatim: `Dimension` + `Pos`/`Rotation`, `Health`,
// `AbsorptionAmount`, `foodLevel`/`foodSaturationLevel`/`foodExhaustionLevel`,
// `XpLevel`/`XpP`/`XpTotal`, `playerGameType`, `SelectedItemSlot`,
// `Inventory`/`EnderItems`, `ActiveEffects`, and the `Spawn*` fields, with a
// private `SpawnKind` extension byte so bed/anchor targets survive too.
// `PlayerSave` inventory keys are window-0 slot ids; vanilla `Slot` keys are
// 0..=8 hotbar, 9..=35 main, 100..=103 feet..head and -106 offhand.

/// A window-0 slot `PlayerSave` carries -> the `Slot` key vanilla writes.
fn playerdata_slot_to_vanilla(slot: i16) -> Option<i16> {
    match slot {
        36..=44 => Some(slot - 36),
        9..=35 => Some(slot),
        5..=8 => Some(108 - slot),
        45 => Some(-106),
        _ => None,
    }
}

/// A `Slot` key read back from NBT -> the window-0 slot `PlayerSave` uses.
fn playerdata_slot_from_vanilla(slot: i16) -> Option<i16> {
    match slot {
        0..=8 => Some(slot + 36),
        9..=35 => Some(slot),
        100..=103 => Some(108 - slot),
        -106 => Some(45),
        _ => None,
    }
}

fn playerdata_slots_to_vanilla(slots: &HashMap<i16, Stack>) -> HashMap<i16, Stack> {
    slots
        .iter()
        .filter_map(|(&slot, stack)| {
            playerdata_slot_to_vanilla(slot).map(|key| (key, stack.clone()))
        })
        .collect()
}

fn playerdata_slots_from_vanilla(slots: HashMap<i16, Stack>) -> HashMap<i16, Stack> {
    slots
        .into_iter()
        .filter_map(|(slot, stack)| {
            playerdata_slot_from_vanilla(slot).map(|key| (key, stack))
        })
        .collect()
}

/// The lowercase dashed `<8-4-4-4-12>.dat` name vanilla gives a player file.
fn playerdata_file_name(uuid: &[u8; 16]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut name = String::with_capacity(40);
    for (index, byte) in uuid.iter().enumerate() {
        if matches!(index, 4 | 6 | 8 | 10) {
            name.push('-');
        }
        name.push(HEX[usize::from(byte >> 4)] as char);
        name.push(HEX[usize::from(byte & 0xF)] as char);
    }
    name.push_str(".dat");
    name
}

/// The 16 bytes a `<8-4-4-4-12>` file stem stands for: strict — exactly 36
/// chars, hyphens only in the four canonical slots, hex elsewhere — so a `..`
/// or otherwise odd name never resolves to a path the loader would touch.
fn playerdata_uuid_of(stem: &str) -> Option<[u8; 16]> {
    let b = stem.as_bytes();
    if b.len() != 36 {
        return None;
    }
    let mut uuid = [0u8; 16];
    let mut nibble = true;
    let mut out = 0usize;
    for (index, &c) in b.iter().enumerate() {
        if matches!(index, 8 | 13 | 18 | 23) {
            if c != b'-' {
                return None;
            }
            continue;
        }
        let digit = match c {
            b'0'..=b'9' => c - b'0',
            b'a'..=b'f' => c - b'a' + 10,
            b'A'..=b'F' => c - b'A' + 10,
            _ => return None,
        };
        if nibble {
            uuid[out] = digit << 4;
        } else {
            uuid[out] |= digit;
            out += 1;
        }
        nibble = !nibble;
    }
    Some(uuid)
}

/// `Tag::Float`/`Tag::Double` hold raw bits; this is the same coercion the
/// `anvil_f*` key accessors apply, for list elements.
fn playerdata_f64(t: &AnvilNbt) -> Option<f64> {
    match t {
        AnvilNbt::Double(v) => Some(f64::from_bits(*v)),
        AnvilNbt::Float(v) => Some(f64::from(f32::from_bits(*v))),
        _ => None,
    }
}

/// The compound one `PlayerSave` serializes to — the vanilla schema, with the
/// private `SpawnKind` byte carrying the bed/anchor kind the classic `Spawn*`
/// fields cannot express.
fn playerdata_nbt_of(save: &PlayerSave) -> AnvilNbt {
    let (dim, x, y, z, yaw, pitch) = save.position;
    let fields: Vec<(&str, AnvilNbt)> = vec![
        ("DataVersion", AnvilNbt::Int(ANVIL_DATA_VERSION)),
        ("Dimension", anvil_s(&dim_key(dim))),
        (
            "Pos",
            anvil_list(vec![
                AnvilNbt::Double(x.to_bits()),
                AnvilNbt::Double(y.to_bits()),
                AnvilNbt::Double(z.to_bits()),
            ]),
        ),
        (
            "Rotation",
            anvil_list(vec![
                AnvilNbt::Float(yaw.to_bits()),
                AnvilNbt::Float(pitch.to_bits()),
            ]),
        ),
        ("Health", AnvilNbt::Float(save.health.to_bits())),
        (
            "AbsorptionAmount",
            AnvilNbt::Float(save.absorption.to_bits()),
        ),
        ("foodLevel", AnvilNbt::Int(save.food)),
        (
            "foodSaturationLevel",
            AnvilNbt::Float(save.saturation.to_bits()),
        ),
        (
            "foodExhaustionLevel",
            AnvilNbt::Float((save.exhaustion as f32).to_bits()),
        ),
        ("foodTickTimer", AnvilNbt::Int(0)),
        ("XpLevel", AnvilNbt::Int(save.xp_level)),
        ("XpP", AnvilNbt::Float(save.xp_progress.to_bits())),
        (
            "XpTotal",
            AnvilNbt::Int(save.xp_total.clamp(0, i64::from(i32::MAX)) as i32),
        ),
        ("playerGameType", AnvilNbt::Int(i32::from(save.gamemode))),
        ("previousPlayerGameType", AnvilNbt::Int(-1)),
        ("SelectedItemSlot", AnvilNbt::Int(i32::from(save.held))),
        (
            "Inventory",
            stacks_to_items_nbt(&playerdata_slots_to_vanilla(&save.inventory)),
        ),
        (
            "EnderItems",
            stacks_to_items_nbt(&save.ender_chest),
        ),
    ];
    let mut root = anvil_obj(&fields);
    let effects: Vec<AnvilNbt> = save
        .effects
        .iter()
        .map(|&(id, amp, left)| {
            anvil_obj(&[
                ("Id", AnvilNbt::Byte(id.clamp(0, 255) as i8)),
                ("Amplifier", AnvilNbt::Byte(amp.clamp(0, 127) as i8)),
                ("Duration", AnvilNbt::Int(left)),
                ("Ambient", AnvilNbt::Byte(0)),
                ("ShowParticles", AnvilNbt::Byte(1)),
                ("ShowIcon", AnvilNbt::Byte(1)),
            ])
        })
        .collect();
    if let AnvilNbt::Compound(map) = &mut root {
        map.insert(
            crate::nbt::NbtString::text("ActiveEffects"),
            anvil_list(effects),
        );
        if let Some(point) = save.respawn {
            map.insert(
                crate::nbt::NbtString::text("SpawnDimension"),
                anvil_s(&dim_key(point.dim)),
            );
            map.insert(crate::nbt::NbtString::text("SpawnX"), AnvilNbt::Int(point.position.0 as i32));
            map.insert(crate::nbt::NbtString::text("SpawnY"), AnvilNbt::Int(point.position.1 as i32));
            map.insert(crate::nbt::NbtString::text("SpawnZ"), AnvilNbt::Int(point.position.2 as i32));
            map.insert(
                crate::nbt::NbtString::text("SpawnAngle"),
                AnvilNbt::Float(point.angle.to_bits()),
            );
            map.insert(
                crate::nbt::NbtString::text("SpawnForced"),
                AnvilNbt::Byte(i8::from(point.forced)),
            );
            map.insert(
                crate::nbt::NbtString::text("SpawnKind"),
                AnvilNbt::Byte(match point.kind {
                    SpawnTargetKind::Legacy => 0,
                    SpawnTargetKind::Bed => 1,
                    SpawnTargetKind::Anchor => 2,
                }),
            );
        }
    }
    root
}

fn uuid_to_int_array(uuid: &[u8; 16]) -> Vec<i32> {
    (0..4)
        .map(|i| {
            i32::from_be_bytes([uuid[4 * i], uuid[4 * i + 1], uuid[4 * i + 2], uuid[4 * i + 3]])
        })
        .collect()
}

fn uuid_from_int_array(values: &[i32]) -> Option<[u8; 16]> {
    if values.len() != 4 {
        return None;
    }
    let mut uuid = [0u8; 16];
    for (i, v) in values.iter().enumerate() {
        uuid[4 * i..4 * i + 4].copy_from_slice(&v.to_be_bytes());
    }
    Some(uuid)
}

/// Read one player file's NBT back into a `PlayerSave`. Required fields that
/// are missing or misshapen abort the file; vanilla extras the record does not
// model are ignored.
fn playerdata_from_nbt(tag: &AnvilNbt) -> Result<PlayerSave, String> {
    let AnvilNbt::Compound(_) = tag else {
        return Err("playerdata root is not a compound".to_string());
    };
    let dimension = anvil_str(tag, "Dimension").unwrap_or_else(|| "minecraft:overworld".to_string());
    let dim = DimId::parse_dimension_id(&dimension)
        .ok_or_else(|| format!("playerdata dimension {dimension} is unregistered"))?;
    let pos = tag
        .get("Pos")
        .and_then(AnvilNbt::list)
        .filter(|p| p.len() == 3)
        .and_then(|p| {
            Some([
                playerdata_f64(&p[0])?,
                playerdata_f64(&p[1])?,
                playerdata_f64(&p[2])?,
            ])
        })
        .ok_or("playerdata has no Pos list")?;
    let rotation = tag
        .get("Rotation")
        .and_then(AnvilNbt::list)
        .filter(|r| r.len() == 2)
        .and_then(|r| {
            Some([
                playerdata_f64(&r[0])? as f32,
                playerdata_f64(&r[1])? as f32,
            ])
        })
        .ok_or("playerdata has no Rotation list")?;
    let inventory = tag
        .get("Inventory")
        .map(items_nbt_to_stacks)
        .transpose()
        .map_err(|e| format!("playerdata Inventory: {e}"))?
        .map(playerdata_slots_from_vanilla)
        .unwrap_or_default();
    let ender_chest = tag
        .get("EnderItems")
        .map(items_nbt_to_stacks)
        .transpose()
        .map_err(|e| format!("playerdata EnderItems: {e}"))?
        .unwrap_or_default();
    let held = anvil_i32(tag, "SelectedItemSlot")
        .unwrap_or(0)
        .clamp(0, 8) as i16;
    let gamemode = anvil_i32(tag, "playerGameType")
        .unwrap_or(0)
        .clamp(0, 3) as u8;
    let mut effects = Vec::new();
    if let Some(list) = tag.get("ActiveEffects").and_then(AnvilNbt::list) {
        if list.len() > 64 {
            return Err("playerdata has too many ActiveEffects".to_string());
        }
        for effect in list {
            let id = nbt_int_val(effect.get("Id").unwrap_or(&AnvilNbt::Int(-1)))
                .unwrap_or(-1) as i32;
            if id < 0 {
                continue;
            }
            effects.push((
                id,
                nbt_int_val(effect.get("Amplifier").unwrap_or(&AnvilNbt::Byte(0)))
                    .unwrap_or(0) as i32,
                nbt_int_val(effect.get("Duration").unwrap_or(&AnvilNbt::Int(0)))
                    .unwrap_or(0) as i32,
            ));
        }
    }
    let respawn = match (
        anvil_i32(tag, "SpawnX"),
        anvil_i32(tag, "SpawnY"),
        anvil_i32(tag, "SpawnZ"),
        anvil_str(tag, "SpawnDimension"),
    ) {
        (Some(x), Some(y), Some(z), Some(dim_key_str)) => {
            let Some(sdim) = DimId::parse_dimension_id(&dim_key_str) else {
                return Err(format!("playerdata respawn dimension {dim_key_str} is unregistered"));
            };
            Some(SpawnTarget {
                kind: match anvil_i8(tag, "SpawnKind").unwrap_or(0) {
                    1 => SpawnTargetKind::Bed,
                    2 => SpawnTargetKind::Anchor,
                    _ => SpawnTargetKind::Legacy,
                },
                dim: sdim,
                position: (f64::from(x), f64::from(y), f64::from(z)),
                angle: anvil_f32(tag, "SpawnAngle").unwrap_or(0.0),
                forced: anvil_i8(tag, "SpawnForced").unwrap_or(0) != 0,
            })
        }
        _ => None,
    };
    Ok(PlayerSave {
        position: (dim, pos[0], pos[1], pos[2], rotation[0], rotation[1]),
        inventory,
        ender_chest,
        held,
        health: anvil_f32(tag, "Health").unwrap_or(20.0),
        absorption: anvil_f32(tag, "AbsorptionAmount").unwrap_or(0.0),
        food: anvil_i32(tag, "foodLevel").unwrap_or(20),
        saturation: anvil_f32(tag, "foodSaturationLevel").unwrap_or(5.0),
        exhaustion: f64::from(anvil_f32(tag, "foodExhaustionLevel").unwrap_or(0.0)),
        xp_level: anvil_i32(tag, "XpLevel").unwrap_or(0),
        xp_progress: anvil_f32(tag, "XpP").unwrap_or(0.0),
        xp_total: anvil_i64(tag, "XpTotal").unwrap_or(0),
        gamemode,
        effects,
        respawn,
    })
}

/// The complete playerdata root compound (`PlayerSave` fields + `UUID` int
/// array) — what `playerdata_to_bytes` serializes, and what the content
/// fingerprint skips re-gzipping and rewriting when unchanged.
fn playerdata_nbt_root(uuid: &[u8; 16], save: &PlayerSave) -> AnvilNbt {
    let mut root = playerdata_nbt_of(save);
    if let AnvilNbt::Compound(map) = &mut root {
        map.insert(
            crate::nbt::NbtString::text("UUID"),
            AnvilNbt::IntArray(uuid_to_int_array(uuid)),
        );
    }
    root
}

/// The gzip wrapper every `playerdata/*.dat` carries; same encode level.dat
/// uses.
fn playerdata_to_bytes(uuid: &[u8; 16], save: &PlayerSave) -> Result<Vec<u8>, String> {
    let nbt = crate::nbt::write_named(
        &crate::nbt::NbtString(Vec::new()),
        &playerdata_nbt_root(uuid, save),
    )
    .map_err(|e| format!("cannot encode playerdata: {e}"))?;
    let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    e.write_all(&nbt)
        .and_then(|_| e.finish())
        .map_err(|e| format!("cannot gzip playerdata: {e}"))
}

/// One `.dat`'s decoded record plus the `UUID` int array it stamped, for the
/// file-name cross-check the loader does.
fn playerdata_from_bytes(bytes: &[u8]) -> Result<(Option<[u8; 16]>, PlayerSave), String> {
    let (_, root) = crate::nbt::read_gzip(bytes, ANVIL_LEVEL_NBT_BUDGET)
        .map_err(|e| format!("invalid playerdata NBT: {e}"))?;
    let recorded = match root.get("UUID") {
        Some(AnvilNbt::IntArray(values)) => uuid_from_int_array(values),
        _ => None,
    };
    Ok((recorded, playerdata_from_nbt(&root)?))
}

/// Write one player's `.dat`: staged temp + fsync + atomic rename, the same
/// discipline `write_level_dat_rotating`/`anvil_atomic_write` give `level.dat`.
fn write_playerdata_dat(
    world_dir: &std::path::Path,
    uuid: &[u8; 16],
    save: &PlayerSave,
) -> Result<(), String> {
    let dir = world_dir.join("playerdata");
    if dir.symlink_metadata().is_ok_and(|m| m.file_type().is_symlink()) {
        return Err(format!("{} is a symlink; refusing to write playerdata into it", dir.display()));
    }
    std::fs::create_dir_all(&dir)
        .map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    anvil_atomic_write(
        &dir.join(playerdata_file_name(uuid)),
        &playerdata_to_bytes(uuid, save)?,
    )
}

/// Every live `saved_players` record flushed to `playerdata/`, returning
/// `(written, skipped_unchanged)`. Files left from players who no longer
/// exist stay on disk, as vanilla keeps them. A record whose NBT fingerprint
/// still matches what the last save committed is skipped outright — no gzip
/// re-encode, no staging write — as long as its file is still on disk.
fn write_all_playerdata_dat(world_dir: &std::path::Path) -> Result<(usize, usize), String> {
    let players: Vec<([u8; 16], PlayerSave)> = saved_players()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .map(|(uuid, save)| (*uuid, save.clone()))
        .collect();
    let dir = world_dir.join("playerdata");
    let mut hashes = anvil_playerdata_hashes()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let mut written = 0;
    let mut skipped = 0;
    for (uuid, save) in &players {
        let fresh_hash = anvil_nbt_fingerprint(&playerdata_nbt_root(uuid, save));
        let key = (world_dir.to_path_buf(), *uuid);
        if hashes.get(&key) == Some(&fresh_hash)
            && dir.join(playerdata_file_name(uuid)).is_file()
        {
            skipped += 1;
            continue;
        }
        write_playerdata_dat(world_dir, uuid, save)?;
        hashes.insert(key, fresh_hash);
        written += 1;
    }
    Ok((written, skipped))
}

/// Load every `playerdata/*.dat` into `saved_players`. Symlinked or
/// non-file entries and odd names are skipped with a diagnostic; a corrupt
/// gzip/NBT or structurally invalid record fails the load outright — a
/// half-restored player is worse than no load at all.
fn load_all_playerdata_dat(world_dir: &std::path::Path) -> Result<usize, String> {
    let dir = world_dir.join("playerdata");
    if dir.symlink_metadata().is_ok_and(|m| m.file_type().is_symlink()) {
        return Err(format!("{} is a symlink; refusing to load playerdata from it", dir.display()));
    }
    if !dir.is_dir() {
        return Ok(0);
    }
    let mut loaded = 0;
    let mut entries: Vec<_> = std::fs::read_dir(&dir)
        .map_err(|e| format!("cannot list {}: {e}", dir.display()))?
        .collect::<Result<_, _>>()
        .map_err(|e| format!("cannot list {}: {e}", dir.display()))?;
    entries.sort_by_key(|e| e.file_name());
    for entry in entries {
        let file_type = entry
            .file_type()
            .map_err(|e| format!("cannot stat {}: {e}", entry.path().display()))?;
        if file_type.is_symlink() {
            eprintln!(
                "[spine] {} is a symlink; skipping it",
                entry.path().display()
            );
            continue;
        }
        if !file_type.is_file() {
            continue;
        }
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        let Some(stem) = name.strip_suffix(".dat") else {
            continue;
        };
        let Some(uuid) = playerdata_uuid_of(stem) else {
            eprintln!("[spine] skipping malformed playerdata name {name}");
            continue;
        };
        let bytes = std::fs::read(entry.path())
            .map_err(|e| format!("cannot read {}: {e}", entry.path().display()))?;
        let (recorded, save) = playerdata_from_bytes(&bytes).map_err(|e| format!("{name}: {e}"))?;
        if recorded.is_some_and(|recorded| recorded != uuid) {
            eprintln!("[spine] {name}: NBT UUID does not match file name; keeping the file name");
        }
        saved_players()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(uuid, save);
        loaded += 1;
    }
    Ok(loaded)
}

/// Open `dir` as the live world: an Anvil directory with data is imported and
/// applied exactly as `import_anvil_world` + `apply_anvil_import` do; an empty
/// or missing directory is created and stamped with a fresh `level.dat` so the
/// world is valid before the first autosave ever runs. Its `playerdata/*.dat`
/// records are then folded into `saved_players` — a corrupt file fails the
/// load closed rather than silently dropping a player.
#[allow(dead_code)]
fn live_anvil_load_dir(dir: &std::path::Path) -> Result<AnvilLoadSummary, String> {
    // Fold any interrupted rotation/staging back into a clean tree first: a
    // crash that left `level.dat_new` or a `*.tmp` orphan is repaired before
    // the shape check, so a recovered `level.dat` still counts as world data.
    anvil_recover_world_staging(dir)?;
    if !anvil_dir_has_data(dir) {
        std::fs::create_dir_all(dir)
            .map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
        let level_name = dir
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("world");
        write_level_dat_rotating(
            &dir.join("level.dat"),
            &anvil_level_dat_from_state(level_name, ANVIL_DATA_VERSION),
        )?;
        return Ok(AnvilLoadSummary {
            initialized: true,
            ..AnvilLoadSummary::default()
        });
    }
    let report = import_anvil_world(dir)?;
    let mut summary = AnvilLoadSummary {
        initialized: false,
        level_loaded: report.level.is_some(),
        region_files: report.region_files,
        chunks_loaded: report.chunks.len(),
        chunk_errors: report.chunk_errors.clone(),
        unsupported_chunks: report.unsupported_chunks,
        dropped_sections: report.dropped_sections,
        players_loaded: 0,
    };
    apply_anvil_import(&report);
    summary.players_loaded = load_all_playerdata_dat(dir)?;
    Ok(summary)
}

/// Flush the live overlay into `dir` as a native Anvil world: `level.dat` is
/// rotated (`level.dat` -> `level.dat_old`, vanilla's crash backup) and each
/// dirty chunk is merged into its region file — chunks already on disk with no
/// overlay edits stay byte-for-byte as they were, so a live save never drops
/// imported content the runtime has not touched.
#[allow(dead_code)]
fn live_anvil_save_dir(dir: &std::path::Path) -> Result<AnvilSaveSummary, String> {
    let mut summary = AnvilSaveSummary::default();
    std::fs::create_dir_all(dir)
        .map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    let level_name = dir
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("world");
    summary.backup_rotated = write_level_dat_rotating(
        &dir.join("level.dat"),
        &anvil_level_dat_from_state(level_name, ANVIL_DATA_VERSION),
    )?;
    summary.level_written = true;
    let timestamp = (border_now_ms() / 1000).min(u64::from(u32::MAX)) as u32;
    // Snapshot the overlay once: no `world()` borrow crosses a worldgen call.
    let snapshot: HashMap<DimChunkKey, HashMap<u32, u32>> =
        world().lock().unwrap_or_else(|e| e.into_inner()).clone();
    let mut by_region: std::collections::BTreeMap<(DimId, i32, i32), Vec<(i32, i32)>> =
        Default::default();
    for &(dim, cx, cz) in snapshot.keys() {
        if anvil_dim_region_dir(dim).is_some() {
            by_region
                .entry((dim, cx.div_euclid(32), cz.div_euclid(32)))
                .or_default()
                .push((cx, cz));
        }
    }
    let mut hashes = anvil_persisted_hashes()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    for ((dim, rx, rz), chunks) in by_region {
        let rel = anvil_dim_region_dir(dim).expect("vanilla dims only");
        let region_dir = dir.join(rel);
        std::fs::create_dir_all(&region_dir)
            .map_err(|e| format!("cannot create {}: {e}", region_dir.display()))?;
        let region_path = region_dir.join(format!("r.{rx}.{rz}.mca"));
        // Merge, never blindly overwrite: a region file that exists keeps its
        // untouched chunks; one that fails to read aborts the save outright —
        // a partial file must never replace a good one.
        let mut region = if region_path.is_file() {
            read_region_file(&region_path)?
        } else {
            AnvilRegion::default()
        };
        let mut region_dirty = false;
        let mut committed_hashes: Vec<(AnvilPersistedKey, u64)> = Vec::new();
        for (cx, cz) in chunks {
            let overlay = snapshot
                .get(&(dim, cx, cz))
                .cloned()
                .unwrap_or_default();
            let chunk = AnvilChunk {
                cx,
                cz,
                data_version: ANVIL_DATA_VERSION,
                status: "minecraft:full".to_string(),
                sections: anvil_export_sections(dim, cx, cz, &overlay),
                block_entities: anvil_export_block_entities(dim, cx, cz),
                unknown_states: 0,
                dropped_sections: 0,
            };
            let fresh_nbt = anvil_chunk_to_nbt(&chunk, dim.min_y() >> 4);
            let fresh_hash = anvil_nbt_fingerprint(&fresh_nbt);
            // Dirty-but-identical: the payload on disk already holds exactly
            // this content, so re-encoding and rewriting it would be pure
            // SSD wear. The on-disk record (its timestamp, its compression
            // flavour, every preserved field) stays untouched.
            let key: AnvilPersistedKey = (dir.to_path_buf(), dim, cx, cz);
            if anvil_payload_unchanged(
                key.clone(),
                fresh_hash,
                region.chunks.get(&(cx, cz)).map(|record| &record.nbt),
                &mut hashes,
            ) {
                summary.skipped_unchanged_writes += 1;
                continue;
            }
            region.chunks.insert(
                (cx, cz),
                AnvilChunkRecord {
                    timestamp,
                    compression: 2,
                    nbt: fresh_nbt,
                },
            );
            committed_hashes.push((key, fresh_hash));
            summary.chunks_written += 1;
            region_dirty = true;
        }
        if region_dirty {
            write_region_file(&region_path, &region)?;
            summary.region_files += 1;
            summary.region_bytes_written = summary
                .region_bytes_written
                .saturating_add(anvil_file_len(&region_path));
            // The new payload is only "persisted" once the rename landed.
            for (key, hash) in committed_hashes {
                hashes.insert(key, hash);
            }
        }
    }
    drop(hashes);
    let (players_written, players_skipped) = write_all_playerdata_dat(dir)?;
    summary.players_written = players_written;
    summary.skipped_unchanged_writes += players_skipped;
    // A merged full save persists everything the incremental queue tracked:
    // the queued entries are durable now, so the queue starts the next tick
    // empty rather than re-checking what was just written.
    anvil_incremental_saves()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .drain_pending();
    Ok(summary)
}

/// File length for `region_bytes_written` accounting; `0` when the file is
/// missing (first write) or unreadable (the subsequent error path reports
/// the real problem).
fn anvil_file_len(path: &std::path::Path) -> u64 {
    std::fs::metadata(path).map(|m| m.len()).unwrap_or(0)
}

/// What one incremental autosave tick did: how many dirty chunks were newly
/// enqueued and what the budgeted drain spent.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[allow(dead_code)]
struct AnvilIncrementalSaveSummary {
    newly_queued: usize,
    drain: IncrementalDrainStats,
}

/// One incremental autosave step: every dirty overlay chunk is enqueued
/// (idempotent, oldest-first), then the oldest pending chunks flush through
/// `flush_single_chunk_to_anvil_dir` until the `IncrementalSaveBudget` caps
/// trip. This is the autosave path that replaces the classic every-5-minutes
/// 100-400ms world-save stall with bounded per-tick work; `/save-all` and
/// shutdown keep calling `live_anvil_save_dir`, which drains everything.
#[allow(dead_code)] // The autosave tick hook is wired by the tick lane.
fn live_anvil_save_tick(dir: &std::path::Path) -> Result<AnvilIncrementalSaveSummary, String> {
    let snapshot: HashMap<DimChunkKey, usize> = world()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .filter(|(&(dim, _, _), _)| anvil_dim_region_dir(dim).is_some())
        .map(|(key, edits)| (*key, edits.len()))
        .collect();
    let mut summary = AnvilIncrementalSaveSummary::default();
    {
        let mut queue = anvil_incremental_saves()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        for (&(dim, cx, cz), cells) in &snapshot {
            // Pacing estimate only: ~8 bytes per overlay cell plus one sector
            // of header/NBT slack bounds the tick's byte leg within an order
            // of magnitude of the true payload.
            let est_bytes = cells.saturating_mul(8).saturating_add(4096);
            if queue.mark_dirty(dim, cx, cz, est_bytes) {
                summary.newly_queued += 1;
            }
        }
        summary.drain = queue.drain_budgeted(|item| {
            flush_single_chunk_to_anvil_dir_outcome(dir, item.dim, item.cx, item.cz)
        })?;
    }
    Ok(summary)
}

// ---------- single-chunk flush / reload (live region-scoped unloader) ----------

/// The `{id, x, y, z}` compound fields at `(x, y, z)` in `by_pos`, inserting a
/// fresh record when the position has none yet. Every tag the block-entity
/// writers produce is a compound, so a non-compound here can only be a foreign
/// record and is left untouched rather than silently overwritten.
#[allow(dead_code)]
fn anvil_be_fields<'a>(
    by_pos: &'a mut std::collections::BTreeMap<(i32, i32, i32), AnvilNbt>,
    x: i32,
    y: i32,
    z: i32,
    id: &str,
) -> Option<&'a mut std::collections::BTreeMap<crate::nbt::NbtString, AnvilNbt>> {
    let entity = by_pos.entry((x, y, z)).or_insert_with(|| {
        anvil_obj(&[
            ("id", anvil_s(id)),
            ("x", AnvilNbt::Int(x)),
            ("y", AnvilNbt::Int(y)),
            ("z", AnvilNbt::Int(z)),
        ])
    });
    match entity {
        AnvilNbt::Compound(fields) => Some(fields),
        _ => None,
    }
}

/// The block-entity `id` a live store writes for the block at `(x, y, z)`: the
/// block's own registry name when a store kind owns it (a hopper keeps
/// `minecraft:hopper`, a trapped chest keeps `minecraft:trapped_chest`), else
/// the store's fallback id so the record still re-hydrates to the right store.
#[allow(dead_code)]
fn live_chunk_be_id(dim: DimId, x: i32, y: i32, z: i32, fallback: &str) -> String {
    breg()
        .and_then(|reg| {
            let name = reg.name_of(block_at_dim(dim, x, y, z));
            crate::generation_payload::ContainerKind::for_block(name).map(|_| name.to_string())
        })
        .unwrap_or_else(|| fallback.to_string())
}

/// Every block-entity record a single-chunk flush must carry: what
/// `anvil_export_block_entities` already emits (imported records, generated
/// spawners, sign text) plus the live stores the export pass cannot see — the
/// `live_block_entities` payloads (beacon, lectern, decorated pot, …) and the
/// container stores' `Items` (chests/barrels/hoppers in `chests()`, the
/// three-stack furnace records, the five-stack brewing stands), with a pending
/// `LootTable` merged into the same position's compound. Positions dedup
/// live-over-imported, so a container the runtime has touched wins over the
/// tag an earlier import parked.
#[allow(dead_code)]
fn evict_chunk_block_entities(dim: DimId, cx: i32, cz: i32) -> Vec<AnvilNbt> {
    let in_chunk = |x: i32, z: i32| x >> 4 == cx && z >> 4 == cz;
    let mut by_pos: std::collections::BTreeMap<(i32, i32, i32), AnvilNbt> =
        Default::default();
    for entity in anvil_export_block_entities(dim, cx, cz) {
        if let (Some(x), Some(y), Some(z)) = (
            anvil_i32(&entity, "x"),
            anvil_i32(&entity, "y"),
            anvil_i32(&entity, "z"),
        ) {
            by_pos.insert((x, y, z), entity);
        }
    }
    let store_keys: Vec<DimBlockPos> = live_block_entities()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .entries
        .keys()
        .copied()
        .filter(|&(d, x, _, z)| d == dim && in_chunk(x, z))
        .collect();
    for key @ (_, x, y, z) in store_keys {
        if let Some((_, tag)) = live_block_entity_snapshot(key) {
            by_pos.insert((x, y, z), tag);
        }
    }
    // Positions whose contents already materialized: a `LootTable` must not be
    // written next to live `Items`, or a reload would roll the table over the
    // real contents.
    let mut items_at: HashSet<(i32, i32, i32)> = HashSet::new();
    let chest_entries: Vec<(DimBlockPos, HashMap<i16, Stack>)> = chests()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .filter_map(|(key, slots)| match key {
            ContainerKey::Block(pos) if pos.0 == dim && in_chunk(pos.1, pos.3) => {
                Some((*pos, slots.clone()))
            }
            _ => None,
        })
        .collect();
    for ((_, x, y, z), slots) in chest_entries {
        let id = live_chunk_be_id(dim, x, y, z, "minecraft:chest");
        if let Some(fields) = anvil_be_fields(&mut by_pos, x, y, z, &id) {
            fields.insert(
                crate::nbt::NbtString::text("Items"),
                stacks_to_items_nbt(&slots),
            );
            items_at.insert((x, y, z));
        }
    }
    let furnace_entries: Vec<(DimBlockPos, Furnace)> = furnaces()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .filter(|&(&(d, x, _, z), _)| d == dim && in_chunk(x, z))
        .map(|(key, fr)| (*key, clone_furnace_for_footer(fr)))
        .collect();
    for ((_, x, y, z), fr) in furnace_entries {
        let id = live_chunk_be_id(dim, x, y, z, "minecraft:furnace");
        let Some(fields) = anvil_be_fields(&mut by_pos, x, y, z, &id) else {
            continue;
        };
        let mut slots = HashMap::new();
        for (slot, stack) in [(0i16, &fr.input), (1, &fr.fuel), (2, &fr.output)] {
            if stack.1 > 0 {
                slots.insert(slot, stack.clone());
            }
        }
        fields.insert(
            crate::nbt::NbtString::text("Items"),
            stacks_to_items_nbt(&slots),
        );
        fields.insert(
            crate::nbt::NbtString::text("BurnTime"),
            AnvilNbt::Short(fr.burn_left.clamp(0, i32::from(i16::MAX)) as i16),
        );
        fields.insert(
            crate::nbt::NbtString::text("CookTime"),
            AnvilNbt::Short(fr.progress.clamp(0, i32::from(i16::MAX)) as i16),
        );
        items_at.insert((x, y, z));
    }
    let stand_entries: Vec<(DimBlockPos, HashMap<i16, Stack>, u16, u8)> =
        live_brewing_stands()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .filter(|&(&(d, x, _, z), _)| d == dim && in_chunk(x, z))
            .map(|(key, live)| {
                let slots = live
                    .menu
                    .slots
                    .iter()
                    .enumerate()
                    .filter(|(_, stack)| stack.1 > 0)
                    .map(|(slot, stack)| (slot as i16, stack.clone()))
                    .collect();
                (*key, slots, live.stand.brew_time, live.stand.fuel)
            })
            .collect();
    for ((_, x, y, z), slots, brew_time, fuel) in stand_entries {
        let Some(fields) = anvil_be_fields(&mut by_pos, x, y, z, "minecraft:brewing_stand")
        else {
            continue;
        };
        fields.insert(
            crate::nbt::NbtString::text("Items"),
            stacks_to_items_nbt(&slots),
        );
        fields.insert(
            crate::nbt::NbtString::text("BrewTime"),
            AnvilNbt::Short(i32::from(brew_time).clamp(0, i32::from(i16::MAX)) as i16),
        );
        fields.insert(
            crate::nbt::NbtString::text("Fuel"),
            AnvilNbt::Byte(i8::try_from(fuel).unwrap_or(i8::MAX)),
        );
        items_at.insert((x, y, z));
    }
    let loot_entries: Vec<(DimBlockPos, PendingBlockLoot)> = pending_block_loot()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .filter(|&(&(d, x, _, z), _)| d == dim && in_chunk(x, z))
        .map(|(key, loot)| (*key, loot.clone()))
        .collect();
    for ((_, x, y, z), loot) in loot_entries {
        if items_at.contains(&(x, y, z)) {
            continue;
        }
        let id = live_chunk_be_id(dim, x, y, z, "minecraft:chest");
        let Some(fields) = anvil_be_fields(&mut by_pos, x, y, z, &id) else {
            continue;
        };
        fields.insert(
            crate::nbt::NbtString::text("LootTable"),
            anvil_s(&loot.loot_table),
        );
        if let Some(seed) = loot.loot_seed {
            fields.insert(
                crate::nbt::NbtString::text("LootTableSeed"),
                AnvilNbt::Long(seed),
            );
        }
    }
    by_pos.into_values().collect()
}

/// A live brewing stand rebuilt from its vanilla `Items`/`BrewTime`/`Fuel`
/// record: the menu keeps the raw stacks and the stand state is re-derived
/// from them exactly the way `LiveBrewingStand::tick` derives it each pass.
#[allow(dead_code)]
fn live_brewing_stand_from_nbt(entity: &AnvilNbt, slots: &HashMap<i16, Stack>) -> LiveBrewingStand {
    let mut menu_slots: [Stack; 5] = Default::default();
    for (&slot, stack) in slots {
        if let Ok(index) = usize::try_from(slot) {
            if let Some(target) = menu_slots.get_mut(index) {
                *target = stack.clone();
            }
        }
    }
    let ingredient = item_to_name(menu_slots[3].0)
        .and_then(BrewingIngredient::from_item)
        .filter(|_| menu_slots[3].1 > 0)
        .map(|item| (item, menu_slots[3].1));
    LiveBrewingStand {
        stand: BrewingStand {
            bottles: [
                live_potion_stack(&menu_slots[0]),
                live_potion_stack(&menu_slots[1]),
                live_potion_stack(&menu_slots[2]),
            ],
            ingredient,
            blaze_powder: menu_slots[4].1,
            fuel: entity
                .get("Fuel")
                .and_then(nbt_int_val)
                .unwrap_or(0)
                .clamp(0, i64::from(u8::MAX)) as u8,
            brew_time: entity
                .get("BrewTime")
                .and_then(nbt_int_val)
                .unwrap_or(0)
                .clamp(0, i64::from(u16::MAX)) as u16,
            active_ingredient: None,
        },
        menu: BrewingMenu {
            slots: menu_slots,
        },
    }
}

/// Flush exactly one chunk's live state into `dir`'s vanilla region file: the
/// overlay edits land on the generated base (`anvil_export_sections`) and the
/// merged live block entities ride in `block_entities`, merged into the
/// existing `r.<rx>.<rz>.mca` so every other chunk already on disk stays
/// untouched — a clean chunk in the same region keeps its record byte-for-byte.
/// The write goes through `write_region_file`'s fsync-then-rename, so a crash
/// leaves either the old file or the new one, never a partial. Returns the
/// number of overlay cells flushed.
#[allow(dead_code)]
fn flush_single_chunk_to_anvil_dir(
    dir: &std::path::Path,
    dim: DimId,
    cx: i32,
    cz: i32,
) -> Result<usize, String> {
    Ok(flush_single_chunk_to_anvil_dir_outcome(dir, dim, cx, cz)?.blocks_flushed)
}

/// `flush_single_chunk_to_anvil_dir` plus the persistence counters the
/// incremental save queue needs: bytes committed and whether the flush was a
/// content-hash skip (`blocks_flushed` still reports the preserved overlay
/// cells either way — they are durable, the write was just unnecessary).
#[allow(dead_code)]
fn flush_single_chunk_to_anvil_dir_outcome(
    dir: &std::path::Path,
    dim: DimId,
    cx: i32,
    cz: i32,
) -> Result<AnvilChunkFlushOutcome, String> {
    let rel = anvil_dim_region_dir(dim)
        .ok_or_else(|| format!("dimension {dim:?} has no anvil region directory"))?;
    let region_dir = dir.join(rel);
    std::fs::create_dir_all(&region_dir)
        .map_err(|e| format!("cannot create {}: {e}", region_dir.display()))?;
    let region_path = region_dir.join(format!(
        "r.{}.{}.mca",
        cx.div_euclid(32),
        cz.div_euclid(32)
    ));
    // Merge, never blindly overwrite: an existing region file keeps its other
    // chunks; one that fails to read aborts the flush outright — a partial file
    // must never replace a good one.
    let mut region = if region_path.is_file() {
        read_region_file(&region_path)?
    } else {
        AnvilRegion::default()
    };
    let overlay = world()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&(dim, cx, cz))
        .cloned()
        .unwrap_or_default();
    let flushed = overlay.len();
    let chunk = AnvilChunk {
        cx,
        cz,
        data_version: ANVIL_DATA_VERSION,
        status: "minecraft:full".to_string(),
        sections: anvil_export_sections(dim, cx, cz, &overlay),
        block_entities: evict_chunk_block_entities(dim, cx, cz),
        unknown_states: 0,
        dropped_sections: 0,
    };
    let fresh_nbt = anvil_chunk_to_nbt(&chunk, dim.min_y() >> 4);
    let fresh_hash = anvil_nbt_fingerprint(&fresh_nbt);
    let key: AnvilPersistedKey = (dir.to_path_buf(), dim, cx, cz);
    let mut outcome = AnvilChunkFlushOutcome {
        blocks_flushed: flushed,
        bytes_written: 0,
        skipped_unchanged: false,
    };
    {
        let mut hashes = anvil_persisted_hashes()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if anvil_payload_unchanged(
            key.clone(),
            fresh_hash,
            region.chunks.get(&(cx, cz)).map(|record| &record.nbt),
            &mut hashes,
        ) {
            outcome.skipped_unchanged = true;
            return Ok(outcome);
        }
        let timestamp = (border_now_ms() / 1000).min(u64::from(u32::MAX)) as u32;
        region.chunks.insert(
            (cx, cz),
            AnvilChunkRecord {
                timestamp,
                compression: 2,
                nbt: fresh_nbt,
            },
        );
        write_region_file(&region_path, &region)?;
        outcome.bytes_written = anvil_file_len(&region_path);
        hashes.insert(key, fresh_hash);
    }
    Ok(outcome)
}

/// Re-home one flushed chunk's `block_entities` into the live stores: `Items`
/// records go to `chests()`/`furnaces()`/`live_brewing_stands()` by the
/// record's container kind, sign text to `signs()`, an unspent `LootTable` back
/// to `pending_block_loot()`, and store-owned kinds (beacon, decorated pot, …)
/// to `live_block_entities()`. Every record also lands in
/// `anvil_block_entities()`: what no live store owns is still re-exported
/// verbatim, so a flush -> reload -> flush cycle loses no tag — the same
/// treatment `apply_anvil_import` gives an imported chunk.
#[allow(dead_code)]
fn apply_chunk_block_entities(dim: DimId, entities: &[AnvilNbt]) {
    use crate::generation_payload::ContainerKind;
    for entity in entities {
        let (Some(x), Some(y), Some(z)) = (
            anvil_i32(entity, "x"),
            anvil_i32(entity, "y"),
            anvil_i32(entity, "z"),
        ) else {
            continue;
        };
        let key: DimBlockPos = (dim, x, y, z);
        let id = anvil_str(entity, "id").unwrap_or_default();
        anvil_block_entities()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(key, entity.clone());
        if id.ends_with(":sign") || id.ends_with(":hanging_sign") {
            let mut sign_map = signs().lock().unwrap_or_else(|e| e.into_inner());
            let front = anvil_sign_lines(entity.get("front_text"));
            if front.iter().any(|line| !line.is_empty()) {
                sign_map.insert((dim, x, y, z, 1), front);
            }
            let back = anvil_sign_lines(entity.get("back_text"));
            if back.iter().any(|line| !line.is_empty()) {
                sign_map.insert((dim, x, y, z, 0), back);
            }
        }
        let _ = restore_live_block_entity(key, entity);
        let mut items_owned = false;
        if let Some(items) = entity.get("Items") {
            if let Ok(slots) = items_nbt_to_stacks(items) {
                items_owned = match ContainerKind::for_block(&id) {
                    Some(ContainerKind::Furnace) => {
                        furnaces()
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .insert(
                                key,
                                Furnace {
                                    input: slots.get(&0).cloned().unwrap_or_default(),
                                    fuel: slots.get(&1).cloned().unwrap_or_default(),
                                    output: slots.get(&2).cloned().unwrap_or_default(),
                                    burn_left: entity
                                        .get("BurnTime")
                                        .and_then(nbt_int_val)
                                        .unwrap_or(0) as i32,
                                    progress: entity
                                        .get("CookTime")
                                        .and_then(nbt_int_val)
                                        .unwrap_or(0) as i32,
                                    dirty: false,
                                    xp: 0.0,
                                    recipe_cache: CachedLookup::default(),
                                    fuel_cache: CachedLookup::default(),
                                },
                            );
                        true
                    }
                    Some(_) => {
                        chests()
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .insert(ContainerKey::Block(key), slots);
                        true
                    }
                    None if id == "minecraft:brewing_stand" => {
                        live_brewing_stands()
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .insert(key, live_brewing_stand_from_nbt(entity, &slots));
                        true
                    }
                    None => false,
                };
            }
        }
        // A `LootTable` next to materialized `Items` was already rolled: the
        // live container owns the cell, so only an Items-less record restores
        // a pending roll.
        if !items_owned {
            if let Some(table) = anvil_str(entity, "LootTable") {
                pending_block_loot()
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(
                        key,
                        PendingBlockLoot {
                            kind: ContainerKind::for_block(&id)
                                .unwrap_or(ContainerKind::Chest),
                            loot_table: table,
                            loot_seed: anvil_i64(entity, "LootTableSeed"),
                        },
                    );
            }
        }
    }
}

/// Re-hydrate one chunk from `dir`'s vanilla region file: the stored sections
/// become the live overlay (generated terrain included, which is what a flush
/// wrote) and the block entities return to the stores that own them via
/// `apply_chunk_block_entities`. `Ok(false)` when the region file or the chunk
/// record does not exist — a chunk that was never flushed is a miss, not an
/// error.
#[allow(dead_code)]
fn reload_single_chunk_from_anvil_dir(
    dir: &std::path::Path,
    dim: DimId,
    cx: i32,
    cz: i32,
) -> Result<bool, String> {
    let Some(rel) = anvil_dim_region_dir(dim) else {
        return Err(format!("dimension {dim:?} has no anvil region directory"));
    };
    let region_path = dir.join(rel).join(format!(
        "r.{}.{}.mca",
        cx.div_euclid(32),
        cz.div_euclid(32)
    ));
    if !region_path.is_file() {
        return Ok(false);
    }
    let region = read_region_file(&region_path)?;
    let Some(record) = region.chunks.get(&(cx, cz)) else {
        return Ok(false);
    };
    let chunk = anvil_chunk_from_nbt(&record.nbt, (cx, cz))?;
    let edits = anvil_chunk_edits(&chunk, dim);
    let mut w = world().lock().unwrap_or_else(|e| e.into_inner());
    if edits.is_empty() {
        w.remove(&(dim, cx, cz));
    } else {
        w.insert((dim, cx, cz), edits);
    }
    drop(w);
    apply_chunk_block_entities(dim, &chunk.block_entities);
    Ok(true)
}

#[cfg(test)]
mod anvil_io_tests {
    use super::*;

    fn anvil_tmp_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("megu-anvil-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn sample_chunk_nbt(cx: i32, cz: i32) -> AnvilNbt {
        anvil_obj(&[
            ("DataVersion", AnvilNbt::Int(ANVIL_DATA_VERSION)),
            ("xPos", AnvilNbt::Int(cx)),
            ("zPos", AnvilNbt::Int(cz)),
            ("yPos", AnvilNbt::Int(-4)),
            ("Status", anvil_s("minecraft:full")),
            (
                "sections",
                anvil_list(vec![anvil_obj(&[
                    ("Y", AnvilNbt::Byte(0)),
                    (
                        "block_states",
                        anvil_obj(&[("palette", anvil_list(vec![anvil_obj(&[(
                            "Name",
                            anvil_s("minecraft:stone"),
                        )])]))]),
                    ),
                    (
                        "biomes",
                        anvil_obj(&[("palette", anvil_list(vec![anvil_s(
                            "minecraft:plains",
                        )]))]),
                    ),
                ])]),
            ),
            (
                "block_entities",
                anvil_list(vec![anvil_obj(&[
                    ("id", anvil_s("minecraft:chest")),
                    ("x", AnvilNbt::Int(3)),
                    ("y", AnvilNbt::Int(70)),
                    ("z", AnvilNbt::Int(-2)),
                    ("LootTable", anvil_s("minecraft:chests/simple_dungeon")),
                ])]),
            ),
        ])
    }

    #[cfg(feature = "mod-suite")]
    #[test]
    fn anvil_take_world_snapshot_roundtrip() {
        let scratch = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target/tmp")
            .join(format!("anvil-world-snapshot-{}", std::process::id()));
        let world_root = scratch.join("world");
        let backup_root = scratch.join("backups");
        let region_path = world_root.join("region/r.0.0.mca");
        std::fs::create_dir_all(region_path.parent().unwrap()).unwrap();
        write_level_dat_path(
            &world_root.join("level.dat"),
            &anvil_level_dat_from_state("snapshot-test", ANVIL_DATA_VERSION),
        )
        .unwrap();
        let mut region = AnvilRegion::default();
        region.chunks.insert(
            (0, 0),
            AnvilChunkRecord {
                timestamp: 17,
                compression: 2,
                nbt: sample_chunk_nbt(0, 0),
            },
        );
        write_region_file(&region_path, &region).unwrap();
        let initial_region = std::fs::read(&region_path).unwrap();

        let snapshot_id = anvil_take_world_snapshot(&world_root, &backup_root, "pre-import", 100)
            .unwrap();
        let store = megu_state_store::world_snapshot::WorldSnapshotStore::new(
            &world_root,
            &backup_root,
        )
        .unwrap();
        assert_eq!(store.verify_snapshot(&snapshot_id).unwrap().files_verified, 2);
        assert_eq!(store.load_manifest(&snapshot_id).unwrap().mc_version, "1.21.1");

        std::fs::write(&region_path, b"modified region").unwrap();
        std::fs::write(world_root.join("post-snapshot.dat"), b"new file").unwrap();
        anvil_verify_and_restore_world_snapshot(&world_root, &backup_root, &snapshot_id, 101)
            .unwrap();
        assert_eq!(std::fs::read(&region_path).unwrap(), initial_region);
        assert_eq!(read_region_file(&region_path).unwrap(), region);
        assert!(!world_root.join("post-snapshot.dat").exists());
        assert_eq!(store.verify_snapshot(&snapshot_id).unwrap().files_verified, 2);

        write_level_dat_path(
            &world_root.join("level.dat"),
            &anvil_level_dat_from_state("snapshot-test", 5023),
        )
        .unwrap();
        let latest_snapshot_id =
            anvil_take_world_snapshot(&world_root, &backup_root, "pre-migration", 102).unwrap();
        assert_eq!(
            store.load_manifest(&latest_snapshot_id).unwrap().mc_version,
            "26.3"
        );
        std::fs::remove_dir_all(scratch).unwrap();
    }

    #[test]
    fn anvil_nbt_roundtrip_all_13_tag_types_and_rejects_truncation() {
        let empty_name = crate::nbt::NbtString(Vec::new());
        // TAG_End as a bare root: id byte 0, no name, no payload.
        let end_bytes = crate::nbt::write_named(&empty_name, &AnvilNbt::End).unwrap();
        assert_eq!(end_bytes, [0]);
        let (_, end) = crate::nbt::read_named(&end_bytes).unwrap();
        assert_eq!(end, AnvilNbt::End);

        // The other twelve inside one compound, with a nested compound+list to
        // exercise the recursive bounds checks.
        let all = anvil_obj(&[
            ("b", AnvilNbt::Byte(-8)),
            ("s", AnvilNbt::Short(-3000)),
            ("i", AnvilNbt::Int(-200_000)),
            ("l", AnvilNbt::Long(-9_000_000_000)),
            ("f", AnvilNbt::Float(1.5f32.to_bits())),
            ("d", AnvilNbt::Double((-0.25f64).to_bits())),
            ("ba", AnvilNbt::ByteArray(vec![0, 1, 250, 255])),
            ("str", anvil_s("héllo wörld")),
            (
                "list",
                AnvilNbt::List(3, vec![AnvilNbt::Int(1), AnvilNbt::Int(-7)]),
            ),
            (
                "compound",
                anvil_obj(&[
                    ("nested", anvil_s("x")),
                    ("empty_list", AnvilNbt::List(0, vec![])),
                ]),
            ),
            ("ia", AnvilNbt::IntArray(vec![1, -1, i32::MAX, i32::MIN])),
            (
                "la",
                AnvilNbt::LongArray(vec![1, -1, i64::MAX, i64::MIN]),
            ),
        ]);
        let bytes = crate::nbt::write_named(&crate::nbt::NbtString::text("root"), &all).unwrap();
        let (name, decoded) = crate::nbt::read_named(&bytes).unwrap();
        assert_eq!(name.to_utf8().unwrap(), "root");
        assert_eq!(decoded, all);
        // Every proper prefix must fail cleanly (no panic, no partial read).
        for cut in 0..bytes.len() {
            assert!(
                crate::nbt::read_named(&bytes[..cut]).is_err(),
                "prefix of {cut} bytes must not parse"
            );
        }
        // A declared collection length beyond the input must fail, not wrap.
        let mut bad = bytes.clone();
        let last = bad.len() - 1;
        bad[last] = 0x7f;
        assert!(crate::nbt::read_named(&bad[..bytes.len() - 3]).is_err());
        // Unknown tag id rejected.
        assert!(crate::nbt::read_named(&[99, 0, 0]).is_err());
        // Trailing bytes after a complete document rejected.
        let mut trailing = bytes.clone();
        trailing.push(0);
        assert!(crate::nbt::read_named(&trailing).is_err());
    }

    #[test]
    fn anvil_bitpack_matches_vanilla_1_16_non_spanning_layout() {
        // Width rule: max(4, ceillog2(len)) for blockstates, max(1, ...) for biomes.
        assert_eq!(anvil_bits_for(1, 4), 4);
        assert_eq!(anvil_bits_for(2, 4), 4);
        assert_eq!(anvil_bits_for(16, 4), 4);
        assert_eq!(anvil_bits_for(17, 4), 5);
        assert_eq!(anvil_bits_for(33, 4), 6);
        assert_eq!(anvil_bits_for(64, 4), 6);
        assert_eq!(anvil_bits_for(65, 4), 7);
        assert_eq!(anvil_bits_for(300, 4), 9);
        assert_eq!(anvil_bits_for(2, 1), 1);
        assert_eq!(anvil_bits_for(5, 1), 3);

        // 4-bit: 16 entries per long, 256 longs for 4096 cells.
        let values: Vec<u64> = (0..4096).map(|i| i as u64 % 16).collect();
        let packed = anvil_pack_longs(&values, 4);
        assert_eq!(packed.len(), 256);
        assert_eq!(
            packed[0] as u64,
            (0..16).fold(0u64, |w, j| w | (j as u64) << (j * 4))
        );
        assert_eq!(anvil_unpack_longs(&packed, 4, 4096).unwrap(), values);

        // 5-bit: 12 entries per long. Entry 11 is the last slot of word 0; entry
        // 12 starts word 1 at bit 0 — a spanning layout would put it at bit 55.
        let mut values = vec![0u64; 4096];
        values[11] = 31;
        values[12] = 31;
        let packed = anvil_pack_longs(&values, 5);
        assert_eq!(packed.len(), (4096usize.div_ceil(12)));
        assert_eq!(packed[0] as u64, 31u64 << 55);
        assert_eq!(packed[1] as u64 & 31, 31);
        assert_eq!((packed[0] as u64) >> 60, 0, "the 4 tail bits of a 5-bit word are unused");
        assert_eq!(anvil_unpack_longs(&packed, 5, 4096).unwrap(), values);

        // 6-bit: 10 per long; boundary at index 10.
        let mut values = vec![0u64; 4096];
        values[9] = 63;
        values[10] = 63;
        let packed = anvil_pack_longs(&values, 6);
        assert_eq!(packed[0] as u64, 63u64 << 54);
        assert_eq!(packed[1] as u64 & 63, 63);
        assert_eq!(anvil_unpack_longs(&packed, 6, 4096).unwrap(), values);

        // 9-bit: 7 per long, one wasted tail bit; boundary at index 7.
        let mut values = vec![0u64; 4096];
        values[6] = 511;
        values[7] = 511;
        let packed = anvil_pack_longs(&values, 9);
        assert_eq!(packed.len(), 4096usize.div_ceil(7));
        assert_eq!(packed[0] as u64, 511u64 << 54);
        assert_eq!((packed[0] as u64) >> 63, 0, "the 64th bit of a 9-bit word is unused");
        assert_eq!(packed[1] as u64 & 511, 511);
        assert_eq!(anvil_unpack_longs(&packed, 9, 4096).unwrap(), values);

        // Biome-scale arrays (64 cells) at small widths.
        for bits in [1u8, 2, 3, 4] {
            let values: Vec<u64> = (0..64).map(|i| i as u64 % (1 << bits)).collect();
            let packed = anvil_pack_longs(&values, bits);
            assert_eq!(anvil_unpack_longs(&packed, bits, 64).unwrap(), values);
        }
        // A short data array is an error, never a silent zero read.
        assert!(anvil_unpack_longs(&[0], 4, 4096).is_err());
        assert!(anvil_unpack_longs(&[0, 0], 0, 8).is_err());
        // A single-entry palette needs no data array at all (checked at the
        // section level: `anvil_pack_section` returns None for it).
        let mut palette = Vec::new();
        assert_eq!(anvil_pack_section(&[7u32; 4096], &mut palette), None);
        assert_eq!(palette, vec![7]);
        let mut palette = Vec::new();
        let mut cells = [7u32; 4096];
        cells[100] = 9;
        assert!(anvil_pack_section(&cells, &mut palette).is_some());
        assert_eq!(palette, vec![7, 9]);
    }

    #[test]
    fn anvil_mca_region_roundtrips_positive_and_negative_chunks_zlib_gzip_raw() {
        let dir = anvil_tmp_dir("region");
        // r.-1.-1.mca: only chunk (-1,-1) belongs; r.0.0.mca: (0,0) and (31,31).
        assert_eq!(
            anvil_region_coords(&dir.join("r.-1.-1.mca")).unwrap(),
            (-1, -1)
        );
        assert_eq!((-1i32).div_euclid(32), -1);
        let negative_chunk = anvil_region_coords(&dir.join("r.-1.-1.mca")).unwrap().0;
        assert_eq!((negative_chunk & 31) + (negative_chunk & 31) * 32, 1023);

        let mut region_neg = AnvilRegion::default();
        region_neg.chunks.insert(
            (-1, -1),
            AnvilChunkRecord {
                timestamp: 0xdead_beef,
                compression: 2,
                nbt: sample_chunk_nbt(-1, -1),
            },
        );
        write_region_file(&dir.join("r.-1.-1.mca"), &region_neg).unwrap();

        let mut region_pos = AnvilRegion::default();
        region_pos.chunks.insert(
            (0, 0),
            AnvilChunkRecord {
                timestamp: 1,
                compression: 1,
                nbt: sample_chunk_nbt(0, 0),
            },
        );
        region_pos.chunks.insert(
            (31, 31),
            AnvilChunkRecord {
                timestamp: 0xffff_ffff,
                compression: 3,
                nbt: sample_chunk_nbt(31, 31),
            },
        );
        write_region_file(&dir.join("r.0.0.mca"), &region_pos).unwrap();

        // File shape: two 4 KiB tables then sector-aligned payload runs.
        let file = std::fs::read(dir.join("r.0.0.mca")).unwrap();
        assert_eq!(file.len() % ANVIL_SECTOR, 0);
        assert!(file.len() > 2 * ANVIL_SECTOR);
        // Slot 0 (chunk 0,0) is first: offset must be 2, gzip payload.
        let offset0 = (usize::from(file[0]) << 16) | (usize::from(file[1]) << 8) | usize::from(file[2]);
        assert_eq!(offset0, 2);
        assert_eq!(file[2 * ANVIL_SECTOR + 4], 1); // gzip compression byte
        assert_eq!(
            u32::from_be_bytes(file[ANVIL_SECTOR..ANVIL_SECTOR + 4].try_into().unwrap()),
            1
        ); // timestamp of chunk (0,0)
        // Slot 1023 (chunk 31,31): uncompressed marker byte.
        let slot1023 = 1023 * 4;
        let off31 = (usize::from(file[slot1023]) << 16)
            | (usize::from(file[slot1023 + 1]) << 8)
            | usize::from(file[slot1023 + 2]);
        assert!(off31 > 2);
        assert_eq!(file[off31 * ANVIL_SECTOR + 4], 3);

        let got_neg = read_region_file(&dir.join("r.-1.-1.mca")).unwrap();
        assert_eq!(got_neg, region_neg);
        let got_pos = read_region_file(&dir.join("r.0.0.mca")).unwrap();
        assert_eq!(got_pos, region_pos);
        // The stored block_entities list survived the full region round-trip.
        let entity = got_pos.chunks[&(0, 0)]
            .nbt
            .get("block_entities")
            .and_then(AnvilNbt::list)
            .and_then(|l| l.first())
            .unwrap();
        assert_eq!(anvil_str(entity, "id").as_deref(), Some("minecraft:chest"));
        assert_eq!(
            anvil_str(entity, "LootTable").as_deref(),
            Some("minecraft:chests/simple_dungeon")
        );

        // A chunk written to the wrong region name is refused before any byte.
        let mut bad = AnvilRegion::default();
        bad.chunks.insert(
            (0, 0),
            AnvilChunkRecord {
                timestamp: 0,
                compression: 2,
                nbt: sample_chunk_nbt(0, 0),
            },
        );
        assert!(write_region_file(&dir.join("r.-1.-1.mca"), &bad).is_err());
        // A file shorter than the two tables is truncated input, not a region.
        let tiny = dir.join("r.5.5.mca");
        std::fs::write(&tiny, [0u8; 100]).unwrap();
        assert!(read_region_file(&tiny).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn anvil_level_dat_gzip_nbt_roundtrips_seed_gamerules_border_spawn_time() {
        let mut rules = Vec::new();
        for &(name, is_bool, default) in COMMAND_GAMERULE_DEFAULTS {
            rules.push((name, anvil_s(&anvil_gamerule_string(is_bool, default))));
        }
        let mut level = AnvilLevelDat {
            data: anvil_obj(&[
                ("DataVersion", AnvilNbt::Int(ANVIL_DATA_VERSION)),
                ("version", AnvilNbt::Int(ANVIL_LEVEL_VERSION)),
                (
                    "Version",
                    anvil_obj(&[
                        ("Id", AnvilNbt::Int(ANVIL_DATA_VERSION)),
                        ("Name", anvil_s("1.21.1")),
                        ("Series", anvil_s("main")),
                        ("Snapshot", AnvilNbt::Byte(0)),
                    ]),
                ),
                ("LevelName", anvil_s("roundtrip world")),
                (
                    "WorldGenSettings",
                    anvil_obj(&[
                        ("seed", AnvilNbt::Long(-5_000_000_000)),
                        ("generate_features", AnvilNbt::Byte(1)),
                        ("bonus_chest", AnvilNbt::Byte(0)),
                        ("dimensions", anvil_level_dimensions()),
                    ]),
                ),
                ("SpawnX", AnvilNbt::Int(-120)),
                ("SpawnY", AnvilNbt::Int(64)),
                ("SpawnZ", AnvilNbt::Int(255)),
                ("SpawnAngle", AnvilNbt::Float(2.5f32.to_bits())),
                ("Time", AnvilNbt::Long(240_000)),
                ("DayTime", AnvilNbt::Long(17_500)),
                ("Difficulty", AnvilNbt::Byte(3)),
                ("DifficultyLocked", AnvilNbt::Byte(1)),
                ("GameType", AnvilNbt::Int(1)),
                ("raining", AnvilNbt::Byte(1)),
                ("rainTime", AnvilNbt::Int(6_000)),
                ("thundering", AnvilNbt::Byte(1)),
                ("thunderTime", AnvilNbt::Int(400)),
                ("BorderCenterX", AnvilNbt::Double(12.75f64.to_bits())),
                ("BorderCenterZ", AnvilNbt::Double((-40.5f64).to_bits())),
                ("BorderSize", AnvilNbt::Double(9_999.5f64.to_bits())),
                ("BorderWarningBlocks", AnvilNbt::Double(11.0f64.to_bits())),
                ("BorderWarningTime", AnvilNbt::Double(9.0f64.to_bits())),
                ("BorderDamagePerBlock", AnvilNbt::Double(0.4f64.to_bits())),
                ("BorderSafeZone", AnvilNbt::Double(7.5f64.to_bits())),
                ("GameRules", anvil_obj(&rules)),
            ]),
        };
        // A field the reader does not model still survives, verbatim.
        level.set("customField", anvil_s("kept"));

        let bytes = write_level_dat(&level).unwrap();
        assert_eq!(&bytes[..2], &[0x1f, 0x8b], "level.dat must be gzip");
        let got = read_level_dat(&bytes).unwrap();
        assert_eq!(got.data_version(), ANVIL_DATA_VERSION);
        assert_eq!(got.version_id(), ANVIL_LEVEL_VERSION);
        assert_eq!(got.level_name(), "roundtrip world");
        assert_eq!(got.seed(), -5_000_000_000);
        assert_eq!(got.spawn(), (-120, 64, 255, 2.5));
        assert_eq!(got.time(), 240_000);
        assert_eq!(got.day_time(), 17_500);
        assert_eq!(got.difficulty(), 3);
        assert!(got.difficulty_locked());
        assert_eq!(got.game_type(), 1);
        assert_eq!(got.weather(), (true, 6_000, true, 400));
        let (cx, cz, size, wb, wt, dmg, safe) = got.border();
        assert_eq!((cx, cz, size, wb, wt, dmg, safe), (12.75, -40.5, 9_999.5, 11.0, 9.0, 0.4, 7.5));
        let rules = got.game_rules();
        assert_eq!(rules.len(), COMMAND_GAMERULE_DEFAULTS.len());
        for &(name, is_bool, default) in COMMAND_GAMERULE_DEFAULTS {
            assert_eq!(
                rules[name].as_str(),
                anvil_gamerule_string(is_bool, default).as_str(),
                "rule {name}"
            );
        }
        assert_eq!(anvil_str(got.data(), "customField").as_deref(), Some("kept"));

        // Not gzip / not a Data compound / truncated input: all refused.
        assert!(read_level_dat(b"not gzip").is_err());
        assert!(read_level_dat(&bytes[..bytes.len() / 2]).is_err());
        let dir = anvil_tmp_dir("leveldat");
        write_level_dat_path(&dir.join("level.dat"), &level).unwrap();
        assert_eq!(read_level_dat_path(&dir.join("level.dat")).unwrap(), got);
        assert!(!dir.join("level.dat.tmp").exists(), "tmp file must be renamed");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Snapshot-and-restore for every shared state export/import touches, so the
    /// test leaves the process the way it found it (the suite shares these).
    struct AnvilWorldFixture {
        seed: u64,
        spawn: (f64, f64, f64, f32),
        time: u64,
        difficulty: u8,
        raining: u64,
        thundering: u64,
        game_mode: u8,
        border: WorldBorderState,
        rules: HashMap<&'static str, i32>,
        rule_keep: u8,
        rule_day: u8,
        rule_weather: u8,
        rule_spawn: u8,
        rule_grief: u8,
        rule_regen: u8,
        rule_loot: u8,
        rule_cram: i32,
        world_snapshot: HashMap<DimChunkKey, HashMap<u32, u32>>,
        entities: HashMap<DimBlockPos, AnvilNbt>,
        sign_entries: Vec<(SignKey, Option<SignLines>)>,
    }

    impl AnvilWorldFixture {
        fn capture(sign_keys: &[SignKey]) -> Self {
            let w = world().lock().unwrap_or_else(|e| e.into_inner());
            let s = signs().lock().unwrap_or_else(|e| e.into_inner());
            Self {
                seed: WORLD_SEED.load(Ordering::Relaxed),
                spawn: *WORLD_SPAWN.lock().unwrap_or_else(|e| e.into_inner()),
                time: WORLD_TIME.load(Ordering::Relaxed),
                difficulty: DIFFICULTY.load(Ordering::Relaxed),
                raining: RAINING.load(Ordering::Relaxed),
                thundering: THUNDERING.load(Ordering::Relaxed),
                game_mode: DEFAULT_GAME_MODE.load(Ordering::Relaxed),
                border: *world_border().lock().unwrap_or_else(|e| e.into_inner()),
                rules: command_gamerules().lock().unwrap_or_else(|e| e.into_inner()).clone(),
                rule_keep: GR_KEEP_INVENTORY.load(Ordering::Relaxed),
                rule_day: GR_DAYLIGHT_CYCLE.load(Ordering::Relaxed),
                rule_weather: GR_WEATHER_CYCLE.load(Ordering::Relaxed),
                rule_spawn: GR_MOB_SPAWNING.load(Ordering::Relaxed),
                rule_grief: GR_MOB_GRIEFING.load(Ordering::Relaxed),
                rule_regen: GR_NATURAL_REGEN.load(Ordering::Relaxed),
                rule_loot: GR_MOB_LOOT.load(Ordering::Relaxed),
                rule_cram: GR_MAX_ENTITY_CRAMMING.load(Ordering::Relaxed),
                world_snapshot: w.clone(),
                entities: anvil_block_entities().lock().unwrap_or_else(|e| e.into_inner()).clone(),
                sign_entries: sign_keys.iter().map(|&k| (k, s.get(&k).cloned())).collect(),
            }
        }
    }

    impl std::ops::Drop for AnvilWorldFixture {
        fn drop(&mut self) {
            WORLD_SEED.store(self.seed, Ordering::Relaxed);
            *WORLD_SPAWN.lock().unwrap_or_else(|e| e.into_inner()) = self.spawn;
            WORLD_TIME.store(self.time, Ordering::Relaxed);
            DIFFICULTY.store(self.difficulty, Ordering::Relaxed);
            RAINING.store(self.raining, Ordering::Relaxed);
            THUNDERING.store(self.thundering, Ordering::Relaxed);
            DEFAULT_GAME_MODE.store(self.game_mode, Ordering::Relaxed);
            *world_border().lock().unwrap_or_else(|e| e.into_inner()) = self.border;
            let restored = std::mem::take(&mut self.rules);
            for (&name, &val) in &restored {
                gamerule_atomic_store(name, val);
            }
            *command_gamerules().lock().unwrap_or_else(|e| e.into_inner()) = restored;
            GR_KEEP_INVENTORY.store(self.rule_keep, Ordering::Relaxed);
            GR_DAYLIGHT_CYCLE.store(self.rule_day, Ordering::Relaxed);
            GR_WEATHER_CYCLE.store(self.rule_weather, Ordering::Relaxed);
            GR_MOB_SPAWNING.store(self.rule_spawn, Ordering::Relaxed);
            GR_MOB_GRIEFING.store(self.rule_grief, Ordering::Relaxed);
            GR_NATURAL_REGEN.store(self.rule_regen, Ordering::Relaxed);
            GR_MOB_LOOT.store(self.rule_loot, Ordering::Relaxed);
            GR_MAX_ENTITY_CRAMMING.store(self.rule_cram, Ordering::Relaxed);
            *world().lock().unwrap_or_else(|e| e.into_inner()) = std::mem::take(&mut self.world_snapshot);
            *anvil_block_entities().lock().unwrap_or_else(|e| e.into_inner()) = std::mem::take(&mut self.entities);
            let mut s = signs().lock().unwrap_or_else(|e| e.into_inner());
            for (key, entry) in self.sign_entries.drain(..) {
                match entry {
                    Some(lines) => {
                        s.insert(key, lines);
                    }
                    None => {
                        s.remove(&key);
                    }
                }
            }
        }
    }

    #[test]
    fn anvil_world_export_and_reimport_preserves_blocks_and_metadata() {
        let _guard = world_guard();
        let ow = (DimId::Overworld, 0i32, 0i32);
        let nether = (DimId::Nether, 0i32, 0i32);
        let end = (DimId::End, 0i32, 0i32);
        let sign_key: SignKey = (DimId::Overworld, 3, 80, 3, 1);
        let _fixture = AnvilWorldFixture::capture(&[sign_key]);

        // Distinctive edits across all three dims: (pos -> vanilla state).
        let grass = name_to_state("grass_block").expect("grass_block state");
        let stone = name_to_state("stone").expect("stone state");
        let netherrack = name_to_state("netherrack").expect("netherrack state");
        let end_stone = name_to_state("end_stone").expect("end_stone state");
        {
            // Replace the whole overlay (restored by the fixture) so the export
            // reports exactly the chunks this test planted.
            let mut w = world().lock().unwrap_or_else(|e| e.into_inner());
            w.clear();
            let mut ow_chunk = HashMap::new();
            ow_chunk.insert(local_idx(1, 70, 2), grass);
            ow_chunk.insert(local_idx(15, -64, 15), stone);
            w.insert(ow, ow_chunk);
            w.insert(nether, HashMap::from([(local_idx(2, 40, 3), netherrack)]));
            w.insert(end, HashMap::from([(local_idx(4, 50, 5), end_stone)]));
        }
        anvil_block_entities().lock().unwrap_or_else(|e| e.into_inner()).clear();
        signs().lock().unwrap_or_else(|e| e.into_inner()).insert(
            sign_key,
            ["first".into(), "second".into(), String::new(), "fourth".into()],
        );
        WORLD_SEED.store(0x1234_5678_9abc_def0, Ordering::Relaxed);
        *WORLD_SPAWN.lock().unwrap_or_else(|e| e.into_inner()) = (12.5, 72.0, -34.5, 45.0);
        WORLD_TIME.store(131_000, Ordering::Relaxed);
        DIFFICULTY.store(3, Ordering::Relaxed);
        RAINING.store(5_000, Ordering::Relaxed);
        THUNDERING.store(300, Ordering::Relaxed);
        command_gamerules().lock().unwrap_or_else(|e| e.into_inner()).insert("keepInventory", 1);
        GR_KEEP_INVENTORY.store(1, Ordering::Relaxed);
        command_gamerules().lock().unwrap_or_else(|e| e.into_inner()).insert("randomTickSpeed", 9);
        gamerule_atomic_store("randomTickSpeed", 9);
        {
            let mut border = world_border().lock().unwrap_or_else(|e| e.into_inner());
            border.center_x = 100.5;
            border.center_z = -20.25;
            border.old_size = 8_000.0;
            border.new_size = 8_000.0;
            border.lerp_duration_ms = 0;
            border.warning_blocks = 10;
            border.warning_time_secs = 20;
            border.damage_per_block = 0.5;
            border.damage_safe_zone = 7.0;
        }

        let out = anvil_tmp_dir("world");
        let report = export_anvil_world(&out).unwrap();
        assert!(report.level_written);
        assert_eq!(report.chunks_written, 3);
        assert_eq!(report.region_files, 3);
        for path in [
            out.join("level.dat"),
            out.join("region").join("r.0.0.mca"),
            out.join("DIM-1").join("region").join("r.0.0.mca"),
            out.join("DIM1").join("region").join("r.0.0.mca"),
        ] {
            assert!(path.is_file(), "missing {}", path.display());
        }

        // First import pass: metadata and per-dimension blocks.
        let imported = import_anvil_world(&out).unwrap();
        assert_eq!(imported.region_files, 3);
        assert!(imported.chunk_errors.is_empty(), "{:?}", imported.chunk_errors);
        let level = imported.level.as_ref().expect("level.dat imported");
        assert_eq!(level.data_version(), ANVIL_DATA_VERSION);
        assert_eq!(level.version_id(), ANVIL_LEVEL_VERSION);
        assert_eq!(level.seed(), 0x1234_5678_9abc_def0u64 as i64);
        assert_eq!(level.spawn(), (12, 72, -35, 45.0));
        assert_eq!(level.day_time(), 131_000);
        assert_eq!(level.difficulty(), 3);
        assert_eq!(level.weather(), (true, 5_000, true, 300));
        assert_eq!(level.game_rules()["keepInventory"], "true");
        assert_eq!(level.game_rules()["randomTickSpeed"], "9");
        let (bcx, bcz, bsz, wb, wt, dmg, safe) = level.border();
        assert_eq!((bcx, bcz, bsz, wb, wt, dmg, safe), (100.5, -20.25, 8_000.0, 10.0, 20.0, 0.5, 7.0));

        let find = |dim: DimId, cx: i32, cz: i32| {
            imported
                .chunks
                .iter()
                .find(|c| c.dim == dim && c.cx == cx && c.cz == cz)
                .unwrap_or_else(|| panic!("chunk ({dim:?}, {cx}, {cz}) not imported"))
        };
        let ow_chunk = find(DimId::Overworld, 0, 0);
        assert_eq!(ow_chunk.edits[&local_idx(1, 70, 2)], grass);
        assert_eq!(ow_chunk.edits[&local_idx(15, -64, 15)], stone);
        assert_eq!(ow_chunk.unknown_states, 0);
        assert_eq!(
            find(DimId::Nether, 0, 0).edits[&local_idx(2, 40, 3)],
            netherrack
        );
        assert_eq!(
            find(DimId::End, 0, 0).edits[&local_idx(4, 50, 5)],
            end_stone
        );
        // The exported chunk's NBT structure is vanilla-shaped.
        let section = read_region_file(&out.join("region").join("r.0.0.mca"))
            .unwrap()
            .chunks
            .remove(&(0, 0))
            .unwrap()
            .nbt;
        assert_eq!(anvil_i32(&section, "DataVersion"), Some(ANVIL_DATA_VERSION));
        assert_eq!(anvil_str(&section, "Status").as_deref(), Some("minecraft:full"));
        assert_eq!(anvil_i32(&section, "xPos"), Some(0));
        assert_eq!(anvil_i32(&section, "yPos"), Some(-4));
        let sign_entity = section
            .get("block_entities")
            .and_then(AnvilNbt::list)
            .unwrap()
            .iter()
            .find(|e| {
                anvil_str(e, "id").is_some_and(|id| id.ends_with("sign"))
                    && anvil_i32(e, "x") == Some(3)
                    && anvil_i32(e, "y") == Some(80)
                    && anvil_i32(e, "z") == Some(3)
            })
            .expect("sign block entity exported");
        let first_line = sign_entity
            .get("front_text")
            .and_then(|t| t.get("messages"))
            .and_then(AnvilNbt::list)
            .and_then(|m| m.first())
            .and_then(anvil_str_of)
            .unwrap();
        assert_eq!(first_line, "\"first\"");

        // Apply: the overlay answers for the imported cells, the sign text lands
        // in the live sign registry, and the entity is kept for re-export.
        apply_anvil_import(&imported);
        assert_eq!(overlay_block_dim(DimId::Overworld, 1, 70, 2), Some(grass));
        assert_eq!(overlay_block_dim(DimId::Nether, 2, 40, 3), Some(netherrack));
        assert_eq!(overlay_block_dim(DimId::End, 4, 50, 5), Some(end_stone));
        assert_eq!(WORLD_SEED.load(Ordering::Relaxed), 0x1234_5678_9abc_def0);
        assert_eq!(*WORLD_SPAWN.lock().unwrap_or_else(|e| e.into_inner()), (12.5, 72.0, -34.5, 45.0));
        assert_eq!(WORLD_TIME.load(Ordering::Relaxed), 131_000);
        assert_eq!(DIFFICULTY.load(Ordering::Relaxed), 3);
        assert_eq!(RAINING.load(Ordering::Relaxed), 5_000);
        assert_eq!(THUNDERING.load(Ordering::Relaxed), 300);
        assert_eq!(command_gamerules().lock().unwrap_or_else(|e| e.into_inner())["keepInventory"], 1);
        assert_eq!(GR_KEEP_INVENTORY.load(Ordering::Relaxed), 1);
        assert_eq!(command_gamerules().lock().unwrap_or_else(|e| e.into_inner())["randomTickSpeed"], 9);
        {
            let b = *world_border().lock().unwrap_or_else(|e| e.into_inner());
            assert_eq!((b.center_x, b.center_z, b.new_size), (100.5, -20.25, 8_000.0));
            assert_eq!((b.warning_blocks, b.warning_time_secs), (10, 20));
        }
        assert_eq!(
            signs().lock().unwrap_or_else(|e| e.into_inner())[&sign_key][..2],
            ["first".to_string(), "second".to_string()]
        );
        assert!(anvil_block_entities()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains_key(&(DimId::Overworld, 3, 80, 3)));

        // Second cycle: export the now-applied world and re-import it; both the
        // blocks and the sign entity survive a full export -> import -> export
        // -> import pass.
        let out2 = anvil_tmp_dir("world2");
        let report2 = export_anvil_world(&out2).unwrap();
        assert!(report2.chunks_written >= 3);
        let imported2 = import_anvil_world(&out2).unwrap();
        let find2 = |dim: DimId, cx: i32, cz: i32| {
            imported2
                .chunks
                .iter()
                .find(|c| c.dim == dim && c.cx == cx && c.cz == cz)
                .unwrap_or_else(|| panic!("chunk ({dim:?}, {cx}, {cz}) not reimported"))
        };
        assert_eq!(find2(DimId::Overworld, 0, 0).edits[&local_idx(1, 70, 2)], grass);
        assert_eq!(find2(DimId::Nether, 0, 0).edits[&local_idx(2, 40, 3)], netherrack);
        assert_eq!(find2(DimId::End, 0, 0).edits[&local_idx(4, 50, 5)], end_stone);
        let level2 = imported2.level.as_ref().unwrap();
        assert_eq!(level2.seed(), 0x1234_5678_9abc_def0u64 as i64);
        assert_eq!(level2.day_time(), 131_000);
        find2(DimId::Overworld, 0, 0)
            .block_entities
            .iter()
            .find(|e| {
                anvil_str(e, "id").is_some_and(|id| id.ends_with("sign"))
                    && anvil_i32(e, "x") == Some(3)
                    && anvil_i32(e, "y") == Some(80)
                    && anvil_i32(e, "z") == Some(3)
            })
            .expect("sign entity survives re-export");

        let _ = std::fs::remove_dir_all(&out);
        let _ = std::fs::remove_dir_all(&out2);
    }

    #[test]
    fn anvil_data_version_table_classifies_every_known_milestone() {
        use AnvilSchemaVersion as V;
        for (schema, dv, name) in [
            (V::V1_20_4, 3700, "1.20.4"),
            (V::V1_20_6, 3839, "1.20.6"),
            (V::V1_21, 3953, "1.21"),
            (V::V1_21_1, 3955, "1.21.1"),
            (V::V1_21_3, 4082, "1.21.3"),
            (V::V1_21_4, 4189, "1.21.4"),
            (V::V1_21_5, 4325, "1.21.5"),
            (V::V26_3, 5023, "26.3"),
        ] {
            assert_eq!(schema.data_version(), dv);
            assert_eq!(schema.release_name(), name);
            assert_eq!(V::from_data_version(dv), schema);
        }
        use AnvilVersionSupport as S;
        // The codec's native format is exactly 1.21.1; 26.3 is the known
        // supported latest-vanilla target, not a second native format.
        assert_eq!(classify_anvil_data_version(ANVIL_DATA_VERSION), S::Native);
        // Every other milestone shares this codec's section schema.
        for dv in [3700, 3839, 3953, 4082, 4189, 4325, 5023] {
            assert_eq!(
                classify_anvil_data_version(dv),
                S::Compatible,
                "milestone {dv}"
            );
        }
        // Snapshot DataVersions between milestones classify the same way.
        for dv in [3701, 3838, 3954, 4000, 4188, 4190, 4324, 4326, 4400, 5022] {
            assert_eq!(
                classify_anvil_data_version(dv),
                S::Compatible,
                "snapshot {dv}"
            );
        }
        // Older than the table or a corrupt/absent value: legacy, not native.
        for dv in [0, -1, 1343, 2566, 3465, 3699] {
            assert_eq!(
                classify_anvil_data_version(dv),
                S::LegacyUnsupported,
                "legacy {dv}"
            );
        }
        // Newer than the table: an unlisted release, never assumed readable.
        for dv in [5024, 5100, i32::MAX] {
            assert_eq!(
                classify_anvil_data_version(dv),
                S::FutureUnknown,
                "future {dv}"
            );
        }
    }

    #[test]
    fn anvil_chunk_decode_tolerates_optional_fields_and_records_diagnostics() {
        // A 1.21.4-stamped chunk: one ordinary section, one carrying only the
        // lowercase `y` spelling, one with no index at all (dropped + counted),
        // and no DataVersion sibling problems — the field list is optional.
        let sections = anvil_list(vec![
            anvil_obj(&[
                ("Y", AnvilNbt::Byte(0)),
                (
                    "block_states",
                    anvil_obj(&[("palette", anvil_list(vec![anvil_obj(&[(
                        "Name",
                        anvil_s("minecraft:stone"),
                    )])]))]),
                ),
            ]),
            // Lowercase `y`: placed like `Y`; no block_states/biomes at all.
            anvil_obj(&[("y", AnvilNbt::Byte(-1))]),
            // No index of either spelling: unplaceable, dropped + counted.
            anvil_obj(&[("block_states", anvil_obj(&[]))]),
        ]);
        let nbt = anvil_obj(&[
            ("DataVersion", AnvilNbt::Int(4189)),
            ("xPos", AnvilNbt::Int(0)),
            ("zPos", AnvilNbt::Int(0)),
            ("sections", sections),
        ]);
        let chunk = anvil_chunk_from_nbt(&nbt, (0, 0)).unwrap();
        assert_eq!(chunk.data_version, 4189);
        assert_eq!(classify_anvil_data_version(chunk.data_version), AnvilVersionSupport::Compatible);
        assert_eq!(chunk.sections.len(), 2);
        assert_eq!((chunk.sections[0].y, chunk.sections[1].y), (0, -1));
        assert_eq!(chunk.dropped_sections, 1);

        // A missing/corrupt DataVersion decodes as 0 and classifies as legacy —
        // recorded, never treated as the native version.
        let nbt = anvil_obj(&[
            ("xPos", AnvilNbt::Int(1)),
            ("zPos", AnvilNbt::Int(1)),
            ("sections", anvil_list(vec![])),
        ]);
        let chunk = anvil_chunk_from_nbt(&nbt, (1, 1)).unwrap();
        assert_eq!(chunk.data_version, 0);
        assert_eq!(
            classify_anvil_data_version(chunk.data_version),
            AnvilVersionSupport::LegacyUnsupported
        );
        // A non-Byte `Y` is absent as far as the index is concerned: dropped.
        let nbt = anvil_obj(&[
            ("DataVersion", AnvilNbt::Int(ANVIL_DATA_VERSION)),
            ("sections", anvil_list(vec![anvil_obj(&[("Y", anvil_s("7"))])])),
        ]);
        let chunk = anvil_chunk_from_nbt(&nbt, (0, 0)).unwrap();
        assert_eq!(chunk.sections.len(), 0);
        assert_eq!(chunk.dropped_sections, 1);
        // Structural errors inside a real section still fail the chunk.
        let nbt = anvil_obj(&[
            ("DataVersion", AnvilNbt::Int(ANVIL_DATA_VERSION)),
            (
                "sections",
                anvil_list(vec![anvil_obj(&[
                    ("Y", AnvilNbt::Byte(0)),
                    ("block_states", anvil_obj(&[])),
                ])]),
            ),
        ]);
        assert!(anvil_chunk_from_nbt(&nbt, (0, 0)).is_err());
    }

    #[test]
    fn anvil_export_with_options_stamps_the_target_data_version() {
        let _guard = world_guard();
        let key = (DimId::Overworld, 0i32, 0i32);
        let _fixture = AnvilWorldFixture::capture(&[]);
        let stone = name_to_state("stone").expect("stone state");
        {
            let mut w = world().lock().unwrap_or_else(|e| e.into_inner());
            w.clear();
            w.insert(key, HashMap::from([(local_idx(1, 70, 2), stone)]));
        }
        let out = anvil_tmp_dir("world4189");
        let report = export_anvil_world_with_options(
            &out,
            &AnvilExportOptions {
                target_data_version: 4189,
            },
        )
        .unwrap();
        assert!(report.level_written);
        assert_eq!(report.chunks_written, 1);
        // level.dat carries the target version in both stamps plus its name.
        let level = read_level_dat_path(&out.join("level.dat")).unwrap();
        assert_eq!(level.data_version(), 4189);
        let version = level.data().get("Version").unwrap();
        assert_eq!(anvil_i32(version, "Id"), Some(4189));
        assert_eq!(anvil_str(version, "Name").as_deref(), Some("1.21.4"));
        // So does every chunk record.
        let region = read_region_file(&out.join("region").join("r.0.0.mca")).unwrap();
        let record = region.chunks.get(&(0, 0)).unwrap();
        assert_eq!(anvil_i32(&record.nbt, "DataVersion"), Some(4189));
        // Reimport: the foreign version is recorded in the diagnostics, the
        // blocks still decode, and nothing is counted unsupported.
        let imported = import_anvil_world(&out).unwrap();
        assert_eq!(imported.data_version_counts.get(&4189), Some(&1));
        assert_eq!(imported.unsupported_chunks, 0);
        assert_eq!(imported.dropped_sections, 0);
        assert!(imported.chunk_errors.is_empty(), "{:?}", imported.chunk_errors);
        let chunk = imported
            .chunks
            .iter()
            .find(|c| c.dim == DimId::Overworld)
            .expect("overworld chunk reimported");
        assert_eq!(chunk.edits[&local_idx(1, 70, 2)], stone);
        assert_eq!(chunk.dropped_sections, 0);
        assert_eq!(chunk.unknown_states, 0);
        let _ = std::fs::remove_dir_all(&out);
    }

    #[test]
    fn anvil_export_26_3_stamps_data_version_5023_and_reimports() {
        let _guard = world_guard();
        let key = (DimId::Overworld, 0i32, 0i32);
        let _fixture = AnvilWorldFixture::capture(&[]);
        let stone = name_to_state("stone").expect("stone state");
        {
            let mut w = world().lock().unwrap_or_else(|e| e.into_inner());
            w.clear();
            w.insert(key, HashMap::from([(local_idx(1, 70, 2), stone)]));
        }
        let out = anvil_tmp_dir("world5023");
        let report = export_anvil_world_with_options(
            &out,
            &AnvilExportOptions {
                target_data_version: 5023,
            },
        )
        .unwrap();
        assert!(report.level_written);
        assert_eq!(report.chunks_written, 1);
        // level.dat stamps the 26.3 DataVersion and release name.
        let level = read_level_dat_path(&out.join("level.dat")).unwrap();
        assert_eq!(level.data_version(), 5023);
        let version = level.data().get("Version").unwrap();
        assert_eq!(anvil_i32(version, "Id"), Some(5023));
        assert_eq!(anvil_str(version, "Name").as_deref(), Some("26.3"));
        // So does every chunk record.
        let region = read_region_file(&out.join("region").join("r.0.0.mca")).unwrap();
        let record = region.chunks.get(&(0, 0)).unwrap();
        assert_eq!(anvil_i32(&record.nbt, "DataVersion"), Some(5023));
        // Reimport: 5023 classifies Compatible (a known supported target, not
        // future-unknown), the blocks decode, nothing counts as unsupported.
        assert_eq!(
            classify_anvil_data_version(5023),
            AnvilVersionSupport::Compatible
        );
        let imported = import_anvil_world(&out).unwrap();
        assert_eq!(imported.data_version_counts.get(&5023), Some(&1));
        assert_eq!(imported.unsupported_chunks, 0);
        assert!(imported.chunk_errors.is_empty(), "{:?}", imported.chunk_errors);
        let chunk = imported
            .chunks
            .iter()
            .find(|c| c.dim == DimId::Overworld)
            .expect("overworld chunk reimported");
        assert_eq!(chunk.edits[&local_idx(1, 70, 2)], stone);
        let _ = std::fs::remove_dir_all(&out);
    }

    // ------------------------------------------ live persistence bridge ----

    /// Every regular file under `dir`, recursively — used to prove a save
    /// leaves no staged bytes behind.
    fn p4a_files_under(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
        let mut out = Vec::new();
        let mut stack = vec![dir.to_path_buf()];
        while let Some(d) = stack.pop() {
            for entry in std::fs::read_dir(&d).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    stack.push(path);
                } else {
                    out.push(path);
                }
            }
        }
        out
    }

    #[test]
    fn p4a_anvil_world_dir_from_env_arms_live_storage_mode() {
        let _guard = crate::world_guard();
        std::env::remove_var("MEGU_ANVIL_WORLD_DIR");
        assert_eq!(anvil_world_dir_from_env(), None);
        assert_eq!(anvil_storage_mode(), AnvilStorageMode::Disabled);
        std::env::set_var("MEGU_ANVIL_WORLD_DIR", "   ");
        assert_eq!(anvil_world_dir_from_env(), None, "blank is unset");
        std::env::set_var(
            "MEGU_ANVIL_WORLD_DIR",
            " /data/tmp/p4a-live-world ",
        );
        assert_eq!(
            anvil_world_dir_from_env(),
            Some(std::path::PathBuf::from("/data/tmp/p4a-live-world")),
            "surrounding whitespace is trimmed"
        );
        assert_eq!(anvil_storage_mode(), AnvilStorageMode::Live);
        std::env::remove_var("MEGU_ANVIL_WORLD_DIR");
        assert_eq!(anvil_storage_mode(), AnvilStorageMode::Disabled);
    }

    #[test]
    fn p4a_live_anvil_save_then_load_roundtrips_three_dimensions() {
        let _guard = world_guard();
        let _fixture = AnvilWorldFixture::capture(&[]);
        let grass = name_to_state("grass_block").expect("grass_block state");
        let netherrack = name_to_state("netherrack").expect("netherrack state");
        let end_stone = name_to_state("end_stone").expect("end_stone state");
        {
            let mut w = world().lock().unwrap_or_else(|e| e.into_inner());
            w.clear();
            // Negative chunk coords exercise the r.-1.0 / r.0.-1 naming too.
            w.insert(
                (DimId::Overworld, 0i32, 0i32),
                HashMap::from([(local_idx(1, 70, 2), grass)]),
            );
            w.insert(
                (DimId::Nether, -1i32, 0i32),
                HashMap::from([(local_idx(-2, 40, 3), netherrack)]),
            );
            w.insert(
                (DimId::End, 0i32, -1i32),
                HashMap::from([(local_idx(4, 50, -5), end_stone)]),
            );
        }
        WORLD_SEED.store(0x0123_4567_89ab_cdef, Ordering::Relaxed);
        *WORLD_SPAWN.lock().unwrap_or_else(|e| e.into_inner()) = (8.5, 65.0, -3.5, 90.0);
        WORLD_TIME.store(77_000, Ordering::Relaxed);

        let dir = anvil_tmp_dir("live-rt");
        let save = live_anvil_save_dir(&dir).unwrap();
        assert!(save.level_written);
        assert!(!save.backup_rotated, "first save has nothing to rotate");
        assert_eq!(save.chunks_written, 3);
        assert_eq!(save.region_files, 3);
        for path in [
            dir.join("level.dat"),
            dir.join("region").join("r.0.0.mca"),
            dir.join("DIM-1").join("region").join("r.-1.0.mca"),
            dir.join("DIM1").join("region").join("r.0.-1.mca"),
        ] {
            assert!(path.is_file(), "missing {}", path.display());
        }

        // Wipe live state, then the load must bring it all back.
        world().lock().unwrap_or_else(|e| e.into_inner()).clear();
        WORLD_SEED.store(0, Ordering::Relaxed);
        *WORLD_SPAWN.lock().unwrap_or_else(|e| e.into_inner()) = (0.5, 0.0, 0.5, 0.0);
        WORLD_TIME.store(0, Ordering::Relaxed);
        let load = live_anvil_load_dir(&dir).unwrap();
        assert!(!load.initialized);
        assert!(load.level_loaded);
        assert_eq!(load.region_files, 3);
        assert_eq!(load.chunks_loaded, 3);
        assert!(load.chunk_errors.is_empty(), "{:?}", load.chunk_errors);
        assert_eq!(load.unsupported_chunks, 0);
        assert_eq!(load.dropped_sections, 0);
        assert_eq!(overlay_block_dim(DimId::Overworld, 1, 70, 2), Some(grass));
        assert_eq!(overlay_block_dim(DimId::Nether, -2, 40, 3), Some(netherrack));
        assert_eq!(overlay_block_dim(DimId::End, 4, 50, -5), Some(end_stone));
        assert_eq!(WORLD_SEED.load(Ordering::Relaxed), 0x0123_4567_89ab_cdef);
        assert_eq!(*WORLD_SPAWN.lock().unwrap_or_else(|e| e.into_inner()), (8.5, 65.0, -3.5, 90.0));
        assert_eq!(WORLD_TIME.load(Ordering::Relaxed), 77_000);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn p4a_live_anvil_load_initializes_empty_dir_then_reloads() {
        let _guard = world_guard();
        let _fixture = AnvilWorldFixture::capture(&[]);
        let dir = anvil_tmp_dir("live-init").join("fresh-world");
        // The directory does not exist yet: open_or_init stamps a level.dat.
        let first = live_anvil_load_dir(&dir).unwrap();
        assert!(first.initialized);
        assert!(!first.level_loaded);
        assert_eq!(first.chunks_loaded, 0);
        let level = read_level_dat_path(&dir.join("level.dat")).unwrap();
        assert_eq!(level.data_version(), ANVIL_DATA_VERSION);
        assert_eq!(level.level_name(), "fresh-world");
        // A level.dat alone also counts as data: a second open is a real load.
        let second = live_anvil_load_dir(&dir).unwrap();
        assert!(!second.initialized);
        assert!(second.level_loaded);
        assert_eq!(second.chunks_loaded, 0);
        let _ = std::fs::remove_dir_all(anvil_tmp_dir("live-init").as_path());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn p4a_live_anvil_save_rotates_level_dat_old_and_leaves_no_staging_files() {
        let _guard = world_guard();
        let _fixture = AnvilWorldFixture::capture(&[]);
        let stone = name_to_state("stone").expect("stone state");
        {
            let mut w = world().lock().unwrap_or_else(|e| e.into_inner());
            w.clear();
            w.insert(
                (DimId::Overworld, 0i32, 0i32),
                HashMap::from([(local_idx(3, 64, 3), stone)]),
            );
        }
        let dir = anvil_tmp_dir("live-rot");
        let first = live_anvil_save_dir(&dir).unwrap();
        assert!(first.level_written && !first.backup_rotated);
        assert!(dir.join("level.dat").is_file());
        assert!(!dir.join("level.dat_old").exists(), "no backup on first save");

        let second = live_anvil_save_dir(&dir).unwrap();
        assert!(second.backup_rotated, "second save must rotate");
        // level.dat_old holds a complete prior level.dat: it parses.
        assert!(read_level_dat_path(&dir.join("level.dat_old")).is_ok());
        let third = live_anvil_save_dir(&dir).unwrap();
        assert!(third.backup_rotated, "every later save rotates too");

        // No staged bytes survive anywhere under the world directory.
        for file in p4a_files_under(&dir) {
            let name = file.file_name().unwrap().to_string_lossy();
            assert!(
                !name.ends_with(".tmp") && !name.ends_with("_new"),
                "stray staging file {}",
                file.display()
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn p4a_live_anvil_save_merges_into_region_preserving_clean_chunks() {
        let _guard = world_guard();
        let _fixture = AnvilWorldFixture::capture(&[]);
        let stone = name_to_state("stone").expect("stone state");
        let dirt = name_to_state("dirt").expect("dirt state");
        {
            let mut w = world().lock().unwrap_or_else(|e| e.into_inner());
            w.clear();
            w.insert(
                (DimId::Overworld, 0i32, 0i32),
                HashMap::from([(local_idx(1, 70, 1), stone)]),
            );
            w.insert(
                (DimId::Overworld, 1i32, 0i32),
                HashMap::from([(local_idx(17, 65, 1), dirt)]),
            );
        }
        let dir = anvil_tmp_dir("live-merge");
        live_anvil_save_dir(&dir).unwrap();

        // Chunk (1,0) is clean this cycle — dropped from the overlay — while
        // chunk (0,0) gains a new edit. The merge must keep (1,0)'s record.
        {
            let mut w = world().lock().unwrap_or_else(|e| e.into_inner());
            w.remove(&(DimId::Overworld, 1, 0));
            w.get_mut(&(DimId::Overworld, 0, 0))
                .unwrap()
                .insert(local_idx(2, 71, 2), dirt);
        }
        let save = live_anvil_save_dir(&dir).unwrap();
        assert_eq!(save.chunks_written, 1, "only the dirty chunk re-encodes");

        let region = read_region_file(&dir.join("region").join("r.0.0.mca")).unwrap();
        assert!(region.chunks.contains_key(&(0, 0)));
        assert!(region.chunks.contains_key(&(1, 0)), "clean chunk preserved");
        let clean = anvil_chunk_from_nbt(&region.chunks[&(1, 0)].nbt, (1, 0)).unwrap();
        assert_eq!(
            anvil_chunk_edits(&clean, DimId::Overworld)[&local_idx(17, 65, 1)],
            dirt,
            "untouched chunk keeps its first-save contents"
        );
        let dirty = anvil_chunk_from_nbt(&region.chunks[&(0, 0)].nbt, (0, 0)).unwrap();
        let edits = anvil_chunk_edits(&dirty, DimId::Overworld);
        assert_eq!(edits[&local_idx(1, 70, 1)], stone);
        assert_eq!(edits[&local_idx(2, 71, 2)], dirt, "new edit lands");

        // An mca-bearing directory without level.dat is still world data: a
        // load imports the chunks instead of reinitializing over them.
        std::fs::remove_file(dir.join("level.dat")).unwrap();
        world().lock().unwrap_or_else(|e| e.into_inner()).clear();
        let load = live_anvil_load_dir(&dir).unwrap();
        assert!(!load.initialized, "region data alone means this is not fresh");
        assert!(!load.level_loaded);
        assert_eq!(load.chunks_loaded, 2);
        assert_eq!(overlay_block_dim(DimId::Overworld, 17, 65, 1), Some(dirt));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn fix_save_playerdata_anvil_roundtrip_restores_every_field() {
        if test_support::isolated_world_test() {
            return;
        }
        let _guard = world_guard();
        test_support::require_vanilla_data();
        let dir = anvil_tmp_dir("playerdata");
        let uuid = [0xABu8; 16];
        let sword = name_to_item("minecraft:iron_sword").expect("iron_sword item");
        let apple = name_to_item("minecraft:apple").expect("apple item");
        let mut inventory = HashMap::new();
        inventory.insert(36i16, (sword, 1u8, Vec::new()));
        inventory.insert(10i16, (apple, 7u8, Vec::new()));
        let save = PlayerSave {
            position: (DimId::Nether, 128.5, 70.25, -256.5, 90.0, -10.0),
            inventory,
            ender_chest: HashMap::from([(0i16, (sword, 2u8, Vec::new()))]),
            held: 0,
            health: 13.5,
            absorption: 4.0,
            food: 14,
            saturation: 2.5,
            exhaustion: 1.25,
            xp_level: 7,
            xp_progress: 0.4,
            xp_total: 77,
            gamemode: 0,
            effects: vec![(1, 0, 400)],
            respawn: Some(SpawnTarget {
                kind: SpawnTargetKind::Bed,
                dim: DimId::Overworld,
                position: (10.0, 64.0, 10.0),
                angle: 45.0,
                forced: true,
            }),
        };
        saved_players()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(uuid, save.clone());

        let written = live_anvil_save_dir(&dir).unwrap();
        assert_eq!(written.players_written, 1);
        let file = dir.join("playerdata").join(playerdata_file_name(&uuid));
        assert_eq!(
            playerdata_file_name(&uuid),
            "abababab-abab-abab-abab-abababababab.dat"
        );
        assert!(file.is_file(), "playerdata/<uuid>.dat exists");
        assert!(!file.with_extension("tmp").exists());

        // Fresh state: the reload must restore every field from disk alone.
        saved_players()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        let loaded = live_anvil_load_dir(&dir).unwrap();
        assert_eq!(loaded.players_loaded, 1);
        let restored = saved_players()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&uuid)
            .cloned()
            .expect("player restored");
        assert_eq!(restored.position.0, DimId::Nether);
        assert_eq!(restored.position.1, 128.5);
        assert_eq!(restored.position.2, 70.25);
        assert_eq!(restored.position.3, -256.5);
        assert_eq!(restored.position.4, 90.0);
        assert_eq!(restored.position.5, -10.0);
        assert_eq!(restored.gamemode, 0, "survival gamemode");
        assert_eq!(restored.health, 13.5);
        assert_eq!(restored.absorption, 4.0);
        assert_eq!(restored.food, 14);
        assert_eq!(restored.saturation, 2.5);
        assert_eq!(restored.exhaustion, 1.25);
        assert_eq!(restored.xp_level, 7);
        assert_eq!(restored.xp_progress, 0.4);
        assert_eq!(restored.xp_total, 77);
        assert_eq!(restored.held, 0);
        assert_eq!(restored.inventory.get(&36), Some(&(sword, 1, Vec::new())),
            "sword in slot 36");
        assert_eq!(restored.inventory.get(&10), Some(&(apple, 7, Vec::new())));
        assert_eq!(restored.ender_chest.get(&0), Some(&(sword, 2, Vec::new())));
        assert_eq!(restored.effects, vec![(1, 0, 400)], "speed effect");
        let Some(respawn) = restored.respawn else {
            panic!("respawn target restored");
        };
        assert_eq!(respawn.kind, SpawnTargetKind::Bed);
        assert_eq!(respawn.dim, DimId::Overworld);
        assert_eq!(respawn.position, (10.0, 64.0, 10.0));
        assert_eq!(respawn.angle, 45.0);
        assert!(respawn.forced);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn fix_save_playerdata_rejects_corrupt_symlinked_and_traversal_entries() {
        if test_support::isolated_world_test() {
            return;
        }
        let _guard = world_guard();
        let dir = anvil_tmp_dir("playerdata-bad");
        let playerdata = dir.join("playerdata");
        std::fs::create_dir_all(&playerdata).unwrap();
        // Corrupt gzip in a well-named file fails the load closed.
        std::fs::write(
            playerdata.join("aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa.dat"),
            b"not gzip at all",
        )
        .unwrap();
        // A traversal-shaped name is skipped outright.
        std::fs::write(playerdata.join("..%2f..%2fetc%2fpasswd.dat"), b"x").unwrap();
        assert!(load_all_playerdata_dat(&dir).is_err(), "corrupt .dat fails closed");
        assert!(saved_players()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_empty());
        // Remove the corrupt file; the malformed name stays and is skipped.
        std::fs::remove_file(
            playerdata.join("aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa.dat"),
        )
        .unwrap();
        // A malformed uuid name is skipped rather than misfiled.
        std::fs::write(playerdata.join("not-a-uuid.dat"), b"x").unwrap();
        assert_eq!(load_all_playerdata_dat(&dir).unwrap(), 0);
        // A symlinked .dat is skipped rather than followed.
        #[cfg(unix)]
        {
            let outside = dir.join("outside.dat");
            let save = PlayerSave {
                position: (DimId::Overworld, 0.0, 64.0, 0.0, 0.0, 0.0),
                inventory: HashMap::new(),
                ender_chest: HashMap::new(),
                held: 0,
                health: 20.0,
                absorption: 0.0,
                food: 20,
                saturation: 5.0,
                exhaustion: 0.0,
                xp_level: 0,
                xp_progress: 0.0,
                xp_total: 0,
                gamemode: 0,
                effects: Vec::new(),
                respawn: None,
            };
            std::fs::write(&outside, playerdata_to_bytes(&[9; 16], &save).unwrap())
                .unwrap();
            std::os::unix::fs::symlink(
                &outside,
                playerdata.join("99999999-9999-9999-9999-999999999999.dat"),
            )
            .unwrap();
            assert_eq!(load_all_playerdata_dat(&dir).unwrap(), 0);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    // -------------------------- PERF-DISK-SAVE lane tests -----------------

    /// The fingerprint machinery itself: deterministic, tag-shape-sensitive.
    #[test]
    fn perf_disk_save_nbt_fingerprint_is_deterministic_and_sensitive() {
        let a = sample_chunk_nbt(0, 0);
        let b = sample_chunk_nbt(0, 0);
        let c = sample_chunk_nbt(0, 1);
        assert_eq!(anvil_nbt_fingerprint(&a), anvil_nbt_fingerprint(&b));
        assert_ne!(anvil_nbt_fingerprint(&a), anvil_nbt_fingerprint(&c));
        // One changed leaf deep in the tree changes the whole fingerprint.
        let mut d = sample_chunk_nbt(0, 0);
        if let AnvilNbt::Compound(fields) = &mut d {
            fields.insert(
                crate::nbt::NbtString::text("yPos"),
                AnvilNbt::Int(-3),
            );
        }
        assert_ne!(anvil_nbt_fingerprint(&a), anvil_nbt_fingerprint(&d));
        // Byte fingerprints agree across calls and differ on one flipped bit.
        assert_eq!(anvil_bytes_fingerprint(b"payload"), anvil_bytes_fingerprint(b"payload"));
        assert_ne!(anvil_bytes_fingerprint(b"payload"), anvil_bytes_fingerprint(b"payloae"));
    }

    /// Goal 1: a re-save whose serialized payload is unchanged skips zlib +
    /// disk entirely; one block edit invalidates the hash and persists.
    #[test]
    fn perf_disk_save_unchanged_chunk_resave_skips_disk_write() {
        let _guard = world_guard();
        let _fixture = AnvilWorldFixture::capture(&[]);
        let stone = name_to_state("stone").expect("stone state");
        let dirt = name_to_state("dirt").expect("dirt state");
        let key = (DimId::Overworld, 0i32, 0i32);
        {
            let mut w = world().lock().unwrap_or_else(|e| e.into_inner());
            w.clear();
            w.insert(key, HashMap::from([(local_idx(1, 70, 2), stone)]));
        }
        let dir = anvil_tmp_dir("perf-skip");
        let first = live_anvil_save_dir(&dir).unwrap();
        assert_eq!(first.chunks_written, 1);
        assert_eq!(first.region_files, 1);
        let mca = dir.join("region").join("r.0.0.mca");
        let committed = std::fs::read(&mca).unwrap();

        // Second save, identical world: zero chunks re-encoded, zero region
        // bytes committed, and the file is byte-identical afterwards.
        let second = live_anvil_save_dir(&dir).unwrap();
        assert_eq!(second.chunks_written, 0, "unchanged payload must not re-encode");
        assert!(
            second.skipped_unchanged_writes >= 1,
            "the dirty-but-identical chunk counts as a skipped write"
        );
        assert_eq!(
            second.region_bytes_written, 0,
            "a fully skipped region commits no bytes"
        );
        assert_eq!(std::fs::read(&mca).unwrap(), committed);

        // A single changed block breaks the fingerprint and persists cleanly.
        {
            let mut w = world().lock().unwrap_or_else(|e| e.into_inner());
            w.get_mut(&key)
                .unwrap()
                .insert(local_idx(5, 71, 5), dirt);
        }
        let third = live_anvil_save_dir(&dir).unwrap();
        assert_eq!(third.chunks_written, 1);
        assert!(third.region_bytes_written > 0);
        let region = read_region_file(&mca).unwrap();
        let chunk = anvil_chunk_from_nbt(&region.chunks[&(0, 0)].nbt, (0, 0)).unwrap();
        let edits = anvil_chunk_edits(&chunk, DimId::Overworld);
        assert_eq!(edits[&local_idx(1, 70, 2)], stone);
        assert_eq!(edits[&local_idx(5, 71, 5)], dirt, "the edit persisted");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Goal 2: the bounded FIFO drain spreads work across ticks; flush_all
    /// guarantees the full synchronous drain.
    #[test]
    fn perf_disk_save_incremental_queue_bounded_slices_and_flush_all() {
        let mut queue = IncrementalSaveQueue::new(IncrementalSaveBudget {
            max_chunks_per_tick: 7,
            max_bytes_per_tick: usize::MAX,
        });
        for i in 0..100i32 {
            assert!(queue.mark_dirty(DimId::Overworld, i % 10, i / 10, 128));
        }
        assert_eq!(queue.len(), 100);
        // Re-marking a queued chunk keeps its oldest position: no duplicates.
        assert!(!queue.mark_dirty(DimId::Overworld, 0, 0, 256));
        assert_eq!(queue.len(), 100);

        let mut order = Vec::new();
        let mut total = 0usize;
        loop {
            let stats = queue
                .drain_budgeted(|item| {
                    order.push((item.cx, item.cz));
                    Ok::<_, String>(AnvilChunkFlushOutcome {
                        blocks_flushed: 1,
                        bytes_written: 128,
                        skipped_unchanged: false,
                    })
                })
                .unwrap();
            assert!(
                stats.chunks_flushed <= 7,
                "one tick never exceeds max_chunks_per_tick"
            );
            total += stats.chunks_flushed;
            if stats.remaining == 0 {
                break;
            }
        }
        assert_eq!(total, 100);
        // FIFO / oldest-dirty: the first mark flushes first.
        assert_eq!(order.first().copied(), Some((0, 0)));
        assert_eq!(order.len(), 100);

        // Byte budget leg: 300-byte cap admits exactly two 150-byte items.
        let mut queue = IncrementalSaveQueue::new(IncrementalSaveBudget {
            max_chunks_per_tick: usize::MAX,
            max_bytes_per_tick: 300,
        });
        for i in 0..10i32 {
            queue.mark_dirty(DimId::Overworld, i, 0, 150);
        }
        let stats = queue
            .drain_budgeted(|_| {
                Ok::<_, String>(AnvilChunkFlushOutcome {
                    blocks_flushed: 1,
                    bytes_written: 150,
                    skipped_unchanged: false,
                })
            })
            .unwrap();
        assert_eq!(stats.chunks_flushed, 2);
        assert_eq!(stats.bytes_charged, 300);
        assert_eq!(stats.remaining, 8);
        // flush_all drains every queued chunk in one synchronous pass.
        let stats = queue
            .flush_all(|_| {
                Ok::<_, String>(AnvilChunkFlushOutcome {
                    blocks_flushed: 1,
                    bytes_written: 150,
                    skipped_unchanged: false,
                })
            })
            .unwrap();
        assert_eq!(stats.chunks_flushed, 8);
        assert_eq!(stats.remaining, 0);
        assert!(queue.is_empty());
    }

    /// Goal 2 live path: `live_anvil_save_tick` enqueues the dirty set once
    /// and flushes the oldest chunks under the budget; the second tick's
    /// identical payloads all skip.
    #[test]
    fn perf_disk_save_live_tick_drains_queue_and_skips_unchanged() {
        let _guard = world_guard();
        let _fixture = AnvilWorldFixture::capture(&[]);
        let stone = name_to_state("stone").expect("stone state");
        {
            let mut w = world().lock().unwrap_or_else(|e| e.into_inner());
            w.clear();
            for cx in 0..2i32 {
                w.insert(
                    (DimId::Overworld, cx, 0i32),
                    HashMap::from([(local_idx(cx * 16 + 1, 70, 2), stone)]),
                );
            }
        }
        // A tight budget forces multi-tick drainage.
        anvil_incremental_saves()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .budget = IncrementalSaveBudget {
            max_chunks_per_tick: 1,
            max_bytes_per_tick: usize::MAX,
        };
        let dir = anvil_tmp_dir("perf-tick");
        let t1 = live_anvil_save_tick(&dir).unwrap();
        assert_eq!(t1.newly_queued, 2);
        assert_eq!(t1.drain.chunks_flushed, 1);
        assert_eq!(t1.drain.remaining, 1);
        // The still-queued chunk is not re-marked; the chunk flushed last
        // tick re-enters because its overlay edits are still dirty.
        let t2 = live_anvil_save_tick(&dir).unwrap();
        assert_eq!(t2.newly_queued, 1);
        assert_eq!(t2.drain.chunks_flushed, 1);
        // Both chunks are now persisted on disk. Every further drain hits the
        // unchanged-payload fast path: zero bytes committed.
        let mut skipped = 0usize;
        let mut flushed = 0usize;
        let mut bytes = 0u64;
        for _ in 0..4 {
            let t = live_anvil_save_tick(&dir).unwrap();
            assert!(t.drain.chunks_flushed <= 1, "tick respects the budget");
            skipped += t.drain.chunks_skipped_unchanged;
            flushed += t.drain.chunks_flushed;
            bytes += t.drain.bytes_flushed;
        }
        assert!(flushed >= 2, "the queue keeps rotating through dirty chunks");
        assert_eq!(skipped, flushed, "every drain of an unchanged chunk skips");
        assert_eq!(bytes, 0, "skipped chunks commit zero bytes");
        // Restore the default budget for other tests sharing the process.
        anvil_incremental_saves()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .budget = IncrementalSaveBudget::default();
        // A full save after partial queue work drains everything and clears
        // the queue.
        let save = live_anvil_save_dir(&dir).unwrap();
        assert!(save.chunks_written <= 2);
        assert_eq!(
            anvil_incremental_saves()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .len(),
            0
        );
        let region = read_region_file(&dir.join("region").join("r.0.0.mca")).unwrap();
        assert!(region.chunks.contains_key(&(0, 0)));
        assert!(region.chunks.contains_key(&(1, 0)));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Goal 3: torn `.tmp` staging and interrupted `level.dat` rotation are
    /// folded back on reopen without clobbering the committed file.
    #[test]
    fn perf_disk_save_corrupt_tmp_staging_never_clobbers_committed_file() {
        let dir = anvil_tmp_dir("perf-staging");
        let mut region = AnvilRegion::default();
        region.chunks.insert(
            (0, 0),
            AnvilChunkRecord {
                timestamp: 0xbeef,
                compression: 2,
                nbt: sample_chunk_nbt(0, 0),
            },
        );
        let mca = dir.join("r.0.0.mca");
        write_region_file(&mca, &region).unwrap();
        let committed = std::fs::read(&mca).unwrap();

        // A crash that died mid-`write_all` leaves a torn `.tmp` sibling.
        let tmp = dir.join("r.0.0.mca.tmp");
        std::fs::write(&tmp, b"torn-partial-write-not-a-region").unwrap();
        let swept = anvil_recover_world_staging(&dir).unwrap();
        assert!(swept >= 1, "the orphan tmp is swept");
        assert!(!tmp.exists());
        assert_eq!(
            std::fs::read(&mca).unwrap(),
            committed,
            "the committed region file is never clobbered by staging debris"
        );
        assert_eq!(read_region_file(&mca).unwrap(), region);

        // Interrupted level.dat rotation: `level.dat` + `level.dat_new`
        // means the staged copy never took the name — it is deleted.
        let level = anvil_level_dat_from_state("world", ANVIL_DATA_VERSION);
        write_level_dat_path(&dir.join("level.dat"), &level).unwrap();
        let staged = dir.join("level.dat_new");
        std::fs::write(&staged, b"interrupted rotation").unwrap();
        anvil_recover_world_staging(&dir).unwrap();
        assert!(!staged.exists());
        assert!(read_level_dat_path(&dir.join("level.dat")).is_ok());

        // Crash between the two renames: staged fsynced bytes adopt the live
        // name (they were fully written before the rename began).
        std::fs::rename(dir.join("level.dat"), &staged).unwrap();
        anvil_recover_world_staging(&dir).unwrap();
        assert!(read_level_dat_path(&dir.join("level.dat")).is_ok());
        assert!(!staged.exists());

        // A lone `level.dat_old` is a normal post-save artifact — recovery
        // leaves it untouched and does not resurrect a live `level.dat`.
        std::fs::rename(dir.join("level.dat"), dir.join("level.dat_old")).unwrap();
        let old_bytes = std::fs::read(dir.join("level.dat_old")).unwrap();
        anvil_recover_world_staging(&dir).unwrap();
        assert!(!dir.join("level.dat").exists());
        assert_eq!(std::fs::read(dir.join("level.dat_old")).unwrap(), old_bytes);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Goal 3 read-side: one corrupt slot degrades to a diagnostic while its
    /// valid neighbor still decodes; the strict reader stays fatal.
    #[test]
    fn perf_disk_save_tolerant_region_read_keeps_neighbors_of_corrupt_slot() {
        let dir = anvil_tmp_dir("perf-tolerant");
        let mut region = AnvilRegion::default();
        for slot in [(0i32, 0i32), (1, 0)] {
            region.chunks.insert(
                slot,
                AnvilChunkRecord {
                    timestamp: 7,
                    compression: 2,
                    nbt: sample_chunk_nbt(slot.0, slot.1),
                },
            );
        }
        let mca = dir.join("r.0.0.mca");
        write_region_file(&mca, &region).unwrap();
        // Corrupt chunk (0,0)'s location entry: point it inside the header.
        let mut file = std::fs::read(&mca).unwrap();
        file[0] = 0;
        file[1] = 0;
        file[2] = 0; // offset 0
        file[3] = 4; // sectors 4 -> "offset < 2" corruption
        std::fs::write(&mca, &file).unwrap();

        assert!(read_region_file(&mca).is_err(), "strict read fails closed");
        let (recovered, errors) = read_region_file_tolerant(&mca).unwrap();
        assert_eq!(errors.len(), 1, "exactly the corrupt slot is reported");
        assert!(errors[0].contains("slot 0"));
        assert!(!recovered.chunks.contains_key(&(0, 0)));
        assert_eq!(
            recovered.chunks.get(&(1, 0)).unwrap(),
            &region.chunks[&(1, 0)],
            "the corrupt slot's valid neighbor still decodes"
        );
        // Header-level corruption is still fatal in both modes.
        std::fs::write(dir.join("r.9.9.mca"), [0u8; 100]).unwrap();
        assert!(read_region_file_tolerant(&dir.join("r.9.9.mca")).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

}
