// SPDX-License-Identifier: LGPL-3.0-or-later

//! Multi-mod microbenchmark for `mod-adapter-core`'s declarative
//! [`UniversalModRuntime`] on `mod-offload`:
//! 1. Settled multi-mod factory: 10,000 dormant cables/pipes/tanks from the
//!    built-in AE2/Mekanism/Pipez-class profiles across 20 regions spanning
//!    two dimensions (Overworld `dim=0` + custom `dim=3`) — wall ns/tick.
//! 2. Active multi-mod workload: 500 generic tech chains (generator -> cable
//!    -> crusher + item-pipe pair + tank pair) across 16 regions —
//!    wall us/cycle and exported S5 delta records.

use std::hint::black_box;
use std::time::Instant;

use mod_adapter_core::blockstates::DEFAULT_BASE_STATE_ID;
use mod_adapter_core::data_loader::{parse_mod_recipe_json, ItemFluidIdTable};
use mod_adapter_core::profile::{builtin_reference_profiles, DimBlockPos, UniversalModRuntime};
use mod_offload::recipe_processor::{Resource, ResourceKind, Stack};

fn main() {
    let profiles = builtin_reference_profiles();

    // 1. Settled 10,000-node multi-mod factory (20 regions x 500 nodes,
    //    split evenly between dim 0 and custom dim 3).
    let mut settled = UniversalModRuntime::from_profiles(DEFAULT_BASE_STATE_ID, &profiles).unwrap();
    let gen = settled
        .registry()
        .default_state("mekanism:heat_generator")
        .unwrap();
    let cable = settled
        .registry()
        .default_state("mekanism:universal_cable")
        .unwrap();
    let ae_drive = settled.registry().default_state("ae2:drive").unwrap();
    let item_pipe = settled.registry().default_state("pipez:item_pipe").unwrap();
    let fluid_pipe = settled
        .registry()
        .default_state("mekanism:mechanical_pipe")
        .unwrap();
    let fluid_tank = settled
        .registry()
        .default_state("mekanism:basic_fluid_tank")
        .unwrap();

    for r in 0..20i32 {
        let dim = if r < 10 { 0 } else { 3 };
        let base_x = (r % 10) * 32;
        // 125-node powered cable network: 5 generators each feeding a
        // 23-cable bus terminated by an AE2 storage drive.
        for row in 0..5i32 {
            settled.place_block(DimBlockPos::new(dim, base_x, 64, row), gen, Vec::new());
            for dx in 1..24i32 {
                settled.place_block(
                    DimBlockPos::new(dim, base_x + dx, 64, row),
                    cable,
                    Vec::new(),
                );
            }
            settled.place_block(
                DimBlockPos::new(dim, base_x + 24, 64, row),
                ae_drive,
                Vec::new(),
            );
        }
        // 200 dormant sleeping item pipes in 10 linked chains of 20.
        for row in 0..10i32 {
            let z = 8 + row;
            let mut prev = None;
            for dx in 0..20i32 {
                let pos = DimBlockPos::new(dim, base_x + dx, 64, z);
                settled.place_block(pos, item_pipe, Vec::new());
                if let Some(upstream) = prev {
                    assert!(settled.link_transport(upstream, pos));
                }
                prev = Some(pos);
            }
        }
        // 175-node pressurized fluid network: 7 rows of tank + 24 pipes.
        for row in 0..7i32 {
            let z = 20 + row;
            for dx in 0..25i32 {
                let state = if dx == 0 { fluid_tank } else { fluid_pipe };
                settled.place_block(DimBlockPos::new(dim, base_x + dx, 64, z), state, Vec::new());
            }
        }
    }
    // Warm up and let every fluid link resolve at tick 1.
    settled.step_tick(1, 10_000, None);

    let settled_iters = 50_000u64;
    let t0 = Instant::now();
    let mut settled_events = 0usize;
    for i in 0..settled_iters {
        settled_events += settled.step_tick(2 + i, 10_000, None).len();
    }
    let settled_ns_per_tick = t0.elapsed().as_nanos() as f64 / settled_iters as f64;
    black_box(settled_events);

    // 2. Active 500-chain multi-mod workload across 16 regions (8 per dim).
    let mut ids = ItemFluidIdTable::new();
    let crushing = parse_mod_recipe_json(
        "mekanism:crushing/bio_fuel",
        1,
        br#"{"type":"mekanism:crushing","input":{"item":"minecraft:wheat"},"output":{"id":"mekanism:bio_fuel","count":2},"processing_time":2}"#,
        &mut ids,
    )
    .unwrap()
    .unwrap();
    let wheat = ids.item_id("minecraft:wheat").unwrap();
    let bio_fuel = ids.item_id("mekanism:bio_fuel").unwrap();
    let water = ids.intern_fluid("minecraft:water");

    let mut active = UniversalModRuntime::from_profiles(DEFAULT_BASE_STATE_ID, &profiles).unwrap();
    let crusher = active.registry().default_state("mekanism:crusher").unwrap();
    let mut chains = Vec::with_capacity(500);
    for c in 0..500u32 {
        let region_idx = c as i32 % 16;
        let dim = if region_idx < 8 { 0 } else { 3 };
        let slot = c as i32 / 16;
        let local_x = (slot % 10) * 3;
        let local_z = (slot / 10) * 10;
        let base_x = (region_idx % 8) * 32;
        let p_gen = DimBlockPos::new(dim, base_x + local_x, 64, local_z);
        let p_cable = DimBlockPos::new(dim, base_x + local_x + 1, 64, local_z);
        let p_crusher = DimBlockPos::new(dim, base_x + local_x + 2, 64, local_z);
        let p_pipe_in = DimBlockPos::new(dim, base_x + local_x, 65, local_z);
        let p_pipe_out = DimBlockPos::new(dim, base_x + local_x + 1, 65, local_z);
        let p_tank_a = DimBlockPos::new(dim, base_x + local_x, 66, local_z);
        let p_tank_b = DimBlockPos::new(dim, base_x + local_x + 1, 66, local_z);
        active.place_block(p_gen, gen, Vec::new());
        active.place_block(p_cable, cable, Vec::new());
        assert!(active.network_running(p_cable));
        active.place_block(p_crusher, crusher, vec![crushing.recipe.clone()]);
        active.place_block(p_pipe_in, item_pipe, Vec::new());
        active.place_block(p_pipe_out, item_pipe, Vec::new());
        assert!(active.link_transport(p_pipe_in, p_pipe_out));
        active.place_block(p_tank_a, fluid_tank, Vec::new());
        active.place_block(p_tank_b, fluid_tank, Vec::new());
        chains.push((p_crusher, p_pipe_in, p_pipe_out, p_tank_a));
    }

    let cycles = 200u64;
    let mut tick = 1u64;
    let mut total_records = 0usize;
    let t1 = Instant::now();
    for _ in 0..cycles {
        for &(p_crusher, p_pipe_in, _, p_tank_a) in &chains {
            let _ = active.insert_transport_item(p_pipe_in, wheat);
            active.insert_processor_input(
                p_crusher,
                Stack {
                    resource: Resource {
                        kind: ResourceKind::Item,
                        id: wheat,
                    },
                    amount: 1,
                },
            );
            let _ = active.insert_tank_fluid(p_tank_a, water, 100);
        }
        for _ in 0..2 {
            for out in active.step_tick(tick, 10_000, None) {
                total_records += out.batch.records;
            }
            tick += 1;
        }
        for &(p_crusher, _, p_pipe_out, _) in &chains {
            let _ = active.extract_transport_output(p_pipe_out);
            let _ = active.extract_processor_output(
                p_crusher,
                Stack {
                    resource: Resource {
                        kind: ResourceKind::Item,
                        id: bio_fuel,
                    },
                    amount: 1,
                },
            );
        }
    }
    let active_us_per_cycle = t1.elapsed().as_secs_f64() * 1e6 / cycles as f64;

    println!(
        "universal_bench settled_10k_multi_mod_2dims_ns_per_tick={:.1} active_500_chains_us_per_2tick_cycle={:.2} s5_records={}",
        settled_ns_per_tick, active_us_per_cycle, total_records
    );
}
