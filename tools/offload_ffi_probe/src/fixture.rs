//! Clean-room workloads driven through the same C ABI as the JNI bridge.

use std::ptr;

use mod_offload::ffi::*;
use mod_offload::region_scheduler::MAX_BATCH_BYTES;

use crate::{status, RegionHandle, Result, INVALID_ARGUMENT};

const MACHINES: u32 = 10_000;
const NODES: u32 = 2_000;
const NODES_PER_NETWORK: u32 = 100;

fn expect(condition: bool) -> Result {
    condition.then_some(()).ok_or(INVALID_ARGUMENT)
}

pub fn settle(region: &mut RegionHandle) -> Result {
    let handle = region.region.as_ptr();
    // The region is exclusively owned and all output pointers are to local storage.
    unsafe {
        for id in 0..MACHINES {
            status(mso_transport_add(handle, id, 1, 1))?;
            if id >= MACHINES / 2 {
                status(mso_transport_insert(handle, id, 7))?;
            }
        }
        let mut scratch = vec![0; MAX_BATCH_BYTES];
        let mut processed = 0;
        loop {
            let step = region.step(1)?;
            processed += step.processed;
            region.export(scratch.as_mut_ptr(), scratch.len())?;
            if step.has_backlog == 0 {
                break;
            }
        }
        expect(processed == (MACHINES / 2) as usize)?;
        for id in MACHINES / 2..MACHINES {
            status(mso_transport_insert(handle, id, 8))?;
        }

        for id in 0..NODES {
            let driver = id.is_multiple_of(NODES_PER_NETWORK);
            let node = if driver {
                MsoNode {
                    capacity: f64::from(NODES_PER_NETWORK),
                    load: 0.0,
                    speed: 16.0,
                    driven: 1,
                }
            } else {
                MsoNode {
                    capacity: 0.0,
                    load: 1.0,
                    speed: 0.0,
                    driven: 0,
                }
            };
            status(mso_kinetic_add(handle, id, node))?;
            if !driver {
                status(mso_kinetic_link(handle, id - 1, id, 1, -1.0))?;
            }
        }
        for id in 0..NODES {
            let mut solution = MsoSolution::default();
            status(mso_kinetic_get(handle, id, &mut solution))?;
            expect(solution.status == 1 && solution.speed.abs() == 16.0)?;
        }
        expect(region.step(2)?.processed == 0)?;
    }
    Ok(())
}

const ORE: MsoStack = MsoStack {
    kind: 0,
    id: 1,
    amount: 1,
};
const WATER: MsoStack = MsoStack {
    kind: 1,
    id: 1,
    amount: 1,
};
const PRODUCT: MsoStack = MsoStack {
    kind: 0,
    id: 2,
    amount: 1,
};
const BYPRODUCT: MsoStack = MsoStack {
    kind: 1,
    id: 2,
    amount: 1,
};

pub struct Factory {
    active: u32,
    cycle: u64,
    phase: i32,
}

impl Factory {
    pub fn new(region: &mut RegionHandle, dormant: u32, active: u32) -> Result<Self> {
        expect((1..=500).contains(&active) && dormant <= 5000)?;
        let handle = region.region.as_ptr();
        let inputs = [ORE, WATER];
        let outputs = [PRODUCT, BYPRODUCT];
        let recipe = MsoRecipe {
            id: 1,
            base_ticks: 2,
            inputs: inputs.as_ptr(),
            input_count: inputs.len(),
            outputs: outputs.as_ptr(),
            output_count: outputs.len(),
        };
        // The C ABI copies recipe and stack descriptors before returning.
        unsafe {
            for id in 0..dormant + active {
                let source = id * 2;
                let sink = source + 1;
                status(mso_kinetic_add(handle, source, driver(32.0)))?;
                status(mso_kinetic_add(
                    handle,
                    sink,
                    MsoNode {
                        capacity: 0.0,
                        load: 1.0,
                        speed: 0.0,
                        driven: 0,
                    },
                ))?;
                status(mso_kinetic_link(handle, source, sink, 1, 1.0))?;
                status(mso_transport_add(handle, id, 1, 1))?;
                status(mso_fluid_add(handle, source, 2, 0))?;
                status(mso_fluid_add(handle, sink, 2, 0))?;
                status(mso_fluid_link(handle, source, sink, 1, 0))?;
                status(mso_recipe_add(
                    handle,
                    id,
                    2,
                    2,
                    drive(handle, sink)?,
                    &recipe,
                    1,
                ))?;
            }
        }

        // Settle construction-time fluid edits before the first timed tick.
        loop {
            if region.step(0)?.has_backlog == 0 {
                break;
            }
        }
        expect(region.export(ptr::null_mut(), 0)? == 0)?;
        Ok(Self {
            active,
            cycle: 0,
            phase: 0,
        })
    }

    pub fn prepare(&mut self, handle: *mut MsoRegion, tick: u64, phase: i32) -> Result {
        expect(phase == self.phase && tick == self.cycle * 2 + phase as u64)?;
        // The caller has exclusive access to the live region. All C outputs
        // below refer to local storage.
        unsafe {
            for id in 0..self.active {
                let source = id * 2;
                let sink = source + 1;
                let mut amount = 0;
                match phase {
                    0 => {
                        if self.cycle.is_multiple_of(8) {
                            let rpm = if self.cycle.is_multiple_of(16) {
                                64.0
                            } else {
                                32.0
                            };
                            status(mso_kinetic_update(handle, source, driver(rpm)))?;
                            status(mso_recipe_set_drive(handle, id, drive(handle, sink)?))?;
                        }
                        status(mso_transport_insert(handle, id, ORE.id))?;
                        status(mso_fluid_insert(handle, source, WATER.id, 2, &mut amount))?;
                        expect(amount == 2)?;
                    }
                    1 => {
                        let mut ore = MsoItem::default();
                        status(mso_transport_extract(handle, id, &mut ore))?;
                        expect(ore.present == 1 && ore.item == ORE.id)?;
                        for tank in [source, sink] {
                            status(mso_fluid_extract(handle, tank, 2, &mut amount))?;
                            expect(amount == 1)?;
                        }
                        for stack in [ORE, WATER] {
                            status(mso_recipe_insert(handle, id, stack, &mut amount))?;
                            expect(amount == 1)?;
                        }
                    }
                    2 => {
                        for stack in [PRODUCT, BYPRODUCT] {
                            status(mso_recipe_extract(handle, id, stack, &mut amount))?;
                            expect(amount == 1)?;
                        }
                    }
                    _ => return Err(INVALID_ARGUMENT),
                }
            }
        }
        self.phase = (phase + 1) % 3;
        if self.phase == 0 {
            self.cycle += 1;
        }
        Ok(())
    }
}

fn driver(speed: f64) -> MsoNode {
    MsoNode {
        capacity: 2.0,
        load: 0.0,
        speed,
        driven: 1,
    }
}

unsafe fn drive(handle: *mut MsoRegion, node: u32) -> Result<MsoDrive> {
    let mut solution = MsoSolution::default();
    status(unsafe { mso_kinetic_get(handle, node, &mut solution) })?;
    expect(solution.status == 1 && matches!(solution.speed, 32.0 | 64.0))?;
    Ok(MsoDrive {
        kind: 0,
        rpm: solution.speed as u32,
        fuel_ticks: 0,
        overstressed: 0,
    })
}
