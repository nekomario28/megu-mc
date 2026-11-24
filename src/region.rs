// SPDX-License-Identifier: LGPL-3.0-or-later
// ----- Anvil region persistence: dirty content-hash skip, incremental save
// budget, and crash-recovery staging sweep -----
//
// PERF-DISK-SAVE: the pieces below sit next to the `.mca` codec in
// `src/anvil_io.rs` (this file is `include!`d into that translation unit):
//
// * `anvil_nbt_fingerprint` — a deterministic 64-bit content hash over a tag's
//   canonical serialization order, used by `anvil_persisted_hashes` to skip a
//   re-save whose payload is identical to what is already on disk (no zlib
//   re-encode, no sector rewrite).
// * `IncrementalSaveBudget` / `IncrementalSaveQueue` — a FIFO oldest-dirty
//   queue of pending chunk flushes with per-tick chunk/byte caps, so an
//   autosave spreads world-wide writes across ticks instead of stalling the
//   loop for the full burst; `flush_all` is the guaranteed synchronous drain
//   for `/save-all` and shutdown.
// * `anvil_sync_parent_dir` — the directory fsync an atomic rename needs to
//   survive a power loss (best-effort on POSIX, no-op elsewhere).
// * `anvil_recover_world_staging` — reopen-time recovery for an interrupted
//   `level.dat` rotation plus an orphan `.tmp` sweep of the staging dirs.
// * `read_region_file_tolerant` — the lenient half of the region reader:
//   header-level faults stay fatal, but one corrupt slot degrades to a
//   diagnostic while its valid neighbors still decode.

// ---------- deterministic 64-bit content fingerprints ----------
//
// FNV-1a over a canonical traversal of the tag (compound keys iterate in
// `BTreeMap` order, the same order `crate::nbt::write_named` serializes), with
// a splitmix64 avalanche finalizer so near-identical payloads differ in every
// output bit. Two equal trees always fingerprint equal; a single flipped
// payload bit changes the fingerprint with overwhelming probability.

/// Splitmix64 avalanche: decorrelates the FNV state into every output bit.
fn anvil_fingerprint_final(mut z: u64) -> u64 {
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

fn anvil_nbt_fingerprint_into(tag: &AnvilNbt, h: &mut megu_core::hash::Fnv1a64) {
    // Lengths and integers feed in big-endian order, the same order the NBT
    // wire itself carries; `Fnv1a64::update_*` helpers are little-endian, so
    // every integer goes through `update` on `to_be_bytes` explicitly.
    h.update_u8(tag.id());
    match tag {
        AnvilNbt::End => {}
        AnvilNbt::Byte(v) => h.update_u8(*v as u8),
        AnvilNbt::Short(v) => h.update(&v.to_be_bytes()),
        AnvilNbt::Int(v) => h.update(&v.to_be_bytes()),
        AnvilNbt::Long(v) => h.update(&v.to_be_bytes()),
        AnvilNbt::Float(v) => h.update(&v.to_be_bytes()),
        AnvilNbt::Double(v) => h.update(&v.to_be_bytes()),
        AnvilNbt::ByteArray(v) => {
            h.update(&(v.len() as u32).to_be_bytes());
            h.update(v);
        }
        AnvilNbt::String(s) => {
            h.update(&(s.0.len() as u32).to_be_bytes());
            for &unit in &s.0 {
                h.update(&unit.to_be_bytes());
            }
        }
        AnvilNbt::List(kind, items) => {
            h.update_u8(*kind);
            h.update(&(items.len() as u32).to_be_bytes());
            for item in items {
                anvil_nbt_fingerprint_into(item, h);
            }
        }
        AnvilNbt::Compound(fields) => {
            h.update(&(fields.len() as u32).to_be_bytes());
            for (key, value) in fields {
                h.update(&(key.0.len() as u32).to_be_bytes());
                for &unit in &key.0 {
                    h.update(&unit.to_be_bytes());
                }
                anvil_nbt_fingerprint_into(value, h);
            }
        }
        AnvilNbt::IntArray(v) => {
            h.update(&(v.len() as u32).to_be_bytes());
            for &x in v {
                h.update(&x.to_be_bytes());
            }
        }
        AnvilNbt::LongArray(v) => {
            h.update(&(v.len() as u32).to_be_bytes());
            for &x in v {
                h.update(&x.to_be_bytes());
            }
        }
    }
}

/// The content fingerprint of one chunk/playerdata NBT payload — the value
/// `anvil_persisted_hashes` compares a would-be save against.
#[allow(dead_code)]
fn anvil_nbt_fingerprint(tag: &AnvilNbt) -> u64 {
    let mut h = megu_core::hash::Fnv1a64::new();
    anvil_nbt_fingerprint_into(tag, &mut h);
    anvil_fingerprint_final(h.finish())
}

/// The same fingerprint over an already-serialized byte payload (a gzip
/// member, a framed file) — for callers whose content arrives as bytes.
#[allow(dead_code)]
fn anvil_bytes_fingerprint(bytes: &[u8]) -> u64 {
    anvil_fingerprint_final(megu_core::hash::fnv1a64(bytes))
}

/// The identity of one persisted chunk record: world directory + dimension +
/// chunk coords. The directory belongs in the key — a fingerprint committed
/// under one world must never skip a write into another.
type AnvilPersistedKey = (std::path::PathBuf, DimId, i32, i32);

/// `AnvilPersistedKey -> fingerprint of the chunk payload last committed to
/// that file (or lazily verified as already on disk)`. Seeded from the
/// on-disk record on first touch and updated only after a successful atomic
/// write, so the map always describes durable content.
#[allow(dead_code)]
fn anvil_persisted_hashes() -> &'static LockedMap<AnvilPersistedKey, u64> {
    static HASHES: OnceLock<LockedMap<AnvilPersistedKey, u64>> = OnceLock::new();
    HASHES.get_or_init(|| Mutex::new(HashMap::new()))
}

/// The same bookkeeping for `playerdata/<uuid>.dat` records, keyed by world
/// directory + uuid.
#[allow(dead_code)]
fn anvil_playerdata_hashes() -> &'static LockedMap<(std::path::PathBuf, [u8; 16]), u64> {
    static HASHES: OnceLock<LockedMap<(std::path::PathBuf, [u8; 16]), u64>> = OnceLock::new();
    HASHES.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Whether `fresh` is provably identical to the payload already committed
