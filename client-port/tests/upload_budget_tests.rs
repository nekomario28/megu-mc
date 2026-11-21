// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
//! `ChunkUploadBudget`: per-frame upload admission, P95-driven adaptive
//! throttling, and burst-smooth spreading of join/teleport upload floods.

use mc_spine_client_mesh::upload_budget::{
    ChunkUploadBudget, DEFAULT_MAX_UPLOADS_PER_FRAME, DEFAULT_MAX_UPLOAD_BYTES_PER_FRAME,
    DEFAULT_MAX_UPLOAD_WORK_US_PER_FRAME, DEFAULT_MIN_UPLOADS_PER_FRAME, DEFAULT_TARGET_FRAME_US,
};

#[test]
fn defaults_and_presets_are_sane() {
    let budget = ChunkUploadBudget::new();
    assert_eq!(budget.max_uploads_per_frame, DEFAULT_MAX_UPLOADS_PER_FRAME);
    assert_eq!(budget.min_uploads_per_frame, DEFAULT_MIN_UPLOADS_PER_FRAME);
    assert_eq!(
        budget.max_upload_bytes_per_frame,
        DEFAULT_MAX_UPLOAD_BYTES_PER_FRAME
    );
    assert_eq!(
        budget.max_upload_work_us_per_frame,
        DEFAULT_MAX_UPLOAD_WORK_US_PER_FRAME
    );
    assert_eq!(budget.target_frame_us, DEFAULT_TARGET_FRAME_US);
    assert_eq!(budget.uploads_per_frame(), DEFAULT_MAX_UPLOADS_PER_FRAME);
    assert!(!budget.is_throttled());

    let low = ChunkUploadBudget::low_spec();
    assert_eq!(low.max_uploads_per_frame, 2);
    assert_eq!(low.max_upload_bytes_per_frame, 1024 * 1024);
    assert_eq!(low.max_upload_work_us_per_frame, 1_000);
    assert_eq!(low.uploads_per_frame(), 2);
}

#[test]
fn nothing_is_admitted_before_begin_frame() {
    let mut budget = ChunkUploadBudget::new();
    assert!(!budget.try_schedule(1, 1));
    assert_eq!(budget.used_uploads(), 0);
    assert_eq!(budget.used_bytes(), 0);
    assert_eq!(budget.used_work_us(), 0);
}

#[test]
fn admission_is_atomic_across_all_three_allowances() {
    let mut budget = ChunkUploadBudget::with_limits(4, 1_000, 100);
    budget.begin_frame();

    assert!(budget.try_schedule(600, 40));
    assert_eq!(budget.remaining_uploads(), 3);
    assert_eq!(budget.remaining_bytes(), 400);
    assert_eq!(budget.remaining_work_us(), 60);

    // Over the byte cap or the work cap: consumes nothing.
    assert!(!budget.try_schedule(500, 10));
    assert!(!budget.try_schedule(10, 70));
    assert_eq!(budget.used_uploads(), 1);
    assert_eq!(budget.used_bytes(), 600);
    assert_eq!(budget.used_work_us(), 40);

    // Count cap binds after three more uploads.
    assert!(budget.try_schedule(100, 10));
    assert!(budget.try_schedule(100, 10));
    assert!(budget.try_schedule(100, 10));
    assert_eq!(budget.used_uploads(), 4);
    assert!(!budget.try_schedule(1, 1), "count cap reached");

    // The next frame resets every allowance.
    budget.begin_frame();
    assert_eq!(budget.remaining_uploads(), 4);
    assert_eq!(budget.remaining_bytes(), 1_000);
    assert_eq!(budget.remaining_work_us(), 100);
}

