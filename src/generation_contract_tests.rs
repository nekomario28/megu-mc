use crate::block_registry::breg;
use crate::generation_context::FC;
use crate::generation_math::{normal_new_l, Xor};
use crate::generation_providers::{p_hp, p_ip, test_pred, test_rt, Pr, BSP, RT};
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
        BSP::Simple(stone),
        BSP::Rotated {
            base: log,
            axis: true,
        },
        BSP::Rotated {
            base: stone,
            axis: false,
        },
        BSP::Weighted(vec![(-1, water), (1, dirt), (3, stone)]),
        BSP::Weighted(vec![]),
        BSP::Weighted(vec![(-1, dirt)]),
        BSP::RandomInt {
            src: Box::new(BSP::Simple(wheat)),
            prop: "age".into(),
            vals: vec![0, 3, 7],
        },
        BSP::RandomInt {
            src: Box::new(BSP::Simple(wheat)),
            prop: "age".into(),
            vals: vec![],
        },
        BSP::Noise {
            n: noise(),
            scale: 0.2,
            states: vec![stone, dirt, water],
        },
        BSP::Noise {
            n: noise(),
            scale: 0.2,
            states: vec![],
        },
        BSP::NoiseThr {
            n: noise(),
            scale: 0.2,
            thr: f32::INFINITY,
            hchance: 1.0,
            def: stone,
            low: vec![dirt, water],
            high: vec![log],
        },
        BSP::NoiseThr {
            n: noise(),
            scale: 0.2,
            thr: f32::INFINITY,
            hchance: 1.0,
            def: stone,
            low: vec![],
            high: vec![log],
        },
        BSP::NoiseThr {
            n: noise(),
            scale: 0.2,
            thr: f32::NEG_INFINITY,
            hchance: 1.0,
            def: stone,
            low: vec![],
            high: vec![log, dirt],
        },
        BSP::NoiseThr {
            n: noise(),
            scale: 0.2,
            thr: f32::NEG_INFINITY,
            hchance: 0.0,
            def: stone,
            low: vec![],
            high: vec![log],
        },
        BSP::NoiseThr {
            n: noise(),
            scale: 0.2,
            thr: f32::NEG_INFINITY,
            hchance: 1.0,
            def: stone,
            low: vec![],
            high: vec![],
        },
        BSP::Dual {
            n: noise(),
            scale: 0.2,
            variety: (1, 3),
            slow_scale: 0.03,
            slow_variety: (1, 2),
            states: vec![stone, dirt, water, log, wheat, 0],
        },
        BSP::Dual {
            n: noise(),
            scale: 0.2,
            variety: (1, 3),
            slow_scale: 0.03,
            slow_variety: (1, 2),
            states: vec![],
        },
        BSP::RuleBased {
            fb: Some(Box::new(BSP::Simple(dirt))),
            rules: vec![
                (Pr::Not(Box::new(Pr::True)), BSP::Simple(log)),
                (Pr::True, BSP::Weighted(vec![(1, stone), (2, water)])),
            ],
        },
        BSP::RuleBased {
            fb: Some(Box::new(BSP::Simple(dirt))),
            rules: vec![],
        },
        BSP::RuleBased {
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
        std::fs::write("evidence/foundation/module-layers/provider-baseline.bin", &trace).unwrap();
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