/// for `key`: the in-memory fingerprint from the last successful write or
/// lazy verification, or a fingerprint of `on_disk` computed on first touch
/// (which is then cached so the comparison costs a map lookup from then on).
/// `None` for `on_disk` means the region file holds no record for this chunk
/// — a write is always required.
#[allow(dead_code)]
fn anvil_payload_unchanged(
    key: AnvilPersistedKey,
    fresh_hash: u64,
    on_disk: Option<&AnvilNbt>,
    hashes: &mut HashMap<AnvilPersistedKey, u64>,
) -> bool {
    let Some(on_disk) = on_disk else {
        return false; // nothing on disk for this chunk: a write is required
    };
    let disk_hash = *hashes
        .entry(key)
        .or_insert_with(|| anvil_nbt_fingerprint(on_disk));
    disk_hash == fresh_hash
}

// ---------- incremental autosave budget (IncrementalSaveQueue) ----------

/// Per-tick flush caps: at most `max_chunks_per_tick` queued chunks and
/// `max_bytes_per_tick` estimated payload bytes per incremental pass. A chunk
/// that alone exceeds the byte leg still flushes — the caps bound batch work,
/// never starve a single write.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[allow(dead_code)]
struct IncrementalSaveBudget {
    max_chunks_per_tick: usize,
    max_bytes_per_tick: usize,
}

#[allow(dead_code)]
impl IncrementalSaveBudget {
    /// No caps: the `/save-all` / shutdown drain.
    const UNLIMITED: IncrementalSaveBudget = IncrementalSaveBudget {
        max_chunks_per_tick: usize::MAX,
        max_bytes_per_tick: usize::MAX,
    };
}

impl Default for IncrementalSaveBudget {
    /// Smooth autosave defaults: a few chunks and a few MiB per tick spread a
    /// multi-hundred-chunk burst across seconds instead of one 100-400ms
    /// stall, while a modest world still drains in one pass.
    fn default() -> IncrementalSaveBudget {
        IncrementalSaveBudget {
            max_chunks_per_tick: 8,
            max_bytes_per_tick: 8 << 20,
        }
    }
}

/// One queued dirty-chunk flush. `est_bytes` is the caller's estimate of the
/// serialized payload — the byte-budget leg charges against it, so it should
/// track the payload's order of magnitude, not be exact.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[allow(dead_code)]
struct QueuedChunkSave {
    dim: DimId,
    cx: i32,
    cz: i32,
    est_bytes: usize,
}

#[allow(dead_code)]
impl QueuedChunkSave {
    fn key(self) -> DimChunkKey {
        (self.dim, self.cx, self.cz)
    }
}

/// What the flush closure reports for one drained chunk.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[allow(dead_code)]
struct AnvilChunkFlushOutcome {
    /// Overlay cells the flush preserved (written or already identical on
    /// disk — either way the edits are now durable).
    blocks_flushed: usize,
    /// Region-file bytes actually committed this flush (0 when skipped).
    bytes_written: u64,
    /// The on-disk payload already held this exact content: nothing was
    /// re-encoded or rewritten.
    skipped_unchanged: bool,
}

/// What one drain pass spent and left.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[allow(dead_code)]
struct IncrementalDrainStats {
    /// Queued chunks popped and flushed (skipped included).
    chunks_flushed: usize,
    /// Of those, the ones whose on-disk payload already matched: no zlib
    /// re-encode, no disk write.
    chunks_skipped_unchanged: usize,
    /// Region-file bytes the flush reports actually committing.
    bytes_flushed: u64,
    /// Estimated payload bytes charged against the byte budget leg.
    bytes_charged: u64,
    /// Chunks still queued after the pass.
    remaining: usize,
}

/// A deterministic FIFO/oldest-dirty queue of pending chunk flushes under a
/// per-tick budget. `mark_dirty` is idempotent — a chunk already queued keeps
/// its oldest position (only its estimate refreshes), so re-dirtying cannot
/// starve it behind newer work. `drain_budgeted` pops the oldest entries
/// until either budget leg trips; `flush_all` drains the whole queue — the
/// `/save-all` and graceful-shutdown guarantee.
#[allow(dead_code)]
struct IncrementalSaveQueue {
    budget: IncrementalSaveBudget,
    pending: std::collections::VecDeque<QueuedChunkSave>,
    queued: std::collections::HashSet<DimChunkKey>,
}

#[allow(dead_code)]
impl IncrementalSaveQueue {
    fn new(budget: IncrementalSaveBudget) -> IncrementalSaveQueue {
        IncrementalSaveQueue {
            budget,
            pending: std::collections::VecDeque::new(),
            queued: std::collections::HashSet::new(),
        }
    }

    fn len(&self) -> usize {
        self.pending.len()
    }

    fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }

    /// Enqueue `(dim, cx, cz)` for a later flush. Returns `true` when the
    /// chunk was not already pending; a re-mark keeps the oldest position.
    fn mark_dirty(&mut self, dim: DimId, cx: i32, cz: i32, est_bytes: usize) -> bool {
        if !self.queued.insert((dim, cx, cz)) {
            return false;
        }
        self.pending.push_back(QueuedChunkSave {
            dim,
            cx,
            cz,
            est_bytes,
        });
        true
    }

    /// Forget every pending entry without flushing — used when a full merged
    /// save just persisted the same chunks through another path.
    fn drain_pending(&mut self) -> usize {
        let n = self.pending.len();
        self.pending.clear();
        self.queued.clear();
        n
    }

    /// Flush the oldest pending chunks until a budget leg trips. A failed
    /// flush keeps the chunk at the head of the queue so the error aborts the
    /// pass without losing the entry — it is the next thing the next pass
    /// retries.
    fn drain_budgeted(
        &mut self,
        mut flush: impl FnMut(&QueuedChunkSave) -> Result<AnvilChunkFlushOutcome, String>,
    ) -> Result<IncrementalDrainStats, String> {
        let mut stats = IncrementalDrainStats::default();
        while let Some(front) = self.pending.front() {
            if stats.chunks_flushed >= self.budget.max_chunks_per_tick {
                break;
            }
            let est = front.est_bytes as u64;
            if stats.chunks_flushed > 0
                && stats.bytes_charged.saturating_add(est)
                    > self.budget.max_bytes_per_tick as u64
            {
                break;
            }
            let item = self.pending.pop_front().expect("front checked");
            match flush(&item) {
                Ok(outcome) => {
                    self.queued.remove(&item.key());
                    stats.chunks_flushed += 1;
                    stats.chunks_skipped_unchanged +=
                        usize::from(outcome.skipped_unchanged);
                    stats.bytes_charged = stats.bytes_charged.saturating_add(est);
                    stats.bytes_flushed =
                        stats.bytes_flushed.saturating_add(outcome.bytes_written);
                }
                Err(e) => {
                    self.pending.push_front(item);
                    return Err(e);
                }
            }
        }
        stats.remaining = self.pending.len();
        Ok(stats)
    }

    /// The guaranteed synchronous drain: `/save-all`, graceful shutdown —
    /// every queued chunk flushes before this returns.
    fn flush_all(
        &mut self,
        flush: impl FnMut(&QueuedChunkSave) -> Result<AnvilChunkFlushOutcome, String>,
    ) -> Result<IncrementalDrainStats, String> {
        let held = self.budget;
        self.budget = IncrementalSaveBudget::UNLIMITED;
        let stats = self.drain_budgeted(flush);
        self.budget = held;
        stats
    }
}

