// SPDX-License-Identifier: LGPL-3.0-or-later
//! Unit tests for `megu_core::diff_engine` — the universal
//! differential-computation primitives.

use megu_core::diff_engine::{
    DiffError, DirtySlotTracker, DirtySpan, FieldDiffBitset, SubtileDiffMask, XorRleDiff,
    XOR_RLE_MAGIC, XOR_RLE_MAX_INPUT_BYTES,
};

/// Deterministic pattern buffer: `byte[i] = (i * 131 + i >> 7) & 0xff`.
fn pattern(len: usize) -> Vec<u8> {
    (0..len)
        .map(|i| ((i * 131 + (i >> 7)) & 0xff) as u8)
        .collect()
}

/// Bitwise IEEE CRC32 — same polynomial as the codec's table-driven form,
/// used to hand-craft structurally valid hostile frames in the test.
fn test_crc32(bytes: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &b in bytes {
        crc ^= u32::from(b);
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                0xEDB8_8320 ^ (crc >> 1)
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

/// Minimal LEB128 encoder matching the codec's canonical form.
fn push_varint(out: &mut Vec<u8>, mut value: u64) {
    loop {
        let mut byte = (value & 0x7f) as u8;
        value >>= 7;
        if value != 0 {
            byte |= 0x80;
        }
        out.push(byte);
        if value == 0 {
            return;
        }
    }
}

/// Hand-rolled `XDF1` frame: magic + declared len varint + `body` + CRC32.
fn craft_frame(declared_len: u64, body: &[u8]) -> Vec<u8> {
    let mut frame = Vec::new();
    frame.extend_from_slice(&XOR_RLE_MAGIC);
    push_varint(&mut frame, declared_len);
    frame.extend_from_slice(body);
    let crc = test_crc32(&frame);
    frame.extend_from_slice(&crc.to_be_bytes());
    frame
}

#[test]
fn xor_rle_diff_zero_bytes_on_identical_and_tiny_patch_on_sparse_edit_roundtrips() {
    // Identical buffers diff to nothing — no allocation, no patch.
    let base = pattern(16 * 1024);
    assert_eq!(XorRleDiff::encode(&base, &base), Ok(None));

    // A 4-byte sparse edit inside 16 KiB compresses to a tiny frame.
    let mut next = base.clone();
    for i in 0..4 {
        next[4_096 + i] ^= 0xff;
    }
    let patch = XorRleDiff::encode(&base, &next)
        .expect("encode")
        .expect("buffers differ");
    assert!(
        patch.len() < 32,
        "patch of {} bytes for a 4-byte edit in 16 KiB",
        patch.len()
    );
    assert!(
        (patch.len() as f64) < base.len() as f64 * 0.002,
        "compression ratio must beat 99.8%"
    );
    assert_eq!(&patch[..4], &XOR_RLE_MAGIC);

    // Both apply forms round-trip bit-exactly.
    assert_eq!(XorRleDiff::apply(&base, &patch).expect("apply"), next);
    let mut in_place = base.clone();
    XorRleDiff::apply_in_place(&mut in_place, &patch).expect("apply_in_place");
    assert_eq!(in_place, next);

    // Dense and boundary edits round-trip too: first byte, last byte,
    // and an alternating byte-level pattern.
    for edit_at in [0usize, base.len() - 1] {
        let mut moved = base.clone();
        moved[edit_at] ^= 0xaa;
        let patch = XorRleDiff::encode(&base, &moved).unwrap().unwrap();
        assert_eq!(XorRleDiff::apply(&base, &patch).unwrap(), moved);
    }
    let alternating: Vec<u8> = base
        .iter()
        .enumerate()
        .map(|(i, &b)| if i % 2 == 0 { b ^ 0x55 } else { b })
        .collect();
    let patch = XorRleDiff::encode(&base, &alternating).unwrap().unwrap();
    assert_eq!(XorRleDiff::apply(&base, &patch).unwrap(), alternating);

    // Length mismatch and oversize inputs fail closed.
    assert!(matches!(
        XorRleDiff::encode(&base[..8], &next[..9]),
        Err(DiffError::LengthMismatch { .. })
    ));
    let oversized = vec![0u8; XOR_RLE_MAX_INPUT_BYTES + 1];
    assert!(matches!(
        XorRleDiff::encode(&oversized, &oversized),
        Err(DiffError::TooLarge { .. })
    ));
}

#[test]
fn xor_rle_diff_fails_closed_on_corrupt_crc_truncation_and_oob() {
    let base = pattern(64);
    let mut next = base.clone();
    next[10] ^= 0x77;
    let patch = XorRleDiff::encode(&base, &next).unwrap().unwrap();

    // Bad magic and CRC corruption are rejected before any write.
    let mut bad_magic = patch.clone();
    bad_magic[0] = b'!';
    assert!(matches!(
        XorRleDiff::apply(&base, &bad_magic),
        Err(DiffError::BadMagic)
    ));
    let mut corrupted = patch.clone();
    corrupted[8] ^= 0x01;
    assert!(matches!(
        XorRleDiff::apply(&base, &corrupted),
        Err(DiffError::CrcMismatch)
    ));

    // Truncated frames never apply — the CRC trailer is checked first, so
    // shorten the whole frame (loses the trailer) and also craft a body
    // whose record runs into the trailer region.
    assert!(matches!(
        XorRleDiff::apply(&base, &patch[..patch.len() - 2]),
        Err(DiffError::CrcMismatch | DiffError::Truncated)
    ));
    let mut body = Vec::new();
    push_varint(&mut body, 0); // skip 0
    push_varint(&mut body, 4); // lit 4, but no literal bytes follow
    let frame = craft_frame(64, &body);
    assert!(matches!(
        XorRleDiff::apply(&base, &frame),
        Err(DiffError::Truncated)
    ));

    // Out-of-bounds: a skip run that walks past the declared length, and a
    // literal run that does the same.
    let mut body = Vec::new();
    push_varint(&mut body, 100); // skip past end of a 64-byte buffer
    push_varint(&mut body, 0);
    let frame = craft_frame(64, &body);
    assert!(matches!(
        XorRleDiff::apply(&base, &frame),
        Err(DiffError::OutOfBounds)
    ));
    let mut body = Vec::new();
    push_varint(&mut body, 0);
    push_varint(&mut body, 1_000); // lit past end
    let frame = craft_frame(64, &body);
    assert!(matches!(
        XorRleDiff::apply(&base, &frame),
        Err(DiffError::OutOfBounds)
    ));

    // Trailing bytes between the last record and the CRC are rejected.
    let mut body = Vec::new();
    push_varint(&mut body, 0);
    push_varint(&mut body, 1);
    body.push(0xaa); // one literal
    body.extend_from_slice(&[0xde, 0xad]); // junk before CRC
    let frame = craft_frame(1, &body);
    assert!(matches!(
        XorRleDiff::apply(&base[..1], &frame),
        Err(DiffError::TrailingBytes)
    ));

    // Buffer-length mismatch against the declared frame length, and no
    // partial mutation on failure (apply_in_place leaves buf untouched).
    assert!(matches!(
        XorRleDiff::apply(&base[..32], &patch),
        Err(DiffError::LengthMismatch {
            declared: 64,
            actual: 32
        })
    ));
    let mut buf = base.clone();
    let _ = XorRleDiff::apply_in_place(&mut buf, &frame);
    assert_eq!(buf, base, "failed apply must not partially patch");

    // Non-canonical varints and non-progressing zero-literal records are
    // malformed, not silently accepted.
    let frame = craft_frame(8, &[0x80, 0x00, 0x01, 0x00, 0xaa]);
    assert!(matches!(
        XorRleDiff::apply(&base[..8], &frame),
        Err(DiffError::Malformed(_))
    ));
    let frame = craft_frame(8, &[0x00, 0x00]); // skip 0, lit 0 mid-frame
    assert!(matches!(
        XorRleDiff::apply(&base[..8], &frame),
        Err(DiffError::Malformed(_))
    ));
}

#[test]
fn dirty_slot_tracker_coalesces_adjacent_slots_and_skips_steady_frames() {
    let initial: Vec<u32> = (0..16).map(|i| i * 7).collect();
    let mut tracker = DirtySlotTracker::from_slice(&initial);

    // Steady frame: identical slice -> empty spans, zero bytes uploaded.
    let plan = tracker.sync_from_slice(&initial);
    assert!(plan.is_steady());
    assert_eq!(plan.spans, Vec::<DirtySpan>::new());
    let telemetry = tracker.telemetry();
    assert_eq!(telemetry.total_syncs, 1);
    assert_eq!(telemetry.zero_diff_syncs, 1);
    assert_eq!(telemetry.slots_uploaded, 0);
    assert_eq!(telemetry.slots_skipped, 16);

    // update_slot marks only real changes; identical writes are no-ops.
    assert!(!tracker.update_slot(4, initial[4]));
    assert!(!tracker.update_slot(64, 999)); // out of range: fail closed
    assert!(tracker.update_slot(4, 1));
    assert!(tracker.update_slot(5, 2));
    assert!(tracker.update_slot(6, 3));
    assert!(tracker.update_slot(12, 9));

    // The pending marks coalesce: {4,5,6} -> one span, {12} -> another.
    assert_eq!(
        tracker.take_dirty_spans(),
        [
            DirtySpan { start: 4, len: 3 },
            DirtySpan { start: 12, len: 1 }
        ]
    );
    assert_eq!(tracker.take_dirty_spans(), Vec::<DirtySpan>::new());

    // A fresh sync diffs the shadow and coalesces contiguous diffs.
    // update_slot already wrote [1, 2, 3] into shadow[4..7] and 9 into
    // shadow[12], so `next` must hold different values there to stay
    // dirty; slot 14 exercises the pending-mark path: `next` matches the
    // shadow value exactly, so only the explicit mark reports it dirty.
    let mut next = initial.clone();
    next[4] = 10;
    next[5] = 11;
    next[6] = 12;
    next[7] = 4; // adjacent: merges into the {4,5,6} run
    next[9] = 9;
    next[12] = 9; // matches the value update_slot pushed earlier
    assert!(tracker.update_slot(14, 77));
    next[14] = 77;
    let plan = tracker.sync_from_slice(&next);
    assert!(!plan.full_realloc);
    assert_eq!(
        plan.spans,
        [
            DirtySpan { start: 4, len: 4 },
            DirtySpan { start: 9, len: 1 },
            DirtySpan { start: 14, len: 1 }
        ]
    );
    assert_eq!(tracker.shadow(), next.as_slice());

    // Length changes trigger a full realloc plan, not a diff.
    let grown: Vec<u32> = (0..24).collect();
    let plan = tracker.sync_from_slice(&grown);
    assert!(plan.full_realloc);
    assert_eq!(plan.spans, [DirtySpan { start: 0, len: 24 }]);
}

#[test]
fn subtile_diff_mask_marks_neighbors_and_computes_tight_aabb() {
    // Interior block: exactly one sub-tile, tight 4³ voxel AABB.
    let mut mask = SubtileDiffMask::new();
    assert!(mask.is_empty());
    assert_eq!(mask.dirty_bounds(), None);
    assert!(mask.mark_block_with_neighbors(5, 9, 13));
    assert_eq!(mask.dirty_count(), 1);
    assert_eq!(mask.dirty_bounds(), Some(([4, 8, 12], [7, 11, 15])));

    // Boundary block touches the lower sub-tile on each boundary axis.
    let mut mask = SubtileDiffMask::new();
    assert!(mask.mark_block_with_neighbors(4, 5, 5)); // x edge
    assert_eq!(mask.dirty_count(), 2);
    let (min, max) = mask.dirty_bounds().expect("non-empty");
    assert_eq!((min, max), ([0, 4, 4], [7, 7, 7]));

    // A 3-axis sub-tile corner marks all 8 touching sub-tiles.
    let mut mask = SubtileDiffMask::new();
    assert!(mask.mark_block_with_neighbors(3, 3, 3));
    assert_eq!(mask.dirty_count(), 8);
    assert_eq!(mask.dirty_bounds(), Some(([0, 0, 0], [7, 7, 7])));

    // Section-edge blocks stay in bounds: x=15's upper neighbor does not
    // exist, only its own sub-tile marks.
    let mut mask = SubtileDiffMask::new();
    assert!(mask.mark_block_with_neighbors(15, 15, 15));
    assert_eq!(mask.dirty_count(), 1);

    // mark_block alone marks exactly the containing sub-tile; out of
    // range is a no-op.
    let mut mask = SubtileDiffMask::new();
    assert!(mask.mark_block(3, 0, 15));
    assert!(!mask.mark_block(16, 0, 0));
    assert_eq!(mask.dirty_count(), 1);

    // iter_dirty_subtiles yields ascending indices in O(popcount).
    let mut mask = SubtileDiffMask::new();
    for bit in [0u8, 5, 21, 63] {
        mask = SubtileDiffMask::from_bits(mask.bits() | (1u64 << bit));
    }
    let collected: Vec<u8> = mask.iter_dirty_subtiles().collect();
    assert_eq!(collected, [0, 5, 21, 63]);
    assert_eq!(
        mask.iter_dirty_subtiles()
            .map(SubtileDiffMask::subtile_coords)
            .collect::<Vec<_>>(),
        vec![[0, 0, 0], [1, 0, 1], [1, 1, 1], [3, 3, 3]]
    );
    // ALL marks every sub-tile; EMPTY none.
    assert_eq!(SubtileDiffMask::ALL.dirty_count(), 64);
    assert_eq!(
        SubtileDiffMask::ALL.dirty_bounds(),
        Some(([0, 0, 0], [15, 15, 15]))
    );
    assert_eq!(SubtileDiffMask::EMPTY.iter_dirty_subtiles().count(), 0);
}

#[test]
fn field_diff_bitset_zero_work_when_clean() {
    // A fresh tracker is clean: no mask, no indices, zero-cost take.
    let mut fields = FieldDiffBitset::new();
    assert!(fields.is_clean());
    assert_eq!(fields.dirty_count(), 0);
    assert_eq!(fields.take_dirty_mask(), 0);
    assert_eq!(fields.iter_dirty_indices().count(), 0);

    // Out-of-range fields fail closed; marking is idempotent.
    assert!(!fields.mark(64));
    assert!(!fields.mark(255));
    assert!(fields.mark(3));
    assert!(fields.mark(9));
    assert!(fields.mark(3));
    assert!(!fields.is_clean());
    assert!(fields.is_dirty(3));
    assert!(fields.is_dirty(9));
    assert!(!fields.is_dirty(4));
    assert!(!fields.is_dirty(64));
    assert_eq!(fields.dirty_mask(), (1 << 3) | (1 << 9));
    assert_eq!(fields.dirty_count(), 2);
    assert_eq!(fields.iter_dirty_indices().collect::<Vec<_>>(), [3, 9]);

    // take drains the mask and returns to the zero-work clean state.
    assert_eq!(fields.take_dirty_mask(), (1 << 3) | (1 << 9));
    assert!(fields.is_clean());
    assert_eq!(fields.take_dirty_mask(), 0);

    // Full sweep: every one of the 64 fields is individually trackable.
    for field in 0..64 {
        assert!(fields.mark(field));
    }
    assert_eq!(fields.dirty_mask(), u64::MAX);
    assert_eq!(fields.iter_dirty_indices().count(), 64);
}
