use super::*;

// Frozen v2.37 tick body; only the explicit builtin resolver argument is adapted.
fn reference_tick() {
    let mut fs = furnaces().lock().unwrap();
    let mut dirty_pos: Vec<(i32, i32, i32)> = Vec::new();
    for (pos, f) in fs.iter_mut() {
        if !is_warm(pos.0, pos.2) {
            continue;
        }
        let out = smelt_out(f.input.0, smelt_result);
        let can_smelt = f.input.1 > 0
            && out
                .as_ref()
                .map(|o| {
                    f.output.1 == 0
                        || (f.output.0 == o.0
                            && f.output.2 == o.2
                            && f.output.1 as i64 + o.1 as i64 <= stack_max_of(&f.output))
                })
                .unwrap_or(false);
        if f.burn_left <= 0 && can_smelt && f.fuel.1 > 0 {
            let t = fuel_ticks(f.fuel.0);
            if t > 0 {
                f.burn_left = t;
                f.fuel.1 -= 1;
                if f.fuel.1 == 0 {
                    f.fuel = (0, 0, Vec::new());
                }
                f.dirty = true;
            }
        }
        if f.burn_left > 0 && can_smelt {
            f.burn_left -= 1;
            f.progress += 1;
            if f.progress >= 40 {
                f.progress = 0;
                let r = out.clone().unwrap();
                f.xp += smelt_xp(f.input.0);
                f.output = (r.0, f.output.1 + r.1, f.output.2.clone());
                f.input.1 -= 1;
                if f.input.1 == 0 {
                    f.input = (0, 0, Vec::new());
                }
                f.dirty = true;
            }
        } else if f.progress > 0 {
            f.progress -= 1;
        }
        if f.dirty {
            f.dirty = false;
            dirty_pos.push(*pos);
        }
    }
    drop(fs);
    if dirty_pos.is_empty() {
        return;
    }
    let cs: Vec<Arc<Client>> = { clients().lock().unwrap().iter().cloned().collect() };
    for c in cs {
        let fp = *c.open_furnace.lock().unwrap();
        if let Some(fp) = fp {
            if dirty_pos.contains(&fp) {
                let inv = c.inv.lock().unwrap().clone();
                if let Ok(mut w) = c.sock.lock() {
                    furnace_push_items(&mut w, fp, &inv);
                }
            }
        }
    }
}

fn new_furnace() -> Furnace {
    Furnace {
        input: (0, 0, Vec::new()),
        fuel: (0, 0, Vec::new()),
        output: (0, 0, Vec::new()),
        burn_left: 0,
        progress: 0,
        dirty: false,
        xp: 0.0,
        recipe_cache: CachedLookup::default(),
        fuel_cache: CachedLookup::default(),
    }
}

#[derive(Debug, PartialEq)]
struct Frame {
    input: Stack,
    fuel: Stack,
    output: Stack,
    burn_left: i32,
    progress: i32,
    xp: f32,
}

fn run(scenario: usize, tick: fn()) -> Vec<Frame> {
    let pos = (0, 70, 0);
    let iron = name_to_item("raw_iron").unwrap();
    let ingot = name_to_item("iron_ingot").unwrap();
    let gold = name_to_item("gold_ingot").unwrap();
    let coal = name_to_item("coal").unwrap();
    let diamond = name_to_item("diamond").unwrap();
    json_smelts().lock().unwrap().clear();
    item_tags().lock().unwrap().remove("test:smelt");
    warm().lock().unwrap().insert((0, 0));
    furnaces().lock().unwrap().clear();
    furnaces().lock().unwrap().insert(pos, new_furnace());
    let mut trace = Vec::new();
    for time in 0..160 {
        {
            let mut fs = furnaces().lock().unwrap();
            let f = fs.get_mut(&pos).unwrap();
            if time == 10 {
                f.input = (if scenario == 2 { diamond } else { iron }, 3, Vec::new());
                f.fuel = (coal, 1, Vec::new());
            }
            if scenario == 1 {
                if time == 20 {
                    f.output = (ingot, 64, Vec::new());
                }
                if time == 35 {
                    f.output = (0, 0, Vec::new());
                }
                if time == 75 {
                    f.input = (diamond, 1, Vec::new());
                }
                if time == 90 {
                    f.input = (iron, 1, Vec::new());
                }
            }
            if scenario == 2 {
                if time == 20 {
                    // Cached builtin miss must not conceal a newly added datapack recipe.
                    json_smelts()
                        .lock()
                        .unwrap()
                        .push((Ingr::Item(diamond), gold, 2, 1.5));
                }
                if time == 65 {
                    json_smelts().lock().unwrap().clear();
                    json_smelts().lock().unwrap().push((
                        Ingr::Tag("test:smelt".into()),
                        gold,
                        2,
                        1.5,
                    ));
                }
                if time == 70 {
                    item_tags()
                        .lock()
                        .unwrap()
                        .insert("test:smelt".into(), vec!["diamond".into()]);
                }
                if time == 85 {
                    item_tags().lock().unwrap().remove("test:smelt");
                }
                if time == 100 {
                    f.input = (iron, 1, Vec::new());
                    f.output = (0, 0, Vec::new());
                    f.fuel = (coal, 1, Vec::new());
                }
            }
        }
        tick();
        let fs = furnaces().lock().unwrap();
        let f = fs.get(&pos).unwrap();
        trace.push(Frame {
            input: f.input.clone(),
            fuel: f.fuel.clone(),
            output: f.output.clone(),
            burn_left: f.burn_left,
            progress: f.progress,
            xp: f.xp,
        });
    }
    trace
}

#[test]
fn cached_tick_matches_v237_with_input_output_recipe_and_tag_changes() {
    for scenario in 0..3 {
        let expected = run(scenario, reference_tick);
        let actual = run(scenario, furnace_tick);
        assert_eq!(actual, expected, "scenario {scenario}");
        let produced: Vec<usize> = actual
            .windows(2)
            .enumerate()
            .filter_map(|(i, w)| (w[1].output.1 > w[0].output.1).then_some(i + 1))
            .collect();
        if scenario == 0 {
            assert_eq!(produced, vec![49, 89, 129]);
            let last = actual.last().unwrap();
            assert_eq!(
                last.output,
                (name_to_item("iron_ingot").unwrap(), 3, Vec::new())
            );
            assert_eq!(last.input.1, 0);
            assert_eq!(last.fuel.1, 0);
        }
        if scenario == 2 {
            assert_eq!(
                actual[59].output,
                (name_to_item("gold_ingot").unwrap(), 2, Vec::new())
            );
            assert_eq!(
                actual[139].output,
                (name_to_item("iron_ingot").unwrap(), 1, Vec::new())
            );
        }
    }
    furnaces().lock().unwrap().clear();
    json_smelts().lock().unwrap().clear();
    item_tags().lock().unwrap().remove("test:smelt");
    warm().lock().unwrap().remove(&(0, 0));
}

#[test]
fn fuel_miss_does_not_delay_valid_fuel() {
    let mut cache = CachedLookup::default();
    let diamond = name_to_item("diamond").unwrap();
    let coal = name_to_item("coal").unwrap();
    assert_eq!(cache.get_or_compute(diamond, || fuel_ticks(diamond)), 0);
    assert_eq!(cache.get_or_compute(coal, || fuel_ticks(coal)), 320);
}
