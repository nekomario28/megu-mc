use super::*;

// The production merge is exercised through drop_tick, so this drives the same
// code path rather than a reimplementation of it.
fn merge_outcome(items: Vec<Drop>) -> Vec<(i32, i64, u8)> {
    let _world_guard = world_guard();
    let old_drops = std::mem::take(&mut *drops().lock().unwrap());
    let old_forced = std::mem::take(&mut *forceloaded().lock().unwrap());
    let old_tick = TICK.swap(1, Ordering::Relaxed);
    let old_index = TICK_IDX.swap(0, Ordering::Relaxed);
    let (old_hot, old_warm) = test_region();
    forceloaded().lock().unwrap().insert((0, 0));
    *drops().lock().unwrap() = items;
    // One tick isolates the merge. More ticks would advance ages and merge again,
    // which is a separate concern covered by the entity-life gate.
    game_tick(|_, tick| tick());
    let mut outcome: Vec<(i32, i64, u8)> = drops()
        .lock()
        .unwrap()
        .iter()
        .map(|d| (d.eid, d.item, d.cnt))
        .collect();
    *drops().lock().unwrap() = old_drops;
    *forceloaded().lock().unwrap() = old_forced;
    TICK.store(old_tick, Ordering::Relaxed);
    TICK_IDX.store(old_index, Ordering::Relaxed);
    set_test_region(old_hot, old_warm);
    outcome.sort_unstable();
    outcome
}

fn drop(eid: i32, x: f64, y: f64, z: f64, item: i64, cnt: u8, age: i32) -> Drop {
    Drop {
        eid,
        x,
        y,
        z,
        item,
        cnt,
        comps: Vec::new(),
        age,
    }
}

/// Reference: the original full pairwise scan, kept verbatim so the bucketed
/// production version can be held to it.
fn reference_outcome(mut dp: Vec<Drop>) -> Vec<(i32, i64, u8)> {
    let mut merged: Vec<i32> = Vec::new();
    for i in 0..dp.len() {
        for j in (i + 1)..dp.len() {
            let (ax, ay, az, ait, acnt, aage) = {
                let a = &dp[i];
                (a.x, a.y, a.z, a.item, a.cnt, a.age)
            };
            let (bx, by, bz, bit, bcnt, bage) = {
                let b = &dp[j];
                (b.x, b.y, b.z, b.item, b.cnt, b.age)
            };
            if ait == bit
                && dp[i].comps == dp[j].comps
                && (ax - bx).powi(2) + (ay - by).powi(2) + (az - bz).powi(2) < 0.25
                && acnt as u16 + bcnt as u16 <= item_stack_max(ait, &dp[i].comps) as u16
            {
                let (keep, eat) = if aage <= bage { (i, j) } else { (j, i) };
                dp[keep].cnt = dp[keep].cnt.saturating_add(dp[eat].cnt);
                merged.push(dp[eat].eid);
                break;
            }
        }
    }
    let mut out: Vec<(i32, i64, u8)> = dp
        .into_iter()
        .filter(|d| !merged.contains(&d.eid))
        .map(|d| (d.eid, d.item, d.cnt))
        .collect();
    out.sort_unstable();
    out
}

fn scenario(index: usize) -> Vec<Drop> {
    match index {
        // Coincident drops, all mergeable.
        0 => (0..8)
            .map(|i| drop(100 + i, 0.5, 70.0, 0.5, 811, 1, i))
            .collect(),
        // Pairs straddling integer boundaries, where a cell bucket must still pair them.
        1 => (0..8)
            .map(|i| drop(200 + i, 16.0 + (i % 2) as f64 - 0.49, 70.0, 0.5, 811, 1, i))
            .collect(),
        // Vertical straddle on y.
        2 => (0..6)
            .map(|i| {
                drop(
                    300 + i,
                    0.5,
                    70.0 + (i % 2) as f64 - 0.49,
                    0.5,
                    811,
                    2,
                    i * 3,
                )
            })
            .collect(),
        // Mixed items and components: only same item plus same comps may merge.
        3 => vec![
            drop(401, 0.5, 70.0, 0.5, 811, 1, 5),
            Drop {
                eid: 402,
                x: 0.5,
                y: 70.0,
                z: 0.5,
                item: 811,
                cnt: 1,
                comps: vec![(2, b"{}".to_vec())],
                age: 5,
            },
            drop(403, 0.6, 70.0, 0.5, 811, 1, 5),
            drop(404, 0.5, 70.0, 0.5, 812, 1, 5),
            drop(405, 0.5, 70.0, 0.5, 811, 40, 5),
            drop(406, 0.5, 70.0, 0.5, 811, 30, 5),
        ],
        // Stack cap: 40 + 30 exceeds 64, so that pair must not merge.
        4 => vec![
            drop(501, 0.5, 70.0, 0.5, 811, 40, 1),
            drop(502, 0.5, 70.0, 0.5, 811, 30, 2),
        ],
        // Age decides which survives, and the kept count must carry both.
        5 => vec![
            drop(601, 0.5, 70.0, 0.5, 811, 3, 9),
            drop(602, 0.5, 70.0, 0.5, 811, 4, 2),
        ],
        // Far apart: nothing merges.
        6 => vec![
            drop(701, 0.5, 70.0, 0.5, 811, 1, 1),
            drop(702, 40.5, 70.0, 0.5, 811, 1, 1),
        ],
        // A chain where merging changes the count and blocks a later merge.
        7 => (0..5)
            .map(|i| drop(800 + i, 0.5 + i as f64 * 0.1, 70.0, 0.5, 811, 20, i))
            .collect(),
        // Deterministic scatter across cells.
        _ => (0..40)
            .map(|i| {
                let h = (i as u64)
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                drop(
                    900 + i,
                    (h % 17) as f64 + (h >> 8) as f64 / 256.0,
                    70.0 + (h >> 16) as f64 % 3.0,
                    900.0 + (h >> 24) as f64 % 40.0,
                    if h.is_multiple_of(3) { 811 } else { 812 },
                    (h % 5) as u8 + 1,
                    (h % 11) as i32,
                )
            })
            .collect(),
    }
}

const SCENARIO_COUNT: usize = 9;

#[test]
fn bucketed_merge_absorbs_exactly_what_the_full_pairwise_scan_absorbs() {
    for index in 0..SCENARIO_COUNT {
        let expected = reference_outcome(scenario(index));
        let actual = merge_outcome(scenario(index));
        assert_eq!(
            actual, expected,
            "scenario {index} diverged from the full pairwise scan"
        );
        assert!(!actual.is_empty(), "scenario {index} lost every drop");
    }
}

#[test]
fn bucketed_merge_still_merges_at_all() {
    // A guard against a bucket that pairs nothing: the coincident case must
    // collapse to a single stack carrying every count.
    let outcome = merge_outcome(scenario(0));
    assert_eq!(
        outcome.len(),
        1,
        "coincident drops must merge into one stack"
    );
    assert!(
        outcome[0].2 > 1,
        "the surviving stack must carry more than one drop, got {outcome:?}"
    );
}