/// The process-wide incremental queue the live autosave ticks drain through.
#[allow(dead_code)]
fn anvil_incremental_saves() -> &'static Mutex<IncrementalSaveQueue> {
    static QUEUE: OnceLock<Mutex<IncrementalSaveQueue>> = OnceLock::new();
    QUEUE.get_or_init(|| Mutex::new(IncrementalSaveQueue::new(IncrementalSaveBudget::default())))
}

// ---------- crash-recovery: dir fsync + reopen staging sweep ----------

/// Best-effort `fsync` of the directory holding `path`. A staged write that is
/// `sync_all`ed and renamed can still be rolled back by a crash if the
/// directory entry itself never reached disk; on POSIX this pins it, and where
/// opening a directory as a file fails (Windows) the write already survives
/// the rename, so failure is a safe no-op.
#[allow(dead_code)]
fn anvil_sync_parent_dir(path: &std::path::Path) {
    let Some(parent) = path.parent() else {
        return;
    };
    let parent = if parent.as_os_str().is_empty() {
        std::path::Path::new(".")
    } else {
        parent
    };
    let _ = std::fs::File::open(parent).and_then(|f| f.sync_all());
}

/// Fold an interrupted `level.dat` rotation back into one live file and sweep
/// orphaned `.tmp` staging files out of the world directory and its region /
/// playerdata subdirectories. Returns how many stray entries were folded or
/// removed.
///
/// The rotation's intermediate states recover deterministically:
/// `level.dat` + `level.dat_new` — the staged copy never made the live name,
/// so it is deleted; no `level.dat` but a `level.dat_new` — the crash landed
/// between the backup rename and the live rename, so the fully fsynced staged
/// file is adopted. `level.dat_old` alone is *never* adopted: it is a normal
/// post-save artifact (vanilla keeps it as the previous generation), so its
/// presence cannot distinguish an interrupted rotation from a deliberately
/// absent `level.dat` — resurrecting it would silently restore stale world
/// state. A `.mca.tmp`/`.dat.tmp` orphan is *never* adopted either: a torn
/// staging file is indistinguishable from a complete one without a full
/// decode, and the region/playerdata write it carried is simply re-done by
/// the next save — deletion is the fail-closed choice.
#[allow(dead_code)]
fn anvil_recover_world_staging(dir: &std::path::Path) -> Result<usize, String> {
    let mut recovered = 0usize;
    if !dir.is_dir() {
        return Ok(0);
    }
    let live = dir.join("level.dat");
    let staged = dir.join("level.dat_new");
    if staged.is_file() {
        if live.is_file() {
            std::fs::remove_file(&staged).map_err(|e| {
                format!("cannot remove stale {}: {e}", staged.display())
            })?;
        } else {
            std::fs::rename(&staged, &live).map_err(|e| {
                format!(
                    "cannot restore {} over {}: {e}",
                    staged.display(),
                    live.display()
                )
            })?;
        }
        recovered += 1;
    }
    for rel in ["", "region", "DIM-1/region", "DIM1/region", "playerdata"] {
        let scan = if rel.is_empty() {
            dir.to_path_buf()
        } else {
            dir.join(rel)
        };
        let Ok(entries) = std::fs::read_dir(&scan) else {
            continue;
        };
        for entry in entries.flatten() {
            let is_tmp = entry
                .file_name()
                .to_str()
                .is_some_and(|name| name.ends_with(".tmp"));
            let is_file = entry.file_type().is_ok_and(|t| t.is_file());
            if is_tmp && is_file {
                std::fs::remove_file(entry.path()).map_err(|e| {
                    format!("cannot remove stale {}: {e}", entry.path().display())
                })?;
                recovered += 1;
            }
        }
    }
    Ok(recovered)
}

// ---------- tolerant region read (import side of fail-closed) ----------

/// Decode one populated location-table slot. `Ok(None)` is an empty slot;
/// `Err` carries the same message `read_region_file` would have aborted with.
type RegionChunkRecord = ((i32, i32), AnvilChunkRecord);
type RegionFileSelection = (Option<Vec<u8>>, HashMap<(i32, i32), AnvilChunkRecord>);

/// The location-table bounds of one populated slot: `(offset, sectors, byte start)`.
/// This is the structural half of `anvil_region_slot` — it never touches the
/// compressed payload, so the raw-update path can copy an untouched run without
/// paying for inflate/NBT work it does not need.
fn anvil_region_run(
    file: &[u8],
    slot: usize,
    path: &std::path::Path,
) -> Result<Option<(usize, usize, usize)>, String> {
    let le = &file[slot * 4..slot * 4 + 4];
    let offset = (usize::from(le[0]) << 16) | (usize::from(le[1]) << 8) | usize::from(le[2]);
    let sectors = usize::from(le[3]);
    if offset == 0 && sectors == 0 {
        return Ok(None);
    }
    // The two header sectors are not legal payload space, and the recorded
    // run must fit inside the file.
    if offset < 2 || sectors == 0 {
        return Err(format!(
            "invalid location entry {offset}+{sectors} in {}",
            path.display()
        ));
    }
    let start = offset
        .checked_mul(ANVIL_SECTOR)
        .filter(|&s| (offset + sectors) * ANVIL_SECTOR <= file.len() && s + 4 <= file.len())
        .ok_or_else(|| format!("chunk sector run outside {}", path.display()))?;
    Ok(Some((offset, sectors, start)))
}

