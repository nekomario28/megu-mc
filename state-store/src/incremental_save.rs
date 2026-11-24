// SPDX-License-Identifier: LGPL-3.0-or-later
//! Incremental autosave machinery: a deterministic FIFO/oldest-dirty
//! [`IncrementalSaveQueue`] drained under a per-tick [`IncrementalSaveBudget`],
//! plus the [`content_fingerprint`]-backed [`PersistedFingerprints`] cache
//! that lets an unchanged payload skip its disk write entirely.
//!
//! The queue exists to kill the classic world-save stall: instead of flushing
//! every dirty chunk in one 100-400ms burst, each autosave tick drains only
//! `max_items_per_tick` items and `max_bytes_per_tick` estimated payload bytes
//! — the rest stays queued, oldest-first, for later ticks. `flush_all` is the
//! guaranteed synchronous drain for `/save-all` and graceful shutdown.
//!
//! The fingerprint is a plain FNV-1a-64 state passed through a splitmix64
//! avalanche — deterministic across processes and platforms, cheap, and good
//! enough for a skip-if-identical check (a flipped payload bit changes the
//! fingerprint with overwhelming probability).

use std::collections::{HashMap, HashSet, VecDeque};
use std::hash::Hash;
use std::io;
use std::path::{Path, PathBuf};


/// Deterministic 64-bit content fingerprint of a serialized payload. The
/// persisted-hash skip compares this value, never the bytes themselves.
pub fn content_fingerprint(bytes: &[u8]) -> u64 {
    megu_core::perf_kit::fingerprint_bytes(bytes)
}

/// Per-tick drain caps for [`IncrementalSaveQueue`]. A single queued item
/// larger than a leg still flushes — the caps bound batch work, they never
/// starve one write.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IncrementalSaveBudget {
    /// Most queued items one `drain_budgeted` pass may flush.
    pub max_items_per_tick: usize,
    /// Most estimated payload bytes one pass may charge.
    pub max_bytes_per_tick: usize,
}

impl IncrementalSaveBudget {
    /// No caps — the `/save-all` and shutdown drain.
    pub const UNLIMITED: Self = Self {
        max_items_per_tick: usize::MAX,
        max_bytes_per_tick: usize::MAX,
    };
}

impl Default for IncrementalSaveBudget {
    /// Smooth autosave defaults: a few chunks and a few MiB per tick spread a
    /// large burst across seconds instead of one stall.
    fn default() -> Self {
        Self {
            max_items_per_tick: 8,
            max_bytes_per_tick: 8 << 20,
        }
    }
}

/// One queued dirty item. `est_bytes` is the caller's estimate of the
/// serialized payload — the byte-budget leg charges against it, so it should
/// track the payload's order of magnitude, not be exact.
#[derive(Clone, Debug)]
pub struct QueuedSave<K> {
    /// The caller's dirty-set identity (a chunk coordinate, a file path).
    pub key: K,
    /// Estimated serialized payload size, charged to the byte budget.
    pub est_bytes: usize,
}

/// What the flush closure reports for one drained item.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FlushOutcome {
    /// `false` when the item needed no write at all — its persisted content
    /// already matched (the content-hash skip).
    pub wrote: bool,
    /// Bytes the flush reports actually committing (0 when skipped).
    pub bytes_written: u64,
}

/// What one drain pass spent and left.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DrainStats {
    /// Queued items popped and flushed (skips included).
    pub items_flushed: usize,
    /// Of those, the ones the flush skipped as unchanged.
    pub skipped_unchanged: usize,
    /// Bytes the flushes report actually committing.
    pub bytes_flushed: u64,
    /// Estimated payload bytes charged against the byte budget leg.
    pub bytes_charged: u64,
    /// Items still queued after the pass.
    pub remaining: usize,
}

