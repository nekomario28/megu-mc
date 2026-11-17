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

// ---------------------------------------------------------------------------
// XP orbs: the same bucket contract, a different merge rule.
//
// The orb merge is not the drop merge with a different item slot: the distance
// is `< 1.0` instead of `< 0.25`, the survivor is chosen by age with `<=` on the
// tie, there is no stack cap, and removals happen once after the scan, so an orb
// already eaten by an earlier `i` stays a candidate for a later one. Each of
// those is a way to be subtly wrong, so the reference below is the exact loop the
// fast version replaced, and the comparison covers the returned eat order as well
// as the surviving orbs.
// ---------------------------------------------------------------------------

fn orb(eid: i32, x: f64, y: f64, z: f64, val: i16, age: i32) -> Xp {
    Xp {
        eid,
        x,
        y,
        z,
        val,
        age,
    }
}

type OrbState = (Vec<(i32, i16, i32)>, Vec<i32>);

fn orb_state(xl: &[Xp], merged: Vec<i32>) -> OrbState {
    let mut live: Vec<(i32, i16, i32)> = xl
        .iter()
        .filter(|x| !merged.contains(&x.eid))
        .map(|x| (x.eid, x.val, x.age))
        .collect();
    live.sort_unstable();
    (live, merged)
}

/// The full pairwise scan, kept verbatim so the bucketed production version can
/// be held to it.
fn reference_orb_merge(mut xl: Vec<Xp>) -> OrbState {
    let mut merged: Vec<i32> = Vec::new();
    for i in 0..xl.len() {
        for j in (i + 1)..xl.len() {
            let (ax, ay, az, aage) = {
                let a = &xl[i];
                (a.x, a.y, a.z, a.age)
            };
            let (bx, by, bz, bage) = {
                let b = &xl[j];
                (b.x, b.y, b.z, b.age)
            };
            if (ax - bx).powi(2) + (ay - by).powi(2) + (az - bz).powi(2) < 1.0 {
                let (keep, eat) = if aage <= bage { (i, j) } else { (j, i) };
                xl[keep].val = xl[keep].val.saturating_add(xl[eat].val);
                merged.push(xl[eat].eid);
                break;
            }
        }
    }
    orb_state(&xl, merged)
}

fn bucketed_orb_merge(mut xl: Vec<Xp>) -> OrbState {
    let merged = merge_xp_orbs(&mut xl);
    orb_state(&xl, merged)
}

