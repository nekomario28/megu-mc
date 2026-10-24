use super::{freg, gen_cf, Cf};
use crate::block_registry::breg;
use crate::generation_context::FC;
use crate::generation_math::{normal_new_l, Xor};
use crate::generation_providers::{p_hp, p_ip, test_pred, test_rt, Bsp, Pr, RT};
use crate::json::jparse;
use crate::worldgen::Gen;

fn context(g: &mut Gen) -> FC<'_> {
    FC::new(g)
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
            vals: vec![0, 3, 7],
        },
        Bsp::RandomInt {
            src: Box::new(Bsp::Simple(wheat)),
            prop: "age".into(),
            vals: vec![],
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
    // (the height providers now follow ehz/eib/ehv and below_top is off by one).
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
fn context_overrides_heights_bounds_and_biomes_keep_existing_results() {
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
    // Existing monotonic height overrides intentionally survive an air overwrite.
    assert_eq!(ctx.hm_ws(x, z), y + 3);
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
