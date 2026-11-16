use super::{freg, gen_cf, Cf, LargeDripstoneCfg};
use crate::block_registry::{breg, bs_parts};
use crate::generation_context::FC;
use crate::generation_math::{mth_cos, mth_next_f32, mth_sin, normal_new_l, Xor};
use crate::generation_providers::{p_fp, p_hp, p_ip, test_pred, test_rt, Bsp, Pr, FP, RT};
use crate::json::{jarr, jparse, js, jstr, J};
use crate::worldgen::Gen;

fn fossil_set_json(object: &mut J, key: &str, value: J) {
    let J::Obj(fields) = object else {
        panic!("expected object")
    };
    if let Some((_, old)) = fields.iter_mut().find(|(k, _)| k == key) {
        *old = value;
    } else {
        fields.push((key.to_owned(), value));
    }
}

fn fossil_case_feature(case: &J) -> Cf {
    use crate::structure_template::{BlockInfo, Template};
    let reg = breg().unwrap();
    let id = format!("minecraft:{}", jstr(case, "config").unwrap());
    let bytes = Gen::dp_resource("worldgen/configured_feature", &id, "json").unwrap();
    let mut root = jparse(&bytes).unwrap();
    let config = js(&root, "config").unwrap();
    let mut config = config.clone();
    if js(case, "single").is_some() {
        for key in ["fossil_structures", "overlay_structures"] {
            let first = jarr(&config, key).unwrap()[0].clone();
            fossil_set_json(&mut config, key, J::Arr(vec![first]));
        }
    }
    if let Some(J::Obj(overrides)) = js(case, "overrides") {
        for (key, value) in overrides {
            fossil_set_json(&mut config, key, value.clone());
        }
    }
    fossil_set_json(&mut root, "config", config);
    let mut cf = super::p_cf(&root).expect("fossil configured feature must resolve all children");
    let Cf::Fossil(fossil) = &mut cf else {
        panic!("fossil dispatch parsed as another type")
    };
    for (key, templates, processors) in [
        ("base_template", &mut fossil.base, &fossil.base_processors),
        (
            "overlay_template",
            &mut fossil.overlay,
            &fossil.overlay_processors,
        ),
    ] {
        if let Some(spec) = js(case, key) {
            let size = jarr(spec, "size").unwrap();
            let palettes = jarr(spec, "palettes")
                .unwrap()
                .iter()
                .map(|values| {
                    let mut blocks: Vec<_> = values
                        .as_arr()
                        .unwrap()
                        .iter()
                        .map(|b| {
                            let pos = jarr(b, "pos").unwrap();
                            let (name, properties) = bs_parts(js(b, "state").unwrap()).unwrap();
                            BlockInfo {
                                pos: (
                                    pos[0].num() as i32,
                                    pos[1].num() as i32,
                                    pos[2].num() as i32,
                                ),
                                state: reg.state_of(&name, properties),
                                nbt: None,
                            }
                        })
                        .collect();
                    blocks.sort_by_key(|b| (b.pos.1, b.pos.0, b.pos.2));
                    blocks
                })
                .collect();
            let template = Template {
                size: (
                    size[0].num() as i32,
                    size[1].num() as i32,
                    size[2].num() as i32,
                ),
                palettes,
                entities: Vec::new(),
            };
            template.validate_solid_placement(processors, reg).unwrap();
            assert_eq!(
                templates.len(),
                1,
                "synthetic templates require an explicit single pair"
            );
            templates[0] = template;
        }
    }
    cf
}

fn fossil_native_case(case: &J, row: &[&str], through_dispatch: bool) {
    use crate::block_registry::HeightmapKind;
    let reg = breg().unwrap();
    let cf = fossil_case_feature(case);
    let origin = jarr(case, "origin").unwrap();
    let origin = (
        origin[0].num() as i32,
        origin[1].num() as i32,
        origin[2].num() as i32,
    );
    let seed = jstr(case, "seed").unwrap().parse::<i64>().unwrap() as u64;
    let mut g = Gen::load(0, crate::name_to_state).unwrap();
    let mut ctx = context(&mut g);
    ctx.test_no_terrain = true;
    ctx.test_writes = Some(Vec::new());
    let J::Obj(inputs) = jparse(row[7].as_bytes()).unwrap() else {
        panic!("missing initial native cells")
    };
    for (key, name) in inputs {
        let p: Vec<i32> = key.split(',').map(|v| v.parse().unwrap()).collect();
        ctx.test_base_cell((p[0], p[1], p[2]), reg.state_of(name.str(), None));
    }
    let J::Obj(heights) = jparse(row[6].as_bytes()).unwrap() else {
        panic!("missing frozen native heights")
    };
    for (key, height) in heights {
        let p: Vec<i32> = key.split(',').map(|v| v.parse().unwrap()).collect();
        ctx.test_frozen_height(HeightmapKind::OceanFloorWg, p[0], p[1], height.num() as i32);
    }
    let mut rng = Xor::seed_worldgen(seed);
    if through_dispatch {
        gen_cf(&cf, freg(), &mut ctx, &mut rng, origin);
    } else {
        let Cf::Fossil(fossil) = &cf else {
            panic!("not fossil")
        };
        assert_eq!(
            fossil.place(&mut ctx, &mut rng, origin),
            row[1].parse::<bool>().unwrap(),
            "native result: {}",
            row[0]
        );
    }
    let sha = |s: &str| -> String {
        crate::generation_math::sha256(s.as_bytes())
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    };
    let cell = |p: (i32, i32, i32), state: u32| {
        format!(
            "{},{},{} {}\n",
            p.0,
            p.1,
            p.2,
            block_state_cells_text(reg, state)
        )
    };
    let mut cells: Vec<_> = ctx.wr.iter().map(|(&p, &state)| cell(p, state)).collect();
    cells.sort();
    assert_eq!(
        ctx.wr.len(),
        row[2].parse::<usize>().unwrap(),
        "native cell count: {}",
        row[0]
    );
    assert_eq!(
        sha(&cells.concat()),
        row[3],
        "native full-state cells: {}",
        row[0]
    );
    let writes: String = ctx
        .test_writes
        .as_ref()
        .unwrap()
        .iter()
        .map(|&(p, state)| cell(p, state))
        .collect();
    assert_eq!(sha(&writes), row[4], "native ordered writes: {}", row[0]);
    let probe = format!(
        "{},{},{}",
        rng.next_bound(1000),
        rng.next_bound(1000),
        rng.next_bound(1000)
    );
    assert_eq!(probe, row[5], "native following RNG: {}", row[0]);
    let mut per_chunk: std::collections::BTreeMap<String, Vec<String>> = Default::default();
    let mut pending: Vec<_> = ctx
        .g
        .generated_fluid_ticks()
        .iter()
        .map(|&(p, kind)| {
            let name = match kind {
                0 => "minecraft:water",
                1 => "minecraft:flowing_water",
                2 => "minecraft:lava",
                3 => "minecraft:flowing_lava",
                _ => panic!("unknown fluid"),
            };
            let cell = format!("{},{},{} {name}", p.0, p.1, p.2);
            per_chunk
                .entry(format!("{},{}", p.0 >> 4, p.2 >> 4))
                .or_default()
                .push(cell.clone());
            cell
        })
        .collect();
    pending.sort();
    let wanted = jparse(row[8].as_bytes()).unwrap();
    let wanted: Vec<_> = wanted
        .as_arr()
        .unwrap()
        .iter()
        .map(|v| v.str().to_owned())
        .collect();
    assert_eq!(
        pending, wanted,
        "native proto-fluid tick membership (not delivery order): {}",
        row[0]
    );
    let reference = include_str!("../tests/data/fossil_proto_fluids_1211.tsv")
        .lines()
        .map(|line| line.split_once('\t').unwrap())
        .find(|(id, _)| *id == row[0])
        .expect("native proto-fluid order case")
        .1;
    let J::Obj(groups) = jparse(reference.as_bytes()).unwrap() else {
        panic!("expected native chunk groups");
    };
    let wanted: std::collections::BTreeMap<_, _> = groups
        .into_iter()
        .map(|(chunk, cells)| {
            (
                chunk,
                cells
                    .as_arr()
                    .unwrap()
                    .iter()
                    .map(|v| v.str().to_owned())
                    .collect::<Vec<_>>(),
            )
        })
        .collect();
    assert_eq!(
        per_chunk, wanted,
        "native per-chunk proto-fluid insertion (not delivery): {}",
        row[0]
    );
}

#[test]
fn fossil_feature_matches_actual_native_placement() {
    let cases = jparse(include_bytes!("../tests/data/fossil_cases_1211.json")).unwrap();
    let cases = cases.as_arr().unwrap();
    let rows: Vec<_> = include_str!("../tests/data/fossil_1211.tsv")
        .lines()
        .collect();
    assert_eq!(
        cases.len(),
        rows.len(),
        "every declared native case must execute"
    );
    assert!(!rows.is_empty());
    for (case, line) in cases.iter().zip(rows) {
        let row: Vec<_> = line.split('\t').collect();
        assert_eq!(row.len(), 9);
        assert_eq!(jstr(case, "id").unwrap(), row[0]);
        fossil_native_case(case, &row, false);
    }
}

#[test]
fn fossil_window_clipping_matches_native() {
    let cases = jparse(include_bytes!("../tests/data/fossil_cases_1211.json")).unwrap();
    let rows: Vec<_> = include_str!("../tests/data/fossil_1211.tsv")
        .lines()
        .collect();
    let mut checked = 0;
    for (case, line) in cases.as_arr().unwrap().iter().zip(rows) {
        if jstr(case, "id").unwrap().starts_with("wide-") {
            fossil_native_case(case, &line.split('\t').collect::<Vec<_>>(), false);
            checked += 1;
        }
    }
    assert!(checked > 0, "clipping cases disappeared");
}

#[test]
fn fossil_production_dispatch_matches_native() {
    let cases = jparse(include_bytes!("../tests/data/fossil_cases_1211.json")).unwrap();
    let rows: Vec<_> = include_str!("../tests/data/fossil_1211.tsv")
        .lines()
        .collect();
    let mut checked = 0;
    for (case, line) in cases.as_arr().unwrap().iter().zip(rows) {
        let id = jstr(case, "id").unwrap();
        if [
            "fossil_coal-0-8-50-8",
            "fossil_diamonds--1-16--60-31",
            "half-water-0",
            "write-through-solid-0",
            "wide-clip-0",
            "all-rotted-0",
            "air-0",
        ]
        .contains(&id.as_str())
        {
            fossil_native_case(case, &line.split('\t').collect::<Vec<_>>(), true);
            checked += 1;
        }
    }
    assert_eq!(checked, 7, "production-route membership changed");
}

#[test]
fn fossil_required_resources_and_children_fail_closed() {
    let id = "minecraft:fossil_coal";
    let base = Gen::dp_resource("worldgen/configured_feature", id, "json").unwrap();
    for (key, value) in [
        ("fossil_structures", J::Arr(vec![])),
        ("overlay_structures", J::Arr(vec![])),
        (
            "fossil_structures",
            J::Arr(vec![J::Str("minecraft:missing_fossil".to_owned())]),
        ),
        ("max_empty_corners_allowed", J::Num(8.0)),
        ("fossil_processors", jparse(br#"{"processors":[{"processor_type":"minecraft:rule","rules":[{"input_predicate":{"predicate_type":"minecraft:always_true"},"location_predicate":{"predicate_type":"minecraft:always_true"},"output_state":{"Name":"minecraft:torch"}}]}]}"#).unwrap()),
        (
            "fossil_processors",
            jparse(br#"{"processors":[{"processor_type":"minecraft:unsupported"}]}"#).unwrap(),
        ),
    ] {
        let mut root = jparse(&base).unwrap();
        let mut config = js(&root, "config").unwrap().clone();
        fossil_set_json(&mut config, key, value);
        fossil_set_json(&mut root, "config", config);
        assert!(
            super::p_cf(&root).is_none(),
            "required child was silently skipped: {key}"
        );
    }
    for (value, wanted) in [(4.5, 4), (-0.5, 0)] {
        let mut root = jparse(&base).unwrap();
        let mut config = js(&root, "config").unwrap().clone();
        fossil_set_json(&mut config, "max_empty_corners_allowed", J::Num(value));
        fossil_set_json(&mut root, "config", config);
        let Some(Cf::Fossil(parsed)) = super::p_cf(&root) else {
            panic!("native integer control rejected")
        };
        assert_eq!(
            parsed.max_empty_corners, wanted,
            "native int codec control: {value}"
        );
    }
    for bad in [
        "../minecraft:fossil/spine_1",
        "..:fossil/spine_1",
        "minecraft:../fossil/spine_1",
        "minecraft:/fossil/spine_1",
    ] {
        assert!(
            Gen::dp_resource("structure", bad, "nbt").is_none(),
            "resource escaped namespace: {bad}"
        );
    }
}

#[test]
fn fossil_scripted_inputs_are_fail_closed_and_write_through() {
    use crate::block_registry::HeightmapKind;
    let mut g = Gen::load(0, crate::name_to_state).unwrap();
    let mut ctx = context(&mut g);
    ctx.test_no_terrain = true;
    let missing = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| ctx.get(3, 30, 4)));
    assert!(
        missing.is_err(),
        "missing native cells fell through to generated terrain"
    );
    let missing = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| ctx.hm_oc_wg(3, 4)));
    assert!(
        missing.is_err(),
        "missing native frozen maps fell through to generated terrain"
    );
    let reg = breg().unwrap();
    let stone = reg.state_of("minecraft:stone", None);
    let bone = reg.state_of("minecraft:bone_block", None);
    ctx.test_base_cell((3, 30, 4), stone);
    ctx.test_frozen_height(HeightmapKind::OceanFloorWg, 3, 4, 64);
    assert_eq!(ctx.get(3, 30, 4), stone);
    ctx.set(3, 30, 4, bone);
    assert_eq!(
        ctx.get(3, 30, 4),
        bone,
        "captured base cells must not mask writes"
    );
    assert_eq!(
        ctx.hm_oc_wg(3, 4),
        64,
        "placement writes must not alter frozen maps"
    );
}

fn context(g: &mut Gen) -> FC<'_> {
    FC::new(g)
}

/// Outer match-arm coverage missed supported types rejected by nested field codecs.
#[test]
fn pinned_configured_features_parse_except_explicit_unimplemented_types() {
    let files = Gen::dp_files("configured_feature");
    assert!(!files.is_empty(), "missing pinned configured features");
    let mut unparsed = Vec::new();
    let mut unimplemented = Vec::new();
    for (name, bytes) in files {
        let json = jparse(&bytes).unwrap_or_else(|| panic!("invalid JSON: {name}"));
        match super::p_cf(&json) {
            None => unparsed.push(name),
            Some(Cf::Unknown(_)) => unimplemented.push(name),
            Some(_) => {}
        }
    }
    unparsed.sort();
    assert!(
        unparsed.is_empty(),
        "unparsed configured features: {unparsed:?}"
    );
    unimplemented.sort();
    // The last gap here was `minecraft:bonus_chest`; every type the pack names now has a
    // dispatch arm, so this is the inventory of identities rather than of behaviour.
    assert!(
        unimplemented.is_empty(),
        "unimplemented configured features: {unimplemented:?}"
    );
}

#[test]
fn pinned_inline_features_and_modifiers_are_not_dropped() {
    fn walk(node: &J, path: &str, checked: &mut usize) {
        match node {
            J::Obj(fields) => {
                if js(node, "feature").is_some() && jarr(node, "placement").is_some() {
                    let pf = super::p_pf(node)
                        .unwrap_or_else(|| panic!("unparsed placed feature: {path}"));
                    assert_eq!(
                        pf.mods.len(),
                        jarr(node, "placement").unwrap().len(),
                        "lost modifier: {path}"
                    );
                    *checked += 1;
                }
                if js(node, "type").is_some() && js(node, "config").is_some() && path.contains('/')
                {
                    assert!(
                        !matches!(super::p_cf(node), None | Some(Cf::Unknown(_))),
                        "unparsed inline configured feature: {path}"
                    );
                    *checked += 1;
                }
                for (key, value) in fields {
                    walk(value, &format!("{path}/{key}"), checked);
                }
            }
            J::Arr(values) => {
                for (i, value) in values.iter().enumerate() {
                    walk(value, &format!("{path}/{i}"), checked);
                }
            }
            _ => {}
        }
    }
    let mut checked = 0;
    for kind in ["configured_feature", "placed_feature"] {
        for (name, bytes) in Gen::dp_files(kind) {
            let json = jparse(&bytes).unwrap_or_else(|| panic!("invalid JSON: {name}"));
            walk(&json, &name, &mut checked);
        }
    }
    assert!(
        checked > 0,
        "no inline features or placed features inspected"
    );
    println!("parsed {checked} inline configured / placed feature nodes without dropped modifiers");
}

#[test]
fn block_column_provider_accepts_native_and_legacy_keys() {
    for key in ["provider", "contents"] {
        let text = format!(
            r#"{{"type":"block_column","config":{{"layers":[{{"height":1,"{key}":{{"type":"simple_state_provider","state":{{"Name":"minecraft:stone"}}}}}}],"direction":"up"}}}}"#
        );
        assert!(
            super::p_cf(&jparse(text.as_bytes()).unwrap()).is_some(),
            "{key}"
        );
    }
}

#[test]
fn block_column_up_matches_native_cells_and_rng() {
    generic_feature_cases(
        include_str!("../tests/feature_cases/block_column_up.json"),
        include_str!("../tests/data/block_column_up_1211.tsv"),
    );
}

#[test]
fn block_column_down_matches_native_cells_and_rng() {
    generic_feature_cases(
        include_str!("../tests/feature_cases/block_column_down.json"),
        include_str!("../tests/data/block_column_down_1211.tsv"),
    );
}

