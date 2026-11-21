// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
//! Lane OPS-1 integration tests: the `AUD1` audit WAL + rollback engine
//! (`audit_log`) and the Chunky-style pregen controller + ticket-driven LRU
//! unloader (`pregen`), exercised end to end against a live `ServerContext`.

use mc_spine_state_store::{
    spiral_order, AuditAction, AuditLog, AuditQuery, AuditRecord, BlockCoord, ChunkCoord,
    ChunkTicketLruUnloader, PregenJob, PregenShape, PregenState, ServerContext, TicketKind,
};
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

static DIR_COUNTER: AtomicU64 = AtomicU64::new(0);

/// A per-test scratch dir under the system temp dir (TMPDIR-honoring).
struct TestDir(PathBuf);

impl TestDir {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "mc-spine-ops1-{}-{}-{tag}",
            std::process::id(),
            DIR_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }

    fn path(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

const OVERWORLD: &str = "minecraft:overworld";

fn record(
    tick: u64,
    actor: &str,
    pos: [i32; 3],
    action: AuditAction,
    old_state: u32,
    new_state: u32,
) -> AuditRecord {
    AuditRecord::new(tick, actor, OVERWORLD, pos, action, old_state, new_state)
}

// ---------------------------------------------------------------- audit WAL

#[test]
fn audit_wal_persists_and_recovers_every_intact_frame() {
    let dir = TestDir::new("wal-roundtrip");
    let path = dir.path("audit.aud1");

    let mut log = AuditLog::new();
    let mut placed = record(10, "Alice", [8, 64, 8], AuditAction::BlockPlace, 0, 42);
    let mut took = record(11, "Bob", [-16, 70, 3], AuditAction::ContainerTake, 5, 5);
    took.item_id = Some("minecraft:diamond".to_string());
    took.item_count = 3;
    let mut exploded = record(12, "#tnt", [40, 60, -9], AuditAction::Explosion, 7, 0);
    exploded.rolled_back = true; // an already-rolled-back record must persist the flag
    for r in [placed.clone(), took.clone(), exploded.clone()] {
        log.append_to_file(&path, r).unwrap();
    }
    placed.seq = 1;
    took.seq = 2;
    exploded.seq = 3;

    let (recovered, truncated) = AuditLog::recover_from_file(&path).unwrap();
    assert_eq!(truncated, 0);
    assert_eq!(recovered.records(), &[placed, took, exploded]);
    // Recovery rebuilt the spatial index: a radius query hits the two nearby
    // records (distances ~11 and ~17) but not the explosion ~41 blocks out.
    let near = recovered.query(&AuditQuery::new().within([0, 64, 0], 30));
    assert_eq!(near.len(), 2);
    assert!(near.iter().all(|r| r.actor != "#tnt"));
}

#[test]
fn audit_wal_mid_frame_crash_truncates_only_the_torn_tail() {
    let dir = TestDir::new("wal-crash");
    let path = dir.path("audit.aud1");

    let mut log = AuditLog::new();
    for i in 0..5 {
        log.append_to_file(
            &path,
            record(
                100 + i,
                "Alice",
                [i as i32 * 16, 64, 0],
                AuditAction::BlockPlace,
                0,
                i as u32 + 1,
            ),
        )
        .unwrap();
    }
    let clean_len = std::fs::metadata(&path).unwrap().len() as usize;

    // Crash while writing record 6: magic + declared length landed, half the
    // payload did not.
    let mut torn = AuditLog::encode_frame(&record(
        105,
        "Griefer",
        [80, 64, 0],
        AuditAction::BlockBreak,
        1,
        0,
    ));
    torn.truncate(8 + 6); // header + 6 payload bytes, no CRC
    {
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        file.write_all(&torn).unwrap();
    }

    assert_eq!(
        std::fs::metadata(&path).unwrap().len() as usize,
        clean_len + torn.len()
    );
    let (recovered, truncated) = AuditLog::recover_from_file(&path).unwrap();
    assert_eq!(
        recovered.len(),
        5,
        "every committed frame survives a torn write"
    );
    assert_eq!(
        truncated,
        torn.len(),
        "the torn tail is reported by byte count"
    );
    assert_eq!(
        recovered
            .records()
            .iter()
            .map(|r| r.seq)
            .collect::<Vec<_>>(),
        vec![1, 2, 3, 4, 5]
    );

    // Recovery keeps a consistent next seq: appending to the recovered log
    // resumes numbering instead of colliding with or skipping history.
    let mut recovered = recovered;
    assert_eq!(
        recovered.append(record(
            200,
            "Alice",
            [0, 64, 0],
            AuditAction::BlockPlace,
            0,
            9
        )),
        6
    );
}

#[test]
fn audit_wal_corrupt_frame_stops_replay_at_that_point() {
    let dir = TestDir::new("wal-corrupt");
    let path = dir.path("audit.aud1");

    let mut frames = Vec::new();
    for i in 0..3u64 {
        frames.extend_from_slice(&AuditLog::encode_frame(&{
            let mut r = record(
                10 + i,
                "Alice",
                [i as i32, 64, 0],
                AuditAction::BlockPlace,
                0,
                1,
            );
            r.seq = i + 1;
            r
        }));
    }
    // Corrupt a payload byte inside frame 2 — its CRC no longer matches, and
    // an append-only log cannot trust anything written after it.
    let second_frame = AuditLog::encode_frame(&record(
        11,
        "Alice",
        [1, 64, 0],
        AuditAction::BlockPlace,
        0,
        1,
    ));
    let corrupt_at = second_frame.len() + 12; // inside frame 2's payload
    frames[corrupt_at] ^= 0x5a;
    std::fs::write(&path, &frames).unwrap();

    let (recovered, truncated) = AuditLog::recover_from_file(&path).unwrap();
    assert_eq!(recovered.len(), 1);
    assert_eq!(truncated, frames.len() - second_frame.len());
}

// ----------------------------------------------------------- grief rollback

#[test]
fn grief_rollback_restores_exact_originals_and_restore_replays() {
    let ctx = ServerContext::new();
    let dim = ctx.ensure_dimension(OVERWORLD);
    let mut log = AuditLog::new();

    // Alice builds a house at tick 10: three distinct blocks.
    let house = [
        ([8, 64, 8], 100u32), // wall block
        ([9, 64, 8], 101),    // second wall
        ([8, 64, 9], 102),    // roof
    ];
    for &(pos, state) in &house {
        dim.set_block(BlockCoord::new(pos[0], pos[1], pos[2]), state);
        log.append(record(10, "Alice", pos, AuditAction::BlockPlace, 0, state));
    }

    // An early griefer edit *before* the query window (must survive).
    dim.set_block(BlockCoord::new(8, 65, 8), 55);
    log.append(record(
        5,
        "Griefer",
        [8, 65, 8],
        AuditAction::BlockPlace,
        0,
        55,
    ));

    // Griefer strikes at tick 20: TNT on the wall, break the roof, plus a
    // far-away break outside the query radius (must survive).
    dim.set_block(BlockCoord::new(8, 64, 8), 999);
    log.append(record(
        20,
        "Griefer",
        [8, 64, 8],
        AuditAction::BlockPlace,
        100,
        999,
    ));
    dim.set_block(BlockCoord::new(8, 64, 9), 0);
    log.append(record(
        21,
        "Griefer",
        [8, 64, 9],
        AuditAction::BlockBreak,
        102,
        0,
    ));
    dim.set_block(BlockCoord::new(160, 64, 160), 0);
    log.append(record(
        22,
        "Griefer",
        [160, 64, 160],
        AuditAction::BlockBreak,
        7,
        0,
    ));

    // Bob's edit inside the radius is not the griefer's — must survive.
    dim.set_block(BlockCoord::new(9, 64, 9), 77);
    log.append(record(
        23,
        "Bob",
        [9, 64, 9],
        AuditAction::BlockPlace,
        0,
        77,
    ));

    let query = AuditQuery::new()
        .actor("Griefer")
        .within([8, 64, 8], 16)
        .since_tick(15);
    let deltas = log.rollback_to_context(&ctx, &query);

    // Two matching block mutations, replayed newest-first.
    assert_eq!(deltas.len(), 2);
    assert!(deltas[0].seq > deltas[1].seq);
    assert_eq!(deltas[0].restored_state, 102); // roof back
    assert_eq!(deltas[1].restored_state, 100); // wall back

    // Alice's EXACT originals are live in the context again.
    for &(pos, state) in &house {
        assert_eq!(
            dim.block(BlockCoord::new(pos[0], pos[1], pos[2])),
            Some(state),
            "Alice's block at {pos:?} must be restored"
        );
    }
    // Out-of-window griefer edit, pre-window griefer edit and Bob's edit all
    // survive the rollback.
    assert_eq!(dim.block(BlockCoord::new(160, 64, 160)), Some(0));
    assert_eq!(dim.block(BlockCoord::new(8, 65, 8)), Some(55));
    assert_eq!(dim.block(BlockCoord::new(9, 64, 9)), Some(77));
    // The two rolled-back records are flagged; the rest are untouched.
    assert_eq!(
        log.records()
            .iter()
            .filter(|r| r.rolled_back)
            .map(|r| r.seq)
            .collect::<HashSet<_>>(),
        [deltas[0].seq, deltas[1].seq].into_iter().collect()
    );

    // A second identical rollback changes nothing.
    assert!(log.rollback_blocks(&query).is_empty());

    // Restore re-applies the griefing in forward order and clears the flags.
    let restored = log.restore_to_context(&ctx, &query);
    assert_eq!(restored.len(), 2);
    assert!(restored[0].seq < restored[1].seq);
    assert_eq!(dim.block(BlockCoord::new(8, 64, 8)), Some(999));
    assert_eq!(dim.block(BlockCoord::new(8, 64, 9)), Some(0));
    assert!(log.records().iter().all(|r| !r.rolled_back));
}

// ------------------------------------------------------------------ pregen

#[test]
fn pregen_spiral_covers_the_square_exactly_once() {
    let mut job = PregenJob::new(0, 0, 5, PregenShape::Square);
    assert_eq!(job.progress().total, 121);

    let mut seen = Vec::new();
    loop {
        let batch = job.take_batch(16);
        if batch.is_empty() {
            break;
        }
        job.mark_done(batch.len());
        seen.extend(batch);
    }

    assert_eq!(seen.len(), 121);
    let unique: HashSet<_> = seen.iter().copied().collect();
    assert_eq!(unique.len(), 121, "zero duplicate chunks");
    assert_eq!(seen[0], (0, 0), "the spiral starts at the center");
    for x in -5..=5 {
        for z in -5..=5 {
            assert!(unique.contains(&(x, z)), "chunk ({x}, {z}) must be covered");
        }
    }
    // The emitted order is identical to the reference spiral_order and its
    // Chebyshev ring index never decreases (strictly outward).
    assert_eq!(seen, spiral_order(0, 0, 5, PregenShape::Square));
    let mut last_ring = 0;
    for &(cx, cz) in &seen {
        let ring = cx.abs().max(cz.abs());
        assert!(ring >= last_ring);
        last_ring = ring;
    }

    let progress = job.progress();
    assert_eq!(progress.state, PregenState::Completed);
    assert_eq!(progress.done, 121);
    assert!((progress.percent - 100.0).abs() < f64::EPSILON);
}

#[test]
fn pregen_pause_resume_preserves_the_exact_spiral_cursor() {
    let mut job = PregenJob::new(3, 3, 2, PregenShape::Square);
    let expected = spiral_order(3, 3, 2, PregenShape::Square);

    let first = job.take_batch(7);
    assert_eq!(first.as_slice(), &expected[..7]);
    job.mark_done(first.len());
    job.pause();
    assert_eq!(job.progress().state, PregenState::Paused);
    assert!(job.take_batch(10).is_empty(), "paused jobs emit nothing");

    job.resume();
    assert_eq!(job.progress().state, PregenState::Running);
    let rest = job.take_batch(expected.len());
    assert_eq!(
        rest.as_slice(),
        &expected[7..],
        "resume continues the exact cursor"
    );
    job.mark_done(rest.len());
    assert_eq!(job.progress().state, PregenState::Completed);
}

#[test]
fn pregen_cancel_and_circle_shape() {
    let mut job = PregenJob::new(0, 0, 5, PregenShape::Circle);
    let total = job.progress().total as usize;
    let all = job.take_batch(total);
    assert_eq!(all.len(), total);
    let unique: HashSet<_> = all.iter().copied().collect();
    assert_eq!(unique.len(), total);
    assert!(all
        .iter()
        .all(|&(cx, cz)| cx as i64 * cx as i64 + cz as i64 * cz as i64 <= 25));

    job.cancel();
    assert_eq!(job.progress().state, PregenState::Cancelled);
    assert!(job.take_batch(1).is_empty());
    // Cancelled jobs cannot be resurrected.
    job.resume();
    assert_eq!(job.progress().state, PregenState::Cancelled);
}

#[test]
fn pregen_drive_tick_marks_chunks_resident_for_the_unloader() {
    let ctx = ServerContext::new();
    let mut job = PregenJob::new(0, 0, 1, PregenShape::Square); // 9 chunks
    let n = job.drive_tick(&ctx, OVERWORLD, 9, |_, _| {});
    assert_eq!(n, 9);
    assert_eq!(ctx.dimension(OVERWORLD).unwrap().overlay_count(), 9);
    assert_eq!(job.progress().state, PregenState::Completed);
}

// --------------------------------------------------- ticket LRU unloader

#[test]
fn unloader_never_evicts_ticketed_chunks_and_evicts_lru_first() {
    let ctx = ServerContext::new();
    let dim = ctx.ensure_dimension(OVERWORLD);
    let mut unloader = ChunkTicketLruUnloader::new();

    // Six resident exploration chunks, touched at increasing ticks.
    for i in 0..6i32 {
        let coord = ChunkCoord::new(i, 0);
        dim.with_chunks_write(&[coord], |set| {
            set.set_block(BlockCoord::new(i * 16, 64, 0), 10 + i as u32);
        });
        unloader.touch(OVERWORLD, i, 0, 100 + i as u64);
    }
    // Chunk 0 is forceloaded (OpenPAC), chunk 1 is inside a player view,
    // chunk 2 holds a temporary ticket that expires at tick 1_000.
    unloader.acquire(OVERWORLD, 0, 0, TicketKind::Forceload, 200);
    unloader.acquire(OVERWORLD, 1, 0, TicketKind::PlayerView, 200);
    unloader.acquire(OVERWORLD, 2, 0, TicketKind::Temporary(1_000), 200);

    // Cap unticketed residents at 1 with no grace: the two oldest unticketed
    // chunks (3 then 4, by touch tick) are evicted; ticketed chunks are
    // untouchable even though they are "oldest".
    let evict = unloader.collect_evictable(500, 1, 0);
    assert_eq!(
        evict,
        vec![(OVERWORLD.to_string(), 3, 0), (OVERWORLD.to_string(), 4, 0),]
    );

    // Grace period: nothing is old enough to evict at tick 110.
    assert!(unloader.collect_evictable(110, 0, 50).is_empty());

    let flushed = unloader.evict_from_context(&ctx, 500, 1, 0);
    assert_eq!(flushed.len(), 2);
    // LRU order is preserved and the flushed overlays keep every edit.
    assert_eq!(flushed[0].coord, ChunkCoord::new(3, 0));
    assert_eq!(flushed[1].coord, ChunkCoord::new(4, 0));
    assert_eq!(flushed[0].overlay.block((0, 64, 0)), Some(13));
    assert_eq!(flushed[1].overlay.block((0, 64, 0)), Some(14));

    // Ticketed chunks are still fully resident; evicted ones are gone.
    assert!(dim.block(BlockCoord::new(0, 64, 0)).is_some());
    assert!(dim.block(BlockCoord::new(16, 64, 0)).is_some());
    assert!(dim.block(BlockCoord::new(32, 64, 0)).is_some());
    assert_eq!(dim.block(BlockCoord::new(48, 64, 0)), None);
    assert_eq!(dim.block(BlockCoord::new(64, 64, 0)), None);
    assert!(dim.block(BlockCoord::new(80, 64, 0)).is_some());

    // After the temporary ticket expires, chunk 2 joins the unticketed set;
    // strict LRU order still puts the older-touched chunk 5 first.
    let evict = unloader.collect_evictable(1_001, 0, 0);
    assert_eq!(
        evict,
        vec![(OVERWORLD.to_string(), 5, 0), (OVERWORLD.to_string(), 2, 0),]
    );
}

#[test]
fn unloader_lru_tie_break_is_deterministic() {
    let mut unloader = ChunkTicketLruUnloader::new();
    // Same last-used tick on purpose: order must fall back to (dim, cx, cz)
    // with "a:first" < "minecraft:overworld" < "z:last" lexicographically.
    for (dim_name, cx, cz) in [
        (OVERWORLD, 9, 0),
        (OVERWORLD, 1, 0),
        ("a:first", 0, 0),
        (OVERWORLD, 1, -7),
        ("z:last", 0, 0),
    ] {
        unloader.touch(dim_name, cx, cz, 50);
    }
    let evict = unloader.collect_evictable(100, 0, 10);
    assert_eq!(
        evict,
        vec![
            ("a:first".to_string(), 0, 0),
            (OVERWORLD.to_string(), 1, -7),
            (OVERWORLD.to_string(), 1, 0),
            (OVERWORLD.to_string(), 9, 0),
            ("z:last".to_string(), 0, 0),
        ]
    );
}

#[test]
fn unloader_reference_counts_and_release() {
    let mut unloader = ChunkTicketLruUnloader::new();
    // Two players see the same chunk: two PlayerView references.
    unloader.acquire(OVERWORLD, 4, 4, TicketKind::PlayerView, 10);
    unloader.acquire(OVERWORLD, 4, 4, TicketKind::PlayerView, 10);
    assert_eq!(unloader.active_tickets(OVERWORLD, 4, 4, 20), 2);
    assert!(unloader.release(OVERWORLD, 4, 4, TicketKind::PlayerView, 20));
    assert!(unloader.is_protected(OVERWORLD, 4, 4, 30));
    assert!(unloader.release(OVERWORLD, 4, 4, TicketKind::PlayerView, 30));
    assert!(!unloader.is_protected(OVERWORLD, 4, 4, 30));
    // Releasing a ticket the chunk never held reports false.
    assert!(!unloader.release(OVERWORLD, 4, 4, TicketKind::Spawn, 30));
    // The released chunk re-enters the LRU at its release tick.
    assert_eq!(unloader.last_used_tick(OVERWORLD, 4, 4), Some(30));
    assert_eq!(
        unloader.collect_evictable(100, 0, 0),
        vec![(OVERWORLD.to_string(), 4, 4)]
    );
}