fn anvil_region_slot(
    file: &[u8],
    timestamps: &[u8],
    slot: usize,
    rx: i32,
    rz: i32,
    path: &std::path::Path,
) -> Result<Option<RegionChunkRecord>, String> {
    let Some((_offset, sectors, start)) = anvil_region_run(file, slot, path)? else {
        return Ok(None);
    };
    let len = u32::from_be_bytes(file[start..start + 4].try_into().unwrap()) as usize;
    if len < 2 || 4 + len > sectors * ANVIL_SECTOR {
        return Err(format!(
            "invalid chunk payload length {len} in {}",
            path.display()
        ));
    }
    let compression = file[start + 4];
    let raw = anvil_decompress(compression, &file[start + 5..start + 4 + len])?;
    let (_, nbt) = crate::nbt::read_named(&raw)
        .map_err(|e| format!("invalid chunk NBT in {}: {e}", path.display()))?;
    let timestamp = u32::from_be_bytes(timestamps[slot * 4..slot * 4 + 4].try_into().unwrap());
    let cx = rx * 32 + (slot % 32) as i32;
    let cz = rz * 32 + (slot / 32) as i32;
    Ok(Some((
        (cx, cz),
        AnvilChunkRecord {
            timestamp,
            compression,
            nbt,
        },
    )))
}

