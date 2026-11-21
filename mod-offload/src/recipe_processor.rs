// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later

//! Item and fluid recipes with one deadline per running machine. Ingress
//! reconciles only affected machines. Work between deadlines accrues lazily.

use std::collections::{hash_map::Entry, BTreeMap, BTreeSet, HashMap};

pub const REFERENCE_RPM: u32 = 16;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub enum ResourceKind {
    Item,
    Fluid,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub struct Resource {
    pub kind: ResourceKind,
    pub id: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Stack {
    pub resource: Resource,
    pub amount: u32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Recipe {
    pub id: u32,
    pub base_ticks: u32,
    pub inputs: Vec<Stack>,
    pub outputs: Vec<Stack>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Drive {
    Mechanical { rpm: u32, overstressed: bool },
    Furnace { fuel_ticks: u64 },
}

impl Drive {
    fn rate(self) -> u64 {
        match self {
            Self::Mechanical {
                rpm,
                overstressed: false,
            } => u64::from(rpm),
            Self::Furnace { fuel_ticks: 1.. } => u64::from(REFERENCE_RPM),
            _ => 0,
        }
    }
}

/// A missing recipe ID indicates fuel exhaustion before completion.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RecipeEvent {
    pub tick: u64,
    pub machine: u32,
    pub recipe: Option<u32>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RecipeError {
    DuplicateMachine,
    UnknownMachine,
    InvalidRecipe,
    InvalidCapacity,
    SelfRoute,
}

#[derive(Default)]
struct Inventory {
    /// Sorted by resource so lookups stay allocation-free.
    stacks: Vec<(Resource, u32)>,
    used: u32,
}

impl Inventory {
    fn amount(&self, resource: Resource) -> u32 {
        match self
            .stacks
            .binary_search_by(|(other, _)| other.cmp(&resource))
        {
            Ok(index) => self.stacks[index].1,
            Err(_) => 0,
        }
    }

    fn put(&mut self, resource: Resource, amount: u32) {
        if amount > 0 {
            match self
                .stacks
                .binary_search_by(|(other, _)| other.cmp(&resource))
            {
                Ok(index) => self.stacks[index].1 += amount,
                Err(index) => self.stacks.insert(index, (resource, amount)),
            }
            self.used += amount;
        }
    }

    fn take(&mut self, resource: Resource, requested: u32) -> u32 {
        let Ok(index) = self
            .stacks
            .binary_search_by(|(other, _)| other.cmp(&resource))
        else {
            return 0;
        };
        let amount = requested.min(self.stacks[index].1);
        self.stacks[index].1 -= amount;
        if self.stacks[index].1 == 0 {
            self.stacks.remove(index);
        }
        self.used -= amount;
        amount
    }
}

struct Machine {
    input_capacity: u32,
    output_capacity: u32,
    recipes: Vec<Recipe>,
    /// Sorted, deduplicated resources accepted at the input.
    input_filter: Vec<Resource>,
    drive: Drive,
    input: Inventory,
    output: Inventory,
    active: Option<usize>,
    remaining: u64,
    updated: u64,
    due: Option<u64>,
}

impl Machine {
    fn accrue(&mut self, now: u64) {
        let elapsed = now - self.updated;
        self.updated = now;
        if self.active.is_none() {
            return;
        }
        let rate = self.drive.rate();
        let mut powered = elapsed;
        if let Drive::Furnace { fuel_ticks } = &mut self.drive {
            powered = elapsed.min(*fuel_ticks);
            *fuel_ticks -= powered;
        }
        self.remaining = self.remaining.saturating_sub(powered.saturating_mul(rate));
    }

    fn schedule(&mut self, now: u64) -> Option<u64> {
        let rate = self.drive.rate();
        if self.active.is_none() && rate > 0 {
            let space = self.output_capacity - self.output.used;
            self.active = self.recipes.iter().position(|recipe| {
                recipe
                    .inputs
                    .iter()
                    .all(|stack| self.input.amount(stack.resource) >= stack.amount)
                    && recipe.outputs.iter().map(|stack| stack.amount).sum::<u32>() <= space
            });
            if let Some(index) = self.active {
                self.remaining =
                    u64::from(self.recipes[index].base_ticks) * u64::from(REFERENCE_RPM);
            }
        }
        self.active?;
        if self.remaining == 0 {
            return Some(now);
        }
        if rate == 0 {
            return None;
        }
        let duration = self.remaining.div_ceil(rate);
        let delay = match self.drive {
            Drive::Mechanical { .. } => duration,
            Drive::Furnace { fuel_ticks } => duration.min(fuel_ticks),
        };
        now.checked_add(delay)
    }

    fn finish(&mut self) -> Option<u32> {
        if self.remaining != 0 {
            return None;
        }
        let recipe = &self.recipes[self.active.take().unwrap()];
        for stack in &recipe.inputs {
            let taken = self.input.take(stack.resource, stack.amount);
            debug_assert_eq!(taken, stack.amount);
        }
        for stack in &recipe.outputs {
            self.output.put(stack.resource, stack.amount);
        }
        Some(recipe.id)
    }
}

#[derive(Default)]
pub struct RecipeProcessor {
    now: u64,
    machines: HashMap<u32, Machine>,
    wakes: BTreeSet<(u64, u32)>,
}

impl RecipeProcessor {
    pub fn add_machine(
        &mut self,
        id: u32,
        input_capacity: u32,
        output_capacity: u32,
        mut recipes: Vec<Recipe>,
        drive: Drive,
    ) -> Result<(), RecipeError> {
        if output_capacity == 0 {
            return Err(RecipeError::InvalidCapacity);
        }
        let Entry::Vacant(entry) = self.machines.entry(id) else {
            return Err(RecipeError::DuplicateMachine);
        };
        let mut ids = BTreeSet::new();
        let mut input_filter = Vec::new();
        for recipe in &mut recipes {
            if recipe.base_ticks == 0 || recipe.outputs.is_empty() || !ids.insert(recipe.id) {
                return Err(RecipeError::InvalidRecipe);
            }
            normalize(&mut recipe.inputs, input_capacity)?;
            normalize(&mut recipe.outputs, output_capacity)?;
            input_filter.extend(recipe.inputs.iter().map(|stack| stack.resource));
        }
        input_filter.sort_unstable();
        input_filter.dedup();
        entry.insert(Machine {
            input_capacity,
            output_capacity,
            recipes,
            input_filter,
            drive,
            input: Inventory::default(),
            output: Inventory::default(),
            active: None,
            remaining: 0,
            updated: self.now,
            due: None,
        });
        self.reconcile(id);
        Ok(())
    }

    /// Replace drive conditions and retain the active recipe's accumulated work.
    /// For a furnace, fuel_ticks is the new available fuel, in ticks.
    pub fn set_drive(&mut self, id: u32, drive: Drive) -> Result<(), RecipeError> {
        let machine = self
            .machines
            .get_mut(&id)
            .ok_or(RecipeError::UnknownMachine)?;
        machine.accrue(self.now);
        machine.drive = drive;
        self.reconcile(id);
        Ok(())
    }

    /// Inputs reject resources absent from the machine's recipe list.
    pub fn insert(&mut self, id: u32, stack: Stack) -> Result<u32, RecipeError> {
        let machine = self
            .machines
            .get_mut(&id)
            .ok_or(RecipeError::UnknownMachine)?;
        if machine.input_filter.binary_search(&stack.resource).is_err() {
            return Ok(0);
        }
        let amount = stack
            .amount
            .min(machine.input_capacity - machine.input.used);
        if amount > 0 {
            machine.input.put(stack.resource, amount);
            self.reconcile(id);
        }
        Ok(amount)
    }

    pub fn extract(&mut self, id: u32, stack: Stack) -> Result<u32, RecipeError> {
        let machine = self
            .machines
            .get_mut(&id)
            .ok_or(RecipeError::UnknownMachine)?;
        let amount = machine.output.take(stack.resource, stack.amount);
        if amount > 0 {
            self.reconcile(id);
        }
        Ok(amount)
    }

    /// Pull one filtered resource and split it among destination inputs in
    /// caller order. Unaccepted output stays at the source.
    pub fn distribute(
        &mut self,
        source: u32,
        stack: Stack,
        destinations: &[u32],
    ) -> Result<u32, RecipeError> {
        let available = self
            .machines
            .get(&source)
            .ok_or(RecipeError::UnknownMachine)?
            .output
            .amount(stack.resource);
        for &id in destinations {
            if id == source {
                return Err(RecipeError::SelfRoute);
            }
            if !self.machines.contains_key(&id) {
                return Err(RecipeError::UnknownMachine);
            }
        }
        let available = available.min(stack.amount);
        let mut remaining = available;
        for &id in destinations {
            if remaining == 0 {
                break;
            }
            remaining -= self.insert(
                id,
                Stack {
                    amount: remaining,
                    ..stack
                },
            )?;
        }
        self.extract(
            source,
            Stack {
                amount: available - remaining,
                ..stack
            },
        )
    }

    pub fn next_due(&self) -> Option<u64> {
        self.wakes.first().map(|&(due, _)| due)
    }

    /// Move the input clock up to the next pending deadline. The caller must
    /// process earlier deadlines before advancing further.
    pub fn advance_idle_to(&mut self, tick: u64) {
        assert!(tick >= self.now && self.next_due().is_none_or(|due| due >= tick));
        self.now = tick;
    }

    /// Process precisely one deadline, ordered by tick and machine ID.
    pub fn step(&mut self) -> Option<RecipeEvent> {
        let (tick, id) = self.wakes.pop_first()?;
        self.now = tick;
        let machine = self.machines.get_mut(&id).unwrap();
        machine.due = None;
        machine.accrue(tick);
        let recipe = machine.finish();
        self.reconcile(id);
        Some(RecipeEvent {
            tick,
            machine: id,
            recipe,
        })
    }

    fn reconcile(&mut self, id: u32) {
        let machine = self.machines.get_mut(&id).unwrap();
        machine.accrue(self.now);
        let next = machine.schedule(self.now);
        let old = std::mem::replace(&mut machine.due, next);
        if old != next {
            if let Some(old) = old {
                self.wakes.remove(&(old, id));
            }
            if let Some(next) = next {
                self.wakes.insert((next, id));
            }
        }
    }
}

fn normalize(stacks: &mut Vec<Stack>, capacity: u32) -> Result<(), RecipeError> {
    let mut combined = BTreeMap::new();
    let mut remaining = capacity;
    for stack in stacks.iter() {
        if stack.amount == 0 || stack.amount > remaining {
            return Err(RecipeError::InvalidRecipe);
        }
        remaining -= stack.amount;
        *combined.entry(stack.resource).or_default() += stack.amount;
    }
    *stacks = combined
        .into_iter()
        .map(|(resource, amount)| Stack { resource, amount })
        .collect();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(id: u32, amount: u32) -> Stack {
        Stack {
            resource: Resource {
                kind: ResourceKind::Item,
                id,
            },
            amount,
        }
    }

    fn fluid(id: u32, amount: u32) -> Stack {
        Stack {
            resource: Resource {
                kind: ResourceKind::Fluid,
                id,
            },
            amount,
        }
    }

    fn motor(rpm: u32, overstressed: bool) -> Drive {
        Drive::Mechanical { rpm, overstressed }
    }

    #[test]
    fn multiple_input_and_output_stacks_block_and_wake_atomically() {
        let mut p = RecipeProcessor::default();
        let recipe = Recipe {
            id: 0,
            base_ticks: 4,
            inputs: vec![item(1, 1), item(1, 1), fluid(1, 1)],
            outputs: vec![item(2, 1), fluid(2, 1)],
        };
        p.add_machine(1, 6, 2, vec![recipe], motor(32, false))
            .unwrap();
        assert_eq!(p.insert(1, item(99, 9)), Ok(0));
        assert_eq!(p.insert(1, item(1, 4)), Ok(4));
        assert_eq!(p.next_due(), None);
        p.insert(1, fluid(1, 2)).unwrap();
        assert_eq!(
            p.step(),
            Some(RecipeEvent {
                tick: 2,
                machine: 1,
                recipe: Some(0)
            })
        );
        assert_eq!(p.next_due(), None);
        assert_eq!(p.extract(1, item(2, 2)), Ok(1));
        assert_eq!(p.next_due(), None);
        assert_eq!(p.extract(1, fluid(2, 2)), Ok(1));
        assert_eq!(p.next_due(), Some(4));
    }

    #[test]
    fn overstress_and_speed_changes_preserve_progress() {
        let mut p = RecipeProcessor::default();
        let recipe = Recipe {
            id: 1,
            base_ticks: 10,
            inputs: vec![item(1, 1)],
            outputs: vec![item(2, 1)],
        };
        p.add_machine(1, 1, 1, vec![recipe], motor(16, false))
            .unwrap();
        p.insert(1, item(1, 1)).unwrap();
        p.advance_idle_to(4);
        p.set_drive(1, motor(64, true)).unwrap();
        assert_eq!(p.next_due(), None);
        p.advance_idle_to(100);
        p.set_drive(1, motor(32, false)).unwrap();
        assert_eq!(p.step().unwrap().tick, 103);
    }

    #[test]
    fn finite_fuel_sleeps_and_resume_needs_only_remaining_work() {
        let mut p = RecipeProcessor::default();
        let recipe = Recipe {
            id: 1,
            base_ticks: 10,
            inputs: vec![item(1, 1)],
            outputs: vec![item(2, 1)],
        };
        p.add_machine(1, 1, 1, vec![recipe], Drive::Furnace { fuel_ticks: 7 })
            .unwrap();
        p.insert(1, item(1, 1)).unwrap();
        assert_eq!(
            p.step(),
            Some(RecipeEvent {
                tick: 7,
                machine: 1,
                recipe: None
            })
        );
        assert_eq!(p.extract(1, item(2, 1)), Ok(0));
        assert_eq!(p.next_due(), None);
        p.advance_idle_to(100);
        p.set_drive(1, Drive::Furnace { fuel_ticks: 3 }).unwrap();
        assert_eq!(
            p.step(),
            Some(RecipeEvent {
                tick: 103,
                machine: 1,
                recipe: Some(1)
            })
        );
        assert_eq!(p.next_due(), None);
    }

    #[test]
    fn filtered_distribution_spills_to_next_eligible_destination() {
        let mut p = RecipeProcessor::default();
        let first = Recipe {
            id: 1,
            base_ticks: 1,
            inputs: vec![],
            outputs: vec![item(2, 4), fluid(2, 1)],
        };
        p.add_machine(0, 0, 5, vec![first], Drive::Furnace { fuel_ticks: 1 })
            .unwrap();
        for id in 1..=3 {
            let input = if id == 1 { fluid(2, 1) } else { item(2, 2) };
            let recipe = Recipe {
                id,
                base_ticks: 1,
                inputs: vec![input],
                outputs: vec![item(4, 1)],
            };
            p.add_machine(id, 2, 1, vec![recipe], motor(16, false))
                .unwrap();
        }
        assert_eq!(p.step().unwrap().machine, 0);
        assert_eq!(
            p.distribute(0, item(2, 4), &[1, 2, 99]),
            Err(RecipeError::UnknownMachine)
        );
        assert_eq!(p.distribute(0, item(2, 4), &[1, 3, 2]), Ok(4));
        assert_eq!(p.distribute(0, fluid(2, 1), &[1, 2, 3]), Ok(1));
        assert_eq!([1, 2, 3].map(|_| p.step().unwrap().machine), [1, 2, 3]);
    }

    #[test]
    fn exhausted_clock_cannot_wrap_a_deadline() {
        let mut p = RecipeProcessor::default();
        p.advance_idle_to(u64::MAX - 1);
        let recipe = Recipe {
            id: 5,
            base_ticks: 2,
            inputs: vec![],
            outputs: vec![item(3, 1)],
        };
        p.add_machine(1, 0, 1, vec![recipe], motor(16, false))
            .unwrap();
        assert_eq!(p.next_due(), None);
        p.set_drive(1, motor(32, false)).unwrap();
        assert_eq!(p.step().unwrap().tick, u64::MAX);
        assert_eq!(p.next_due(), None);
    }
}