#[test]
fn burst_of_200_chunks_spreads_smoothly_within_every_frame_budget() {
    // 20 KiB / 10 us per chunk; caps admit at most 3 chunks per frame
    // (the 64 KiB byte cap binds before the 4-count / 100 us caps).
    let mut budget = ChunkUploadBudget::with_limits(4, 64 * 1024, 100);
    let mut pending = 200u32;
    let mut frames = 0u32;

    while pending > 0 {
        frames += 1;
        assert!(frames <= 200, "burst must drain without hanging");
        budget.begin_frame();
        while pending > 0 && budget.try_schedule(20 * 1024, 10) {
            pending -= 1;
        }
        // No frame may ever exceed its microsecond budget — that is the
        // whole point of the throttle.
        assert!(budget.used_work_us() <= 100, "frame {frames} work cap");
        assert!(budget.used_bytes() <= 64 * 1024, "frame {frames} byte cap");
        assert!(budget.used_uploads() <= 4, "frame {frames} count cap");
        // And a partially consumed allowance must be whole-upload atomic.
        assert_eq!(budget.used_bytes() % (20 * 1024), 0);
        assert_eq!(budget.used_work_us() % 10, 0);
    }

    assert_eq!(frames, 67, "200 chunks at 3/frame drains in 67 frames");
}

#[test]
fn adapt_halves_on_p95_overrun_and_regrows_smoothly() {
    let mut budget = ChunkUploadBudget::new().with_target_frame_us(10_000);
    assert_eq!(budget.uploads_per_frame(), 8);

    // P95 at 125% of target: throttle 8 -> 4 -> 2 -> 1 (the floor).
    for expected in [4, 2, 1, 1] {
        budget.adapt(12_500.0);
        assert_eq!(budget.uploads_per_frame(), expected);
    }
    assert!(budget.is_throttled());

    // Inside the dead band (90%..110% of target): the count holds.
    budget.adapt(10_000.0);
    assert_eq!(budget.uploads_per_frame(), 1);

    // P95 under 90% of target: regrow one slot per frame, capped at max.
    for expected in [2, 3, 4, 5, 6, 7, 8, 8] {
        budget.adapt(9_000.0);
        assert_eq!(budget.uploads_per_frame(), expected);
    }
    assert!(!budget.is_throttled());
}

#[test]
fn adapt_fails_safe_on_non_finite_p95() {
    let mut budget = ChunkUploadBudget::new();
    budget.adapt(f64::NAN);
    assert_eq!(budget.uploads_per_frame(), 4);
    budget.adapt(f64::INFINITY);
    assert_eq!(budget.uploads_per_frame(), 2);
    budget.adapt(f64::NEG_INFINITY);
    assert_eq!(budget.uploads_per_frame(), 1);
}

#[test]
fn min_floor_and_max_cap_are_enforced() {
    let mut budget = ChunkUploadBudget::with_limits(6, 1024, 100)
        .with_min_uploads_per_frame(3)
        .with_target_frame_us(1_000);
    for _ in 0..10 {
        budget.adapt(f64::INFINITY);
    }
    assert_eq!(budget.uploads_per_frame(), 3, "never below the floor");

    for _ in 0..10 {
        budget.adapt(0.0);
    }
    assert_eq!(budget.uploads_per_frame(), 6, "never above the max");

    // Degenerate configurations still admit at least one upload so a dirty
    // queue can always drain.
    let zero = ChunkUploadBudget::with_limits(0, 0, 0);
    assert_eq!(zero.max_uploads_per_frame, 1);
}

#[test]
fn low_spec_burst_stays_inside_tighter_caps() {
    let mut budget = ChunkUploadBudget::low_spec();
    let mut pending = 50u32;
    let mut frames = 0u32;
    while pending > 0 {
        frames += 1;
        budget.begin_frame();
        while pending > 0 && budget.try_schedule(16 * 1024, 200) {
            pending -= 1;
        }
        assert!(budget.used_uploads() <= 2);
        assert!(budget.used_bytes() <= 1024 * 1024);
        assert!(budget.used_work_us() <= 1_000);
    }
    assert_eq!(frames, 25);
}
