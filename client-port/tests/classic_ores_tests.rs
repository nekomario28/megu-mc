// SPDX-License-Identifier: LGPL-3.0-or-later

use megu_client_mesh::classic_ores::*;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn enabled_config() -> ClassicOresConfig {
    ClassicOresConfig {
        enabled: true,
        ..ClassicOresConfig::default()
    }
}

fn input<'a>(block_id: &'a str, rgba: &'a [u8]) -> OreTextureInput<'a> {
    OreTextureInput {
        block_id,
        tags: Vec::new(),
        width: 16,
        height: 16,
        has_mcmeta_animation: false,
        has_custom_model: false,
        has_external_pack_override: false,
        rgba,
    }
}

/// Builds a synthetic 16x16 ore texture: `bg` background with `vein` pixels
/// sprinkled at deterministic positions (~44 vein px — inside 0.06..=0.48).
fn synth_mod_ore(bg: [u8; 3], vein: [u8; 3]) -> Vec<u8> {
    let mut rgba = vec![0u8; 1024];
    for y in 0..16usize {
        for x in 0..16usize {
            let i = (y * 16 + x) * 4;
            let is_vein = (x * 7 + y * 11) % 9 == 0;
            let c = if is_vein { vein } else { bg };
            rgba[i..i + 4].copy_from_slice(&[c[0], c[1], c[2], 255]);
        }
    }
    rgba
}

const STONE_BG: [u8; 3] = [126, 126, 126];
const ZINC_TEAL: [u8; 3] = [110, 200, 185];

fn pixel_at(rgba: &[u8], x: usize, y: usize) -> [u8; 4] {
    let i = (y * 16 + x) * 4;
    [rgba[i], rgba[i + 1], rgba[i + 2], rgba[i + 3]]
}

// ---------------------------------------------------------------------------
// Default-off behaviour
// ---------------------------------------------------------------------------

#[test]
fn default_config_is_disabled() {
    let cfg = ClassicOresConfig::default();
    assert!(!cfg.enabled);
    assert_eq!(cfg.scope, ClassicOresScope::VanillaAndStandardModOres);
    assert!(cfg.respect_external_resourcepack);
    assert!(cfg.denylist_mods.is_empty());
    assert!(cfg.denylist_blocks.is_empty());
    assert!(cfg.allowlist_blocks.is_empty());
}

#[test]
fn disabled_pack_preserves_every_ore() {
    let cfg = ClassicOresConfig::default();
    for id in [
        "minecraft:iron_ore",
        "minecraft:copper_ore",
        "create:zinc_ore",
    ] {
        let i = input(id, &[]);
        assert_eq!(
            evaluate_ore_adaptation(&cfg, &i),
            OreAdaptationDecision::PreserveOriginal(PreserveReason::PackDisabled),
            "{id} must be preserved while the pack is off"
        );
    }
}

// ---------------------------------------------------------------------------
// Vanilla ores
// ---------------------------------------------------------------------------

#[test]
fn vanilla_ores_synthesize_unified_1024b_textures() {
    let cfg = enabled_config();
    for id in [
        "minecraft:iron_ore",
        "minecraft:diamond_ore",
        "minecraft:copper_ore",
        "minecraft:deepslate_copper_ore",
        "minecraft:nether_gold_ore",
        "minecraft:coal_ore",
        "minecraft:redstone_ore",
        "minecraft:lapis_ore",
        "minecraft:emerald_ore",
        "minecraft:nether_quartz_ore",
    ] {
        let i = input(id, &[]);
        let OreAdaptationDecision::SynthesizeClassic(plan) = evaluate_ore_adaptation(&cfg, &i)
        else {
            panic!("{id} should synthesize");
        };
        let out = synthesize_classic_ore_16x16(&plan, None);
        assert_eq!(out.len(), 1024);
        assert!(out.as_chunks::<4>().0.iter().all(|px| px[3] == 255), "{id} alpha");
        // The unified mask paints palette colours verbatim.
        let p = plan.palette.primary;
        assert!(
            out.as_chunks::<4>().0.iter().any(|px| px[..3] == p),
            "{id} must contain its primary vein colour {p:?}"
        );
    }
}