#[test]
fn required_feature_children_cannot_silently_disappear() {
    for ty in ["simple_random_selector", "sequence"] {
        let json = jparse(format!(r#"{{"type":"{ty}","config":{{"features":[{{"feature":{{"type":"glowstone_blob","config":{{}}}},"placement":[]}},{{"feature":{{"type":"block_column","config":{{}}}},"placement":[]}}]}}}}"#).as_bytes()).unwrap();
        assert!(
            super::p_cf(&json).is_none(),
            "{ty} dropped an invalid child"
        );
    }
    let json = jparse(br#"{"feature":"minecraft:glowstone_blob","placement":[{"type":"minecraft:unsupported_modifier"}]}"#).unwrap();
    assert!(
        super::p_pf(&json).is_none(),
        "a dropped modifier changes placement and RNG"
    );
    for ty in ["all_of", "any_of"] {
        let json = jparse(format!(r#"{{"type":"{ty}","predicates":[{{"type":"true"}},{{"type":"unsupported_predicate"}}]}}"#).as_bytes()).unwrap();
        assert!(
            crate::generation_providers::p_pred(&json).is_none(),
            "{ty} dropped an invalid predicate"
        );
    }
}

#[test]
fn glowstone_matches_native_cells_and_rng() {
    let reg = breg().unwrap();
    let cf = super::p_cf(&jparse(br#"{"type":"minecraft:glowstone_blob","config":{}}"#).unwrap())
        .unwrap();
    let stone = reg.state_of("minecraft:stone", None);
    let netherrack = reg.state_of("minecraft:netherrack", None);
    let basalt = reg.state_of("minecraft:basalt", None);
    let blackstone = reg.state_of("minecraft:blackstone", None);
    for row in include_str!("../tests/data/glowstone_1211.tsv").lines() {
        let p: Vec<_> = row.split('\t').collect();
        let scene = p[0].parse::<i32>().unwrap();
        let seed = p[1].parse::<i64>().unwrap() as u64;
        let mut g = Gen::load(0, crate::name_to_state).unwrap();
        for x in -8i32..=8 {
            for z in -8i32..=8 {
                for dy in -12..=1 {
                    let state = if scene == 4 && (x, dy, z) == (0, 0, 0) {
                        stone
                    } else if dy == 1 && scene != 5 {
                        match scene {
                            1 => basalt,
                            2 => blackstone,
                            3 => stone,
                            _ => netherrack,
                        }
                    } else if scene == 6 && dy < 0 && (x + z).rem_euclid(3) == 0 {
                        netherrack
                    } else {
                        0
                    };
                    g.feat_write(x, 64 + dy, z, state);
                }
            }
        }
        let mut ctx = context(&mut g);
        let mut rng = Xor::seed_worldgen(seed);
        gen_cf(&cf, freg(), &mut ctx, &mut rng, (0, 64, 0));
        assert_eq!(!ctx.wr.is_empty(), p[2].parse::<bool>().unwrap(), "{row}");
        assert_eq!(ctx.wr.len(), p[3].parse::<usize>().unwrap(), "{row}");
        let mut cells = Vec::new();
        for (&(x, y, z), &state) in &ctx.wr {
            assert_eq!(reg.name_of(state), "minecraft:glowstone");
            cells.push(format!("{x},{},{z}\n", y - 64));
        }
        cells.sort();
        let digest: String = crate::generation_math::md5(cells.concat().as_bytes())
            .iter()
            .map(|v| format!("{v:02x}"))
            .collect();
        assert_eq!(digest, p[5], "{row}");
        assert_eq!(rng.next_i64(), p[4].parse::<i64>().unwrap(), "{row}");
    }
}

#[test]
fn lake_grid_anchor_is_down_only() {
    // `LakeFeature.place` anchors its 16x8x16 grid with `pos.relative(Direction.DOWN, 4)`
    // (`jd.c(4)`, verified against the pinned bytecode: `relative(ji.a, 4)` and `LakeFeature`'s
    // `jd.e()` is `below()`), so grid cell (x, y, z) is the world position
    // `pos.offset(x, y, z)` of `(pos.x, pos.y - 4, pos.z)`. The x and z of the origin are NOT
    // shifted. A `pos.offset(-8, -4, -8)` reading moved every lava lake eight blocks
    // diagonally: on seed 4242 chunk (5,-32) the port put its lake at x 73..88 / z -516..-499
    // where the native world has it at x 81..96 / z -508..-493.
    let reg = breg().unwrap();
    let cf = super::p_cf(
        &jparse(
            br#"{"type":"minecraft:lake","config":{
                 "fluid":{"type":"minecraft:simple_state_provider","state":{"Name":"minecraft:lava"}},
                 "barrier":{"type":"minecraft:simple_state_provider","state":{"Name":"minecraft:stone"}}
               }}"#,
        )
        .unwrap(),
    )
    .expect("lake configured feature");
    let stone = reg.state_of("minecraft:stone", None);
    let lava = reg.state_of("minecraft:lava", None);
    let cave_air = reg.state_of("minecraft:cave_air", None);
    let oy = 64i32;
    let mut g = Gen::load(0, crate::name_to_state).unwrap();
    // Solid slate under the anchor (grid rows 0..3 must read solid-or-fluid) and open air
    // above it (grid rows 4..7 must not read liquid), over the grid plus its one-cell shell.
    for x in -2i32..=17 {
        for z in -2i32..=17 {
            for y in (oy - 4)..oy {
                g.feat_write(x, y, z, stone);
            }
            for y in oy..(oy + 8) {
                g.feat_write(x, y, z, 0);
            }
        }
    }
    let mut ctx = context(&mut g);
    let mut rng = Xor::seed_worldgen(42);
    gen_cf(&cf, freg(), &mut ctx, &mut rng, (0, oy, 0));
    assert!(!ctx.wr.is_empty(), "the lake must place on this scene");
    let mut fluid = 0usize;
    let mut air = 0usize;
    for (&(x, y, z), &state) in &ctx.wr {
        let name = reg.name_of(state);
        assert!(
            (0..=15).contains(&x) && (0..=15).contains(&z),
            "lake cell ({x},{y},{z}) is outside the 16x16 grid anchored at the origin x/z"
        );
        assert!(
            (oy - 4..=oy + 3).contains(&y),
            "lake cell ({x},{y},{z}) is outside rows oy-4..=oy+3"
        );
        match name {
            "minecraft:lava" => {
                // `y >= 4` fills CAVE_AIR, `y < 4` fills the fluid; the grid row is y - (oy-4).
                assert!(y < oy, "fluid placed in a y>=4 grid row at ({x},{y},{z})");
                fluid += 1;
            }
            "minecraft:cave_air" => {
                assert!(y >= oy, "cave_air placed in a y<4 grid row at ({x},{y},{z})");
                assert_eq!(state, cave_air);
                air += 1;
            }
            "minecraft:stone" => {}
            other => panic!("unexpected lake state {other} at ({x},{y},{z})"),
        }
        let _ = lava;
    }
    assert!(fluid > 0, "a lake on a solid floor must fill fluid");
    assert!(air > 0, "a lake must carve cave_air above its fluid");
}

#[test]
fn java_hashset_order_matches_java_util_hashset_iteration() {
    // `VegetationPatchFeature` binds its `vegetation_chance` draws to surface points in
    // `HashSet` order. Both expected sequences below are *measured* on the pinned 1.21.1 jar
    // (`new HashSet<BlockPos>()`, added in the shown order, iterated), not derived from a
    // re-derivation of `HashMap`, so a refactor cannot quietly fall back to insertion order.
    //
    // 25 points: `Vec3i.hashCode() = (y + z * 31) * 31 + x`, bucket
    // `spread(h) & (capacity - 1)` with `spread(h) = h ^ (h >>> 16)`, capacity 64 for 25
    // elements, and a shared bin keeps insertion order (bins 0, 1 and 2 hold repeats here).
    let pts: Vec<(i32, i32, i32)> = (0..5).flat_map(|x| (0..5).map(move |z| (x, -2, z))).collect();
    let got = super::java_hashset_order(&pts);
    let want = vec![
        (0, -2, 1),
        (0, -2, 2),
        (1, -2, 1),
        (0, -2, 3),
        (1, -2, 2),
        (2, -2, 1),
        (0, -2, 4),
        (1, -2, 3),
        (2, -2, 2),
        (3, -2, 1),
        (1, -2, 4),
        (2, -2, 3),
        (3, -2, 2),
        (4, -2, 1),
        (2, -2, 4),
        (3, -2, 3),
        (4, -2, 2),
        (3, -2, 4),
        (4, -2, 3),
        (4, -2, 4),
        (4, -2, 0),
        (3, -2, 0),
        (2, -2, 0),
        (1, -2, 0),
        (0, -2, 0),
    ];
    assert_eq!(got, want);
    assert_ne!(got, pts, "hash order is not insertion order");

    // 197 points (a full ceiling-patch radius) exercises the capacity growth past 16/64:
    // Java keeps 197 elements in a 512-slot table, so the measured head and tail below pin
    // the growth rule as well as the bucket rule.
    let disc: Vec<(i32, i32, i32)> = (-8..=8)
        .flat_map(|dx| (-8..=8).map(move |dz| (dx, dz)))
        .filter(|(dx, dz)| dx * dx + dz * dz <= 64)
        .map(|(dx, dz)| (dx, -3, dz))
        .collect();
    assert_eq!(disc.len(), 197);
    let ordered = super::java_hashset_order(&disc);
    assert_eq!(
        &ordered[..8],
        &[
            (7, -3, -1),
            (6, -3, -1),
            (5, -3, -1),
            (4, -3, -1),
            (3, -3, -1),
            (2, -3, -1),
            (1, -3, -1),
            (0, -3, -1),
        ]
    );
    assert_eq!(
        &ordered[189..],
        &[
            (-7, -3, -2),
            (-3, -3, 7),
            (-2, -3, 7),
            (-1, -3, 7),
            (0, -3, 7),
            (1, -3, 7),
            (2, -3, 7),
            (3, -3, 7),
        ]
    );
    // An empty/small set stays in insertion order because every point lands in its own bin.
    assert_eq!(super::java_hashset_order(&[(7, 1, 9)]), vec![(7, 1, 9)]);
    assert!(super::java_hashset_order(&[]).is_empty());
}

#[test]
fn lake_above_the_bottom_guard() {
    // `LakeFeature.place` returns false when `pos.getY() <= level.getMinBuildHeight() + 4`.
    let cf = super::p_cf(
        &jparse(
            br#"{"type":"minecraft:lake","config":{
                 "fluid":{"type":"minecraft:simple_state_provider","state":{"Name":"minecraft:lava"}},
                 "barrier":{"type":"minecraft:simple_state_provider","state":{"Name":"minecraft:stone"}}
               }}"#,
        )
        .unwrap(),
    )
    .expect("lake configured feature");
    let reg = breg().unwrap();
    let stone = reg.state_of("minecraft:stone", None);
    let mut g = Gen::load(0, crate::name_to_state).unwrap();
    let min_y = g.min_y;
    for x in -2i32..=17 {
        for z in -2i32..=17 {
            for y in min_y..min_y + 16 {
                g.feat_write(x, y, z, stone);
            }
        }
    }
    let mut ctx = context(&mut g);
    let mut rng = Xor::seed_worldgen(7);
    gen_cf(&cf, freg(), &mut ctx, &mut rng, (0, min_y + 4, 0));
    assert!(
        ctx.wr.is_empty(),
        "a lake whose origin is at min_y + 4 must fail before writing"
    );
}

#[test]
fn within_manhattan_keeps_the_native_order() {
    // Measured with a probe on the pinned jar (2,1,3):
    // 0,0,0 -1,0,0 0,-1,0 0,0,1 0,0,-1 0,1,0 1,0,0 -2,0,0 -1,-1,0 -1,0,1 -1,0,-1 ...
    let order = super::within_manhattan(2, 1, 3);
    let first: Vec<_> = order.iter().take(11).copied().collect();
    assert_eq!(
        first,
        vec![
            (0, 0, 0),
            (-1, 0, 0),
            (0, -1, 0),
            (0, 0, 1),
            (0, 0, -1),
            (0, 1, 0),
            (1, 0, 0),
            (-2, 0, 0),
            (-1, -1, 0),
            (-1, 0, 1),
            (-1, 0, -1),
        ]
    );
    assert_eq!(order.len(), 5 * 3 * 7);
}

#[test]
fn netherrack_replace_blobs_matches_native_cells_and_rng() {
    let reg = breg().unwrap();
    let basalt = reg.state_of("minecraft:basalt", None);
    let netherrack = reg.state_of("minecraft:netherrack", None);
    let cf = super::p_cf(
        &jparse(
            br#"{"type":"minecraft:netherrack_replace_blobs","config":{"radius":{"type":"minecraft:uniform","min_inclusive":3,"max_inclusive":7},"state":{"Name":"minecraft:basalt","Properties":{"axis":"y"}},"target":{"Name":"minecraft:netherrack"}}}"#,
        )
        .unwrap(),
    )
    .unwrap();
    for row in include_str!("../tests/data/netherrack_replace_blobs_1211.tsv").lines() {
        let p: Vec<_> = row.split('\t').collect();
        let scene = p[0].parse::<i32>().unwrap();
        let oy = p[1].parse::<i32>().unwrap();
        let seed = p[2].parse::<i64>().unwrap() as u64;
        let mut g = Gen::load(0, crate::name_to_state).unwrap();
        for x in -16i32..=16 {
            for z in -16i32..=16 {
                for dy in -20i32..=20 {
                    let y = oy + dy;
                    let r2 = x * x + dy * dy + z * z;
                    let state = match scene {
                        0 => {
                            if dy <= -1 {
                                netherrack
                            } else {
                                0
                            }
                        }
                        1 => {
                            if r2 <= 9 || (dy <= -4 && dy >= -8 && x.abs() <= 4 && z.abs() <= 4) {
                                netherrack
                            } else {
                                0
                            }
                        }
                        2 => 0,
                        3 => {
                            if dy <= -1 {
                                if x.abs() <= 2 && z.abs() <= 2 && dy >= -3 {
                                    basalt
                                } else {
                                    netherrack
                                }
                            } else {
                                0
                            }
                        }
                        4 => {
                            if (1..=3).contains(&dy) {
                                netherrack
                            } else {
                                0
                            }
                        }
                        _ => {
                            if dy <= -1 && !(x.abs() <= 1 && z.abs() <= 1) {
                                netherrack
                            } else {
                                0
                            }
                        }
                    };
                    g.feat_write(x, y, z, state);
                }
            }
        }
        let mut ctx = context(&mut g);
        let mut rng = Xor::seed_worldgen(seed);
        gen_cf(&cf, freg(), &mut ctx, &mut rng, (0, oy, 0));
        assert_eq!(!ctx.wr.is_empty(), p[3].parse::<bool>().unwrap(), "{row}");
        assert_eq!(ctx.wr.len(), p[4].parse::<usize>().unwrap(), "{row}");
        assert_eq!(block_cells_digest(reg, &ctx.wr, (0, oy, 0)), p[5], "{row}");
    }
}

#[test]
fn end_gateway_matches_native_cells() {
    let reg = breg().unwrap();
    let stone = reg.state_of("minecraft:stone", None);
    let dirt = reg.state_of("minecraft:dirt", None);
    let cf = super::p_cf(
        &jparse(br#"{"type":"minecraft:end_gateway","config":{"exact":false}}"#).unwrap(),
    )
    .unwrap();
    for row in include_str!("../tests/data/end_gateway_1211.tsv").lines() {
        let p: Vec<_> = row.split('\t').collect();
        let scene = p[0].parse::<i32>().unwrap();
        let oy = p[1].parse::<i32>().unwrap();
        let mut g = Gen::load(0, crate::name_to_state).unwrap();
        for x in -3i32..=3 {
            for z in -3i32..=3 {
                for dy in -3i32..=3 {
                    let state = match scene {
                        0 => 0,
                        1 => stone,
                        2 => {
                            if dy > 0 {
                                stone
                            } else {
                                0
                            }
                        }
                        _ => {
                            if x.abs() == 1 || z.abs() == 1 {
                                dirt
                            } else {
                                0
                            }
                        }
                    };
                    g.feat_write(x, oy + dy, z, state);
                }
            }
        }
        let mut ctx = context(&mut g);
        let mut rng = Xor::seed_worldgen(p[2].parse::<i64>().unwrap() as u64);
        gen_cf(&cf, freg(), &mut ctx, &mut rng, (0, oy, 0));
        assert_eq!(p[3], "true", "{row}");
        assert_eq!(ctx.wr.len(), p[4].parse::<usize>().unwrap(), "{row}");
        assert_eq!(block_cells_digest(reg, &ctx.wr, (0, oy, 0)), p[5], "{row}");
    }
}

#[test]
fn huge_brown_mushroom_matches_native_cells_and_height() {
    let reg = breg().unwrap();
    let dirt = reg.state_of("minecraft:dirt", None);
    let mycelium = reg.state_of("minecraft:mycelium", None);
    let stone = reg.state_of("minecraft:stone", None);
    let leaves = reg.state_of("minecraft:oak_leaves", None);
    // The extended piston: its shape is the head and the base, so it occludes as a cube without
    // being a solid render. The default (retracted) piston is a plain full cube.
    let piston = reg.state_of(
        "minecraft:piston",
        Some(&jparse(br#"{"extended":"true"}"#).expect("props")),
    );
    let cf = super::p_cf(
        &jparse(
            br#"{"type":"minecraft:huge_brown_mushroom","config":{"cap_provider":{"type":"minecraft:simple_state_provider","state":{"Name":"minecraft:brown_mushroom_block","Properties":{"down":"false","east":"true","north":"true","south":"true","up":"true","west":"true"}}},"foliage_radius":3,"stem_provider":{"type":"minecraft:simple_state_provider","state":{"Name":"minecraft:mushroom_stem","Properties":{"down":"false","east":"true","north":"true","south":"true","up":"true","west":"true"}}}}}"#,
        )
        .unwrap(),
    )
    .unwrap();
    for row in include_str!("../tests/data/huge_brown_mushroom_1211.tsv").lines() {
        let p: Vec<_> = row.split('\t').collect();
        let scene = p[0].parse::<i32>().unwrap();
        let oy = p[1].parse::<i32>().unwrap();
        let mut g = Gen::load(0, crate::name_to_state).unwrap();
        // The native scene is an infinite ground below the origin plus (per scene) a
        // leaf/stone slab at dy == 5 or a stone ceiling at dy >= 8; everything else is
        // air. Write the whole box the feature can read, so no cell falls back to the
        // generated terrain and silently changes a support or a can-place test.
        for x in -4i32..=4 {
            for z in -4i32..=4 {
                for dy in -1i32..=14 {
                    let state = match scene {
                        0 => (dy == -1).then_some(dirt),
                        1 => (dy == -1).then_some(mycelium),
                        2 => (dy == -1).then_some(stone),
                        3 => {
                            if dy == -1 {
                                Some(dirt)
                            } else if dy == 5 && x.abs() <= 2 && z.abs() <= 2 {
                                Some(leaves)
                            } else {
                                None
                            }
                        }
                        4 => {
                            if dy == -1 {
                                Some(dirt)
                            } else if dy == 5 && x.abs() <= 2 && z.abs() <= 2 {
                                Some(stone)
                            } else {
                                None
                            }
                        }
                        _ => {
                            if dy == -1 {
                                Some(dirt)
                            } else if dy >= 8 {
                                Some(stone)
                            } else {
                                None
                            }
                        }
                    };
                    g.feat_write(x, oy + dy, z, state.unwrap_or(0));
                }
            }
        }
        let mut ctx = context(&mut g);
        let mut rng = Xor::seed_worldgen(p[2].parse::<i64>().unwrap() as u64);
        gen_cf(&cf, freg(), &mut ctx, &mut rng, (0, oy, 0));
        assert_eq!(!ctx.wr.is_empty(), p[3].parse::<bool>().unwrap(), "{row}");
        assert_eq!(ctx.wr.len(), p[4].parse::<usize>().unwrap(), "{row}");
        assert_eq!(
            block_state_cells_digest(reg, &ctx.wr, (0, oy, 0)),
            p[5],
            "{row}"
        );
    }
}

#[test]
fn huge_red_mushroom_matches_native_cells_and_layers() {
    let reg = breg().unwrap();
    let dirt = reg.state_of("minecraft:dirt", None);
    let mycelium = reg.state_of("minecraft:mycelium", None);
    let stone = reg.state_of("minecraft:stone", None);
    let leaves = reg.state_of("minecraft:oak_leaves", None);
    // The extended piston: its shape is the head and the base, so it occludes as a cube without
    // being a solid render. The default (retracted) piston is a plain full cube.
    let piston = reg.state_of(
        "minecraft:piston",
        Some(&jparse(br#"{"extended":"true"}"#).expect("props")),
    );
    // the pinned pack's own red config: foliage_radius 2, and a stem whose `up` is false
    let cf = super::p_cf(
        &jparse(
            br#"{"type":"minecraft:huge_red_mushroom","config":{"cap_provider":{"type":"minecraft:simple_state_provider","state":{"Name":"minecraft:red_mushroom_block","Properties":{"down":"false","east":"true","north":"true","south":"true","up":"true","west":"true"}}},"foliage_radius":2,"stem_provider":{"type":"minecraft:simple_state_provider","state":{"Name":"minecraft:mushroom_stem","Properties":{"down":"false","east":"true","north":"true","south":"true","up":"false","west":"true"}}}}}"#,
        )
        .unwrap(),
    )
    .unwrap();
    for row in include_str!("../tests/data/huge_red_mushroom_1211.tsv").lines() {
        let p: Vec<_> = row.split('\t').collect();
        let scene = p[0].parse::<i32>().unwrap();
        let oy = p[1].parse::<i32>().unwrap();
        let mut g = Gen::load(0, crate::name_to_state).unwrap();
        // Scene 4 is the species difference: an obstruction in the cap band that misses
        // the centre column. The red position check only inspects that column, so the
        // mushroom grows and the cap skips the solid cell; the brown envelope would
        // refuse the placement outright (0 cells).
        for x in -3i32..=3 {
            for z in -3i32..=3 {
                for dy in -1i32..=15 {
                    let state = match scene {
                        0 => (dy == -1).then_some(dirt),
                        1 => (dy == -1).then_some(mycelium),
                        2 => (dy == -1).then_some(stone),
                        3 => {
                            if dy == -1 {
                                Some(dirt)
                            } else if (4..=8).contains(&dy) && x.abs() <= 2 && z.abs() <= 2 {
                                Some(leaves)
                            } else {
                                None
                            }
                        }
                        4 => {
                            if dy == -1 {
                                Some(dirt)
                            } else if dy == 5 && x.abs() <= 1 && z.abs() <= 1 && (x != 0 || z != 0)
                            {
                                Some(stone)
                            } else {
                                None
                            }
                        }
                        _ => {
                            if dy == -1 {
                                Some(dirt)
                            } else if dy >= 9 {
                                Some(stone)
                            } else {
                                None
                            }
                        }
                    };
                    g.feat_write(x, oy + dy, z, state.unwrap_or(0));
                }
            }
        }
        let mut ctx = context(&mut g);
        let mut rng = Xor::seed_worldgen(p[2].parse::<i64>().unwrap() as u64);
        gen_cf(&cf, freg(), &mut ctx, &mut rng, (0, oy, 0));
        assert_eq!(!ctx.wr.is_empty(), p[3].parse::<bool>().unwrap(), "{row}");
        assert_eq!(ctx.wr.len(), p[4].parse::<usize>().unwrap(), "{row}");
        assert_eq!(
            block_state_cells_digest(reg, &ctx.wr, (0, oy, 0)),
            p[5],
            "{row}"
        );
    }
}

/// One inclusive `[min, max]` pair.
fn case_range_of(j: &J) -> [i32; 2] {
    match j {
        J::Arr(a) => {
            assert_eq!(a.len(), 2, "a range must be [min, max]");
            [a[0].num() as i32, a[1].num() as i32]
        }
        _ => panic!("a range must be [min, max]"),
    }
}

/// One inclusive `[min, max]` range from a cases file, by key.
fn case_range(j: &J, key: &str) -> [i32; 2] {
    case_range_of(js(j, key).unwrap_or_else(|| panic!("missing range {key}")))
}

/// Replay a native case set (`tools/structure-oracle/FeatureOracle.java`) through the port.
///
/// The cases file drives both sides: the oracle builds its scene from it, and this builds the
/// port's scene from the same JSON, so a feature type costs a cases file plus an arm in
/// `p_cf` instead of a second test body. Plies are origin-relative inclusive boxes written
/// over a fixed air box; a scene that needs to write outside that box fails loudly rather
/// than silently reading generated terrain. The last fixture column is the oracle's
/// post-placement `nextInt(1000)` triple, so a draw that changes no cell still fails.
fn generic_feature_cases(cases_json: &str, fixture: &str) {
    // The air box is the volume the scene is *defined* over: everything inside it that no ply
    // covers is written as air, so the port never reads generated terrain. A tall feature
    // (`huge_fungus` reaches dy 26) declares its own with `"box"`.
    const DEFAULT_BOX: [[i32; 2]; 3] = [[-8, 8], [-6, 6], [-8, 8]];
    let reg = breg().unwrap();
    let cases = jparse(cases_json.as_bytes()).unwrap();
    let air_box: [[i32; 2]; 3] = match jarr(&cases, "box") {
        Some(b) => {
            assert_eq!(b.len(), 3, "box must be [[x0,x1],[y0,y1],[z0,z1]]");
            [
                case_range_of(&b[0]),
                case_range_of(&b[1]),
                case_range_of(&b[2]),
            ]
        }
        None => DEFAULT_BOX,
    };
    let cf = super::p_cf(&cases).expect("the cases file is a configured feature");
    let mut scenes: Vec<(String, Vec<(u32, [[i32; 2]; 3])>)> = Vec::new();
    for scene in jarr(&cases, "scenes").expect("scenes") {
        let mut plies = Vec::new();
        for ply in jarr(scene, "plies").expect("plies") {
            let (name, props) = bs_parts(js(ply, "block").expect("block")).expect("block");
            plies.push((
                reg.state_of(&name, props),
                [
                    case_range(ply, "dx"),
                    case_range(ply, "dy"),
                    case_range(ply, "dz"),
                ],
            ));
        }
        scenes.push((jstr(scene, "name").expect("scene name"), plies));
    }
    for row in fixture.lines() {
        let p: Vec<_> = row.split('\t').collect();
        assert_eq!(p.len(), 7, "{row}");
        let oy = p[1].parse::<i32>().unwrap();
        let seed = p[2].parse::<i64>().unwrap() as u64;
        let plies = &scenes
            .iter()
            .find(|(name, _)| name == p[0])
            .unwrap_or_else(|| panic!("unknown scene in fixture: {row}"))
            .1;
        for (_, r) in plies {
            assert!(
                (0..3).all(|k| r[k][0] >= air_box[k][0] && r[k][1] <= air_box[k][1]),
                "scene {} writes outside the air box: {row}",
                p[0]
            );
        }
        let mut g = Gen::load(0, crate::name_to_state).unwrap();
        for x in air_box[0][0]..=air_box[0][1] {
            for z in air_box[2][0]..=air_box[2][1] {
                for dy in air_box[1][0]..=air_box[1][1] {
                    g.feat_write(x, oy + dy, z, 0);
                }
            }
        }
        for (state, r) in plies {
            for x in r[0][0]..=r[0][1] {
                for dy in r[1][0]..=r[1][1] {
                    for z in r[2][0]..=r[2][1] {
                        g.feat_write(x, oy + dy, z, *state);
                    }
                }
            }
        }
        let mut ctx = context(&mut g);
        let mut rng = Xor::seed_worldgen(seed);
        gen_cf(&cf, freg(), &mut ctx, &mut rng, (0, oy, 0));
        // `GENERIC_CASE_DUMP=<path>` writes this side's cells in the oracle's `--cells`
        // format, so a failing digest can be diffed cell by cell instead of guessed at.
        if let Ok(path) = std::env::var("GENERIC_CASE_DUMP") {
            let mut rows: Vec<String> = ctx
                .wr
                .iter()
                .map(|(&(x, y, z), &st)| {
                    format!(
                        "{}\t{}\t{}\t{},{},{} {}",
                        p[0],
                        oy,
                        p[2], // the fixture's own signed spelling of the seed
                        x,
                        y - oy,
                        z,
                        block_state_cells_text(reg, st)
                    )
                })
                .collect();
            rows.sort();
            let mut out = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
                .expect("GENERIC_CASE_DUMP path");
            use std::io::Write;
            for r in rows {
                writeln!(out, "{r}").unwrap();
            }
        }
        let probe = format!(
            "{},{},{}",
            rng.next_bound(1000),
            rng.next_bound(1000),
            rng.next_bound(1000)
        );
        assert_eq!(!ctx.wr.is_empty(), p[3].parse::<bool>().unwrap(), "{row}");
        assert_eq!(ctx.wr.len(), p[4].parse::<usize>().unwrap(), "{row}");
        assert_eq!(
            block_state_cells_digest(reg, &ctx.wr, (0, oy, 0)),
            p[5],
            "{row}"
        );
        assert_eq!(probe, p[6], "{row}");
    }
}

#[test]
fn pointed_dripstone_matches_native_cells_and_rng() {
    generic_feature_cases(
        include_str!("../tests/feature_cases/pointed_dripstone.json"),
        include_str!("../tests/data/pointed_dripstone_1211.tsv"),
    );
}

#[test]
fn coral_tree_matches_native_cells_and_rng() {
    generic_feature_cases(
        include_str!("../tests/feature_cases/coral_tree.json"),
        include_str!("../tests/data/coral_tree_1211.tsv"),
    );
}

#[test]
fn coral_claw_matches_native_cells_and_rng() {
    generic_feature_cases(
        include_str!("../tests/feature_cases/coral_claw.json"),
        include_str!("../tests/data/coral_claw_1211.tsv"),
    );
}

#[test]
fn coral_mushroom_matches_native_cells_and_rng() {
    generic_feature_cases(
        include_str!("../tests/feature_cases/coral_mushroom.json"),
        include_str!("../tests/data/coral_mushroom_1211.tsv"),
    );
}

#[test]
fn huge_fungus_matches_native_cells_and_rng() {
    generic_feature_cases(
        include_str!("../tests/feature_cases/huge_fungus.json"),
        include_str!("../tests/data/huge_fungus_1211.tsv"),
    );
}

#[test]
fn huge_fungus_crimson_matches_native_cells_and_rng() {
    generic_feature_cases(
        include_str!("../tests/feature_cases/huge_fungus_crimson.json"),
        include_str!("../tests/data/huge_fungus_crimson_1211.tsv"),
    );
}

#[test]
fn basalt_columns_matches_native_cells_and_rng() {
    generic_feature_cases(
        include_str!("../tests/feature_cases/basalt_columns.json"),
        include_str!("../tests/data/basalt_columns_1211.tsv"),
    );
}

/// The sculk cursor trace of chunk (-16,112) against the native slice captured with
/// `tools/carve-agent` (`tests/data/sculk_cursor_1211.tsv`).
///
/// This is the loop the sculk work uses: `cargo test sculk_cursor` generates the chunk
/// in-process and compares cursor trajectories by (round, index) - no CLI dump, no fixture
/// server. The assertion requires every row of the pinned native slice, not every cursor
/// in the whole chunk. The sink is thread-local; matching this bounded slice is not a
/// full-cell or whole-patch parity claim.
#[test]
fn sculk_catalyst_requires_a_full_collision_block_below() {
    let reg = breg().unwrap();
    // Keep collision fullness distinct from render solidity and one sturdy face.
    let air = reg.state_of("minecraft:air", None);
    let stone = reg.state_of("minecraft:stone", None);
    let bottom_props = jparse(br#"{"type":"bottom"}"#).unwrap();
    let double_props = jparse(br#"{"type":"double"}"#).unwrap();
    let bottom_slab = reg.state_of("minecraft:oak_slab", Some(&bottom_props));
    let double_slab = reg.state_of("minecraft:oak_slab", Some(&double_props));
    assert!(!reg.is_collision_shape_full_block(air));
    assert!(reg.is_collision_shape_full_block(stone));
    assert!(!reg.is_collision_shape_full_block(bottom_slab));
    assert!(reg.is_collision_shape_full_block(double_slab));
    // Zero spread/growth rounds are legal in eez's codec. Isolate the terminal
    // placement gate observed at (-236,-51,1792), where the below state was air.
    let cf = super::p_cf(
        &jparse(
            br#"{"type":"minecraft:sculk_patch","config":{
        "charge_count":1,"amount_per_charge":1,"spread_attempts":1,
        "growth_rounds":0,"spread_rounds":0,"extra_rare_growths":0,"catalyst_chance":1.0
    }}"#,
        )
        .unwrap(),
    )
    .unwrap();
    let origin = (0, 120, 0);
    for (below, expected) in [
        ("minecraft:stone", "minecraft:sculk_catalyst"),
        ("minecraft:air", "minecraft:sculk"),
    ] {
        let mut g = Gen::load(0, crate::name_to_state).expect("data pack");
        let mut ctx = context(&mut g);
        ctx.set(0, 119, 0, reg.state_of(below, None));
        ctx.set(0, 120, 0, reg.state_of("minecraft:sculk", None));
        gen_cf(&cf, freg(), &mut ctx, &mut Xor::seed_worldgen(0), origin);
        assert_eq!(reg.name_of(ctx.get(0, 120, 0)), expected, "support={below}");
    }
}

#[test]
fn sculk_cursor_matches_the_native_trace() {
    // The sink is thread-local and the generator lock keeps the other generator tests out, so
    // this trace is this test's alone (a process-wide sink interleaved a concurrent sculk test's
    // cursors: 5310 lines, no match).
    let _gen = crate::worldgen::lock_gen_for_tests();
    super::spc_trace_begin();
    let mut g = Gen::load(0, crate::name_to_state).expect("datapack");
    // `feat_state` populates the chunk that contains (x, z), and a chunk's population is what
    // arms the patch whose origin may sit in a *neighbouring* chunk - which is why the native
    // slice's first cursor is at (-261,-42,1785), in chunk (-17,112), while the population that
    // produced it is chunk (-16,112)'s. Generating any other chunk gives a different scene (and
    // 5798 lines instead of 5150, measured).
    let _ = g.feat_state(-256, 64, 1792, super::populate);
    let lines = super::spc_trace_take();
    // Only the cursor lines take part in this comparison; the `SPATCH` markers the port now emits
    // (same format as the reference agent's, `SculkLog.patch`) split the trace by patch and are
    // what a patch-scoped comparison would use - see the spec page for why that is not done yet.
    let lines: Vec<String> = lines
        .into_iter()
        .filter(|l| l.starts_with("SCURSOR"))
        .collect();
    let native: Vec<String> = include_str!("../tests/data/sculk_cursor_1211.tsv")
        .lines()
        .map(|l| l.to_string())
        .collect();
    let matched = lines
        .iter()
        .zip(native.iter())
        .take_while(|(a, b)| a == b)
        .count();
    eprintln!(
        "sculk cursor trace: {} lines, {matched} of {} native lines match",
        lines.len(),
        native.len()
    );
    if matched < native.len() {
        eprintln!(
            "first divergence at line {matched}: port {:?} native {:?}",
            lines.get(matched),
            native.get(matched)
        );
    }
    assert_eq!(
        matched,
        native.len(),
        "sculk cursor trace diverged within the pinned native slice"
    );
}

/// The same chunk's cursor trace, captured for the record: `SPC_CURSOR_TRACE=1` on the CLI dump
/// must produce the same first lines as the in-process test above.
#[test]
fn sculk_cursor_trace_is_the_same_shape_as_the_agents() {
    let first = include_str!("../tests/data/sculk_cursor_1211.tsv")
        .lines()
        .next()
        .unwrap();
    assert!(first.starts_with("SCURSOR r=0 i=0 "), "{first}");
    assert_eq!(
        first.split_whitespace().count(),
        10,
        "SCURSOR, r, i, x, y, z, charge, decay, update, mask"
    );
}

/// The 18 neighbours' *order* is the draw: `findNextSculk` shuffles this list, so a different
/// base order is a different permutation for the same random stream. Measured through the
/// pinned jar: `BlockPos.betweenClosed(-1,-1,-1, 1,1,1)` yields `(-1,-1,-1) (0,-1,-1)
/// (1,-1,-1) (-1,0,-1) ...` - x varies fastest, then y, then z - and the centre is dropped.
/// (The port iterated x outermost, which is what made its first divergent cursor pick a
/// different neighbour while every earlier write matched.)
#[test]
fn sculk_offsets_keep_vanillas_iteration_order() {
    let offsets = super::spc_offsets();
    assert_eq!(offsets.len(), 18, "18 face/edge neighbours");
    // the first five and the last, copied from the probe's `KEPT` line
    assert_eq!(offsets[0], (0, -1, -1));
    assert_eq!(offsets[1], (-1, 0, -1));
    assert_eq!(offsets[2], (0, 0, -1));
    assert_eq!(offsets[3], (1, 0, -1));
    assert_eq!(offsets[4], (0, 1, -1));
    assert_eq!(*offsets.last().unwrap(), (0, 1, 1));
    // a corner (three non-zero coordinates) is not a neighbour
    assert!(!offsets.contains(&(1, -1, -1)));
    assert!(
        !offsets.contains(&(0, 0, 0)),
        "the centre is not a neighbour"
    );
}

/// `SculkBehaviour.decayDelay` (`j_`): `SculkBlock` and `SculkVeinBlock` inherit the interface
/// default (1); `max(x - 1, 0)` belongs to the anonymous default implementation in `dmp`.
/// Reading it the other way round made every cursor standing on a vein decay.
#[test]
fn sculk_decay_delay_follows_the_behaviour() {
    use super::{spc_decay_delay, SpcKind};
    assert_eq!(
        spc_decay_delay(SpcKind::Vein, 1),
        1,
        "the vein inherits the default"
    );
    assert_eq!(spc_decay_delay(SpcKind::Sculk, 0), 1, "and so does sculk");
    assert_eq!(
        spc_decay_delay(SpcKind::Other, 1),
        0,
        "only the default behaviour decrements"
    );
    assert_eq!(
        spc_decay_delay(SpcKind::Other, 0),
        0,
        "and it clamps at zero"
    );
}

/// Blocks.qQ is the sensor, not sculk: pin both growth outcomes and their draw count.
#[test]
fn sculk_growth_is_sensor_or_shrieker_not_sculk() {
    let reg = breg().unwrap();
    let mut g = Gen::load(0, crate::name_to_state).unwrap();
    g.feat_write(0, 80, 0, 0);
    let mut ctx = context(&mut g);
    let mut rng = Xor::seed_worldgen(0);
    let mut expected_rng = Xor::seed_worldgen(0);
    for _ in 0..64 {
        let rare = expected_rng.next_bound(11) == 0;
        let st = super::spc_growth_state(&mut ctx, reg, &mut rng, (0, 80, 0), true);
        assert_eq!(
            reg.name_of(st),
            if rare {
                "minecraft:sculk_shrieker"
            } else {
                "minecraft:sculk_sensor"
            }
        );
    }
    assert_eq!(rng.next_bound(1000), expected_rng.next_bound(1000));
}

#[test]
fn sculk_growth_density_counts_sensors_not_sculk_floor() {
    let reg = breg().unwrap();
    let mut g = Gen::load(0, crate::name_to_state).unwrap();
    for x in -4..=4 {
        for y in 80..=83 {
            for z in -4..=4 {
                g.feat_write(x, y, z, 0);
            }
        }
    }
    let sculk = reg.state_of("minecraft:sculk", None);
    for x in -4..=4 {
        g.feat_write(x, 80, 0, sculk);
    }
    let mut ctx = context(&mut g);
    assert!(
        super::spc_can_place_growth(&mut ctx, reg, (0, 80, 0)),
        "sculk floor is not growth density"
    );
    let sensor = reg.state_of("minecraft:sculk_sensor", None);
    for x in 1..=3 {
        ctx.set(x, 81, 0, sensor);
    }
    assert!(
        !super::spc_can_place_growth(&mut ctx, reg, (0, 80, 0)),
        "three nearby sensors suppress new growth"
    );
}

#[test]
fn sculk_regrow_restores_supported_remembered_faces() {
    let reg = breg().unwrap();
    for waterlogged in [false, true] {
        let mut g = Gen::load(0, crate::name_to_state).unwrap();
        for x in -1..=1 {
            for y in 79..=81 {
                for z in -1..=1 {
                    g.feat_write(x, y, z, 0);
                }
            }
        }
        let deepslate = reg.state_of("minecraft:deepslate", None);
        g.feat_write(0, 79, 0, deepslate);
        if waterlogged {
            g.feat_write(0, 80, 0, reg.state_of("minecraft:water", None));
        }
        let mut ctx = context(&mut g);
        let mut cursor = super::SpcCursor::new((0, 80, 0), 10);
        cursor.facings = Some(0b000011); // remembered down + up; only down is supported
        super::spc_trace_begin();
        let grew = super::spc_attempt_spread_vein(&mut ctx, reg, &cursor);
        let trace = super::spc_trace_take();
        assert!(grew);
        let st = ctx.get(0, 80, 0);
        assert_eq!(reg.name_of(st), "minecraft:sculk_vein");
        assert_eq!(super::spc_face_mask(reg, st), 1);
        assert_eq!(
            reg.get_prop(st, "waterlogged").as_deref(),
            Some(if waterlogged { "true" } else { "false" })
        );
        assert_eq!(
            trace.iter().filter(|s| *s == "SCMARK attempt-vein").count(),
            1
        );
    }
}

/// MultifaceSpreader spreads FROM an existing face TOWARD an absent face.
#[test]
fn sculk_vein_spreads_from_its_existing_face() {
    let reg = breg().unwrap();
    let mut g = Gen::load(0, crate::name_to_state).unwrap();
    let stone = reg.state_of("minecraft:deepslate", None);
    for x in -2..=2 {
        for z in -2..=2 {
            for y in 78..=82 {
                g.feat_write(x, y, z, if y == 79 { stone } else { 0 });
            }
        }
    }
    let vein = reg.with_prop(reg.state_of("minecraft:sculk_vein", None), "down", "true");
    g.feat_write(0, 80, 0, vein);
    let mut ctx = context(&mut g);
    let cursor = super::SpcCursor::new((0, 80, 0), 10);
    assert!(super::spc_attempt_spread_vein(&mut ctx, reg, &cursor));
    for (x, z) in [(0, -1), (0, 1), (-1, 0), (1, 0)] {
        let state = ctx.get(x, 80, z);
        assert_eq!(
            reg.name_of(state),
            "minecraft:sculk_vein",
            "neighbour {x},{z}"
        );
        assert_eq!(
            super::spc_face_mask(reg, state),
            1,
            "floor-facing neighbour {x},{z}"
        );
    }
}

/// Vein inherits SculkBehaviour.canChangeBlockStateOnSpread=true; sculk alone overrides it.
#[test]
fn sculk_vein_cursor_remembers_faces_added_by_spread() {
    let reg = breg().unwrap();
    let mut g = Gen::load(0, crate::name_to_state).unwrap();
    // Bedrock supports faces but is not sculk-replaceable. The sealed cage also
    // keeps findNextSculk from moving away and masking the current-cell reread.
    let bedrock = reg.state_of("minecraft:bedrock", None);
    for x in -1..=1 {
        for y in 79..=81 {
            for z in -1..=1 {
                g.feat_write(x, y, z, bedrock);
            }
        }
    }
    let vein = reg.with_prop(reg.state_of("minecraft:sculk_vein", None), "down", "true");
    g.feat_write(0, 80, 0, vein);
    let mut ctx = context(&mut g);
    let mut cursor = super::SpcCursor::new((0, 80, 0), 32);
    let mut rng = Xor::seed_worldgen(0);
    super::spc_cursor_update(&mut ctx, &mut rng, &mut cursor, (0, 80, 0), true);
    // The original down face spreads to four perpendicular faces, not up.
    assert_eq!(super::spc_face_mask(reg, ctx.get(0, 80, 0)), 0b111101);
    assert_eq!(cursor.pos, (0, 80, 0));
    assert_eq!(
        cursor.facings,
        Some(0b111101),
        "cursor kept pre-spread faces"
    );
}

/// The native caller passes the state captured after spread but before conversion.
/// Conversion can spread a new north face back onto the source, which discharge
/// must not retain just because it appeared in the live world in the meantime.
#[test]
fn sculk_cursor_discharges_the_captured_state_after_conversion() {
    let reg = breg().unwrap();
    for initial_charge in [1, 2] {
        let mut g = Gen::load(0, crate::name_to_state).unwrap();
        for x in -2..=2 {
            for y in 78..=82 {
                for z in -2..=2 {
                    g.feat_write(x, y, z, 0);
                }
            }
        }
        let bedrock = reg.state_of("minecraft:bedrock", None);
        g.feat_write(0, 80, -1, bedrock);
        g.feat_write(0, 80, 1, bedrock);
        g.feat_write(0, 79, 0, reg.state_of("minecraft:deepslate", None));
        let vein = reg.with_prop(reg.state_of("minecraft:sculk_vein", None), "south", "true");
        g.feat_write(0, 80, 0, vein);
        let mut ctx = context(&mut g);
        let mut cursor = super::SpcCursor::new((0, 80, 0), initial_charge);
        let mut rng = Xor::seed_worldgen(0);
        super::spc_cursor_update(&mut ctx, &mut rng, &mut cursor, (0, 80, 0), true);
        assert_eq!(
            cursor.charge,
            initial_charge - 1,
            "conversion uses one charge"
        );
        if initial_charge == 2 {
            assert_ne!(cursor.pos, (0, 80, 0), "exercise discharge before moving");
        }
        assert_eq!(reg.name_of(ctx.get(0, 79, 0)), "minecraft:sculk");
        assert_eq!(
            super::spc_face_mask(reg, ctx.get(0, 80, 0)),
            8,
            "discharge must use captured down+south, not live down+north+south"
        );
    }
}

#[test]
fn sculk_discharge_matches_native_state_snapshots() {
    let reg = breg().unwrap();
    let vein = |mask: u8| {
        let mut st = reg.state_of("minecraft:sculk_vein", None);
        for d in 0..6 {
            st = reg.with_prop(
                st,
                super::multiface_property(d),
                if mask & (1 << d) == 0 {
                    "false"
                } else {
                    "true"
                },
            );
        }
        st
    };
    for row in include_str!("../tests/data/sculk_discharge_1211.tsv").lines() {
        let p: Vec<_> = row.split('\t').collect();
        assert_eq!(p.len(), 5, "{row}");
        let captured = vein(p[0].parse().unwrap());
        let live = vein(p[1].parse().unwrap());
        let mut g = Gen::load(0, crate::name_to_state).unwrap();
        for x in -1..=1 {
            for y in 79..=81 {
                for z in -1..=1 {
                    g.feat_write(x, y, z, 0);
                }
            }
        }
        g.feat_write(0, 80, 0, live);
        g.feat_write(
            0,
            79,
            0,
            reg.state_of(
                if p[2] == "1" {
                    "minecraft:sculk"
                } else {
                    "minecraft:bedrock"
                },
                None,
            ),
        );
        let mut ctx = context(&mut g);
        super::spc_vein_decay(&mut ctx, reg, (0, 80, 0), captured);
        assert_eq!(
            block_state_cells_text(reg, ctx.get(0, 80, 0)),
            p[3],
            "{row}"
        );
        assert_eq!(
            Xor::seed_worldgen(0).next_bound(1000).to_string(),
            p[4],
            "{row}"
        );
    }
}

/// availableFaces(sculk) is an empty set, distinct from an uninitialized cursor's null.
#[test]
fn sculk_cursor_keeps_empty_faces_distinct_from_unknown() {
    let reg = breg().unwrap();
    let mut g = Gen::load(0, crate::name_to_state).unwrap();
    let bedrock = reg.state_of("minecraft:bedrock", None);
    for x in -1..=1 {
        for y in 79..=81 {
            for z in -1..=1 {
                g.feat_write(x, y, z, bedrock);
            }
        }
    }
    g.feat_write(0, 80, 0, reg.state_of("minecraft:sculk", None));
    let mut ctx = context(&mut g);
    let mut cursor = super::SpcCursor::new((0, 80, 0), 32);
    assert_eq!(cursor.facings, None);
    let mut rng = Xor::seed_worldgen(0);
    super::spc_cursor_update(&mut ctx, &mut rng, &mut cursor, (0, 80, 0), true);
    assert_eq!(cursor.pos, (0, 80, 0));
    assert!(cursor.charge > 0);
    assert_eq!(cursor.facings, Some(0), "known empty faces became unknown");
}

/// A wrap-around candidate's blocking corner is relative to its source, not its target.
#[test]
fn sculk_vein_wrap_checks_the_source_corner() {
    let reg = breg().unwrap();
    let stone = reg.state_of("minecraft:deepslate", None);
    for source_corner in [false, true] {
        for target_corner in [false, true] {
            let mut g = Gen::load(0, crate::name_to_state).unwrap();
            for x in -1..=3 {
                for y in 78..=81 {
                    for z in -1..=1 {
                        g.feat_write(x, y, z, 0);
                    }
                }
            }
            g.feat_write(0, 79, 0, stone); // support west of the candidate
            g.feat_write(1, 80, 0, if source_corner { stone } else { 0 });
            g.feat_write(2, 79, 0, if target_corner { stone } else { 0 });
            let mut ctx = context(&mut g);
            assert_eq!(
                super::spc_can_spread(&mut ctx, reg, (0, 80, 0), (1, 79, 0), 4),
                !source_corner,
                "source corner={source_corner}, unrelated target-relative cell={target_corner}"
            );
        }
    }
}

#[test]
fn sculk_charge_decay_matches_native() {
    // Captured by invoking the pinned jar's private getDecayPenalty itself.
    // Both the numerator and denominator use Mth.square, with different overloads.
    let fixture = include_str!("../tests/data/sculk_decay_1211.tsv");
    assert!(!fixture.is_empty());
    for row in fixture.lines() {
        let fields: Vec<i32> = row.split('\t').map(|s| s.parse().unwrap()).collect();
        let [x, y, z, charge, expected]: [i32; 5] = fields.try_into().unwrap();
        assert_eq!(
            super::spc_decrease_charge((x, y, z), (0, 0, 0), charge),
            expected,
            "native decay at ({x},{y},{z}), charge {charge}"
        );
    }
}

/// A sculk patch on a synthetic deepslate floor must convert its neighbourhood and leave both
/// sculk and veins behind. Not a parity claim - the native fixture for that is the capture in
/// `specs/worldgen-parity/sculk-patch.md` - but a net under the spread machinery, which the
/// trace work showed can be wrong in ways no cell total notices.
#[test]
fn sculk_patch_converts_a_deepslate_floor() {
    use crate::generation_providers::IP;
    let reg = breg().unwrap();
    let deepslate = reg.state_of("minecraft:deepslate", None);
    let cf = Cf::SculkPatch(super::SculkPatchCfg {
        charge_count: 10,
        amount_per_charge: 32,
        spread_attempts: 16,
        growth_rounds: 0,
        spread_rounds: 1,
        extra_rare_growths: IP::Const(0),
        catalyst_chance: 0.0,
    });
    let mut g = Gen::load(0, crate::name_to_state).unwrap();
    for x in -6i32..=6 {
        for z in -6i32..=6 {
            for dy in -3i32..=3 {
                // a floor with a cave above it: the origin is air over the floor
                g.feat_write(x, 64 + dy, z, if dy <= -1 { deepslate } else { 0 });
            }
        }
    }
    let mut ctx = context(&mut g);
    let mut rng = Xor::seed_worldgen(0);
    gen_cf(&cf, freg(), &mut ctx, &mut rng, (0, 64, 0));
    let names: Vec<&str> = ctx
        .wr
        .values()
        .map(|&st| reg.name_of(st))
        .filter(|n| *n != "minecraft:deepslate" && *n != "minecraft:air")
        .collect();
    assert!(
        names.iter().any(|n| *n == "minecraft:sculk"),
        "no sculk written: {names:?}"
    );
    assert!(
        names.iter().any(|n| *n == "minecraft:sculk_vein"),
        "no vein written: {names:?}"
    );
}

#[test]
fn twisting_vines_matches_native_cells_and_rng() {
    let reg = breg().unwrap();
    let netherrack = reg.state_of("minecraft:netherrack", None);
    let warped_nylium = reg.state_of("minecraft:warped_nylium", None);
    let soul_sand = reg.state_of("minecraft:soul_sand", None);
    let cf = super::p_cf(
        &jparse(
            br#"{"type":"minecraft:twisting_vines","config":{"spread_width":8,"spread_height":4,"max_height":8}}"#,
        )
        .unwrap(),
    )
    .unwrap();
    for row in include_str!("../tests/data/twisting_vines_1211.tsv").lines() {
        let p: Vec<_> = row.split('\t').collect();
        let scene = p[0].parse::<i32>().unwrap();
        let seed = p[1].parse::<i64>().unwrap() as u64;
        let mut g = Gen::load(0, crate::name_to_state).unwrap();
        for x in -16i32..=16 {
            for z in -16i32..=16 {
                for dy in -12i32..=12 {
                    let state = match scene {
                        0 => {
                            if dy <= -1 {
                                netherrack
                            } else {
                                0
                            }
                        }
                        1 => {
                            if dy <= -1 || dy >= 3 {
                                netherrack
                            } else {
                                0
                            }
                        }
                        2 => {
                            if dy <= 0 {
                                netherrack
                            } else {
                                0
                            }
                        }
                        3 => {
                            if dy <= -1 {
                                warped_nylium
                            } else {
                                0
                            }
                        }
                        4 => {
                            if dy <= -1 {
                                soul_sand
                            } else {
                                0
                            }
                        }
                        _ => {
                            if dy <= -1 - ((x + z).rem_euclid(3)) {
                                netherrack
                            } else {
                                0
                            }
                        }
                    };
                    g.feat_write(x, 64 + dy, z, state);
                }
            }
        }
        let mut ctx = context(&mut g);
        let mut rng = Xor::seed_worldgen(seed);
        gen_cf(&cf, freg(), &mut ctx, &mut rng, (0, 64, 0));
        assert_eq!(!ctx.wr.is_empty(), p[2].parse::<bool>().unwrap(), "{row}");
        assert_eq!(ctx.wr.len(), p[3].parse::<usize>().unwrap(), "{row}");
        assert_eq!(block_cells_digest(reg, &ctx.wr, (0, 64, 0)), p[4], "{row}");
    }
}

#[test]
fn weeping_vines_matches_native_cells_and_rng() {
    let reg = breg().unwrap();
    let netherrack = reg.state_of("minecraft:netherrack", None);
    let wart = reg.state_of("minecraft:nether_wart_block", None);
    let cf = super::p_cf(&jparse(br#"{"type":"minecraft:weeping_vines","config":{}}"#).unwrap())
        .unwrap();
    for row in include_str!("../tests/data/weeping_vines_1211.tsv").lines() {
        let p: Vec<_> = row.split('\t').collect();
        let scene = p[0].parse::<i32>().unwrap();
        let seed = p[1].parse::<i64>().unwrap() as u64;
        let mut g = Gen::load(0, crate::name_to_state).unwrap();
        for x in -16i32..=16 {
            for z in -16i32..=16 {
                for dy in -24i32..=12 {
                    let state = match scene {
                        0 => {
                            if dy >= 1 {
                                netherrack
                            } else {
                                0
                            }
                        }
                        1 => 0,
                        2 => {
                            if dy >= 1 {
                                wart
                            } else {
                                0
                            }
                        }
                        3 => {
                            if dy >= 0 {
                                netherrack
                            } else {
                                0
                            }
                        }
                        4 => {
                            if dy >= 1 || dy <= -12 {
                                netherrack
                            } else {
                                0
                            }
                        }
                        _ => {
                            if dy >= 1 || dy <= -2 {
                                netherrack
                            } else {
                                0
                            }
                        }
                    };
                    g.feat_write(x, 64 + dy, z, state);
                }
            }
        }
        let mut ctx = context(&mut g);
        ctx.test_no_terrain = true;
        let mut rng = Xor::seed_worldgen(seed);
        gen_cf(&cf, freg(), &mut ctx, &mut rng, (0, 64, 0));
        assert_eq!(!ctx.wr.is_empty(), p[2].parse::<bool>().unwrap(), "{row}");
        assert_eq!(ctx.wr.len(), p[3].parse::<usize>().unwrap(), "{row}");
        assert_eq!(block_cells_digest(reg, &ctx.wr, (0, 64, 0)), p[4], "{row}");
    }
}

#[test]
fn delta_feature_matches_native_cells_and_rng() {
    let reg = breg().unwrap();
    let netherrack = reg.state_of("minecraft:netherrack", None);
    let lava = reg.state_of("minecraft:lava", None);
    let bedrock = reg.state_of("minecraft:bedrock", None);
    let bricks = reg.state_of("minecraft:nether_bricks", None);
    let cf = super::p_cf(
        &jparse(
            br#"{"type":"minecraft:delta_feature","config":{"contents":{"Name":"minecraft:lava","Properties":{"level":"0"}},"rim":{"Name":"minecraft:magma_block"},"rim_size":{"type":"minecraft:uniform","min_inclusive":0,"max_inclusive":2},"size":{"type":"minecraft:uniform","min_inclusive":3,"max_inclusive":7}}}"#,
        )
        .unwrap(),
    )
    .unwrap();
    for row in include_str!("../tests/data/delta_feature_1211.tsv").lines() {
        let p: Vec<_> = row.split('\t').collect();
        let scene = p[0].parse::<i32>().unwrap();
        let seed = p[1].parse::<i64>().unwrap() as u64;
        let mut g = Gen::load(0, crate::name_to_state).unwrap();
        for x in -16i32..=16 {
            for z in -16i32..=16 {
                for dy in -6i32..=6 {
                    let hole = x.rem_euclid(3) == 0
                        && z.rem_euclid(3) == 0
                        && x.abs() <= 9
                        && z.abs() <= 9;
                    let state = match scene {
                        0 => {
                            if hole && (dy == 0 || dy == 1) {
                                0
                            } else {
                                netherrack
                            }
                        }
                        1 => netherrack,
                        2 => {
                            if hole && (dy == 0 || dy == 1) {
                                if dy == 0 {
                                    lava
                                } else {
                                    0
                                }
                            } else {
                                netherrack
                            }
                        }
                        3 => {
                            if hole && dy == 0 {
                                0
                            } else {
                                netherrack
                            }
                        }
                        4 => {
                            if hole && dy == 0 {
                                bedrock
                            } else {
                                netherrack
                            }
                        }
                        _ => {
                            if hole && dy == 0 {
                                bricks
                            } else {
                                netherrack
                            }
                        }
                    };
                    g.feat_write(x, 64 + dy, z, state);
                }
            }
        }
        let mut ctx = context(&mut g);
        let mut rng = Xor::seed_worldgen(seed);
        gen_cf(&cf, freg(), &mut ctx, &mut rng, (0, 64, 0));
        assert_eq!(!ctx.wr.is_empty(), p[2].parse::<bool>().unwrap(), "{row}");
        assert_eq!(ctx.wr.len(), p[3].parse::<usize>().unwrap(), "{row}");
        assert_eq!(block_cells_digest(reg, &ctx.wr, (0, 64, 0)), p[4], "{row}");
    }
}

#[test]
fn nether_forest_vegetation_matches_native_cells_and_rng() {
    let reg = breg().unwrap();
    let weighted = r#"{"state_provider":{"type":"minecraft:weighted_state_provider","entries":[{"data":{"Name":"minecraft:crimson_roots"},"weight":87},{"data":{"Name":"minecraft:crimson_fungus"},"weight":11},{"data":{"Name":"minecraft:warped_fungus"},"weight":1}]},"spread_width":8,"spread_height":4}"#;
    for row in include_str!("../tests/data/nether_forest_vegetation_1211.tsv").lines() {
        let p: Vec<_> = row.split('\t').collect();
        let (support, plant) = (p[0], p[1]);
        let oy = p[2].parse::<i32>().unwrap();
        let seed = p[3].parse::<i64>().unwrap() as u64;
        let json = if plant == "weighted" {
            format!(r#"{{"type":"minecraft:nether_forest_vegetation","config":{weighted}}}"#)
        } else {
            format!(
                r#"{{"type":"minecraft:nether_forest_vegetation","config":{{"state_provider":{{"type":"minecraft:simple_state_provider","state":{{"Name":"{plant}"}}}},"spread_width":8,"spread_height":4}}}}"#
            )
        };
        let cf = super::p_cf(&jparse(json.as_bytes()).unwrap()).unwrap();
        let support = reg.state_of(support, None);
        let mut g = Gen::load(0, crate::name_to_state).unwrap();
        for x in -12i32..=12 {
            for z in -12i32..=12 {
                for dy in -6i32..=6 {
                    let state = if dy == -1 { support } else { 0 };
                    g.feat_write(x, oy + dy, z, state);
                }
            }
        }
        let mut ctx = context(&mut g);
        let mut rng = Xor::seed_worldgen(seed);
        gen_cf(&cf, freg(), &mut ctx, &mut rng, (0, oy, 0));
        assert_eq!(!ctx.wr.is_empty(), p[4].parse::<bool>().unwrap(), "{row}");
        assert_eq!(ctx.wr.len(), p[5].parse::<usize>().unwrap(), "{row}");
        assert_eq!(block_cells_digest(reg, &ctx.wr, (0, oy, 0)), p[6], "{row}");
    }
}

#[test]
fn flower_type_names_share_one_native_class_and_one_port_arm() {
    let mut classes = std::collections::HashSet::new();
    let mut seen = 0;
    for row in include_str!("../tests/data/feature_classes_1211.tsv").lines() {
        let mut f = row.split('\t');
        let name = f.next().unwrap();
        if !matches!(name, "flower" | "no_bonemeal_flower" | "random_patch") {
            continue;
        }
        seen += 1;
        classes.insert(f.next().unwrap().to_string());
    }
    assert_eq!(seen, 3);
    assert_eq!(
        classes,
        std::collections::HashSet::from(["edc".to_string()])
    );
    for ty in ["flower", "no_bonemeal_flower", "random_patch"] {
        let json = format!(
            r#"{{"type":"minecraft:{ty}","config":{{"feature":{{"feature":{{"type":"minecraft:simple_block","config":{{"to_place":{{"type":"minecraft:simple_state_provider","state":{{"Name":"minecraft:poppy"}}}}}}}},"placement":[]}},"tries":1,"xz_spread":0,"y_spread":0}}}}"#
        );
        let cf = super::p_cf(&jparse(json.as_bytes()).unwrap()).unwrap();
        assert!(matches!(cf, Cf::RandomPatch { .. }), "{ty}");
    }
}

/// Build the digest the native oracles record: cells relative to the origin, with
/// the placed block name, sorted.
fn block_cells_digest(
    reg: &crate::block_registry::BReg,
    wr: &std::collections::HashMap<(i32, i32, i32), u32>,
    origin: (i32, i32, i32),
) -> String {
    let mut cells = Vec::new();
    for (&(x, y, z), &state) in wr {
        cells.push(format!(
            "{},{},{} {}\n",
            x - origin.0,
            y - origin.1,
            z - origin.2,
            reg.name_of(state)
        ));
    }
    cells.sort();
    crate::generation_math::md5(cells.concat().as_bytes())
        .iter()
        .map(|v| format!("{v:02x}"))
        .collect()
}

/// `minecraft:block[prop=value,...]` with the properties sorted by name: the canonical form
/// `NativeHarness.normalizedStateName` writes, used by the digest and by `GENERIC_CASE_DUMP`.
fn block_state_cells_text(reg: &crate::block_registry::BReg, state: u32) -> String {
    let mut cell = reg.name_of(state).to_string();
    let mut props = reg.properties_of(state).unwrap_or_default();
    if !props.is_empty() {
        props.sort();
        cell.push('[');
        for (i, (k, v)) in props.iter().enumerate() {
            if i > 0 {
                cell.push(',');
            }
            cell.push_str(k);
            cell.push('=');
            cell.push_str(v);
        }
        cell.push(']');
    }
    cell
}

/// The same cells, but with the state's properties sorted by name — the canonical form
/// `NativeHarness.normalizedStateName` writes. A block name alone cannot show that a
/// mushroom cap's `west`/`east`/`north`/`south` are wrong.
fn block_state_cells_digest(
    reg: &crate::block_registry::BReg,
    wr: &std::collections::HashMap<(i32, i32, i32), u32>,
    origin: (i32, i32, i32),
) -> String {
    let mut cells = Vec::new();
    for (&(x, y, z), &state) in wr {
        cells.push(format!(
            "{},{},{} {}\n",
            x - origin.0,
            y - origin.1,
            z - origin.2,
            block_state_cells_text(reg, state)
        ));
    }
    cells.sort();
    crate::generation_math::md5(cells.concat().as_bytes())
        .iter()
        .map(|v| format!("{v:02x}"))
        .collect()
}

#[test]
fn void_start_platform_matches_native_cells() {
    let reg = breg().unwrap();
    let cf =
        super::p_cf(&jparse(br#"{"type":"minecraft:void_start_platform","config":{}}"#).unwrap())
            .unwrap();
    for row in include_str!("../tests/data/void_start_platform_1211.tsv").lines() {
        let p: Vec<_> = row.split('\t').collect();
        let origin: Vec<i32> = p[1].split(',').map(|v| v.parse().unwrap()).collect();
        let origin = (origin[0], origin[1], origin[2]);
        let mut g = Gen::load(0, crate::name_to_state).unwrap();
        let mut ctx = context(&mut g);
        let mut rng = Xor::seed_worldgen(0);
        gen_cf(&cf, freg(), &mut ctx, &mut rng, origin);
        // The refusal branch of `void_start_platform` also returns true, so the write
        // set is the evidence; the placed flag is asserted where it is informative.
        assert_eq!(p[2], "true", "{row}");
        assert_eq!(ctx.wr.len(), p[3].parse::<usize>().unwrap(), "{row}");
        assert_eq!(block_cells_digest(reg, &ctx.wr, origin), p[5], "{row}");
        assert_eq!(rng.next_i64(), p[4].parse::<i64>().unwrap(), "{row}");
    }
}

#[test]
fn end_platform_matches_native_cells() {
    let reg = breg().unwrap();
    let cf =
        super::p_cf(&jparse(br#"{"type":"minecraft:end_platform","config":{}}"#).unwrap()).unwrap();
    let obsidian = reg.state_of("minecraft:obsidian", None);
    let stone = reg.state_of("minecraft:stone", None);
    for row in include_str!("../tests/data/end_platform_1211.tsv").lines() {
        let p: Vec<_> = row.split('\t').collect();
        let scene = p[0].parse::<i32>().unwrap();
        let mut g = Gen::load(0, crate::name_to_state).unwrap();
        for dx in -2i32..=2 {
            for dz in -2i32..=2 {
                for dy in -1i32..=2 {
                    // the same scenes the native oracle scripts
                    let state = match scene {
                        0 => 0,
                        1 => {
                            if dy == -1 {
                                obsidian
                            } else {
                                0
                            }
                        }
                        2 => {
                            if dy == -1 {
                                if dx > 0 {
                                    obsidian
                                } else {
                                    0
                                }
                            } else if dy >= 0 && dz < 0 {
                                stone
                            } else {
                                0
                            }
                        }
                        3 => {
                            if dy >= 2 {
                                stone
                            } else {
                                0
                            }
                        }
                        _ => {
                            if dy <= -1 {
                                obsidian
                            } else {
                                0
                            }
                        }
                    };
                    g.feat_write(dx, 64 + dy, dz, state);
                }
            }
        }
        let mut ctx = context(&mut g);
        let mut rng = Xor::seed_worldgen(0);
        gen_cf(&cf, freg(), &mut ctx, &mut rng, (0, 64, 0));
        // `end_platform` always returns true, so its writes are the only evidence.
        assert_eq!(p[2], "true", "{row}");
        assert_eq!(ctx.wr.len(), p[3].parse::<usize>().unwrap(), "{row}");
        assert_eq!(block_cells_digest(reg, &ctx.wr, (0, 64, 0)), p[5], "{row}");
        assert_eq!(rng.next_i64(), p[4].parse::<i64>().unwrap(), "{row}");
    }
}

#[test]
fn basalt_pillar_matches_native_cells_and_rng() {
    let reg = breg().unwrap();
    let cf = super::p_cf(&jparse(br#"{"type":"minecraft:basalt_pillar","config":{}}"#).unwrap())
        .unwrap();
    let basalt = reg.state_of("minecraft:basalt", None);
    // The native check reads the cell above the origin, so every scene is built
    // around a ceiling at dy = 1 (see tools/structure-oracle/BasaltPillarOracle.java).
    let solid = |scene: i32, dx: i32, dy: i32, dz: i32| -> bool {
        match scene {
            0 => dy == 1 || dy <= -5,
            1 => dy == 1 || dy <= -24,
            2 => dy <= -5,
            3 => dy == 0 || dy == 1 || dy <= -5,
            4 => {
                dy == 1
                    || dy <= -5
                    || (dx.abs() == 1 && dz == 0 && dy % 3 == 0)
                    || (dz.abs() == 1 && dx == 0 && dy % 2 == 0)
            }
            5 => dy == 1,
            6 => dy == 1 || dy <= -1 - ((dx * dx + dz * dz) % 3),
            _ => unreachable!(),
        }
    };
    for row in include_str!("../tests/data/basalt_pillar_1211.tsv").lines() {
        let p: Vec<_> = row.split('\t').collect();
        let scene = p[0].parse::<i32>().unwrap();
        let seed = p[1].parse::<i64>().unwrap() as u64;
        let mut g = Gen::load(0, crate::name_to_state).unwrap();
        for x in -12i32..=12 {
            for z in -12i32..=12 {
                for dy in -140i32..=2 {
                    let state = if solid(scene, x, dy, z) { basalt } else { 0 };
                    g.feat_write(x, 64 + dy, z, state);
                }
            }
        }
        let mut ctx = context(&mut g);
        let mut rng = Xor::seed_worldgen(seed);
        gen_cf(&cf, freg(), &mut ctx, &mut rng, (0, 64, 0));
        assert_eq!(!ctx.wr.is_empty(), p[2].parse::<bool>().unwrap(), "{row}");
        assert_eq!(ctx.wr.len(), p[3].parse::<usize>().unwrap(), "{row}");
        let mut cells = Vec::new();
        for (&(x, y, z), &state) in &ctx.wr {
            assert_eq!(reg.name_of(state), "minecraft:basalt");
            cells.push(format!("{x},{},{z}\n", y - 64));
        }
        cells.sort();
        let digest: String = crate::generation_math::md5(cells.concat().as_bytes())
            .iter()
            .map(|v| format!("{v:02x}"))
            .collect();
        assert_eq!(digest, p[5], "{row}");
        assert_eq!(rng.next_i64(), p[4].parse::<i64>().unwrap(), "{row}");
    }
}

#[test]
fn chorus_plant_matches_native_cells_states_and_rng() {
    let reg = breg().unwrap();
    let cf =
        super::p_cf(&jparse(br#"{"type":"minecraft:chorus_plant","config":{}}"#).unwrap()).unwrap();
    let stone = reg.state_of("minecraft:stone", None);
    let end_stone = reg.state_of("minecraft:end_stone", None);
    for row in include_str!("../tests/data/chorus_plant_1211.tsv").lines() {
        let p: Vec<_> = row.split('\t').collect();
        let scene = p[0].parse::<i32>().unwrap();
        let seed = p[1].parse::<i64>().unwrap() as u64;
        let mut g = Gen::load(0, crate::name_to_state).unwrap();
        for x in -10i32..=10 {
            for z in -10i32..=10 {
                for dy in -4..=36 {
                    let obstacle = scene == 1 && x.abs() + z.abs() == 2 && (2..=8).contains(&dy)
                        || scene == 3 && (x, dy, z) == (0, 0, 0)
                        || scene == 5 && dy == 4 && x.abs() + z.abs() <= 5;
                    let state = if obstacle {
                        stone
                    } else if dy < 0 && (scene != 4 || x == 0 && z == 0) {
                        if scene == 2 {
                            stone
                        } else {
                            end_stone
                        }
                    } else {
                        0
                    };
                    g.feat_write(x, 64 + dy, z, state);
                }
            }
        }
        let mut ctx = context(&mut g);
        let mut rng = Xor::seed_worldgen(seed);
        gen_cf(&cf, freg(), &mut ctx, &mut rng, (0, 64, 0));
        assert_eq!(!ctx.wr.is_empty(), p[2].parse::<bool>().unwrap(), "{row}");
        assert_eq!(ctx.wr.len(), p[3].parse::<usize>().unwrap(), "{row}");
        let mut cells = Vec::new();
        for (&(x, y, z), &state) in &ctx.wr {
            let value = match reg.name_of(state) {
                "minecraft:chorus_plant" => {
                    let mut mask = 0;
                    for (i, name) in ["down", "up", "north", "south", "west", "east"]
                        .iter()
                        .enumerate()
                    {
                        if reg.get_prop(state, name).as_deref() == Some("true") {
                            mask |= 1 << i;
                        }
                    }
                    format!("plant:{mask}")
                }
                "minecraft:chorus_flower" => {
                    assert_eq!(reg.get_prop(state, "age").as_deref(), Some("5"));
                    "flower:5".to_string()
                }
                name => panic!("unexpected chorus state {name}"),
            };
            cells.push(format!("{x},{},{z}:{value}\n", y - 64));
        }
        cells.sort();
        let digest: String = crate::generation_math::md5(cells.concat().as_bytes())
            .iter()
            .map(|v| format!("{v:02x}"))
            .collect();
        assert_eq!(digest, p[5], "{row}");
        assert_eq!(rng.next_i64(), p[4].parse::<i64>().unwrap(), "{row}");
    }
}

#[test]
fn each_dimension_feature_order_matches_native_sorter_indices() {
    let native = jparse(include_bytes!("../assets/feature_order_1211.json")).unwrap();
    for preset in [
        crate::worldgen::NoisePreset::Overworld,
        crate::worldgen::NoisePreset::Nether,
        crate::worldgen::NoisePreset::End,
    ] {
        let expected = native
            .get(preset.name())
            .unwrap()
            .get("steps")
            .unwrap()
            .as_arr()
            .unwrap();
        let actual = freg().order.get(&preset).unwrap();
        assert_eq!(actual.len(), expected.len(), "{preset:?}");
        for (step, (actual, expected)) in actual.iter().zip(expected).enumerate() {
            let expected = expected.as_arr().unwrap();
            assert_eq!(actual.len(), expected.len(), "{preset:?} step {step}");
            for (index, ((slot, name), native)) in actual.iter().zip(expected).enumerate() {
                assert_eq!(*slot as usize, index, "{preset:?} step {step}");
                assert_eq!(name, native.str(), "{preset:?} step {step} index {index}");
            }
        }
    }
}

#[test]
fn end_island_feature_matches_native_writes_and_rng() {
    let reg = breg().unwrap();
    let cf =
        super::p_cf(&jparse(br#"{"type":"minecraft:end_island","config":{}}"#).unwrap()).unwrap();
    for row in include_str!("../tests/data/end_island_feature_1211.tsv").lines() {
        let p: Vec<_> = row.split('\t').collect();
        let seed = p[0].parse::<i64>().unwrap() as u64;
        let mut g = Gen::load(0, crate::name_to_state).unwrap();
        let mut ctx = context(&mut g);
        let mut rng = Xor::seed_worldgen(seed);
        gen_cf(&cf, freg(), &mut ctx, &mut rng, (0, 64, 0));
        assert_eq!(p[1], "true");
        assert_eq!(ctx.wr.len(), p[2].parse::<usize>().unwrap(), "{row}");
        let mut cells = Vec::new();
        for (&(x, y, z), &state) in &ctx.wr {
            assert_eq!(reg.name_of(state), "minecraft:end_stone");
            cells.push(format!("{x},{},{z}\n", y - 64));
        }
        cells.sort();
        let digest: String = crate::generation_math::md5(cells.concat().as_bytes())
            .iter()
            .map(|v| format!("{v:02x}"))
            .collect();
        assert_eq!(digest, p[4], "{row}");
        assert_eq!(rng.next_i64(), p[3].parse::<i64>().unwrap(), "{row}");
    }
}

#[test]
fn carver_below_top_anchor_is_relative_to_the_last_block() {
    // VerticalAnchor.BelowTop.resolveY: genDepth - 1 + minGenY - value.
    assert_eq!(super::Anchor::BelowTop(1).resolve(0, 128), 126);
    assert_eq!(super::Anchor::BelowTop(0).resolve(-64, 384), 319);
}

#[test]
fn a_chunk_claims_its_carver_pass_once() {
    let mut g = Gen::load(0, crate::name_to_state).expect("datapack");
    assert!(g.begin_carve(1, 2), "the first pass is claimed");
    assert!(!g.begin_carve(1, 2), "the second pass is a no-op");
    assert!(g.begin_carve(1, 3), "a different chunk has its own claim");
}

/// The WG heightmaps are frozen at the end of the carver pass, and the carver is the
/// only thing that moves them. Native `ProtoChunk.setBlockState` tracks
/// `getStatus().heightmapsAfter()`; a chunk still reports SURFACE while its carvers run
/// (the new status is applied when the stage body returns) and FEATURES while its
/// features do, and FEATURES' set is the non-WG one - so carve writes lower
/// WORLD_SURFACE_WG/OCEAN_FLOOR_WG and feature or structure writes never touch them.
/// Reading the uncarved terrain instead cost the ruined portal at seed 4242 chunk (6,7)
/// its whole netherrack base: (107,122) read y=110 where vanilla reads 100, which shifted
/// the weighted placement stream and 124 portal cells.
#[test]
fn wg_heightmap_keeps_the_carved_top_and_ignores_the_feature_layer() {
    use crate::block_registry::HeightmapKind;
    let reg = breg().unwrap();
    let mut g = Gen::load(0, crate::name_to_state).expect("datapack");
    let air = reg.state_of("minecraft:air", None);
    let stone = reg.state_of("minecraft:stone", None);
    let (cx, cz) = (0, 0);
    let (lx, lz) = (0u8, 0u8);
    let (x, z) = (lx as i32, lz as i32);
    let cc = g.cols(cx, cz).expect("chunk columns");
    let plain = super::wg_heightmaps_after_carve(&mut g, &cc, cx, cz, &Default::default());
    let index = lz as usize * 16 + lx as usize;
    let top = plain[index];
    assert!(top > g.min_y, "the fixture column must have terrain");
    // A cave mouth that takes the top three cells of the column.
    let mut out: std::collections::HashMap<(u8, i32, u8), u32> = Default::default();
    for y in top - 3..top {
        out.insert((lx, y, lz), air);
    }
    let carved = super::wg_heightmaps_after_carve(&mut g, &cc, cx, cz, &out);
    assert!(
        carved[index] <= top - 3,
        "a carve of the top three cells must lower the WG top ({top} -> {})",
        carved[index]
    );
    let carved_surface = carved[index];
    let carved_floor = carved[256 + index];
    g.set_wg_after_carve(cx, cz, carved);
    let mut ctx = context(&mut g);
    assert_eq!(
        ctx.heightmap(HeightmapKind::WorldSurfaceWg, x, z),
        carved_surface
    );
    assert_eq!(
        ctx.heightmap(HeightmapKind::OceanFloorWg, x, z),
        carved_floor
    );
    // A feature layer write above the carved top moves the live reads and the non-WG
    // heightmap, but not the WG one.
    let high = top + 20;
    ctx.set(x, high, z, stone);
    assert_eq!(ctx.get(x, high, z), stone);
    assert_eq!(
        ctx.heightmap(HeightmapKind::WorldSurfaceWg, x, z),
        carved_surface,
        "the feature layer must not move a WG heightmap"
    );
    assert_eq!(
        ctx.heightmap(HeightmapKind::OceanFloor, x, z),
        high + 1,
        "the non-WG map does track the feature layer"
    );
}

/// A feature runs after the carver pass and must read the carver's air/fluid
/// writes, not the uncarved terrain. This is the lapis-in-a-cave regression.
#[test]
fn feature_context_sees_carver_writes() {
    let mut g = Gen::load(0, crate::name_to_state).expect("datapack");
    let cave_air = crate::name_to_state("minecraft:cave_air").expect("cave air");
    let (x, y, z) = (0, -60, 0);
    assert_ne!(g.terrain_state(x, y, z), cave_air);
    g.feat_write(x, y, z, cave_air);
    let mut ctx = context(&mut g);
    assert_eq!(ctx.get(x, y, z), cave_air);
}

/// Native canPlaceOre vectors pin both the decision and the following RNG draw.
/// In particular discard=1 means check all six neighbours, not reject all ore.
#[test]
fn ore_air_exposure_matches_native_decisions_and_rng() {
    let reg = breg().unwrap();
    let mut g = Gen::load(0, crate::name_to_state).expect("datapack");
    let stone = reg.state_of("minecraft:stone", None);
    let air = reg.state_of("minecraft:air", None);
    let rule = RT::Block("minecraft:stone".into());
    let mut ctx = context(&mut g);
    let rows = include_str!("../tests/data/ore_air_1211.tsv");
    let mut checked = 0;
    for line in rows.lines() {
        let fields: Vec<_> = line.split('\t').collect();
        assert_eq!(fields.len(), 6, "{line}");
        let seed = fields[0].parse().unwrap();
        let chance = fields[1].parse().unwrap();
        let exposed: i32 = fields[2].parse().unwrap();
        let matches: bool = fields[3].parse().unwrap();
        let expected: bool = fields[4].parse().unwrap();
        let following: i64 = fields[5].parse().unwrap();
        ctx.set(0, 16, 0, if matches { stone } else { air });
        for face in 0..6u8 {
            let (x, y, z) = crate::generation_providers::dir_off(face);
            ctx.set(
                x,
                16 + y,
                z,
                if exposed == face as i32 { air } else { stone },
            );
        }
        let mut rng = Xor::seed(seed);
        assert_eq!(
            super::ore_should_place(chance, &mut ctx, &rule, (0, 16, 0), &mut rng),
            expected,
            "native canPlaceOre decision: {line}"
        );
        assert_eq!(
            rng.next_long() as i64,
            following,
            "native following RNG: {line}"
        );
        checked += 1;
    }
    assert_eq!(
        checked, 126,
        "native exposure matrix must not be empty/truncated"
    );
}

/// Compare every dungeon write (including chest facing) and the following RNG
/// against the actual MonsterRoomFeature with real chest/spawner entities.
fn monster_room_volume(seed: u64, wrapped: bool) -> Gen {
    let reg = breg().unwrap();
    let mut dimensions = if wrapped {
        Xor::seed_worldgen(seed)
    } else {
        Xor::seed(seed)
    };
    let xr = dimensions.next_bound(2) + 2;
    let mut g = Gen::load(0, crate::name_to_state).expect("datapack");
    let stone = reg.state_of("minecraft:stone", None);
    let cave_air = reg.state_of("minecraft:cave_air", None);
    for x in -6..=6 {
        for y in 14..=20 {
            for z in -6..=6 {
                let hole = x == -xr - 1 && z == 0 && (y == 16 || y == 17);
                g.feat_write(x, y, z, if hole { cave_air } else { stone });
            }
        }
    }
    g
}

#[test]
fn monster_room_cells_and_rng_match_native() {
    let reg = breg().unwrap();
    let mut count = 0;
    let mut total_chests = 0;
    let mut wrapped_chests = 0;
    for row in include_str!("../tests/data/monster_room_1211.tsv").lines() {
        let f: Vec<_> = row.split('\t').collect();
        assert_eq!(f.len(), 7);
        let seed = f[0].parse().unwrap();
        let wrapped = match f[6] {
            "raw" => false,
            "worldgen" => true,
            other => panic!("unknown RNG: {other}"),
        };
        assert_eq!(f[1], "true", "native dungeon must place");
        let mut g = monster_room_volume(seed, wrapped);
        let mut ctx = context(&mut g);
        let mut rng = if wrapped {
            Xor::seed_worldgen(seed)
        } else {
            Xor::seed(seed)
        };
        gen_cf(&Cf::MonsterRoom, freg(), &mut ctx, &mut rng, (0, 16, 0));
        let mut cells = Vec::new();
        let mut chests = 0;
        for (&(x, y, z), &st) in &ctx.wr {
            let name = reg.name_of(st);
            let facing = if name == "minecraft:chest" {
                chests += 1;
                reg.get_prop(st, "facing").unwrap()
            } else {
                "-".into()
            };
            cells.push(format!("{x},{y},{z}:{name}:{facing}\n"));
        }
        cells.sort();
        let hash: String = crate::generation_math::md5(cells.concat().as_bytes())
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        assert_eq!(cells.len(), f[2].parse::<usize>().unwrap(), "writes: {row}");
        assert_eq!(chests, f[3].parse::<usize>().unwrap(), "chests: {row}");
        total_chests += chests;
        if wrapped {
            wrapped_chests += chests;
        }
        assert_eq!(hash, f[4], "native cell/facing set: {row}");
        assert_eq!(rng.next_i64(), f[5].parse::<i64>().unwrap(), "RNG: {row}");
        count += 1;
    }
    assert_eq!(count, 8);
    assert!(wrapped_chests > 0, "exercise wrapped nextLong loot seeds");
    assert!(
        total_chests > 0,
        "native fixture must exercise chest placement"
    );
}

#[test]
fn monster_room_preserves_protected_floor_and_centre() {
    let mut g = monster_room_volume(0, false);
    let bedrock = breg().unwrap().state_of("minecraft:bedrock", None);
    for y in [15, 16] {
        g.feat_write(0, y, 0, bedrock);
    }
    let mut ctx = context(&mut g);
    gen_cf(
        &Cf::MonsterRoom,
        freg(),
        &mut ctx,
        &mut Xor::seed(0),
        (0, 16, 0),
    );
    assert!(!ctx.wr.is_empty(), "fixture must reach room placement");
    for y in [15, 16] {
        assert_eq!(
            ctx.get(0, y, 0),
            bedrock,
            "Feature.safeSetBlock protection at {y}"
        );
        assert!(!ctx.wr.contains_key(&(0, y, 0)));
    }
}

#[test]
fn simple_block_places_both_halves_or_neither() {
    let reg = breg().unwrap();
    for blocked in [false, true] {
        let mut g = Gen::load(0, crate::name_to_state).unwrap();
        g.feat_write(0, 199, 0, reg.state_of("minecraft:grass_block", None));
        g.feat_write(0, 200, 0, 0);
        g.feat_write(
            0,
            201,
            0,
            if blocked {
                reg.state_of("minecraft:stone", None)
            } else {
                0
            },
        );
        let mut ctx = context(&mut g);
        let state = reg.state_of("minecraft:lilac", None);
        let cf = Cf::SimpleBlock {
            to: Bsp::Simple(state),
            on: vec![],
            in_: vec![],
            under: vec![],
        };
        let mut rng = Xor::seed_worldgen(11);
        let mut untouched = Xor::seed_worldgen(11);
        gen_cf(&cf, freg(), &mut ctx, &mut rng, (0, 200, 0));
        assert_eq!(rng.next_i64(), untouched.next_i64(), "no placement RNG");
        if blocked {
            assert!(ctx.wr.is_empty(), "do not leave a lower half");
        } else {
            assert_eq!(ctx.wr.len(), 2);
            assert_eq!(ctx.get(0, 200, 0), reg.with_prop(state, "half", "lower"));
            assert_eq!(ctx.get(0, 201, 0), reg.with_prop(state, "half", "upper"));
        }
    }
    let mut g = Gen::load(0, crate::name_to_state).unwrap();
    g.feat_write(0, 199, 0, reg.state_of("minecraft:clay", None));
    g.feat_write(0, 200, 0, reg.state_of("minecraft:water", None));
    g.feat_write(0, 201, 0, 0);
    let mut ctx = context(&mut g);
    let cf = Cf::SimpleBlock {
        to: Bsp::Simple(reg.state_of("minecraft:small_dripleaf", None)),
        on: vec![],
        in_: vec![],
        under: vec![],
    };
    gen_cf(
        &cf,
        freg(),
        &mut ctx,
        &mut Xor::seed_worldgen(11),
        (0, 200, 0),
    );
    assert_eq!(ctx.wr.len(), 2);
    assert_eq!(
        reg.get_prop(ctx.get(0, 200, 0), "waterlogged").as_deref(),
        Some("true")
    );
    assert_eq!(
        reg.get_prop(ctx.get(0, 201, 0), "waterlogged").as_deref(),
        Some("false")
    );
    assert!(!reg.is_double_plant(reg.state_of("minecraft:oak_stairs", None)));
}

#[test]
fn mushrooms_in_fresh_generation_need_the_special_substrate() {
    let reg = breg().unwrap();
    let mut g = Gen::load(0, crate::name_to_state).unwrap();
    let mut ctx = context(&mut g);
    // The native fixture's uninitialized sky light reads 15 even under trees.
    ctx.set(0, 202, 0, reg.state_of("minecraft:oak_leaves", None));
    for (soil, expected) in [
        ("dirt", false),
        ("grass_block", false),
        ("stone", false),
        ("basalt", false),
        ("netherrack", false),
        ("podzol", true),
        ("mycelium", true),
    ] {
        ctx.set(0, 199, 0, reg.state_of(&format!("minecraft:{soil}"), None));
        for mushroom in ["minecraft:brown_mushroom", "minecraft:red_mushroom"] {
            assert_eq!(
                super::can_place(reg, &mut ctx, mushroom, 0, 200, 0),
                expected,
                "{soil} {mushroom}"
            );
        }
    }
}

/// `MushroomBlock.canSurvive` asks `isSolidRender`, not `BushBlock.mayPlaceOn`.
///
/// `dla.a(dtc,dcz,jd)` is
///
/// ```java
/// return support.is(BlockTags.MUSHROOM_GROW_BLOCK)
///       ? true
///       : level.getRawBrightness(pos, 0) < 13 && this.b(support, level, below);
/// ```
///
/// and `this.b` resolves to `dla`'s own override - `BlockState.isSolidRender(BlockGetter,
/// BlockPos)` (`dtb$a.i(dcc,jd)`, official mappings `1035:1042:boolean
/// isSolidRender(net.minecraft.world.level.BlockGetter,net.minecraft.core.BlockPos) -> i`). The
/// obvious-looking reading, `BushBlock.mayPlaceOn` (farmland | `#minecraft:dirt`, `dgh.b`), has
/// the same signature and is wrong: measured on the pinned Nether fixture (seed 4242,
/// `(-25,19)+3x3`) that reading refuses 24 `brown/red_mushroom_nether` cells the reference places
/// on netherrack.
///
/// The Overworld fixture cannot see this either way, because its uninitialized sky light is 15 and
/// the brightness half already refuses every non-grow-block support there. A dimension without a
/// sky light reads its block light (0 before anything lights the chunk), so the support test is
/// the only gate - which is why this table lives in the Nether preset.
#[test]
fn nether_mushrooms_survive_a_solid_render_support() {
    let reg = breg().unwrap();
    let mut g = Gen::load_preset(0, crate::name_to_state, crate::worldgen::NoisePreset::Nether)
        .expect("nether preset");
    assert!(!g.has_skylight(), "the Nether has no sky light layer");
    let mut ctx = context(&mut g);
    // A cell to place into: the mushroom itself must be in open air.
    ctx.set(0, 200, 0, 0);
    for (soil, expected) in [
        // solidRender: a placeable support in the dark, whether or not it is dirt
        ("netherrack", true),
        ("basalt", true),
        ("soul_soil", true),
        ("blackstone", true),
        // `#minecraft:mushroom_grow_block` skips the light test
        ("podzol", true),
        ("mycelium", true),
        // neither: not a solid render (this is where `isSolidRender` and the dirt tag differ in
        // the other direction - nothing here is in `#minecraft:dirt` either, so both readings
        // agree, and the cell is the discriminator for the solid-render half)
        ("water", false),
        ("lava", false),
    ] {
        ctx.set(0, 199, 0, reg.state_of(&format!("minecraft:{soil}"), None));
        for mushroom in ["minecraft:brown_mushroom", "minecraft:red_mushroom"] {
            assert_eq!(
                super::can_place(reg, &mut ctx, mushroom, 0, 200, 0),
                expected,
                "{soil} {mushroom}"
            );
        }
    }
}

/// `BushBlock.mayPlaceOn` is `state.is(Blocks.FARMLAND) || state.is(BlockTags.DIRT)`.
///
/// The table is the native `BlockState.canSurvive` for `spruce_sapling[stage=0]`,
/// `oak_sapling[stage=0]` and `short_grass`, run against a scripted 1.21.1 level on the pinned
/// server jar; all three plants agree cell for cell. `minecraft:dirt_path` is the one support the
/// port's old `is_dirtish` list accepted that the reference refuses (`dirt.json` holds
/// dirt/grass_block/podzol/coarse_dirt/mycelium/rooted_dirt/moss_block/mud/muddy_mangrove_roots
/// and not the path).
///
/// This is what grew trees on taiga village roads: `spruce_checked` is a
/// `block_predicate_filter(would_survive)` in front of `minecraft:spruce`, so a path under the
/// sapling let the tree through, and `TrunkPlacer.setDirtAt` then wrote `dirt` over the road.
/// Seed 4242 (4,-32)+3x3 spawned trees at (65,70,-498) and (71,69,-502) that way, and those two
/// cells did not stay put: two spurious spruces rearranged the canopy, the `would_survive`
/// outcomes of later candidates and 2,543 `trees_taiga` cells.
#[test]
fn bush_block_family_support_is_the_dirt_tag_or_farmland() {
    let reg = breg().unwrap();
    let mut g = Gen::load(0, crate::name_to_state).unwrap();
    let mut ctx = context(&mut g);
    ctx.set(0, 201, 0, 0);
    for (support, expected) in [
        ("dirt", true),
        ("grass_block", true),
        ("coarse_dirt", true),
        ("podzol", true),
        ("moss_block", true),
        ("mycelium", true),
        ("rooted_dirt", true),
        ("farmland", true),
        ("dirt_path", false),
        ("stone", false),
        ("sand", false),
        ("gravel", false),
        ("cobblestone", false),
    ] {
        ctx.set(0, 199, 0, reg.state_of(&format!("minecraft:{support}"), None));
        for plant in [
            "minecraft:spruce_sapling",
            "minecraft:oak_sapling",
            "minecraft:short_grass",
            "minecraft:fern",
        ] {
            assert_eq!(
                super::can_place(reg, &mut ctx, plant, 0, 200, 0),
                expected,
                "{plant} on {support}"
            );
        }
    }
    // The same rule through the predicate the tree placement actually uses.
    ctx.set(0, 199, 0, reg.state_of("minecraft:dirt_path", None));
    assert!(!test_pred(
        &Pr::WouldSurvive("minecraft:spruce_sapling".into()),
        reg,
        &mut ctx,
        0,
        200,
        0
    ));
    ctx.set(0, 199, 0, reg.state_of("minecraft:grass_block", None));
    assert!(test_pred(
        &Pr::WouldSurvive("minecraft:spruce_sapling".into()),
        reg,
        &mut ctx,
        0,
        200,
        0
    ));
}

/// `SporeBlossomBlock.canSurvive` (`dnp.a(dtc,dcz,jd)`): `Block.canSupportCenter(level,
/// pos.above(), DOWN) && !level.isWaterAt(pos)`.
///
/// The spore blossom hangs from a ceiling, so the support is the block *above* and its DOWN face.
/// Falling through to the `BushBlock` family rule asked for dirt *below*, which air never
/// satisfies - the taiga 3x3 had 7 native spore blossoms and the port had none, and each missing
/// one also changed the ceiling walk of the `moss_patch_ceiling` above it.
#[test]
fn spore_blossom_needs_a_sturdy_ceiling_and_a_dry_cell() {
    let reg = breg().unwrap();
    let mut g = Gen::load(0, crate::name_to_state).unwrap();
    let mut ctx = context(&mut g);
    let blossom = "minecraft:spore_blossom";
    ctx.set(0, 199, 0, 0);
    ctx.set(0, 200, 0, 0);
    ctx.set(0, 201, 0, reg.state_of("minecraft:stone", None));
    assert!(super::can_place(reg, &mut ctx, blossom, 0, 200, 0));
    // dirt below is irrelevant: the support is the ceiling.
    ctx.set(0, 199, 0, reg.state_of("minecraft:dirt", None));
    assert!(super::can_place(reg, &mut ctx, blossom, 0, 200, 0));
    // used to pass through the family rule and hang in mid-air.
    ctx.set(0, 201, 0, 0);
    assert!(!super::can_place(reg, &mut ctx, blossom, 0, 200, 0));
    // a non-full ceiling offers no DOWN face.
    ctx.set(0, 201, 0, reg.state_of("minecraft:oak_leaves", None));
    assert!(!super::can_place(reg, &mut ctx, blossom, 0, 200, 0));
    // a water cell refuses even under stone.
    ctx.set(0, 201, 0, reg.state_of("minecraft:stone", None));
    ctx.set(0, 200, 0, reg.state_of("minecraft:water", None));
    assert!(!super::can_place(reg, &mut ctx, blossom, 0, 200, 0));
}

/// `SmallDripleafBlock.canSurvive` (`dne.a(dtc,dcz,jd)`), measured on the pinned jar: the support
/// is a `#minecraft:small_dripleaf_placeable` block (clay, moss_block) under *any* cell, or
/// `#minecraft:dirt`/farmland under water. The cell is not part of the first case.
///
/// The port required water in the cell *and* a placeable support, so a `clay_with_dripleaves`
/// dripleaf on dry clay was refused (measured: (69,-7,-493) and (69,-6,-493) in the taiga 3x3).
#[test]
fn small_dripleaf_supports_are_the_placeable_tag_or_dirt_under_water() {
    let reg = breg().unwrap();
    let mut g = Gen::load(0, crate::name_to_state).unwrap();
    let mut ctx = context(&mut g);
    let dripleaf = "minecraft:small_dripleaf";
    let air = reg.state_of("minecraft:air", None);
    let water = reg.state_of("minecraft:water", None);
    ctx.set(0, 201, 0, air);
    for (support, cell, expected) in [
        ("clay", air, true),
        ("moss_block", air, true),
        ("clay", water, true),
        ("moss_block", water, true),
        ("dirt", water, true),
        ("grass_block", water, true),
        ("dirt", air, false),
        ("stone", air, false),
        ("stone", water, false),
        ("water", water, false),
    ] {
        ctx.set(0, 199, 0, reg.state_of(&format!("minecraft:{support}"), None));
        ctx.set(0, 200, 0, cell);
        assert_eq!(
            super::can_place(reg, &mut ctx, dripleaf, 0, 200, 0),
            expected,
            "small_dripleaf on {support} under {}",
            reg.name_of(cell)
        );
    }
}

/// `VinesFeature.place` (`edu.a(ecg)`): with the origin air, the first direction in
/// `Direction.values()` order - DOWN skipped, so UP first - whose neighbour satisfies
/// `VineBlock.canSupportAtFace` gets `minecraft:vine` with that direction's property set, and the
/// feature writes exactly **one** block.
///
/// The port hung a four-block column and only from horizontal neighbours, which is not this
/// version's rule: in the lush caves the ceiling-anchored `vine[up=true]` cells were missing
/// (measured at (78,-2,-497) in chunk (5,-32)), and each missing vine let a later
/// `moss_patch_ceiling` column pass its `isAir` test that vanilla refuses.
#[test]
fn vines_attach_to_the_first_support_in_direction_values_order() {
    let reg = breg().unwrap();
    let stone = reg.state_of("minecraft:stone", None);
    let vine = reg.state_of("minecraft:vine", None);
    let mut g = Gen::load(0, crate::name_to_state).unwrap();
    let mut ctx = context(&mut g);
    // ceiling: UP is tested before any horizontal direction.
    ctx.set(0, 201, 0, stone);
    gen_cf(&Cf::Vines, freg(), &mut ctx, &mut Xor::seed_worldgen(3), (0, 200, 0));
    assert_eq!(
        reg.with_prop(vine, "up", "true"),
        ctx.get(0, 200, 0),
        "a ceiling support must write vine[up=true]"
    );
    // one block only: the old column model also wrote 199/198/197.
    for y in [199, 198, 197] {
        assert_eq!(0, ctx.get(0, y, 0), "the vine must not hang down ({y})");
    }
    // no support at all writes nothing.
    let mut g = Gen::load(0, crate::name_to_state).unwrap();
    let mut ctx = context(&mut g);
    gen_cf(&Cf::Vines, freg(), &mut ctx, &mut Xor::seed_worldgen(3), (0, 200, 0));
    assert_eq!(0, ctx.get(0, 200, 0));
    // two horizontal supports on opposite sides: NORTH comes first in Direction.values().
    let mut g = Gen::load(0, crate::name_to_state).unwrap();
    let mut ctx = context(&mut g);
    ctx.set(0, 200, -1, stone);
    ctx.set(0, 200, 1, stone);
    gen_cf(&Cf::Vines, freg(), &mut ctx, &mut Xor::seed_worldgen(3), (0, 200, 0));
    assert_eq!(reg.with_prop(vine, "north", "true"), ctx.get(0, 200, 0));
}

/// `VegetationPatchFeature.placeGround` returns "the column joins the vegetation set" unless the
/// **first** depth iteration meets a non-replaceable block that is not already the patch's own
/// ground. A column whose ground already equals `ground_state` therefore still contributes a
/// surface point and still receives a vegetation roll.
///
/// The port gated the set on "wrote at least one block", which dropped exactly those columns:
/// measured on chunk (5,-32), `moss_patch` stopped one roll short of native (its draw 124 is
/// native's `nextFloat` and the port's next attempt's `nextInt(16)`) and every moss/clay cell of
/// that pass shifted with it.
#[test]
fn vegetation_patch_keeps_a_column_whose_ground_is_already_the_patch_block() {
    let reg = breg().unwrap();
    let dirt = reg.state_of("minecraft:dirt", None);
    let grass = reg.state_of("minecraft:short_grass", None);
    let mut g = Gen::load(0, crate::name_to_state).unwrap();
    // The column's ground already IS the patch's ground_state, so the depth loop writes nothing.
    g.feat_write(0, 199, 0, dirt);
    let mut ctx = context(&mut g);
    let cf = Cf::VegPatch {
        depth: p_ip(&J::Num(1.0)).unwrap(),
        extra_bottom: 0.0,
        extra_edge: 0.0,
        replaceable: "minecraft:moss_replaceable".to_owned(),
        ground: Bsp::Simple(dirt),
        floor: true,
        chance: 1.0,
        vfeat: Box::new(super::Pf {
            feat: super::FRef::Inline(Box::new(Cf::SimpleBlock {
                to: Bsp::Simple(grass),
                on: vec![],
                in_: vec![],
                under: vec![],
            })),
            mods: Vec::new(),
            name: "inline short_grass".to_owned(),
        }),
        xz: p_ip(&J::Num(0.0)).unwrap(),
        vrange: 5,
        waterlog: false,
    };
    gen_cf(&cf, freg(), &mut ctx, &mut Xor::seed_worldgen(11), (0, 200, 0));
    assert_eq!(
        grass,
        ctx.get(0, 200, 0),
        "a column whose ground is already the patch block still gets a vegetation roll"
    );
}

/// `UnderwaterMagmaFeature` internals that the port had wrong, each one measurable on its own.
///
/// `Column.scan` moves `i < floor_search_range` times and then tests the **moved** position, so a
/// floor four blocks below a five-range origin is still a floor; `isValidPosition` tests the cell,
/// the cell below and the four horizontal neighbours - never the cell above.
#[test]
fn underwater_magma_scans_five_positions_and_ignores_the_cell_above() {
    let reg = breg().unwrap();
    let stone = reg.state_of("minecraft:stone", None);
    let water = reg.state_of("minecraft:water", None);
    let magma = reg.state_of("minecraft:magma_block", None);
    let mut g = Gen::load(0, crate::name_to_state).unwrap();
    // water at 200..197, floor at 196: the fifth position, which the port used to give up on.
    for y in 197..=200 {
        g.feat_write(0, y, 0, water);
    }
    g.feat_write(0, 196, 0, stone);
    g.feat_write(0, 195, 0, stone);
    for (dx, dz) in [(1, 0), (-1, 0), (0, 1), (0, -1)] {
        g.feat_write(dx, 196, dz, stone);
    }
    // (0,197,0) is water: vanilla does not look at the cell above, so the floor still fills.
    let mut ctx = context(&mut g);
    let cf = Cf::UnderMagma {
        floor: 5,
        prob: 1.0,
        radius: 0,
    };
    gen_cf(&cf, freg(), &mut ctx, &mut Xor::seed_worldgen(5), (0, 200, 0));
    assert_eq!(magma, ctx.get(0, 196, 0));
    // a floor five below a five-range origin is out of reach (four moves).
    let mut g = Gen::load(0, crate::name_to_state).unwrap();
    for y in 196..=200 {
        g.feat_write(0, y, 0, water);
    }
    g.feat_write(0, 195, 0, stone);
    g.feat_write(0, 194, 0, stone);
    for (dx, dz) in [(1, 0), (-1, 0), (0, 1), (0, -1)] {
        g.feat_write(dx, 195, dz, stone);
    }
    let mut ctx = context(&mut g);
    gen_cf(&cf, freg(), &mut ctx, &mut Xor::seed_worldgen(5), (0, 200, 0));
    assert_eq!(stone, ctx.get(0, 195, 0));
}

/// `BlockPos.betweenClosedStream` walks x fastest, then y, then z, and
/// `UnderwaterMagmaFeature` draws one `nextFloat` per cell in that order before the enclosure
/// test. Binding the draws to cells in any other order moves magma diagonally (measured: the
/// taiga 3x3 had magma at (69..70,19,-501) where the reference has it at (71,19,-502..-503)).
#[test]
fn underwater_magma_binds_its_probability_draws_in_betweenclosed_order() {
    use std::collections::HashMap;
    let reg = breg().unwrap();
    let stone = reg.state_of("minecraft:stone", None);
    let water = reg.state_of("minecraft:water", None);
    let magma = reg.state_of("minecraft:magma_block", None);
    let (fy, r, prob) = (196, 1, 0.5);
    let seed = 17;
    let mut world: HashMap<(i32, i32, i32), u32> = HashMap::new();
    for y in 197..=200 {
        world.insert((0, y, 0), water);
    }
    // Everything the cube and its shell can read is stone, so the only water is that column.
    for dy in -2..=2 {
        for dx in -2..=2 {
            for dz in -2..=2 {
                world.insert((dx, fy + dy, dz), stone);
            }
        }
    }
    for y in 197..=200 {
        world.insert((0, y, 0), water);
    }
    let wet = |w: &HashMap<(i32, i32, i32), u32>, p: (i32, i32, i32)| {
        let st = *w.get(&p).unwrap_or(&0);
        reg.is_air(st) || reg.name_of(st) == "minecraft:water"
    };
    let valid = |w: &HashMap<(i32, i32, i32), u32>, p: (i32, i32, i32)| {
        !wet(w, p)
            && !wet(w, (p.0, p.1 - 1, p.2))
            && [(1, 0), (-1, 0), (0, 1), (0, -1)]
                .iter()
                .all(|(dx, dz)| !wet(w, (p.0 + dx, p.1, p.2 + dz)))
    };
    // `BlockPos.betweenClosed`: x fastest, then y, then z.
    let between_closed = |()| {
        let mut cells = Vec::new();
        for dz in -r..=r {
            for dy in -r..=r {
                for dx in -r..=r {
                    cells.push((dx, fy + dy, dz));
                }
            }
        }
        cells
    };
    let pick = |cells: &[(i32, i32, i32)]| {
        let mut draws = Xor::seed_worldgen(seed);
        cells
            .iter()
            .copied()
            .filter(|p| draws.next_f32() < prob && valid(&world, *p))
            .collect::<Vec<_>>()
    };
    let expected = pick(&between_closed(()));
    let mut by_dz = Vec::new();
    for dx in -r..=r {
        for dy in -r..=r {
            for dz in -r..=r {
                by_dz.push((dx, fy + dy, dz));
            }
        }
    }
    assert_ne!(
        expected,
        pick(&by_dz),
        "the fixture must distinguish the two draw orders"
    );

    let mut g = Gen::load(0, crate::name_to_state).unwrap();
    for (p, st) in &world {
        g.feat_write(p.0, p.1, p.2, *st);
    }
    let mut ctx = context(&mut g);
    let cf = Cf::UnderMagma {
        floor: 5,
        prob,
        radius: r,
    };
    gen_cf(&cf, freg(), &mut ctx, &mut Xor::seed_worldgen(seed), (0, 200, 0));
    for p in between_closed(()) {
        // A cell the probability refuses keeps whatever the world holds (the water column).
        let want = if expected.contains(&p) {
            magma
        } else {
            world.get(&p).copied().unwrap_or(0)
        };
        assert_eq!(
            want,
            ctx.get(p.0, p.1, p.2),
            "magma at {p:?} (expected set {expected:?})"
        );
    }
}

#[test]
fn normal_cache_identity_survives_slot_reuse() {
    use crate::generation_math::normal_new_amps;
    let mut slot = Box::new(normal_new_amps(&mut Xor::seed(0), -4, &[1.0]));
    let address = &*slot as *const _ as usize;
    let first = slot.get(-982.0, -35.0, 520.0);
    let next = normal_new_amps(&mut Xor::seed(1), -4, &[1.0]);
    let expected = next.get(-982.0, -35.0, 520.0);
    assert_ne!(first.to_bits(), expected.to_bits());
    *slot = next;
    assert_eq!(&*slot as *const _ as usize, address, "reuse the same slot");
    assert_eq!(slot.get(-982.0, -35.0, 520.0).to_bits(), expected.to_bits());
}

#[test]
fn normal_noise_legacy_source_matches_native_create() {
    let cases: &[(i32, &[f64])] = &[
        (-4, &[1.0]),
        (-2, &[1.0, 0.5]),
        (-3, &[1.0, 0.0, 0.25]),
        (-2, &[0.0, 1.0]),
        (1, &[1.0]),
        (-2, &[0.0, 0.0]),
    ];
    let mut count = 0;
    for row in include_str!("../tests/data/normal_noise_legacy_source_1211.tsv").lines() {
        let f: Vec<_> = row.split('\t').collect();
        let (first, amps) = cases[f[1].parse::<usize>().unwrap()];
        let n = normal_new_l(f[0].parse().unwrap(), first, amps);
        let got = n.get(
            f[2].parse().unwrap(),
            f[3].parse().unwrap(),
            f[4].parse().unwrap(),
        );
        assert_eq!(
            got.to_bits(),
            u64::from_str_radix(f[5], 16).unwrap(),
            "native NormalNoise.create: {row}"
        );
        count += 1;
    }
    assert_eq!(count, 168);
}

/// Native OreFeature.place queried these 121 OCEAN_FLOOR_WG columns for
/// (-1003,71,513), then skipped its vein. A neighbour's canopy must not cause
/// the port to consume the 17 sphere-size draws that vanilla skipped.
#[test]
fn ore_uses_frozen_worldgen_floor() {
    let mut g = Gen::load(0, crate::name_to_state).expect("data pack");
    let reg = breg().expect("blocks");
    let fr = freg();
    let mut ctx = context(&mut g);
    // The actual failing trace read an earlier neighbour's oak leaf at y=70.
    ctx.set(-1003, 70, 513, reg.state_of("minecraft:oak_leaves", None));
    assert_eq!(ctx.hm_oc(-1003, 513), 71, "live floor sees the canopy");
    let mut columns = 0;
    for row in include_str!("../tests/data/ore_floor_wg_1211.tsv").lines() {
        let v: Vec<i32> = row.split('\t').map(|s| s.parse().unwrap()).collect();
        assert_eq!(ctx.hm_oc_wg(v[0], v[1]), v[2], "native WG floor at {row}");
        columns += 1;
    }
    assert_eq!(columns, 121);
    let mut expected = Xor::seed(0);
    expected.next_f32();
    expected.next_bound(3);
    expected.next_bound(3);
    let mut rng = Xor::seed(0);
    let ore = Cf::Ore {
        size: 17,
        discard: 0.5,
        targets: vec![(RT::True, reg.state_of("minecraft:coal_ore", None))],
        scattered: false,
    };
    gen_cf(&ore, fr, &mut ctx, &mut rng, (-1003, 71, 513));
    assert_eq!(
        rng.next_long(),
        expected.next_long(),
        "native early-out RNG"
    );
    assert_eq!(ctx.wr.len(), 1, "vein must not run above the frozen floor");
}

/// The actual patch_grass_forest origins from neighbouring chunk (-64,33).
/// Its 388 typed draws matched; only the parsed WORLD_SURFACE_WG height was
/// wrong (+5 from the canopy), losing a grass cell in the target chunk.
#[test]
fn grass_patch_keeps_worldgen_heightmap_through_the_modifier() {
    let mut g = Gen::load(0, crate::name_to_state).expect("data pack");
    let reg = breg().unwrap();
    let mut ctx = context(&mut g);
    let modifier = crate::generation_providers::p_mod(
        &jparse(br#"{"type":"minecraft:heightmap","heightmap":"WORLD_SURFACE_WG"}"#).unwrap(),
    )
    .unwrap();
    let pf = super::Pf {
        feat: super::FRef::Name(String::new()),
        mods: vec![],
        name: String::new(),
    };
    let mut rows = 0;
    for row in include_str!("../tests/data/grass_patch_floor_wg_1211.tsv").lines() {
        let v: Vec<i32> = row.split('\t').map(|s| s.parse().unwrap()).collect();
        ctx.set(
            v[0],
            v[1] + 4,
            v[2],
            reg.state_of("minecraft:oak_leaves", None),
        );
        assert_eq!(ctx.hm_ws(v[0], v[2]), v[1] + 5, "live canopy height");
        let mut out = Vec::new();
        super::mod_positions(
            &modifier,
            freg(),
            &mut ctx,
            &mut Xor::seed(0),
            (v[0], 0, v[2]),
            &pf,
            &mut out,
        );
        assert_eq!(out, vec![(v[0], v[1], v[2])], "native patch origin: {row}");
        rows += 1;
    }
    assert_eq!(rows, 2);
}

#[test]
fn multiface_cells_and_following_rng_match_native() {
    let reg = breg().unwrap();
    let air = reg.state_of("minecraft:air", None);
    let stone = reg.state_of("minecraft:stone", None);
    let water = reg.state_of("minecraft:water", None);
    let flowing = reg.with_prop(water, "level", "8");
    let leaves = reg.state_of("minecraft:oak_leaves", None);
    // The extended piston: its shape is the head and the base, so it occludes as a cube without
    // being a solid render. The default (retracted) piston is a plain full cube.
    let piston = reg.state_of(
        "minecraft:piston",
        Some(&jparse(br#"{"extended":"true"}"#).expect("props")),
    );
    let slab = reg.state_of("minecraft:oak_slab", None);
    let lichen = reg.state_of("minecraft:glow_lichen", None);
    let mut seen = std::collections::HashSet::new();
    let mut spread_vectors = 0;
    for row in include_str!("../tests/data/multiface_1211.tsv").lines() {
        let f: Vec<_> = row.split('\t').collect();
        assert_eq!(f.len(), 7, "{row}");
        let kind: usize = f[0].parse().unwrap();
        assert!(kind < 10);
        let seed = f[1].parse::<i64>().unwrap() as u64;
        let chance: f32 = f[2].parse().unwrap();
        assert!(
            seen.insert((kind, seed, chance.to_bits())),
            "duplicate native case"
        );
        let mut g = Gen::load(0, crate::name_to_state).unwrap();
        // Native asserts that every world read stays inside this controlled volume.
        for x in -4i32..=4 {
            for dy in -4i32..=4 {
                for z in -4i32..=4 {
                    let state = if kind == 9 && (x, dy, z) == (0, 0, 0) {
                        lichen
                    } else if kind == 0 || kind == 7 {
                        if (x, dy, z) == (0, 0, 0) {
                            air
                        } else {
                            stone
                        }
                    } else if x
                        == match kind {
                            2 => 2,
                            3 => 3,
                            _ => 1,
                        }
                    {
                        match kind {
                            5 => leaves,
                            6 => slab,
                            _ => stone,
                        }
                    } else {
                        match kind {
                            4 => water,
                            8 => flowing,
                            _ => air,
                        }
                    };
                    g.feat_write(x, 200 + dy, z, state);
                }
            }
        }
        let on = match kind {
            5 => "oak_leaves",
            6 => "oak_slab",
            _ => "stone",
        };
        let floor = if kind == 7 {
            ",\"can_place_on_floor\":true"
        } else {
            ""
        };
        let json = format!(
            r#"{{"type":"multiface_growth","block":"minecraft:glow_lichen","search_range":20,"can_place_on_ceiling":true,"can_place_on_wall":true,"chance_of_spreading":{chance},"can_be_placed_on":["minecraft:{on}"]{floor}}}"#
        );
        let cf = super::p_cf(&jparse(json.as_bytes()).unwrap()).unwrap();
        let mut ctx = context(&mut g);
        let mut rng = Xor::seed_worldgen(seed);
        gen_cf(&cf, freg(), &mut ctx, &mut rng, (0, 200, 0));
        assert_eq!(
            !ctx.wr.is_empty(),
            f[3].parse::<bool>().unwrap(),
            "placed: {row}"
        );
        assert_eq!(
            ctx.wr.len(),
            f[4].parse::<usize>().unwrap(),
            "writes: {row}"
        );
        spread_vectors += usize::from(ctx.wr.len() > 1);
        let mut cells = Vec::new();
        for (&(x, y, z), &state) in &ctx.wr {
            assert_eq!(reg.name_of(state), "minecraft:glow_lichen");
            let mut mask = 0u8;
            for (i, p) in [
                "down",
                "up",
                "north",
                "south",
                "west",
                "east",
                "waterlogged",
            ]
            .iter()
            .enumerate()
            {
                if reg.get_prop(state, p).as_deref() == Some("true") {
                    mask |= 1 << i;
                }
            }
            cells.push(format!("{x},{},{z}:{mask}\n", y - 200));
        }
        cells.sort();
        let hash: String = crate::generation_math::md5(cells.concat().as_bytes())
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        assert_eq!(hash, f[6], "cell/faces: {row}");
        assert_eq!(
            rng.next_i64(),
            f[5].parse::<i64>().unwrap(),
            "following RNG: {row}"
        );
    }
    assert_eq!(seen.len(), 120);
    assert!(spread_vectors > 0, "exercise spreading into another cell");
}

#[test]
fn matching_fluids_reads_the_fluid_state_not_the_block_name() {
    let mut g = Gen::load(0, crate::name_to_state).unwrap();
    let reg = breg().unwrap();
    let water = reg.state_of("minecraft:water", None);
    let lava = reg.state_of("minecraft:lava", None);
    let stairs = reg.state_of("minecraft:oak_stairs", None);
    let mut ctx = context(&mut g);
    for (state, expected) in [
        (reg.state_of("minecraft:stone", None), "minecraft:empty"),
        (water, "minecraft:water"),
        (
            reg.with_prop(water, "level", "8"),
            "minecraft:flowing_water",
        ),
        (reg.state_of("minecraft:seagrass", None), "minecraft:water"),
        (
            reg.state_of("minecraft:tall_seagrass", None),
            "minecraft:water",
        ),
        (reg.state_of("minecraft:kelp", None), "minecraft:water"),
        (
            reg.with_prop(stairs, "waterlogged", "true"),
            "minecraft:water",
        ),
        (
            reg.with_prop(stairs, "waterlogged", "false"),
            "minecraft:empty",
        ),
        (lava, "minecraft:lava"),
        (reg.with_prop(lava, "level", "1"), "minecraft:flowing_lava"),
    ] {
        ctx.set(0, 200, 0, state);
        for fluid in [
            "minecraft:empty",
            "minecraft:water",
            "minecraft:flowing_water",
            "minecraft:lava",
            "minecraft:flowing_lava",
        ] {
            let json = format!(r#"{{"type":"matching_fluids","fluids":"{fluid}"}}"#);
            let predicate =
                crate::generation_providers::p_pred(&jparse(json.as_bytes()).unwrap()).unwrap();
            assert_eq!(
                test_pred(&predicate, reg, &mut ctx, 0, 200, 0),
                fluid == expected,
                "state={state}, wanted={fluid}"
            );
        }
    }
}

#[test]
fn surface_relative_filter_keeps_its_heightmap_and_long_bounds() {
    let mut g = Gen::load(0, crate::name_to_state).unwrap();
    let reg = breg().unwrap();
    let mut ctx = context(&mut g);
    let (x, z) = (-1003, 513);
    let floor = ctx.hm_oc_wg(x, z);
    ctx.set(x, 200, z, reg.state_of("minecraft:stone", None));
    assert_eq!(ctx.hm_ws(x, z), 201);
    let pf = super::Pf {
        feat: super::FRef::Name(String::new()),
        mods: vec![],
        name: String::new(),
    };
    for (min, max, y, accept) in [
        (i32::MIN, -13, floor - 13, true),
        (i32::MIN, -13, floor - 12, false),
        (i32::MAX, i32::MAX, i32::MAX, false),
    ] {
        let json = format!(
            r#"{{"type":"surface_relative_threshold_filter","heightmap":"OCEAN_FLOOR_WG","min_inclusive":{min},"max_inclusive":{max}}}"#
        );
        let modifier =
            crate::generation_providers::p_mod(&jparse(json.as_bytes()).unwrap()).unwrap();
        let mut out = Vec::new();
        super::mod_positions(
            &modifier,
            freg(),
            &mut ctx,
            &mut Xor::seed(0),
            (x, y, z),
            &pf,
            &mut out,
        );
        assert_eq!(!out.is_empty(), accept, "{json}, y={y}");
    }
}

#[test]
fn wg_heightmaps_stay_frozen_while_live_heightmaps_follow_writes() {
    use crate::block_registry::HeightmapKind as H;
    let reg = breg().unwrap();
    for warm in [false, true] {
        let mut g = Gen::load(0, crate::name_to_state).expect("data pack");
        let kinds = [H::WorldSurfaceWg, H::OceanFloorWg];
        let expected = {
            let mut baseline = context(&mut g);
            kinds.map(|kind| baseline.heightmap(kind, 0, 0))
        };
        let mut ctx = context(&mut g);
        if warm {
            for (kind, height) in kinds.into_iter().zip(expected) {
                assert_eq!(ctx.heightmap(kind, 0, 0), height);
            }
        }
        // Remove the old terrain tops and add a new feature far above them.
        for height in expected {
            ctx.set(0, height - 1, 0, reg.state_of("minecraft:air", None));
        }
        ctx.set(0, 240, 0, reg.state_of("minecraft:stone", None));
        for (kind, height) in kinds.into_iter().zip(expected) {
            assert_eq!(ctx.heightmap(kind, 0, 0), height, "{kind:?}, warm={warm}");
        }
        assert_eq!(ctx.heightmap(H::WorldSurface, 0, 0), 241);
        assert_eq!(ctx.heightmap(H::OceanFloor, 0, 0), 241);
    }
}

#[test]
fn heightmap_types_keep_their_predicates_and_removed_tops() {
    use crate::block_registry::HeightmapKind as H;
    let reg = breg().unwrap();
    for (name, expected) in [
        ("minecraft:short_grass", [true, false, false, false]),
        ("minecraft:water", [true, false, true, true]),
        ("minecraft:oak_leaves", [true, true, true, false]),
        ("minecraft:stone", [true, true, true, true]),
    ] {
        let state = reg.state_of(name, None);
        for (kind, counts) in [
            H::WorldSurface,
            H::OceanFloor,
            H::MotionBlocking,
            H::MotionBlockingNoLeaves,
        ]
        .into_iter()
        .zip(expected)
        {
            assert_eq!(reg.heightmap_counts(state, kind), counts, "{name} {kind:?}");
        }
    }
    assert!(!reg.is_solid_render(reg.state_of("minecraft:tinted_glass", None)));
    let mut g = Gen::load(0, crate::name_to_state).expect("data pack");
    let mut ctx = context(&mut g);
    for (y, name) in [
        (220, "stone"),
        (221, "oak_leaves"),
        (222, "water"),
        (223, "short_grass"),
    ] {
        ctx.set(0, y, 0, reg.state_of(&format!("minecraft:{name}"), None));
    }
    for (kind, expected) in [
        (H::WorldSurface, 224),
        (H::OceanFloor, 222),
        (H::MotionBlocking, 223),
        (H::MotionBlockingNoLeaves, 223),
    ] {
        assert_eq!(ctx.heightmap(kind, 0, 0), expected, "{kind:?}");
    }
    // A maximum is a scan bound, not a permanent answer after another writer.
    ctx.set(0, 223, 0, 0);
    ctx.set(0, 222, 0, 0);
    assert_eq!(ctx.heightmap(H::WorldSurface, 0, 0), 222);
    assert_eq!(ctx.heightmap(H::MotionBlockingNoLeaves, 0, 0), 221);
    ctx.set(0, 221, 0, 0);
    assert_eq!(ctx.hm_oc(0, 0), 221);
    assert_eq!(ctx.hm_ws(0, 0), 221);
    for name in [
        "WORLD_SURFACE_WG",
        "WORLD_SURFACE",
        "OCEAN_FLOOR_WG",
        "OCEAN_FLOOR",
        "MOTION_BLOCKING",
        "MOTION_BLOCKING_NO_LEAVES",
    ] {
        let parsed = crate::generation_providers::p_mod(
            &jparse(format!(r#"{{"type":"heightmap","heightmap":"{name}"}}"#).as_bytes()).unwrap(),
        )
        .unwrap();
        assert!(
            matches!(parsed, crate::generation_providers::PM::Heightmap(h) if h == H::parse(name).unwrap())
        );
    }
    assert!(crate::generation_providers::p_mod(
        &jparse(br#"{"type":"heightmap","heightmap":"UNKNOWN_FLOOR"}"#).unwrap()
    )
    .is_none());
}

/// Pin how much randomness a provider consumed, without pinning platform libm's
/// last bit: draw one Gaussian (so its spare is populated and later consumed),
/// then record two raw draws. A provider that draws a different number of times
/// moves the following longs and changes the fixture.
fn follow(trace: &mut Vec<u8>, r: &mut Xor) {
    let _ = r.next_gauss();
    trace.extend_from_slice(&r.next_long().to_le_bytes());
    trace.extend_from_slice(&r.next_long().to_le_bytes());
}

#[test]
fn kelp_feature_grows_up_from_the_ocean_floor_and_places_one_head() {
    let reg = breg().unwrap();
    let mut g = Gen::load(0, crate::name_to_state).expect("datapack");
    let mut ctx = context(&mut g);
    let (x, z) = (1280, 0); // sampled as lukewarm_ocean for seed 0
    let base = ctx.hm_oc(x, z);
    let sand = reg.state_of("minecraft:sand", None);
    let water = reg.state_of("minecraft:water", None);
    ctx.set(x, base - 1, z, sand);
    for y in base..=base + 20 {
        ctx.set(x, y, z, water);
    }

    let mut rng = Xor::seed(0);
    gen_cf(&Cf::Kelp, freg(), &mut ctx, &mut rng, (x, 0, z));

    let plant = reg.state_of("minecraft:kelp_plant", None);
    let placed_heads: Vec<_> = ctx
        .wr
        .iter()
        .filter_map(|(&(px, y, pz), &st)| {
            (px == x && pz == z && reg.name_of(st) == "minecraft:kelp").then_some((y, st))
        })
        .collect();
    assert_eq!(
        placed_heads.len(),
        1,
        "KelpFeature must place exactly one terminal head"
    );
    let (top, head_state) = placed_heads[0];
    let age = reg
        .get_prop(head_state, "age")
        .unwrap()
        .parse::<u8>()
        .unwrap();
    assert!(
        (20..=23).contains(&age),
        "head age must come from nextInt(4)+20"
    );
    assert_eq!(reg.name_of(ctx.get(x, top, z)), "minecraft:kelp");
    assert!(
        top > base,
        "the kelp head must be above its OCEAN_FLOOR origin"
    );
    for y in base..top {
        assert_eq!(
            ctx.get(x, y, z),
            plant,
            "kelp stem must be continuous at y={y}"
        );
    }
}

#[test]
fn kelp_stops_at_the_water_surface_and_replaces_the_last_plant_with_a_head() {
    let reg = breg().unwrap();
    let mut g = Gen::load(0, crate::name_to_state).expect("datapack");
    let mut ctx = context(&mut g);
    let (x, z) = (1280, 0);
    let base = ctx.hm_oc(x, z);
    ctx.set(x, base - 1, z, reg.state_of("minecraft:sand", None));
    let water = reg.state_of("minecraft:water", None);
    ctx.set(x, base, z, water);
    ctx.set(x, base + 1, z, water);
    ctx.set(x, base + 2, z, 0);

    let mut rng = Xor::seed(0);
    gen_cf(&Cf::Kelp, freg(), &mut ctx, &mut rng, (x, 0, z));

    assert_eq!(reg.name_of(ctx.get(x, base, z)), "minecraft:kelp");
    assert_eq!(reg.name_of(ctx.get(x, base + 1, z)), "minecraft:water");
    let age = reg
        .get_prop(ctx.get(x, base, z), "age")
        .unwrap()
        .parse::<u8>()
        .unwrap();
    assert!((20..=23).contains(&age));
}

#[test]
fn kelp_places_its_head_when_the_column_is_one_water_cell_deep() {
    // `KelpFeature.place` moves the position up on *every* iteration - `pos.move(UP)`
    // sits outside its if/else - so a base cell that is water with air above it
    // rejects at i=0, then rejects again at i=1 as the head branch, and the head
    // lands on the base cell itself. A loop that only advances on a placement
    // examines the base cell twice and writes the head one cell lower, into the
    // floor, where `canSurvive` refuses it and nothing is placed at all.
    let reg = breg().unwrap();
    let mut g = Gen::load(0, crate::name_to_state).expect("datapack");
    let mut ctx = context(&mut g);
    let (x, z) = (1280, 0);
    let base = ctx.hm_oc(x, z);
    ctx.set(x, base - 1, z, reg.state_of("minecraft:sand", None));
    ctx.set(x, base, z, reg.state_of("minecraft:water", None));
    ctx.set(x, base + 1, z, 0);

    let mut rng = Xor::seed(0);
    gen_cf(&Cf::Kelp, freg(), &mut ctx, &mut rng, (x, 0, z));

    let written: Vec<_> = ctx
        .wr
        .iter()
        .filter(|(&(px, _, pz), &st)| {
            px == x
                && pz == z
                && matches!(reg.name_of(st), "minecraft:kelp" | "minecraft:kelp_plant")
        })
        .map(|(&(_, y, _), &st)| (y, reg.name_of(st).to_string()))
        .collect();
    assert_eq!(
        written,
        vec![(base, "minecraft:kelp".to_string())],
        "one head on the base cell: no stem above it, nothing written into the floor"
    );
}

#[test]
fn provider_values_and_following_rng_match_pre_extraction() {
    let mut trace = Vec::new();
    let ints = [
        "3",
        r#"{"type":"constant","value":-7}"#,
        r#"{"type":"uniform","min_inclusive":-3,"max_inclusive":7}"#,
        r#"{"type":"uniform","min_inclusive":4,"max_inclusive":4}"#,
        r#"{"type":"biased_to_bottom","min_inclusive":-3,"max_inclusive":7}"#,
        r#"{"type":"very_biased_to_bottom","min_inclusive":-3,"max_inclusive":7,"inner":2}"#,
        r#"{"type":"clamped","source":{"type":"uniform","min":-10,"max":10},"min_inclusive":-2,"max_inclusive":2}"#,
        r#"{"type":"trapezoid","min":-6,"max":8,"plateau":2}"#,
        r#"{"type":"trapezoid","min":2,"max":3,"plateau":4}"#,
        r#"{"type":"clamped_normal","mean":2,"deviation":3,"min_inclusive":-4,"max_inclusive":6}"#,
        r#"{"type":"weighted_list","distribution":[{"weight":-1,"data":99},{"weight":1,"data":4},{"weight":3,"data":{"type":"uniform","min":8,"max":12}}]}"#,
        r#"{"type":"weighted_list","distribution":[]}"#,
    ];
    for seed in [0, 7, u64::MAX] {
        for text in ints {
            let p = p_ip(&jparse(text.as_bytes()).unwrap()).unwrap();
            let mut r = Xor::seed(seed);
            for _ in 0..4 {
                trace.extend_from_slice(&p.get(&mut r).to_le_bytes());
                follow(&mut trace, &mut r);
            }
        }
        for text in [
            r#"{"type":"constant","value":{"above_bottom":5}}"#,
            r#"{"type":"uniform","min_inclusive":{"absolute":-12},"max_inclusive":{"below_top":3}}"#,
            r#"{"type":"trapezoid","min_inclusive":{"above_bottom":6},"max_inclusive":{"below_top":7}}"#,
            r#"{"type":"very_biased_to_bottom","min_inclusive":-3,"max_inclusive":15}"#,
            r#"{"type":"trapezoid","min_inclusive":8,"max_inclusive":2}"#,
            r#"{"type":"very_biased_to_bottom","min_inclusive":8,"max_inclusive":2}"#,
        ] {
            let p = p_hp(&jparse(text.as_bytes()).unwrap()).unwrap();
            let mut r = Xor::seed(seed);
            for _ in 0..4 {
                trace.extend_from_slice(&p.get(&mut r, -64, 384).to_le_bytes());
                follow(&mut trace, &mut r);
            }
        }
    }
    let reg = breg().unwrap();
    let stone = reg.state_of("stone", None);
    let dirt = reg.state_of("dirt", None);
    let water = reg.state_of("water", None);
    let log = reg.state_of("oak_log", None);
    let wheat = reg.state_of("wheat", None);
    let noise = || normal_new_l(11, -2, &[1.0, 0.5]);
    let blocks = vec![
        Bsp::Simple(stone),
        Bsp::Rotated {
            base: log,
            axis: true,
        },
        Bsp::Rotated {
            base: stone,
            axis: false,
        },
        Bsp::Weighted(vec![(-1, water), (1, dirt), (3, stone)]),
        Bsp::Weighted(vec![]),
        Bsp::Weighted(vec![(-1, dirt)]),
        Bsp::RandomInt {
            src: Box::new(Bsp::Simple(wheat)),
            prop: "age".into(),
            vals: Some(crate::generation_providers::IP::Weighted(vec![
                (1, crate::generation_providers::IP::Const(0)),
                (1, crate::generation_providers::IP::Const(3)),
                (1, crate::generation_providers::IP::Const(7)),
            ])),
        },
        Bsp::RandomInt {
            src: Box::new(Bsp::Simple(wheat)),
            prop: "age".into(),
            vals: None,
        },
        Bsp::Noise {
            n: noise(),
            scale: 0.2,
            states: vec![stone, dirt, water],
        },
        Bsp::Noise {
            n: noise(),
            scale: 0.2,
            states: vec![],
        },
        Bsp::NoiseThr {
            n: noise(),
            scale: 0.2,
            thr: f32::INFINITY,
            hchance: 1.0,
            def: stone,
            low: vec![dirt, water],
            high: vec![log],
        },
        Bsp::NoiseThr {
            n: noise(),
            scale: 0.2,
            thr: f32::INFINITY,
            hchance: 1.0,
            def: stone,
            low: vec![],
            high: vec![log],
        },
        Bsp::NoiseThr {
            n: noise(),
            scale: 0.2,
            thr: f32::NEG_INFINITY,
            hchance: 1.0,
            def: stone,
            low: vec![],
            high: vec![log, dirt],
        },
        Bsp::NoiseThr {
            n: noise(),
            scale: 0.2,
            thr: f32::NEG_INFINITY,
            hchance: 0.0,
            def: stone,
            low: vec![],
            high: vec![log],
        },
        Bsp::NoiseThr {
            n: noise(),
            scale: 0.2,
            thr: f32::NEG_INFINITY,
            hchance: 1.0,
            def: stone,
            low: vec![],
            high: vec![],
        },
        Bsp::Dual {
            n: noise(),
            scale: 0.2,
            variety: (1, 3),
            slow_scale: 0.03,
            slow_variety: (1, 2),
            states: vec![stone, dirt, water, log, wheat, 0],
        },
        Bsp::Dual {
            n: noise(),
            scale: 0.2,
            variety: (1, 3),
            slow_scale: 0.03,
            slow_variety: (1, 2),
            states: vec![],
        },
        Bsp::RuleBased {
            fb: Some(Box::new(Bsp::Simple(dirt))),
            rules: vec![
                (Pr::Not(Box::new(Pr::True)), Bsp::Simple(log)),
                (Pr::True, Bsp::Weighted(vec![(1, stone), (2, water)])),
            ],
        },
        Bsp::RuleBased {
            fb: Some(Box::new(Bsp::Simple(dirt))),
            rules: vec![],
        },
        Bsp::RuleBased {
            fb: None,
            rules: vec![],
        },
    ];
    let mut g = Gen::load(0, crate::name_to_state).unwrap();
    let mut ctx = context(&mut g);
    ctx.set(0, 100, 0, stone);
    for seed in [0, 7, u64::MAX] {
        for p in &blocks {
            let mut r = Xor::seed(seed);
            for (x, y, z) in [(0, 100, 0), (-3, 101, 2), (16, -63, -17)] {
                trace.extend_from_slice(&p.get(&mut ctx, &mut r, x, y, z).to_le_bytes());
                follow(&mut trace, &mut r);
            }
        }
    }
    for st in [stone, dirt, water, 0] {
        ctx.set(0, 100, 0, st);
        ctx.set(0, 99, 0, dirt);
        for p in [
            Pr::True,
            Pr::Blocks(vec!["minecraft:stone".into()]),
            Pr::Tag("dirt".into()),
            Pr::Fluids(vec!["minecraft:water".into()]),
            Pr::Solid,
            Pr::Replaceable,
            Pr::WouldSurvive("minecraft:oak_sapling".into()),
            Pr::InsideWB,
            Pr::HasSturdy([0, -1, 0], 1),
            Pr::Any(vec![]),
            Pr::All(vec![]),
            Pr::Any(vec![Pr::Solid, Pr::True]),
            Pr::All(vec![Pr::Solid, Pr::Not(Box::new(Pr::Replaceable))]),
        ] {
            trace.push(u8::from(test_pred(&p, reg, &mut ctx, 0, 100, 0)));
        }
    }
    for y in [
        ctx.g.min_y - 1,
        ctx.g.min_y,
        ctx.g.min_y + ctx.g.height - 1,
        ctx.g.min_y + ctx.g.height,
    ] {
        trace.push(u8::from(test_pred(&Pr::InsideWB, reg, &mut ctx, 0, y, 0)));
    }
    ctx.set(0, 319, 0, 0);
    trace.push(u8::from(test_pred(
        &Pr::Unobstructed,
        reg,
        &mut ctx,
        0,
        318,
        0,
    )));
    ctx.set(0, 319, 0, stone);
    trace.push(u8::from(test_pred(
        &Pr::Unobstructed,
        reg,
        &mut ctx,
        0,
        318,
        0,
    )));
    // A solid block several cells above must still be found: the loop has to walk,
    // not only inspect the first cell. This stays out of `trace`, so the frozen
    // fixture above is unchanged.
    let base = ctx.g.min_y;
    for y in base + 2..base + 5 {
        ctx.set(0, y, 0, 0);
    }
    ctx.set(0, base + 5, 0, stone);
    assert!(!test_pred(&Pr::Unobstructed, reg, &mut ctx, 0, base + 1, 0));
    for seed in [0, 7, u64::MAX] {
        for rt in [
            RT::True,
            RT::Tag("dirt".into()),
            RT::Block("minecraft:stone".into()),
            RT::BlockState(stone),
            RT::Any(vec![RT::BlockState(dirt), RT::True]),
            RT::All(vec![RT::True, RT::Not(Box::new(RT::BlockState(dirt)))]),
            RT::Height(90, 110),
            RT::RandBlock("minecraft:stone".into(), 0.5),
            RT::RandBlock("minecraft:dirt".into(), 1.0),
            RT::RandBlockState(stone, 0.5),
            RT::RandBlockState(dirt, 1.0),
        ] {
            let mut r = Xor::seed(seed);
            trace.push(u8::from(test_rt(&rt, reg, stone, 100, &mut r)));
            follow(&mut trace, &mut r);
        }
    }
    // `UPDATE_BASELINE=1` re-records the fixture after a *verified* correction
    // (height providers, NormalNoise and biased integer heights have independent native vectors).
    if std::env::var("UPDATE_BASELINE").is_ok() {
        std::fs::write(
            "evidence/foundation/module-layers/provider-baseline.bin",
            &trace,
        )
        .unwrap();
    }
    let expected = std::fs::read("evidence/foundation/module-layers/provider-baseline.bin")
        .expect("the pre-extraction provider/context baseline fixture");
    assert_eq!(
        trace, expected,
        "provider/predicate values or their RNG consumption changed from the pre-extraction baseline"
    );
}

#[test]
fn context_overrides_heights_bounds_and_biomes_track_live_state() {
    let mut g = Gen::load(0, crate::name_to_state).unwrap();
    let reg = breg().unwrap();
    let stone = reg.state_of("stone", None);
    let water = reg.state_of("water", None);
    let x = -1;
    let z = -1;
    let ws = g.top_ws(x, z);
    let oc = g.top_oc(x, z);
    let y = ws.max(oc) + 2;
    assert!(y + 3 < g.min_y + g.height);
    let original = g.terrain_state(x, y, z);
    let biome = g.biome_name_at(x, y, z);
    let other_biome = g.biome_name_at(16, y, 16);
    let mut ctx = context(&mut g);
    assert_eq!(ctx.get(x, y, z), original);
    assert_eq!(ctx.get(x, y, z), original);
    assert_eq!(ctx.hm_ws(x, z), ws);
    assert_eq!(ctx.hm_oc(x, z), oc);
    ctx.set(x, y, z, stone);
    assert_eq!(ctx.get(x, y, z), stone);
    assert_eq!(ctx.hm_ws(x, z), y + 1);
    assert_eq!(ctx.hm_oc(x, z), y + 1);
    ctx.set(x, y + 2, z, water);
    assert_eq!(ctx.hm_ws(x, z), y + 3);
    assert_eq!(ctx.hm_oc(x, z), y + 1);
    ctx.set(x, y + 2, z, 0);
    assert_eq!(ctx.get(x, y + 2, z), 0);
    // Removing the water top reveals the stone again, like native Heightmap.update.
    assert_eq!(ctx.hm_ws(x, z), y + 1);
    assert_eq!(ctx.hm_oc(x, z), y + 1);
    assert_eq!(ctx.biome(x, y, z), biome);
    assert_eq!(ctx.biome(x, y, z), biome);
    assert_eq!(ctx.biome(16, y, 16), other_biome);
    assert!(ctx.out_of_h(ctx.g.min_y - 1));
    assert!(!ctx.out_of_h(ctx.g.min_y));
    assert!(!ctx.out_of_h(ctx.g.min_y + ctx.g.height - 1));
    assert!(ctx.out_of_h(ctx.g.min_y + ctx.g.height));
}

/// The carver configs are read with the 1.21.1 field shapes, and two of them are
/// easy to get subtly wrong: `yScale` is a bare number in `canyon.json` (the codec
/// is `either(float, FloatProvider)`), and `replaceable` carries the `#` tag
/// marker in the file but is looked up without it.
#[test]
fn carver_configs_parse_the_1_21_1_field_shapes() {
    use crate::generation_math::Lcg;

    let reg = super::creg();
    let cave = reg.cfg.get("minecraft:cave").expect("cave config");
    assert_eq!(cave.probability, 0.15);
    assert_eq!(cave.replaceable, "minecraft:overworld_carver_replaceables");
    assert_eq!(
        cave.kind,
        super::CKind::Cave,
        "the cave type must not become another algorithm"
    );
    // y is uniform(above_bottom 8 .. absolute 180) and the world's min y is -64
    let mut r = Lcg::new(7);
    let (mut lo, mut hi) = (i32::MAX, i32::MIN);
    for _ in 0..2000 {
        let v = cave.y.sample(&mut r, -64, 384);
        lo = lo.min(v);
        hi = hi.max(v);
    }
    assert!(lo >= -56, "y never below the above_bottom anchor: {lo}");
    assert!(hi <= 180, "y never above the absolute anchor: {hi}");
    assert!(lo < 0 && hi > 100, "the range is exercised: {lo}..{hi}");

    // canyon.json's yScale is a plain number, not a provider object
    let canyon = reg.cfg.get("minecraft:canyon").expect("canyon config");
    assert_eq!(canyon.y_scale.sample(&mut Lcg::new(0)), 3.0);
    assert!(canyon.canyon.is_some(), "the canyon shape must parse");
    // and the biome list order is what the carver seed index counts
    let forest = reg.biome.get("minecraft:forest").expect("forest carvers");
    assert_eq!(
        forest,
        &vec![
            "minecraft:cave".to_string(),
            "minecraft:cave_extra_underground".to_string(),
            "minecraft:canyon".to_string()
        ]
    );
}

/// Every biome's AIR carver list, in the order the carver seed index counts,
/// against the pinned jar's own `BiomeGenerationSettings`. The list is written as
/// `{"air": [...]}` in 1.21.1, not as a bare array.
#[test]
fn carver_lists_match_the_pinned_biomes() {
    let fixture = jparse(include_bytes!(
        "../evidence/foundation/biome-carvers/vanilla.json"
    ))
    .unwrap();
    let J::Obj(biomes) = fixture.get("biomes").unwrap() else {
        panic!("biomes must be an object");
    };
    let reg = super::creg();
    let mut nonempty = 0;
    for (name, list) in biomes {
        let want: Vec<String> = list
            .as_arr()
            .expect("carver list")
            .iter()
            .map(|v| v.str().to_string())
            .collect();
        assert_eq!(reg.biome.get(name), Some(&want), "carvers for {name}");
        nonempty += usize::from(!want.is_empty());
    }
    assert_eq!(biomes.len(), 64, "pinned biome coverage changed");
    assert_eq!(nonempty, 58, "non-empty carver coverage changed");
    assert_eq!(
        reg.biome.get("minecraft:forest").map(Vec::as_slice),
        Some(
            [
                "minecraft:cave",
                "minecraft:cave_extra_underground",
                "minecraft:canyon"
            ]
            .map(str::to_string)
            .as_slice()
        )
    );
}

/// Converted packs wrote the AIR step as a bare list or string. No bundled
/// biome uses that shape any more, so pin it directly.
#[test]
fn converted_carver_lists_still_parse() {
    let list = jparse(br#"{"carvers":["minecraft:cave"]}"#).unwrap();
    assert_eq!(
        super::carver_list(&list),
        Some(vec!["minecraft:cave".to_string()])
    );
    let single = jparse(br#"{"carvers":"minecraft:canyon"}"#).unwrap();
    assert_eq!(
        super::carver_list(&single),
        Some(vec!["minecraft:canyon".to_string()])
    );
}

/// Every biome's `MobSpawnSettings` from the pinned jar: all eight categories,
/// each weighted entry, the two spawn-cost fields and the creature probability.
/// The nightly runtime uses only the supported monsters, but the whole loaded
/// table is pinned so a loader change cannot silently drop a category.
#[test]
fn biome_spawn_tables_match_the_pinned_classes() {
    let fixture = jparse(include_bytes!(
        "../evidence/foundation/biome-spawners/vanilla.json"
    ))
    .unwrap();
    let J::Obj(biomes) = fixture.get("biomes").unwrap() else {
        panic!("biomes must be an object");
    };
    let reg = super::freg();
    let mut entries = 0;
    let mut costs = 0;
    for (name, row) in biomes {
        let got = reg.spawners.get(name).expect("biome spawners");
        let J::Obj(want) = row.get("spawners").unwrap() else {
            panic!("spawners must be an object");
        };
        assert_eq!(got.len(), want.len(), "categories for {name}");
        for (category, list) in want {
            let want_list = list.as_arr().unwrap();
            let got_list = got.get(category).unwrap();
            assert_eq!(got_list.len(), want_list.len(), "{name}/{category}");
            for (g, w) in got_list.iter().zip(want_list) {
                assert_eq!(g.kind, w.get("type").unwrap().str(), "{name}/{category}");
                assert_eq!(g.min_count, w.get("min").unwrap().num() as i32);
                assert_eq!(g.max_count, w.get("max").unwrap().num() as i32);
                assert_eq!(g.weight, w.get("weight").unwrap().num() as i32);
                entries += 1;
            }
        }
        let J::Obj(want_costs) = row.get("spawn_costs").unwrap() else {
            panic!("spawn_costs must be an object");
        };
        let got_costs = reg.spawn_costs.get(name).unwrap();
        assert_eq!(got_costs.len(), want_costs.len(), "costs for {name}");
        for (kind, want_cost) in want_costs {
            let got_cost = got_costs.get(kind).unwrap();
            assert_eq!(
                got_cost.energy_budget.to_bits(),
                want_cost.get("energy_budget").unwrap().num().to_bits()
            );
            assert_eq!(
                got_cost.charge.to_bits(),
                want_cost.get("charge").unwrap().num().to_bits()
            );
            costs += 1;
        }
        let probability = reg.spawn_probability.get(name).unwrap();
        assert_eq!(
            probability.to_bits(),
            (row.get("creature_generation_probability").unwrap().num() as f32).to_bits()
        );
    }
    assert_eq!(biomes.len(), 64, "pinned biome coverage changed");
    assert_eq!(entries, 746, "weighted spawner entries changed");
    assert_eq!(costs, 5, "spawn costs changed");
    assert_eq!(
        super::choose_supported_hostile("minecraft:plains", 0),
        Some("minecraft:spider")
    );
    assert_eq!(
        super::choose_supported_hostile("minecraft:the_end", 0),
        None,
        "the End has only unsupported endermen"
    );
    assert_eq!(
        super::spawn_cost_for("minecraft:soul_sand_valley", "minecraft:enderman"),
        Some(super::SpawnCost {
            energy_budget: 0.15,
            charge: 0.7
        })
    );
}

/// A geode config whose shape is fully determined: one point at a fixed offset, no
/// noise, no crack, no alternate layer, no inner placements. The decompiled
/// `GeodeFeature` then compares `1/sqrt(distSqr + pointOffset)` against
/// `1/sqrt(layer + points / outerWallDistance.max)`, which makes every layer a
/// distance band around that one point.
const GEODE_TEST_JSON: &str = r##"{
  "type": "minecraft:geode",
  "config": {
    "blocks": {
      "filling_provider": {"type":"minecraft:simple_state_provider","state":{"Name":"minecraft:air"}},
      "inner_layer_provider": {"type":"minecraft:simple_state_provider","state":{"Name":"minecraft:amethyst_block"}},
      "alternate_inner_layer_provider": {"type":"minecraft:simple_state_provider","state":{"Name":"minecraft:budding_amethyst"}},
      "middle_layer_provider": {"type":"minecraft:simple_state_provider","state":{"Name":"minecraft:calcite"}},
      "outer_layer_provider": {"type":"minecraft:simple_state_provider","state":{"Name":"minecraft:smooth_basalt"}},
      "inner_placements": [
        {"Name":"minecraft:small_amethyst_bud","Properties":{"facing":"up","waterlogged":"false"}}
      ],
      "cannot_replace": "#minecraft:features_cannot_replace",
      "invalid_blocks": "#minecraft:geode_invalid_blocks"
    },
    "layers": {"filling": 1.7, "inner_layer": 2.2, "middle_layer": 3.2, "outer_layer": 4.2},
    "crack": {"generate_crack_chance": 0.0, "base_crack_size": 2.0, "crack_point_offset": 2},
    "use_alternate_layer0_chance": 0.0,
    "use_potential_placements_chance": 0.0,
    "placements_require_layer0_alternate": true,
    "outer_wall_distance": {"type":"minecraft:uniform","min_inclusive":1,"max_inclusive":1},
    "distribution_points": {"type":"minecraft:uniform","min_inclusive":1,"max_inclusive":1},
    "point_offset": {"type":"minecraft:uniform","min_inclusive":1,"max_inclusive":1},
    "min_gen_offset": -16,
    "max_gen_offset": 16,
    "noise_multiplier": 0.0,
    "invalid_blocks_threshold": 1
  }
}"##;

/// The four layers are concentric, and which one a cell belongs to follows from the
/// distance thresholds alone: with `d0 = points / max = 1`, a cell at squared
/// distance `d` gets `1/sqrt(d + 1)` and the layers are `1/sqrt(1.7)`,
/// `1/sqrt(2.2+1)`, `1/sqrt(3.2+1)` and `1/sqrt(4.2+1)` counting inwards. The
/// expected layer of every written cell is derived here, not read back from the
/// code, so a swapped branch, a wrong threshold, a wrong `d0` or a distance that
/// skips `pointOffset` all fail.
#[test]
fn geode_layers_are_concentric_bands_around_its_points() {
    let reg = breg().unwrap();
    let cf = super::p_cf(&jparse(GEODE_TEST_JSON.as_bytes()).expect("json"))
        .expect("the geode config must parse");
    let mut g = Gen::load(0, crate::name_to_state).expect("datapack");
    let mut ctx = context(&mut g);
    let origin = (8, 8, 8);
    let mut rng = Xor::seed(7);
    gen_cf(&cf, freg(), &mut ctx, &mut rng, origin);

    // the single point is at origin + outer_wall_distance = (9,9,9), point_offset = 1
    let (d0, fill, inner, middle, outer) = (1.0 / 1.0, 1.7f64, 2.2f64, 3.2f64, 4.2f64);
    let (d1, d2, d3, d4) = (
        1.0 / fill.sqrt(),
        1.0 / (inner + d0).sqrt(),
        1.0 / (middle + d0).sqrt(),
        1.0 / (outer + d0).sqrt(),
    );
    let expect = |x: i32, y: i32, z: i32| -> &'static str {
        let sq = ((x - 9).pow(2) + (y - 9).pow(2) + (z - 9).pow(2)) as f64;
        let v = 1.0 / (sq + 1.0).sqrt();
        if v < d4 {
            "outside"
        } else if v >= d1 {
            "minecraft:air"
        } else if v >= d2 {
            "minecraft:amethyst_block"
        } else if v >= d3 {
            "minecraft:calcite"
        } else {
            "minecraft:smooth_basalt"
        }
    };
    let mut counts: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
    let mut wrong = vec![];
    for (&(x, y, z), &st) in ctx.wr.iter() {
        let want = expect(x, y, z);
        *counts.entry(want).or_default() += 1;
        if reg.name_of(st) != want {
            wrong.push(((x, y, z), reg.name_of(st).to_string(), want));
        }
    }
    assert!(
        wrong.is_empty(),
        "{} of {} written cells are in the wrong layer, first few: {:?}",
        wrong.len(),
        ctx.wr.len(),
        &wrong[..wrong.len().min(5)]
    );
    for layer in [
        "minecraft:air",
        "minecraft:amethyst_block",
        "minecraft:calcite",
        "minecraft:smooth_basalt",
    ] {
        assert!(
            counts.get(layer).copied().unwrap_or(0) > 0,
            "layer {layer} was never placed: {counts:?}"
        );
    }
    assert_eq!(
        counts.get("outside").copied().unwrap_or(0),
        0,
        "nothing outside the outermost threshold may be written"
    );
}

/// `GEODE_TEST_JSON` with the crack and inner-placement knobs overridden, so the
/// tests below differ from the shape fixture only in the field under test.
fn geode_test_json(crack: (f64, f64), placements: Option<f64>) -> String {
    let mut s = GEODE_TEST_JSON.to_string();
    s = s.replace(
        "\"generate_crack_chance\": 0.0",
        &format!("\"generate_crack_chance\": {}", crack.0),
    );
    s = s.replace(
        "\"base_crack_size\": 2.0",
        &format!("\"base_crack_size\": {}", crack.1),
    );
    if let Some(chance) = placements {
        s = s.replace(
            "\"use_potential_placements_chance\": 0.0",
            &format!("\"use_potential_placements_chance\": {chance}"),
        );
        // `use_alternate_layer0_chance` is 0, so the alternate flag is never set and
        // a requiring config would never collect a candidate at all.
        s = s.replace(
            "\"placements_require_layer0_alternate\": true",
            "\"placements_require_layer0_alternate\": false",
        );
    }
    s
}

/// The generation box is walked in `BlockPos.betweenClosed` order, which decodes
/// its index as `x = i % width`, `y = (i / width) % height`, `z = i / (width *
/// height)`: x is the fastest axis and z the slowest. Every layer provider draws
/// from the feature RNG per written cell, so that order *is* the draw order — the
/// x-major walk this test was written against handed vanilla's draws to a
/// different cell in every geode and swapped amethyst/budding states in chunks
/// (6,7) and (-96,35) of seed 4242.
#[test]
fn geode_walks_the_generation_box_in_between_closed_order() {
    let cf = super::p_cf(&jparse(GEODE_TEST_JSON.as_bytes()).expect("json"))
        .expect("the geode config must parse");
    let mut g = Gen::load(0, crate::name_to_state).expect("datapack");
    let mut ctx = context(&mut g);
    ctx.test_writes = Some(Vec::new());
    let mut rng = Xor::seed(7);
    gen_cf(&cf, freg(), &mut ctx, &mut rng, (8, 8, 8));
    let writes = ctx.test_writes.as_ref().expect("scripted writes");
    assert!(
        writes.len() > 20,
        "the fixture must place a layer to order, got {}",
        writes.len()
    );
    let mut prev: Option<(i32, i32, i32)> = None;
    for &(p, _) in writes {
        if let Some(q) = prev {
            assert!(
                (p.2, p.1, p.0) > (q.2, q.1, q.0),
                "betweenClosed visits z slowest and x fastest, so {p:?} cannot be \
                 written after {q:?}"
            );
        }
        prev = Some(p);
    }
}

/// The crack's six-neighbour step is `FluidState.createLegacyBlock()` on the
/// neighbour, not "blank whatever is not air": a dry neighbour keeps its block —
/// the over-eager version turned the geode's own amethyst shell, calcite and the
/// surrounding deepslate into air in chunks (6,7) and (-96,35) of seed 4242 — while
/// a waterlogged solid holds *source* water and becomes a water block.
///
/// The check is a differential over the write log: the reference run over a dry
/// world must contain no neighbour write at all, and giving one *unwritten*
/// neighbour of a carved cell a waterlogged slab must add exactly that one water
/// write and change nothing else.
#[test]
fn geode_crack_resets_only_neighbours_that_hold_a_fluid() {
    let reg = breg().unwrap();
    let json = geode_test_json((1.0, 2.0), None);
    let (ox, oy, oz) = (8, 8, 8);
    let slab = reg.with_prop(
        reg.state_of("minecraft:oak_slab", None),
        "waterlogged",
        "true",
    );
    let water = reg.state_of("minecraft:water", None);
    let run = |extra: Option<(i32, i32, i32)>| -> Vec<((i32, i32, i32), u32)> {
        let cf = super::p_cf(&jparse(json.as_bytes()).expect("json"))
            .expect("the geode config must parse");
        let mut g = Gen::load(0, crate::name_to_state).expect("datapack");
        let mut ctx = context(&mut g);
        ctx.test_no_terrain = true;
        ctx.test_writes = Some(Vec::new());
        let deepslate = reg.state_of("minecraft:deepslate", None);
        for x in ox - 16..=ox + 16 {
            for y in oy - 16..=oy + 16 {
                for z in oz - 16..=oz + 16 {
                    ctx.test_base_cell((x, y, z), deepslate);
                }
            }
        }
        if let Some(at) = extra {
            ctx.test_base_cell(at, slab);
        }
        let mut rng = Xor::seed(7);
        gen_cf(&cf, freg(), &mut ctx, &mut rng, (ox, oy, oz));
        let writes = ctx.test_writes.take().expect("scripted writes");
        if let Some(at) = extra {
            assert_eq!(
                ctx.get(at.0, at.1, at.2),
                water,
                "a waterlogged neighbour holds source water, so the crack step writes \
                 the fluid's legacy block there (`WaterFluid.Source.createLegacyBlock`)"
            );
        }
        writes
    };
    let dry = run(None);
    assert!(!dry.is_empty(), "the fixture must carve something");
    // The crack carves cells; every cell it writes must be one the walk reached, so a
    // neighbour write at a cell the walk never reached is the old bug's signature.
    let dry_at: std::collections::HashSet<(i32, i32, i32)> = dry.iter().map(|&(p, _)| p).collect();
    let mut carved: Vec<(i32, i32, i32)> = dry
        .iter()
        .filter(|(_, st)| reg.name_of(*st) == "minecraft:air")
        .map(|&(p, _)| p)
        .collect();
    carved.sort_by_key(|p| (p.2, p.1, p.0));
    // A neighbour of a carved cell that the walk never wrote: outside the whole
    // geode, which is exactly where the old version injected air.
    let mut cand = None;
    for c in &carved {
        for (dx, dy, dz) in [
            (0, -1, 0),
            (0, 1, 0),
            (0, 0, -1),
            (0, 0, 1),
            (-1, 0, 0),
            (1, 0, 0),
        ] {
            let at = (c.0 + dx, c.1 + dy, c.2 + dz);
            // The walk order makes `-x/-y/-z` neighbours final; only those can show
            // the step's own value.
            if !dry_at.contains(&at) && (at.2, at.1, at.0) < (c.2, c.1, c.0) {
                cand = Some(at);
                break;
            }
        }
        if cand.is_some() {
            break;
        }
    }
    let cand = cand.expect("the fixture must carve a cell with an unwritten neighbour");
    let wet = run(Some(cand));
    let touched: Vec<_> = wet.iter().filter(|&&(p, _)| p == cand).cloned().collect();
    assert!(
        !touched.is_empty() && touched.iter().all(|&(_, st)| st == water),
        "the crack step writes the fluid neighbour at {cand:?}: {touched:?}"
    );
    let rest: Vec<_> = wet.iter().filter(|&&(p, _)| p != cand).cloned().collect();
    assert_eq!(
        rest, dry,
        "the crack step may only re-write the fluid neighbour, at {cand:?}"
    );
}

/// `BuddingAmethystBlock.canClusterGrowAtState`: air, or a *water* block with a full
/// 8-unit fluid — not "any liquid", and not a waterlogged solid. `WATERLOGGED` on
/// the crystal comes from `FluidState.isSource()`.
///
/// The assertion replays the write log against the pre-generation world, so it
/// checks the state the picker actually saw, not the final one. Under lava the only
/// air is the geode's own core, so the buds that exist there are the ones hung into
/// it; a lava cell is never a target even though it is a "liquid" with level 0.
#[test]
fn geode_hangs_crystals_only_in_air_or_a_water_source() {
    let reg = breg().unwrap();
    let json = geode_test_json((0.0, 2.0), Some(1.0));
    let cf =
        super::p_cf(&jparse(json.as_bytes()).expect("json")).expect("the geode config must parse");

    let run = |fill: u32| -> Vec<((i32, i32, i32), u32)> {
        let mut g = Gen::load(0, crate::name_to_state).expect("datapack");
        let mut ctx = context(&mut g);
        ctx.test_no_terrain = true;
        ctx.test_writes = Some(Vec::new());
        let (ox, oy, oz) = (8, 8, 8);
        for x in ox - 16..=ox + 16 {
            for y in oy - 16..=oy + 16 {
                for z in oz - 16..=oz + 16 {
                    ctx.test_base_cell((x, y, z), fill);
                }
            }
        }
        let mut rng = Xor::seed(7);
        gen_cf(&cf, freg(), &mut ctx, &mut rng, (ox, oy, oz));
        ctx.test_writes.take().expect("scripted writes")
    };
    let bud = |st: u32| {
        reg.name_of(st).ends_with("_amethyst_bud")
            || reg.name_of(st) == "minecraft:amethyst_cluster"
    };
    for (name, fill) in [
        ("lava", reg.state_of("minecraft:lava", None)),
        ("water", reg.state_of("minecraft:water", None)),
    ] {
        let writes = run(fill);
        let mut world: std::collections::HashMap<(i32, i32, i32), u32> =
            std::collections::HashMap::new();
        let mut buds = 0;
        for &(p, st) in &writes {
            if bud(st) {
                buds += 1;
                let at = world.get(&p).copied().unwrap_or(fill);
                let water_source = at == reg.state_of("minecraft:water", None);
                assert!(
                    reg.is_air(at) || water_source,
                    "{name}: a {} was hung into {} at {p:?}",
                    reg.name_of(st),
                    reg.name_of(at)
                );
                assert_eq!(
                    reg.get_prop(st, "waterlogged").as_deref(),
                    Some(if water_source { "true" } else { "false" }),
                    "{name}: WATERLOGGED follows FluidState.isSource"
                );
            }
            world.insert(p, st);
        }
        assert!(
            buds > 0,
            "{name}: the fixture must hang at least one crystal, else it pins nothing"
        );
    }
}

/// `invalid_blocks_threshold` is a guard, not a filter: the first point that lands
/// in air past the threshold abandons the whole geode, which is why an aborted one
/// leaves no trace at all.
#[test]
fn geode_abandons_itself_when_a_point_lands_in_air() {
    let reg = breg().unwrap();
    let cf = super::p_cf(&jparse(GEODE_TEST_JSON.as_bytes()).expect("json"))
        .expect("the geode config must parse");
    let mut g = Gen::load(0, crate::name_to_state).expect("datapack");
    let mut ctx = context(&mut g);
    let air = reg.state_of("minecraft:air", None);
    // the point is `origin + outer_wall_distance` = (9,9,9); make it air so the
    // first point already counts as invalid and `0 >= threshold` cannot hold
    ctx.set(9, 9, 9, air);
    let cf = match cf {
        Cf::Geode(mut g) => {
            g.invalid_blocks_threshold = 0;
            Cf::Geode(g)
        }
        _ => unreachable!("the test config is a geode"),
    };
    let mut rng = Xor::seed(7);
    gen_cf(&cf, freg(), &mut ctx, &mut rng, (8, 8, 8));
    let placed = ctx
        .wr
        .values()
        .filter(|&&st| {
            matches!(
                reg.name_of(st),
                "minecraft:amethyst_block" | "minecraft:calcite" | "minecraft:smooth_basalt"
            )
        })
        .count();
    assert_eq!(placed, 0, "an abandoned geode must place nothing");
}

/// The heightmap a feature reads is the block that is there, not the noise surface.
/// Vanilla maintains `OCEAN_FLOOR_WG` from every `setBlock` during terrain, so a
/// solid the aquifer pushes above the noise surface counts; the port's basis was
/// `ChunkCols::surface`, the density top, which reads the floor too low there.
///
/// Measured on the pinned kelp fixture (seed 0, chunk (79,-1)): this column's
/// density top is 18 while both worlds hold stone at y=33, so the floor is 34. With
/// the old basis the whole fixture region disagreed with vanilla's saved heightmap
/// on 329 of 9216 columns and agreed on none of them; with the walk it is 0 of 9216.
#[test]
fn heightmap_reads_the_floor_the_blocks_are_at_not_the_noise_surface() {
    let reg = breg().unwrap();
    let mut g = Gen::load(0, crate::name_to_state).expect("datapack");
    let mut ctx = context(&mut g);
    let (x, z) = (1264, -16);
    let stone = reg.state_of("minecraft:stone", None);
    assert_eq!(
        reg.name_of(ctx.get(x, 33, z)),
        "minecraft:stone",
        "the fixture column holds a solid at y=33"
    );
    assert!(
        ctx.hm_oc(x, z) > 33,
        "OCEAN_FLOOR must be above that solid, got {}",
        ctx.hm_oc(x, z)
    );
    assert_eq!(
        ctx.hm_oc(x, z),
        34,
        "the first free y above the floor is 34 (vanilla's saved OCEAN_FLOOR)"
    );
    // and the port's own block at the floor is above the density top it started from
    assert!(
        reg.name_of(ctx.get(x, 18, z)) != "minecraft:air",
        "the density top itself is solid, so the walk has a floor to stop at"
    );
    let _ = stone;
}

/// `minecraft:sea_pickle` (`edj`): `count` attempts, each `nextInt(8) - nextInt(8)`
/// on both horizontal axes, the OCEAN_FLOOR height at that column, then a
/// `nextInt(4) + 1` pickle count *before* the two survival tests. The count here is
/// 2 so both the placements and the draws are pinned.
const SEA_PICKLE_TEST_JSON: &str = r#"{
  "type": "minecraft:sea_pickle",
  "config": { "count": 2 }
}"#;

fn sea_pickle_cf() -> Cf {
    super::p_cf(&jparse(SEA_PICKLE_TEST_JSON.as_bytes()).expect("json"))
        .expect("the sea pickle config must parse")
}

/// The two tests are ordered after the pickle-count draw, so a rejected attempt
/// still consumes its five draws. Both halves are checked: an ocean patch where
/// both attempts land, and the generated terrain around it where neither does.
#[test]
fn sea_pickle_places_on_water_over_a_solid_top_face_and_draws_first() {
    let reg = breg().unwrap();
    let water = reg.state_of("minecraft:water", None);
    let sand = reg.state_of("minecraft:sand", None);
    let mut g = Gen::load(0, crate::name_to_state).expect("datapack");
    let mut ctx = context(&mut g);
    let (x, z) = (1280, 0); // lukewarm_ocean for seed 0

    // a flat patch: sand with water above, everywhere the offsets can reach
    for dx in -8..=8 {
        for dz in -8..=8 {
            let (px, pz) = (x + dx, z + dz);
            let fy = ctx.hm_oc_terrain(px, pz);
            ctx.set(px, fy - 1, pz, sand);
            for y in fy..=fy + 4 {
                ctx.set(px, y, pz, water);
            }
        }
    }
    let mut rng = Xor::seed(3);
    gen_cf(&sea_pickle_cf(), freg(), &mut ctx, &mut rng, (x, 0, z));

    let placed: Vec<((i32, i32, i32), u32)> = ctx
        .wr
        .iter()
        .filter(|(_, &st)| reg.name_of(st) == "minecraft:sea_pickle")
        .map(|(&p, &st)| (p, st))
        .collect();
    assert_eq!(
        placed.len(),
        2,
        "both attempts land on prepared water: {placed:?}"
    );
    for ((px, py, pz), st) in &placed {
        let pickles = reg.get_prop(*st, "pickles").expect("pickles property");
        assert!(
            (1..=4).contains(&pickles.parse::<i32>().unwrap()),
            "pickles must come from nextInt(4) + 1, got {pickles}"
        );
        assert_eq!(
            reg.get_prop(*st, "waterlogged").as_deref(),
            Some("true"),
            "the default sea pickle is waterlogged"
        );
        assert_eq!(
            *py,
            ctx.hm_oc_terrain(*px, *pz),
            "the pickle sits at the column's OCEAN_FLOOR height"
        );
        assert!(
            reg.side_solid(ctx.get(*px, py - 1, *pz), 1),
            "the block below must have a solid top face"
        );
    }

    // two attempts, five bound draws each (dx twice, dz twice, pickles once)
    let mut expect = Xor::seed(3);
    for _ in 0..10 {
        let v = expect.next_bound(8);
        let _ = v;
    }
    assert_eq!(
        rng.next_bound(1 << 30),
        expect.next_bound(1 << 30),
        "the stream must advance by exactly two attempts of five draws"
    );
}

/// The other half of the same rule: where the drawn cell is not water, nothing is
/// placed and the draws are still spent. The patch is filled with air rather than
/// left as generated - the origin is lukewarm_ocean, so its own terrain would be
/// water and the attempt would legitimately succeed.
#[test]
fn sea_pickle_skips_cells_that_are_not_water() {
    let reg = breg().unwrap();
    let air = reg.state_of("minecraft:air", None);
    let mut g = Gen::load(0, crate::name_to_state).expect("datapack");
    let mut ctx = context(&mut g);
    let (x, z) = (1280, 0);
    for dx in -8..=8 {
        for dz in -8..=8 {
            let (px, pz) = (x + dx, z + dz);
            let fy = ctx.hm_oc_terrain(px, pz);
            for y in fy..=fy + 4 {
                ctx.set(px, y, pz, air);
            }
        }
    }
    let before = ctx.wr.len();
    let mut rng = Xor::seed(3);
    gen_cf(&sea_pickle_cf(), freg(), &mut ctx, &mut rng, (x, 0, z));
    let pickles = ctx
        .wr
        .iter()
        .filter(|(_, &st)| reg.name_of(st) == "minecraft:sea_pickle")
        .count();
    assert_eq!(pickles, 0, "nothing may be placed on unprepared terrain");
    assert_eq!(
        ctx.wr.len(),
        before,
        "a skipped attempt writes nothing at all"
    );
    let mut expect = Xor::seed(3);
    for _ in 0..10 {
        let v = expect.next_bound(8);
        let _ = v;
    }
    assert_eq!(
        rng.next_bound(1 << 30),
        expect.next_bound(1 << 30),
        "the skipped attempts still spent their draws"
    );
}

/// `minecraft:ice_spike` (`ecq`), whose config is `{}`. The feature walks down
/// through air, requires a snow block, then stacks `radius` shrinking over
/// `nextInt(4) + 7` layers; the drawn base offset and height are replayed here from
/// a fresh stream, so the draw order is pinned as well as the shape.
#[test]
fn ice_spike_builds_a_tapering_spike_on_snow() {
    let reg = breg().unwrap();
    let snow = reg.state_of("minecraft:snow_block", None);
    let packed = reg.state_of("minecraft:packed_ice", None);
    let mut g = Gen::load(0, crate::name_to_state).expect("datapack");
    let mut ctx = context(&mut g);
    let (x, z) = (8, 8);
    // a snow platform at y=70, high enough for the downward drill (it stops at 50)
    let top = 70;
    for dx in -10..=10 {
        for dz in -10..=10 {
            for y in top - 6..=top {
                ctx.set(x + dx, y, z + dz, snow);
            }
            for y in top + 1..=top + 30 {
                ctx.set(x + dx, y, z + dz, reg.state_of("minecraft:air", None));
            }
        }
    }

    // replay the first draws: base offset, height, radius term, and the
    // floating-spike roll, which is only drawn when the radius exceeds one
    let mut probe = Xor::seed(11);
    let base_off = probe.next_bound(4);
    let height = probe.next_bound(4) + 7;
    let radius_max = height / 4 + probe.next_bound(2);
    let mut float_off = 0;
    if radius_max > 1 && probe.next_bound(60) == 0 {
        float_off = 10 + probe.next_bound(30);
    }
    let base_y = top + base_off + float_off;

    let mut rng = Xor::seed(11);
    gen_cf(&Cf::IceSpike, freg(), &mut ctx, &mut rng, (x, top + 20, z));

    let cells: Vec<(i32, i32, i32)> = ctx
        .wr
        .iter()
        .filter(|(_, &st)| st == packed)
        .map(|(&p, _)| p)
        .collect();
    assert!(!cells.is_empty(), "a snow platform must produce a spike");
    assert!(
        cells
            .iter()
            .any(|(x0, y, z0)| (*x0, *y, *z0) == (x, base_y, z)),
        "the base cell is always placed (dx = dz = 0 is inside, and r >= 1 at h = 0)"
    );
    let tip = cells.iter().map(|(_, y, _)| *y).max().unwrap();
    assert_eq!(
        tip,
        base_y + height - 1,
        "the tip must be the drawn height above the drawn base"
    );
    let tip_cols: Vec<_> = cells.iter().filter(|(_, y, _)| *y == tip).collect();
    assert_eq!(
        tip_cols.len(),
        1,
        "the tip is a single column: {tip_cols:?}"
    );
    assert!(
        cells.iter().any(|(_, y, _)| *y < top),
        "the downward drill must replace the snow below the base"
    );
    // and the drill only writes the one material
    assert!(ctx
        .wr
        .values()
        .all(|&st| { st == packed || st == snow || reg.name_of(st) == "minecraft:air" }));
}

/// The snow requirement is checked before any draw, so a spike that finds stone
/// places nothing and leaves the stream untouched.
#[test]
fn ice_spike_does_nothing_without_snow() {
    let reg = breg().unwrap();
    let stone = reg.state_of("minecraft:stone", None);
    let mut g = Gen::load(0, crate::name_to_state).expect("datapack");
    let mut ctx = context(&mut g);
    let (x, z) = (8, 8);
    for dx in -10..=10 {
        for dz in -10..=10 {
            for y in 60..=70 {
                ctx.set(x + dx, y, z + dz, stone);
            }
        }
    }
    let before = ctx.wr.len();
    let mut rng = Xor::seed(11);
    gen_cf(&Cf::IceSpike, freg(), &mut ctx, &mut rng, (x, 90, z));
    assert_eq!(
        ctx.wr.len(),
        before,
        "a non-snow surface places nothing at all"
    );
    assert_eq!(
        rng.next_bound(1 << 30),
        Xor::seed(11).next_bound(1 << 30),
        "and draws nothing"
    );
}

/// `minecraft:root_system` (`edg`): the ported *rules*, unit-tested because they are
/// where the obfuscated readings went wrong (`BlockState.e()` is `isSolid()`, not
/// `isAir()`; `FluidTags.a`/`b` are WATER/LAVA).
///
/// The end-to-end path (tree placed -> root column -> hanging roots) is not covered
/// here: it needs the nested placed feature to write, and in this harness the
/// synthetic nested features tried (an inline `disk` and an inline `simple_block`,
/// each also run directly at the candidate position) wrote nothing even though the
/// target blocks were present. That is an open harness question, recorded in
/// evidence/gameplay/feature-parity/README.md, not a claim about this feature.
#[test]
fn root_system_vertical_water_allowance_follows_the_config() {
    let reg = breg().unwrap();
    let air = reg.state_of("minecraft:air", None);
    let water = reg.state_of("minecraft:water", None);
    let stone = reg.state_of("minecraft:stone", None);
    // air is always fine
    assert!(super::root_valid_vertical(reg, air, 1, 2));
    assert!(super::root_valid_vertical(reg, air, 9, 2));
    // water only up to allowed_vertical_water_for_tree, counted from the base
    assert!(super::root_valid_vertical(reg, water, 1, 2));
    assert!(
        !super::root_valid_vertical(reg, water, 2, 2),
        "i + 1 <= max_water, so index 2 is past an allowance of 2"
    );
    assert!(!super::root_valid_vertical(reg, water, 1, 0));
    // anything else is not a valid vertical state at all
    assert!(!super::root_valid_vertical(reg, stone, 1, 2));
    // and the fluid predicates separate the two tags
    assert!(super::is_water_fluid(reg, water));
    assert!(!super::is_lava_fluid(reg, water));
    let lava = reg.state_of("minecraft:lava", None);
    assert!(super::is_lava_fluid(reg, lava));
    assert!(!super::is_water_fluid(reg, lava));
}

/// The origin must be air: a root system under a solid block returns immediately,
/// before a single draw (the check is the first statement in vanilla's `place`).
#[test]
fn root_system_does_nothing_when_its_origin_is_not_air() {
    let reg = breg().unwrap();
    let stone = reg.state_of("minecraft:stone", None);
    let cf = pack_cf("rooted_azalea_tree");
    let mut g = Gen::load(0, crate::name_to_state).expect("datapack");
    let mut ctx = context(&mut g);
    let (x, z) = (8, 8);
    for dx in -8..=8 {
        for dz in -8..=8 {
            for y in 60..=90 {
                ctx.set(x + dx, y, z + dz, stone);
            }
        }
    }
    let before = ctx.wr.len();
    let mut rng = Xor::seed(5);
    gen_cf(&cf, freg(), &mut ctx, &mut rng, (x, 70, z));
    assert_eq!(ctx.wr.len(), before, "a buried origin places nothing");
    assert_eq!(
        rng.next_bound(1 << 30),
        Xor::seed(5).next_bound(1 << 30),
        "and draws nothing"
    );
}

/// `minecraft:desert_well` (`ebw`), config `{}`. Every cell is a fixed offset from
/// the sand block the walk-down lands on, so the shape is asserted exactly; the only
/// randomness is which of the five shaft columns receives the two suspicious-sand
/// blocks (`nextInt(5)` twice), and that is replayed from the same seed.
#[test]
fn desert_well_builds_its_shape_on_sand() {
    let reg = breg().unwrap();
    let states = |n: &str| reg.state_of(n, None);
    let (sand, slab, sandstone, water, suspicious) = (
        states("minecraft:sand"),
        states("minecraft:sandstone_slab"),
        states("minecraft:sandstone"),
        states("minecraft:water"),
        states("minecraft:suspicious_sand"),
    );
    let mut g = Gen::load(0, crate::name_to_state).expect("datapack");
    let mut ctx = context(&mut g);
    let (x, z) = (8, 8);
    let top = 70;
    for dx in -6..=6 {
        for dz in -6..=6 {
            for y in top - 4..=top {
                ctx.set(x + dx, y, z + dz, sand);
            }
        }
    }
    let mut rng = Xor::seed(9);
    gen_cf(&Cf::DesertWell, freg(), &mut ctx, &mut rng, (x, top + 3, z));

    let at = |ctx: &mut FC, p: (i32, i32, i32)| ctx.get(p.0, p.1, p.2);
    // the walk-down lands on the sand's top cell and the well is built there
    assert_eq!(
        at(&mut ctx, (x, top, z)),
        water,
        "the well's column is water"
    );
    for (dx, dz) in [(1, 0), (0, 1), (-1, 0), (0, -1)] {
        assert_eq!(
            at(&mut ctx, (x + dx, top, z + dz)),
            water,
            "and one cell wider"
        );
        assert_eq!(
            at(&mut ctx, (x + dx, top - 1, z + dz)),
            sand,
            "with a sand floor under the arms"
        );
    }
    assert_eq!(at(&mut ctx, (x, top - 1, z)), sand);
    // base plate, roof, posts and pillars
    assert_eq!(at(&mut ctx, (x - 2, top - 2, z)), slab, "base plate");
    assert_eq!(at(&mut ctx, (x, top + 4, z)), sandstone, "roof centre");
    assert_eq!(at(&mut ctx, (x + 1, top + 4, z)), slab, "roof ring");
    assert_eq!(at(&mut ctx, (x + 2, top + 1, z)), slab, "a post");
    assert_eq!(at(&mut ctx, (x - 1, top + 1, z - 1)), sandstone, "a pillar");
    assert_eq!(
        at(&mut ctx, (x + 1, top + 3, z + 1)),
        sandstone,
        "up the pillar"
    );

    // the two suspicious-sand blocks, on the columns the two draws choose
    let shaft = [
        (x, top, z),
        (x + 1, top, z),
        (x, top, z + 1),
        (x - 1, top, z),
        (x, top, z - 1),
    ];
    let mut probe = Xor::seed(9);
    let first = shaft[probe.next_bound(5) as usize];
    let second = shaft[probe.next_bound(5) as usize];
    assert_eq!(
        at(&mut ctx, (first.0, first.1 + 1, first.2)),
        suspicious,
        "the first draw picks the column at +1"
    );
    assert_eq!(
        at(&mut ctx, (second.0, second.1 + 2, second.2)),
        suspicious,
        "the second draw picks the column at +2"
    );
    let cells: Vec<((i32, i32, i32), u32)> = ctx.wr.iter().map(|(&p, &st)| (p, st)).collect();
    assert_eq!(
        cells.iter().filter(|(_, st)| *st == suspicious).count(),
        2,
        "exactly two suspicious-sand blocks"
    );
}

/// The sand gate is the first thing after the walk-down and precedes the feature's
/// only two draws, so a stone surface places nothing and spends nothing.
#[test]
fn desert_well_needs_sand_and_draws_nothing_otherwise() {
    let reg = breg().unwrap();
    let stone = reg.state_of("minecraft:stone", None);
    let mut g = Gen::load(0, crate::name_to_state).expect("datapack");
    let mut ctx = context(&mut g);
    let (x, z) = (8, 8);
    for dx in -6..=6 {
        for dz in -6..=6 {
            for y in 60..=70 {
                ctx.set(x + dx, y, z + dz, stone);
            }
        }
    }
    let before = ctx.wr.clone();
    let mut rng = Xor::seed(9);
    gen_cf(&Cf::DesertWell, freg(), &mut ctx, &mut rng, (x, 73, z));
    assert!(ctx.wr == before, "a stone surface places nothing");
    assert_eq!(
        rng.next_bound(1 << 30),
        Xor::seed(9).next_bound(1 << 30),
        "and draws nothing"
    );
}

/// `minecraft:iceberg` (`ecr`), config `{state}`. The shape parameters all come from
/// the head of the draw stream (`nextDouble > 0.7`, `nextDouble*2pi`, `nextInt(5)`,
/// `nextInt(3)`, a second `nextDouble > 0.7` choosing the tall shape, then the height
/// and the two radii), so they are replayed here and used as the bounds the placed
/// cells must satisfy - which ties the replay to what the placement actually used.
#[test]
fn iceberg_builds_a_shape_around_sea_level() {
    let reg = breg().unwrap();
    let packed = reg.state_of("minecraft:packed_ice", None);
    let snow = reg.state_of("minecraft:snow_block", None);
    let water = reg.state_of("minecraft:water", None);
    let stone = reg.state_of("minecraft:stone", None);
    let mut g = Gen::load(0, crate::name_to_state).expect("datapack");
    let mut ctx = context(&mut g);
    let sea = crate::worldgen::sea_level();
    let (x, z) = (8, 8);
    // an ocean deep enough for the underside and wide enough for the widest shape
    for dx in -40..=40 {
        for dz in -40..=40 {
            for y in sea - 40..=sea - 1 {
                ctx.set(x + dx, y, z + dz, water);
            }
            for y in sea..=sea + 40 {
                ctx.set(x + dx, y, z + dz, water);
            }
            ctx.set(x + dx, sea - 41, z + dz, stone);
        }
    }

    // replay the head of the stream
    let mut probe = Xor::seed(21);
    let _fire = probe.next_f64() > 0.7;
    let _angle = probe.next_f64() * 2.0 * std::f64::consts::PI;
    let i = 11 - probe.next_bound(5);
    let _j = 3 + probe.next_bound(3);
    let tall = probe.next_f64() > 0.7;
    let mut l = if tall {
        probe.next_bound(6) + 6
    } else {
        probe.next_bound(15) + 3
    };
    if !tall && probe.next_f64() > 0.9 {
        l += probe.next_bound(19) + 7;
    }
    let i1 = (l + probe.next_bound(11)).min(18);
    let _j1 = (l + probe.next_bound(7) - probe.next_bound(5)).min(11);
    let k1 = if tall { i } else { 11 };

    let mut rng = Xor::seed(21);
    // the origin is handed in *above* sea level and the feature re-anchors it, which
    // is what the mutation anchor for that line checks
    gen_cf(
        &Cf::Iceberg { state: packed },
        freg(),
        &mut ctx,
        &mut rng,
        (x, sea + 12, z),
    );

    let cells: Vec<((i32, i32, i32), u32)> = ctx.wr.iter().map(|(&p, &st)| (p, st)).collect();
    let ice: Vec<_> = cells
        .iter()
        .filter(|(_, st)| *st == packed || *st == snow)
        .collect();
    assert!(!ice.is_empty(), "the iceberg must place its material");
    assert!(
        cells.iter().any(|(_, st)| *st == packed),
        "the config's state is what most of the shape is built from"
    );
    let mut dmin = i32::MAX;
    let mut dmax = i32::MIN;
    for ((px, py, pz), _) in ice {
        let (dx, dy, dz) = (px - x, py - sea, pz - z);
        assert!(
            dx.abs() <= k1 + 4 && dz.abs() <= k1 + 4,
            "a cell must stay near the footprint (k1 = {k1}): {dx},{dy},{dz}"
        );
        dmin = dmin.min(dy);
        dmax = dmax.max(dy);
    }
    assert!(
        dmin >= -(i1 - 1),
        "the underside must not go below -i1 + 1 (i1 = {i1}), got {dmin}"
    );
    assert!(
        dmax < l,
        "the top must not go above l - 1 (l = {l}), got {dmax}"
    );
    assert!(
        dmin < 0 && dmax > 0,
        "an iceberg straddles sea level: below {dmin}, above {dmax}"
    );
    // and the water it was built in is still water elsewhere
    assert_eq!(ctx.get(x + 30, sea + 30, z), water, "far away stays water");
}

/// Load a configured feature straight from the pack's own JSON, so a test never
/// restates a config that the pack already holds.
fn pack_cf(name: &str) -> Cf {
    // Gen::dp_files_any(["configured_feature", "feature"]) overlays `feature`
    // last, so a converted pack's `feature/` wins. Match that precedence.
    for dir in ["feature", "configured_feature"] {
        let path = format!("datapacks/vanilla/data/minecraft/worldgen/{dir}/{name}.json");
        if let Ok(text) = std::fs::read_to_string(&path) {
            return super::p_cf(&jparse(text.as_bytes()).expect("json"))
                .expect("the config must parse");
        }
    }
    panic!("no configured feature {name} in the pack");
}

/// The gallery of `dripstone_cluster_hangs_...`: a stone slab with a span of `fill`
/// cells between a floor endpoint at y = 59 and a ceiling endpoint at y = 67. The
/// origin sits in the middle of the span.
fn dripstone_gallery(ctx: &mut FC, ox: i32, oz: i32, floor_y: i32, span: i32, fill: u32) {
    let reg = breg().unwrap();
    let stone = reg.state_of("minecraft:stone", None);
    for dx in -12..=12 {
        for dz in -12..=12 {
            for y in 40..=floor_y {
                ctx.set(ox + dx, y, oz + dz, stone);
            }
            for y in floor_y + 1..=floor_y + span {
                ctx.set(ox + dx, y, oz + dz, fill);
            }
            for y in floor_y + span + 1..=100 {
                ctx.set(ox + dx, y, oz + dz, stone);
            }
        }
    }
}

/// The cells a feature changed, given the fixture's own write set: a cell counts as
/// the feature's when it is new *or* its state differs (a replacement is not "new").
fn changed(
    before: &std::collections::HashMap<(i32, i32, i32), u32>,
    ctx: &FC,
) -> Vec<((i32, i32, i32), u32)> {
    let mut out: Vec<((i32, i32, i32), u32)> = ctx
        .wr
        .iter()
        .filter(|(p, st)| before.get(*p) != Some(*st))
        .map(|(&p, &st)| (p, st))
        .collect();
    out.sort();
    out
}

/// `minecraft:dripstone_cluster` (`eby`, `DripstoneClusterFeature`) with a dry gallery
/// (span of air). Pinned here: the orientation of every tip (a stalactite hangs below
/// the ceiling with `vertical_direction=down` and starts at the ceiling's own
/// neighbour cell, a stalagmite stands the other way up), the layer sequence
/// (base -> middles -> frustum -> tip), that no dripstone block lands inside the span,
/// that nothing is written outside the drawn radius, and the exact number of cells this
/// seeded stream produces.
#[test]
fn dripstone_cluster_hangs_stalactites_below_and_stands_stalagmites_above() {
    let reg = breg().unwrap();
    let cf = pack_cf("dripstone_cluster");
    let Cf::DripstoneCluster(dcfg) = &cf else {
        panic!("dripstone_cluster must parse as a cluster");
    };
    let mut g = Gen::load(0, crate::name_to_state).expect("datapack");
    let mut ctx = context(&mut g);
    let air = reg.state_of("minecraft:air", None);
    let (ox, oz, oy) = (8, 8, 64);
    let (floor_y, ceil_y) = (59, 67);
    dripstone_gallery(&mut ctx, ox, oz, floor_y, 7, air);

    // replay the head of the stream for the radii the footprint bound needs
    let (rx, rz) = {
        let mut probe = Xor::seed(31);
        let _ = dcfg.height.get(&mut probe);
        let _ = dcfg.wetness.get(&mut probe);
        let _ = dcfg.density.get(&mut probe);
        let rx = dcfg.radius.get(&mut probe);
        let rz = dcfg.radius.get(&mut probe);
        (rx, rz)
    };
    let before = ctx.wr.clone();
    let mut rng = Xor::seed(31);
    gen_cf(&cf, freg(), &mut ctx, &mut rng, (ox, oy, oz));

    let mut blocks = 0usize;
    let mut cols: std::collections::BTreeMap<(i32, i32), Vec<(i32, u32)>> = Default::default();
    for ((px, py, pz), st) in changed(&before, &ctx) {
        assert!(
            (px - ox).abs() <= rx && (pz - oz).abs() <= rz,
            "a write at {px},{py},{pz} is outside the drawn radius {rx}x{rz}"
        );
        match reg.name_of(st) {
            "minecraft:pointed_dripstone" => {
                cols.entry((px, pz)).or_default().push((py, st));
            }
            "minecraft:dripstone_block" => {
                blocks += 1;
                assert!(
                    !(floor_y + 1..=ceil_y - 1).contains(&py),
                    "a dripstone block inside the span at {px},{py},{pz}"
                );
            }
            _ => {}
        }
    }

    let mut tips = 0usize;
    for ((px, pz), cells) in &cols {
        for up in [false, true] {
            let want = if up { "up" } else { "down" };
            let mut run: Vec<(i32, String)> = cells
                .iter()
                .filter(|(_, st)| reg.get_prop(*st, "vertical_direction").as_deref() == Some(want))
                .map(|(y, st)| (*y, reg.get_prop(*st, "thickness").unwrap_or_default()))
                .collect();
            if run.is_empty() {
                continue;
            }
            run.sort_by_key(|(y, _)| if up { *y } else { -*y });
            tips += run.len();
            let first_y = run[0].0;
            let want_y = if up { floor_y + 1 } else { ceil_y - 1 };
            assert_eq!(
                first_y, want_y,
                "a {want} tip at {px},{pz} must start at the span's own neighbour cell"
            );
            let n = run.len() as i32;
            let mut expect: Vec<&str> = Vec::new();
            if n >= 3 {
                expect.push("base");
                expect.extend(std::iter::repeat_n("middle", (n - 3) as usize));
            }
            if n >= 2 {
                expect.push("frustum");
            }
            expect.push("tip");
            let last = run.last().unwrap().1.as_str();
            assert!(
                last == "tip" || last == "tip_merge",
                "the tip of a {want} run at {px},{pz} is {last}"
            );
            let got: Vec<&str> = run[..run.len() - 1]
                .iter()
                .map(|(_, t)| t.as_str())
                .collect();
            assert_eq!(
                got,
                expect[..expect.len() - 1],
                "layer sequence of the {want} run at {px},{pz}"
            );
            if want == "down" {
                assert!(
                    first_y < ceil_y && run.iter().all(|(y, _)| *y > floor_y),
                    "a stalactite belongs between the floor and the ceiling"
                );
            }
        }
    }
    println!(
        "DRIPSTONE dry: tips={tips} blocks={blocks} columns={} rx={rx} rz={rz}",
        cols.len()
    );
    assert!(tips > 0, "the seeded stream must grow at least one tip");
    // A regression pin, not a vanilla comparison: the seeded stream with the pack's
    // own config produces exactly this, so any change to a draw, a gate or a loop
    // bound moves it.
    assert_eq!((tips, blocks), (209, 675), "golden cell counts");
}

/// The same feature with a **water** span. `canPlacePool` wants base stone or water on
/// all four sides and above the floor cell, which a dry gallery's air cannot provide -
/// so a pool only appears in an already-wet column. The pool turns the floor endpoint
/// into water and moves the column's floor one cell *down* into the rock, so the
/// stalagmite that follows starts one cell lower than in the dry case, replaces the
/// pool's own water, and is waterlogged. That lowered tip is what this pins.
#[test]
fn dripstone_cluster_pools_lower_the_stalagmite_into_the_water() {
    let reg = breg().unwrap();
    let cf = pack_cf("dripstone_cluster");
    let mut g = Gen::load(0, crate::name_to_state).expect("datapack");
    let mut ctx = context(&mut g);
    let water = reg.state_of("minecraft:water", None);
    let (ox, oz, oy) = (8, 8, 64);
    let floor_y = 59;
    dripstone_gallery(&mut ctx, ox, oz, floor_y, 7, water);

    let before = ctx.wr.clone();
    let mut rng = Xor::seed(31);
    gen_cf(&cf, freg(), &mut ctx, &mut rng, (ox, oy, oz));

    let mut lowered = 0usize;
    for ((px, py, pz), st) in changed(&before, &ctx) {
        if py != floor_y {
            continue;
        }
        if reg.name_of(st) == "minecraft:pointed_dripstone" {
            // the pool's cell: the tip replaced the water it had just written
            assert_eq!(
                reg.get_prop(st, "vertical_direction").as_deref(),
                Some("up"),
                "a lowered tip at {px},{pz} must grow up"
            );
            assert_eq!(
                reg.get_prop(st, "waterlogged").as_deref(),
                Some("true"),
                "the tip standing in the pool at {px},{pz} is waterlogged"
            );
            lowered += 1;
        }
        // Otherwise the column kept its water: that happens when the floor tip is not
        // grown at all, and the two never share a cell.
        assert!(
            reg.name_of(st) == "minecraft:water"
                || reg.name_of(st) == "minecraft:pointed_dripstone"
                || reg.name_of(st) == "minecraft:dripstone_block",
            "unexpected {} at the floor endpoint {px},{pz}",
            reg.name_of(st)
        );
    }
    println!("DRIPSTONE wet: lowered_tips={lowered} water_cells={water}");
    assert!(
        lowered > 0,
        "a water-filled span must produce pools for this seed"
    );
}

/// The float providers (`bpu` and its four types) arrived with the dripstone cluster.
/// The feature's whole shape shifts if any of them consumes a different number of draws,
/// so each is checked against a twin `Xor` allowed to draw exactly what the vanilla
/// method draws - value *and* follow-on stream position.
#[test]
fn float_providers_draw_and_clamp_like_the_vanilla_types() {
    // constant (`bps`): no draw
    let p = p_fp(&jparse(br#"{"type":"constant","value":0.25}"#).unwrap()).unwrap();
    let (mut a, mut b) = (Xor::seed(3), Xor::seed(3));
    assert_eq!(p.get(&mut a), 0.25);
    assert_eq!(a.next_long(), b.next_long(), "a constant draws nothing");

    // uniform (`bqb`): one nextFloat, `min + r * (max - min)`, max exclusive
    let p =
        p_fp(&jparse(br#"{"type":"uniform","min_inclusive":0.3,"max_exclusive":0.7}"#).unwrap())
            .unwrap();
    let (mut a, mut b) = (Xor::seed(3), Xor::seed(3));
    assert_eq!(p.get(&mut a), b.next_f32() * (0.7f32 - 0.3) + 0.3);
    assert_eq!(a.next_long(), b.next_long(), "a uniform float draws once");

    // clamped normal (`bpq`): one Gaussian (two doubles), f64 arithmetic, then clamp
    let p = p_fp(
        &jparse(br#"{"type":"clamped_normal","mean":0.1,"deviation":0.3,"min":0.1,"max":0.9}"#)
            .unwrap(),
    )
    .unwrap();
    let (mut a, mut b) = (Xor::seed(3), Xor::seed(3));
    // the config's floats are promoted to f64 (`Mth.nextGaussian(RandomSource, double,
    // double)`), so the f32->f64 widening of `0.1` is part of the value
    let want =
        (0.1f32 as f64 + b.next_gauss() * 0.3f32 as f64).clamp(0.1f32 as f64, 0.9f32 as f64) as f32;
    assert_eq!(p.get(&mut a), want);
    assert_eq!(
        a.next_long(),
        b.next_long(),
        "a clamped normal draws one Gaussian"
    );

    // trapezoid (`bqa`): two nextFloats, `min + r1*h + r2*g`
    let p = p_fp(&jparse(br#"{"type":"trapezoid","min":-1.0,"max":2.0,"plateau":1.0}"#).unwrap())
        .unwrap();
    let (mut a, mut b) = (Xor::seed(3), Xor::seed(3));
    let (f, g) = (3.0f32, (3.0f32 - 1.0) / 2.0);
    assert_eq!(
        p.get(&mut a),
        -1.0 + b.next_f32() * (f - g) + b.next_f32() * g
    );
    assert_eq!(
        a.next_long(),
        b.next_long(),
        "a trapezoid float draws twice"
    );
}

/// `minecraft:large_dripstone` (`ecu`, `LargeDripstoneFeature`) in a gallery whose span
/// is 7 tall. The span's height gives the radius (`height * 0.33` clamped into the
/// pack's `column_radius` 3..19, so exactly 3) and the two spikes reach their bases at
/// the span's own neighbour cells. The body is built *in* the empty cells and stops when
/// it walks into base stone, so every placed cell is inside the span.
#[test]
fn large_dripstone_hangs_and_stands_a_tapered_spike() {
    let reg = breg().unwrap();
    let cf = pack_cf("large_dripstone");
    let Cf::LargeDripstone(dcfg) = &cf else {
        panic!("large_dripstone must parse as a large dripstone");
    };
    let mut g = Gen::load(0, crate::name_to_state).expect("datapack");
    let mut ctx = context(&mut g);
    let air = reg.state_of("minecraft:air", None);
    let drip = reg.state_of("minecraft:dripstone_block", None);
    let (ox, oz, oy) = (8, 8, 63);
    let (floor_y, ceil_y) = (59, 67);
    dripstone_gallery(&mut ctx, ox, oz, floor_y, 7, air);

    // replay the radius draw (the span height and the config decide it)
    let want_radius = {
        let mut probe = Xor::seed(11);
        let est = ((ceil_y - floor_y - 1) as f32 * dcfg.ratio) as i32;
        let est = est.clamp(dcfg.radius.min_inclusive(), dcfg.radius.max_inclusive());
        probe.next_inbetween(dcfg.radius.min_inclusive(), est)
    };
    let before = ctx.wr.clone();
    let mut rng = Xor::seed(11);
    gen_cf(&cf, freg(), &mut ctx, &mut rng, (ox, oy, oz));

    let changed = changed(&before, &ctx);
    assert!(!changed.is_empty(), "the gallery must grow something");
    let mut above = 0usize;
    let mut below = 0usize;
    let mut cols: std::collections::BTreeMap<(i32, i32), Vec<i32>> = Default::default();
    for ((px, py, pz), st) in &changed {
        assert_eq!(
            *st, drip,
            "the large dripstone is built out of dripstone blocks: {px},{py},{pz}"
        );
        assert!(
            (floor_y + 1..=ceil_y - 1).contains(py),
            "the body is built in the span, not in the rock: {px},{py},{pz}"
        );
        assert!(
            ((px - ox).pow(2) + (pz - oz).pow(2)) as f64 <= (want_radius as f64).powi(2) + 1.0,
            "a cell at {px},{pz} is outside the drawn radius {want_radius}"
        );
        if *py > oy {
            above += 1;
        } else {
            below += 1;
        }
        cols.entry((*px, *pz)).or_default().push(*py);
    }
    assert!(
        above > 0 && below > 0,
        "both spikes must appear: {above} / {below}"
    );

    // Each spike's cells in a column are anchored at its own base and run contiguously
    // away from the span's end - a spike can only place on empty cells and can only stop
    // where a cell was already filled or its taper ran out, so the two anchored walks
    // (down from the ceiling's neighbour, up from the floor's) reproduce the column
    // exactly. A spike that reached neither end would show up here.
    let (mut hang_runs, mut stand_runs) = (0, 0);
    for ((px, pz), ys) in &cols {
        let set: std::collections::BTreeSet<i32> = ys.iter().copied().collect();
        let mut walk: std::collections::BTreeSet<i32> = Default::default();
        let mut y = ceil_y - 1;
        while set.contains(&y) {
            walk.insert(y);
            y -= 1;
        }
        if set.contains(&(ceil_y - 1)) {
            hang_runs += 1;
        }
        y = floor_y + 1;
        while set.contains(&y) {
            walk.insert(y);
            y += 1;
        }
        if set.contains(&(floor_y + 1)) {
            stand_runs += 1;
        }
        assert_eq!(
            &walk, &set,
            "the cells at {px},{pz} must be the two anchored runs: {ys:?}"
        );
    }
    assert!(
        hang_runs > 0 && stand_runs > 0,
        "both spikes must build: {hang_runs} hanging / {stand_runs} standing"
    );
    println!(
        "LARGE dry: cells={} columns={} above={} below={} radius={want_radius} runs={hang_runs}/{stand_runs}",
        changed.len(),
        cols.len(),
        above,
        below
    );
    assert_eq!(changed.len(), 37, "golden cell count");
}

/// The same feature in a span tall enough to satisfy the wind gate
/// (`min_radius_for_wind` / `min_bluntness_for_wind`). The sway is a per-y translation
/// `floor(wind * (origin_y - y))`, so running the identical seed with the pack's wind
/// speed and with a *zero-drawing* zero speed (same draws, no displacement) must produce
/// the same cells moved by exactly that vector.
#[test]
fn large_dripstone_sways_when_both_spikes_clear_the_wind_gate() {
    let reg = breg().unwrap();
    let cf = pack_cf("large_dripstone");
    let Cf::LargeDripstone(dcfg) = &cf else {
        panic!("large_dripstone must parse as a large dripstone");
    };
    let mut g = Gen::load(0, crate::name_to_state).expect("datapack");
    let mut ctx = context(&mut g);
    let air = reg.state_of("minecraft:air", None);
    let (ox, oz, oy) = (8, 8, 63);
    let (floor_y, ceil_y) = (55, 71);
    dripstone_gallery(&mut ctx, ox, oz, floor_y, 15, air);

    // The gate is a precondition: with a failing seed there is no sway, and the
    // comparison below would prove nothing. Pick the first seed that clears it and say
    // which one, so the test cannot silently degrade into comparing two still shapes.
    let est = ((ceil_y - floor_y - 1) as f32 * dcfg.ratio) as i32;
    let est = est.clamp(dcfg.radius.min_inclusive(), dcfg.radius.max_inclusive());
    let (seed, radius, wind_v) = (0u64..64)
        .find_map(|seed| {
            let mut probe = Xor::seed(seed);
            let radius = probe.next_inbetween(dcfg.radius.min_inclusive(), est);
            let hang_blunt = dcfg.stalactite_bluntness.get(&mut probe);
            let _hang_scale = dcfg.height_scale.get(&mut probe);
            let stand_blunt = dcfg.stalagmite_bluntness.get(&mut probe);
            let _stand_scale = dcfg.height_scale.get(&mut probe);
            if radius < dcfg.min_radius_for_wind
                || hang_blunt < dcfg.min_bluntness_for_wind
                || stand_blunt < dcfg.min_bluntness_for_wind
            {
                return None;
            }
            let speed = dcfg.wind_speed.get(&mut probe);
            let angle = mth_next_f32(&mut probe, 0.0, std::f32::consts::PI);
            Some((
                seed,
                radius,
                (
                    (mth_cos(angle) * speed) as f64,
                    (mth_sin(angle) * speed) as f64,
                ),
            ))
        })
        .expect("some seed under 64 must clear the wind gate");

    let before = ctx.wr.clone();
    let mut rng = Xor::seed(seed);
    gen_cf(&cf, freg(), &mut ctx, &mut rng, (ox, oy, oz));
    let with_wind = changed(&before, &ctx);

    // `Uniform(0.0, 0.0)` draws exactly as much as the pack's uniform speed, so the two
    // runs share a stream and differ only in the displacement
    let plain_cfg = LargeDripstoneCfg {
        range: dcfg.range,
        radius: dcfg.radius.clone(),
        height_scale: dcfg.height_scale.clone(),
        ratio: dcfg.ratio,
        stalactite_bluntness: dcfg.stalactite_bluntness.clone(),
        stalagmite_bluntness: dcfg.stalagmite_bluntness.clone(),
        wind_speed: FP::Uniform(0.0, 0.0),
        min_radius_for_wind: dcfg.min_radius_for_wind,
        min_bluntness_for_wind: dcfg.min_bluntness_for_wind,
    };
    let mut ctx2 = context(&mut g);
    dripstone_gallery(&mut ctx2, ox, oz, floor_y, 15, air);
    let before2 = ctx2.wr.clone();
    let mut rng2 = Xor::seed(seed);
    gen_cf(
        &Cf::LargeDripstone(Box::new(plain_cfg)),
        freg(),
        &mut ctx2,
        &mut rng2,
        (ox, oy, oz),
    );
    let plain = changed(&before2, &ctx2);

    assert!(!with_wind.is_empty() && with_wind.len() == plain.len());
    let (vx, vz) = wind_v;
    let expect: std::collections::BTreeSet<((i32, i32, i32), u32)> = with_wind
        .iter()
        .map(|((px, py, pz), st)| {
            let dy = (oy - py) as f64;
            (
                (
                    px - (vx * dy).floor() as i32,
                    *py,
                    pz - (vz * dy).floor() as i32,
                ),
                *st,
            )
        })
        .collect();
    let got: std::collections::BTreeSet<((i32, i32, i32), u32)> = plain.iter().copied().collect();
    assert_eq!(
        expect, got,
        "the wind moves each cell by floor(wind * (origin_y - y))"
    );
    println!(
        "LARGE wind: seed={seed} cells={} radius={radius} wind={vx:.4},{vz:.4}",
        with_wind.len()
    );
    assert_eq!(with_wind.len(), 126, "golden cell count");
}

#[test]
fn freeze_top_layer_matches_native_oracle() {
    let reg = breg().expect("registry");
    let grass = reg.state_of("minecraft:grass_block", None);
    let grass_snowy = reg.with_prop(grass, "snowy", "true");
    let podzol = reg.state_of("minecraft:podzol", None);
    let mycelium = reg.state_of("minecraft:mycelium", None);
    let stone = reg.state_of("minecraft:stone", None);
    let water0 = reg.state_of("minecraft:water", None);
    let water1 = reg.with_prop(water0, "level", "1");
    let slab_bottom = reg.state_of("minecraft:oak_slab", None);
    let slab_top = reg.with_prop(slab_bottom, "type", "top");
    let slab_waterlogged = reg.with_prop(slab_bottom, "waterlogged", "true");
    let snow = reg.state_of("minecraft:snow", None);
    let snow8 = reg.with_prop(snow, "layers", "8");

    let fixture = include_str!("../tests/data/freeze_top_layer_1211.tsv");
    let mut g = Gen::load(0, crate::name_to_state).expect("datapack");
    let mut tested = 0usize;
    for line in fixture.lines() {
        if line.is_empty() {
            continue;
        }
        let cols: Vec<&str> = line.split('\t').collect();
        assert_eq!(cols.len(), 4, "bad row: {line}");
        let id = cols[0];
        let origin_parts: Vec<i32> = cols[1].split(',').map(|v| v.parse().unwrap()).collect();
        let origin = (origin_parts[0], origin_parts[1], origin_parts[2]);

        let mut ctx = context(&mut g);
        ctx.test_no_terrain = true;
        ctx.test_default_air = true;
        ctx.test_writes = Some(Vec::new());
        ctx.test_biome("minecraft:snowy_plains");

        match id {
            "cold_grass_air" => ctx.test_base_cell((0, 64, 0), grass),
            "cold_podzol_air" => ctx.test_base_cell((0, 64, 0), podzol),
            "cold_mycelium_air" => ctx.test_base_cell((0, 64, 0), mycelium),
            "cold_stone_air" => ctx.test_base_cell((0, 64, 0), stone),
            "cold_grass_short_grass" => {
                ctx.test_base_cell((0, 64, 0), grass);
                ctx.test_base_cell((0, 65, 0), reg.state_of("minecraft:short_grass", None));
            }
            "cold_grass_poppy" => {
                ctx.test_base_cell((0, 64, 0), grass);
                ctx.test_base_cell((0, 65, 0), reg.state_of("minecraft:poppy", None));
            }
            "cold_grass_dandelion" => {
                ctx.test_base_cell((0, 64, 0), grass);
                ctx.test_base_cell((0, 65, 0), reg.state_of("minecraft:dandelion", None));
            }
            "cold_snowy_grass_poppy" => {
                ctx.test_base_cell((0, 64, 0), grass_snowy);
                ctx.test_base_cell((0, 65, 0), reg.state_of("minecraft:poppy", None));
            }
            "cold_grass_existing_snow" => {
                ctx.test_base_cell((0, 64, 0), grass);
                ctx.test_base_cell((0, 65, 0), snow);
            }
            "cold_water_source" => ctx.test_base_cell((0, 63, 0), water0),
            "cold_flowing_water" => ctx.test_base_cell((0, 63, 0), water1),
            "cold_waterlogged_slab" => ctx.test_base_cell((0, 63, 0), slab_waterlogged),
            "cold_ice" => ctx.test_base_cell((0, 64, 0), reg.state_of("minecraft:ice", None)),
            "cold_packed_ice" => {
                ctx.test_base_cell((0, 64, 0), reg.state_of("minecraft:packed_ice", None));
            }
            "cold_soul_sand" => {
                ctx.test_base_cell((0, 64, 0), reg.state_of("minecraft:soul_sand", None));
            }
            "cold_mud" => ctx.test_base_cell((0, 64, 0), reg.state_of("minecraft:mud", None)),
            "cold_oak_leaves" => {
                ctx.test_base_cell((0, 64, 0), reg.state_of("minecraft:oak_leaves", None));
            }
            "cold_slab_bottom" => ctx.test_base_cell((0, 64, 0), slab_bottom),
            "cold_slab_top" => ctx.test_base_cell((0, 64, 0), slab_top),
            "cold_snow8" => ctx.test_base_cell((0, 64, 0), snow8),
            "cold_leaves_above_grass" => {
                ctx.test_base_cell((0, 64, 0), grass);
                ctx.test_base_cell((0, 70, 0), reg.state_of("minecraft:oak_leaves", None));
            }
            "cold_light10_grass" => {
                ctx.test_base_cell((0, 64, 0), grass);
                ctx.test_block_light = 10;
            }
            "cold_light10_water" => {
                ctx.test_base_cell((0, 63, 0), water0);
                ctx.test_block_light = 10;
            }
            "warm_plains" => {
                ctx.test_biome("minecraft:plains");
                ctx.test_base_cell((0, 64, 0), grass);
                ctx.test_base_cell((1, 63, 0), water0);
            }
            "taiga_low_64" => {
                ctx.test_biome("minecraft:taiga");
                ctx.test_base_cell((0, 64, 0), grass);
            }
            "taiga_high_170" => {
                ctx.test_biome("minecraft:taiga");
                ctx.test_base_cell((0, 170, 0), grass);
            }
            "frozen_ocean_grid" => {
                ctx.test_biome("minecraft:frozen_ocean");
                for x in 0..16 {
                    for z in 0..16 {
                        ctx.test_base_cell((x, 63, z), water0);
                    }
                }
            }
            "build_ceiling_319" => ctx.test_base_cell((0, 319, 0), grass),
            "chunk_3_m2_offset" => {
                ctx.test_base_cell((50, 64, -31), grass);
                ctx.test_base_cell((51, 64, -30), grass);
                ctx.test_base_cell((51, 65, -30), reg.state_of("minecraft:short_grass", None));
            }
            other => panic!("unhandled freeze_top_layer oracle scene: {other}"),
        }

        let mut rng = Xor::seed_worldgen(0);
        gen_cf(&Cf::FreezeTop, freg(), &mut ctx, &mut rng, origin);
        let got: Vec<String> = ctx
            .test_writes
            .as_ref()
            .unwrap()
            .iter()
            .map(|&((x, y, z), st)| format!("{x},{y},{z}={}", reg.canonical_state(st)))
            .collect();
        assert_eq!(
            got.join(";"),
            cols[3],
            "native freeze_top_layer mismatch for scene {id}"
        );
        tested += 1;
    }
    assert_eq!(tested, 29);
}

#[test]
fn end_spike_config_parses_explicit_fields_and_rejects_malformed() {
    let valid = jparse(
        br#"{"type":"minecraft:end_spike","config":{"crystal_invulnerable":true,"crystal_beam_target":[12,95,-34],"spikes":[{"centerX":2,"centerZ":3,"radius":4,"height":88,"guarded":true}]}}"#,
    )
    .unwrap();
    let Some(Cf::EndSpike(cfg)) = super::p_cf(&valid) else {
        panic!("expected Cf::EndSpike");
    };
    assert!(cfg.crystal_invulnerable);
    assert_eq!(cfg.crystal_beam_target, Some((12, 95, -34)));
    assert_eq!(
        cfg.spikes,
        vec![super::EndSpike {
            center_x: 2,
            center_z: 3,
            radius: 4,
            height: 88,
            guarded: true,
        }]
    );

    let missing_spikes =
        jparse(br#"{"type":"minecraft:end_spike","config":{"crystal_invulnerable":false}}"#)
            .unwrap();
    assert!(super::p_cf(&missing_spikes).is_none());

    let bad_target = jparse(
        br#"{"type":"minecraft:end_spike","config":{"crystal_invulnerable":false,"crystal_beam_target":[1,2],"spikes":[]}}"#,
    )
    .unwrap();
    assert!(super::p_cf(&bad_target).is_none());
}

#[test]
fn end_spike_matches_native_oracle() {
    let reg = breg().expect("registry");
    let fixture = include_str!("../tests/data/end_spike_1211.tsv");
    let sha = |s: &str| -> String {
        crate::generation_math::sha256(s.as_bytes())
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    };
    let mut tested = 0usize;
    for line in fixture.lines() {
        if line.is_empty() {
            continue;
        }
        let cols: Vec<&str> = line.split('\t').collect();
        assert_eq!(cols.len(), 12, "bad row: {line}");
        let id = cols[0];
        let min_y = cols[1].parse::<i32>().unwrap();
        let height = cols[2].parse::<i32>().unwrap();
        let world_seed = cols[3].parse::<i64>().unwrap() as u64;
        let origin_parts: Vec<i32> = cols[4].split(',').map(|v| v.parse().unwrap()).collect();
        let origin = (origin_parts[0], origin_parts[1], origin_parts[2]);
        let rng_seed = cols[5].parse::<i64>().unwrap() as u64;
        let cf_json = format!(r#"{{"type":"minecraft:end_spike","config":{}}}"#, cols[6]);
        let cf = super::p_cf(&jparse(cf_json.as_bytes()).unwrap())
            .unwrap_or_else(|| panic!("failed to parse end_spike config for {id}"));

        let preset = match (min_y, height) {
            (0, 128) => crate::worldgen::NoisePreset::End,
            (-64, 384) => crate::worldgen::NoisePreset::Overworld,
            _ => panic!("unexpected bounds ({min_y}, {height}) in {id}"),
        };
        let mut g = Gen::load_preset(world_seed, crate::name_to_state, preset).expect("datapack");
        let mut ctx = context(&mut g);
        assert_eq!(ctx.g.min_y, min_y, "{id}");
        assert_eq!(ctx.g.height, height, "{id}");
        ctx.test_no_terrain = true;
        ctx.test_default_air = true;

        let mut rng = Xor::seed_worldgen(rng_seed);
        gen_cf(&cf, freg(), &mut ctx, &mut rng, origin);

        let expect_wrote = cols[7].parse::<bool>().unwrap();
        let expect_count = cols[8].parse::<usize>().unwrap();
        assert_eq!(!ctx.wr.is_empty(), expect_wrote, "wrote mismatch for {id}");
        assert_eq!(ctx.wr.len(), expect_count, "cell count mismatch for {id}");

        let mut cells: Vec<String> = ctx
            .wr
            .iter()
            .map(|(&(x, y, z), &state)| {
                format!("{x},{y},{z} {}\n", block_state_cells_text(reg, state))
            })
            .collect();
        cells.sort();
        assert_eq!(sha(&cells.concat()), cols[9], "cells SHA-256 mismatch for {id}");

        let probe = format!(
            "{},{},{}",
            rng.next_bound(1000),
            rng.next_bound(1000),
            rng.next_bound(1000)
        );
        assert_eq!(probe, cols[10], "rng probe mismatch for {id}");
        tested += 1;
    }
    assert_eq!(tested, 37);
}

/// The walk `BonusChestFeature` takes over the origin chunk, recomputed from `nextInt`
/// draws alone: two `Util.toShuffledList` passes (X first, then Z) before any read, then
/// the cross product with X outermost. The loot seed is the `nextLong` after that walk.
fn bonus_chest_walk(seed: u64, cx: i32, cz: i32) -> (Vec<(i32, i32)>, i64) {
    let mut probe = Xor::seed(seed);
    let mut xs: Vec<i32> = (cx * 16..cx * 16 + 16).collect();
    let mut zs: Vec<i32> = (cz * 16..cz * 16 + 16).collect();
    for list in [&mut xs, &mut zs] {
        for i in (1..list.len()).rev() {
            let j = probe.next_bound(i as i32 + 1) as usize;
            list.swap(i, j);
        }
    }
    let mut walk = Vec::with_capacity(256);
    for &x in &xs {
        for &z in &zs {
            walk.push((x, z));
        }
    }
    (walk, probe.next_i64())
}

/// A scripted flat chunk: stone at `y = 64` over all 16x16 columns of chunk (0,0), so
/// every column has a `MOTION_BLOCKING_NO_LEAVES` surface and the walk never leaves it.
fn bonus_chest_floor(ctx: &mut FC) {
    let stone = breg().unwrap().state_of("minecraft:stone", None);
    for x in 0..16 {
        for z in 0..16 {
            ctx.set(x, 64, z, stone);
        }
    }
}

/// `minecraft:bonus_chest` (`ebo`, `BonusChestFeature`), config `{}`.
///
/// The contract is the draw order plus three effects, so all four are pinned here:
/// the chest lands on the *shuffled* first cell of the origin chunk (not the origin, not
/// the lowest x/z), `RandomizableContainer.setBlockEntityLootTable` spends exactly one
/// `nextLong` on `minecraft:chests/spawn_bonus_chest`, the four horizontal neighbours get
/// a torch where `torch.canSurvive` holds, and one of them loses it when its support cell
/// is air — which is the only thing the native body asks (`canSurvive`), not whether the
/// torch's own cell is empty.
#[test]
fn bonus_chest_places_chest_loot_seed_and_supported_torches() {
    let reg = breg().unwrap();
    let air = reg.state_of("minecraft:air", None);
    let chest = reg.state_of("minecraft:chest", None);
    let torch = reg.state_of("minecraft:torch", None);
    const SEED: u64 = 11;
    // origin inside chunk (0,0); the origin itself is not where the chest goes
    let (walk, loot_seed) = bonus_chest_walk(SEED, 0, 0);
    let (x, z) = walk[0];

    let mut g = Gen::load(0, crate::name_to_state).expect("datapack");
    let mut ctx = context(&mut g);
    ctx.test_no_terrain = true;
    ctx.test_default_air = true;
    bonus_chest_floor(&mut ctx);
    // east neighbour of the first cell: no support, so its torch must not appear
    ctx.set(x + 1, 64, z, air);
    ctx.test_writes = Some(Vec::new());

    let mut rng = Xor::seed(SEED);
    gen_cf(&Cf::BonusChest, freg(), &mut ctx, &mut rng, (5, 70, 5));

    let at = |ctx: &mut FC, p: (i32, i32, i32)| ctx.get(p.0, p.1, p.2);
    assert_eq!(
        at(&mut ctx, (x, 65, z)),
        chest,
        "the chest takes the first walked cell"
    );
    assert_eq!(at(&mut ctx, (x, 66, z)), air, "and nothing above it");
    assert_eq!(
        at(&mut ctx, (x + 1, 65, z)),
        air,
        "no torch without a support below"
    );
    for (dx, dz, name) in [(0, -1, "north"), (0, 1, "south"), (-1, 0, "west")] {
        assert_eq!(
            at(&mut ctx, (x + dx, 65, z + dz)),
            torch,
            "the {name} neighbour stands on the floor, so it gets a torch"
        );
    }
    // `ctx.test_writes` records just the feature's own writes: chest + three torches.
    let writes = ctx.test_writes.clone().unwrap();
    assert_eq!(
        writes.len(),
        4,
        "one chest and three supported torches: {writes:?}"
    );

    // the loot table and the seed it was given: one nextLong after the two shuffles
    assert_eq!(ctx.payloads.len(), 1, "exactly one payload");
    match &ctx.payloads[0].1 {
        crate::generation_payload::GenerationPayload::LootContainer {
            kind,
            loot_table,
            loot_seed: seeded,
        } => {
            assert_eq!(*kind, crate::generation_payload::ContainerKind::Chest);
            assert_eq!(loot_table, "minecraft:chests/spawn_bonus_chest");
            assert_eq!(
                *seeded,
                Some(loot_seed),
                "the seed is the nextLong after the walk"
            );
        }
        other => panic!("the chest payload is a loot container, not {other:?}"),
    }
    assert_eq!(
        ctx.payloads[0].0,
        (x, 65, z),
        "the payload sits on the chest"
    );

    // the stream then continues one nextLong after the walk, and not a draw further
    let mut control = Xor::seed(SEED);
    for _ in 0..2 {
        for i in (1..16).rev() {
            let _ = control.next_bound(i as i32 + 1);
        }
    }
    assert_eq!(control.next_i64(), loot_seed, "the 30 draws, then the seed");
    assert_eq!(
        rng.next_bound(1 << 30),
        control.next_bound(1 << 30),
        "one nextLong, no more"
    );
}

/// The cell test is `isEmptyBlock(pos) || getCollisionShape(level, pos).isEmpty()`, and
/// the heightmap is `MOTION_BLOCKING_NO_LEAVES`: a leaf block above the floor is *at* the
/// heightmap cell (leaves are excluded from that heightmap) and has a collision shape, so
/// the walk skips it; a plant is at its column's heightmap cell too (grass does not block
/// motion) but has no collision shape, so the chest replaces it. A port that read
/// `MOTION_BLOCKING` would put the chest one cell higher, and a port that tested only
/// `isEmptyBlock` would place it in the leaves.
#[test]
fn bonus_chest_skips_a_collision_shape_and_accepts_a_plant() {
    let reg = breg().unwrap();
    let leaves = reg.state_of("minecraft:oak_leaves", None);
    // The extended piston: its shape is the head and the base, so it occludes as a cube without
    // being a solid render. The default (retracted) piston is a plain full cube.
    let piston = reg.state_of(
        "minecraft:piston",
        Some(&jparse(br#"{"extended":"true"}"#).expect("props")),
    );
    let grass = reg.state_of("minecraft:short_grass", None);
    let chest = reg.state_of("minecraft:chest", None);
    // A seed whose first two walked cells are not adjacent: the torch pass reads the four
    // neighbours of the chest, so a blocked cell next to it would be overwritten by a torch
    // and the "skipped" assertion would be testing the torch rule instead of this one.
    let (seed, blocked, accepted, loot_seed) = (0u64..64)
        .find_map(|seed| {
            let (walk, loot_seed) = bonus_chest_walk(seed, 0, 0);
            let (first, second) = (walk[0], walk[1]);
            let apart = (first.0 - second.0).abs() + (first.1 - second.1).abs() > 1;
            apart.then_some((seed, first, second, loot_seed))
        })
        .expect("a seed whose first two walked cells are far apart");

    let mut g = Gen::load(0, crate::name_to_state).expect("datapack");
    let mut ctx = context(&mut g);
    ctx.test_no_terrain = true;
    ctx.test_default_air = true;
    bonus_chest_floor(&mut ctx);
    ctx.set(blocked.0, 65, blocked.1, leaves);
    ctx.set(accepted.0, 65, accepted.1, grass);
    ctx.test_writes = Some(Vec::new());

    let mut rng = Xor::seed(seed);
    gen_cf(&Cf::BonusChest, freg(), &mut ctx, &mut rng, (5, 70, 5));

    assert_eq!(
        ctx.get(blocked.0, 65, blocked.1),
        leaves,
        "a leaf block has a collision shape, so the walk skips its cell"
    );
    assert_eq!(
        ctx.get(accepted.0, 65, accepted.1),
        chest,
        "a plant has no collision shape, so the chest takes its cell"
    );
    assert_eq!(
        ctx.test_writes
            .as_ref()
            .unwrap()
            .iter()
            .filter(|(_, st)| *st == chest)
            .count(),
        1,
        "exactly one chest"
    );
    let seeded = ctx
        .payloads
        .iter()
        .find_map(|(_, p)| match p {
            crate::generation_payload::GenerationPayload::LootContainer { loot_seed, .. } => {
                Some(*loot_seed)
            }
            _ => None,
        })
        .expect("the placed chest carries a loot payload");
    assert_eq!(seeded, Some(loot_seed));
}

/// A column filled to the build limit puts the first free heightmap cell outside the
/// world, and `WorldGenRegion.setBlock` refuses that y before it writes or leaves a
/// `DUMMY` block entity — so the chest and its `nextLong` both disappear while the walk
/// still ends. Without the guard the port writes a chest at `min_y + height` and spends
/// the seed, which is visible here as writes and as a shifted stream.
#[test]
fn bonus_chest_writes_nothing_when_the_heightmap_is_outside_the_world() {
    const SEED_MAX: u64 = 4096;
    let full = (0, 0);
    let seed = (0u64..SEED_MAX)
        .find(|&seed| bonus_chest_walk(seed, 0, 0).0[0] == full)
        .expect("a seed whose first walked cell is (0,0)");

    let reg = breg().unwrap();
    let stone = reg.state_of("minecraft:stone", None);
    let mut g = Gen::load(0, crate::name_to_state).expect("datapack");
    let mut ctx = context(&mut g);
    ctx.test_no_terrain = true;
    ctx.test_default_air = true;
    let top = ctx.g.min_y + ctx.g.height - 1;
    ctx.set(full.0, top, full.1, stone);
    assert_eq!(
        ctx.heightmap(
            crate::block_registry::HeightmapKind::MotionBlockingNoLeaves,
            full.0,
            full.1
        ),
        top + 1,
        "the first free cell of a full column is outside the build height"
    );
    ctx.test_writes = Some(Vec::new());

    let mut rng = Xor::seed(seed);
    gen_cf(&Cf::BonusChest, freg(), &mut ctx, &mut rng, (5, 70, 5));

    assert!(
        ctx.test_writes.as_ref().unwrap().is_empty(),
        "an out-of-world cell takes no write: {:?}",
        ctx.test_writes
    );
    assert!(ctx.payloads.is_empty(), "and no loot-container payload");
    // the two shuffles ran; the loot seed did not
    let mut control = Xor::seed(seed);
    for _ in 0..2 {
        for i in (1..16).rev() {
            let _ = control.next_bound(i as i32 + 1);
        }
    }
    assert_eq!(
        rng.next_bound(1 << 30),
        control.next_bound(1 << 30),
        "no nextLong was spent, so the next draw is the shuffles' successor"
    );
}

/// The nether plant classes do **not** share one `mayPlaceOn`; the old arm read
/// `RootsBlock`'s set off `NetherForestVegetationFeature`, whose origin gate is
/// `#minecraft:nylium`, so the feature fixture could not see the difference.
///
/// * `RootsBlock` (`dmk`) and `NetherSproutsBlock` (`dld`):
///   `state.is(NYLIUM) || state.is(SOUL_SOIL) || super.mayPlaceOn(...)`
/// * `BushBlock.mayPlaceOn` (`dgh`): `state.is(DIRT) || state.is(FARMLAND)`
/// * `FungusBlock` (`dje`) adds `MYCELIUM` and keeps the rest; its `grows_on`
///   block only gates bonemeal (`isValidBonemealTarget`), not placement.
#[test]
fn nether_roots_sprouts_and_fungi_keep_their_own_may_place_on() {
    let reg = breg().unwrap();
    let mut g = Gen::load(0, crate::name_to_state).expect("data pack");
    let mut ctx = context(&mut g);
    // (below, roots/sprouts, fungi) - the pinned `#minecraft:dirt` tag covers dirt,
    // grass_block, podzol, coarse_dirt, mycelium, rooted_dirt, moss_block, mud and
    // muddy_mangrove_roots; farmland is the extra `Blocks.FARMLAND` term.
    for (below, plant_ok) in [
        ("minecraft:crimson_nylium", true),
        ("minecraft:warped_nylium", true),
        ("minecraft:soul_soil", true),
        ("minecraft:dirt", true),
        ("minecraft:grass_block", true),
        ("minecraft:podzol", true),
        ("minecraft:rooted_dirt", true),
        ("minecraft:mud", true),
        ("minecraft:mycelium", true),
        ("minecraft:farmland", true),
        // not in `#dirt`: the BushBlock helper `is_dirtish` accepts it, the pinned
        // tag does not, and `RootsBlock` only adds soul soil to the tag set.
        ("minecraft:dirt_path", false),
        ("minecraft:netherrack", false),
        ("minecraft:soul_sand", false),
        ("minecraft:warped_wart_block", false),
        ("minecraft:air", false),
    ] {
        ctx.set(0, 99, 0, reg.state_of(below, None));
        for plant in ["minecraft:crimson_roots", "minecraft:warped_roots"] {
            assert_eq!(
                super::can_place(reg, &mut ctx, plant, 0, 100, 0),
                plant_ok,
                "{plant} on {below}"
            );
        }
        assert_eq!(
            super::can_place(reg, &mut ctx, "minecraft:nether_sprouts", 0, 100, 0),
            plant_ok,
            "nether_sprouts on {below}"
        );
        for fungus in ["minecraft:crimson_fungus", "minecraft:warped_fungus"] {
            assert_eq!(
                super::can_place(reg, &mut ctx, fungus, 0, 100, 0),
                plant_ok,
                "{fungus} on {below}"
            );
        }
    }
}

/// `BasaltColumnsFeature` (`ebi`) `canPlaceAt`: the cell is air (or ocean lava) and
/// `!world.getBlockState(pos.below()).isAir()`. `isAir()` is the block predicate, so
/// `cave_air` and `void_air` are **not** support - comparing the state name against
/// `minecraft:air` accepted both.
#[test]
fn basalt_columns_need_a_non_air_support() {
    let reg = breg().unwrap();
    let mut g = Gen::load(0, crate::name_to_state).expect("data pack");
    let sea = crate::worldgen::sea_level();
    let mut ctx = context(&mut g);
    for (below, start_ok) in [
        ("minecraft:stone", true),
        ("minecraft:netherrack", true),
        ("minecraft:cave_air", false),
        ("minecraft:void_air", false),
        ("minecraft:air", false),
        // in `CANNOT_PLACE_ON` (`ebi.a`) even though it is not air
        ("minecraft:bedrock", false),
        ("minecraft:magma_block", false),
        ("minecraft:lava", false),
    ] {
        ctx.set(0, 99, 0, reg.state_of(below, None));
        ctx.set(0, 100, 0, reg.state_of("minecraft:air", None));
        assert_eq!(
            super::bc_can_start(&mut ctx, reg, sea, (0, 100, 0)),
            start_ok,
            "support={below}"
        );
    }
}

/// `CountOnEveryLayerPlacement` (`eim`): the pass walks `MOTION_BLOCKING` downward,
/// counts a layer where the cell is `isEmpty() = isAir() || WATER || LAVA` and the
/// cell below is not (`Integer.MAX_VALUE` beyond the column), skips a `BEDROCK`
/// support, and re-samples the count inside **every** layer pass.
#[test]
fn count_on_every_layer_walks_motion_blocking_layers() {
    use crate::generation_providers::{IP, PM};
    let reg = breg().unwrap();
    let mut g = Gen::load(0, crate::name_to_state).expect("data pack");
    let stone = reg.state_of("minecraft:stone", None);
    let lava = reg.state_of("minecraft:lava", None);
    let bedrock = reg.state_of("minecraft:bedrock", None);
    let pf = super::Pf {
        feat: super::FRef::Name(String::new()),
        mods: vec![],
        name: String::new(),
    };
    // Every column of the 16x16 square carries the same profile, because the
    // modifier draws its candidate column from the RNG: stone at 55, 58, 61, 65 and
    // air elsewhere. Four "empty above non-empty" boundaries, top down.
    let boundaries = [66, 62, 59, 56];
    let mut script = |ctx: &mut FC, cells: &[(i32, u32)]| {
        for lx in 0..16 {
            for lz in 0..16 {
                for &(y, st) in cells {
                    ctx.test_base_cell((lx, y, lz), st);
                }
            }
        }
    };
    {
        let mut ctx = context(&mut g);
        ctx.test_default_air = true;
        script(&mut ctx, &[(55, stone), (58, stone), (61, stone), (65, stone)]);
        assert_eq!(
            ctx.heightmap(crate::block_registry::HeightmapKind::MotionBlocking, 3, 7),
            66,
            "MOTION_BLOCKING is the first free cell above the top solid"
        );
        // `uniform(0, 4)` samples the count: `nextInt(5)` per layer pass. The model
        // below is vanilla's loop read off `eim`: the sample is inside the `do`, so a
        // pass that draws 0 ends the walk even when the column has more layers.
        let mut deepest = 0usize;
        for seed in 0..=7u64 {
            let mut probe = Xor::seed(seed);
            let mut expected: Vec<(i32, i32, i32)> = Vec::new();
            let mut layer = 0usize;
            loop {
                let n = probe.next_bound(5);
                let mut any = false;
                for _ in 0..n {
                    let x = probe.next_bound(16);
                    let z = probe.next_bound(16);
                    if let Some(&y) = boundaries.get(layer) {
                        expected.push((x, y, z));
                        any = true;
                    }
                }
                if any {
                    deepest = deepest.max(layer + 1);
                }
                layer += 1;
                if !any {
                    break;
                }
            }
            let mut out = Vec::new();
            super::mod_positions(
                &PM::CountEveryLayer(IP::Uniform(0, 4)),
                freg(),
                &mut ctx,
                &mut Xor::seed(seed),
                (0, 0, 0),
                &pf,
                &mut out,
            );
            assert_eq!(out, expected, "layer walk / per-layer count at seed {seed}");
        }
        assert!(
            deepest >= 2,
            "the fixture must reach at least a second layer: {deepest}"
        );
        // The same walk with a constant count: two positions on each of the four
        // layers, nothing on the exhausted fifth.
        let mut out = Vec::new();
        super::mod_positions(
            &PM::CountEveryLayer(IP::Const(2)),
            freg(),
            &mut ctx,
            &mut Xor::seed(0),
            (0, 0, 0),
            &pf,
            &mut out,
        );
        assert_eq!(out.len(), 8, "{out:?}");
        for y in boundaries {
            assert_eq!(out.iter().filter(|p| p.1 == y).count(), 2, "y={y}: {out:?}");
        }
    }
    // A fluid is `isEmpty()`, so the layer sits in the fluid, not above it: water
    // 62..=64 over stone 61 gives the layer at 62 even though `MOTION_BLOCKING` is 65.
    {
        let mut ctx = context(&mut g);
        ctx.test_default_air = true;
        script(
            &mut ctx,
            &[
                (61, stone),
                (62, reg.state_of("minecraft:water", None)),
                (63, reg.state_of("minecraft:water", None)),
                (64, reg.state_of("minecraft:water", None)),
            ],
        );
        let mut out = Vec::new();
        super::mod_positions(
            &PM::CountEveryLayer(IP::Const(1)),
            freg(),
            &mut ctx,
            &mut Xor::seed(0),
            (0, 0, 0),
            &pf,
            &mut out,
        );
        assert_eq!(out.len(), 1, "{out:?}");
        assert_eq!(out[0].1, 62, "the water cell above the stone: {out:?}");
    }
    // Lava is empty too, and a `BEDROCK` support is skipped: stone 50, air, stone 55,
    // bedrock 56. `MOTION_BLOCKING` is 57, and the cell above the bedrock is a
    // non-empty/empty pair like any other - only the bedrock exclusion drops it, which
    // leaves the layer above stone 55's neighbour at 51.
    {
        let mut ctx = context(&mut g);
        ctx.test_default_air = true;
        script(&mut ctx, &[(50, stone), (55, stone), (56, bedrock)]);
        assert_eq!(
            ctx.heightmap(crate::block_registry::HeightmapKind::MotionBlocking, 1, 1),
            57
        );
        let mut out = Vec::new();
        super::mod_positions(
            &PM::CountEveryLayer(IP::Const(1)),
            freg(),
            &mut ctx,
            &mut Xor::seed(0),
            (0, 0, 0),
            &pf,
            &mut out,
        );
        assert_eq!(out.len(), 1, "the bedrock support is not a layer: {out:?}");
        assert_eq!(out[0].1, 51, "{out:?}");
        // a lava support below the same air is empty, so it is no layer either
        let mut ctx = context(&mut g);
        ctx.test_default_air = true;
        script(&mut ctx, &[(50, stone), (55, lava)]);
        let mut out = Vec::new();
        super::mod_positions(
            &PM::CountEveryLayer(IP::Const(1)),
            freg(),
            &mut ctx,
            &mut Xor::seed(0),
            (0, 0, 0),
            &pf,
            &mut out,
        );
        assert_eq!(out.len(), 1, "{out:?}");
        assert_eq!(out[0].1, 51, "{out:?}");
    }
}

/// `CEL_TRACE`'s layer list is the same walk `cel_find_y` makes, layer by layer.
///
/// The trace exists so a *layer-count* difference between the port and the reference can name
/// the boundary that one side has and the other does not, instead of only "one more layer".
/// A list that disagreed with the placement it annotates would make that reading wrong in the
/// one place it is used - measured on the Nether `(-25,19)+3x3` fixture, where the port's
/// `large_basalt_columns` placement sampled a fifth layer that the reference did not.
#[test]
fn cel_layer_trace_lists_exactly_the_layers_cel_find_y_walks() {
    use crate::block_registry::HeightmapKind;
    let reg = breg().unwrap();
    let mut g = Gen::load(0, crate::name_to_state).expect("data pack");
    let mut ctx = context(&mut g);
    ctx.test_default_air = true;
    // stone at 8, 11 and 14 over a bedrock floor at 3: three layers, top down, and the
    // bedrock support is skipped (`below.is(BEDROCK)`), so nothing at 4.
    ctx.test_base_cell((0, 3, 0), reg.state_of("minecraft:bedrock", None));
    ctx.test_base_cell((0, 8, 0), reg.state_of("minecraft:stone", None));
    ctx.test_base_cell((0, 11, 0), reg.state_of("minecraft:stone", None));
    ctx.test_base_cell((0, 14, 0), reg.state_of("minecraft:stone", None));
    let top = ctx.heightmap(HeightmapKind::MotionBlocking, 0, 0);
    assert_eq!(top, 15, "first free cell above the topmost blocking block");
    let layers = super::cel_layer_ys(&mut ctx, reg, 0, top, 0);
    assert_eq!(
        layers,
        vec![15, 12, 9],
        "the bedrock support is not a layer"
    );
    for (layer, &y) in layers.iter().enumerate() {
        assert_eq!(
            super::cel_find_y(&mut ctx, reg, 0, top, 0, layer as i32),
            Some(y),
            "layer {layer}"
        );
    }
    assert_eq!(
        super::cel_find_y(&mut ctx, reg, 0, top, 0, layers.len() as i32),
        None,
        "a layer past the column answers Integer.MAX_VALUE"
    );
}

/// `FC::get_src` is the provenance-annotated form of `FC::get` that `CEL_COLUMN_TRACE`
/// prints. A trace that cannot name *which* layer answered turns a read disagreement into
/// "the column is different", which is what the `(-25,22)` delta investigation needed to
/// avoid: the same chunk's post-carve dump and a feature-time read disagreed, and only the
/// tag said the feature-time answer came from the terrain read-through cache.
///
/// The invariant is that the state agrees with `FC::get` while the tag names the layer:
/// a feature write is `wr`, a cached terrain read is `rc`, the carver layer is `ovr`, and a
/// fresh `terrain_cell` is `terrain`.
#[test]
fn cel_column_trace_tags_the_layer_that_answered() {
    let reg = breg().unwrap();
    let mut g = Gen::load(0, crate::name_to_state).expect("data pack");
    let mut ctx = context(&mut g);
    ctx.test_default_air = true;
    let stone = reg.state_of("minecraft:stone", None);
    // `test_base_cell` seeds the terrain read-through cache, so the first read is `rc`.
    ctx.test_base_cell((0, 10, 0), stone);
    assert_eq!(ctx.get_src(0, 10, 0), (stone, "rc"));
    // An uncached cell is a fresh terrain read, and that read is what fills the cache.
    assert_eq!(ctx.get_src(0, 11, 0), (0, "terrain"));
    assert_eq!(ctx.get_src(0, 10, 0), (stone, "rc"));
    // A feature write outranks every layer below it.
    ctx.set(0, 10, 0, 0);
    assert_eq!(ctx.get_src(0, 10, 0), (0, "wr"));
    assert_eq!(ctx.get(0, 10, 0), 0, "the tag must not change the answer");
}

/// `WorldGenRegion.ensureCanWrite` (`arb.f_`): the FEATURES step's
/// `blockStateWriteRadius` is 1 chunk, so a write further than one chunk from the
/// decorated chunk is dropped and the writer's next read still sees the old cell.
/// Outside a pass there is no region and nothing to drop - every direct
/// `feature::populate` / `Gen::feat_write` caller (and unit test) relies on that.
#[test]
fn feature_writes_outside_the_population_window_are_dropped() {
    let reg = breg().unwrap();
    let stone = reg.state_of("minecraft:stone", None);
    let mut g = Gen::load(0, crate::name_to_state).expect("data pack");
    g.pop_active = Some((0, 0));
    {
        let mut ctx = context(&mut g);
        ctx.set(0, 64, 0, stone); // own chunk
        ctx.set(16, 64, 0, stone); // (1,0): Chebyshev 1
        ctx.set(-16, 64, -16, stone); // (-1,-1): Chebyshev 1
        ctx.set(32, 64, 0, stone); // (2,0): outside
        ctx.set(0, 64, -32, stone); // (0,-2): outside
        assert!(ctx.wr.contains_key(&(0, 64, 0)));
        assert!(ctx.wr.contains_key(&(16, 64, 0)));
        assert!(ctx.wr.contains_key(&(-16, 64, -16)));
        assert!(!ctx.wr.contains_key(&(32, 64, 0)));
        assert!(!ctx.wr.contains_key(&(0, 64, -32)));
        // a refused write must not raise the feature heightmap tally either
        assert_eq!(ctx.g.feature_height(32, 0, 0), i32::MIN);
    }
    // No active pass: the same far cell is accepted (the diagnostics' direct
    // population path).
    g.pop_active = None;
    {
        let mut ctx = context(&mut g);
        ctx.set(32, 64, 0, stone);
        assert!(ctx.wr.contains_key(&(32, 64, 0)));
    }
}

// ---------- tree: the decorator lists and the trunk's dirt ----------
//
// Both contracts below are draw-stream contracts, not cosmetics: `trunk_vine` consumes
// four `nextInt(3)` per entry of the tree's log list and `leave_vine` four `nextFloat`
// per entry of its leaf list, so a short list or a differently ordered one shifts every
// later decorator draw. Measured on the pinned jungle fixture
// (`/tmp/nat-4242-jungle-temple/world`, chunk (74,-41)): the missing dirt entry alone
// moved 732 differing cells to 489, and the list order (below) took that to 145.

/// `TreeDecorator.Context`'s constructor copies each `Set<BlockPos>` into an
/// `ObjectArrayList` (walking the `HashSet` in bucket order) and sorts it by
/// `BlockPos::getY` with a stable sort. The expected order here is hand-derived from
/// `Vec3i.hashCode = (y + z*31)*31 + x`, the `HashMap` spread `h ^ (h >>> 16)` and a
/// 16-bucket table (`(spread & 15)`), which is what a 5-element set uses.
#[test]
fn tree_decorator_lists_use_hash_bucket_order_then_y() {
    use crate::tree::decorator_list;
    let zero = vec![(0, 0, 0), (1, 0, 0), (0, 0, 1), (0, 0, 2), (-1, 0, 0)];
    // h: 0, 1, 961, 1922, -1 -> buckets 0, 1, 1, 2, 0. Bucket order, insertion order
    // inside a bucket. Not the insertion order.
    assert_eq!(
        decorator_list(&zero),
        vec![(0, 0, 0), (-1, 0, 0), (1, 0, 0), (0, 0, 1), (0, 0, 2)],
        "bucket order, ties in insertion order"
    );
    // A `Set` collapses duplicates before the sort, so the same position cannot feed two
    // decorator draws.
    assert_eq!(
        decorator_list(&[(1, 0, 0), (1, 0, 0), (0, 0, 0)]),
        vec![(0, 0, 0), (1, 0, 0)],
        "duplicates must collapse"
    );
    // The foliage placer emits rows top-down; the decorators see them bottom-up.
    let column = vec![(2, 7, 2), (2, 5, 2), (2, 6, 2), (2, 5, 2)];
    assert_eq!(
        decorator_list(&column),
        vec![(2, 5, 2), (2, 6, 2), (2, 7, 2)],
        "y is the primary key"
    );
}

/// `TrunkPlacer.setDirtUnderneath` (`ehm.a`) writes the config's dirt under the trunk
/// when `force_dirt` is set *or* the cell is not in `BlockTags.LOGS` (`awe.u`), and it
/// writes through the same `BiConsumer` as `placeLog`, so a written dirt position joins
/// the tree's log list. `tree_dirt_joins` pins that list; `tree_dirt_keeps_a_log_below`
/// pins the skip, which is *not* the dirt tag: a trunk grown on another tree's log keeps
/// the log.
#[test]
fn trunk_dirt_joins_the_log_list_and_a_log_below_is_kept() {
    let reg = breg().unwrap();
    let grass = reg.state_of("minecraft:grass_block", None);
    let log = reg.state_of("minecraft:jungle_log", None);
    let dirt = reg.state_of("minecraft:dirt", None);
    let provider = crate::generation_providers::p_bsp(
        &jparse(br#"{"type":"minecraft:simple_state_provider","state":{"Name":"minecraft:dirt"}}"#)
            .expect("inline provider"),
    )
    .map(Box::new);
    assert!(provider.is_some(), "the provider must parse");
    let mut g = Gen::load(0, crate::name_to_state).expect("data pack");
    let mut ctx = context(&mut g);
    ctx.test_no_terrain = true;
    let mut rng = Xor::seed(11);
    // ground below the trunk, air at the trunk base, and a log below a second trunk
    for (x, below) in [(5, grass), (9, log), (13, log)] {
        ctx.test_base_cell((x, 59, 5), below);
        ctx.test_base_cell((x, 60, 5), 0);
    }
    let mut logs = Vec::new();
    crate::tree::set_dirt(
        &mut ctx,
        &mut rng,
        (5, 59, 5),
        &provider,
        false,
        &mut logs,
    );
    assert_eq!(
        logs,
        vec![(5, 59, 5)],
        "the written dirt position is part of the log list"
    );
    assert_eq!(ctx.get(5, 59, 5), dirt, "dirt replaces a non-log cell");
    // A log below is kept, and then there is no position to add to the list.
    let mut logs = Vec::new();
    crate::tree::set_dirt(
        &mut ctx,
        &mut rng,
        (9, 59, 5),
        &provider,
        false,
        &mut logs,
    );
    assert!(logs.is_empty(), "a skipped dirt write adds no log entry");
    assert_eq!(ctx.get(9, 59, 5), log, "the log below must survive");
    // `force_dirt` overrides the log check.
    let mut logs = Vec::new();
    crate::tree::set_dirt(
        &mut ctx,
        &mut rng,
        (13, 59, 5),
        &provider,
        true,
        &mut logs,
    );
    assert_eq!(logs, vec![(13, 59, 5)], "force_dirt writes and records");
    assert_eq!(ctx.get(13, 59, 5), dirt);
}

/// The four `Heightmap.Types` a feature can read are different quantities, and
/// `MC_SPINE_HM_DUMP` now prints them live (via the same `FC::heightmap` accessors this
/// test drives). Read off the pinned jungle chunk (73,-41) at column (1170,-652): the
/// stored vanilla `Heightmaps` are WORLD_SURFACE 70, OCEAN_FLOOR 70, MOTION_BLOCKING 70
/// and MOTION_BLOCKING_NO_LEAVES 69, where the column is `jungle_log` at 68 and
/// `oak_leaves` at 69 - so leaves count for the first three and not for the last.
#[test]
fn live_heightmap_types_differ_on_leaves() {
    use crate::block_registry::HeightmapKind;
    let reg = breg().unwrap();
    let stone = reg.state_of("minecraft:stone", None);
    let leaves = reg.state_of("minecraft:oak_leaves", None);
    // The extended piston: its shape is the head and the base, so it occludes as a cube without
    // being a solid render. The default (retracted) piston is a plain full cube.
    let piston = reg.state_of(
        "minecraft:piston",
        Some(&jparse(br#"{"extended":"true"}"#).expect("props")),
    );
    let mut g = Gen::load(0, crate::name_to_state).expect("data pack");
    let (x, z) = (4, 4);
    let base = g.top_ws(x, z).max(g.top_oc(x, z)) + 2;
    let mut ctx = context(&mut g);
    ctx.set(x, base, z, stone);
    let all: Vec<HeightmapKind> = [
        HeightmapKind::WorldSurface,
        HeightmapKind::OceanFloor,
        HeightmapKind::MotionBlocking,
        HeightmapKind::MotionBlockingNoLeaves,
    ]
    .to_vec();
    for kind in &all {
        assert_eq!(ctx.heightmap(*kind, x, z), base + 1, "{kind:?} on stone");
    }
    ctx.set(x, base + 1, z, leaves);
    assert_eq!(ctx.hm_ws(x, z), base + 2, "leaves raise WORLD_SURFACE");
    assert_eq!(ctx.hm_oc(x, z), base + 2, "leaves raise OCEAN_FLOOR too");
    assert_eq!(
        ctx.heightmap(HeightmapKind::MotionBlocking, x, z),
        base + 2,
        "leaves block motion"
    );
    assert_eq!(
        ctx.heightmap(HeightmapKind::MotionBlockingNoLeaves, x, z),
        base + 1,
        "MOTION_BLOCKING_NO_LEAVES is the one that excludes leaves"
    );
    // A removal lowers the live map: native `Heightmap.update` rescans the column.
    ctx.set(x, base + 1, z, 0);
    assert_eq!(ctx.hm_ws(x, z), base + 1, "removal lowers WORLD_SURFACE");
}

// ---- pinned per-state facts --------------------------------------------------------------
//
// `assets/block_state_facts_1211.bin` records ten `BlockBehaviour.BlockStateBase` answers for all
// 26,684 states, exported by `tools/block_state_facts_from_jar.py` from the pinned server jar.
// Before it, the physical predicates were composed from two other assets: the same values, as it
// turns out, but the *questions* were being re-derived at each call site (`is_solid && side_solid`
// for a face, "has a collision list" for a collision shape), and that is where they drifted.
//
// These tests are the audit that keeps the composition honest: the table's own answers against
// named states vanilla is known by, the table against the older single-column assets, and the
// exact states each old composition got wrong.

/// The three older assets record the same native calls the fact table now owns. They are read
/// here and nowhere else, so a shifted state id or a swapped plane in either exporter shows up
/// as a disagreement instead of as two plausible tables.
#[test]
fn pinned_fact_tables_agree_with_the_older_oracle_assets() {
    let reg = breg().expect("registry");
    let states = reg.state_count();
    let facts = include_bytes!("../assets/block_state_facts_1211.bin");
    assert_eq!(facts.len(), states * 5, "one five-byte record per state");
    let predicates = include_bytes!("../assets/block_predicates_1211.bin");
    let collision_full = include_bytes!("../assets/collision_shape_full_1211.bin");
    let mineshaft = include_bytes!("../assets/mineshaft_support_1211.bin");
    let mut disagreements = Vec::new();
    for st in 0..states as u32 {
        let i = st as usize;
        if reg.is_solid_render(st) != (predicates[i] & 1 != 0) {
            disagreements.push(format!("solid render {st}"));
        }
        if reg.is_collision_shape_full_block(st) != (collision_full[i] != 0) {
            disagreements.push(format!("collision full {st}"));
        }
        // The mineshaft asset packs FULL faces in the low six bits and CENTER(down) in bit 6.
        for face in 0..6u8 {
            if reg.mineshaft_sturdy_face(st, face) != (mineshaft[i] & (1 << face) != 0) {
                disagreements.push(format!("FULL face {face} {st}"));
            }
        }
        if reg.is_face_sturdy(st, 0, crate::block_registry::Support::Center)
            != (mineshaft[i] & 64 != 0)
        {
            disagreements.push(format!("CENTER down {st}"));
        }
    }
    assert!(
        disagreements.is_empty(),
        "the pinned tables disagree on {} answers, first: {:?}",
        disagreements.len(),
        &disagreements[..disagreements.len().min(8)]
    );
}

/// The states that separate the ten predicates from one another. Every assertion here is a
/// vanilla fact read from the jar, and each one moves if two bit constants are swapped.
#[test]
fn state_facts_answer_the_predicates_vanilla_separates() {
    use crate::block_registry::Support;
    let reg = breg().expect("registry");
    let air = reg.state_of("minecraft:air", None);
    let cave_air = reg.state_of("minecraft:cave_air", None);
    let stone = reg.state_of("minecraft:stone", None);
    let water = reg.state_of("minecraft:water", None);
    let glass = reg.state_of("minecraft:glass", None);
    let leaves = reg.state_of("minecraft:oak_leaves", None);
    // The extended piston: its shape is the head and the base, so it occludes as a cube without
    // being a solid render. The default (retracted) piston is a plain full cube.
    let piston = reg.state_of(
        "minecraft:piston",
        Some(&jparse(br#"{"extended":"true"}"#).expect("props")),
    );
    let glowstone = reg.state_of("minecraft:glowstone", None);
    let dirt_path = reg.state_of("minecraft:dirt_path", None);
    let grass = reg.state_of("minecraft:short_grass", None);
    let torch = reg.state_of("minecraft:torch", None);
    let propagule = reg.state_of(
        "minecraft:mangrove_propagule",
        Some(&jparse(br#"{"waterlogged":"true"}"#).expect("props")),
    );
    let chest = reg.state_of("minecraft:chest", None);
    let fence = reg.state_of("minecraft:oak_fence", None);

    // isAir is three states, not "id zero or the pumpkin flag": cave air is a distinct id.
    assert!(reg.is_air(air) && reg.is_air(cave_air) && !reg.is_air(stone));
    assert!(reg.is_air(reg.state_of("minecraft:void_air", None)));

    // blocksMotion, liquid() and "carries a fluid state" are three different questions.
    assert!(!reg.blocks_motion(water) && reg.is_liquid_block(water) && reg.is_liquid(water));
    assert!(reg.blocks_motion(stone) && !reg.is_liquid_block(stone) && !reg.is_liquid(stone));
    // A waterlogged plant holds water without being a liquid block. The registry keeps the
    // fluid-state meaning under `is_liquid`, which is what the generation call sites ask.
    assert!(!reg.is_liquid_block(propagule) && reg.is_liquid(propagule));

    // canOcclude is not isSolidRender. Every solid render is an occluder, but 14,747 states
    // occlude as a full cube without being one: a piston and a dirt path have a full occlusion
    // shape over a collision shape that is not a cube, and glass and leaves are neither.
    assert!(reg.can_occlude(piston) && !reg.is_solid_render(piston));
    assert!(reg.can_occlude(dirt_path) && !reg.is_solid_render(dirt_path));
    assert!(reg.can_occlude(stone) && reg.is_solid_render(stone));
    assert!(!reg.can_occlude(glass) && !reg.is_solid_render(glass));
    assert!(!reg.can_occlude(leaves) && !reg.is_solid_render(leaves));

    // The collision shape is not the outline: a plant and a torch are drawn and collide with
    // nothing, and a dirt path collides without covering a face.
    assert!(reg.is_collision_empty(grass) && reg.is_collision_empty(torch));
    assert!(!reg.is_collision_empty(stone) && !reg.is_collision_empty(dirt_path));
    assert!(reg.is_collision_shape_full_block(stone) && !reg.is_collision_shape_full_block(dirt_path));
    // The registry's collision list and the native collision shape agree on 26,683 of the 26,684
    // states. The exception is a pitcher crop's upper half at age 0, where the asset lists no box
    // and vanilla does collide - so the fact table is the exact owner of the predicate.
    let pitcher_upper = reg.state_of(
        "minecraft:pitcher_crop",
        Some(&jparse(br#"{"half":"upper","age":"0"}"#).expect("props")),
    );
    assert!(!reg.is_collision_empty(pitcher_upper));
    assert!(reg.collision_boxes(pitcher_upper).next().is_none());
    assert!(reg.is_collision_empty(grass) && reg.collision_boxes(grass).next().is_none());

    // Light: emission 15 on glowstone, 0 on stone; block 15 through stone, 0 through glass.
    assert_eq!(reg.luminance_of(glowstone), 15);
    assert_eq!(reg.luminance_of(stone), 0);
    assert_eq!(reg.opacity_of(stone), 15);
    assert_eq!(reg.opacity_of(glass), 0);
    // A dirt path occludes as a cube but damps no light: `canOcclude` and `getLightBlock` are
    // separate columns for exactly this state.
    assert!(reg.can_occlude(dirt_path) && reg.opacity_of(dirt_path) == 0);

    // Face support: a stone supports every face at FULL; a fence post supports the centre of
    // every face but covers no whole face - which is why `canSupportCenter` is a separate
    // question and why a torch may stand on it.
    let arms_props = jparse(br#"{"north":"true","south":"true","east":"true","west":"true"}"#)
        .expect("props");
    let fence_arms = reg.state_of("minecraft:oak_fence", Some(&arms_props));
    for dir in 0..6u8 {
        assert!(reg.is_face_sturdy(stone, dir, Support::Full));
        assert!(!reg.is_face_sturdy(fence, dir, Support::Full));
        // The default fence state is the isolated post: its centre answer is vertical only, and
        // the state with four arms covers the centre of every face. Same block, different id.
        assert_eq!(
            reg.is_face_sturdy(fence, dir, Support::Center),
            dir < 2,
            "isolated fence post centre, direction {dir}"
        );
        assert!(reg.is_face_sturdy(fence_arms, dir, Support::Center));
    }
    // The extended piston covers exactly the one face its head points at while its centre answer
    // covers five, and a chest supports the centre of its down face and nothing else.
    assert_eq!(reg.face_plane(piston, Support::Full).count_ones(), 1);
    assert!(reg.face_plane(piston, Support::Center).count_ones() > 1);
    assert!(reg.is_face_sturdy(chest, 0, Support::Center));
    assert!(!reg.is_face_sturdy(chest, 1, Support::Center));
}

/// The states the old `is_solid(st) && side_solid(st, dir)` composition denied, and the states a
/// FULL-only answer denies to a caller that asked `canSupportCenter`. Both lists are measured.
#[test]
fn face_support_answers_the_support_type_the_caller_asks_for() {
    use crate::block_registry::Support;
    let reg = breg().expect("registry");
    // A repeater, a comparator, a snow layer and a carpet cover their whole down face and do not
    // block motion, so the old `is_solid && side_solid` gate answered "no support" on the very
    // face their shape covers. The ladder covers one whole side (its facing) instead of the down
    // face, and the gate denied that one too.
    for name in [
        "minecraft:repeater",
        "minecraft:comparator",
        "minecraft:snow",
        "minecraft:white_carpet",
    ] {
        let st = reg.state_of(name, None);
        assert!(!reg.blocks_motion(st), "{name} blocks motion after all");
        assert_eq!(
            reg.face_plane(st, Support::Full),
            1,
            "{name} covers its down face"
        );
    }
    let ladder = reg.state_of("minecraft:ladder", None);
    assert!(!reg.blocks_motion(ladder));
    assert_eq!(
        reg.face_plane(ladder, Support::Full).count_ones(),
        1,
        "a ladder covers exactly the face it hangs on"
    );
    // The FULL plane and the CENTER plane disagree in both directions, which is why the support
    // type is an argument rather than a re-derivation at the call site.
    let fence = reg.state_of("minecraft:oak_fence", None);
    assert!(!reg.is_face_sturdy(fence, 1, Support::Full));
    assert!(reg.is_face_sturdy(fence, 1, Support::Center));
    let slab = reg.state_of("minecraft:oak_slab", None);
    assert!(reg.is_face_sturdy(slab, 0, Support::Full));
    assert!(!reg.is_face_sturdy(slab, 1, Support::Full));
    // RIGID equals FULL for every state except the six container shapes, whose open top face
    // answers RIGID without covering a full face.
    let cauldron = reg.state_of("minecraft:cauldron", None);
    assert!(!reg.is_face_sturdy(cauldron, 1, Support::Full));
    assert!(reg.is_face_sturdy(cauldron, 1, Support::Rigid));
    let stone = reg.state_of("minecraft:stone", None);
    assert!(reg.is_face_sturdy(stone, 1, Support::Rigid));
}
