// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later

//! Region-local simulation and little-endian, direct-buffer-friendly delta batches.
//! Transport wakes are scheduled work. Kinetic topology and contraption edits are
//! explicit synchronous operations on the owning region's kernels.

use std::collections::{BTreeMap, VecDeque};
use std::time::{Duration, Instant};

use crate::contraption::Contraption;
use crate::kinetic_graph::KineticGraph;
use crate::sleeping_transport::{SleepingTransport, TransportDelta, TransportError};

pub const REGION_SIDE_BLOCKS: i32 = 32;
pub const MAX_BATCH_BYTES: usize = 256 * 1024;
pub const MAX_BATCH_RECORDS: usize = 4096;
pub const BATCH_HEADER_BYTES: usize = 32;
pub const RECORD_HEADER_BYTES: usize = 16;
pub const TRANSPORT_TRANSFER: u16 = 1;
pub const FIRST_ADAPTER_KIND: u16 = 0x100;

const FRAME_MAGIC: [u8; 4] = [b'S', b'5', 1, 1];
const TRANSFER_PAYLOAD_BYTES: usize = 9;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct RegionKey {
    pub dimension: i32,
    pub x: i32,
    pub z: i32,
}

impl RegionKey {
    pub fn for_block(dimension: i32, x: i32, z: i32) -> Self {
        Self {
            dimension,
            x: x.div_euclid(REGION_SIDE_BLOCKS),
            z: z.div_euclid(REGION_SIDE_BLOCKS),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RegionBudget {
    pub max_steps: usize,
    /// Checked between completed transfers. A started transfer is atomic.
    pub max_time: Option<Duration>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RegionReport {
    pub processed: usize,
    pub backlog: Option<u64>,
    pub output_blocked: bool,
    pub time_exhausted: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BatchError {
    ReservedKind,
    OversizedRecord,
    QueueFull,
    BufferTooSmall { needed: usize },
    SequenceExhausted,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BatchReport {
    pub bytes: usize,
    pub records: usize,
    pub sequence: Option<u64>,
}

#[derive(Default)]
pub struct RegionScheduler {
    regions: BTreeMap<RegionKey, Region>,
}

impl RegionScheduler {
    pub fn region(&self, key: RegionKey) -> Option<&Region> {
        self.regions.get(&key)
    }

    /// Obtain a region's independent kernels, clock, work budget and output queue.
    pub fn region_mut(&mut self, key: RegionKey) -> &mut Region {
        self.regions.entry(key).or_insert_with(|| Region::new(key))
    }
}

pub struct Region {
    key: RegionKey,
    pub kinetic: KineticGraph,
    pub contraptions: BTreeMap<u32, Contraption>,
    transport: SleepingTransport,
    pending: VecDeque<Delta>,
    pending_bytes: usize,
    sequence: u64,
}

struct Delta {
    tick: u64,
    source: u32,
    kind: u16,
    payload: Box<[u8]>,
}

impl Delta {
    fn bytes(&self) -> usize {
        RECORD_HEADER_BYTES + self.payload.len()
    }
}

impl Region {
    fn new(key: RegionKey) -> Self {
        Self {
            key,
            kinetic: KineticGraph::default(),
            contraptions: BTreeMap::new(),
            transport: SleepingTransport::default(),
            pending: VecDeque::new(),
            pending_bytes: 0,
            sequence: 0,
        }
    }

    pub fn transport(&self) -> &SleepingTransport {
        &self.transport
    }

    pub fn add_machine(
        &mut self,
        id: u32,
        capacity: u32,
        delay: u64,
    ) -> Result<(), TransportError> {
        self.transport.add_machine(id, capacity, delay)
    }

    pub fn set_downstream(&mut self, id: u32, to: Option<u32>) -> Result<(), TransportError> {
        self.transport.set_downstream(id, to)
    }

    pub fn insert_item(&mut self, id: u32, item: u32) -> Result<(), TransportError> {
        self.transport.insert(id, item)
    }

    pub fn extract_output(&mut self, id: u32) -> Result<Option<u32>, TransportError> {
        self.transport.extract_output(id)
    }

    /// Advance this region's ready work. Due transfers are held until the
    /// caller drains batch output when space runs out.
    pub fn advance_to(
        &mut self,
        tick: u64,
        budget: RegionBudget,
    ) -> Result<RegionReport, TransportError> {
        self.transport.check_time(tick)?;
        let time_limit = budget.max_time.map(|limit| (Instant::now(), limit));
        let mut report = RegionReport::default();
        while report.processed < budget.max_steps
            && self
                .transport
                .next_wake_tick()
                .is_some_and(|next| next <= tick)
        {
            if !self.has_room(TRANSFER_PAYLOAD_BYTES) {
                report.output_blocked = true;
                break;
            }
            if time_limit.is_some_and(|(start, limit)| start.elapsed() >= limit) {
                report.time_exhausted = true;
                break;
            }
            let delta = self.transport.step(tick).unwrap();
            self.queue_transfer(delta);
            report.processed += 1;
        }
        report.backlog = self.transport.finish_advance(tick);
        Ok(report)
    }

    /// Queue an adapter-defined snapshot delta at the region's logical tick.
    /// The caller retains its payload on failure so it can retry after export.
    pub fn queue_delta(
        &mut self,
        kind: u16,
        source: u32,
        payload: &[u8],
    ) -> Result<(), BatchError> {
        if kind < FIRST_ADAPTER_KIND {
            return Err(BatchError::ReservedKind);
        }
        if payload.len() > u16::MAX as usize {
            return Err(BatchError::OversizedRecord);
        }
        if !self.has_room(payload.len()) {
            return Err(BatchError::QueueFull);
        }
        self.push_delta(self.transport.now(), source, kind, payload);
        Ok(())
    }

    /// Export the longest FIFO prefix that fits the caller's byte and record
    /// limits. An empty batch writes nothing and does not consume a sequence.
    pub fn export_into(
        &mut self,
        buffer: &mut [u8],
        max_records: usize,
    ) -> Result<BatchReport, BatchError> {
        let empty = BatchReport {
            bytes: 0,
            records: 0,
            sequence: None,
        };
        if max_records == 0 {
            return Ok(empty);
        }
        let Some(first) = self.pending.front() else {
            return Ok(empty);
        };
        let limit = buffer.len().min(MAX_BATCH_BYTES);
        let needed = BATCH_HEADER_BYTES + first.bytes();
        if needed > limit {
            return Err(BatchError::BufferTooSmall { needed });
        }
        let sequence = self
            .sequence
            .checked_add(1)
            .ok_or(BatchError::SequenceExhausted)?;
        let mut offset = BATCH_HEADER_BYTES;
        let mut count = 0_u32;
        while (count as usize) < max_records.min(MAX_BATCH_RECORDS) {
            let Some(delta) = self.pending.front() else {
                break;
            };
            let bytes = delta.bytes();
            if offset + bytes > limit {
                break;
            }
            let delta = self.pending.pop_front().unwrap();
            let record = &mut buffer[offset..offset + bytes];
            record[..8].copy_from_slice(&delta.tick.to_le_bytes());
            record[8..12].copy_from_slice(&delta.source.to_le_bytes());
            record[12..14].copy_from_slice(&delta.kind.to_le_bytes());
            record[14..16].copy_from_slice(&(delta.payload.len() as u16).to_le_bytes());
            record[16..].copy_from_slice(&delta.payload);
            offset += bytes;
            count += 1;
            self.pending_bytes -= bytes;
        }
        buffer[..4].copy_from_slice(&FRAME_MAGIC);
        buffer[4..8].copy_from_slice(&count.to_le_bytes());
        buffer[8..16].copy_from_slice(&sequence.to_le_bytes());
        buffer[16..20].copy_from_slice(&self.key.dimension.to_le_bytes());
        buffer[20..24].copy_from_slice(&self.key.x.to_le_bytes());
        buffer[24..28].copy_from_slice(&self.key.z.to_le_bytes());
        buffer[28..32].fill(0);
        self.sequence = sequence;
        Ok(BatchReport {
            bytes: offset,
            records: count as usize,
            sequence: Some(sequence),
        })
    }

    fn has_room(&self, payload_len: usize) -> bool {
        self.pending.len() < MAX_BATCH_RECORDS
            && BATCH_HEADER_BYTES + self.pending_bytes + RECORD_HEADER_BYTES + payload_len
                <= MAX_BATCH_BYTES
    }

    fn queue_transfer(&mut self, delta: TransportDelta) {
        let mut payload = [0; TRANSFER_PAYLOAD_BYTES];
        payload[0] = u8::from(delta.destination.is_some());
        payload[1..5].copy_from_slice(&delta.destination.unwrap_or(0).to_le_bytes());
        payload[5..9].copy_from_slice(&delta.item.to_le_bytes());
        self.push_delta(delta.tick, delta.source, TRANSPORT_TRANSFER, &payload);
    }

    fn push_delta(&mut self, tick: u64, source: u32, kind: u16, payload: &[u8]) {
        self.pending_bytes += RECORD_HEADER_BYTES + payload.len();
        self.pending.push_back(Delta {
            tick,
            source,
            kind,
            payload: payload.into(),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BUDGET: RegionBudget = RegionBudget {
        max_steps: 1,
        max_time: None,
    };

    #[test]
    fn block_coordinates_use_euclidean_regions_and_include_dimension() {
        assert_eq!(
            RegionKey::for_block(-1, -1, -32),
            RegionKey {
                dimension: -1,
                x: -1,
                z: -1
            }
        );
        assert_eq!(
            RegionKey::for_block(1, -33, 32),
            RegionKey {
                dimension: 1,
                x: -2,
                z: 1
            }
        );
        assert_ne!(RegionKey::for_block(1, 0, 0), RegionKey::for_block(2, 0, 0));
    }

    #[test]
    fn deadlines_and_full_queue_are_local_to_a_region() {
        let mut scheduler = RegionScheduler::default();
        let busy = RegionKey::for_block(0, 0, 0);
        let quiet = RegionKey::for_block(0, 32, 0);
        for key in [busy, quiet] {
            let region = scheduler.region_mut(key);
            region.add_machine(0, 2, 1).unwrap();
            region.insert_item(0, 3).unwrap();
            region.insert_item(0, 5).unwrap();
        }
        let region = scheduler.region_mut(busy);
        assert_eq!(
            region.advance_to(
                10,
                RegionBudget {
                    max_time: Some(Duration::ZERO),
                    ..BUDGET
                }
            ),
            Ok(RegionReport {
                processed: 0,
                backlog: Some(1),
                time_exhausted: true,
                output_blocked: false
            })
        );
        for _ in 0..MAX_BATCH_RECORDS {
            region.queue_delta(FIRST_ADAPTER_KIND, 0, b"").unwrap();
        }
        assert_eq!(
            region.queue_delta(FIRST_ADAPTER_KIND, 0, b""),
            Err(BatchError::QueueFull)
        );
        assert_eq!(
            region.advance_to(10, BUDGET),
            Ok(RegionReport {
                processed: 0,
                backlog: Some(1),
                time_exhausted: false,
                output_blocked: true
            })
        );
        assert_eq!(region.transport().now(), 0);

        let region = scheduler.region_mut(quiet);
        assert_eq!(region.advance_to(10, BUDGET).unwrap().backlog, Some(2));
        assert_eq!(region.advance_to(10, BUDGET).unwrap().backlog, None);
        assert_eq!(region.extract_output(0), Ok(Some(3)));
        assert_eq!(region.extract_output(0), Ok(Some(5)));
        assert_eq!(region.transport().now(), 10);

        let mut buffer = [0; 128];
        let region = scheduler.region_mut(busy);
        assert_eq!(region.export_into(&mut buffer, 1).unwrap().records, 1);
        assert_eq!(region.advance_to(10, BUDGET).unwrap().processed, 1);
        assert_eq!(region.extract_output(0), Ok(Some(3)));
    }

    #[test]
    fn batch_has_exact_wire_bytes_and_preserves_remainder_on_small_buffers() {
        let mut scheduler = RegionScheduler::default();
        let region = scheduler.region_mut(RegionKey {
            dimension: -2,
            x: -1,
            z: 3,
        });
        region.add_machine(7, 1, 2).unwrap();
        region.add_machine(0, 1, 1).unwrap();
        region.set_downstream(7, Some(0)).unwrap();
        region.insert_item(7, 0x1234_abcd).unwrap();
        region.advance_to(2, BUDGET).unwrap();
        region
            .queue_delta(FIRST_ADAPTER_KIND, 0x102, b"pose")
            .unwrap();

        let mut short = [0xee; 56];
        assert_eq!(
            region.export_into(&mut short, 2),
            Err(BatchError::BufferTooSmall { needed: 57 })
        );
        assert_eq!(short, [0xee; 56]);
        let mut buffer = [0xee; 64];
        let first = region.export_into(&mut buffer, 2).unwrap();
        assert_eq!(
            first,
            BatchReport {
                bytes: 57,
                records: 1,
                sequence: Some(1)
            }
        );
        assert_eq!(
            &buffer[..32],
            &[
                b'S', b'5', 1, 1, 1, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 254, 255, 255, 255, 255, 255,
                255, 255, 3, 0, 0, 0, 0, 0, 0, 0,
            ]
        );
        assert_eq!(
            &buffer[32..57],
            &[
                2, 0, 0, 0, 0, 0, 0, 0, 7, 0, 0, 0, 1, 0, 9, 0, 1, 0, 0, 0, 0, 0xcd, 0xab, 0x34,
                0x12,
            ]
        );
        assert_eq!(&buffer[57..], &[0xee; 7]);
        let second = region.export_into(&mut buffer, 1).unwrap();
        assert_eq!(
            second,
            BatchReport {
                bytes: 52,
                records: 1,
                sequence: Some(2)
            }
        );
        assert_eq!(
            &buffer[32..52],
            &[2, 0, 0, 0, 0, 0, 0, 0, 2, 1, 0, 0, 0, 1, 4, 0, b'p', b'o', b's', b'e']
        );
        assert_eq!(region.export_into(&mut buffer, 1).unwrap().sequence, None);
    }

    #[test]
    fn batch_validates_record_kind_length_and_queue_byte_budget() {
        let mut scheduler = RegionScheduler::default();
        let region = scheduler.region_mut(RegionKey::for_block(0, 0, 0));
        let payload = vec![0; u16::MAX as usize];
        assert_eq!(
            region.queue_delta(TRANSPORT_TRANSFER, 0, b""),
            Err(BatchError::ReservedKind)
        );
        assert_eq!(
            region.queue_delta(FIRST_ADAPTER_KIND, 0, &[0; 65536]),
            Err(BatchError::OversizedRecord)
        );
        for _ in 0..3 {
            region.queue_delta(FIRST_ADAPTER_KIND, 0, &payload).unwrap();
        }
        assert_eq!(
            region.queue_delta(FIRST_ADAPTER_KIND, 0, &payload),
            Err(BatchError::QueueFull)
        );
        let mut buffer = vec![0; MAX_BATCH_BYTES];
        assert_eq!(region.export_into(&mut buffer, 2).unwrap().records, 2);
        region.queue_delta(FIRST_ADAPTER_KIND, 0, &payload).unwrap();
        assert_eq!(
            region
                .export_into(&mut buffer, MAX_BATCH_RECORDS)
                .unwrap()
                .records,
            2
        );
    }
}
