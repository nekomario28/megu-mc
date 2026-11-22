// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later

//! Lane GEN-MOD-CREATE-DEFLATE: real production Modrinth JARs are ~100%
//! Deflate-compressed (`method == 8`). These clean-room tests build
//! synthetic JARs whose entries are deflated in-test — mostly via raw
//! RFC 1951 *stored* blocks (a valid deflate stream the pure-`std`
//! inflater decodes), plus one genuine Huffman-coded blob produced by a
//! stock zlib compressor — and prove every Create 1.21.1 machine family
//! plus the generic `<ns>:<machine>` fallback is discovered with
//! `skipped_compressed_entries == 0`.

use create_adapter::addon_profile::{
    discover_addon_from_jar, AddonProcessingRecipe, AssemblyStepKind,
};
use create_adapter::data_loader::{load_from_stored_zip, CreateRecipeKind, ItemFluidIdTable};
use mod_offload::recipe_processor::ResourceKind;

/// Encode `data` as a raw RFC 1951 deflate stream of stored blocks — a
/// valid `method == 8` ZIP payload exercising the inflater without an
/// external compressor.
fn deflate_stored_blocks(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut chunks = data.chunks(u16::MAX as usize).peekable();
    if chunks.peek().is_none() {
        out.push(0x01); // BFINAL=1, BTYPE=00
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0xFFFFu16.to_le_bytes());
        return out;
    }
    while let Some(chunk) = chunks.next() {
        let last = chunks.peek().is_none();
        out.push(u8::from(last));
        let len = chunk.len() as u16;
        out.extend_from_slice(&len.to_le_bytes());
        out.extend_from_slice(&(!len).to_le_bytes());
        out.extend_from_slice(chunk);
    }
    out
}

/// Genuine zlib-compressed (dynamic-Huffman) raw-deflate blob for
/// `MY_MOD_MIXING_JSON` below — proves `method == 8` decoding works on real
/// compressor output, not only stored blocks.
const ZLIB_DEFLATE_MIXING: &[u8] = &[
    85, 142, 221, 10, 194, 48, 12, 70, 223, 37, 215, 69, 167, 176, 11, 251, 42, 50, 74, 105, 179,
    89, 108, 179, 97, 90, 112, 142, 190, 187, 241, 103, 56, 111, 66, 66, 114, 206, 151, 5, 242, 60,
    33, 104, 72, 179, 73, 163, 215, 41, 220, 3, 13, 160, 64, 234, 13, 125, 64, 202, 12, 250, 188,
    64, 182, 195, 239, 202, 23, 206, 188, 127, 4, 114, 80, 213, 178, 42, 250, 88, 130, 55, 156,
    173, 187, 138, 224, 61, 109, 196, 99, 204, 72, 102, 136, 150, 89, 182, 54, 141, 133, 50, 232,
    67, 219, 212, 78, 193, 13, 185, 196, 111, 210, 150, 234, 11, 163, 55, 147, 37, 20, 198, 125,
    144, 163, 116, 23, 75, 78, 34, 155, 221, 233, 149, 191, 202, 218, 70, 253, 209, 28, 229, 233,
    218, 213, 39,
];

/// 1.21.1-style mixing recipe: `{"tag"}` ingredient + `{"type":
/// "fluid_stack"}` fluid input + `{"id"}` item result + `{"amount","id"}`
/// fluid result.
const MY_MOD_MIXING_JSON: &[u8] = br#"{"type":"my_mod:mixing","ingredients":[{"tag":"my_mod:dusts/zinc"},{"type":"fluid_stack","fluid":"my_mod:molten_glass","amount":150}],"results":[{"id":"my_mod:fused_pane","count":2,"chance":0.9},{"amount":50,"id":"my_mod:slag"}]}"#;

