use super::*;

fn weird_next_up(v: f64) -> f64 {
    if v == 0.0 {
        f64::from_bits(1)
    } else if v > 0.0 {
        f64::from_bits(v.to_bits() + 1)
    } else {
        f64::from_bits(v.to_bits() - 1)
    }
}

fn weird_next_down(v: f64) -> f64 {
    if v == 0.0 {
        f64::from_bits(0x8000_0000_0000_0001)
    } else if v > 0.0 {
        f64::from_bits(v.to_bits() - 1)
    } else {
        f64::from_bits(v.to_bits() + 1)
    }
}

fn weird_reachable(g: &Gen) -> (Vec<usize>, Vec<bool>) {
    let mut seen = vec![false; g.dfs.len()];
    let mut found = Vec::new();
    let mut kids = Vec::new();
    let mut stack = vec![g.final_density];
    while let Some(id) = stack.pop() {
        if seen[id] {
            continue;
        }
        seen[id] = true;
        if matches!(&g.dfs[id], DF::WeirdScaledSampler { .. }) {
            found.push(id);
        }
        kids.clear();
        g.push_kids(id, &mut kids);
        for &kid in &kids {
            if !seen[kid] {
                stack.push(kid);
            }
        }
    }
    (found, seen)
}

fn weird_reaches(g: &Gen, root: usize, target: usize) -> bool {
    let mut seen = vec![false; g.dfs.len()];
    let mut kids = Vec::new();
    let mut stack = vec![root];
    while let Some(id) = stack.pop() {
        if id == target {
            return true;
        }
        if seen[id] {
            continue;
        }
        seen[id] = true;
        kids.clear();
        g.push_kids(id, &mut kids);
        stack.extend(kids.iter().copied());
    }
    false
}

#[test]
fn weird_sampler_rarity_thresholds_are_strict_across_adjacent_bits() {
    let t1 = RarityValueMapper::Type1;
    let t2 = RarityValueMapper::Type2;
    let nan = f64::NAN;
    let cases1 = [
        (f64::NEG_INFINITY, 0.75),
        (weird_next_down(-0.5), 0.75),
        (-0.5, 1.0),
        (weird_next_up(-0.5), 1.0),
        (weird_next_down(0.0), 1.0),
        (-0.0, 1.5),
        (0.0, 1.5),
        (weird_next_up(0.0), 1.5),
        (weird_next_down(0.5), 1.5),
        (0.5, 2.0),
        (weird_next_up(0.5), 2.0),
        (f64::INFINITY, 2.0),
        (nan, 2.0),
    ];
    for (input, expected) in cases1 {
        assert_eq!(
            t1.rarity(input),
            expected,
            "TYPE1 input={input:?} bits={:016x}",
            input.to_bits()
        );
    }
    let cases2 = [
        (f64::NEG_INFINITY, 0.5),
        (weird_next_down(-0.75), 0.5),
        (-0.75, 0.75),
        (weird_next_up(-0.75), 0.75),
        (weird_next_down(-0.5), 0.75),
        (-0.5, 1.0),
        (weird_next_up(-0.5), 1.0),
        (weird_next_down(0.0), 1.0),
        (-0.0, 1.0),
        (0.0, 1.0),
        (weird_next_down(0.5), 1.0),
        (0.5, 2.0),
        (weird_next_up(0.5), 2.0),
        (weird_next_down(0.75), 2.0),
        (0.75, 3.0),
        (weird_next_up(0.75), 3.0),
        (f64::INFINITY, 3.0),
        (nan, 3.0),
    ];
    for (input, expected) in cases2 {
        assert_eq!(
            t2.rarity(input),
            expected,
            "TYPE2 input={input:?} bits={:016x}",
            input.to_bits()
        );
    }
}

