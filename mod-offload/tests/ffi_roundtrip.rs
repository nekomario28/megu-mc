// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later

use std::{mem::MaybeUninit, ptr, slice};

use mod_offload::ffi::*;
use mod_offload::region_scheduler::{FIRST_ADAPTER_KIND, MAX_BATCH_RECORDS};

#[test]
fn handles_are_independent_and_validation_preserves_state() {
    // SAFETY: Every non-null handle stays live and exclusive until freed,
    // and every non-null output is a unique writable stack allocation.
    unsafe {
        let mut a = ptr::null_mut();
        let mut b = ptr::null_mut();
        assert_eq!(mso_region_create(3, 4, 5, &mut a), MsoStatus::Ok);
        assert_eq!(mso_region_create(3, 6, 5, &mut b), MsoStatus::Ok);
        let mut step = MsoStepReport::default();
        assert_eq!(mso_transport_add(a, 1, 0, 2), MsoStatus::InvalidArgument);
        assert_eq!(mso_transport_add(a, 1, 1, 2), MsoStatus::Ok);
        assert_eq!(mso_transport_add(a, 1, 1, 2), MsoStatus::AlreadyExists);
        assert_eq!(mso_transport_insert(b, 1, 20), MsoStatus::NotFound);
        assert_eq!(mso_transport_insert(a, 1, 20), MsoStatus::Ok);
        assert_eq!(mso_transport_insert(a, 1, 21), MsoStatus::Full);
        assert_eq!(
            mso_region_step(a, 2, 1, u64::MAX, ptr::null_mut()),
            MsoStatus::InvalidArgument
        );
        assert_eq!(mso_region_step(b, 9, 1, u64::MAX, &mut step), MsoStatus::Ok);
        assert_eq!(step.processed, 0);
        assert_eq!(
            mso_region_step(b, 8, 1, u64::MAX, &mut step),
            MsoStatus::TimeWentBackwards
        );
        assert_eq!(mso_region_step(a, 2, 1, u64::MAX, &mut step), MsoStatus::Ok);
        assert_eq!(step.processed, 1);
        assert_eq!(
            mso_transport_extract(a, 1, ptr::null_mut()),
            MsoStatus::InvalidArgument
        );
        let mut item = MsoItem::default();
        assert_eq!(mso_transport_extract(a, 1, &mut item), MsoStatus::Ok);
        assert_eq!((item.present, item.item), (1, 20));
        assert_eq!(mso_transport_extract(a, 1, &mut item), MsoStatus::Ok);
        assert_eq!((item.present, item.item), (0, 0));
        assert_eq!(mso_region_free(a), MsoStatus::Ok);
        assert_eq!(mso_region_free(b), MsoStatus::Ok);
    }
}