fn orb_scenario(index: usize) -> Vec<Xp> {
    match index {
        // Coincident orbs, all mergeable.
        0 => (0..8)
            .map(|i| orb(100 + i, 0.5, 70.0, 0.5, 1, i))
            .collect(),
        // x straddles an integer boundary: 16.01 and 16.99 are 0.98 apart but the
        // floors differ, so a bucket that only reads its own cell misses the pair.
        1 => vec![
            orb(201, 16.01, 70.0, 0.5, 1, 0),
            orb(202, 16.99, 70.0, 0.5, 1, 1),
            orb(203, 15.02, 70.0, 0.5, 1, 2),
            orb(204, 16.5, 70.0, 0.5, 1, 3),
        ],
        // The same straddle on y and on z.
        2 => vec![
            orb(301, 0.5, 70.01, 0.5, 1, 0),
            orb(302, 0.5, 70.99, 0.5, 1, 1),
            orb(303, 0.5, 69.02, 0.5, 1, 2),
            orb(304, 0.5, 70.0, 70.01, 1, 3),
            orb(305, 0.5, 70.0, 70.99, 1, 4),
        ],
        // Negative coordinates: floor is not trunc, so -0.01 and 0.5 are one cell
        // apart and must still be paired.
        3 => vec![
            orb(401, -0.01, 70.0, -0.01, 1, 0),
            orb(402, 0.5, 70.0, 0.5, 1, 1),
            orb(403, -1.0, 70.0, -0.99, 2, 2),
            orb(404, -0.5, 70.0, -0.5, 1, 3),
        ],
        // Age decides the survivor, and the tie must keep the lower index.
        4 => vec![
            orb(501, 0.5, 70.0, 0.5, 3, 9),
            orb(502, 0.5, 70.0, 0.5, 4, 2),
            orb(503, 0.5, 70.0, 0.5, 5, 2),
        ],
        // A chain: each step is within range of the one before it and out of range
        // of the one before that, so the outcome depends on scan order.
        5 => (0..6)
            .map(|i| orb(600 + i, 0.1 + i as f64 * 0.8, 70.0, 0.5, 1, i))
            .collect(),
        // Out of range pairs only: 1.0 apart is not < 1.0, so nothing merges and
        // the eat list must stay empty.
        6 => (0..10)
            .map(|i| orb(700 + i, i as f64, 70.0, 0.5, 1, i))
            .collect(),
        // The partner that only a `-1` offset reaches: it sits in the cell below on one
        // axis and nothing closer exists, so a neighbourhood missing that cell loses the
        // merge instead of replacing it. One pair per axis.
        7 => vec![
            orb(801, 16.01, 70.0, 0.5, 1, 0),
            orb(802, 15.02, 70.0, 0.5, 1, 1),
            orb(803, 0.5, 70.01, 0.5, 1, 2),
            orb(804, 0.5, 69.02, 0.5, 1, 3),
            orb(805, 0.5, 70.0, 70.01, 1, 4),
            orb(806, 0.5, 70.0, 69.02, 1, 5),
        ],
        // Deterministic scatter: dense enough that an eaten orb is still offered to
        // a later `i`, which is the shape that produces duplicate eat entries.
        _ => (0..60)
            .map(|i| {
                let h = (i as u64)
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                orb(
                    800 + i,
                    (h % 7) as f64 + (h >> 8) as f64 / 256.0,
                    (h >> 16) as f64 % 3.0,
                    (h >> 24) as f64 % 7.0,
                    (h % 5) as i16 + 1,
                    (h % 4) as i32,
                )
            })
            .collect(),
    }
}

const ORB_SCENARIO_COUNT: usize = 9;

#[test]
fn xp_merge_absorbs_exactly_what_the_full_pairwise_scan_absorbs() {
    for index in 0..ORB_SCENARIO_COUNT {
        let expected = reference_orb_merge(orb_scenario(index));
        let actual = bucketed_orb_merge(orb_scenario(index));
        assert_eq!(
            actual, expected,
            "orb scenario {index} diverged from the full pairwise scan"
        );
    }
}

#[test]
fn xp_merge_still_merges_at_all() {
    // A bucket that pairs nothing passes the differential only if the reference
    // pairs nothing too, so state the two cases that must merge.
    let coincident = bucketed_orb_merge(orb_scenario(0));
    assert_eq!(coincident.0.len(), 1, "coincident orbs must become one orb");
    assert_eq!(coincident.1.len(), 7, "seven orbs must be eaten");
    // The survivor carries the first pair's two values, not all eight: each `i`
    // absorbs its neighbour and is itself absorbed in turn, so the values added to
    // orbs 1..6 leave with them. That is the behaviour this optimization had to
    // preserve - it is not the sum - and the differential above is what pins it.
    assert_eq!(coincident.0[0].1, 2, "the survivor keeps the first pair's value");

    // A pair one integer boundary apart, where the cell changes on x, y and z.
    let straddle = bucketed_orb_merge(vec![
        orb(1, 16.99, 70.01, 0.01, 2, 0),
        orb(2, 16.01, 70.0, 0.0, 3, 1),
    ]);
    assert_eq!(straddle.0, vec![(1, 5, 0)], "the boundary pair must merge");
    assert_eq!(straddle.1, vec![2]);

    // And the pair that is exactly one block apart must not.
    let apart = bucketed_orb_merge(vec![orb(1, 0.5, 70.0, 0.5, 1, 0), orb(2, 1.5, 70.0, 0.5, 1, 0)]);
    assert_eq!(apart.0.len(), 2, "orbs 1.0 apart are not merged by vanilla");
    assert!(apart.1.is_empty());
}