#[test]
fn deepslate_and_nether_variants_use_matching_hosts() {
    let cfg = enabled_config();
    let deepslate = input("minecraft:deepslate_iron_ore", &[]);
    let OreAdaptationDecision::SynthesizeClassic(p1) = evaluate_ore_adaptation(&cfg, &deepslate)
    else {
        panic!()
    };
    assert_eq!(p1.host, HostRock::Deepslate);

    let nether = input("minecraft:nether_gold_ore", &[]);
    let OreAdaptationDecision::SynthesizeClassic(p2) = evaluate_ore_adaptation(&cfg, &nether)
    else {
        panic!()
    };
    assert_eq!(p2.host, HostRock::Netherrack);

    // Deepslate background is visibly darker than stone, netherrack is warm.
    let deep = synthesize_classic_ore_16x16(&p1, None);
    let stone_plan = SynthesisPlan {
        host: HostRock::Stone,
        palette: p1.palette,
    };
    let stone = synthesize_classic_ore_16x16(&stone_plan, None);
    let neth = synthesize_classic_ore_16x16(&p2, None);
    let rock_lum = |t: &[u8], x: usize, y: usize| {
        let px = pixel_at(t, x, y);
        px[0] as u32 + px[1] as u32 + px[2] as u32
    };
    assert!(rock_lum(&deep, 8, 14) < rock_lum(&stone, 8, 14));
    let warm = pixel_at(&neth, 8, 14);
    assert!(warm[0] > warm[1] + 20, "netherrack must be warm red");
}

#[test]
fn special_vanilla_blocks_are_always_preserved() {
    let cfg = enabled_config();
    for id in [
        "minecraft:ancient_debris",
        "minecraft:gilded_blackstone",
        "minecraft:budding_amethyst",
        "minecraft:amethyst_cluster",
        "mymod:astral_crystal_block",
        "mymod:quartz_geode",
    ] {
        let i = input(id, &[]);
        assert_eq!(
            evaluate_ore_adaptation(&cfg, &i),
            OreAdaptationDecision::PreserveOriginal(PreserveReason::SpecialUniqueOre),
            "{id} is unique art and must never be restyled"
        );
    }
}

#[test]
fn copper_palette_has_orange_body_and_verdigris_accent() {
    let pal = curated_vanilla_palette("minecraft:copper_ore").expect("copper is curated");
    // Warm copper orange body.
    assert!(pal.primary[0] > 150 && pal.primary[0] > pal.primary[1]);
    assert!(pal.primary[1] > pal.primary[2]);
    // Teal-green verdigris patina highlight: green+blue dominate red.
    assert!(pal.highlight[1] > pal.highlight[0]);
    assert!(pal.highlight[2] > pal.highlight[0]);

    let cfg = enabled_config();
    let i = input("minecraft:copper_ore", &[]);
    let OreAdaptationDecision::SynthesizeClassic(plan) = evaluate_ore_adaptation(&cfg, &i) else {
        panic!()
    };
    let out = synthesize_classic_ore_16x16(&plan, None);
    let has = |c: [u8; 3]| out.as_chunks::<4>().0.iter().any(|px| px[..3] == c);
    assert!(has(plan.palette.primary), "warm copper orange present");
    assert!(has(plan.palette.highlight), "verdigris patina accent present");
    assert!(has(plan.palette.shadow), "copper shadow tone present");
}

// ---------------------------------------------------------------------------
// Standard stone-hosted mod ores
// ---------------------------------------------------------------------------

#[test]
fn standard_mod_ore_passes_and_extracts_vein_colour() {
    let cfg = enabled_config();
    let zinc = synth_mod_ore(STONE_BG, ZINC_TEAL);
    let i = input("create:zinc_ore", &zinc);

    let OreAdaptationDecision::SynthesizeClassic(plan) = evaluate_ore_adaptation(&cfg, &i) else {
        panic!("standard stone-hosted mod ore must synthesize")
    };
    assert_eq!(plan.host, HostRock::Stone);
    assert_eq!(
        plan.palette.primary, ZINC_TEAL,
        "primary must be the exact extracted teal"
    );

    let out = synthesize_classic_ore_16x16(&plan, None);
    assert!(out
        .as_chunks::<4>().0.iter()
        .any(|px| px[..3] == ZINC_TEAL));
}