#[test]
fn output_backpressure_resumes_staged_fluid_updates_in_sequence() {
    // SAFETY: All buffers and the handle are live, disjoint allocations. Only
    // the reported, initialized prefix of the uninitialized buffer is read.
    unsafe {
        let mut region = ptr::null_mut();
        assert_eq!(mso_region_create(0, 0, 0, &mut region), MsoStatus::Ok);
        assert_eq!(mso_fluid_add(region, 0, 10, 0), MsoStatus::Ok);
        assert_eq!(mso_fluid_add(region, 1, 10, 0), MsoStatus::Ok);
        assert_eq!(mso_fluid_link(region, 0, 1, 1, 0), MsoStatus::Ok);
        let mut amount = 0;
        assert_eq!(
            mso_fluid_insert(region, 0, 5, 6, &mut amount),
            MsoStatus::Ok
        );
        assert_eq!(amount, 6);
        for _ in 0..MAX_BATCH_RECORDS - 1 {
            assert_eq!(
                mso_region_queue_delta(region, FIRST_ADAPTER_KIND, 99, ptr::null(), 0),
                MsoStatus::Ok
            );
        }
        let mut step = MsoStepReport::default();
        assert_eq!(
            mso_region_step(region, 10, 10, u64::MAX, &mut step),
            MsoStatus::Ok
        );
        assert_eq!(
            (
                step.processed,
                step.has_backlog,
                step.backlog_tick,
                step.output_blocked
            ),
            (1, 1, 0, 1)
        );

        let mut frame = [MaybeUninit::<u8>::uninit(); 128 * 1024];
        let mut batch = MsoBatchReport::default();
        let buffer = frame.as_mut_ptr().cast::<u8>();
        assert_eq!(
            mso_region_export(
                region,
                buffer,
                frame.len(),
                MAX_BATCH_RECORDS - 1,
                &mut batch
            ),
            MsoStatus::Ok
        );
        assert_eq!((batch.records, batch.sequence), (MAX_BATCH_RECORDS - 1, 1));
        assert_eq!(
            mso_region_step(region, 10, 10, u64::MAX, &mut step),
            MsoStatus::Ok
        );
        assert_eq!(
            (step.processed, step.has_backlog, step.output_blocked),
            (1, 0, 0)
        );
        assert_eq!(
            mso_region_export(region, buffer, frame.len(), MAX_BATCH_RECORDS, &mut batch),
            MsoStatus::Ok
        );
        assert_eq!((batch.bytes, batch.records, batch.sequence), (82, 2, 2));
        let bytes = slice::from_raw_parts(buffer, batch.bytes);
        assert_eq!(&bytes[..16], b"S5\x01\x01\x02\0\0\0\x02\0\0\0\0\0\0\0");
        let (records, tail) = bytes[32..].as_chunks::<25>();
        assert!(tail.is_empty());
        for (id, record) in records.iter().enumerate() {
            assert_eq!(&record[..8], &[0; 8]);
            assert_eq!(record[8], id as u8);
            assert_eq!(&record[12..], &[2, 0, 9, 0, 1, 5, 0, 0, 0, 3, 0, 0, 0]);
        }
        assert_eq!(mso_region_free(region), MsoStatus::Ok);
    }
}