#[test]
fn weird_sampler_requires_known_mapper_input_and_noise() {
    let mut g = Gen::load(0, crate::name_to_state).expect("pinned pack must load");
    let empty = HashMap::new();
    let noise = "minecraft:spaghetti_2d";
    let cases = [
        (
            "unknown mapper",
            format!(r#"{{"type":"minecraft:weird_scaled_sampler","input":0.25,"noise":"{noise}","rarity_value_mapper":"type_3"}}"#),
        ),
        (
            "missing mapper",
            format!(r#"{{"type":"minecraft:weird_scaled_sampler","input":0.25,"noise":"{noise}"}}"#),
        ),
        (
            "missing input",
            format!(r#"{{"type":"minecraft:weird_scaled_sampler","noise":"{noise}","rarity_value_mapper":"type_1"}}"#),
        ),
        (
            "missing noise",
            r#"{"type":"minecraft:weird_scaled_sampler","input":0.25,"rarity_value_mapper":"type_1"}"#.to_string(),
        ),
        (
            "unresolved input reference",
            r#"{"type":"minecraft:weird_scaled_sampler","input":"minecraft:no_such_density_for_test","noise":"minecraft:spaghetti_2d","rarity_value_mapper":"type_1"}"#.to_string(),
        ),
        (
            "missing noise file",
            r#"{"type":"minecraft:weird_scaled_sampler","input":0.25,"noise":"minecraft:no_such_noise_for_test","rarity_value_mapper":"type_1"}"#.to_string(),
        ),
    ];
    for (label, json) in cases {
        let j = jparse(json.as_bytes()).expect("test JSON");
        assert!(
            g.parse_df(&j, &empty).is_none(),
            "weird_scaled_sampler accepted {label}"
        );
    }
}

#[test]
fn weird_sampler_failed_named_reference_is_retried_not_cached_as_zero() {
    let mut g = Gen::load(0, crate::name_to_state).expect("pinned pack must load");
    let name = "test:weird_sampler_retry";
    let mut files = HashMap::new();
    files.insert(
        name.to_string(),
        br#"{"type":"minecraft:weird_scaled_sampler","input":0.25,"noise":"minecraft:spaghetti_2d","rarity_value_mapper":"type_3"}"#.to_vec(),
    );
    assert!(g.resolve_ref(&files, name).is_none(), "bad mapper resolved");
    assert!(
        g.resolve_ref(&files, name).is_none(),
        "a failed named density was cached as a successful placeholder"
    );
    assert!(
        !g.names.contains_key(name),
        "failed density name stayed cached"
    );
    files.insert(
        name.to_string(),
        br#"{"type":"minecraft:weird_scaled_sampler","input":0.25,"noise":"minecraft:spaghetti_2d","rarity_value_mapper":"type_2"}"#.to_vec(),
    );
    let id = g.resolve_ref(&files, name).expect("retry after failure");
    match &g.dfs[id] {
        DF::WeirdScaledSampler { mapper, .. } => {
            assert_eq!(*mapper, RarityValueMapper::Type2)
        }
        _ => panic!("retry produced a non-weird node"),
    }
    assert_eq!(g.resolve_ref(&files, name), Some(id));
}

#[test]
fn weird_sampler_production_evaluation_scales_all_axes_with_real_normal() {
    let g = Gen::load(0, crate::name_to_state).expect("pinned pack must load");
    let (nodes, _) = weird_reachable(&g);
    assert!(!nodes.is_empty(), "no reachable weird_scaled_sampler nodes");
    let mut checked = 0;
    for &id in &nodes {
        let (input, ni, mapper) = match &g.dfs[id] {
            DF::WeirdScaledSampler { input, ni, mapper } => (*input, *ni, *mapper),
            _ => unreachable!(),
        };
        let mut found_positive = false;
        let mut memo = HashMap::new();
        'search: for y in (-64..=320).step_by(16) {
            for z in (-64..=64).step_by(8) {
                for x in (-64..=64).step_by(8) {
                    let actual = g.eval(id, x, y, z, &mut memo);
                    assert!(actual.is_finite(), "weird node {id} produced {actual}");
                    if actual <= 0.0 {
                        continue;
                    }
                    let mut input_memo = HashMap::new();
                    let raw = g.eval(input, x, y, z, &mut input_memo);
                    let rarity = mapper.rarity(raw);
                    let expected = rarity
                        * g.noises[ni]
                            .get(x as f64 / rarity, y as f64 / rarity, z as f64 / rarity)
                            .abs();
                    assert_eq!(
                        actual.to_bits(),
                        expected.to_bits(),
                        "weird node {id} at ({x},{y},{z}) rarity={rarity}"
                    );
                    found_positive = true;
                    break 'search;
                }
            }
        }
        assert!(
            found_positive,
            "weird node {id} had no positive sample in the test window"
        );
        checked += 1;
    }
    assert_eq!(checked, nodes.len());
}

#[test]
fn weird_sampler_input_is_reachable_through_the_interpolated_node() {
    let mut g = Gen::load(0, crate::name_to_state).expect("pinned pack must load");
    assert!(!g.noises.is_empty(), "pinned pack loaded no noises");
    let value = g.dfs.len();
    g.dfs.push(DF::C(0.25));
    let interp = g.dfs.len();
    g.dfs.push(DF::Interp(value));
    let weird = g.dfs.len();
    g.dfs.push(DF::WeirdScaledSampler {
        input: interp,
        ni: 0,
        mapper: RarityValueMapper::Type1,
    });
    g.final_density = weird;
    let mut kids = Vec::new();
    g.push_kids(weird, &mut kids);
    assert_eq!(kids, vec![interp], "weird input missing from push_kids");
    let interps = g.interp_ids();
    assert!(
        interps.contains(&interp),
        "weird input missing from interpolated reachability: {interps:?}"
    );
}

#[test]
fn weird_sampler_installed_pack_has_three_reachable_typed_nodes() {
    let g = Gen::load(0, crate::name_to_state).expect("pinned pack must load");
    let (nodes, seen) = weird_reachable(&g);
    let mut type1 = Vec::new();
    let mut type2 = Vec::new();
    for &id in &nodes {
        match &g.dfs[id] {
            DF::WeirdScaledSampler {
                mapper: RarityValueMapper::Type1,
                ..
            } => type1.push(id),
            DF::WeirdScaledSampler {
                mapper: RarityValueMapper::Type2,
                ..
            } => type2.push(id),
            _ => {}
        }
    }
    if type1.len() != 2 || type2.len() != 1 {
        let mut details = Vec::new();
        for &id in &nodes {
            let mapper = match &g.dfs[id] {
                DF::WeirdScaledSampler { mapper, .. } => *mapper,
                _ => unreachable!(),
            };
            let mut roots: Vec<String> = g
                .names
                .iter()
                .filter(|(_, &root)| seen[root] && weird_reaches(&g, root, id))
                .map(|(name, _)| name.clone())
                .collect();
            roots.sort();
            roots.dedup();
            details.push(format!("id {id} {mapper:?} via {roots:?}"));
        }
        panic!(
            "expected 3 reachable weird nodes (2 type_1, 1 type_2), got {} type_1 + {} type_2: {:?}",
            type1.len(),
            type2.len(),
            details
        );
    }
}

fn weird_nodes<'a>(j: &'a J, out: &mut Vec<&'a J>) {
    match j {
        J::Obj(fields) => {
            if j.get("type").map(J::str) == Some("minecraft:weird_scaled_sampler") {
                out.push(j);
            }
            for (_, child) in fields {
                weird_nodes(child, out);
            }
        }
        J::Arr(items) => {
            for child in items {
                weird_nodes(child, out);
            }
        }
        _ => {}
    }
}

#[test]
fn weird_sampler_installed_nodes_are_nonzero() {
    let mut g = Gen::load(0, crate::name_to_state).expect("pinned pack");
    let files = Gen::df_files();
    let mut count = 0;
    for (name, bytes) in &files {
        let json = jparse(bytes).expect("installed density JSON");
        let mut nodes = Vec::new();
        weird_nodes(&json, &mut nodes);
        for node in nodes {
            count += 1;
            let id = g
                .node(node, &files)
                .expect("installed weird sampler must parse");
            let values: Vec<_> = [(17, -23, 41), (-97, 63, -131), (4097, -61, -8189)]
                .into_iter()
                .map(|(x, y, z)| g.eval(id, x, y, z, &mut HashMap::new()))
                .collect();
            assert!(
                values.iter().any(|value| *value > 0.0),
                "{name}: sampler still zero: {values:?}"
            );
        }
    }
    assert_eq!(count, 3, "installed sampler coverage changed");
}

fn weird_bits(j: &J) -> u64 {
    u64::from_str_radix(j.str(), 16).expect("raw f64 hex")
}

#[test]
fn weird_sampler_matches_native_mapper_and_samples() {
    let fixture = jparse(include_bytes!(
        "../evidence/foundation/weird-sampler/vanilla.json"
    ))
    .unwrap();
    assert_eq!(fixture.get("minecraft").unwrap().str(), "1.21.1");
    assert_eq!(
        fixture.get("server_sha1").unwrap().str(),
        "59353fb40c36d304f2035d51e7d6e6baa98dc05c"
    );
    // Keep both generators alive: these tests must not confuse pointer-keyed
    // noise-cache entries by recycling a generator's allocation between seeds.
    let mut generators = [
        Gen::load(0, crate::name_to_state).unwrap(),
        Gen::load(42, crate::name_to_state).unwrap(),
    ];
    let mut counts = [0, 0];
    let files = HashMap::new();
    for row in fixture.get("cases").unwrap().as_arr().unwrap() {
        let ident = row.get("id").unwrap().str();
        let mapper_name = row.get("mapper").unwrap().str();
        let mapper = RarityValueMapper::parse(mapper_name).unwrap();
        let input = f64::from_bits(weird_bits(row.get("input_bits").unwrap()));
        let expected = row.get("expected").unwrap();
        let rarity = mapper.rarity(input);
        assert_eq!(
            rarity.to_bits(),
            weird_bits(expected.get("rarity_bits").unwrap()),
            "{ident}: rarity"
        );
        match row.get("op").unwrap().str() {
            "mapper" => counts[0] += 1,
            "sample" => {
                let g = match row.get("seed").unwrap().num() as u64 {
                    0 => &mut generators[0],
                    42 => &mut generators[1],
                    seed => panic!("unsupported corpus seed {seed}"),
                };
                let pos: Vec<_> = row
                    .get("pos")
                    .unwrap()
                    .as_arr()
                    .unwrap()
                    .iter()
                    .map(|n| n.num() as i32)
                    .collect();
                assert_eq!(pos.len(), 3, "{ident}: position shape");
                let noise = row.get("noise").unwrap().str();
                let ni = g.noise_id(noise).unwrap();
                let scaled = [
                    pos[0] as f64 / rarity,
                    pos[1] as f64 / rarity,
                    pos[2] as f64 / rarity,
                ];
                let scaled_bits = expected.get("scaled_bits").unwrap().as_arr().unwrap();
                assert_eq!(scaled_bits.len(), 3, "{ident}: scaled coordinate shape");
                for (value, bits) in scaled.iter().zip(scaled_bits) {
                    assert_eq!(
                        value.to_bits(),
                        weird_bits(bits),
                        "{ident}: scaled coordinate"
                    );
                }
                let raw = g.noises[ni].get(scaled[0], scaled[1], scaled[2]);
                assert_eq!(
                    raw.to_bits(),
                    weird_bits(expected.get("noise_bits").unwrap()),
                    "{ident}: NormalNoise boundary"
                );
                let json = format!(
                    r#"{{"type":"minecraft:weird_scaled_sampler","input":{input},"noise":"{noise}","rarity_value_mapper":"{mapper_name}"}}"#
                );
                let node = g.node(&jparse(json.as_bytes()).unwrap(), &files).unwrap();
                let actual = g.eval(node, pos[0], pos[1], pos[2], &mut HashMap::new());
                assert_eq!(
                    actual.to_bits(),
                    weird_bits(expected.get("value_bits").unwrap()),
                    "{ident}: sampler value"
                );
                counts[1] += 1;
            }
            other => panic!("unexpected oracle operation {other}"),
        }
    }
    assert_eq!(counts, [38, 216], "native corpus coverage changed");
}

#[test]
fn weird_sampler_converted_noise_is_unchanged() {
    let mut rng = Xor::seed(0).splitter().of_str("minecraft:spaghetti_2d");
    let noise = normal_new(&mut rng, -7, 1.0, &[1.0]);
    // Pre-change converted-encoding regression pins, not native-parity values.
    for ((x, y, z), expected) in [
        ((1.25, 17.5, -23.75), 0x3fe8_5b6f_697b_cb46),
        ((-97.5, 32.25, 131.0), 0xbfe0_c1c6_d52f_f834),
        ((30000000.0, -64.0, -30000000.0), 0xbfce_35b6_467c_a0cd),
    ] {
        assert_eq!(noise.get(x, y, z).to_bits(), expected);
    }
}

#[test]
fn weird_sampler_uses_the_callers_interpolation_context() {
    let mut g = Gen::load(0, crate::name_to_state).unwrap();
    let ni = g.noise_id("minecraft:spaghetti_3d_1").unwrap();
    let value = g.dfs.len();
    g.dfs.push(DF::C(-1.0));
    let interp = g.dfs.len();
    g.dfs.push(DF::Interp(value));
    let node = g.dfs.len();
    g.dfs.push(DF::WeirdScaledSampler {
        input: interp,
        ni,
        mapper: RarityValueMapper::Type1,
    });
    let mut cc = ChunkCols {
        grids: vec![None; g.dfs.len()],
        surf: [0; 256],
        ny: 2,
        min_y: 0,
        mats: (0..256).map(|_| None).collect(),
        rd: [0; 256],
        wtop: [0; 256],
    };
    cc.grids[interp] = Some(vec![1.0; 5 * 5 * 2].into_boxed_slice());
    let actual = g.eval_c(node, 3, 4, 5, &mut HashMap::new(), Some(&cc));
    let expected = 2.0 * g.noises[ni].get(1.5, 2.0, 2.5).abs();
    assert!(actual > 0.0);
    assert_eq!(actual.to_bits(), expected.to_bits());
    assert_ne!(
        actual.to_bits(),
        g.eval(node, 3, 4, 5, &mut HashMap::new()).to_bits(),
        "context control must differ"
    );
}
