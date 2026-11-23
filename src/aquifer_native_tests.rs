use super::*;

fn aq_state_name(raw: &str) -> Option<u32> {
    let name = raw
        .strip_prefix("Block{")
        .and_then(|s| s.split_once('}'))
        .map(|(s, _)| s)
        .expect("BlockState rendering");
    if name == "minecraft:air" {
        Some(0)
    } else {
        crate::name_to_state(name)
    }
}

#[cfg(megu_vanilla_data)]
#[test]
fn aquifer_matches_the_pinned_chunk_through_its_public_api() {
    let fixture = jparse(include_bytes!(
        "../evidence/foundation/aquifer/vanilla.json"
    ))
    .unwrap();
    assert_eq!(fixture.get("minecraft").unwrap().str(), "1.21.1");
    assert_eq!(
        fixture.get("server_sha1").unwrap().str(),
        "59353fb40c36d304f2035d51e7d6e6baa98dc05c"
    );
    let request = fixture.get("request").unwrap();
    let result = fixture.get("result").unwrap();
    assert_eq!(request.get("seed").unwrap().num(), 0.0);
    let chunk = request.get("chunk").unwrap().as_arr().unwrap();
    assert_eq!(chunk.len(), 2);
    assert_eq!(chunk[0].num() as i32, 7);
    assert_eq!(chunk[1].num() as i32, 4);

    let mut g = Gen::load(0, crate::name_to_state).unwrap();
    let idwj = g
        .prelim_df
        .expect("initial_density_without_jaggedness resolved");
    let density = result.get("density").unwrap().as_arr().unwrap();
    let aligned: std::collections::BTreeSet<(i32, i32)> = request
        .get("prelim")
        .unwrap()
        .as_arr()
        .unwrap()
        .iter()
        .map(|p| {
            let p = p.as_arr().unwrap();
            (p[0].num() as i32 & !3, p[1].num() as i32 & !3)
        })
        .collect();
    assert_eq!(
        density.len(),
        aligned.len() * 49,
        "raw density coverage changed"
    );
    for row in density {
        let row = row.as_arr().unwrap();
        let (x, y, z) = (
            row[0].num() as i32,
            row[1].num() as i32,
            row[2].num() as i32,
        );
        let want = u64::from_str_radix(row[3].str(), 16).unwrap();
        let got = g.eval(idwj, x, y, z, &mut HashMap::new()).to_bits();
        assert_eq!(got, want, "initial_density_without_jaggedness {x},{y},{z}");
    }
    let prelim = result.get("prelim").unwrap().as_arr().unwrap();
    let mut columns = 0;
    for row in prelim {
        let row = row.as_arr().unwrap();
        let (x, z, want) = (
            row[0].num() as i32,
            row[1].num() as i32,
            row[2].num() as i32,
        );
        assert_eq!(g.prelim_est(x, z), want, "prelim {x},{z}");
        columns += 1;
    }
    assert_eq!(columns, 64, "native preliminary-surface coverage changed");

    let states = result.get("state").unwrap().as_arr().unwrap();
    let mut names = std::collections::BTreeSet::new();
    for row in states {
        let row = row.as_arr().unwrap();
        let (x, y, z) = (
            row[0].num() as i32,
            row[1].num() as i32,
            row[2].num() as i32,
        );
        let density = row[3].num();
        let want = match &row[4] {
            J::Null => None,
            J::Str(s) => aq_state_name(s),
            other => panic!("unexpected state rendering {other:?}"),
        };
        let got = g.aq_state(x, y, z, density);
        assert_eq!(got, want, "aquifer state {x},{y},{z} d={density}");
        names.insert(match &row[4] {
            J::Null => None,
            J::Str(s) => Some(s.clone()),
            other => panic!("unexpected state rendering {other:?}"),
        });
    }
    assert_eq!(states.len(), 150, "native aquifer-state coverage changed");
    assert!(names.contains(&None), "barrier/solid coverage");
    let raw = |prefix: &str| {
        names
            .iter()
            .any(|n| matches!(n, Some(s) if s.starts_with(prefix)))
    };
    assert!(raw("Block{minecraft:air"), "air coverage");
    assert!(raw("Block{minecraft:water"), "water coverage");
    assert!(raw("Block{minecraft:lava"), "lava coverage");
}

#[test]
fn aquifer_deep_level_reads_erosion_and_depth_in_f32() {
    let mut g = Gen::load(0, crate::name_to_state).unwrap();
    let c = g.climate.expect("climate router");
    g.dfs.push(DF::C(0.5));
    let flood = g.dfs.len() - 1;
    g.dfs.push(DF::C(0.0));
    let spread = g.dfs.len() - 1;
    {
        let aq = g.aq.as_mut().expect("aquifer");
        aq.flood = flood;
        aq.spread = spread;
    }
    // OverworldBiomeBuilder.isDeepDark: erosion < -0.225F && depth > 0.9F.
    // Continents is deliberately non-deep so a wrong field selection fails.
    g.dfs[c[2]] = DF::C(0.5);
    g.dfs[c[3]] = DF::C(-1.0);
    g.dfs[c[4]] = DF::C(1.0);
    assert_eq!(g.aq_fluid_y(0, 0, 0, (0, 1), 100, false), AQ_MIN);
    g.dfs[c[3]] = DF::C(f64::from(-0.225f32));
    assert_eq!(g.aq_fluid_y(0, 0, 0, (0, 1), 100, false), 20);
    // Between the f64 literal -0.225 and the promoted f32 boundary: the native
    // f32 threshold is crossed, a de-promoted f64 threshold is not.
    g.dfs[c[3]] = DF::C(-0.224999997);
    assert_eq!(g.aq_fluid_y(0, 0, 0, (0, 1), 100, false), AQ_MIN);
    g.dfs[c[3]] = DF::C(-1.0);
    g.dfs[c[4]] = DF::C(f64::from(0.9f32));
    assert_eq!(g.aq_fluid_y(0, 0, 0, (0, 1), 100, false), 20);
}