#[test]
fn contraption_diffs_and_pose_roundtrip_with_atomic_validation() {
    let block = |x, state| MsoBlockDiff {
        position: [x, 0, 0],
        state,
        present: 1,
    };
    // SAFETY: The handle is exclusive and live until freed. The buffers are
    // disjoint and only the written prefix of the export buffer is read.
    unsafe {
        let mut body = ptr::null_mut();
        let seeds = [block(0, 0), block(3, 7)];
        let duplicate = [seeds[0]; 2];
        assert_eq!(
            mso_contraption_create(-2, 3, -4, 42, duplicate.as_ptr(), 2, &mut body),
            MsoStatus::InvalidArgument
        );
        assert!(body.is_null());
        assert_eq!(
            mso_contraption_create(-2, 3, -4, 42, seeds.as_ptr(), 2, &mut body),
            MsoStatus::Ok
        );
        let mut output = MsoBatchReport::default();
        assert_eq!(
            mso_contraption_export_delta(body, ptr::null_mut(), 0, 2, &mut output),
            MsoStatus::Ok
        );
        assert_eq!((output.bytes, output.sequence), (0, 0));

        let removed = MsoBlockDiff {
            present: 0,
            ..seeds[1]
        };
        let edits = [removed, seeds[0], block(-2, u32::MAX)];
        assert_eq!(
            mso_contraption_apply_diffs(body, 4, edits.as_ptr(), usize::MAX),
            MsoStatus::InvalidArgument
        );
        assert_eq!(
            mso_contraption_apply_diffs(body, 4, duplicate.as_ptr(), duplicate.len()),
            MsoStatus::InvalidArgument
        );
        assert_eq!(
            mso_contraption_apply_diffs(body, 4, edits.as_ptr(), edits.len()),
            MsoStatus::Ok
        );
        assert_eq!(
            mso_contraption_apply_diffs(body, 4, edits.as_ptr(), edits.len()),
            MsoStatus::Ok
        );

        let pose = MsoContraptionPose {
            position: [10.0, 2.0, -4.0],
            yaw: 90.0,
            pitch: 0.0,
            linear_velocity: [1.0, 2.0, 3.0],
            angular_velocity: [4.0, 5.0, 6.0],
        };
        assert_eq!(mso_contraption_set_pose(body, 5, pose), MsoStatus::Ok);
        assert_eq!(
            mso_contraption_set_pose(
                body,
                6,
                MsoContraptionPose {
                    pitch: f32::NAN,
                    ..pose
                }
            ),
            MsoStatus::InvalidArgument
        );
        assert_eq!(
            mso_contraption_set_pose(
                body,
                6,
                MsoContraptionPose {
                    angular_velocity: [f32::INFINITY; 3],
                    ..pose
                }
            ),
            MsoStatus::InvalidArgument
        );
        let mut actor = MsoActor {
            bounds: MsoAabb {
                min: [10.25, 2.25, -4.75],
                max: [10.75, 2.75, -4.25],
            },
            previous_anchor: [0.5; 3],
            carried: 1,
        };
        let mut collision = MsoActorCollision::default();
        assert_eq!(
            mso_contraption_collide_actor(body, actor, &mut collision),
            MsoStatus::Ok
        );
        assert_eq!(
            (collision.intersects, collision.delta),
            (1, [10.0, 2.0, -5.0])
        );
        actor.bounds.min[0] += 2.0;
        actor.bounds.max[0] += 2.0;
        assert_eq!(
            mso_contraption_collide_actor(body, actor, &mut collision),
            MsoStatus::Ok
        );
        assert_eq!(
            (collision.intersects, collision.delta),
            (0, [10.0, 2.0, -5.0])
        );
        actor.carried = 2;
        assert_eq!(
            mso_contraption_collide_actor(body, actor, &mut collision),
            MsoStatus::InvalidArgument
        );
        assert_eq!(collision.delta, [10.0, 2.0, -5.0]);
        actor.carried = 1;
        assert_eq!(mso_contraption_set_pose(body, 5, pose), MsoStatus::Ok);
        assert_eq!(
            mso_contraption_collide_actor(body, actor, &mut collision),
            MsoStatus::Ok
        );
        assert_eq!(collision.delta, [0.0; 3]);

        let mut storage = [MaybeUninit::<u8>::uninit(); 104];
        let buffer = storage.as_mut_ptr().cast();
        assert_eq!(
            mso_contraption_export_delta(body, ptr::null_mut(), 0, 2, &mut output),
            MsoStatus::BufferTooSmall
        );
        assert_eq!(
            (
                output.bytes,
                output.records,
                output.required_bytes,
                output.sequence
            ),
            (0, 0, 82, 0)
        );
        assert_eq!(
            mso_contraption_export_delta(body, buffer, 82, 2, &mut output),
            MsoStatus::Ok
        );
        assert_eq!((output.bytes, output.records, output.sequence), (82, 1, 1));
        let area = b"\xfe\xff\xff\xff\x03\0\0\0\xfc\xff\xff\xff\0\0\0\0";
        let diffs = [
            b"S5\x01\x01\x01\0\0\0\x01\0\0\0\0\0\0\0".as_slice(),
            area,
            b"\x04\0\0\0\0\0\0\0\x2a\0\0\0\x02\x01\x22\0",
            b"\xfe\xff\xff\xff\0\0\0\0\0\0\0\0\x01\xff\xff\xff\xff",
            b"\x03\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0",
        ]
        .concat();
        assert_eq!(slice::from_raw_parts(buffer, output.bytes), diffs);
        assert_eq!(
            mso_contraption_export_delta(body, buffer, storage.len(), 2, &mut output),
            MsoStatus::Ok
        );
        assert_eq!((output.bytes, output.records, output.sequence), (104, 1, 2));
        let mut motion = [
            b"S5\x01\x01\x01\0\0\0\x02\0\0\0\0\0\0\0".as_slice(),
            area,
            b"\x05\0\0\0\0\0\0\0\x2a\0\0\0\x01\x01\x38\0",
        ]
        .concat();
        for v in [10.0_f64, 2.0, -4.0] {
            motion.extend(v.to_le_bytes());
        }
        for v in [90.0_f32, 0.0, 1.0, 2.0, 3.0, 4.0, 5.0, 6.0] {
            motion.extend(v.to_le_bytes());
        }
        assert_eq!(slice::from_raw_parts(buffer, output.bytes), motion);
        assert_eq!(
            mso_contraption_export_delta(body, ptr::null_mut(), 0, 2, &mut output),
            MsoStatus::Ok
        );
        assert_eq!((output.bytes, output.sequence), (0, 0));
        assert_eq!(mso_contraption_free(body), MsoStatus::Ok);
    }
}