/// Build a JAR byte slice where every entry is `method == 8` unless marked
/// otherwise; `(name, payload, deflate)`.
fn build_mixed_zip(entries: &[(&str, &[u8], bool)]) -> Vec<u8> {
    let mut out = Vec::new();
    for (name, data, deflate) in entries {
        let payload;
        let (method, comp) = if *deflate {
            payload = deflate_stored_blocks(data);
            (8u16, payload.as_slice())
        } else {
            (0u16, *data)
        };
        out.extend_from_slice(&0x0403_4b50u32.to_le_bytes());
        out.extend_from_slice(&20u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&method.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(&(comp.len() as u32).to_le_bytes());
        out.extend_from_slice(&(data.len() as u32).to_le_bytes());
        out.extend_from_slice(&(name.len() as u16).to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(name.as_bytes());
        out.extend_from_slice(comp);
    }
    out
}

/// Build a JAR where each entry's compressed payload is supplied verbatim
/// (used for the real zlib blob).
fn build_zip_raw(entries: &[(&str, u16, &[u8], u32)]) -> Vec<u8> {
    let mut out = Vec::new();
    for (name, method, comp, uncomp_len) in entries {
        out.extend_from_slice(&0x0403_4b50u32.to_le_bytes());
        out.extend_from_slice(&20u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&method.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(&(comp.len() as u32).to_le_bytes());
        out.extend_from_slice(&uncomp_len.to_le_bytes());
        out.extend_from_slice(&(name.len() as u16).to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(name.as_bytes());
        out.extend_from_slice(comp);
    }
    out
}

/// An unknown namespace `my_mod` JAR — every member Deflate-compressed —
/// carrying every real Create 1.21.1 machine family plus generic recipes.
fn my_mod_deflated_jar() -> Vec<u8> {
    build_mixed_zip(&[
        (
            "assets/my_mod/blockstates/alloy_mixer.json",
            br#"{"variants": {"axis=x": {}, "axis=y": {}, "axis=z": {}}}"#,
            true,
        ),
        (
            "data/my_mod/tags/block/mixers.json",
            br#"{"values": ["my_mod:alloy_mixer"]}"#,
            true,
        ),
        (
            "data/my_mod/recipe/compacting/fused_block.json",
            br#"{"type":"my_mod:compacting","ingredients":[{"item":"my_mod:shard"},{"item":"my_mod:shard"},{"type":"neoforge:single","amount":100,"fluid":"minecraft:lava"}],"results":[{"id":"my_mod:fused_block"}]}"#,
            true,
        ),
        (
            "data/my_mod/recipe/splashing/washed_ore.json",
            br#"{"type":"my_mod:splashing","ingredients":[{"tag":"my_mod:dirty_ores"}],"results":[{"id":"my_mod:clean_ore","count":2,"chance":0.75},{"id":"my_mod:grit","chance":0.25}]}"#,
            true,
        ),
        (
            "data/my_mod/recipe/haunting/cursed_stone.json",
            br#"{"type":"my_mod:haunting","ingredients":[{"item":"minecraft:stone"}],"results":[{"id":"my_mod:cursed_stone"}]}"#,
            true,
        ),
        (
            "data/my_mod/recipe/deploying/assemble_gear.json",
            br#"{"type":"my_mod:deploying","ingredients":[{"item":"my_mod:plate"},{"tag":"my_mod:bolts"}],"results":[{"id":"my_mod:gear"}],"keep_held_item":false}"#,
            true,
        ),
        (
            "data/my_mod/recipe/emptying/drain_flask.json",
            br#"{"type":"my_mod:emptying","ingredients":[{"item":"my_mod:full_flask"}],"results":[{"id":"my_mod:empty_flask"},{"amount":250,"id":"my_mod:essence"}]}"#,
            true,
        ),
        (
            "data/my_mod/recipe/item_application/coated_block.json",
            br#"{"type":"my_mod:item_application","ingredients":[{"tag":"my_mod:base_blocks"},{"item":"my_mod:coating"}],"results":[{"id":"my_mod:coated_block"}]}"#,
            true,
        ),
        (
            "data/my_mod/recipe/sandpaper_polishing/glossy_gem.json",
            br#"{"type":"my_mod:sandpaper_polishing","ingredients":[{"item":"my_mod:raw_gem"}],"results":[{"id":"my_mod:glossy_gem"}]}"#,
            true,
        ),
        (
            "data/my_mod/recipe/mechanical_crafting/press_head.json",
            br#"{"type":"my_mod:mechanical_crafting","accept_mirrored":false,"key":{"A":{"item":"my_mod:alloy"},"P":{"tag":"my_mod:plates"}},"pattern":[" A ","APA"," A "],"result":{"count":1,"id":"my_mod:press_head"}}"#,
            true,
        ),
        (
            "data/my_mod/recipe/mixing/fused_pane.json",
            MY_MOD_MIXING_JSON,
            true,
        ),
        (
            "data/my_mod/recipe/pressing/flattened.json",
            br#"{"type":"my_mod:pressing","ingredients":[{"item":"my_mod:blank"}],"results":[{"id":"my_mod:flat","count":3}]}"#,
            true,
        ),
        (
            "data/my_mod/recipe/filling/soaked.json",
            br#"{"type":"my_mod:filling","ingredients":[{"item":"my_mod:sponge"},{"type":"fluid_stack","fluid":"minecraft:water","amount":500}],"results":[{"id":"my_mod:wet_sponge"}]}"#,
            true,
        ),
        (
            "data/my_mod/recipe/pulverizing/dust.json",
            br#"{"type":"my_mod:pulverizing","ingredients":[{"item":"my_mod:ore_chunk","count":1}],"results":[{"id":"my_mod:fine_dust","count":2},{"id":"my_mod:bonus_dust","chance":0.15}]}"#,
            true,
        ),
        (
            "data/my_mod/recipe/sequenced_assembly/widget.json",
            br#"{"type":"my_mod:sequenced_assembly","ingredient":{"tag":"my_mod:blanks"},"transitionalItem":{"id":"my_mod:partial_widget"},"loops":1,"sequence":[{"type":"my_mod:deploying","ingredients":[{"item":"my_mod:partial_widget"},{"item":"my_mod:cap"}]}],"results":[{"id":"my_mod:widget"}]}"#,
            true,
        ),
        // Decoys: vanilla serializers under the addon's own data dir plus a
        // non-machine mod serializer — all skipped gracefully.
        (
            "data/my_mod/recipe/crafting_shaped_decoy.json",
            br##"{"type":"my_mod:crafting_shaped","pattern":["#"],"key":{"#":{"item":"my_mod:stick"}},"result":{"id":"my_mod:torch"}}"##,
            true,
        ),
        (
            "data/my_mod/recipe/smelting_decoy.json",
            br#"{"type":"minecraft:smelting","ingredient":{"item":"my_mod:ore"},"result":{"id":"my_mod:ingot"}}"#,
            true,
        ),
        (
            "data/my_mod/recipe/special_serializer.json",
            br#"{"type":"my_mod:item_copying","category":"misc"}"#,
            true,
        ),
    ])
}

#[test]
fn all_deflate_jar_yields_every_machine_family_and_generic_fallback() {
    let jar = my_mod_deflated_jar();
    let discovered = discover_addon_from_jar(&jar, "my_mod").unwrap();
    assert_eq!(discovered.skipped_compressed_entries, 0);
    let recipes = &discovered.profile.processing_recipes;

    // Every Create 1.21.1 family plus the generic machine landed; the three
    // decoys (shaped crafting, vanilla smelting, non-machine serializer)
    // were skipped without aborting the scan.
    assert_eq!(recipes.len(), 13, "{recipes:?}");
    for (variant, family) in [
        ("compacting", "my_mod:compacting"),
        ("splashing", "my_mod:splashing"),
        ("haunting", "my_mod:haunting"),
        ("deploying", "my_mod:deploying"),
        ("emptying", "my_mod:emptying"),
        ("item_application", "my_mod:item_application"),
        ("sandpaper_polishing", "my_mod:sandpaper_polishing"),
        ("mechanical_crafting", "my_mod:mechanical_crafting"),
        ("mixing", "my_mod:mixing"),
        ("pressing", "my_mod:pressing"),
        ("filling", "my_mod:filling"),
        ("pulverizing", "my_mod:pulverizing"),
        ("sequenced_assembly", "my_mod:sequenced_assembly"),
    ] {
        let found = recipes.iter().any(|r| match r {
            AddonProcessingRecipe::SequencedAssembly(a) => {
                variant == "sequenced_assembly" && a.id.contains(family.split(':').nth(1).unwrap())
            }
            AddonProcessingRecipe::Pressing { id, .. } => {
                variant == "pressing" && id.contains("pressing")
            }
            AddonProcessingRecipe::Mixing { id, .. } => variant == "mixing" && id.contains("mixing"),
            AddonProcessingRecipe::Filling { id, .. } => {
                variant == "filling" && id.contains("filling")
            }
            AddonProcessingRecipe::Compacting(m) => m.recipe_type == family,
            AddonProcessingRecipe::Splashing(m) => m.recipe_type == family,
            AddonProcessingRecipe::Haunting(m) => m.recipe_type == family,
            AddonProcessingRecipe::Deploying(m) => m.recipe_type == family,
            AddonProcessingRecipe::Emptying(m) => m.recipe_type == family,
            AddonProcessingRecipe::ItemApplication(m) => m.recipe_type == family,
            AddonProcessingRecipe::SandpaperPolishing(m) => m.recipe_type == family,
            AddonProcessingRecipe::MechanicalCrafting(m) => m.recipe_type == family,
            AddonProcessingRecipe::GenericMachine(m) => m.recipe_type == family,
            _ => false,
        });
        assert!(found, "missing discovered family {family} ({variant})");
    }

    // Tag ingredients resolve to `#tag` resource keys; fluid inputs/outputs
    // survive with their amounts.
    let compacting = recipes
        .iter()
        .find_map(|r| match r {
            AddonProcessingRecipe::Compacting(m) => Some(m),
            _ => None,
        })
        .unwrap();
    assert_eq!(compacting.ingredients.len(), 2);
    assert_eq!(compacting.ingredients[0].id, "my_mod:shard");
    assert_eq!(compacting.fluid_inputs, vec![("minecraft:lava".into(), 100)]);

    let splashing = recipes
        .iter()
        .find_map(|r| match r {
            AddonProcessingRecipe::Splashing(m) => Some(m),
            _ => None,
        })
        .unwrap();
    assert!(splashing.ingredients[0].is_tag);
    assert_eq!(splashing.ingredients[0].resource_key(), "#my_mod:dirty_ores");
    assert_eq!(
        splashing.results,
        vec![
            ("my_mod:clean_ore".into(), 2, 0.75),
            ("my_mod:grit".into(), 1, 0.25),
        ]
    );

    let emptying = recipes
        .iter()
        .find_map(|r| match r {
            AddonProcessingRecipe::Emptying(m) => Some(m),
            _ => None,
        })
        .unwrap();
    assert_eq!(emptying.results.len(), 1);
    assert_eq!(emptying.fluid_results, vec![("my_mod:essence".into(), 250)]);

    let mixing = recipes
        .iter()
        .find(|r| matches!(r, AddonProcessingRecipe::Mixing { .. }))
        .unwrap();
    match mixing {
        AddonProcessingRecipe::Mixing {
            ingredient_item,
            fluid_inputs,
            results,
            fluid_results,
            ..
        } => {
            assert_eq!(ingredient_item, "#my_mod:dusts/zinc");
            assert_eq!(
                fluid_inputs,
                &vec![("my_mod:molten_glass".to_string(), 150)]
            );
            assert_eq!(results.len(), 1);
            assert_eq!(fluid_results, &vec![("my_mod:slag".to_string(), 50)]);
        }
        _ => unreachable!(),
    }

    // Mechanical crafting folds `key`/`pattern` usage into ingredient
    // counts: 'A' appears 4 times, 'P' once.
    let crafting = recipes
        .iter()
        .find_map(|r| match r {
            AddonProcessingRecipe::MechanicalCrafting(m) => Some(m),
            _ => None,
        })
        .unwrap();
    assert_eq!(
        crafting
            .ingredients
            .iter()
            .map(|i| (i.resource_key(), i.count))
            .collect::<Vec<_>>(),
        vec![("my_mod:alloy".to_string(), 4), ("#my_mod:plates".to_string(), 1)]
    );
    assert_eq!(crafting.results, vec![("my_mod:press_head".into(), 1, 1.0)]);

    // Generic fallback preserved the unknown recipe type.
    let pulverizer = recipes
        .iter()
        .find_map(|r| match r {
            AddonProcessingRecipe::GenericMachine(m) => Some(m),
            _ => None,
        })
        .expect("generic machine discovered");
    assert_eq!(pulverizer.recipe_type, "my_mod:pulverizing");
    assert_eq!(pulverizer.results.len(), 2);

    // Sequenced assembly accepted the tag start ingredient.
    let assembly = recipes
        .iter()
        .find_map(|r| match r {
            AddonProcessingRecipe::SequencedAssembly(a) => Some(a),
            _ => None,
        })
        .unwrap();
    assert_eq!(assembly.ingredient_item, "#my_mod:blanks");
    assert_eq!(
        assembly.sequence,
        vec![AssemblyStepKind::Deploying {
            held_item: "my_mod:cap".into()
        }]
    );
}

#[test]
fn every_discovered_recipe_converts_to_a_processor_recipe() {
    let jar = my_mod_deflated_jar();
    let discovered = discover_addon_from_jar(&jar, "my_mod").unwrap();
    let mut ids = ItemFluidIdTable::new();
    for recipe in &discovered.profile.processing_recipes {
        match recipe {
            AddonProcessingRecipe::SequencedAssembly(_) => {
                assert!(recipe.to_processor_recipe(&mut ids, 1, 40).is_none());
            }
            _ => {
                let flat = recipe
                    .to_processor_recipe(&mut ids, 1, 40)
                    .unwrap_or_else(|| panic!("{recipe:?} did not convert"));
                assert!(!flat.inputs.is_empty() && !flat.outputs.is_empty());
            }
        }
    }
    // The generic machine carried both of its chance-weighted results; the
    // dominant `fine_dust` won the deterministic collapse and its fluid-free
    // recipe has item-only stacks.
    let pulverizer = discovered
        .profile
        .processing_recipes
        .iter()
        .find_map(|r| match r {
            AddonProcessingRecipe::GenericMachine(m) => Some(m),
            _ => None,
        })
        .unwrap();
    let flat = pulverizer.to_processor_recipe(&mut ItemFluidIdTable::new(), 7, 80).unwrap();
    assert_eq!(flat.inputs.len(), 1);
    assert_eq!(flat.outputs.len(), 1);
    assert_eq!(flat.outputs[0].resource.kind, ResourceKind::Item);

    // The emptying recipe emits both the returned container and the
    // drained fluid output.
    let emptying = discovered
        .profile
        .processing_recipes
        .iter()
        .find_map(|r| match r {
            AddonProcessingRecipe::Emptying(m) => Some(m),
            _ => None,
        })
        .unwrap();
    let flat = emptying.to_processor_recipe(&mut ItemFluidIdTable::new(), 8, 80).unwrap();
    assert!(flat
        .outputs
        .iter()
        .any(|s| s.resource.kind == ResourceKind::Fluid && s.amount == 250));
}

#[test]
fn create_loader_reads_deflate_jar_with_all_1_21_1_kinds() {
    let jar = my_mod_deflated_jar();
    let mut ids = ItemFluidIdTable::new();
    let loaded = load_from_stored_zip(&jar, &mut ids).unwrap();
    assert_eq!(loaded.skipped_compressed_zip_entries, 0);
    // `create:*`-typed kinds only — the my_mod JAR has none, so add a second
    // deflate JAR carrying real `create:` recipes.
    let create_jar = build_mixed_zip(&[
        (
            "data/create/recipe/compacting/diorite.json",
            br#"{"type":"create:compacting","ingredients":[{"item":"minecraft:flint"},{"item":"minecraft:calcite"},{"type":"neoforge:single","amount":100,"fluid":"minecraft:lava"}],"results":[{"id":"minecraft:diorite"}]}"#,
            true,
        ),
        (
            "data/create/recipe/deploying/cogwheel.json",
            br#"{"type":"create:deploying","ingredients":[{"item":"create:shaft"},{"tag":"minecraft:planks"}],"results":[{"id":"create:cogwheel"}]}"#,
            true,
        ),
        (
            "data/create/recipe/emptying/honey_bottle.json",
            br#"{"type":"create:emptying","ingredients":[{"item":"minecraft:honey_bottle"}],"results":[{"id":"minecraft:glass_bottle"},{"amount":250,"id":"create:honey"}]}"#,
            true,
        ),
        (
            "data/create/recipe/item_application/brass_casing.json",
            br#"{"type":"create:item_application","ingredients":[{"tag":"c:stripped_logs"},{"tag":"c:ingots/brass"}],"results":[{"id":"create:brass_casing"}]}"#,
            true,
        ),
        (
            "data/create/recipe/sandpaper_polishing/rose_quartz.json",
            br#"{"type":"create:sandpaper_polishing","ingredients":[{"item":"create:rose_quartz"}],"results":[{"id":"create:polished_rose_quartz"}]}"#,
            true,
        ),
        (
            "data/create/recipe/mechanical_crafting/wheel.json",
            br#"{"type":"create:mechanical_crafting","key":{"A":{"item":"create:andesite_alloy"}},"pattern":["A"],"result":{"count":2,"id":"create:crushing_wheel"}}"#,
            true,
        ),
        (
            "data/create/recipe/sequenced_assembly/mechanism.json",
            br#"{"type":"create:sequenced_assembly","ingredient":{"tag":"c:plates/gold"},"transitionalItem":{"id":"create:incomplete_precision_mechanism"},"loops":5,"sequence":[{"type":"create:pressing"}],"results":[{"id":"create:precision_mechanism"}]}"#,
            true,
        ),
    ]);
    let mut ids = ItemFluidIdTable::new();
    let loaded = load_from_stored_zip(&create_jar, &mut ids).unwrap();
    assert_eq!(loaded.skipped_compressed_zip_entries, 0);
    let kinds: Vec<CreateRecipeKind> = loaded.recipes.iter().map(|r| r.kind).collect();
    for expected in [
        CreateRecipeKind::Compacting,
        CreateRecipeKind::Deploying,
        CreateRecipeKind::Emptying,
        CreateRecipeKind::ItemApplication,
        CreateRecipeKind::SandpaperPolishing,
        CreateRecipeKind::MechanicalCrafting,
        CreateRecipeKind::SequencedAssembly,
    ] {
        assert!(kinds.contains(&expected), "missing kind {expected:?}");
    }
    assert_eq!(loaded.recipes.len(), 7);
    // The generic (non-Create-typed) view also produced a usable
    // `LoadedCreateData` (blockstates + tags flow through identically).
    assert_eq!(loaded.blockstates.len(), 0);
    let loaded = load_from_stored_zip(&jar, &mut ids).unwrap();
    assert!(loaded.blockstates.contains_key("my_mod:alloy_mixer"));
    assert!(loaded.tags.contains_key("my_mod:block/mixers"));
}

#[test]
fn zlib_huffman_deflate_blob_round_trips_through_discovery() {
    let jar = build_zip_raw(&[
        (
            "data/my_mod/recipe/mixing/fused_pane.json",
            8,
            ZLIB_DEFLATE_MIXING,
            MY_MOD_MIXING_JSON.len() as u32,
        ),
        (
            "data/my_mod/recipe/pulverizing/dust.json",
            8,
            &deflate_stored_blocks(
                br#"{"type":"my_mod:pulverizing","ingredients":[{"item":"my_mod:ore_chunk"}],"results":[{"id":"my_mod:fine_dust","count":2}]}"#,
            ),
            br#"{"type":"my_mod:pulverizing","ingredients":[{"item":"my_mod:ore_chunk"}],"results":[{"id":"my_mod:fine_dust","count":2}]}"#.len() as u32,
        ),
    ]);
    let discovered = discover_addon_from_jar(&jar, "my_mod").unwrap();
    assert_eq!(discovered.skipped_compressed_entries, 0);
    assert_eq!(discovered.profile.processing_recipes.len(), 2);
    let mixing = discovered
        .profile
        .processing_recipes
        .iter()
        .find_map(|r| match r {
            AddonProcessingRecipe::Mixing {
                ingredient_item,
                fluid_inputs,
                fluid_results,
                ..
            } => Some((ingredient_item.clone(), fluid_inputs.clone(), fluid_results.clone())),
            _ => None,
        })
        .expect("huffman-decoded mixing recipe");
    assert_eq!(mixing.0, "#my_mod:dusts/zinc");
    assert_eq!(mixing.1, vec![("my_mod:molten_glass".to_string(), 150)]);
    assert_eq!(mixing.2, vec![("my_mod:slag".to_string(), 50)]);
}

#[test]
fn mixed_stored_and_deflate_members_both_decode() {
    let jar = build_mixed_zip(&[
        (
            "data/my_mod/recipe/pressing/flat.json",
            br#"{"type":"my_mod:pressing","ingredients":[{"item":"my_mod:blank"}],"results":[{"id":"my_mod:flat"}]}"#,
            false,
        ),
        (
            "data/my_mod/recipe/milling/dust.json",
            br#"{"type":"my_mod:milling","ingredients":[{"item":"my_mod:chunk"}],"results":[{"id":"my_mod:dust"}]}"#,
            true,
        ),
    ]);
    let discovered = discover_addon_from_jar(&jar, "my_mod").unwrap();
    assert_eq!(discovered.skipped_compressed_entries, 0);
    assert_eq!(discovered.profile.processing_recipes.len(), 2);
}

#[test]
fn deflate_scan_fails_closed_on_corruption_and_traversal() {
    // Truncated deflate payload (stream does not terminate).
    let good = deflate_stored_blocks(b"{\"type\":\"my_mod:milling\",\"ingredients\":[{\"item\":\"my_mod:c\"}],\"results\":[{\"id\":\"my_mod:d\"}]}");
    let truncated = &good[..good.len() - 2];
    let jar = build_zip_raw(&[(
        "data/my_mod/recipe/milling/dust.json",
        8,
        truncated,
        4096,
    )]);
    assert!(discover_addon_from_jar(&jar, "my_mod").is_err());

    // Declared uncompressed size beyond the per-entry cap fails before
    // amplification.
    let jar = build_zip_raw(&[(
        "data/my_mod/recipe/milling/bomb.json",
        8,
        &good,
        (8 * 1024 * 1024) + 1,
    )]);
    assert!(discover_addon_from_jar(&jar, "my_mod").is_err());

    // Path traversal in a wanted entry name fails closed.
    let jar = build_mixed_zip(&[(
        "data/../escape.json",
        br#"{"type":"my_mod:milling","ingredients":[{"item":"my_mod:c"}],"results":[{"id":"my_mod:d"}]}"#,
        true,
    )]);
    assert!(discover_addon_from_jar(&jar, "my_mod").is_err());

    // A recognized-but-malformed deflate recipe aborts the scan, same as
    // the stored path.
    let jar = build_mixed_zip(&[(
        "data/my_mod/recipe/milling/broken.json",
        b"{truncated",
        true,
    )]);
    assert!(discover_addon_from_jar(&jar, "my_mod").is_err());
}
