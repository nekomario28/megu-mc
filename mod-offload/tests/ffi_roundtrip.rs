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