#[test]
fn mod_ore_excluded_under_vanilla_only_scope() {
    let cfg = ClassicOresConfig {
        enabled: true,
        scope: ClassicOresScope::VanillaOnly,
        ..ClassicOresConfig::default()
    };
    let zinc = synth_mod_ore(STONE_BG, ZINC_TEAL);
    let i = input("create:zinc_ore", &zinc);
    assert_eq!(
        evaluate_ore_adaptation(&cfg, &i),
        OreAdaptationDecision::PreserveOriginal(PreserveReason::ExcludedByScope)
    );
    // Vanilla still works under the narrow scope.
    let iron = input("minecraft:iron_ore", &[]);
    assert!(matches!(
        evaluate_ore_adaptation(&cfg, &iron),
        OreAdaptationDecision::SynthesizeClassic(_)
    ));
}

#[test]
fn deepslate_hosted_mod_ore_detects_deepslate() {
    let cfg = enabled_config();
    let ore = synth_mod_ore([62, 62, 66], ZINC_TEAL);
    let i = input("create:deepslate_zinc_ore", &ore);
    let OreAdaptationDecision::SynthesizeClassic(plan) = evaluate_ore_adaptation(&cfg, &i) else {
        panic!()
    };
    assert_eq!(plan.host, HostRock::Deepslate);
}

// ---------------------------------------------------------------------------
// Fail-safe preservation guards — every one verified
// ---------------------------------------------------------------------------

#[test]
fn animated_ores_are_preserved() {
    let cfg = enabled_config();
    // .mcmeta animation descriptor present.
    let mut i = input("mymod:starlight_ore", &[]);
    i.has_mcmeta_animation = true;
    assert_eq!(
        evaluate_ore_adaptation(&cfg, &i),
        OreAdaptationDecision::PreserveOriginal(PreserveReason::AnimatedTexture)
    );
    // 16x32 vertical animation strip.
    let mut i = input("mymod:starlight_ore", &[]);
    i.height = 32;
    assert_eq!(
        evaluate_ore_adaptation(&cfg, &i),
        OreAdaptationDecision::PreserveOriginal(PreserveReason::AnimatedTexture)
    );
}

#[test]
fn hd_art_ores_are_preserved() {
    let cfg = enabled_config();
    let mut i = input("mymod:hd_ruby_ore", &[]);
    i.width = 32;
    i.height = 32;
    assert_eq!(
        evaluate_ore_adaptation(&cfg, &i),
        OreAdaptationDecision::PreserveOriginal(PreserveReason::NonStandardResolution)
    );
}

#[test]
fn custom_model_ores_are_preserved() {
    let cfg = enabled_config();
    let mut i = input("mymod:cluster_ore", &[]);
    i.has_custom_model = true;
    assert_eq!(
        evaluate_ore_adaptation(&cfg, &i),
        OreAdaptationDecision::PreserveOriginal(PreserveReason::CustomModel)
    );
    // Even a vanilla ore with a custom model stays untouched.
    let mut i = input("minecraft:iron_ore", &[]);
    i.has_custom_model = true;
    assert_eq!(
        evaluate_ore_adaptation(&cfg, &i),
        OreAdaptationDecision::PreserveOriginal(PreserveReason::CustomModel)
    );
}

#[test]
fn stylized_non_stone_mod_ore_is_preserved() {
    let cfg = enabled_config();
    // Glowing purple astral block: purple field, lighter purple veins — no
    // standard rock background at all.
    let astral = synth_mod_ore([120, 40, 200], [190, 120, 255]);
    let i = input("mymod:astral_ore", &astral);
    assert_eq!(
        evaluate_ore_adaptation(&cfg, &i),
        OreAdaptationDecision::PreserveOriginal(PreserveReason::UniqueModArtStyle)
    );
}

#[test]
fn missing_pixel_data_fails_closed_for_mod_ores() {
    let cfg = enabled_config();
    let i = input("mymod:mystery_ore", &[]);
    assert_eq!(
        evaluate_ore_adaptation(&cfg, &i),
        OreAdaptationDecision::PreserveOriginal(PreserveReason::UniqueModArtStyle)
    );
}

#[test]
fn external_resourcepack_always_wins() {
    let cfg = enabled_config();
    let mut i = input("minecraft:diamond_ore", &[]);
    i.has_external_pack_override = true;
    assert_eq!(
        evaluate_ore_adaptation(&cfg, &i),
        OreAdaptationDecision::PreserveOriginal(PreserveReason::ExternalPackPriority)
    );

    // Only when the user disables the respect flag does the pack take over.
    let cfg = ClassicOresConfig {
        enabled: true,
        respect_external_resourcepack: false,
        ..ClassicOresConfig::default()
    };
    assert!(matches!(
        evaluate_ore_adaptation(&cfg, &i),
        OreAdaptationDecision::SynthesizeClassic(_)
    ));
}