#[test]
fn contraption_rejects_oversized_and_backpressured_edits_before_mutating() {
    let edits: Vec<_> = (0..3856)
        .map(|x| MsoBlockDiff {
            position: [x, 0, 0],
            state: 1,
            present: 1,
        })
        .collect();
    let actor = MsoActor {
        bounds: MsoAabb {
            min: [0.25; 3],
            max: [0.75; 3],
        },
        ..MsoActor::default()
    };
    // SAFETY: The seed is empty and all nonempty arrays and outputs
    // are live disjoint buffers. The handle is owned until freed.
    unsafe {
        let mut body = ptr::null_mut();
        assert_eq!(
            mso_contraption_create(0, 0, 0, 0, ptr::null(), 0, &mut body),
            MsoStatus::Ok
        );
        assert_eq!(
            mso_contraption_apply_diffs(body, 9, edits.as_ptr(), edits.len()),
            MsoStatus::InvalidArgument
        );
        let mut collision = MsoActorCollision::default();
        assert_eq!(
            mso_contraption_collide_actor(body, actor, &mut collision),
            MsoStatus::Ok
        );
        assert_eq!(collision.intersects, 0);
        // Three near-limit records fit before the 256 KiB byte cap.
        for tick in 1..=3 {
            let present = u8::from(tick != 2);
            let changes: Vec<_> = edits[..3855]
                .iter()
                .map(|edit| MsoBlockDiff { present, ..*edit })
                .collect();
            assert_eq!(
                mso_contraption_apply_diffs(body, tick, changes.as_ptr(), changes.len()),
                MsoStatus::Ok
            );
        }
        let removals: Vec<_> = edits[..3855]
            .iter()
            .map(|edit| MsoBlockDiff {
                present: 0,
                ..*edit
            })
            .collect();
        assert_eq!(
            mso_contraption_apply_diffs(body, 4, removals.as_ptr(), removals.len()),
            MsoStatus::Full
        );
        assert_eq!(
            mso_contraption_collide_actor(body, actor, &mut collision),
            MsoStatus::Ok
        );
        assert_eq!(collision.intersects, 1);
        let mut storage = vec![0; 32 + 16 + 3855 * 17];
        let mut report = MsoBatchReport::default();
        assert_eq!(
            mso_contraption_export_delta(body, storage.as_mut_ptr(), storage.len(), 1, &mut report),
            MsoStatus::Ok
        );
        assert_eq!(
            (report.bytes, report.records, report.sequence),
            (storage.len(), 1, 1)
        );
        assert_eq!(
            mso_contraption_apply_diffs(body, 4, removals.as_ptr(), removals.len()),
            MsoStatus::Ok
        );
        assert_eq!(
            mso_contraption_collide_actor(body, actor, &mut collision),
            MsoStatus::Ok
        );
        assert_eq!(collision.intersects, 0);
        assert_eq!(mso_contraption_free(body), MsoStatus::Ok);
    }
}
