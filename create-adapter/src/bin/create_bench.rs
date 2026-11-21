// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later

//! Microbenchmark for `create-adapter` on top of `mod-offload`:
//! 1. Settled factory: 10,000 kinetic/belt nodes across 20 regions (idle tick).
//! 2. Active factory: 500 active processing/transport/fluid chains across 16 regions.

use std::hint::black_box;
use std::time::Instant;

use create_adapter::blockstates::{CreateBlockRegistry, DEFAULT_BASE_STATE_ID};
use create_adapter::data_loader::{parse_create_recipe_json, ItemFluidIdTable};
use create_adapter::factory::{CreateFactory, DimBlockPos};
use mod_offload::recipe_processor::{Resource, ResourceKind, Stack};

fn main() {
    let reg = CreateBlockRegistry::core_create(DEFAULT_BASE_STATE_ID);
    let wheel_x = reg
        .with_property(
            reg.default_state("create:water_wheel").unwrap(),
            "axis",
            "x",
        )
        .unwrap();
    let shaft_x = reg
        .with_property(reg.default_state("create:shaft").unwrap(), "axis", "x")
        .unwrap();

    // 1. Settled 10,000-node Create factory (2,000 kinetic shafts/wheels + 8,000 dormant belts).
    let mut settled = CreateFactory::new(reg.clone());
    let mut belt_id = 1u32;
    for r in 0..20i32 {
        let base_x = r * 32;
        for row in 0..5i32 {
            settled.place_block(DimBlockPos::new(0, base_x, 64, row * 2), wheel_x);
            for dx in 1..20i32 {
                settled.place_block(DimBlockPos::new(0, base_x + dx, 64, row * 2), shaft_x);
            }
        }
        let anchor = DimBlockPos::new(0, base_x, 64, 0);
        for _ in 0..400 {
            settled.add_belt_segment(anchor, belt_id, 4, 4).unwrap();
            belt_id += 1;
        }
    }
    // Warm up and settle at tick 1.
    settled.step_tick(1, 10_000, None).unwrap();

    let settled_iters = 50_000u64;
    let t0 = Instant::now();
    let mut settled_events = 0usize;
    for i in 0..settled_iters {
        let out = settled.step_tick(2 + i, 10_000, None).unwrap();
        settled_events += out.len();
    }
    let settled_ns_per_tick = t0.elapsed().as_nanos() as f64 / settled_iters as f64;
    black_box(settled_events);

    // 2. Active 500-chain factory (500 belts + 500 presses + 500 tank pairs across 16 regions).
    let mut ids = ItemFluidIdTable::new();
    let pressing = parse_create_recipe_json(
        "create:pressing/iron_sheet",
        1,
        br#"{"type":"create:pressing","ingredients":[{"item":"minecraft:iron_ingot"}],"results":[{"id":"create:iron_sheet","count":1}],"processing_time":2}"#,
        &mut ids,
    )
    .unwrap()
    .unwrap();
    let ingot = ids.item_id("minecraft:iron_ingot").unwrap();
    let sheet = ids.item_id("create:iron_sheet").unwrap();
    let water = ids.intern_fluid("minecraft:water");

    let windmill = reg
        .with_property(
            reg.default_state("create:windmill_bearing").unwrap(),
            "facing",
            "south",
        )
        .unwrap();
    let press = reg
        .with_property(
            reg.default_state("create:mechanical_press").unwrap(),
            "facing",
            "north",
        )
        .unwrap();
    let mut active = CreateFactory::new(reg);
    let mut chains = Vec::with_capacity(500);
    for c in 0..500u32 {
        let rx = (c as i32 % 16) * 32;
        let slot = c as i32 / 16;
        let local_x = (slot % 16) * 2;
        let local_z = (slot / 16) * 4;
        let p_gen = DimBlockPos::new(0, rx + local_x, 64, local_z);
        let p_press = DimBlockPos::new(0, rx + local_x, 64, local_z + 1);
        active.place_block(p_gen, windmill);
        active.place_block(p_press, press);
        let b_in = c * 4 + 1;
        let b_out = c * 4 + 2;
        let t_a = c * 4 + 3;
        let t_b = c * 4 + 4;
        active.add_belt_segment(p_gen, b_in, 8, 1).unwrap();
        active.add_belt_segment(p_gen, b_out, 8, 1).unwrap();
        active.link_belt(p_gen, b_in, Some(b_out)).unwrap();
        active
            .add_processor(p_press, c + 1, 16, 16, vec![pressing.recipe.clone()])
            .unwrap();
        active.add_fluid_tank(p_gen, t_a, 8000).unwrap();
        active.add_fluid_tank(p_gen, t_b, 8000).unwrap();
        active.link_fluid_tanks(p_gen, t_a, t_b, Some(0)).unwrap();
        chains.push((p_gen, p_press, b_in, b_out, c + 1, t_a));
    }

    let cycles = 200u64;
    let mut tick = 1u64;
    let mut total_records = 0usize;
    let t1 = Instant::now();
    for _ in 0..cycles {
        for &(p_gen, p_press, b_in, _b_out, proc_id, t_a) in &chains {
            let _ = active.insert_belt_item(p_gen, b_in, ingot);
            let _ = active.insert_processor_ingredient(
                p_press,
                proc_id,
                Stack {
                    resource: Resource {
                        kind: ResourceKind::Item,
                        id: ingot,
                    },
                    amount: 1,
                },
            );
            let _ = active.insert_fluid(p_gen, t_a, water, 100);
        }
        for _ in 0..2 {
            let outs = active.step_tick(tick, 10_000, None).unwrap();
            for o in outs {
                total_records += o.batch.records;
            }
            tick += 1;
        }
        for &(_p_gen, p_press, _b_in, b_out, proc_id, _t_a) in &chains {
            let _ = active
                .scheduler_mut()
                .region_mut(p_press.region_key())
                .extract_output(b_out);
            let _ = active.extract_processor_product(
                p_press,
                proc_id,
                Stack {
                    resource: Resource {
                        kind: ResourceKind::Item,
                        id: sheet,
                    },
                    amount: 1,
                },
            );
        }
    }
    let active_us_per_cycle = t1.elapsed().as_secs_f64() * 1e6 / cycles as f64;

    println!(
        "create_bench settled_10k_ns_per_tick={:.1} active_500_chains_us_per_2tick_cycle={:.2} total_records={}",
        settled_ns_per_tick, active_us_per_cycle, total_records
    );
}