/// `read_region_file` shared core: header-level faults (bad name, too small,
/// truncated tables) are always fatal. With `tolerate`, a corrupt slot is
/// collected into the returned error list and decoding continues — an import
/// keeps the valid neighbor chunks; without it, the first bad slot aborts the
/// read so a merge-write can never silently drop the record it refuses to
/// parse.
fn read_region_file_impl(
    path: &std::path::Path,
    tolerate: bool,
) -> Result<(AnvilRegion, Vec<String>), String> {
    let (rx, rz) = anvil_region_coords(path)?;
    let file = std::fs::read(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    if file.len() < 2 * ANVIL_SECTOR {
        return Err(format!("region file too small: {}", path.display()));
    }
    let (_, timestamps) = file.split_at(ANVIL_SECTOR);
    let mut region = AnvilRegion::default();
    let mut errors = Vec::new();
    for slot in 0..1024usize {
        match anvil_region_slot(&file, timestamps, slot, rx, rz, path) {
            Ok(Some(((cx, cz), record))) => {
                region.chunks.insert((cx, cz), record);
            }
            Ok(None) => {}
            Err(e) => {
                if !tolerate {
                    return Err(e);
                }
                errors.push(format!("slot {slot}: {e}"));
            }
        }
    }
    Ok((region, errors))
}

/// Decode only the requested chunk slots of `path`. Returns the raw file bytes
/// alongside the records so a caller can hand the same bytes to the raw updater
/// without a second read. A missing file is `(None, empty)`, not an error.
fn read_region_file_selected(
    path: &std::path::Path,
    wanted: &HashSet<(i32, i32)>,
) -> Result<RegionFileSelection, String> {
    let (rx, rz) = anvil_region_coords(path)?;
    if !path.is_file() {
        return Ok((None, HashMap::new()));
    }
    let file = std::fs::read(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    if file.len() < 2 * ANVIL_SECTOR {
        return Err(format!("region file too small: {}", path.display()));
    }
    let (_, timestamps) = file.split_at(ANVIL_SECTOR);
    let mut records = HashMap::new();
    for &(cx, cz) in wanted {
        if cx.div_euclid(32) != rx || cz.div_euclid(32) != rz {
            continue;
        }
        let slot = ((cx & 31) + (cz & 31) * 32) as usize;
        if let Some(((want_cx, want_cz), record)) =
            anvil_region_slot(&file, timestamps, slot, rx, rz, path)?
        {
            debug_assert_eq!((want_cx, want_cz), (cx, cz));
            records.insert((cx, cz), record);
        }
    }
    Ok((Some(file), records))
}

/// Write one fresh chunk record into the packed body and point its slot at it.
fn anvil_write_region_record(
    body: &mut Vec<u8>,
    locations: &mut [u8; ANVIL_SECTOR],
    timestamps: &mut [u8; ANVIL_SECTOR],
    slot: usize,
    record: &AnvilChunkRecord,
    path: &std::path::Path,
) -> Result<(), String> {
    let nbt = crate::nbt::write_named(&crate::nbt::NbtString(Vec::new()), &record.nbt)
        .map_err(|e| format!("cannot encode chunk NBT for {path:?}: {e}"))?;
    let compressed = anvil_compress(record.compression, &nbt)?;
    let len = compressed.len() + 1;
    let sectors = (4 + len).div_ceil(ANVIL_SECTOR);
    if sectors > 255 {
        let cx = path.display().to_string();
        return Err(format!("chunk in {cx} exceeds 255 sectors"));
    }
    let offset = 2 + body.len() / ANVIL_SECTOR;
    body.extend_from_slice(&(len as u32).to_be_bytes());
    body.push(record.compression);
    body.extend_from_slice(&compressed);
    body.resize(body.len().div_ceil(ANVIL_SECTOR) * ANVIL_SECTOR, 0);
    locations[slot * 4] = ((offset >> 16) & 0xff) as u8;
    locations[slot * 4 + 1] = ((offset >> 8) & 0xff) as u8;
    locations[slot * 4 + 2] = (offset & 0xff) as u8;
    locations[slot * 4 + 3] = sectors as u8;
    timestamps[slot * 4..slot * 4 + 4]
        .copy_from_slice(&record.timestamp.to_be_bytes());
    Ok(())
}

/// Copy one populated sector run byte-for-byte — length, compression byte,
/// compressed payload and sector padding all stay exactly as they were on disk.
/// The caller writes the slot's timestamp entry after a successful copy.
fn anvil_copy_region_run(
    file: &[u8],
    body: &mut Vec<u8>,
    locations: &mut [u8; ANVIL_SECTOR],
    slot: usize,
    sectors: usize,
    start: usize,
    path: &std::path::Path,
) -> Result<(), String> {
    let len = u32::from_be_bytes(file[start..start + 4].try_into().unwrap()) as usize;
    if len < 2 || 4 + len > sectors * ANVIL_SECTOR {
        return Err(format!(
            "invalid chunk payload length {len} in {}",
            path.display()
        ));
    }
    let offset = 2 + body.len() / ANVIL_SECTOR;
    body.extend_from_slice(&file[start..start + sectors * ANVIL_SECTOR]);
    locations[slot * 4] = ((offset >> 16) & 0xff) as u8;
    locations[slot * 4 + 1] = ((offset >> 8) & 0xff) as u8;
    locations[slot * 4 + 2] = (offset & 0xff) as u8;
    locations[slot * 4 + 3] = sectors as u8;
    Ok(())
}

/// Merge `updates` into `path` without decoding or recompressing untouched
/// chunks. `existing` must be the bytes of `path` as read for the merge (or
/// `None` only when the file was verified absent); populated slots missing from
/// `updates` are copied as their original sector runs, so a one-chunk edit stops
/// paying the 1,024-slot inflate/NBT/deflate cost.
fn update_region_file_raw_with_existing(
    path: &std::path::Path,
    updates: &HashMap<(i32, i32), AnvilChunkRecord>,
    existing: Option<&[u8]>,
) -> Result<(), String> {
    let (rx, rz) = anvil_region_coords(path)?;
    if let Some(file) = existing {
        if file.len() < 2 * ANVIL_SECTOR {
            return Err(format!("region file too small: {}", path.display()));
        }
    }
    for &(cx, cz) in updates.keys() {
        if cx.div_euclid(32) != rx || cz.div_euclid(32) != rz {
            return Err(format!(
                "chunk ({cx}, {cz}) does not belong in {}",
                path.display()
            ));
        }
    }
    let mut locations = [0u8; ANVIL_SECTOR];
    let mut timestamps = [0u8; ANVIL_SECTOR];
    let mut body = Vec::new();
    let source_timestamps = existing.map(|file| {
        debug_assert!(file.len() >= 2 * ANVIL_SECTOR);
        &file[ANVIL_SECTOR..2 * ANVIL_SECTOR]
    });
    for slot in 0..1024usize {
        let cx = rx * 32 + (slot % 32) as i32;
        let cz = rz * 32 + (slot / 32) as i32;
        if let Some(record) = updates.get(&(cx, cz)) {
            anvil_write_region_record(
                &mut body,
                &mut locations,
                &mut timestamps,
                slot,
                record,
                path,
            )?;
            continue;
        }
        let Some(file) = existing else { continue };
        let Some((_offset, sectors, start)) = anvil_region_run(file, slot, path)? else {
            continue;
        };
        let ts = source_timestamps.expect("existing file has timestamp table");
        anvil_copy_region_run(file, &mut body, &mut locations, slot, sectors, start, path)?;
        timestamps[slot * 4..slot * 4 + 4].copy_from_slice(&ts[slot * 4..slot * 4 + 4]);
    }
    let mut out = Vec::with_capacity(2 * ANVIL_SECTOR + body.len());
    out.extend_from_slice(&locations);
    out.extend_from_slice(&timestamps);
    out.extend_from_slice(&body);
    anvil_atomic_write(path, &out)
}

/// `update_region_file_raw_with_existing` for callers that have not already
/// read the region file for a hash-compare pass.
#[allow(dead_code)]
fn update_region_file_raw(
    path: &std::path::Path,
    updates: &HashMap<(i32, i32), AnvilChunkRecord>,
) -> Result<(), String> {
    let existing = if path.is_file() {
        Some(
            std::fs::read(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?,
        )
    } else {
        None
    };
    update_region_file_raw_with_existing(path, updates, existing.as_deref())
}

/// The lenient half of the region reader: every decodable chunk returns,
/// every corrupt slot reports into `errors` instead of aborting the file.
/// Used by the import path — a merge-write keeps the strict reader, since a
/// partial file must never be rewritten over itself.
#[allow(dead_code)]
fn read_region_file_tolerant(
    path: &std::path::Path,
) -> Result<(AnvilRegion, Vec<String>), String> {
    read_region_file_impl(path, true)
}

#[cfg(test)]
mod region_tests {
    use super::*;

    fn tmp_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("megu-region-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A minimal valid chunk NBT: unnamed root compound with xPos/zPos ints.
    fn chunk_nbt(cx: i32, cz: i32) -> AnvilNbt {
        anvil_obj(&[
            ("xPos", AnvilNbt::Int(cx)),
            ("zPos", AnvilNbt::Int(cz)),
            ("Status", anvil_s("minecraft:full")),
        ])
    }

    fn record(cx: i32, cz: i32, timestamp: u32, compression: u8) -> AnvilChunkRecord {
        AnvilChunkRecord {
            timestamp,
            compression,
            nbt: chunk_nbt(cx, cz),
        }
    }

    /// Serialize `nbt` exactly the way `write_region_file` stores a chunk:
    /// `write_named` of the unnamed root, then `compression`-flavoured bytes.
    fn stored_payload(compression: u8, nbt: &AnvilNbt) -> Vec<u8> {
        let raw = crate::nbt::write_named(&crate::nbt::NbtString(Vec::new()), nbt).unwrap();
        anvil_compress(compression, &raw).unwrap()
    }

    /// Hand-build an `.mca` image: two zeroed 4 KiB tables, then `runs` of
    /// `(slot, offset_sectors, sector_count, payload_bytes)` where the payload
    /// region at `offset_sectors * 4096` starts with `len | compression | data`.
    /// Sectors between runs are left as a distinct filler pattern so tests can
    /// prove the reader skips unreferenced space verbatim.
    fn craft_mca(
        rx: i32,
        rz: i32,
        entries: &[(usize, u32, u8, u8, &[u8])],
        file_sectors: usize,
    ) -> Vec<u8> {
        let mut file = vec![0u8; file_sectors * ANVIL_SECTOR];
        // Foreign filler: unreferenced sectors carry a recognizable pattern.
        for sector in 2..file_sectors {
            let base = sector * ANVIL_SECTOR;
            for (i, b) in file[base..base + ANVIL_SECTOR].iter_mut().enumerate() {
                *b = (sector as u8).wrapping_add(i as u8);
            }
        }
        for &(slot, offset, sectors, compression, payload) in entries {
            let le = &mut file[slot * 4..slot * 4 + 4];
            le[0] = ((offset >> 16) & 0xff) as u8;
            le[1] = ((offset >> 8) & 0xff) as u8;
            le[2] = (offset & 0xff) as u8;
            le[3] = sectors;
            let start = offset as usize * ANVIL_SECTOR;
            let len = payload.len() + 1;
            file[start..start + 4].copy_from_slice(&(len as u32).to_be_bytes());
            file[start + 4] = compression;
            file[start + 5..start + 5 + payload.len()].copy_from_slice(payload);
        }
        let _ = (rx, rz);
        file
    }

    type SlotOut = Result<Option<((i32, i32), AnvilChunkRecord)>, String>;

    fn slot_of(file: &[u8], timestamps: &[u8], slot: usize, rx: i32, rz: i32) -> SlotOut {
        anvil_region_slot(file, timestamps, slot, rx, rz, std::path::Path::new("r.0.0.mca"))
    }

    #[test]
    fn nbt_fingerprint_is_deterministic_and_covers_every_tag_variant() {
        let mut fields = std::collections::BTreeMap::new();
        fields.insert(crate::nbt::NbtString::text("a"), AnvilNbt::Int(1));
        fields.insert(
            crate::nbt::NbtString::text("b"),
            AnvilNbt::List(3, vec![AnvilNbt::Int(-1), AnvilNbt::Int(2)]),
        );
        let tag = AnvilNbt::Compound(fields);
        assert_eq!(anvil_nbt_fingerprint(&tag), anvil_nbt_fingerprint(&tag));
        // Every variant contributes to the stream: a tree holding all 12 tag
        // types must differ from the same tree with any single leaf changed.
        let full = anvil_obj(&[
            ("end", AnvilNbt::End),
            ("byte", AnvilNbt::Byte(-5)),
            ("short", AnvilNbt::Short(-300)),
            ("int", AnvilNbt::Int(-70000)),
            ("long", AnvilNbt::Long(-5_000_000_000)),
            ("float", AnvilNbt::Float(1.5f32.to_bits())),
            ("double", AnvilNbt::Double((-2.25f64).to_bits())),
            ("bytes", AnvilNbt::ByteArray(vec![1, 2, 3])),
            ("string", anvil_s("héllo")),
            ("list", anvil_list(vec![anvil_s("x"), anvil_s("y")])),
            ("intarray", AnvilNbt::IntArray(vec![i32::MIN, i32::MAX])),
            ("longarray", AnvilNbt::LongArray(vec![i64::MIN, i64::MAX])),
        ]);
        let base = anvil_nbt_fingerprint(&full);
        for (key, alt) in [
            ("byte", AnvilNbt::Byte(6)),
            ("short", AnvilNbt::Short(-301)),
            ("long", AnvilNbt::Long(5_000_000_000)),
            ("bytes", AnvilNbt::ByteArray(vec![1, 2, 4])),
            ("string", anvil_s("hello")),
            ("intarray", AnvilNbt::IntArray(vec![i32::MIN, 0])),
            ("longarray", AnvilNbt::LongArray(vec![0, i64::MAX])),
        ] {
            let mut changed = full.clone();
            if let AnvilNbt::Compound(fields) = &mut changed {
                fields.insert(crate::nbt::NbtString::text(key), alt);
            }
            assert_ne!(
                anvil_nbt_fingerprint(&changed),
                base,
                "changing {key} must change the fingerprint"
            );
        }
        // Tag identity is part of the hash: Int(7) and Long(7) differ.
        let as_int = anvil_obj(&[("v", AnvilNbt::Int(7))]);
        let as_long = anvil_obj(&[("v", AnvilNbt::Long(7))]);
        assert_ne!(anvil_nbt_fingerprint(&as_int), anvil_nbt_fingerprint(&as_long));
        // The avalanche decorrelates: sequential integers do not produce
        // sequential fingerprints.
        let f0 = anvil_bytes_fingerprint(&[0u8; 8]);
        let f1 = anvil_bytes_fingerprint(&[1u8; 8]);
        assert_ne!(f0, f1);
        assert_ne!(f0 ^ f1, 1);
    }

    #[test]
    fn payload_unchanged_gates_writes_on_cached_disk_hash() {
        let key: AnvilPersistedKey = (std::path::PathBuf::from("/world"), DimId::Overworld, 1, 2);
        let mut hashes = HashMap::new();
        let on_disk = chunk_nbt(1, 2);
        let disk_hash = anvil_nbt_fingerprint(&on_disk);
        // Nothing on disk: a write is always required.
        assert!(!anvil_payload_unchanged(key.clone(), disk_hash, None, &mut hashes));
        assert!(hashes.is_empty(), "a missing record caches nothing");
        // Identical content: skip the write.
        assert!(anvil_payload_unchanged(
            key.clone(),
            disk_hash,
            Some(&on_disk),
            &mut hashes
        ));
        // The first touch cached the on-disk hash; a *different* on-disk tag
        // under the same key must not resurrect a write for the same fresh
        // payload — the committed record is what the cache describes.
        let other = chunk_nbt(9, 9);
        assert!(anvil_payload_unchanged(
            key.clone(),
            disk_hash,
            Some(&other),
            &mut hashes
        ));
        // A changed fresh payload under the same key still writes.
        let fresh_other = anvil_nbt_fingerprint(&other);
        assert!(!anvil_payload_unchanged(
            key.clone(),
            fresh_other,
            Some(&on_disk),
            &mut hashes
        ));
        // A different chunk key caches its own disk hash independently: same
        // fresh payload against different on-disk content still writes.
        let key2: AnvilPersistedKey = (std::path::PathBuf::from("/world"), DimId::Overworld, 3, 4);
        assert!(!anvil_payload_unchanged(
            key2,
            fresh_other,
            Some(&on_disk),
            &mut hashes
        ));
        // Identical content under a fresh key skips: the hash is content,
        // not identity.
        let key3: AnvilPersistedKey = (std::path::PathBuf::from("/world"), DimId::Nether, 0, 0);
        assert!(anvil_payload_unchanged(
            key3,
            disk_hash,
            Some(&on_disk),
            &mut hashes
        ));
    }

    #[test]
    fn incremental_queue_drain_pending_error_requeue_and_stats() {
        let mut queue = IncrementalSaveQueue::new(IncrementalSaveBudget {
            max_chunks_per_tick: usize::MAX,
            max_bytes_per_tick: usize::MAX,
        });
        assert!(queue.mark_dirty(DimId::Overworld, 0, 0, 10));
        assert!(queue.mark_dirty(DimId::Nether, 5, 5, 20));
        // A re-mark keeps the oldest position and reports not-newly-queued.
        assert!(!queue.mark_dirty(DimId::Overworld, 0, 0, 99));
        assert_eq!(queue.drain_pending(), 2);
        assert!(queue.is_empty());

        // Byte leg: the first chunk always flushes even when it alone exceeds
        // the budget — caps bound batches, never starve a single write.
        let mut queue = IncrementalSaveQueue::new(IncrementalSaveBudget {
            max_chunks_per_tick: usize::MAX,
            max_bytes_per_tick: 1,
        });
        queue.mark_dirty(DimId::Overworld, 0, 0, usize::MAX / 2);
        queue.mark_dirty(DimId::Overworld, 1, 0, 10);
        let stats = queue
            .drain_budgeted(|_| {
                Ok::<_, String>(AnvilChunkFlushOutcome {
                    blocks_flushed: 3,
                    bytes_written: 4096,
                    skipped_unchanged: true,
                })
            })
            .unwrap();
        assert_eq!(stats.chunks_flushed, 1, "oversized first chunk still flushes");
        assert_eq!(stats.chunks_skipped_unchanged, 1);
        assert_eq!(stats.bytes_flushed, 4096);
        assert_eq!(stats.remaining, 1);

        // Error path: the failed chunk returns to the head and the pass
        // aborts — the next drain retries it before newer entries.
        let mut queue = IncrementalSaveQueue::new(IncrementalSaveBudget::UNLIMITED);
        for cx in 0..4 {
            queue.mark_dirty(DimId::Overworld, cx, 0, 8);
        }
        let mut seen = Vec::new();
        let err = queue
            .drain_budgeted(|item| {
                seen.push(item.cx);
                if item.cx == 2 {
                    Err("boom".to_string())
                } else {
                    Ok(AnvilChunkFlushOutcome::default())
                }
            })
            .unwrap_err();
        assert_eq!(err, "boom");
        assert_eq!(seen, vec![0, 1, 2], "chunks after the failure are untouched");
        assert_eq!(queue.len(), 2, "cx=2 requeued at head, cx=3 still pending");
        let stats = queue
            .drain_budgeted(|item| {
                seen.push(item.cx);
                Ok(AnvilChunkFlushOutcome {
                    blocks_flushed: 1,
                    bytes_written: 0,
                    skipped_unchanged: false,
                })
            })
            .unwrap();
        assert_eq!(stats.chunks_flushed, 2);
        assert_eq!(&seen[3..], &[2, 3], "the retried chunk leads the next pass");
        assert!(queue.is_empty());
    }

    #[test]
    fn region_slot_rejects_every_malformed_location_entry() {
        let payload = stored_payload(3, &chunk_nbt(0, 0));
        let good = |slot: usize, offset: u32, sectors: u8, payload: &[u8]| {
            craft_mca(0, 0, &[(slot, offset, sectors, 3, payload)], 8)
        };
        // offset == 0 with nonzero sectors, and nonzero offset with zero
        // sectors, are both invalid location entries.
        for file in [
            good(0, 0, 4, &payload),
            good(0, 5, 0, &payload),
        ] {
            let (_, timestamps) = file.split_at(ANVIL_SECTOR);
            assert!(slot_of(&file, timestamps, 0, 0, 0).is_err());
        }
        // A sector run that does not fit inside the file fails closed.
        let file = good(0, 7, 2, &payload); // needs sectors 7..9, file has 8
        let (_, timestamps) = file.split_at(ANVIL_SECTOR);
        assert!(slot_of(&file, timestamps, 0, 0, 0).is_err());
        // len < 2 (no compression byte) is rejected.
        let mut file = craft_mca(0, 0, &[(0, 2, 1, 3, &[])], 4);
        file[2 * ANVIL_SECTOR..2 * ANVIL_SECTOR + 4].copy_from_slice(&1u32.to_be_bytes());
        let (_, timestamps) = file.split_at(ANVIL_SECTOR);
        assert!(slot_of(&file, timestamps, 0, 0, 0).is_err());
        // 4 + len exceeding the recorded sector run is rejected.
        let mut file = craft_mca(0, 0, &[(0, 2, 1, 3, &payload)], 4);
        file[2 * ANVIL_SECTOR..2 * ANVIL_SECTOR + 4].copy_from_slice(&4093u32.to_be_bytes());
        let (_, timestamps) = file.split_at(ANVIL_SECTOR);
        assert!(slot_of(&file, timestamps, 0, 0, 0).is_err());
        // Unknown compression flavour is rejected before any decode.
        let file = craft_mca(0, 0, &[(0, 2, 1, 9, &payload)], 4);
        let (_, timestamps) = file.split_at(ANVIL_SECTOR);
        assert!(slot_of(&file, timestamps, 0, 0, 0).is_err());
        // Truncated payload inside a claimed run: len says more than sectors hold.
        let file = craft_mca(0, 0, &[(0, 2, 1, 3, &payload[..payload.len() / 2])], 4);
        let (_, timestamps) = file.split_at(ANVIL_SECTOR);
        // len field still claims the full payload -> 4+len > sectors*4096? no;
        // it decodes garbage -> NBT error. Either way it must fail.
        assert!(slot_of(&file, timestamps, 0, 0, 0).is_err());
    }

    #[test]
    fn region_slot_decodes_coords_timestamp_and_skips_foreign_sectors() {
        let payload = stored_payload(3, &chunk_nbt(-32, -32));
        // Chunk (-32,-32) is slot 0 of r.-1.-1.mca; sectors 2..4 hold filler.
        let mut file = craft_mca(-1, -1, &[(0, 5, 2, 3, &payload)], 8);
        file[ANVIL_SECTOR..ANVIL_SECTOR + 4].copy_from_slice(&0x1234_5678u32.to_be_bytes());
        let (_, ts) = file.split_at(ANVIL_SECTOR);
        let result = slot_of(&file, ts, 0, -1, -1).unwrap().unwrap();
        assert_eq!(result.0, (-32, -32), "rx*32 + slot%32 for negative region");
        assert_eq!(result.1.timestamp, 0x1234_5678);
        assert_eq!(result.1.compression, 3);
        // Empty slot decodes as None.
        assert!(slot_of(&file, ts, 1, -1, -1).unwrap().is_none());
        // Slot 1023 of r.-1.-1 maps to chunk (-1,-1).
        let file2 = craft_mca(-1, -1, &[(1023, 2, 1, 3, &payload)], 4);
        let (_, ts2) = file2.split_at(ANVIL_SECTOR);
        let result2 = slot_of(&file2, ts2, 1023, -1, -1).unwrap().unwrap();
        assert_eq!(result2.0, (-1, -1));
    }

    #[test]
    fn strict_and_tolerant_reads_disagree_only_on_slot_level_corruption() {
        let dir = tmp_dir("strict-tolerant");
        let mca = dir.join("r.0.0.mca");
        let mut region = AnvilRegion::default();
        region.chunks.insert((0, 0), record(0, 0, 11, 2));
        region.chunks.insert((1, 0), record(1, 0, 22, 3));
        write_region_file(&mca, &region).unwrap();
        // Corrupt chunk (0,0)'s *payload*: flip bytes inside its sector run so
        // the slot decodes to invalid NBT while the location entry stays valid.
        let mut file = std::fs::read(&mca).unwrap();
        let start = 2 * ANVIL_SECTOR;
        for b in &mut file[start + 5..start + 40] {
            *b ^= 0xff;
        }
        std::fs::write(&mca, &file).unwrap();
        assert!(read_region_file(&mca).is_err(), "strict read fails closed");
        let (recovered, errors) = read_region_file_tolerant(&mca).unwrap();
        assert_eq!(errors.len(), 1);
        assert!(errors[0].contains("slot 0"));
        assert!(!recovered.chunks.contains_key(&(0, 0)));
        assert_eq!(
            recovered.chunks.get(&(1, 0)),
            Some(&record(1, 0, 22, 3)),
            "the valid neighbor still decodes under payload corruption"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn region_rewrite_deletes_chunks_and_reuses_sectors_deterministically() {
        let dir = tmp_dir("rewrite");
        let mca = dir.join("r.0.0.mca");
        let mut full = AnvilRegion::default();
        for (cx, ts) in [(0, 100u32), (1, 200), (2, 300)] {
            full.chunks.insert((cx, 0), record(cx, 0, ts, 2));
        }
        write_region_file(&mca, &full).unwrap();
        let first_bytes = std::fs::read(&mca).unwrap();
        // Deleting two chunks repacks: only (1,0) remains, at offset 2 again,
        // and its timestamp survives the rewrite.
        let mut shrunk = AnvilRegion::default();
        shrunk.chunks.insert((1, 0), record(1, 0, 200, 2));
        write_region_file(&mca, &shrunk).unwrap();
        let got = read_region_file(&mca).unwrap();
        assert_eq!(got, shrunk);
        let shrunk_bytes = std::fs::read(&mca).unwrap();
        assert!(shrunk_bytes.len() < first_bytes.len(), "dead sectors reclaimed");
        // The surviving chunk's location entry points at offset 2 (first run).
        let slot = 4;
        let off = (usize::from(shrunk_bytes[slot]) << 16)
            | (usize::from(shrunk_bytes[slot + 1]) << 8)
            | usize::from(shrunk_bytes[slot + 2]);
        assert_eq!(off, 2);
        // Re-adding the deleted chunks reproduces the original file byte for
        // byte: packing order is the BTreeMap order, so sector reuse is exact.
        write_region_file(&mca, &full).unwrap();
        assert_eq!(std::fs::read(&mca).unwrap(), first_bytes);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn region_read_ignores_unreferenced_sectors_and_rewrite_compacts() {
        let dir = tmp_dir("foreign-sectors");
        let mca = dir.join("r.3.4.mca");
        let payload = stored_payload(3, &chunk_nbt(96, 128));
        // Chunk (96,128) is slot (0 + 0*32)=0? cx%32=0? 96%32=0, 128%32=0 -> slot 0.
        let file = craft_mca(3, 4, &[(0, 5, 2, 3, &payload)], 8);
        std::fs::write(&mca, &file).unwrap();
        let region = read_region_file(&mca).unwrap();
        assert_eq!(region.chunks.len(), 1);
        let rec = &region.chunks[&(96, 128)];
        assert_eq!(rec.timestamp, 0);
        assert_eq!(rec.compression, 3);
        // Tolerant read agrees: foreign sectors are not slots, not errors.
        let (tolerant, errors) = read_region_file_tolerant(&mca).unwrap();
        assert!(errors.is_empty());
        assert_eq!(tolerant, region);
        // Rewrite through the writer compacts the run to offset 2.
        write_region_file(&mca, &region).unwrap();
        let rewritten = std::fs::read(&mca).unwrap();
        assert_eq!(rewritten.len(), 3 * ANVIL_SECTOR, "dead sectors dropped");
        let off = (usize::from(rewritten[0]) << 16)
            | (usize::from(rewritten[1]) << 8)
            | usize::from(rewritten[2]);
        assert_eq!(off, 2);
        assert_eq!(read_region_file(&mca).unwrap(), region);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn staging_recovery_handles_missing_dir_and_nested_tmp_sweep() {
        let missing = tmp_dir("staging").join("does-not-exist");
        assert_eq!(anvil_recover_world_staging(&missing).unwrap(), 0);
        let dir = tmp_dir("staging2");
        // Orphan .tmp files inside nested dimension region dirs are swept;
        // a .tmp *directory* is not a staging file and is left alone.
        for rel in ["region", "DIM-1/region", "DIM1/region", "playerdata"] {
            let d = dir.join(rel);
            std::fs::create_dir_all(&d).unwrap();
            std::fs::write(d.join("r.0.0.mca.tmp"), b"torn").unwrap();
        }
        let nested_tmp_dir = dir.join("region").join("keep.tmp");
        std::fs::create_dir_all(&nested_tmp_dir).unwrap();
        let swept = anvil_recover_world_staging(&dir).unwrap();
        assert_eq!(swept, 4);
        assert!(!dir.join("region/r.0.0.mca.tmp").exists());
        assert!(nested_tmp_dir.is_dir(), "directories are never swept");
        // Syncing a parent dir is best-effort and must not fail the caller.
        anvil_sync_parent_dir(&dir.join("level.dat"));
        anvil_sync_parent_dir(std::path::Path::new("relative-name-only"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
