// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
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

const ANVIL_FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const ANVIL_FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

fn anvil_fnv_mix(h: &mut u64, byte: u8) {
    *h = (*h ^ u64::from(byte)).wrapping_mul(ANVIL_FNV_PRIME);
}

fn anvil_fnv_bytes(h: &mut u64, bytes: &[u8]) {
    for &b in bytes {
        anvil_fnv_mix(h, b);
    }
}

fn anvil_fnv_u32(h: &mut u64, v: u32) {
    anvil_fnv_bytes(h, &v.to_be_bytes());
}

fn anvil_fnv_u64(h: &mut u64, v: u64) {
    anvil_fnv_bytes(h, &v.to_be_bytes());
}

/// Splitmix64 avalanche: decorrelates the FNV state into every output bit.
fn anvil_fingerprint_final(mut z: u64) -> u64 {
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

fn anvil_nbt_fingerprint_into(tag: &AnvilNbt, h: &mut u64) {
    anvil_fnv_mix(h, tag.id());
    match tag {
        AnvilNbt::End => {}
        AnvilNbt::Byte(v) => anvil_fnv_mix(h, *v as u8),
        AnvilNbt::Short(v) => anvil_fnv_bytes(h, &v.to_be_bytes()),
        AnvilNbt::Int(v) => anvil_fnv_bytes(h, &v.to_be_bytes()),
        AnvilNbt::Long(v) => anvil_fnv_u64(h, *v as u64),
        AnvilNbt::Float(v) => anvil_fnv_u32(h, *v),
        AnvilNbt::Double(v) => anvil_fnv_u64(h, *v),
        AnvilNbt::ByteArray(v) => {
            anvil_fnv_u32(h, v.len() as u32);
            anvil_fnv_bytes(h, v);
        }
        AnvilNbt::String(s) => {
            anvil_fnv_u32(h, s.0.len() as u32);
            for &unit in &s.0 {
                anvil_fnv_bytes(h, &unit.to_be_bytes());
            }
        }
        AnvilNbt::List(kind, items) => {
            anvil_fnv_mix(h, *kind);
            anvil_fnv_u32(h, items.len() as u32);
            for item in items {
                anvil_nbt_fingerprint_into(item, h);
            }
        }
        AnvilNbt::Compound(fields) => {
            anvil_fnv_u32(h, fields.len() as u32);
            for (key, value) in fields {
                anvil_fnv_u32(h, key.0.len() as u32);
                for &unit in &key.0 {
                    anvil_fnv_bytes(h, &unit.to_be_bytes());
                }
                anvil_nbt_fingerprint_into(value, h);
            }
        }
        AnvilNbt::IntArray(v) => {
            anvil_fnv_u32(h, v.len() as u32);
            for &x in v {
                anvil_fnv_bytes(h, &x.to_be_bytes());
            }
        }
        AnvilNbt::LongArray(v) => {
            anvil_fnv_u32(h, v.len() as u32);
            for &x in v {
                anvil_fnv_u64(h, x as u64);
            }
        }
    }
}

/// The content fingerprint of one chunk/playerdata NBT payload — the value
/// `anvil_persisted_hashes` compares a would-be save against.
#[allow(dead_code)]
fn anvil_nbt_fingerprint(tag: &AnvilNbt) -> u64 {
    let mut h = ANVIL_FNV_OFFSET;
    anvil_nbt_fingerprint_into(tag, &mut h);
    anvil_fingerprint_final(h)
}

/// The same fingerprint over an already-serialized byte payload (a gzip
/// member, a framed file) — for callers whose content arrives as bytes.
#[allow(dead_code)]
fn anvil_bytes_fingerprint(bytes: &[u8]) -> u64 {
    let mut h = ANVIL_FNV_OFFSET;
    anvil_fnv_bytes(&mut h, bytes);
    anvil_fingerprint_final(h)
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
        loop {
            let Some(front) = self.pending.front() else {
                break;
            };
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
fn anvil_region_slot(
    file: &[u8],
    timestamps: &[u8],
    slot: usize,
    rx: i32,
    rz: i32,
    path: &std::path::Path,
) -> Result<Option<((i32, i32), AnvilChunkRecord)>, String> {
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
        .filter(|&s| (offset + sectors) * ANVIL_SECTOR <= file.len() && s + 5 <= file.len())
        .ok_or_else(|| format!("chunk sector run outside {}", path.display()))?;
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