#[test]
fn denylist_blocks_and_mods_are_preserved() {
    let zinc = synth_mod_ore(STONE_BG, ZINC_TEAL);

    let mut cfg = enabled_config();
    cfg.denylist_blocks.insert("create:zinc_ore".to_string());
    let i = input("create:zinc_ore", &zinc);
    assert_eq!(
        evaluate_ore_adaptation(&cfg, &i),
        OreAdaptationDecision::PreserveOriginal(PreserveReason::Denylisted)
    );

    let mut cfg = enabled_config();
    cfg.denylist_mods.insert("create".to_string());
    assert_eq!(
        evaluate_ore_adaptation(&cfg, &i),
        OreAdaptationDecision::PreserveOriginal(PreserveReason::Denylisted)
    );

    // Denylist wins even for vanilla blocks.
    let mut cfg = enabled_config();
    cfg.denylist_blocks.insert("iron_ore".to_string());
    let iron = input("minecraft:iron_ore", &[]);
    assert_eq!(
        evaluate_ore_adaptation(&cfg, &iron),
        OreAdaptationDecision::PreserveOriginal(PreserveReason::Denylisted)
    );
}

#[test]
fn allowlist_bypasses_pixel_heuristic_but_not_other_guards() {
    // Weird glowing ore would normally be UniqueModArtStyle; the explicit
    // allowlist trusts the user and restyles it anyway (palette extracted).
    let cfg = {
        let mut c = enabled_config();
        c.allowlist_blocks.insert("mymod:astral_ore".to_string());
        c
    };
    let mut astral = synth_mod_ore([120, 40, 200], [190, 120, 255]);
    let i = input("mymod:astral_ore", &astral);
    assert!(matches!(
        evaluate_ore_adaptation(&cfg, &i),
        OreAdaptationDecision::SynthesizeClassic(_)
    ));

    // But animation is still a hard guard even for allowlisted blocks.
    astral.extend_from_slice(&[0u8; 1024]);
    let mut i = input("mymod:astral_ore", &astral);
    i.height = 32;
    assert_eq!(
        evaluate_ore_adaptation(&cfg, &i),
        OreAdaptationDecision::PreserveOriginal(PreserveReason::AnimatedTexture)
    );
}

#[test]
fn non_ore_blocks_are_never_touched() {
    let cfg = enabled_config();
    for id in [
        "minecraft:stone",
        "minecraft:oak_planks",
        "mymod:machine_casing",
    ] {
        let i = input(id, &[]);
        assert_eq!(
            evaluate_ore_adaptation(&cfg, &i),
            OreAdaptationDecision::PreserveOriginal(PreserveReason::NotAnOre),
            "{id} is not an ore"
        );
    }
}

#[test]
fn ore_tags_and_ore_suffix_mark_mod_blocks() {
    let cfg = enabled_config();
    let zinc = synth_mod_ore(STONE_BG, ZINC_TEAL);

    // Tag-driven detection for a non-`*_ore` name.
    let mut i = input("mymod:galena", &zinc);
    i.tags = vec!["c:ores".to_string()];
    assert!(matches!(
        evaluate_ore_adaptation(&cfg, &i),
        OreAdaptationDecision::SynthesizeClassic(_)
    ));
    let mut i = input("mymod:galena", &zinc);
    i.tags = vec!["forge:ores".to_string(), "minecraft:iron_ores".to_string()];
    assert!(matches!(
        evaluate_ore_adaptation(&cfg, &i),
        OreAdaptationDecision::SynthesizeClassic(_)
    ));
}

// ---------------------------------------------------------------------------
// Synthesizer properties
// ---------------------------------------------------------------------------

#[test]
fn synthesis_is_deterministic_and_opaque() {
    let plan = SynthesisPlan {
        host: HostRock::Stone,
        palette: curated_vanilla_palette("minecraft:diamond_ore").unwrap(),
    };
    let a = synthesize_classic_ore_16x16(&plan, None);
    let b = synthesize_classic_ore_16x16(&plan, None);
    assert_eq!(a, b, "synthesis must be byte-deterministic");
    assert_eq!(a.len(), CLASSIC_ORE_BYTES);
    assert!(a.as_chunks::<4>().0.iter().all(|px| px[3] == 255));
}