/// A deterministic FIFO/oldest-dirty queue of pending saves drained under a
/// per-tick [`IncrementalSaveBudget`].
///
/// `mark_dirty` is idempotent: a key already queued keeps its oldest
/// position, so re-dirtying cannot push it behind newer work. `drain_budgeted`
/// pops the oldest entries until a budget leg trips; a flush error pushes the
/// item back to the head and aborts the pass — the failed item is the next
/// thing retried, never dropped. `flush_all` ignores the budget entirely.
pub struct IncrementalSaveQueue<K> {
    budget: IncrementalSaveBudget,
    pending: VecDeque<QueuedSave<K>>,
    queued: HashSet<K>,
}

impl<K: Eq + Hash + Clone> IncrementalSaveQueue<K> {
    /// An empty queue under `budget`.
    pub fn new(budget: IncrementalSaveBudget) -> Self {
        Self {
            budget,
            pending: VecDeque::new(),
            queued: HashSet::new(),
        }
    }

    /// The queue's current budget.
    pub fn budget(&self) -> IncrementalSaveBudget {
        self.budget
    }

    /// Number of items waiting to flush.
    pub fn len(&self) -> usize {
        self.pending.len()
    }

    pub fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }

    /// Whether `key` is already queued.
    pub fn contains(&self, key: &K) -> bool {
        self.queued.contains(key)
    }

    /// Enqueue `key` for a later flush. Returns `true` when the key was not
    /// already pending; a re-mark keeps the oldest position and refreshes
    /// only the byte estimate.
    pub fn mark_dirty(&mut self, key: K, est_bytes: usize) -> bool {
        if !self.queued.insert(key.clone()) {
            if let Some(entry) = self.pending.iter_mut().find(|e| e.key == key) {
                entry.est_bytes = est_bytes;
            }
            return false;
        }
        self.pending.push_back(QueuedSave { key, est_bytes });
        true
    }

    /// Forget every pending item without flushing — used when a full save
    /// persisted the same keys through another path.
    pub fn drain_pending(&mut self) -> usize {
        let n = self.pending.len();
        self.pending.clear();
        self.queued.clear();
        n
    }

    /// Flush the oldest pending items until a budget leg trips. A failed
    /// flush keeps the item at the head of the queue so the error aborts the
    /// pass without losing it — it is the next thing the next pass retries.
    pub fn drain_budgeted<E>(
        &mut self,
        mut flush: impl FnMut(&QueuedSave<K>) -> Result<FlushOutcome, E>,
    ) -> Result<DrainStats, E> {
        let mut stats = DrainStats::default();
        while let Some(front) = self.pending.front() {
            if stats.items_flushed >= self.budget.max_items_per_tick {
                break;
            }
            let est = front.est_bytes as u64;
            if stats.items_flushed > 0
                && stats.bytes_charged.saturating_add(est)
                    > self.budget.max_bytes_per_tick as u64
            {
                break;
            }
            let item = self.pending.pop_front().expect("front checked above");
            match flush(&item) {
                Ok(outcome) => {
                    self.queued.remove(&item.key);
                    stats.items_flushed += 1;
                    stats.skipped_unchanged += usize::from(!outcome.wrote);
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

    /// The guaranteed synchronous drain — `/save-all`, graceful shutdown:
    /// every queued item flushes before this returns.
    pub fn flush_all<E>(
        &mut self,
        flush: impl FnMut(&QueuedSave<K>) -> Result<FlushOutcome, E>,
    ) -> Result<DrainStats, E> {
        let held = self.budget;
        self.budget = IncrementalSaveBudget::UNLIMITED;
        let stats = self.drain_budgeted(flush);
        self.budget = held;
        stats
    }
}

/// In-memory `(path -> fingerprint)` bookkeeping of what the process last
/// committed, used by [`PersistedFingerprints::write_atomic_if_changed`] to
/// skip a write whose payload is already on disk.
#[derive(Debug, Default)]
pub struct PersistedFingerprints {
    map: HashMap<PathBuf, u64>,
}

impl PersistedFingerprints {
    pub fn new() -> Self {
        Self::default()
    }

    /// Atomically write `bytes` to `path` — unless the bytes already on disk
    /// match, in which case nothing is touched and `Ok(false)` returns.
    ///
    /// The skip is three-tiered and conservative: the in-memory fingerprint
    /// of the last successful write short-circuits first; on a miss the file
    /// itself is read and fingerprinted (an identical file produced by a
    /// previous process still skips); only a genuine mismatch or missing
    /// file pays for [`crate::migrate::write_file_atomic`]. A file that
    /// disappears between the check and the write simply makes the write a
    /// normal first commit.
    pub fn write_atomic_if_changed(&mut self, path: &Path, bytes: &[u8]) -> io::Result<bool> {
        let fresh = content_fingerprint(bytes);
        if self.map.get(path) == Some(&fresh) && path.is_file() {
            return Ok(false);
        }
        if let Ok(existing) = std::fs::read(path) {
            let on_disk = content_fingerprint(&existing);
            self.map.insert(path.to_path_buf(), on_disk);
            if on_disk == fresh {
                return Ok(false);
            }
        }
        crate::migrate::write_file_atomic(path, bytes)?;
        self.map.insert(path.to_path_buf(), fresh);
        Ok(true)
    }

    /// The fingerprint recorded for `path`, if any.
    pub fn persisted(&self, path: &Path) -> Option<u64> {
        self.map.get(path).copied()
    }

    /// Forget a recorded entry (the caller deleted or replaced the file
    /// through another path).
    pub fn forget(&mut self, path: &Path) {
        self.map.remove(path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn test_dir(tag: &str) -> PathBuf {
        static DIR_COUNTER: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "megu-incremental-save-{}-{}-{tag}",
            std::process::id(),
            DIR_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn perf_disk_save_fingerprint_is_deterministic_and_sensitive() {
        assert_eq!(content_fingerprint(b"payload"), content_fingerprint(b"payload"));
        assert_ne!(content_fingerprint(b"payload"), content_fingerprint(b"payloae"));
        assert_ne!(content_fingerprint(b""), content_fingerprint(b"\0"));
        // Length is part of the input space: a prefix never collides.
        let mut long = b"chunk-record".to_vec();
        let short = long.clone();
        long.push(0);
        assert_ne!(content_fingerprint(&short), content_fingerprint(&long));
    }

    #[test]
    fn perf_disk_save_queue_drains_bounded_slices_and_flush_all_empties() {
        let mut queue = IncrementalSaveQueue::new(IncrementalSaveBudget {
            max_items_per_tick: 7,
            max_bytes_per_tick: usize::MAX,
        });
        for i in 0..100u32 {
            assert!(queue.mark_dirty(i, 64));
        }
        assert_eq!(queue.len(), 100);
        // Re-marking keeps the oldest position — no duplicates.
        assert!(!queue.mark_dirty(50, 128));
        assert_eq!(queue.len(), 100);

        let mut flushed = Vec::new();
        let mut drain = |item: &QueuedSave<u32>| -> Result<FlushOutcome, String> {
            flushed.push(item.key);
            Ok(FlushOutcome {
                wrote: true,
                bytes_written: 64,
            })
        };
        // Bounded slices: exactly `max_items_per_tick` per pass, oldest first.
        let mut total = 0usize;
        loop {
            let stats = queue.drain_budgeted(&mut drain).unwrap();
            assert!(stats.items_flushed <= 7);
            if stats.remaining == 0 {
                total += stats.items_flushed;
                break;
            }
            total += stats.items_flushed;
        }
        assert_eq!(total, 100);
        assert_eq!(flushed.len(), 100);
        // FIFO: oldest dirty flushes first.
        assert_eq!(flushed, (0..100).collect::<Vec<_>>());
        assert!(queue.is_empty());
    }

    #[test]
    fn perf_disk_save_queue_byte_leg_bounds_and_never_starves() {
        let mut queue = IncrementalSaveQueue::new(IncrementalSaveBudget {
            max_items_per_tick: usize::MAX,
            max_bytes_per_tick: 300,
        });
        for i in 0..10u32 {
            queue.mark_dirty(i, 150);
        }
        let mut writes = 0u64;
        let stats = queue
            .drain_budgeted(|_| {
                Ok::<_, String>({
                    writes += 150;
                    FlushOutcome {
                        wrote: true,
                        bytes_written: 150,
                    }
                })
            })
            .unwrap();
        assert_eq!(stats.items_flushed, 2, "300-byte budget admits two 150-byte items");
        assert_eq!(stats.bytes_charged, 300);
        assert_eq!(stats.bytes_flushed, 300);
        assert_eq!(stats.remaining, 8);

        // An oversized first item still flushes: the budget bounds batches,
        // never starves the head of the queue.
        let mut queue = IncrementalSaveQueue::new(IncrementalSaveBudget {
            max_items_per_tick: usize::MAX,
            max_bytes_per_tick: 1,
        });
        queue.mark_dirty(0u32, 1_000_000);
        queue.mark_dirty(1, 1);
        let stats = queue
            .drain_budgeted(|_| {
                Ok::<_, String>(FlushOutcome {
                    wrote: true,
                    bytes_written: 1_000_000,
                })
            })
            .unwrap();
        assert_eq!(stats.items_flushed, 1);
        assert_eq!(stats.remaining, 1);
    }

    #[test]
    fn perf_disk_save_queue_error_keeps_item_at_head_and_flush_all_drains() {
        let mut queue = IncrementalSaveQueue::new(IncrementalSaveBudget {
            max_items_per_tick: 10,
            max_bytes_per_tick: usize::MAX,
        });
        for i in 0..5u32 {
            queue.mark_dirty(i, 1);
        }
        // Fail on the third item: 0,1 flush; 2 stays at the head.
        let mut seen = Vec::new();
        let err = queue
            .drain_budgeted(|item: &QueuedSave<u32>| -> Result<FlushOutcome, String> {
                if item.key == 2 {
                    return Err("injected flush failure".to_string());
                }
                seen.push(item.key);
                Ok(FlushOutcome {
                    wrote: true,
                    bytes_written: 1,
                })
            })
            .unwrap_err();
        assert_eq!(err, "injected flush failure");
        assert_eq!(seen, vec![0, 1]);
        assert_eq!(queue.len(), 3);
        // Retry succeeds: the failed item is still oldest-first.
        let stats = queue
            .flush_all(|item: &QueuedSave<u32>| {
                seen.push(item.key);
                Ok::<_, String>(FlushOutcome {
                    wrote: true,
                    bytes_written: 1,
                })
            })
            .unwrap();
        assert_eq!(stats.items_flushed, 3);
        assert_eq!(stats.remaining, 0);
        assert_eq!(seen, vec![0, 1, 2, 3, 4]);
        assert!(queue.is_empty());
    }

    #[test]
    fn perf_disk_save_write_if_changed_skips_identical_and_persists_changed() {
        let dir = test_dir("if-changed");
        let path = dir.join("snapshot.stv");
        let mut cache = PersistedFingerprints::new();

        // First commit writes.
        assert!(cache.write_atomic_if_changed(&path, b"v1").unwrap());
        let committed = std::fs::read(&path).unwrap();
        assert_eq!(committed, b"v1");
        let recorded = cache.persisted(&path).unwrap();

        // Identical content: no write, no mtime churn, no staging litter.
        assert!(!cache.write_atomic_if_changed(&path, b"v1").unwrap());
        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(leftovers.len(), 1);

        // A single flipped byte invalidates the fingerprint and persists.
        assert!(cache.write_atomic_if_changed(&path, b"v2").unwrap());
        assert_eq!(std::fs::read(&path).unwrap(), b"v2");
        assert_ne!(cache.persisted(&path).unwrap(), recorded);

        // A cache cleared between runs still skips through the file read.
        let mut cold = PersistedFingerprints::new();
        assert!(!cold.write_atomic_if_changed(&path, b"v2").unwrap());
        assert!(cold.write_atomic_if_changed(&path, b"v3").unwrap());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
