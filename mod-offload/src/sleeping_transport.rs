// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later

//! Deterministic item-slot FIFO transport with one scheduled wake per machine.
//! Each connection carries items from a machine to its downstream input.

use std::collections::{hash_map::Entry, BTreeSet, HashMap, VecDeque};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransportError {
    DuplicateMachine(u32),
    MissingMachine(u32),
    InvalidCapacity,
    InvalidDelay,
    InputFull(u32),
    OccupiedUpstream(u32),
    SelfLink,
    TimeWentBackwards { current: u64, requested: u64 },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MachineStatus {
    Dormant,
    Waiting,
    ClockExhausted,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MachineState {
    pub status: MachineStatus,
    pub input_items: usize,
    pub output_items: usize,
    pub scheduled_wake_tick: Option<u64>,
}

/// One completed transfer. A missing destination places the item in the
/// source's output FIFO, where a caller can collect it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TransportDelta {
    pub tick: u64,
    pub source: u32,
    pub destination: Option<u32>,
    pub item: u32,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct AdvanceReport {
    pub processed: usize,
    pub backlog: Option<u64>,
    pub deltas: Vec<TransportDelta>,
}

struct Machine {
    capacity: usize,
    delay: u64,
    input: VecDeque<u32>,
    output: VecDeque<u32>,
    upstream: Option<u32>,
    downstream: Option<u32>,
    status: MachineStatus,
    scheduled_wake_tick: Option<u64>,
}

#[derive(Default)]
pub struct SleepingTransport {
    now: u64,
    machines: HashMap<u32, Machine>,
    wakes: BTreeSet<(u64, u32)>,
}

impl SleepingTransport {
    /// Allocate a machine with separate input and output FIFOs of the given capacity.
    /// A positive delay advances time even when links form a cycle.
    pub fn add_machine(
        &mut self,
        id: u32,
        capacity: u32,
        processing_ticks: u64,
    ) -> Result<(), TransportError> {
        if capacity == 0 {
            return Err(TransportError::InvalidCapacity);
        }
        if processing_ticks == 0 {
            return Err(TransportError::InvalidDelay);
        }
        let Entry::Vacant(entry) = self.machines.entry(id) else {
            return Err(TransportError::DuplicateMachine(id));
        };
        entry.insert(Machine {
            capacity: capacity as usize,
            delay: processing_ticks,
            input: VecDeque::new(),
            output: VecDeque::new(),
            upstream: None,
            downstream: None,
            status: MachineStatus::Dormant,
            scheduled_wake_tick: None,
        });
        Ok(())
    }

    pub fn now(&self) -> u64 {
        self.now
    }

    pub fn next_wake_tick(&self) -> Option<u64> {
        self.wakes.first().map(|&(tick, _)| tick)
    }

    pub fn machine(&self, id: u32) -> Option<MachineState> {
        self.machines.get(&id).map(|machine| MachineState {
            status: machine.status,
            input_items: machine.input.len(),
            output_items: machine.output.len(),
            scheduled_wake_tick: machine.scheduled_wake_tick,
        })
    }

    /// Each machine has at most one downstream and one upstream. Output already
    /// collected in the local FIFO remains available after a link change.
    pub fn set_downstream(&mut self, id: u32, to: Option<u32>) -> Result<(), TransportError> {
        let old = self
            .machines
            .get(&id)
            .ok_or(TransportError::MissingMachine(id))?
            .downstream;
        if old == to {
            return Ok(());
        }
        if let Some(target) = to {
            if target == id {
                return Err(TransportError::SelfLink);
            }
            let target_machine = self
                .machines
                .get(&target)
                .ok_or(TransportError::MissingMachine(target))?;
            if target_machine.upstream.is_some() {
                return Err(TransportError::OccupiedUpstream(target));
            }
        }
        if let Some(old) = old {
            self.machines.get_mut(&old).unwrap().upstream = None;
        }
        if let Some(target) = to {
            self.machines.get_mut(&target).unwrap().upstream = Some(id);
        }
        self.machines.get_mut(&id).unwrap().downstream = to;
        self.reconcile(id);
        Ok(())
    }

    pub fn insert(&mut self, id: u32, item: u32) -> Result<(), TransportError> {
        let node = self
            .machines
            .get_mut(&id)
            .ok_or(TransportError::MissingMachine(id))?;
        if node.input.len() == node.capacity {
            return Err(TransportError::InputFull(id));
        }
        node.input.push_back(item);
        let upstream = node.upstream;
        self.reconcile(id);
        if let Some(upstream) = upstream {
            self.reconcile(upstream);
        }
        Ok(())
    }

    pub fn extract_output(&mut self, id: u32) -> Result<Option<u32>, TransportError> {
        let node = self
            .machines
            .get_mut(&id)
            .ok_or(TransportError::MissingMachine(id))?;
        let item = node.output.pop_front();
        if item.is_some() {
            self.reconcile(id);
        }
        Ok(item)
    }

    /// Process up to `budget` due transfers. Runs in timer/ID order even when
    /// jumping across many ticks. While due work remains, the logical clock
    /// stays at the last processed tick and the caller can resume the same target.
    pub fn advance_to(
        &mut self,
        tick: u64,
        budget: usize,
    ) -> Result<AdvanceReport, TransportError> {
        self.check_time(tick)?;
        let mut report = AdvanceReport::default();
        while report.processed < budget {
            let Some(delta) = self.step(tick) else {
                break;
            };
            report.deltas.push(delta);
            report.processed += 1;
        }
        report.backlog = self.finish_advance(tick);
        Ok(report)
    }

    pub(crate) fn check_time(&self, tick: u64) -> Result<(), TransportError> {
        if tick < self.now {
            return Err(TransportError::TimeWentBackwards {
                current: self.now,
                requested: tick,
            });
        }
        Ok(())
    }

    pub(crate) fn advance_clock_to(&mut self, tick: u64) {
        debug_assert!(tick >= self.now && self.next_wake_tick().is_none_or(|due| due >= tick));
        self.now = tick;
    }

    pub(crate) fn finish_advance(&mut self, tick: u64) -> Option<u64> {
        let backlog = self.next_wake_tick().filter(|&due| due <= tick);
        if backlog.is_none() {
            self.now = tick;
        }
        backlog
    }

    pub(crate) fn step(&mut self, limit: u64) -> Option<TransportDelta> {
        let (tick, source) = *self.wakes.first()?;
        if tick > limit {
            return None;
        }
        self.wakes.pop_first();
        self.now = tick;
        let node = self.machines.get_mut(&source).unwrap();
        node.scheduled_wake_tick = None;
        let item = node.input.pop_front().unwrap();
        let upstream = node.upstream;
        let destination = node.downstream;
        if let Some(id) = destination {
            self.machines.get_mut(&id).unwrap().input.push_back(item);
            self.reconcile(id);
        } else {
            node.output.push_back(item);
        }
        self.reconcile(source);
        if let Some(id) = upstream {
            self.reconcile(id);
        }
        Some(TransportDelta {
            tick,
            source,
            destination,
            item,
        })
    }

    fn reconcile(&mut self, id: u32) {
        let node = &self.machines[&id];
        let can_output = match node.downstream {
            Some(target) => {
                let target = &self.machines[&target];
                target.input.len() < target.capacity
            }
            None => node.output.len() < node.capacity,
        };
        let old_wake = node.scheduled_wake_tick;
        let (status, wake) = if node.input.is_empty() || !can_output {
            (MachineStatus::Dormant, None)
        } else if let Some(tick) = old_wake.or_else(|| self.now.checked_add(node.delay)) {
            (MachineStatus::Waiting, Some(tick))
        } else {
            (MachineStatus::ClockExhausted, None)
        };
        let node = self.machines.get_mut(&id).unwrap();
        node.status = status;
        node.scheduled_wake_tick = wake;
        if old_wake != wake {
            if let Some(tick) = old_wake {
                self.wakes.remove(&(tick, id));
            }
            if let Some(tick) = wake {
                self.wakes.insert((tick, id));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inventory_and_output_capacity_wake_a_dormant_machine() {
        let mut transport = SleepingTransport::default();
        transport.add_machine(7, 1, 3).unwrap();
        assert_eq!(
            transport.advance_to(100, 10).unwrap(),
            AdvanceReport::default()
        );
        assert_eq!(transport.machine(7).unwrap().status, MachineStatus::Dormant);
        transport.insert(7, 101).unwrap();
        assert_eq!(transport.machine(7).unwrap().scheduled_wake_tick, Some(103));
        assert_eq!(transport.advance_to(102, 10).unwrap().processed, 0);
        assert_eq!(transport.advance_to(103, 10).unwrap().deltas[0].item, 101);
        transport.insert(7, 102).unwrap();
        assert_eq!(transport.machine(7).unwrap().status, MachineStatus::Dormant);
        assert_eq!(transport.advance_to(1000, 10).unwrap().processed, 0);
        assert_eq!(transport.extract_output(7), Ok(Some(101)));
        assert_eq!(
            transport.machine(7).unwrap().scheduled_wake_tick,
            Some(1003)
        );
        assert_eq!(transport.advance_to(1003, 10).unwrap().deltas[0].item, 102);
        assert_eq!(transport.machine(7).unwrap().status, MachineStatus::Dormant);
        assert_eq!(transport.extract_output(7), Ok(Some(102)));
        assert_eq!(transport.extract_output(7), Ok(None));
    }

    #[test]
    fn downstream_backpressure_cancels_a_timer_and_slot_release_restarts_it() {
        let mut transport = SleepingTransport::default();
        transport.add_machine(0, 1, 2).unwrap();
        transport.add_machine(1, 1, 5).unwrap();
        transport.set_downstream(0, Some(1)).unwrap();
        transport.insert(0, 12).unwrap();
        assert_eq!(transport.next_wake_tick(), Some(2));
        transport.insert(1, 11).unwrap();
        assert_eq!(transport.machine(0).unwrap().status, MachineStatus::Dormant);
        assert_eq!(transport.next_wake_tick(), Some(5));
        let report = transport.advance_to(100, 10).unwrap();
        let trace: Vec<_> = report
            .deltas
            .iter()
            .map(|d| (d.tick, d.source, d.destination, d.item))
            .collect();
        assert_eq!(trace, vec![(5, 1, None, 11), (7, 0, Some(1), 12)]);
        assert_eq!(transport.extract_output(1), Ok(Some(11)));
        assert_eq!(transport.advance_to(105, 1).unwrap().deltas[0].item, 12);
    }

    #[test]
    fn bounded_catchup_matches_one_shot_order_and_keeps_logical_time() {
        let setup = || {
            let mut transport = SleepingTransport::default();
            for id in [2, 1] {
                transport.add_machine(id, 2, 4).unwrap();
                transport.insert(id, 10).unwrap();
                transport.insert(id, 20).unwrap();
            }
            transport
        };
        let expected = setup().advance_to(100, 10).unwrap().deltas;
        let mut transport = setup();
        let mut actual = Vec::new();
        assert_eq!(transport.advance_to(100, 0).unwrap().backlog, Some(4));
        assert_eq!(transport.now(), 0);
        for (now, backlog) in [(4, Some(4)), (4, Some(8)), (8, Some(8)), (100, None)] {
            let report = transport.advance_to(100, 1).unwrap();
            assert_eq!((report.backlog, transport.now()), (backlog, now));
            actual.extend(report.deltas);
        }
        assert_eq!(actual, expected);
        assert_eq!(
            actual.iter().map(|delta| delta.source).collect::<Vec<_>>(),
            [1, 2, 1, 2]
        );
        assert!(matches!(
            transport.advance_to(99, 1),
            Err(TransportError::TimeWentBackwards { .. })
        ));
    }

    #[test]
    fn positive_delay_and_tick_overflow_keep_cyclic_work_finite() {
        let mut transport = SleepingTransport::default();
        assert_eq!(
            transport.add_machine(0, 1, 0),
            Err(TransportError::InvalidDelay)
        );
        transport.add_machine(0, 1, 1).unwrap();
        transport.add_machine(1, 1, 1).unwrap();
        transport.set_downstream(0, Some(1)).unwrap();
        transport.set_downstream(1, Some(0)).unwrap();
        transport.advance_to(u64::MAX - 1, 1).unwrap();
        transport.insert(0, 42).unwrap();
        assert_eq!(
            transport
                .advance_to(u64::MAX, usize::MAX)
                .unwrap()
                .processed,
            1
        );
        assert_eq!(
            transport.machine(1).unwrap().status,
            MachineStatus::ClockExhausted
        );
        assert_eq!(transport.machine(1).unwrap().input_items, 1);
        assert_eq!(transport.next_wake_tick(), None);
    }
}