#[test]
fn all_three_palettes_layers_appear() {
    for id in [
        "minecraft:coal_ore",
        "minecraft:iron_ore",
        "minecraft:copper_ore",
        "minecraft:diamond_ore",
        "minecraft:emerald_ore",
    ] {
        let palette = curated_vanilla_palette(id).unwrap();
        let out = synthesize_classic_ore_16x16(
            &SynthesisPlan {
                host: HostRock::Stone,
                palette,
            },
            None,
        );
        for tone in [palette.highlight, palette.primary, palette.shadow] {
            assert!(
                out.as_chunks::<4>().0.iter().any(|px| px[..3] == tone),
                "{id} missing tone {tone:?}"
            );
        }
    }
}

#[test]
fn caller_host_texture_is_reused_as_background() {
    // A "base pack" stone texture with a distinctive marker colour.
    let mut host = vec![0u8; 1024];
    for px in host.as_chunks_mut::<4>().0.iter_mut() {
        px.copy_from_slice(&[140, 138, 130, 255]);
    }
    let plan = SynthesisPlan {
        host: HostRock::Stone,
        palette: curated_vanilla_palette("minecraft:iron_ore").unwrap(),
    };
    let out = synthesize_classic_ore_16x16(&plan, Some(&host));
    let marker = pixel_at(&out, 8, 14); // interior non-vein pixel
    // Reused host pixels are the caller's colour (interior, unshaded).
    let vein = plan.palette;
    let is_vein = marker[..3] == vein.highlight
        || marker[..3] == vein.primary
        || marker[..3] == vein.shadow;
    if !is_vein {
        assert_eq!(marker[..3], [140, 138, 130]);
    }
    // Veins still stamped on top.
    assert!(out
        .as_chunks::<4>().0.iter()
        .any(|px| px[..3] == plan.palette.primary));
}

#[test]
fn short_host_rgba_falls_back_to_procedural() {
    let plan = SynthesisPlan {
        host: HostRock::Deepslate,
        palette: curated_vanilla_palette("minecraft:gold_ore").unwrap(),
    };
    let short = [255u8; 64];
    let out = synthesize_classic_ore_16x16(&plan, Some(&short));
    assert!(out.as_chunks::<4>().0.iter().all(|px| px[3] == 255));
}

#[test]
fn extract_palette_recovers_mod_vein_colour() {
    let zinc = synth_mod_ore(STONE_BG, ZINC_TEAL);
    let pal = extract_ore_vein_palette(&zinc, HostRock::Stone).expect("veins present");
    assert_eq!(pal.primary, ZINC_TEAL);
    // Derived tones bracket the primary colour.
    assert!(pal.highlight[1] >= pal.primary[1]);
    assert!(pal.shadow[1] <= pal.primary[1]);

    assert!(extract_ore_vein_palette(&[], HostRock::Stone).is_none());
    assert!(extract_ore_vein_palette(&zinc[..512], HostRock::Stone).is_none());
}

#[test]
fn unified_mask_is_shared_across_ores() {
    // Every ore stamps the same silhouette: vein pixel positions must match
    // between two unrelated minerals.
    let iron = synthesize_classic_ore_16x16(
        &SynthesisPlan {
            host: HostRock::Stone,
            palette: curated_vanilla_palette("minecraft:iron_ore").unwrap(),
        },
        None,
    );
    let lapis = synthesize_classic_ore_16x16(
        &SynthesisPlan {
            host: HostRock::Stone,
            palette: curated_vanilla_palette("minecraft:lapis_ore").unwrap(),
        },
        None,
    );
    let iron_pal = curated_vanilla_palette("minecraft:iron_ore").unwrap();
    let lapis_pal = curated_vanilla_palette("minecraft:lapis_ore").unwrap();
    let is_vein = |px: &[u8], pal: &OreVeinPalette| {
        px[..3] == pal.highlight || px[..3] == pal.primary || px[..3] == pal.shadow
    };
    for (a, b) in iron.as_chunks::<4>().0.iter().zip(lapis.as_chunks::<4>().0.iter()) {
        assert_eq!(
            is_vein(a, &iron_pal),
            is_vein(b, &lapis_pal),
            "vein mask must be identical for every ore"
        );
    }
}
