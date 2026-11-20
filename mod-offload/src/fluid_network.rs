// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later

//! Integer pressure-head equilibrium. A connected component carries at most
//! one fluid. A positive link head raises the equilibrium fill on its b side.

use std::collections::{btree_map::Entry, BTreeMap, BTreeSet, VecDeque};

pub type TankId = u32;
pub type FluidId = u32;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TankState {
    pub fluid: Option<FluidId>,
    pub amount: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FluidError {
    DuplicateTank(TankId),
    MissingTank(TankId),
    InvalidCapacity(TankId),
    SelfLink,
    IncompatibleFluids,
    ConflictingHead,
}

#[derive(Debug, PartialEq, Eq)]
pub struct FluidSolve {
    pub source: TankId,
    pub visited_nodes: usize,
    pub changes: Vec<(TankId, TankState)>,
}

struct Tank {
    capacity: u32,
    elevation: i32,
    state: TankState,
    links: BTreeMap<TankId, i64>,
}

#[derive(Default)]
pub struct FluidNetwork {
    tanks: BTreeMap<TankId, Tank>,
    dirty: BTreeSet<TankId>,
}

impl FluidNetwork {
    pub fn add_tank(
        &mut self,
        id: TankId,
        capacity: u32,
        elevation: i32,
    ) -> Result<(), FluidError> {
        if capacity == 0 {
            return Err(FluidError::InvalidCapacity(id));
        }
        let Entry::Vacant(entry) = self.tanks.entry(id) else {
            return Err(FluidError::DuplicateTank(id));
        };
        entry.insert(Tank {
            capacity,
            elevation,
            state: TankState::default(),
            links: BTreeMap::new(),
        });
        Ok(())
    }

    pub fn set_tank(
        &mut self,
        id: TankId,
        capacity: u32,
        elevation: i32,
    ) -> Result<(), FluidError> {
        let tank = self.tanks.get_mut(&id).ok_or(FluidError::MissingTank(id))?;
        if capacity == 0 || capacity < tank.state.amount {
            return Err(FluidError::InvalidCapacity(id));
        }
        if (tank.capacity, tank.elevation) != (capacity, elevation) {
            tank.capacity = capacity;
            tank.elevation = elevation;
            self.dirty.insert(id);
        }
        Ok(())
    }

    /// Set or remove an edge atomically. Pump heads on a cycle must sum to
    /// zero for the component to admit a scalar pressure equilibrium.
    pub fn set_link(
        &mut self,
        a: TankId,
        b: TankId,
        pump_head: Option<i32>,
    ) -> Result<(), FluidError> {
        if a == b {
            return Err(FluidError::SelfLink);
        }
        let before = self.get(a)?.links.get(&b).copied();
        self.get(b)?;
        let after = pump_head.map(i64::from);
        if before == after {
            return Ok(());
        }
        if let Some(head) = after {
            let left = self.component(a, Some((a, b)));
            if let Some(&existing) = left.get(&b) {
                if existing != i128::from(head) {
                    return Err(FluidError::ConflictingHead);
                }
            } else if before.is_none() {
                if let Some(fluid) = self.fluid_in(&left) {
                    let right = self.component(b, None);
                    if self.fluid_in(&right).is_some_and(|other| fluid != other) {
                        return Err(FluidError::IncompatibleFluids);
                    }
                }
            }
            self.tanks.get_mut(&a).unwrap().links.insert(b, head);
            self.tanks.get_mut(&b).unwrap().links.insert(a, -head);
        } else {
            self.tanks.get_mut(&a).unwrap().links.remove(&b);
            self.tanks.get_mut(&b).unwrap().links.remove(&a);
        }
        self.dirty.extend([a, b]);
        Ok(())
    }

    pub fn insert(&mut self, id: TankId, fluid: FluidId, amount: u32) -> Result<u32, FluidError> {
        let tank = self.get(id)?;
        if amount == 0 {
            return Ok(0);
        }
        let resident = tank
            .state
            .fluid
            .or_else(|| self.fluid_in(&self.component(id, None)));
        if resident.is_some_and(|other| other != fluid) {
            return Err(FluidError::IncompatibleFluids);
        }
        let accepted = amount.min(tank.capacity - tank.state.amount);
        if accepted != 0 {
            let state = &mut self.tanks.get_mut(&id).unwrap().state;
            state.fluid = Some(fluid);
            state.amount += accepted;
            self.dirty.insert(id);
        }
        Ok(accepted)
    }

    pub fn extract(&mut self, id: TankId, amount: u32) -> Result<u32, FluidError> {
        let state = &mut self
            .tanks
            .get_mut(&id)
            .ok_or(FluidError::MissingTank(id))?
            .state;
        let extracted = amount.min(state.amount);
        if extracted != 0 {
            state.amount -= extracted;
            if state.amount == 0 {
                state.fluid = None;
            }
            self.dirty.insert(id);
        }
        Ok(extracted)
    }

    pub fn tank(&self, id: TankId) -> Option<TankState> {
        self.tanks.get(&id).map(|tank| tank.state)
    }

    pub fn has_pending(&self) -> bool {
        !self.dirty.is_empty()
    }

    /// Solve one dirty component and report only solver-induced state changes.
    pub fn step(&mut self) -> Option<FluidSolve> {
        let source = self.dirty.pop_first()?;
        let component = self.component(source, None);
        let mut result = FluidSolve {
            source,
            visited_nodes: component.len(),
            changes: Vec::new(),
        };
        let mut volume = 0_u64;
        let mut capacity = 0_u64;
        let mut fluid = None;
        let columns: Vec<_> = component
            .into_iter()
            .map(|(id, head)| {
                self.dirty.remove(&id);
                let tank = &self.tanks[&id];
                volume += u64::from(tank.state.amount);
                capacity += u64::from(tank.capacity);
                fluid = fluid.or(tank.state.fluid);
                (id, i128::from(tank.elevation) - head, tank.capacity)
            })
            .collect();
        if columns.len() == 1 || volume == 0 || volume == capacity {
            return Some(result);
        }

        // Find the highest integer surface whose fill does not exceed the
        // conserved volume, then allocate any fractional remainder in ID order.
        let filled =
            |level: i128, base: i128, cap: u32| (level - base).clamp(0, i128::from(cap)) as u32;
        let mut low = columns.iter().map(|&(_, base, _)| base).min().unwrap();
        let mut high = columns
            .iter()
            .map(|&(_, base, cap)| base + i128::from(cap))
            .max()
            .unwrap();
        while high - low > 1 {
            let mid = low + (high - low) / 2;
            let total = columns
                .iter()
                .map(|&(_, base, cap)| u64::from(filled(mid, base, cap)))
                .sum::<u64>();
            if total <= volume {
                low = mid;
            } else {
                high = mid;
            }
        }
        let mut remainder = volume
            - columns
                .iter()
                .map(|&(_, base, cap)| u64::from(filled(low, base, cap)))
                .sum::<u64>();
        for (id, base, cap) in columns {
            let mut amount = filled(low, base, cap);
            if remainder > 0 && filled(high, base, cap) > amount {
                amount += 1;
                remainder -= 1;
            }
            let state = TankState {
                fluid: fluid.filter(|_| amount > 0),
                amount,
            };
            let tank = self.tanks.get_mut(&id).unwrap();
            if tank.state != state {
                tank.state = state;
                result.changes.push((id, state));
            }
        }
        debug_assert_eq!(remainder, 0);
        Some(result)
    }

    fn get(&self, id: TankId) -> Result<&Tank, FluidError> {
        self.tanks.get(&id).ok_or(FluidError::MissingTank(id))
    }

    fn component(
        &self,
        source: TankId,
        skip_edge: Option<(TankId, TankId)>,
    ) -> BTreeMap<TankId, i128> {
        let mut offsets = BTreeMap::from([(source, 0)]);
        let mut queue = VecDeque::from([source]);
        while let Some(id) = queue.pop_front() {
            let offset = offsets[&id];
            for (&other, &head) in &self.tanks[&id].links {
                if skip_edge == Some((id, other)) || skip_edge == Some((other, id)) {
                    continue;
                }
                if let Entry::Vacant(entry) = offsets.entry(other) {
                    entry.insert(offset + i128::from(head));
                    queue.push_back(other);
                }
            }
        }
        offsets
    }

    fn fluid_in(&self, component: &BTreeMap<TankId, i128>) -> Option<FluidId> {
        component.keys().find_map(|id| self.tanks[id].state.fluid)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn equilibrium_conserves_volume_propagates_heads_and_sleeps() {
        let mut n = FluidNetwork::default();
        for id in 0..4 {
            n.add_tank(id, 10, 0).unwrap();
        }
        n.set_link(1, 2, Some(4)).unwrap();
        n.set_link(2, 3, Some(0)).unwrap();
        assert_eq!(n.insert(1, 0, 9), Ok(9));
        let solve = n.step().unwrap();
        assert_eq!(solve.visited_nodes, 3);
        assert_eq!([1, 2, 3].map(|id| n.tank(id).unwrap().amount), [1, 4, 4]);
        assert_eq!(n.step(), None);
        n.set_tank(2, 10, 4).unwrap();
        assert_eq!(n.step().unwrap().visited_nodes, 3);
        assert_eq!([1, 2, 3].map(|id| n.tank(id).unwrap().amount), [2, 2, 5]);
        assert_eq!(n.step(), None);
    }

    #[test]
    fn rejects_mixing_and_inconsistent_cycles_without_mutating() {
        let mut n = FluidNetwork::default();
        for id in 1..=4 {
            n.add_tank(id, 8, 0).unwrap();
        }
        n.set_link(1, 2, Some(i32::MIN)).unwrap();
        n.set_link(2, 3, Some(i32::MAX)).unwrap();
        n.set_link(1, 3, Some(-1)).unwrap();
        n.insert(1, 7, 8).unwrap();
        n.insert(4, 9, 1).unwrap();
        assert_eq!(n.insert(2, 9, 1), Err(FluidError::IncompatibleFluids));
        assert_eq!(
            n.set_link(3, 4, Some(0)),
            Err(FluidError::IncompatibleFluids)
        );
        assert_eq!(n.set_link(1, 3, Some(0)), Err(FluidError::ConflictingHead));
        assert_eq!(n.step().unwrap().visited_nodes, 3);
        assert_eq!(n.step().unwrap().visited_nodes, 1);
        assert_eq!(n.tank(4).unwrap().fluid, Some(9));
        assert_eq!(n.step(), None);
    }

    #[test]
    fn split_components_wake_independently_and_allow_different_fluids() {
        let mut n = FluidNetwork::default();
        for id in 0..3 {
            n.add_tank(id, 8, 0).unwrap();
        }
        n.set_link(0, 1, Some(0)).unwrap();
        n.set_link(1, 2, Some(0)).unwrap();
        n.insert(0, 3, 8).unwrap();
        assert_eq!(n.step().unwrap().visited_nodes, 3);
        n.set_link(1, 2, None).unwrap();
        assert_eq!(n.extract(2, 8), Ok(2));
        assert_eq!(n.insert(2, 9, 1), Ok(1));
        assert_eq!(n.step().unwrap().visited_nodes, 2);
        assert_eq!(n.step().unwrap().visited_nodes, 1);
        assert_eq!(n.tank(2).unwrap().fluid, Some(9));
        assert_eq!(n.step(), None);
    }
}
