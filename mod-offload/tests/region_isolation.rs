// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later

use std::sync::mpsc::{self, TryRecvError};
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::Duration;

use mod_offload::region_scheduler::{
    BatchError, Region, RegionBudget, RegionKey, RegionScheduler, FIRST_ADAPTER_KIND,
    MAX_BATCH_BYTES, MAX_BATCH_RECORDS, TRANSPORT_TRANSFER,
};
use mod_offload::sleeping_transport::TransportError;

const PEERS: i32 = 4;
const TICKS: u64 = 1024;
const MACHINE: u32 = 7;
const WATCHDOG: Duration = Duration::from_secs(20);
const BUDGET: RegionBudget = RegionBudget {
    max_steps: 1,
    max_time: None,
};

#[test]
fn independent_scheduler_threads_progress_while_one_output_queue_stays_full() {
    let (phase_tx, phase_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel::<()>();
    let blocked = thread::spawn(move || {
        let key = RegionKey {
            dimension: -1,
            x: -8,
            z: 9,
        };
        let mut scheduler = RegionScheduler::default();
        let region = scheduler.region_mut(key);
        region.add_machine(MACHINE, 1, 1).unwrap();
        let item = u32::MAX;
        region.insert_item(MACHINE, item).unwrap();
        for ordinal in 0..MAX_BATCH_RECORDS as u32 {
            region
                .queue_delta(FIRST_ADAPTER_KIND, MACHINE, &ordinal.to_le_bytes())
                .unwrap();
        }
        let check_blocked = |region: &mut Region| {
            assert_eq!(
                region.queue_delta(FIRST_ADAPTER_KIND, MACHINE, b"rejected"),
                Err(BatchError::QueueFull)
            );
            let report = region.advance_to(1, BUDGET).unwrap();
            assert_eq!(
                (report.processed, report.backlog, report.output_blocked),
                (0, Some(1), true)
            );
            assert_eq!(region.extract_output(MACHINE), Ok(None));
        };
        check_blocked(region);
        phase_tx.send(()).unwrap();
        while matches!(release_rx.try_recv(), Err(TryRecvError::Empty)) {
            check_blocked(region);
            thread::yield_now();
        }

        check_blocked(region);
        let mut frame = vec![0; MAX_BATCH_BYTES];
        let report = region.export_into(&mut frame, usize::MAX).unwrap();
        assert_eq!(
            (report.records, report.sequence),
            (MAX_BATCH_RECORDS, Some(1))
        );
        let mut records = check_header(&frame[..report.bytes], key, MAX_BATCH_RECORDS as u32, 1);
        for ordinal in 0..MAX_BATCH_RECORDS as u32 {
            check_record(&mut records, 0, FIRST_ADAPTER_KIND, &ordinal.to_le_bytes());
        }
        assert!(records.is_empty());

        let report = region.advance_to(1, BUDGET).unwrap();
        assert_eq!(
            (report.processed, report.backlog, report.output_blocked),
            (1, None, false)
        );
        assert_eq!(region.extract_output(MACHINE), Ok(Some(item)));
        let report = region.export_into(&mut frame, usize::MAX).unwrap();
        assert_eq!((report.records, report.sequence), (1, Some(2)));
        let mut records = check_header(&frame[..report.bytes], key, 1, 2);
        check_record(&mut records, 1, TRANSPORT_TRANSFER, &transfer(item));
        assert!(records.is_empty());
        phase_tx.send(()).unwrap();
    });

    phase_rx
        .recv_timeout(WATCHDOG)
        .expect("the blocked queue never reached capacity");
    let (done_tx, done_rx) = mpsc::channel();
    let start = Arc::new(Barrier::new(PEERS as usize));
    let peers: Vec<_> = (0..PEERS)
        .map(|peer| {
            let done = done_tx.clone();
            let start = Arc::clone(&start);
            thread::spawn(move || {
                run_peer(peer, &start);
                done.send(peer).unwrap();
            })
        })
        .collect();
    drop(done_tx);
    let mut finished: Vec<_> = (0..PEERS)
        .map(|_| {
            done_rx
                .recv_timeout(WATCHDOG)
                .expect("a peer failed to finish while the queue was full")
        })
        .collect();
    finished.sort_unstable();
    assert_eq!(finished, (0..PEERS).collect::<Vec<_>>());
    release_tx.send(()).unwrap();
    phase_rx
        .recv_timeout(WATCHDOG)
        .expect("the blocked region failed to recover after export");
    for peer in peers {
        peer.join().unwrap();
    }
    blocked.join().unwrap();
}

fn run_peer(peer: i32, start: &Barrier) {
    let key = RegionKey {
        dimension: peer % 2,
        x: peer,
        z: -peer,
    };
    let mut scheduler = RegionScheduler::default();
    let region = scheduler.region_mut(key);
    region.add_machine(MACHINE, 1, 1).unwrap();
    let mut frame = [0; 84];
    start.wait();
    for round in 1..=TICKS {
        let tick = round + peer as u64 * TICKS;
        let item = tick as u32;
        let now = region.transport().now();
        region
            .queue_delta(FIRST_ADAPTER_KIND, MACHINE, &item.to_le_bytes())
            .unwrap();
        region.insert_item(MACHINE, item).unwrap();
        let report = region.advance_to(tick, BUDGET).unwrap();
        assert_eq!(
            (report.processed, report.backlog, report.output_blocked),
            (1, None, false)
        );
        assert_eq!(region.extract_output(MACHINE), Ok(Some(item)));
        let report = region.export_into(&mut frame, usize::MAX).unwrap();
        assert_eq!((report.records, report.sequence), (2, Some(round)));
        let mut records = check_header(&frame[..report.bytes], key, 2, round);
        check_record(&mut records, now, FIRST_ADAPTER_KIND, &item.to_le_bytes());
        check_record(&mut records, now + 1, TRANSPORT_TRANSFER, &transfer(item));
        assert!(records.is_empty());
    }
    assert_eq!(region.transport().now(), (peer as u64 + 1) * TICKS);
    assert!(matches!(
        region.advance_to(0, BUDGET),
        Err(TransportError::TimeWentBackwards { .. })
    ));
}

fn check_header(frame: &[u8], key: RegionKey, count: u32, sequence: u64) -> &[u8] {
    assert_eq!(&frame[..4], b"S5\x01\x01");
    assert_eq!(&frame[4..8], &count.to_le_bytes());
    assert_eq!(&frame[8..16], &sequence.to_le_bytes());
    assert_eq!(&frame[16..20], &key.dimension.to_le_bytes());
    assert_eq!(&frame[20..24], &key.x.to_le_bytes());
    assert_eq!(&frame[24..28], &key.z.to_le_bytes());
    assert_eq!(&frame[28..32], &[0; 4]);
    &frame[32..]
}

fn check_record(frame: &mut &[u8], tick: u64, kind: u16, payload: &[u8]) {
    let (record, remaining) = frame.split_at(16 + payload.len());
    assert_eq!(&record[..8], &tick.to_le_bytes());
    assert_eq!(&record[8..12], &MACHINE.to_le_bytes());
    assert_eq!(&record[12..14], &kind.to_le_bytes());
    assert_eq!(&record[14..16], &(payload.len() as u16).to_le_bytes());
    assert_eq!(&record[16..], payload);
    *frame = remaining;
}

fn transfer(item: u32) -> [u8; 9] {
    let mut payload = [0; 9];
    payload[5..].copy_from_slice(&item.to_le_bytes());
    payload
}
