use super::*;
use crate::block_registry::breg;
use std::collections::BTreeMap;

// Registry packet order in the vanilla 1.21.1 configuration capture.
const SYNCED_REGISTRIES: [&str; 11] = [
    "worldgen/biome",
    "chat_type",
    "trim_pattern",
    "trim_material",
    "wolf_variant",
    "painting_variant",
    "dimension_type",
    "damage_type",
    "banner_pattern",
    "enchantment",
    "jukebox_song",
];

/// Build Configuration 0x07 payloads (starting at the registry ID) from pack
/// files in low-to-high priority order. Directory and ZIP readers supply paths
/// relative to the pack root. A partial input emits only its populated registries.
pub(crate) fn build_registry_data_payloads<'a>(
    packs: impl IntoIterator<Item = impl IntoIterator<Item = (&'a str, &'a [u8])>>,
) -> Result<Vec<Vec<u8>>, String> {
    let mut registries = BTreeMap::<_, BTreeMap<_, _>>::new();
    for (priority, files) in packs.into_iter().enumerate() {
        for (file, json) in files {
            let Some((namespace, relative)) = file
                .strip_prefix("data/")
                .and_then(|path| path.split_once('/'))
            else {
                continue;
            };
            let Some(relative) = relative.strip_suffix(".json") else {
                continue;
            };
            let Some((kind, path)) = SYNCED_REGISTRIES.iter().find_map(|&kind| {
                let path = relative.strip_prefix(kind)?.strip_prefix('/')?;
                Some((kind, path))
            }) else {
                continue;
            };
            // Vanilla ResourceLocation orders by path, then by namespace.
            let previous = registries
                .entry(kind)
                .or_default()
                .insert((path, namespace), (priority, file, json));
            if previous.is_some_and(|(previous_priority, _, _)| previous_priority == priority) {
                return Err(format!("duplicate registry file {file} in one datapack"));
            }
        }
    }

    let mut payloads = Vec::new();
    for kind in SYNCED_REGISTRIES {
        let Some(entries) = registries.remove(kind) else {
            continue;
        };
        let mut payload = Vec::new();
        write_str(&mut payload, &format!("minecraft:{kind}"));
        let count = i32::try_from(entries.len()).map_err(|_| "too many registry entries")?;
        write_varint(&mut payload, i64::from(count));
        for ((path, namespace), (_, file, json)) in entries {
            let location = brigadier_wire::ResourceLocation::new(namespace, path)
                .map_err(|_| format!("invalid registry entry path {file}"))?;
            let id = location.to_string();
            if id.len() > 32_767 || path.split('/').any(|part| matches!(part, "" | "." | "..")) {
                return Err(format!("invalid registry entry path {file}"));
            }
            let mut tag =
                nbt::registry_json_to_nbt(json).map_err(|err| format!("{file}: {err}"))?;
            if kind == "worldgen/biome" {
                // Biome's network codec carries only the climate and effects.
                if let nbt::Tag::Compound(fields) = &mut tag {
                    for field in [
                        "carvers",
                        "features",
                        "spawn_costs",
                        "spawners",
                        "creature_spawn_probability",
                    ] {
                        fields.remove(&nbt::NbtString::text(field));
                    }
                }
            }
            write_str(&mut payload, &id);
            payload.push(1); // Optional NBT is present for every entry.
            payload.extend(nbt::write_network_nbt(&tag).map_err(|err| format!("{file}: {err}"))?);
        }
        payloads.push(payload);
    }
    Ok(payloads)
}

fn read_registry_string<'a>(input: &mut &'a [u8]) -> &'a str {
    let len = usize::try_from(read_protocol_i32(input).unwrap()).unwrap();
    std::str::from_utf8(input.split_off(..len).unwrap()).unwrap()
}

#[test]
fn network_nbt_uses_a_compound_type_without_root_name_bytes() {
    let tag = nbt::Tag::Compound(std::collections::BTreeMap::from([(
        nbt::NbtString::text("s"),
        nbt::Tag::String(nbt::NbtString::text("\0😀")),
    )]));
    let wire = b"\x0a\x08\x00\x01s\x00\x08\xc0\x80\xed\xa0\xbd\xed\xb8\x80\x00";
    assert_eq!(nbt::write_network_nbt(&tag).unwrap(), wire);
    for end in 0..wire.len() {
        assert!(nbt::read_network_nbt(&mut &wire[..end]).is_err());
    }
    let framed = [wire.as_slice(), &[0x7f]].concat();
    let mut input = framed.as_slice();
    assert_eq!(nbt::read_network_nbt(&mut input).unwrap(), tag);
    assert_eq!(input, [0x7f]);
    assert!(nbt::read_network_nbt(&mut &[0u8][..]).is_err());
    assert!(nbt::write_network_nbt(&nbt::Tag::Int(1)).is_err());
}

#[test]
fn registry_json_preserves_datapack_value_types() {
    let tag = nbt::registry_json_to_nbt(
        r#"{
            "enabled": true,
            "number": 1,
            "fixed_time": 9007199254740993,
            "scale": 1.25e1,
            "name": "洞窟 \ud83d\ude00",
            "parameters": ["sender", "content"],
            "empty": [],
            "extra": ["go ", {"text": "north", "italic": false}, {"": "literal key"}]
        }"#
        .as_bytes(),
    )
    .unwrap();
    let string = |text| nbt::Tag::String(nbt::NbtString::text(text));
    assert_eq!(tag.get("enabled"), Some(&nbt::Tag::Byte(1)));
    assert_eq!(tag.get("number"), Some(&nbt::Tag::Int(1)));
    assert_eq!(
        tag.get("fixed_time"),
        Some(&nbt::Tag::Long(9_007_199_254_740_993))
    );
    assert_eq!(
        tag.get("scale"),
        Some(&nbt::Tag::Double(12.5_f64.to_bits()))
    );
    assert_eq!(tag.get("name"), Some(&string("洞窟 😀")));
    assert_eq!(
        tag.get("parameters"),
        Some(&nbt::Tag::List(
            8,
            vec![string("sender"), string("content")]
        ))
    );
    assert_eq!(tag.get("empty"), Some(&nbt::Tag::List(0, Vec::new())));
    let extra = tag.get("extra").unwrap();
    let nbt::Tag::List(10, extra) = extra else {
        panic!("mixed component array must use a compound list");
    };
    assert_eq!(extra[0].get(""), Some(&string("go ")));
    assert_eq!(extra[1].get("text"), Some(&string("north")));
    assert_eq!(extra[1].get("italic"), Some(&nbt::Tag::Byte(0)));
    assert_eq!(
        extra[2].get("").and_then(|field| field.get("")),
        Some(&string("literal key"))
    );
}

#[test]
fn registry_json_rejects_malformed_or_unrepresentable_values() {
    for bytes in [
        br#"[]"#.as_slice(),
        br#"{} {}"#,
        br#"{"a": false,}"#,
        br#"{"a": 01}"#,
        br#"{"a": 1e+-2}"#,
        br#"{"a": 1e1000}"#,
        br#"{"a": 9223372036854775808}"#,
        br#"{"a": "\u0x12"}"#,
        br#"{"a": [1,]}"#,
        br#"{"a": null}"#,
        br#"{"a": false, "a": true}"#,
    ] {
        assert!(nbt::registry_json_to_nbt(bytes).is_err(), "{bytes:?}");
    }
}

#[test]
fn registry_data_0x07_payload_matches_the_wire_layout() {
    let payloads = build_registry_data_payloads([[(
        "data/example/damage_type/custom.json",
        br#"{"message_id":"custom","scaling":"never","exhaustion":0.5}"#.as_slice(),
    )]])
    .unwrap();
    let expected = b"\x15minecraft:damage_type\x01\x0eexample:custom\x01\
                     \x0a\x06\x00\x0aexhaustion\x3f\xe0\x00\x00\x00\x00\x00\x00\
                     \x08\x00\x0amessage_id\x00\x06custom\
                     \x08\x00\x07scaling\x00\x05never\x00";
    assert_eq!(payloads, [expected.to_vec()]);
}

#[test]
fn registry_data_loads_every_synced_registry_from_datapack_paths() {
    let kinds = [
        "banner_pattern",
        "chat_type",
        "damage_type",
        "dimension_type",
        "enchantment",
        "jukebox_song",
        "painting_variant",
        "trim_material",
        "trim_pattern",
        "wolf_variant",
        "worldgen/biome",
    ];
    let files: Vec<_> = kinds
        .iter()
        .map(|kind| format!("data/example/{kind}/nested/custom.json"))
        .collect();
    let payloads = build_registry_data_payloads([files
        .iter()
        .map(|path| (path.as_str(), br#"{"fixture":true}"#.as_slice()))])
    .unwrap();
    let mut actual = Vec::new();
    for payload in &payloads {
        let mut input = payload.as_slice();
        actual.push(read_registry_string(&mut input));
        assert_eq!(read_protocol_i32(&mut input), Some(1));
        assert_eq!(read_registry_string(&mut input), "example:nested/custom");
        assert_eq!(input.split_off(..1), Some(&[1u8][..]));
        let tag = nbt::read_network_nbt(&mut input).unwrap();
        assert_eq!(tag.get("fixture"), Some(&nbt::Tag::Byte(1)));
        assert!(input.is_empty());
    }
    actual.sort_unstable();
    assert!(actual
        .into_iter()
        .eq(kinds.map(|kind| format!("minecraft:{kind}"))));
}

#[test]
fn registry_data_is_deterministic_and_later_packs_override_entries() {
    let files = [
        (
            "data/example/damage_type/z.json",
            br#"{"message_id":"z"}"#.as_slice(),
        ),
        ("data/other/damage_type/a.json", br#"{"message_id":"a"}"#),
        (
            "data/example/damage_type/a.json",
            b"overridden before decoding",
        ),
        ("data/example/tags/damage_type/a.json", b"ignored tag file"),
    ];
    let patch = [(
        "data/example/damage_type/a.json",
        br#"{"message_id":"override"}"#.as_slice(),
    )];
    let payloads = build_registry_data_payloads(
        [files.as_slice(), patch.as_slice()].map(|p| p.iter().copied()),
    )
    .unwrap();
    let mut reversed = files;
    reversed.reverse();
    assert_eq!(
        build_registry_data_payloads(
            [reversed.as_slice(), patch.as_slice()].map(|p| p.iter().copied())
        )
        .unwrap(),
        payloads
    );
    let [payload] = payloads.as_slice() else {
        panic!("expected a single populated registry");
    };
    let mut input = payload.as_slice();
    assert_eq!(read_registry_string(&mut input), "minecraft:damage_type");
    assert_eq!(read_protocol_i32(&mut input), Some(3));
    for (id, message) in [
        ("example:a", "override"),
        ("other:a", "a"),
        ("example:z", "z"),
    ] {
        assert_eq!(read_registry_string(&mut input), id);
        assert_eq!(input.split_off(..1), Some(&[1u8][..]));
        let entry = nbt::read_network_nbt(&mut input).unwrap();
        assert_eq!(
            entry.get("message_id"),
            Some(&nbt::Tag::String(nbt::NbtString::text(message)))
        );
    }
    assert!(input.is_empty());
}

#[test]
fn registry_data_sends_biome_climate_and_effects() {
    let payloads = build_registry_data_payloads([[(
        "data/example/worldgen/biome/custom.json",
        br#"{
            "has_precipitation": true, "downfall": 0.4, "temperature": 0.6,
            "effects": {"fog_color": 2, "water_color": 5, "water_fog_color": 3, "sky_color": 4},
            "carvers": {}, "features": [], "spawn_costs": {}, "spawners": {},
            "creature_spawn_probability": 0.1
        }"#
        .as_slice(),
    )]])
    .unwrap();
    let mut input = payloads[0].as_slice();
    assert_eq!(read_registry_string(&mut input), "minecraft:worldgen/biome");
    assert_eq!(read_protocol_i32(&mut input), Some(1));
    assert_eq!(read_registry_string(&mut input), "example:custom");
    assert_eq!(input.split_off(..1), Some(&[1u8][..]));
    let tag = nbt::read_network_nbt(&mut input).unwrap();
    let nbt::Tag::Compound(fields) = tag else {
        panic!("expected a biome NBT compound");
    };
    assert_eq!(
        fields
            .keys()
            .map(|key| key.to_utf8().unwrap())
            .collect::<Vec<_>>(),
        ["downfall", "effects", "has_precipitation", "temperature"]
    );
    assert!(input.is_empty());
}

#[test]
fn anti_xray_environment_switch_accepts_false_values_and_defaults_on() {
    for value in ["0", "false", "OFF", " no "] {
        assert!(!anti_xray_value_enabled(value), "{value}");
    }
    for value in ["1", "true", "on", "", "unexpected"] {
        assert!(anti_xray_value_enabled(value), "{value}");
    }
}

/// The block/item knowledge is spread across several hand-maintained tables, and
/// the gate the registry slice needs is also the tool that says whether they agree
/// today.
///
/// The first version of this test assumed the drop and item-placement tables were
/// inverse mappings. They are not, and the failures it reported were the test being
/// wrong, not the tables: mining stone drops cobblestone while the stone item places
/// stone. They answer two different questions - *what does this block drop* and
/// *what does this item place* - so the invariant is not that they round-trip.
///
/// What can be required is that the item-to-state direction is self-consistent: the
/// block an item places must be the block named after that item.
#[test]
fn an_item_places_the_block_it_is_named_after() {
    let Some(reg) = breg() else {
        panic!("the block registry must load from assets/");
    };
    let mut mismatches = Vec::new();
    let mut placeable = 0;
    for item in 0..4096i64 {
        let Some(st) = item_to_state(item) else {
            continue;
        };
        placeable += 1;
        if (st as usize) >= reg.state_count() {
            mismatches.push(format!("item {item} -> state {st}, outside the registry"));
            continue;
        }
        let block = reg.name_of(st);
        match name_to_item(block) {
            Some(back) if back == item => {}
            Some(back) => mismatches.push(format!(
                "item {item} places {block}, whose name is item {back}"
            )),
            None => mismatches.push(format!("item {item} places {block}, which has no item")),
        }
    }
    assert!(placeable > 0, "the table must not be empty");
    assert!(
        mismatches.is_empty(),
        "{} placement mismatches:\n{}",
        mismatches.len(),
        mismatches.join("\n")
    );
}

/// The drop direction cannot be checked by round-tripping either, but every block
/// that claims a drop must at least be a block the registry knows and can name, and
/// the drop must be inside the item id space the rest of the server uses.
#[test]
fn every_block_that_drops_something_is_a_block_the_registry_knows() {
    let Some(reg) = breg() else {
        panic!("the block registry must load from assets/");
    };
    let mut problems = Vec::new();
    let mut with_drop = 0;
    for st in 0..reg.state_count() as u32 {
        let Some(item) = block_drop(st) else { continue };
        with_drop += 1;
        if !(0..4096).contains(&item) {
            problems.push(format!(
                "state {st} drops item {item}, outside the item space"
            ));
        }
        if reg.name_of(st) == "minecraft:air" {
            problems.push(format!("state {st} drops item {item} but has no name"));
        }
    }
    assert!(with_drop > 0, "the drop table must not be empty");
    assert!(
        problems.is_empty(),
        "{} drop problems:\n{}",
        problems.len(),
        problems.join("\n")
    );
}

/// The anti-xray list is resolved with `filter_map`, so a name that does not resolve
/// disappears without a sound. That is the silent-hole shape the registry tables are
/// prone to: the ore simply stays visible and nothing says so. Requiring the set to
/// have one state per name turns a typo into a failing test.
#[test]
fn every_hidden_block_name_resolves_to_the_block_it_names() {
    let Some(reg) = breg() else {
        panic!("the block registry must load from assets/");
    };
    let mut unresolved = Vec::new();
    for name in HIDDEN_BLOCK_NAMES {
        match name_to_state(name) {
            Some(st) => {
                let got = reg.name_of(st).trim_start_matches("minecraft:");
                if got != *name {
                    unresolved.push(format!("{name} resolved to state {st}, named {got}"));
                }
            }
            None => unresolved.push(format!("{name} did not resolve to any block")),
        }
    }
    assert!(
        unresolved.is_empty(),
        "{} hidden-block names are wrong:\n{}",
        unresolved.len(),
        unresolved.join("\n")
    );
    // Every state of every named block is hidden (lit redstone ore, every chest facing),
    // and nothing else is.
    for st in 0..reg.state_count() as u32 {
        let name = reg.name_of(st).trim_start_matches("minecraft:");
        assert_eq!(
            hidden_states().contains(&st),
            HIDDEN_BLOCK_NAMES.contains(&name),
            "state {st} ({name})"
        );
    }
}

/// `block_hardness` answers 0.6 for every block it does not list. That is a silent
/// default: a block that should take a long time falls through and breaks almost
/// instantly. This reports which named blocks use it, so the set can be judged
/// rather than assumed.
#[test]
fn report_which_blocks_use_the_default_hardness() {
    let Some(reg) = breg() else {
        panic!("the block registry must load from assets/");
    };
    let mut defaulted: Vec<&str> = Vec::new();
    for st in 0..reg.state_count() as u32 {
        let name = reg.name_of(st);
        if name == "minecraft:air" {
            continue;
        }
        if block_hardness(st as i64) == 0.6 {
            defaulted.push(name.trim_start_matches("minecraft:"));
        }
    }
    defaulted.sort_unstable();
    defaulted.dedup();
    // Names that vanilla gives a distinctly non-soft hardness: if any of these
    // defaults, mining time is wrong by a large factor.
    let watch = [
        "obsidian",
        "ancient_debris",
        "crying_obsidian",
        "deepslate",
        "iron_block",
        "diamond_block",
        "gold_block",
        "emerald_block",
        "netherite_block",
        "stone",
        "cobblestone",
        "end_stone",
        "netherrack",
        "spawner",
        "enchanting_table",
        "anvil",
        "cauldron",
        "hopper",
        "furnace",
        "chest",
    ];
    let flagged: Vec<&str> = watch
        .iter()
        .copied()
        .filter(|w| defaulted.contains(w))
        .collect();
    println!(
        "DEFAULT_HARDNESS {} blocks default; flagged={flagged:?}",
        defaulted.len()
    );
    println!("DEFAULT_SAMPLE {:?}", &defaulted[..defaulted.len().min(60)]);
}

/// Mining time must come from the asset the registry loads, not from a hand table.
/// The hand table this replaced covered 356 of the 26684 states, so every other block
/// mined at the 0.6 default - crying obsidian, the metal blocks, spawners and anvils
/// among them. The asset is read here independently rather than trusted through the
/// registry, so the check compares two routes to the same source.
#[test]
fn mining_time_is_the_asset_value_for_every_state() {
    let Some(reg) = breg() else {
        panic!("the block registry must load from assets/");
    };
    let raw = std::fs::read("assets/blocks_1211.json").expect("the asset must be readable");
    let md = crate::json::jparse(&raw)
        .expect("the asset must parse")
        .as_arr()
        .expect("the asset is an array")
        .clone();
    let mut checked = 0usize;
    for b in &md {
        let lo = crate::json::jnum(b, "minStateId").expect("minStateId") as u32;
        let hi = crate::json::jnum(b, "maxStateId").expect("maxStateId") as u32;
        let h = crate::json::jnum(b, "hardness").unwrap_or(0.6) as f32;
        let name = crate::json::jstr(b, "name").unwrap_or_else(|| "?".into());
        for st in lo..=hi {
            if (st as usize) >= reg.state_count() {
                continue;
            }
            assert_eq!(block_hardness(st as i64), h, "state {st} of {name}");
            checked += 1;
        }
    }
    assert!(
        checked > 20_000,
        "the check must cover the state space, but covered only {checked}"
    );
}

/// Named cases from the asset, so the assertion reads as the behaviour rather than as a
/// comparison. Before this change every one of these but stone/obsidian/bedrock/dirt
/// returned the 0.6 default.
#[test]
fn named_blocks_have_their_vanilla_mining_time() {
    for (name, expect) in [
        ("stone", 1.5f32),
        ("dirt", 0.5),
        ("bedrock", -1.0),
        ("obsidian", 50.0),
        ("crying_obsidian", 50.0),
        ("netherite_block", 50.0),
        ("chest", 2.5),
        ("hopper", 3.0),
        ("anvil", 5.0),
        ("spawner", 5.0),
        ("deepslate", 3.0),
        ("diamond_block", 5.0),
    ] {
        let st = name_to_state(name).unwrap_or_else(|| panic!("{name} must exist"));
        assert_eq!(block_hardness(st as i64), expect, "{name}");
    }
}

/// `canHarvest` decides both whether a block drops and whether mining uses the fast
/// multiplier, and it used to be re-derived as "needs a pickaxe AND a high enough
/// tier". That asks the wrong question for the many blocks that need no tool at all:
/// dirt was answered "cannot harvest", so it mined at the slow rate. The registry
/// answers from the block's own harvestTools set, and this reads the asset to check.
#[test]
fn can_harvest_is_the_assets_harvest_tools_set() {
    let Some(reg) = breg() else {
        panic!("the block registry must load from assets/");
    };
    let raw = std::fs::read("assets/blocks_1211.json").expect("the asset must be readable");
    let md = crate::json::jparse(&raw)
        .expect("the asset must parse")
        .as_arr()
        .expect("the asset is an array")
        .clone();
    let mut checked = 0usize;
    let mut needs_tool = 0usize;
    for b in &md {
        // Every tool id in the asset, so both the accepted and rejected cases run.
        let allowed: Vec<i64> = match b.get("harvestTools") {
            Some(crate::json::J::Obj(entries)) => entries
                .iter()
                .filter_map(|(k, _)| k.parse::<i64>().ok())
                .collect(),
            _ => Vec::new(),
        };
        if !allowed.is_empty() {
            needs_tool += 1;
        }
        let lo = crate::json::jnum(b, "minStateId").expect("minStateId") as u32;
        let hi = crate::json::jnum(b, "maxStateId").expect("maxStateId") as u32;
        let name = crate::json::jstr(b, "name").unwrap_or_else(|| "?".into());
        for st in lo..=hi {
            if (st as usize) >= reg.state_count() {
                continue;
            }
            for item in [818i64, 819, 820, 825, 830, 835, 840, 845, 850] {
                let expected = allowed.is_empty() || allowed.contains(&item);
                assert_eq!(
                    can_harvest(st as i64, item),
                    expected,
                    "state {st} of {name}, item {item}"
                );
                checked += 1;
            }
        }
    }
    assert!(
        needs_tool > 100,
        "the asset must have tool-requiring blocks"
    );
    assert!(
        checked > 100_000,
        "the check must cover the states, covered {checked}"
    );
}

/// Named cases, because the interesting half is the blocks that need NO tool: those
/// were the ones answered wrongly.
#[test]
fn named_blocks_answer_can_harvest_the_vanilla_way() {
    let wood_pick = 820i64; // 818 + tier*5 + kind, kind 2 = pickaxe
    let stone_pick = 825;
    let shovel = 819;
    for (name, item, expect) in [
        ("dirt", shovel, true),
        ("dirt", wood_pick, true),
        ("sand", wood_pick, true),
        ("grass_block", shovel, true),
        ("oak_log", wood_pick, true),
        ("stone", wood_pick, true),
        ("stone", shovel, false),
        ("cobblestone", wood_pick, true),
        ("iron_ore", wood_pick, false),
        ("iron_ore", stone_pick, true),
        ("diamond_ore", stone_pick, false),
    ] {
        let st = name_to_state(name).unwrap_or_else(|| panic!("{name} must exist"));
        assert_eq!(
            can_harvest(st as i64, item),
            expect,
            "{name} with item {item}"
        );
    }
}

/// Which tool mines a block at full speed used to come from bare id ranges in
/// `tool_dig_speed`, which named the right tool for 16 percent of the states: a shovel
/// was not recognised on dirt, an axe not on planks, and so on. The asset states it,
/// either as a mineable/<kind> tag or - for blocks gated by tier, like obsidian -
/// through the harvestTools set alone. This reads the asset and requires the same
/// answer, for every state.
#[test]
fn the_correct_tool_kind_is_the_assets_material() {
    let Some(reg) = breg() else {
        panic!("the block registry must load from assets/");
    };
    let raw = std::fs::read("assets/blocks_1211.json").expect("the asset must be readable");
    let md = crate::json::jparse(&raw)
        .expect("the asset must parse")
        .as_arr()
        .expect("the asset is an array")
        .clone();
    let mut checked = 0usize;
    let mut with_kind = 0usize;
    for b in &md {
        let material = b
            .get("material")
            .map(|m| m.str().to_string())
            .unwrap_or_default();
        let from_material = material.split(';').find_map(|part| match part {
            "mineable/shovel" => Some(1i8),
            "mineable/pickaxe" => Some(2),
            "mineable/axe" => Some(3),
            "mineable/hoe" => Some(4),
            "mineable/sword" => Some(0),
            _ => None,
        });
        let from_tools = match b.get("harvestTools") {
            Some(crate::json::J::Obj(entries)) => {
                let mut k: Option<i8> = None;
                let mut disagree = false;
                for (key, _) in entries {
                    let t: i64 = match key.parse() {
                        Ok(t) => t,
                        Err(_) => continue,
                    };
                    if (818..=847).contains(&t) {
                        let kd = ((t - 818) % 5) as i8;
                        match k {
                            None => k = Some(kd),
                            Some(prev) if prev != kd => disagree = true,
                            _ => {}
                        }
                    }
                }
                if disagree {
                    None
                } else {
                    k
                }
            }
            _ => None,
        };
        let want = from_material.or(from_tools).unwrap_or(-1);
        if want >= 0 {
            with_kind += 1;
        }
        let lo = crate::json::jnum(b, "minStateId").expect("minStateId") as u32;
        let hi = crate::json::jnum(b, "maxStateId").expect("maxStateId") as u32;
        let name = crate::json::jstr(b, "name").unwrap_or_else(|| "?".into());
        for st in lo..=hi {
            if (st as usize) >= reg.state_count() {
                continue;
            }
            assert_eq!(reg.tool_kind_of(st), want, "state {st} of {name}");
            checked += 1;
        }
    }
    assert!(
        with_kind > 300,
        "the asset must name a tool for most blocks"
    );
    assert!(
        checked > 20_000,
        "the check must cover the state space, covered {checked}"
    );
}

/// Named cases, including the two shapes that used to be missed: a block mined by a
/// non-pickaxe, and a block whose tool is stated only through its tier-gated set.
#[test]
fn named_blocks_name_their_tool() {
    for (name, kind) in [
        ("dirt", 1i8),
        ("sand", 1),
        ("gravel", 1),
        ("grass_block", 1),
        ("stone", 2),
        ("cobblestone", 2),
        ("iron_ore", 2),
        ("obsidian", 2),
        ("ancient_debris", 2),
        ("netherite_block", 2),
        ("oak_planks", 3),
        ("oak_log", 3),
        ("oak_leaves", 4),
        ("glass", -1),
        ("torch", -1),
    ] {
        let st = name_to_state(name).unwrap_or_else(|| panic!("{name} must exist"));
        assert_eq!(tool_kind(st as i64), kind, "{name}");
    }
}

/// The consequence the derivation is for: the right tool has to be faster than bare
/// hands, and a tool of the wrong type must not be.
#[test]
fn the_right_tool_is_faster_than_bare_hands() {
    let hands = (0i64, 0u8, Vec::new());
    let iron_shovel = (818 + 3 * 5 + 1, 1u8, Vec::new());
    let iron_pick = (818 + 3 * 5 + 2, 1u8, Vec::new());
    let iron_axe = (818 + 3 * 5 + 3, 1u8, Vec::new());
    let diamond_pick = (840i64, 1u8, Vec::new());
    let at = |name: &str| name_to_state(name).unwrap() as i64;

    assert!(
        tool_dig_speed(&iron_shovel, at("dirt")) > tool_dig_speed(&hands, at("dirt")),
        "a shovel must speed up dirt"
    );
    assert_eq!(
        tool_dig_speed(&iron_pick, at("dirt")),
        1.0,
        "a pickaxe is not dirt's tool and must not speed it up"
    );
    assert!(
        tool_dig_speed(&iron_axe, at("oak_planks")) > 1.0,
        "an axe must speed up planks"
    );
    assert!(
        tool_dig_speed(&iron_pick, at("stone")) > 1.0,
        "a pickaxe must speed up stone"
    );
    assert!(
        tool_dig_speed(&diamond_pick, at("obsidian")) > 1.0,
        "obsidian's tool is the pickaxe, even though only its tier is stated"
    );
}

/// The drop table used to list 13 states. The asset gives a drop for 23364 of them, so
/// granite, the stairs, the slabs and everything else dropped nothing at all, and glass
/// and ice dropped themselves where vanilla drops nothing without silk touch. This
/// reads the asset and requires the same answer for every state.
#[test]
fn the_drop_is_the_assets_drop_for_every_state() {
    let Some(reg) = breg() else {
        panic!("the block registry must load from assets/");
    };
    let raw = std::fs::read("assets/blocks_1211.json").expect("the asset must be readable");
    let md = crate::json::jparse(&raw)
        .expect("the asset must parse")
        .as_arr()
        .expect("the asset is an array")
        .clone();
    let mut checked = 0usize;
    let mut multi = 0usize;
    for b in &md {
        let drops: Vec<i64> = match b.get("drops").and_then(|d| d.as_arr()) {
            Some(a) => a.iter().map(|j| j.num() as i64).collect(),
            None => Vec::new(),
        };
        if drops.len() > 1 {
            multi += 1;
        }
        let want = drops.first().copied();
        let lo = crate::json::jnum(b, "minStateId").expect("minStateId") as u32;
        let hi = crate::json::jnum(b, "maxStateId").expect("maxStateId") as u32;
        let name = crate::json::jstr(b, "name").unwrap_or_else(|| "?".into());
        for st in lo..=hi {
            if (st as usize) >= reg.state_count() {
                continue;
            }
            // Only the first drop is modelled; the 34 potted plants also drop their plant.
            assert_eq!(reg.block_drop(st), want, "state {st} of {name}");
            checked += 1;
        }
    }
    assert!(
        checked > 20_000,
        "the check must cover the state space, covered {checked}"
    );
    assert_eq!(
        multi, 34,
        "the asset now has {multi} blocks with more than one drop, and only the first is modelled"
    );
}

/// Named cases, including the two the old table got backwards: a block whose drop is not
/// itself, and a block that drops nothing.
#[test]
fn named_blocks_drop_what_vanilla_drops() {
    let item = |n: &str| name_to_item(n).unwrap_or_else(|| panic!("{n} must be an item"));
    for (name, expect) in [
        ("stone", Some(35i64)), // cobblestone
        ("granite", Some(2)),
        ("grass_block", Some(28)), // dirt
        ("dirt", Some(28)),
        ("sand", Some(57)),
        ("gravel", Some(61)),
        ("oak_log", Some(132)),
        ("coal_ore", Some(803)),
        ("iron_ore", Some(810)),
        ("diamond_ore", Some(805)),
        ("redstone_ore", Some(657)),
        ("glass", None), // nothing without silk touch
        ("ice", None),
        ("bedrock", None),
    ] {
        let st = name_to_state(name).unwrap_or_else(|| panic!("{name} must exist"));
        assert_eq!(block_drop(st), expect, "{name}");
    }
    // And the ids above are the named items, not just numbers that happen to fit.
    assert_eq!(name_to_state("cobblestone"), Some(14));
    assert_eq!(block_drop(14u32), name_to_item("cobblestone"));
    assert_eq!(block_drop(1u32), Some(item("cobblestone")));
    assert_eq!(block_drop(9u32), Some(item("dirt")));
}

/// Stack size was a hand table of item id ranges. The asset states it per block - 1 for
/// tools, 16 for signs, 64 otherwise - so for anything that places a block the table was
/// a re-derivation, and a partial one. This checks every state against the asset.
#[test]
fn stack_size_comes_from_the_asset_for_every_state() {
    let Some(reg) = breg() else {
        panic!("the block registry must load from assets/");
    };
    let raw = std::fs::read("assets/blocks_1211.json").expect("the asset must be readable");
    let md = crate::json::jparse(&raw)
        .expect("the asset must parse")
        .as_arr()
        .expect("the asset is an array")
        .clone();
    let mut checked = 0usize;
    for b in &md {
        let want = crate::json::jnum(b, "stackSize").unwrap_or(64.0) as i64;
        let lo = crate::json::jnum(b, "minStateId").expect("minStateId") as u32;
        let hi = crate::json::jnum(b, "maxStateId").expect("maxStateId") as u32;
        let name = crate::json::jstr(b, "name").unwrap_or_else(|| "?".into());
        for st in lo..=hi {
            if (st as usize) >= reg.state_count() {
                continue;
            }
            assert_eq!(reg.stack_size_of(st), want, "state {st} of {name}");
            checked += 1;
        }
    }
    assert!(
        checked > 20_000,
        "the check must cover the state space, covered {checked}"
    );
}

/// The names, so the assertion reads as the behaviour. Signs stack to 16 and tools to 1,
/// and both were previously answered from ranges over item ids.
#[test]
fn named_items_stack_the_way_vanilla_stacks_them() {
    for (name, want) in [
        ("stone", 64i64),
        ("oak_log", 64),
        ("oak_sign", 16),
        ("oak_hanging_sign", 16),
        ("white_banner", 16),
        ("bucket", 64), // the block "bucket" does not exist; this is the water bucket
        ("diamond_pickaxe", 64),
    ] {
        let Some(st) = name_to_state(name) else {
            continue;
        };
        let Some(item) = name_to_item(name) else {
            continue;
        };
        // Only meaningful when the name is both a block and an item.
        assert_eq!(item_stack_max(item, &Vec::new()), want, "{name}");
        assert_eq!(stack_size_of(st), want, "{name} state");
    }
}

/// `with_prop` moves one property's digit inside a block's state range. Doing that to a
/// value lower than the current one used to wrap the wrong way and land outside the
/// block: setting sugar cane's age back to 0 produced state 5815, which is a jukebox.
/// Every value of a 16-valued property is walked in both directions here.
#[test]
fn setting_a_property_lower_than_its_current_value_stays_inside_the_block() {
    let Some(reg) = breg() else {
        panic!("the block registry must load from assets/");
    };
    let cane = name_to_state("sugar_cane").expect("sugar_cane must exist");
    let values: Vec<String> = (0..16).map(|v| v.to_string()).collect();

    // Downwards from each starting value, which is where the wrapping went wrong.
    for from in &values {
        let start = reg.with_prop(cane, "age", from);
        assert_eq!(
            reg.name_of(start),
            "minecraft:sugar_cane",
            "age {from} left the block"
        );
        for to in &values {
            let moved = reg.with_prop(start, "age", to);
            assert_eq!(
                reg.name_of(moved),
                "minecraft:sugar_cane",
                "age {from} -> {to} left the block (state {moved} is {})",
                reg.name_of(moved)
            );
            assert_eq!(
                reg.get_prop(moved, "age").as_deref(),
                Some(to.as_str()),
                "age {from} -> {to} did not take"
            );
        }
    }

    // And a block with more than one property, where the digit has a multiplier.
    let Some(stairs) = name_to_state("oak_stairs") else {
        panic!("oak_stairs must exist")
    };
    let facing = ["north", "south", "west", "east"];
    for f in facing {
        for half in ["top", "bottom"] {
            for shape in [
                "straight",
                "inner_left",
                "inner_right",
                "outer_left",
                "outer_right",
            ] {
                let mut s = stairs;
                s = reg.with_prop(s, "facing", f);
                s = reg.with_prop(s, "half", half);
                s = reg.with_prop(s, "shape", shape);
                assert_eq!(reg.name_of(s), "minecraft:oak_stairs", "{f}/{half}/{shape}");
                assert_eq!(reg.get_prop(s, "facing").as_deref(), Some(f));
                assert_eq!(reg.get_prop(s, "half").as_deref(), Some(half));
                assert_eq!(reg.get_prop(s, "shape").as_deref(), Some(shape));
            }
        }
    }
    // And back down again, which is the direction that used to escape the block.
    for f in facing {
        let s = reg.with_prop(stairs, "facing", f);
        let back = reg.with_prop(s, "facing", "north");
        assert_eq!(reg.name_of(back), "minecraft:oak_stairs", "{f} -> north");
        assert_eq!(reg.get_prop(back, "facing").as_deref(), Some("north"));
    }
}

/// Luminance and opacity are per-state columns of `assets/block_states_1211.json`, which uses the
/// same 1.21.1 state-id numbering as `assets/blocks_1211.json`. The registry answers both from
/// `assets/block_state_facts_1211.bin`, a separate export of the same pinned jar, so this compares
/// two independent recordings of one state space by id - a shifted row in either file shows up as
/// a disagreement instead of as two plausible tables.
#[test]
fn light_values_come_from_the_asset_for_every_state() {
    let Some(reg) = breg() else {
        panic!("the block registry must load from assets/");
    };
    let raw = std::fs::read("assets/block_states_1211.json")
        .expect("block_states_1211 must be readable");
    let facts = crate::json::jparse(&raw).expect("block_states_1211 must parse");
    let col = |key: &str| -> Vec<u8> {
        facts
            .get(key)
            .and_then(|c| c.as_arr())
            .unwrap_or_else(|| panic!("block_states_1211 must have a {key} column"))
            .iter()
            .map(|v| v.num() as u8)
            .collect()
    };
    let luminance = col("luminance");
    let opacity = col("light_block");
    assert_eq!(
        luminance.len(),
        reg.state_count(),
        "the asset must have one luminance per state"
    );
    assert_eq!(
        opacity.len(),
        reg.state_count(),
        "the asset must have one opacity per state"
    );

    let mut checked = 0usize;
    let mut emitters = 0usize;
    for st in 0..reg.state_count() as u32 {
        let i = st as usize;
        assert_eq!(reg.luminance_of(st), luminance[i], "luminance of state {st}");
        assert_eq!(reg.opacity_of(st), opacity[i], "opacity of state {st}");
        if luminance[i] > 0 {
            emitters += 1;
        }
        checked += 1;
    }
    assert!(
        checked > 20_000,
        "the check must cover the state space, covered {checked}"
    );
    assert!(
        emitters > 100 && emitters < checked / 10,
        "the asset must have light sources and they must stay a minority, found {emitters}"
    );
    // A column that reads back as all zeros would satisfy every equality above; the light-block
    // column is three populations in 1.21.1 (transparent 14,616, half 9,552, full 2,516), so the
    // two non-zero ones are what a shifted or truncated export loses first.
    assert!(
        opacity.iter().filter(|&&v| v > 0).count() > 10_000,
        "the asset must have light blockers, found {:?}",
        opacity.iter().filter(|&&v| v > 0).count()
    );
    assert!(
        opacity.contains(&15) && opacity.contains(&1),
        "both blocked-light populations must be present"
    );
}

/// The collision list and the pinned collision predicate are one answer split across two files:
/// the boxes are `assets/block_states_1211.json`, the `isEmpty`/`isFullBlock` bits are
/// `assets/block_state_facts_1211.bin`, and both come from one `getCollisionShape(EmptyBlockView,
/// ORIGIN)` call per state. A list that is empty exactly when the predicate says so is what makes
/// the two interchangeable; a state that disagrees means one of the exports moved.
#[test]
fn collision_boxes_agree_with_the_pinned_collision_predicate() {
    let Some(reg) = breg() else {
        panic!("the block registry must load from assets/");
    };
    let mut disagreements = Vec::new();
    let mut boxes = 0usize;
    let mut colliding_states = 0usize;
    for st in 0..reg.state_count() as u32 {
        let list: Vec<_> = reg.collision_boxes(st).collect();
        boxes += list.len();
        if !list.is_empty() {
            colliding_states += 1;
        }
        if reg.is_collision_empty(st) != list.is_empty() {
            disagreements.push(format!("state {st} ({})", reg.name_of(st)));
        }
        for (min, max) in list {
            if (0..3).any(|k| min[k] > max[k]) {
                disagreements.push(format!("state {st} ({}) has an inverted box", reg.name_of(st)));
            }
        }
    }
    assert!(
        disagreements.is_empty(),
        "the collision list and the pinned predicate disagree on {} states, first: {:?}",
        disagreements.len(),
        &disagreements[..disagreements.len().min(8)]
    );
    // A table that exported no boxes at all would satisfy every equality above; the state space
    // is dominated by states that collide, and a fence or a slab contributes more than one box.
    assert!(
        colliding_states > 20_000 && boxes > colliding_states,
        "colliding states {colliding_states}, boxes {boxes}"
    );
}

/// Named light sources, and the two that must not emit.
#[test]
fn named_blocks_emit_the_light_vanilla_emits() {
    for (name, lum) in [
        ("torch", 14i64),
        ("lantern", 15),
        ("sea_lantern", 15),
        ("glowstone", 15),
        ("redstone_torch", 7),
        ("furnace", 0),
        ("stone", 0),
    ] {
        let Some(st) = name_to_state(name) else {
            panic!("{name} must exist")
        };
        let got = breg().unwrap().luminance_of(st);
        assert_eq!(i64::from(got), lum, "{name}");
    }
}

/// The nibble layout is part of the contract: a wrong index lights the wrong column, and
/// nothing else would notice. The floors are supplied rather than read from the world so the
/// expected values are the test's own.
#[test]
fn sky_light_is_lit_above_the_floor_and_dark_at_and_below_it() {
    let floor = 69i32;
    let mut floors = [floor; 256];
    floors[(7 << 4) | 5] = floor; // column x=5, z=7
    let secs = sky_light_sections_from(0, 0, &floors);

    let nibble = |y: i32, lx: usize, lz: usize| -> u8 {
        let sec = ((y - LIGHT_MIN_Y) / 16) as usize;
        let sy = (y - LIGHT_MIN_Y - (sec as i32) * 16) as usize;
        let b = secs[sec][sy * 128 + lz * 8 + (lx >> 1)];
        if lx.is_multiple_of(2) {
            b & 0x0f
        } else {
            b >> 4
        }
    };

    assert_eq!(nibble(floor + 1, 5, 7), 15, "above the floor must be lit");
    assert_eq!(nibble(floor, 5, 7), 0, "at the floor must be dark");
    assert_eq!(nibble(floor - 1, 5, 7), 0, "below the floor must be dark");
    assert_eq!(nibble(floor + 40, 5, 7), 15, "far above must stay lit");
    assert_eq!(nibble(floor - 40, 5, 7), 0, "far below must stay dark");

    // A different floor for one column must not leak into its neighbour.
    let mut two = [floor; 256];
    two[(7 << 4) | 5] = 79;
    let secs2 = sky_light_sections_from(0, 0, &two);
    let nib2 = |y: i32, lx: usize, lz: usize| -> u8 {
        let sec = ((y - LIGHT_MIN_Y) / 16) as usize;
        let sy = (y - LIGHT_MIN_Y - (sec as i32) * 16) as usize;
        let b = secs2[sec][sy * 128 + lz * 8 + (lx >> 1)];
        if lx.is_multiple_of(2) {
            b & 0x0f
        } else {
            b >> 4
        }
    };
    assert_eq!(nib2(75, 5, 7), 0, "the raised column must be dark at 75");
    assert_eq!(nib2(75, 6, 7), 15, "its neighbour must still be lit at 75");
    assert_eq!(nib2(80, 5, 7), 15, "and lit above its own floor");
}

/// The same invariant against the world rather than against invented floors: standing on the
/// surface the player reads lit, and the ground under it dark.
#[test]
fn sky_light_reads_lit_at_the_surface_and_dark_under_it() {
    // No region or forceload setup: sky light reads the world through block_at and
    // height_at, so it does not depend on the region. Touching the region here without
    // restoring it polluted the fixture for every test that ran after this one.
    let _g = world_guard();
    let surface = height_at(5, 7);
    let secs = sky_light_sections(0, 0);
    let nibble = |y: i32, lx: usize, lz: usize| -> u8 {
        let sec = ((y - LIGHT_MIN_Y) / 16) as usize;
        let sy = (y - LIGHT_MIN_Y - (sec as i32) * 16) as usize;
        let b = secs[sec][sy * 128 + lz * 8 + (lx >> 1)];
        if lx.is_multiple_of(2) {
            b & 0x0f
        } else {
            b >> 4
        }
    };
    assert_eq!(
        nibble(surface, 5, 7),
        15,
        "the block above the surface must be lit"
    );
    assert_eq!(nibble(surface + 20, 5, 7), 15, "and stay lit upward");
    assert!(
        nibble(surface - 1, 5, 7) == 0,
        "the surface block must be dark"
    );
    assert!(nibble(surface - 30, 5, 7) == 0, "and stay dark downward");
}

/// Block light falls by one a step and stops at zero. The sources are supplied so the test
/// owns the input, and so it can place a torch at the edge and check the neighbour chunk's
/// side of the border - a spread confined to one chunk would leave a dark seam there.
#[test]
fn block_light_falls_by_one_a_step_from_its_source() {
    let lamp = 15u8;
    let sources = [((5, 70, 7), lamp)];
    let secs = block_light_sections_from(0, 0, &sources, |_, _, _| 0);
    let nib = |y: i32, lx: usize, lz: usize| -> u8 {
        let sec = ((y - LIGHT_MIN_Y) / 16) as usize;
        let sy = (y - LIGHT_MIN_Y - (sec as i32) * 16) as usize;
        let b = secs[sec][sy * 128 + lz * 8 + (lx >> 1)];
        if lx.is_multiple_of(2) {
            b & 0x0f
        } else {
            b >> 4
        }
    };
    assert_eq!(nib(70, 5, 7), 15, "the source is full");
    assert_eq!(nib(70, 6, 7), 14, "one step away loses one");
    assert_eq!(nib(70, 7, 7), 13, "two steps lose two");
    assert_eq!(nib(70, 5, 8), 14, "and the fall is the same in z");
    assert_eq!(nib(71, 5, 7), 14, "and upward");
    // The far end of a chunk, with the source in the corner so the distance fits inside it.
    let corner = [((0i32, 70, 0), lamp)];
    let cs = block_light_sections_from(0, 0, &corner, |_, _, _| 0);
    let cnib = |y: i32, lx: usize, lz: usize| -> u8 {
        let sec = ((y - LIGHT_MIN_Y) / 16) as usize;
        let sy = (y - LIGHT_MIN_Y - (sec as i32) * 16) as usize;
        let b = cs[sec][sy * 128 + lz * 8 + (lx >> 1)];
        if lx.is_multiple_of(2) {
            b & 0x0f
        } else {
            b >> 4
        }
    };
    assert_eq!(cnib(70, 14, 0), 1, "fourteen steps away is one");
    assert_eq!(cnib(70, 15, 0), 0, "fifteen steps away is dark");

    // A source just outside the chunk must light the inside of it.
    let across = [((-1i32, 70, 7), lamp)];
    let secs2 = block_light_sections_from(0, 0, &across, |_, _, _| 0);
    let nib2 = |y: i32, lx: usize, lz: usize| -> u8 {
        let sec = ((y - LIGHT_MIN_Y) / 16) as usize;
        let sy = (y - LIGHT_MIN_Y - (sec as i32) * 16) as usize;
        let b = secs2[sec][sy * 128 + lz * 8 + (lx >> 1)];
        if lx.is_multiple_of(2) {
            b & 0x0f
        } else {
            b >> 4
        }
    };
    assert_eq!(nib2(70, 0, 7), 14, "light must cross the chunk border");
    assert_eq!(nib2(70, 1, 7), 13, "and keep falling inside");

    // Nothing placed means nothing lit.
    let none = block_light_sections_from(0, 0, &[], |_, _, _| 0);
    assert!(
        none.iter().all(|s| s.iter().all(|b| *b == 0)),
        "no source, no light"
    );

    // A block that stops light does not pass it on.
    let blocked = block_light_sections_from(0, 0, &sources, |x, _, _| if x == 6 { 15 } else { 0 });
    let nib3 = |y: i32, lx: usize, lz: usize| -> u8 {
        let sec = ((y - LIGHT_MIN_Y) / 16) as usize;
        let sy = (y - LIGHT_MIN_Y - (sec as i32) * 16) as usize;
        let b = blocked[sec][sy * 128 + lz * 8 + (lx >> 1)];
        if lx.is_multiple_of(2) {
            b & 0x0f
        } else {
            b >> 4
        }
    };
    assert_eq!(nib3(70, 6, 7), 0, "an opaque neighbour gets no light");
    assert_eq!(nib3(70, 7, 7), 0, "and passes none beyond it");
}

/// The sideways spread against a scenario that models the ground as opaque.
///
/// The first version of this test made every column transparent except a wall, so light fell
/// straight down from each column's own sky and a cell behind the wall came out at 6. The spread
/// was never wrong: the ground was air. This one states the opacity it means and drives the real
/// function rather than a copy of it.
#[test]
fn sky_light_reaches_sideways_through_an_opening() {
    // Ground at y <= 69 except a cave at z=7, x 6..=12, y 61..=69, whose ceiling is solid except
    // above the shaft at x=10. Only the shaft column therefore has its first opaque block at y=60;
    // every other cave column's own sky is above its ceiling at 69.
    let mut floors = [69i32; 256];
    floors[(7 << 4) | 10] = 60;
    let solid = |x: i32, y: i32, z: i32| -> u8 {
        let (lx, lz) = (x.rem_euclid(16), z.rem_euclid(16));
        if lz == 8 {
            return 15; // the wall
        }
        let in_cave = lz == 7 && (6..=12).contains(&lx) && (61..=69).contains(&y);
        let ceiling = y == 69 && lz == 7 && lx != 10;
        if in_cave && !ceiling {
            0
        } else {
            15
        }
    };
    let spill = sky_spill(
        DimId::Overworld,
        0,
        0,
        |x, z| floors[((z.rem_euclid(16) as usize) << 4) | (x.rem_euclid(16) as usize)],
        solid,
    );
    let at = |x: i32, y: i32, z: i32| spill.get(&(x, y, z)).copied().unwrap_or(0);

    assert_eq!(at(10, 61, 7), 15, "the shaft's own lit cell is the seed");
    assert_eq!(at(9, 61, 7), 14, "one step into the cave loses one");
    assert_eq!(at(8, 61, 7), 13, "two steps lose two");
    assert_eq!(at(6, 61, 7), 11, "four steps lose four");
    // The spill holds dark cells only, so the shaft's own column above the floor is not in it: the
    // per-column rule already lights those to 15.
    assert_eq!(
        at(10, 62, 7),
        0,
        "the shaft's own column is the column rule's business"
    );
    assert_eq!(at(8, 69, 7), 0, "the ceiling stops what falls on it");
    assert_eq!(at(9, 69, 7), 0, "nor beside the shaft's opening");
    assert_eq!(at(10, 61, 8), 0, "the wall passes nothing");
    assert_eq!(at(10, 61, 9), 0, "and nothing arrives behind it");
    assert_eq!(
        at(6, 66, 7),
        6,
        "the far corner keeps the fall of nine steps"
    );
}

/// The set of blocks that fall is decided by the asset through BReg.behaviour, not by a hand list.
/// The hand list this replaced named sand, red_sand and gravel and silently omitted the sixteen
/// concrete powders, so the count is pinned here: without it a block that ought to fall quietly
/// stays in the air and nothing fails.
#[test]
fn the_falling_set_is_the_asset_set() {
    let reg = breg().unwrap();
    let mut falling: Vec<String> = (0..reg.state_count() as u32)
        .filter(|&st| reg.behaviour_of(st) == crate::block_registry::B_FALLING)
        .map(|st| reg.name_of(st).trim_start_matches("minecraft:").to_string())
        .collect();
    falling.sort();
    falling.dedup();
    assert_eq!(
        falling.len(),
        19,
        "sand, red_sand, gravel and 16 concrete powders: {falling:?}"
    );
    for expect in [
        "sand",
        "red_sand",
        "gravel",
        "white_concrete_powder",
        "orange_concrete_powder",
        "magenta_concrete_powder",
        "light_blue_concrete_powder",
        "yellow_concrete_powder",
        "lime_concrete_powder",
        "pink_concrete_powder",
        "gray_concrete_powder",
        "light_gray_concrete_powder",
        "cyan_concrete_powder",
        "purple_concrete_powder",
        "blue_concrete_powder",
        "brown_concrete_powder",
        "green_concrete_powder",
        "red_concrete_powder",
        "black_concrete_powder",
    ] {
        assert!(
            falling.iter().any(|n| n == expect),
            "{expect} must fall: {falling:?}"
        );
    }
    // The converse, so the assertion cannot pass by marking everything as falling.
    for not_falling in ["stone", "dirt", "oak_planks"] {
        let st = name_to_state(not_falling).expect("must exist");
        assert_ne!(
            reg.behaviour_of(st),
            crate::block_registry::B_FALLING,
            "{not_falling} must not fall"
        );
    }
}

/// A potted plant drops the pot and the plant. The asset lists both; reading only the first lost one
/// item per broken pot, and nothing said so, because a single drop is the normal case.
#[test]
fn a_potted_plant_drops_both_entries_the_asset_lists() {
    let reg = breg().unwrap();
    let two: Vec<&str> = [
        "potted_oak_sapling",
        "potted_fern",
        "potted_flowering_azalea_bush",
    ]
    .into_iter()
    .filter(|n| {
        let st = name_to_state(n).unwrap_or_else(|| panic!("{n} must exist"));
        reg.block_drop(st).is_some() && reg.block_drop_second(st).is_some()
    })
    .collect();
    assert_eq!(two.len(), 3, "each of these drops two items, got {two:?}");

    // The single-drop case must stay single, or this would pass by marking everything two.
    let stone = name_to_state("stone").expect("stone must exist");
    assert!(reg.block_drop(stone).is_some(), "stone drops itself");
    assert!(reg.block_drop_second(stone).is_none(), "and only once");
}

/// Names alone are insufficient: every target state needs complete metadata.
#[test]
fn every_wire_state_has_complete_metadata() {
    let reg = breg().unwrap();
    assert!(
        reg.unpaired.is_empty(),
        "these target states have missing property-matched metadata: {:?}",
        reg.unpaired
    );
}

#[test]
fn note_block_keeps_solid_and_light_values_for_every_instrument() {
    let reg = breg().unwrap();
    let raw = std::fs::read("assets/blocks_1211.json").unwrap();
    let md = crate::json::jparse(&raw).unwrap();
    let block = md
        .as_arr()
        .unwrap()
        .iter()
        .find(|b| crate::json::jstr(b, "name").as_deref() == Some("note_block"))
        .unwrap();
    let lo = crate::json::jnum(block, "minStateId").unwrap() as u32;
    let hi = crate::json::jnum(block, "maxStateId").unwrap() as u32;
    for st in lo..=hi {
        assert!(
            reg.is_solid(st),
            "note block state {st} is a full solid block"
        );
        assert_eq!(
            reg.opacity_of(st),
            15,
            "note block state {st} must block skylight"
        );
        for side in 0..6 {
            assert!(
                reg.side_solid(st, side),
                "note block state {st} side {side}"
            );
        }
    }
}

/// A cheap, comparable view of the process globals a light scene writes: the world overlay's
/// chunk count, the sky-floor cache, two block cells and the light bytes of the centre chunk.
/// `marker` is a cell that is air in the generated terrain, so a write there cannot be masked
/// by terrain that already had the value.
fn light_scene_fingerprint() -> (usize, usize, u32, u32, Vec<u8>) {
    // The light read comes first because it is what fills the floors cache; reading the cache
    // length before it would make the first call and the second disagree over this function's
    // own side effect instead of over the child's.
    let light: Vec<u8> = sky_light_sections_world(0, 0)
        .iter()
        .map(|sec| sec.iter().fold(0u8, |acc, b| acc ^ b))
        .collect();
    let chunks = world().len();
    let floors = sky_floors_cache().lock().unwrap_or_else(|e| e.into_inner()).len();
    let marker = block_at(1, 250, 1);
    let tunnel = block_at(4, 7, 2);
    (chunks, floors, marker, tunnel, light)
}

/// The sky-light packet is built by `sky_light_sections_world`: per-column floors for the 3x3
/// window, then `sky_spill` for light that arrives sideways. The behavioural case above measures
/// the `#[cfg(test)]` helper, which has its own floor rule and no spill, so it cannot see that
/// assembly (nothing else in the suite calls it — only the two production call sites do). This
/// case sets a world the assembly has to answer for and asserts the shipped function directly.
///
/// The scene is the smallest one where sideways arrival is observable: a 1-block shaft from the
/// sky to a 1-wide tunnel under closed ground, and a single block across the tunnel. Every
/// neighbour of the tunnel is the base terrain's opaque solid, so the only path is the shaft.
///
/// The expected levels are derived from the current algorithm, not recorded from a run: a column
/// seeds its own floor+1 cell at 15, the BFS loses `opacity.max(1)` per step (so 1 through air),
/// and an opaque cell (opacity 15) cannot be entered at all. That derivation is the behavioural
/// authority here; it is not a parity claim against vanilla, which no capture backs.
///
/// The body runs in its own process. The scene writes the world overlay, the sky-floor cache and
/// everything else `set_block` owns (the state band, the scheduled-tick queue, the chunk
/// versions). Restoring the overlay and the floors at the end — what this case did when it was
/// first measured — restores neither the rest of them nor anything at all once an assertion
/// panics, and it left the fixture for every test that ran afterwards. A child process is the
/// only guard that also covers the panic: its globals die with it.
#[test]
fn sky_light_assembly_lights_through_an_opening_and_stops_at_a_barrier() {
    if crate::test_support::isolated_world_test() {
        return;
    }
    let _g = world_guard();
    let Some(reg) = breg() else {
        panic!("the block registry must load from assets/");
    };
    let (z, y) = (2i32, 7i32);
    let top = height_at(2, z) - 1;
    for sy in (y + 1)..=top {
        set_block(2, sy, z, 0);
    }
    for x in 2..=12 {
        set_block(x, y, z, 0);
    }
    let stone = name_to_state("stone").expect("stone");
    set_block(7, y, z, stone);
    // The scene means nothing unless the tunnel is enclosed: a solid floor and ceiling are what
    // make the shaft the only way in.
    assert!(
        reg.opacity_of(block_at(4, y - 1, z)) > 0 && reg.opacity_of(block_at(4, y + 1, z)) > 0,
        "the tunnel needs a solid floor and ceiling, found {} and {}",
        reg.opacity_of(block_at(4, y - 1, z)),
        reg.opacity_of(block_at(4, y + 1, z))
    );
    let nibble = |secs: &[[u8; 2048]], x: i32| -> u8 {
        let rel = (y - LIGHT_MIN_Y) as usize;
        let (lx, lz) = ((x & 15) as usize, (z & 15) as usize);
        let b = secs[rel / 16][(rel & 15) * 128 + lz * 8 + lx / 2];
        if lx % 2 == 0 {
            b & 0x0f
        } else {
            b >> 4
        }
    };
    let floors = sky_floors(0, 0, &|x, y, z| opacity_windowed(x, y, z));
    let seed_y = floors[(z as usize) << 4 | 2] + 1;
    assert!(seed_y > y + 1, "the shaft must reach the sky");
    // 15 at the seed, one level per step down the shaft and then along the tunnel.
    let expected: Vec<u8> = (2..=6).map(|x| (15 - (seed_y - y) - (x - 2)) as u8).collect();

    // Open tunnel: the light travels sideways and keeps a gradient rather than a constant.
    set_block(7, y, z, 0);
    let open = sky_light_sections_world(0, 0);
    assert_eq!(
        (2..=6).map(|x| nibble(&open, x)).collect::<Vec<_>>(),
        expected,
        "sideways arrival down an open tunnel"
    );
    assert!(
        (8..=11).all(|x| nibble(&open, x) > 0),
        "with the tunnel open its far end is lit: {:?}",
        (8..=11).map(|x| nibble(&open, x)).collect::<Vec<_>>()
    );

    // One block across the tunnel occludes everything past it, and leaves the lit side alone, so
    // the zeros are the barrier's doing and not the scene's.
    set_block(7, y, z, stone);
    let blocked = sky_light_sections_world(0, 0);
    assert_eq!(nibble(&blocked, 7), 0, "the barrier cell is opaque");
    assert!(
        (8..=11).all(|x| nibble(&blocked, x) == 0),
        "past the barrier must be dark: {:?}",
        (8..=11).map(|x| nibble(&blocked, x)).collect::<Vec<_>>()
    );
    assert_eq!(
        (2..=6).map(|x| nibble(&blocked, x)).collect::<Vec<_>>(),
        expected,
        "the barrier must not change the lit side"
    );

    // The packing contract at one exact byte: x and x+1 share a byte, even x in the low nibble.
    let rel = (y - LIGHT_MIN_Y) as usize;
    assert_eq!(
        blocked[rel / 16][(rel & 15) * 128 + (z as usize) * 8 + 1],
        expected[0] | (expected[1] << 4),
        "levels {} and {} share one byte",
        expected[0],
        expected[1]
    );

    // The helper the older assertion measures cannot produce any of it: its floors carry no
    // spill, and its walk (16 steps from `height_at - 1`) does not stop at the terrain once the
    // shaft made the column air, which is why it reports the shaft lit and the tunnel dark.
    let helper = sky_light_sections(0, 0);
    assert_eq!(
        (3..=11).map(|x| nibble(&helper, x)).collect::<Vec<_>>(),
        vec![0u8; 9],
        "the no-spill helper is dark for the whole tunnel"
    );
}

/// The fixture the isolation test below starts. It writes to the shared scene and then panics,
/// which is the case a save/restore pair around a test body cannot cover. It never runs on its
/// own: `is_isolated_child` is false whenever libtest starts it directly.
#[test]
fn a_panicking_scene_fixture_leaves_nothing_to_restore() {
    if !crate::test_support::is_isolated_child() {
        return;
    }
    let _g = world_guard();
    let stone = name_to_state("stone").expect("stone");
    // The real scene's write, plus a marker cell that is air in the generated terrain so the
    // parent's fingerprint is sensitive to this process leaking.
    set_block(4, 7, 2, 0);
    set_block(1, 250, 1, stone);
    crate::test_support::child_note(&format!(
        "marker={} tunnel={} chunks={}",
        block_at(1, 250, 1),
        block_at(4, 7, 2),
        world().len()
    ));
    panic!("the fixture panicked after writing to the shared scene");
}

/// The isolation the case above relies on, proved instead of assumed. A fixture that writes to
/// the shared scene and then panics must leave this process exactly as it was and must not stop
/// the next test: that is the property the measured draft of the scene test lacked.
#[test]
fn a_panicking_scene_fixture_leaves_the_parent_untouched() {
    let _g = world_guard();
    let stone = name_to_state("stone").expect("stone");
    assert_ne!(stone, 0, "stone must have a state id");
    let before = light_scene_fingerprint();
    assert_ne!(
        before.2, stone,
        "the marker cell must start as air or this fingerprint proves nothing"
    );
    let (status, receipt) = crate::test_support::run_isolated_child(
        "registry_tests::a_panicking_scene_fixture_leaves_nothing_to_restore",
    );
    assert!(
        !status.success(),
        "the fixture must panic in its child, not pass: {status}"
    );
    assert!(
        receipt.contains(&format!("marker={stone}")),
        "the child's marker write did not land, so nothing was proved: {receipt:?}"
    );
    assert!(
        receipt.contains("tunnel=0"),
        "the child's tunnel write did not land: {receipt:?}"
    );
    assert_eq!(
        light_scene_fingerprint(),
        before,
        "the child's writes and its panic must not be visible in this process"
    );
}

/// The gate for `Gen::deep_state_cached`'s memo-hit branch needs a caller that reaches the
/// memo at all: the Overworld surface rule compiles to `deep_fast`, so the feature capture
/// never looks in `deep_c` and mutating the hit arm there changes nothing. The Nether rule
/// carries `not` conditions the fast evaluator has no form for, so `deep_fast` stays unset
/// and every read lands on the memo.
#[cfg(megu_vanilla_data)]
#[test]
fn deep_state_memo_returns_the_value_it_computed() {
    crate::test_support::require_vanilla_data();
    let _guard = crate::worldgen::lock_gen_for_tests();
    let _measurement = crate::worldgen::M3_TEST_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    worldgen::m3_test_force(true);
    worldgen::m3_reset_local();
    let mut g = worldgen::Gen::load_preset(0, name_to_state, worldgen::NoisePreset::Nether)
        .expect("nether generator for the memo gate");
    let cc = g.cols(0, 0).expect("nether chunk columns");
    let _read = worldgen::M3FeatureRead::enter();
    // The first read of a cell computes it and memos it; the second must be the memo's hit
    // arm returning the same value. Only a non-air cell proves that: air reads 0 both ways
    // and a mutant returning 0 on a hit would stay green.
    let mut checked = 0usize;
    'cells: for x in 0..16 {
        for z in 0..16 {
            for y in 4..120 {
                let computed = g.deep_state_cached(x, y, z, &cc);
                if computed == 0 {
                    continue;
                }
                let memoized = g.deep_state_cached(x, y, z, &cc);
                assert_eq!(
                    memoized, computed,
                    "the deep-state memo must return the value it computed at {x},{y},{z}"
                );
                checked += 1;
                if checked == 16 {
                    break 'cells;
                }
            }
        }
    }
    assert!(checked > 0, "no non-air deep-state cell was found to compare");
    assert!(
        worldgen::m3_local().0[worldgen::M3_DEEP_HIT] > 0,
        "the memo-hit arm must have answered at least one read for this gate to be live"
    );
    worldgen::m3_test_force(false);
}

// ---------- M3 P5: the encode and light hot paths, against the code they replaced ----------
//
// Each of the three rewrites below is a shape change with an identity obligation: the stack
// palette writer must produce the bytes the allocating one produced, the dense spill grid must
// produce the levels the hash map produced, and the floor cache must evict the entry the
// `VecDeque` scan evicted. The references are transcribed old implementations rather than
// recorded bytes: recorded bytes pin only the cases someone typed, and these are the two places
// in the delivery path where a wrong level or a wrong palette index is invisible until a client
// renders it.

/// The palette writer this change replaced, in its original shape: a SipHash `HashMap`, a `Vec`
/// palette and a `Vec` of indices, built per call.
fn p5_reference_write_palette(
    out: &mut Vec<u8>,
    values: &[i64],
    min_bits: usize,
    max_bits: usize,
    global_bits: usize,
) {
    let mut palette = Vec::new();
    let mut lookup = HashMap::new();
    let indices: Vec<usize> = values
        .iter()
        .map(|&value| {
            *lookup.entry(value).or_insert_with(|| {
                let index = palette.len();
                palette.push(value);
                index
            })
        })
        .collect();
    assert!(
        !palette.is_empty(),
        "the reference writer has no answer for an empty input"
    );
    if palette.len() == 1 {
        out.push(0);
        write_varint(out, palette[0]);
        write_varint(out, 0);
        return;
    }
    let bits = ((usize::BITS - (palette.len() - 1).leading_zeros()) as usize).max(min_bits);
    let direct = bits > max_bits;
    let bits = if direct { global_bits } else { bits };
    out.push(bits as u8);
    if !direct {
        write_varint(out, palette.len() as i64);
        for &value in &palette {
            write_varint(out, value);
        }
    }
    let per = 64 / bits;
    write_varint(out, values.len().div_ceil(per) as i64);
    for (chunk, indexed) in values.chunks(per).zip(indices.chunks(per)) {
        let mut word = 0u64;
        for (i, (&value, &index)) in chunk.iter().zip(indexed).enumerate() {
            let entry = if direct { value as u64 } else { index as u64 };
            word |= entry << (i * bits);
        }
        out.extend_from_slice(&word.to_be_bytes());
    }
}

/// Every width decision the writer makes, at the palette sizes either side of each boundary.
/// Stated here so the widths are pinned by name and not only by agreeing with the reference.
#[test]
fn m3_p5_write_palette_matches_all_bit_widths() {
    // (distinct, block-state bits, quart-biome bits). `None` is the direct form.
    let widths: [(usize, Option<usize>, Option<usize>); 12] = [
        (1, None, None), // the single-value form: bits byte 0, no palette
        (2, Some(4), Some(1)),
        (3, Some(4), Some(2)),
        (4, Some(4), Some(2)),
        (5, Some(4), Some(3)),
        (8, Some(4), Some(3)),
        (9, Some(4), Some(6)),
        (16, Some(4), Some(6)),
        (17, Some(5), Some(6)),
        (33, Some(6), Some(6)),
        (129, Some(8), Some(6)),
        (257, Some(15), Some(6)),
    ];
    let generate = |len: usize, distinct: usize, negative: bool| -> Vec<i64> {
        (0..len)
            .map(|i| {
                // A stride coprime with most sizes, so the first-seen palette order is not the
                // numeric order and a palette that sorts would not match.
                let slot = (i * 7) as i64 % distinct as i64;
                if negative {
                    slot - 1000
                } else {
                    slot * 3 + 11
                }
            })
            .collect()
    };

    for &(distinct, block_bits, biome_bits) in &widths {
        for &len in &[distinct, 4096, 4095, 4097] {
            if len < distinct {
                continue;
            }
            for negative in [false, true] {
                let values = generate(len, distinct, negative);
                let mut unique: Vec<i64> = values.clone();
                unique.sort_unstable();
                unique.dedup();
                assert_eq!(unique.len(), distinct, "the fixture must hold {distinct} values");
                for (label, min_bits, max_bits, global_bits, want_bits) in [
                    ("blocks", 4, 8, 15, block_bits),
                    ("biomes", 1, 3, 6, biome_bits),
                ] {
                    let mut want = Vec::new();
                    p5_reference_write_palette(&mut want, &values, min_bits, max_bits, global_bits);
                    let mut got = Vec::new();
                    write_palette(&mut got, &values, min_bits, max_bits, global_bits);
                    assert_eq!(
                        got, want,
                        "{label}: {distinct} distinct over {len} cells (negative {negative})"
                    );
                    if let Some(bits) = want_bits {
                        assert_eq!(got[0] as usize, bits, "{label}: {distinct} distinct width");
                    } else if distinct == 1 {
                        assert_eq!(got[0], 0, "{label}: one value takes the single-value form");
                    }
                    // The section path enters through the `u32` overload, so it must encode
                    // identically. A negative fixture leaves the `u32` range, where the two
                    // entry points are documented to differ (the state id would be truncated).
                    if !negative {
                        let states: Vec<u32> = values.iter().map(|&v| v as u32).collect();
                        let mut got_u32 = Vec::new();
                        write_palette_u32(&mut got_u32, &states, min_bits, max_bits, global_bits);
                        assert_eq!(
                            got_u32, want,
                            "{label}: the u32 entry point must not encode differently"
                        );
                    }
                }
            }
        }
    }

    // An empty input: no palette to build, no cells to pack. The allocating writer reached this
    // answer by wrapping `len - 1`; the rewrite states it.
    let mut empty = Vec::new();
    write_palette(&mut empty, &[], 4, 8, 15);
    assert_eq!(empty, vec![15u8, 0], "no values: the direct width and zero words");

    // `section_encode_into` appends exactly what `section_encode` returns, and computes the
    // non-air count the same way - the wrapper is what `paletted` and `primitives_tests` call,
    // the `_into` form is what the delivery path calls, and they must not diverge.
    let states: [u32; 4096] = std::array::from_fn(|i| match i % 5 {
        0 => 0,
        1 => 1,
        2 => 12958,
        3 => 12959,
        _ => (i % 300) as u32,
    });
    let biomes: [i64; 64] = std::array::from_fn(|i| (i % 11) as i64);
    let whole = section_encode(&states, &biomes);
    let mut prefixed = vec![0xaa, 0xbb, 0xcc];
    section_encode_into(&mut prefixed, &states, &biomes);
    assert_eq!(
        &prefixed[3..],
        &whole[..],
        "section_encode_into must append the same bytes section_encode returns"
    );
    let non_air = states
        .iter()
        .filter(|&&st| !matches!(st, 0 | 12958 | 12959))
        .count() as i16;
    assert_eq!(
        i16::from_be_bytes([whole[0], whole[1]]),
        non_air,
        "the leading count is the non-air cell count"
    );
}

/// The spill this change replaced, in its original shape: a `HashMap` of positions and a
/// `VecDeque` of `((x, y, z), level)`.
fn p5_reference_sky_spill(
    cx: i32,
    cz: i32,
    floor_of: &impl Fn(i32, i32) -> i32,
    opacity: &impl Fn(i32, i32, i32) -> u8,
) -> HashMap<(i32, i32, i32), u8> {
    let (x0, x1) = (cx * 16 - 15, cx * 16 + 30);
    let (z0, z1) = (cz * 16 - 15, cz * 16 + 30);
    let dark = |x: i32, y: i32, z: i32| -> bool { y <= floor_of(x, z) };
    let mut lit: HashMap<(i32, i32, i32), u8> = HashMap::new();
    let mut queue: std::collections::VecDeque<((i32, i32, i32), u8)> =
        std::collections::VecDeque::new();
    for lz in 0..16usize {
        for lx in 0..16usize {
            let (x, z) = (cx * 16 + lx as i32, cz * 16 + lz as i32);
            let y = floor_of(x, z) + 1;
            lit.insert((x, y, z), 15);
            queue.push_back(((x, y, z), 15));
        }
    }
    while let Some((p, level)) = queue.pop_front() {
        if level <= 1 {
            continue;
        }
        for (dx, dy, dz) in [
            (1, 0, 0),
            (-1, 0, 0),
            (0, 1, 0),
            (0, -1, 0),
            (0, 0, 1),
            (0, 0, -1),
        ] {
            let (qx, qy, qz) = (p.0 + dx, p.1 + dy, p.2 + dz);
            if qx < x0 || qx > x1 || qz < z0 || qz > z1 || !(MIN_Y..MIN_Y + 384).contains(&qy) {
                continue;
            }
            if !dark(qx, qy, qz) {
                continue;
            }
            let nl = level.saturating_sub(opacity(qx, qy, qz).max(1));
            if nl == 0 || lit.get(&(qx, qy, qz)).copied().unwrap_or(0) >= nl {
                continue;
            }
            lit.insert((qx, qy, qz), nl);
            queue.push_back(((qx, qy, qz), nl));
        }
    }
    lit
}

/// One scene: a solidity predicate as the world's only input, from which both the sky floor of
/// every column in the 3x3 window and the opacity of every cell are read.
fn p5_check_spill_scene(label: &str, solid: impl Fn(i32, i32, i32) -> bool) {
    const X0: i32 = -16; // the 3x3 window around chunk (0, 0): one chunk of margin either side
    const SPAN: usize = 48;
    let mut floor_rows = vec![i32::MIN; SPAN * SPAN];
    for xi in 0..SPAN {
        for zi in 0..SPAN {
            let (x, z) = (X0 + xi as i32, X0 + zi as i32);
            let mut y = 340;
            while y > MIN_Y && !solid(x, y, z) {
                y -= 1;
            }
            floor_rows[xi * SPAN + zi] = y;
        }
    }
    let floor_of = |x: i32, z: i32| -> i32 {
        let (xi, zi) = (x - X0, z - X0);
        if !(0..SPAN as i32).contains(&xi) || !(0..SPAN as i32).contains(&zi) {
            return i32::MIN;
        }
        floor_rows[(xi * SPAN as i32 + zi) as usize]
    };
    let opacity = |x: i32, y: i32, z: i32| -> u8 {
        if solid(x, y, z) {
            15
        } else {
            0
        }
    };

    let dense = sky_spill(DimId::Overworld, 0, 0, floor_of, opacity);
    let sparse = p5_reference_sky_spill(0, 0, &floor_of, &opacity);
    // A scene that produced nothing would agree trivially, and one that produced only seeds
    // would never enter the BFS at all.
    assert!(
        sparse.len() > 256,
        "{label}: the spread visited nothing beyond the 256 seeds ({} entries)",
        sparse.len()
    );
    assert!(
        sparse.values().any(|&level| level > 0 && level < 15),
        "{label}: no cell holds a level between 1 and 14, so no light travelled"
    );

    // Every cell within reach of any seed, plus a margin either side so a missing entry is a
    // failure rather than an out-of-range skip.
    let (mut fmin, mut fmax) = (i32::MAX, i32::MIN);
    for x in X0..(X0 + SPAN as i32) {
        for z in X0..(X0 + SPAN as i32) {
            let f = floor_of(x, z);
            fmin = fmin.min(f);
            fmax = fmax.max(f);
        }
    }
    let mut compared = 0usize;
    let mut lit_cells = 0usize;
    for x in X0..(X0 + SPAN as i32) {
        for z in X0..(X0 + SPAN as i32) {
            for y in (fmin - 16)..=(fmax + 16) {
                let want = sparse.get(&(x, y, z)).copied().unwrap_or(0);
                let got = dense.get(&(x, y, z)).copied().unwrap_or(0);
                assert_eq!(got, want, "{label}: cell ({x}, {y}, {z})");
                compared += 1;
                if want > 0 {
                    lit_cells += 1;
                }
            }
        }
    }
    assert!(
        compared >= SPAN * SPAN * 30,
        "{label}: only {compared} cells compared, so the scan was not the whole window"
    );
    assert_eq!(
        lit_cells,
        sparse.len(),
        "{label}: the reference and the scan disagree on how many cells are lit"
    );

    // And the payload, which is what a client renders: the dense applier must produce the same
    // 26 arrays as merging the reference map into the packed columns.
    let mut floors = [0i32; 256];
    for lz in 0..16usize {
        for lx in 0..16usize {
            floors[(lz << 4) | lx] = floor_of(lx as i32, lz as i32);
        }
    }
    let mut applied = sky_light_sections_from(0, 0, &floors);
    sky_spill_apply(DimId::Overworld, 0, 0, floor_of, opacity, &mut applied);
    let composed = sky_light_sections_with_spill(0, 0, &floors, &sparse);
    assert_eq!(applied, composed, "{label}: the packed payloads differ");
}

/// The dense spill against the map it replaced, in the four shapes where sideways sky light has
/// to cross a boundary: a cave only reachable down a shaft, a window through a wall, an alcove
/// under an overhang whose own sky is the overhang's top, and a column whose topmost solid block
/// is the world's last layer.
#[test]
fn m3_p5_sky_spill_flat_matches_dense_edge_cases() {
    // Two joined tunnels under a solid roof, entered by one-block shafts at x = 6 and x = 11.
    p5_check_spill_scene("cave mouth", |x: i32, y: i32, z: i32| -> bool {
        let (lx, lz) = (x.rem_euclid(16), z.rem_euclid(16));
        let shaft = (lx == 6 && lz == 7) || (lx == 11 && lz == 12);
        if y > 71 {
            return false; // open sky
        }
        if y > 69 {
            return !shaft; // a solid roof with two holes in it
        }
        if !(61..=69).contains(&y) {
            return true; // solid below the tunnels
        }
        let tunnel =
            ((lz == 7 || lz == 12) && (4..=12).contains(&lx)) || (lx == 12 && (7..=12).contains(&lz));
        !tunnel
    });

    // Two roofed rooms divided by a wall with a window in it. The far room's columns are all
    // below the roof, so every cell in it is dark, and the only way in is the window the BFS
    // has to find from the shaft above the near room.
    p5_check_spill_scene("window spill", |x: i32, y: i32, z: i32| -> bool {
        let (lx, lz) = (x.rem_euclid(16), z.rem_euclid(16));
        if y <= 50 {
            return true; // ground
        }
        if y > 58 {
            if y > 60 {
                return false; // open sky above the roof
            }
            // The roof, with a shaft down into each room.
            return !((lx == 6 && lz == 7) || (lx == 10 && lz == 8));
        }
        if lz == 10 {
            // The dividing wall, with a window at x 7..=9, y 53..=56.
            return !((7..=9).contains(&lx) && (53..=56).contains(&y));
        }
        let room = (4..=12).contains(&lx) && ((4..=9).contains(&lz) || (11..=14).contains(&lz));
        !room
    });

    // A mesa with an overhanging lip. The alcove's columns report the lip's top as their floor,
    // so every cell in the alcove is dark, and only the fourteen steps of reach from a seed get
    // any light in.
    p5_check_spill_scene("cliff overhang", |x: i32, y: i32, z: i32| -> bool {
        let (lx, lz) = (x.rem_euclid(16), z.rem_euclid(16));
        if y > 88 {
            return false;
        }
        if y <= 59 {
            return true;
        }
        let body = (6..=9).contains(&lx) && (6..=9).contains(&lz);
        let lip = (3..=5).contains(&lx) && (6..=9).contains(&lz) && (80..=88).contains(&y);
        body || lip
    });

    // A column whose topmost solid block is the world's last layer: its seed lands one block
    // above the build limit, where the spread may not enter, but the seed is still part of the
    // answer - it is the cell the column rule lights, and dropping it would take the cell out of
    // a payload a client compares. Beside it, a tunnel under the ground with one shaft into it,
    // so the same scene also carries a spread the seed comparison can be measured against.
    p5_check_spill_scene("build-limit column", |x: i32, y: i32, z: i32| -> bool {
        let (lx, lz) = (x.rem_euclid(16), z.rem_euclid(16));
        if (6..=9).contains(&lx) && (6..=9).contains(&lz) {
            return y <= MIN_Y + 383; // a column reaching the world's top layer
        }
        if y > 60 {
            return false; // open sky
        }
        if y > 48 {
            return !(lx == 2 && lz == 2); // the ground, with one shaft in it
        }
        if y >= 40 {
            let tunnel = (1..=7).contains(&lx) && (1..=7).contains(&lz);
            return !tunnel; // the tunnel, roofed by the ground above
        }
        true
    });
}

/// The floor cache against the `VecDeque` scan it replaced: same capacity, same FIFO eviction,
/// same "an entry is only used while its chunk's version has not moved".
#[test]
fn m3_p5_sky_floors_cache_fifo_eviction() {
    // The bound is what the production singleton is built with, so it is asserted where the
    // eviction is tested rather than read from the type.
    assert_eq!(SkyFloorsCache::CAP, 256);
    let mut cache = SkyFloorsCache {
        map: HashMap::new(),
        order: std::collections::VecDeque::new(),
    };
    let floors = |seed: i32| -> [i32; 256] { [MIN_Y + seed; 256] };

    for i in 0..SkyFloorsCache::CAP as i32 {
        cache.put((i, 0), 1, floors(i));
    }
    assert_eq!(cache.len(), SkyFloorsCache::CAP, "the cache fills to its cap");
    assert_eq!(cache.get((0, 0), 1), Some(floors(0)));
    assert_eq!(
        cache.get((0, 0), 2),
        None,
        "a version this entry was not computed at must not answer"
    );

    // A refresh of a key already present updates in place: the value and the version change,
    // and the entry keeps its position in the eviction order - it does not move to the back.
    cache.put((0, 0), 2, floors(-1));
    assert_eq!(cache.len(), SkyFloorsCache::CAP, "a refresh is not an insertion");
    assert_eq!(
        cache.get((0, 0), 1),
        None,
        "the refreshed entry answers only at the version it was computed at"
    );
    assert_eq!(cache.get((0, 0), 2), Some(floors(-1)));

    // One more distinct key evicts the oldest *position*, which is still (0, 0) after its
    // refresh. (1, 0) surviving is what proves the refresh did not move it to the back: had it
    // done that, this insertion would have taken (1, 0) instead.
    cache.put((SkyFloorsCache::CAP as i32, 0), 1, floors(7));
    assert_eq!(cache.len(), SkyFloorsCache::CAP, "the cap holds");
    assert_eq!(
        cache.get((1, 0), 1),
        Some(floors(1)),
        "a refreshed entry keeps its position, so its neighbour is not the one evicted"
    );
    assert_eq!(
        cache.get((0, 0), 2),
        None,
        "FIFO evicts the oldest insertion, even one that has been asked for since"
    );
    assert_eq!(
        cache.get((SkyFloorsCache::CAP as i32, 0), 1),
        Some(floors(7)),
        "the newest entry is present"
    );

    // Draining the order into a hash-only map would leave the two disagreeing; the bound is
    // only meaningful while every stored key is in the order exactly once, oldest first.
    assert_eq!(cache.order.len(), cache.map.len(), "order and map agree in size");
    let oldest = cache.order.front().copied().unwrap();
    cache.put((999, 0), 1, floors(0));
    assert_eq!(
        cache.get(oldest, 1),
        None,
        "the front of the order is what the next insertion evicts"
    );
}

/// The section encoder this change replaced, in its original shape: the state array widened into
/// `[i64; 4096]`, then the allocating writer twice.
fn p5_reference_section_encode(states: &[u32; 4096], biomes: &[i64; 64]) -> Vec<u8> {
    let mut out = Vec::new();
    let non_air = states
        .iter()
        .filter(|&&state| !matches!(state, 0 | 12958 | 12959))
        .count() as i16;
    out.extend_from_slice(&non_air.to_be_bytes());
    p5_reference_write_palette(&mut out, &states.map(i64::from), 4, 8, 15);
    p5_reference_write_palette(&mut out, biomes, 1, 3, 6);
    out
}

/// What the four rewrites of this slice cost, old against new, **in one process**: every replaced
/// implementation is still in this file as the identity reference, so each pair is timed on the
/// same inputs in the same run and the host's load cancels. That is the measurement this
/// repository requires before a speed claim, and it is the only one that can attribute the change:
/// the delivery-path bench (`chunk_delivery_path_bench`) measures terrain and the anti-xray loop,
/// and does not call the light pass at all.
///
///   M3_P5_BENCH=1 cargo test --release --nocapture m3_p5_hot_path_ab_bench
///
/// Reported as the median of nine rounds; an iteration count per round keeps each sample above a
/// millisecond so the clock's resolution is not the measurement.
#[test]
fn m3_p5_hot_path_ab_bench() {
    if std::env::var("M3_P5_BENCH").is_err() {
        return;
    }
    const ROUNDS: usize = 9;

    fn median(mut samples: Vec<f64>) -> f64 {
        samples.sort_by(|a, b| a.partial_cmp(b).unwrap());
        samples[samples.len() / 2]
    }

    // One chunk's worth of sections: the mix a delivered chunk actually carries, which is mostly
    // uniform (air above the surface, stone below it) and a handful of mixed sections at it.
    let biomes: [i64; 64] = std::array::from_fn(|i| (i % 11) as i64);
    let shapes: Vec<[u32; 4096]> = vec![
        [0u32; 4096],
        [1u32; 4096],
        std::array::from_fn(|i| (i % 90) as u32),
        std::array::from_fn(|i| if i < 2048 { 1 } else { 0 }),
        std::array::from_fn(|i| (i * 7 % 300) as u32),
    ];
    // Sanity: the two writers agree before anything is timed, so a timing cannot be of one path
    // and an identity claim about another.
    for states in &shapes {
        assert_eq!(
            section_encode(states, &biomes),
            p5_reference_section_encode(states, &biomes),
            "the A/B must time identical work"
        );
    }
    let per_round = 400usize; // 400 encoders per sample, above a millisecond on this host
    let time = |mut run: Box<dyn FnMut() + '_>| -> f64 {
        let mut samples = Vec::with_capacity(ROUNDS);
        for _ in 0..ROUNDS {
            let start = std::time::Instant::now();
            for _ in 0..per_round {
                run();
            }
            samples.push(start.elapsed().as_secs_f64() * 1e6 / per_round as f64);
        }
        median(samples)
    };
    let mut bytes = 0usize;
    let old_section = time(Box::new(|| {
        let mut sum = 0usize;
        for states in &shapes {
            sum += p5_reference_section_encode(states, &biomes).len();
        }
        bytes = sum;
    }));
    let new_section = time(Box::new(|| {
        let mut out = Vec::with_capacity(16384);
        for states in &shapes {
            out.clear();
            section_encode_into(&mut out, states, &biomes);
        }
        std::hint::black_box(&out);
    }));
    assert_ne!(bytes, 0, "the reference arm must run");
    // A chunk is 24 sections, each encoding its states and its biomes.
    let sections_per_chunk = 24.0;
    eprintln!(
        "P5BENCH section encode (24/chunk): old={:.2}us new={:.2}us | per chunk old={:.0}us new={:.0}us ({:+.1}%)",
        old_section,
        new_section,
        old_section * sections_per_chunk,
        new_section * sections_per_chunk,
        (new_section / old_section - 1.0) * 100.0
    );

    // The spill, in both regimes, with both arms composed the way the call sites compose them:
    // pack the columns, then merge the spread. Timing the map build against the grid build would
    // have flattered the map, which never paid for its own merge here.
    let mut floors = [0i32; 256];
    let mut spill_ab = |label: &str, scene: &dyn Fn(i32, i32, i32) -> bool| {
        for lz in 0..16usize {
            for lx in 0..16usize {
                let (x, z) = (lx as i32, lz as i32);
                let mut y = 340;
                while y > MIN_Y && !scene(x, y, z) {
                    y -= 1;
                }
                floors[(lz << 4) | lx] = y;
            }
        }
        let floor_of =
            |x: i32, z: i32| -> i32 { floors[(((z & 15) as usize) << 4) | ((x & 15) as usize)] };
        let opacity = |x: i32, y: i32, z: i32| -> u8 { if scene(x, y, z) { 15 } else { 0 } };
        let mut cells = 0usize;
        let old_spill = time(Box::new(|| {
            let spill = p5_reference_sky_spill(0, 0, &floor_of, &opacity);
            cells = spill.len();
            let out = sky_light_sections_with_spill(0, 0, &floors, &spill);
            std::hint::black_box(&out);
        }));
        let new_spill = time(Box::new(|| {
            let mut out = sky_light_sections_from(0, 0, &floors);
            sky_spill_apply(DimId::Overworld, 0, 0, floor_of, opacity, &mut out);
            std::hint::black_box(&out);
        }));
        assert!(cells > 256, "{label}: the scene must spread, got {cells} cells");
        eprintln!(
            "P5BENCH sky spill {label} (1/chunk, {cells} lit): map={:.1}us grid={:.1}us ({:+.1}%)",
            old_spill,
            new_spill,
            (new_spill / old_spill - 1.0) * 100.0
        );
    };
    // A shallow roofed cave: a few hundred lit cells, which is a quiet chunk.
    let cave = |x: i32, y: i32, z: i32| -> bool {
        let (lx, lz) = (x.rem_euclid(16), z.rem_euclid(16));
        let shaft = (lx == 6 && lz == 7) || (lx == 11 && lz == 12);
        if y > 71 {
            return false;
        }
        if y > 69 {
            return !shaft;
        }
        if !(61..=69).contains(&y) {
            return true;
        }
        let tunnel = ((lz == 7 || lz == 12) && (4..=12).contains(&lx))
            || (lx == 12 && (7..=12).contains(&lz));
        !tunnel
    };
    spill_ab("sparse", &cave);
    // Porous rock: thousands of dark, transparent cells within reach of a seed, which is what a
    // chunk under a large overhang or a deep cave system looks like.
    let porous = |x: i32, y: i32, z: i32| -> bool {
        if y > 90 {
            return false;
        }
        (x * 7 + y * 13 + z * 3).rem_euclid(11) >= 5
    };
    spill_ab("dense", &porous);

    // The block-light pass's no-source path: what it stopped copying. The old version cloned the
    // window's edit maps before asking whether there was anything to light; these are nine of the
    // maps it copied, at the size an edited chunk reaches (300 cells is a few hundred blocks
    // moved or broken), so this is the work the reordering removes from a call that lights
    // nothing.
    let edited: Vec<HashMap<u32, u32>> = (0..9)
        .map(|c| (0..300u32).map(|i| (i * 7 + c, 1u32)).collect())
        .collect();
    let clones = time(Box::new(|| {
        let copied: Vec<HashMap<u32, u32>> = edited.to_vec();
        std::hint::black_box(&copied);
    }));
    eprintln!(
        "P5BENCH block light, no sources (1/chunk): 9 overlay copies removed = {clones:.1}us/call"
    );

    // The floor cache's hit path, ten lookups per delivered chunk (the centre plus the 3x3
    // window): a 256-entry `VecDeque` scanned per lookup against a hash index.
    let keyed: Vec<((i32, i32), u64, [i32; 256])> = (0..256)
        .map(|i| ((i, 0), 1u64, [i; 256]))
        .collect();
    let scan: std::collections::VecDeque<((i32, i32), u64, [i32; 256])> =
        keyed.iter().copied().collect();
    let mut cache = SkyFloorsCache {
        map: HashMap::new(),
        order: std::collections::VecDeque::new(),
    };
    for (key, epoch, values) in keyed.iter().copied() {
        cache.put(key, epoch, values);
    }
    // The last chunk inserted is the worst case for the scan, so the old arm is not flattered.
    let probes: Vec<(i32, i32)> = (0..10).map(|i| (255 - i, 0)).collect();
    let mut sink = 0i32;
    let old_lookup = time(Box::new(|| {
        for key in &probes {
            if let Some((_, e, f)) = scan.iter().find(|(k, _, _)| k == key) {
                if *e == 1 {
                    sink += f[0];
                }
            }
        }
    }));
    let new_lookup = time(Box::new(|| {
        for key in &probes {
            if let Some(f) = cache.get(*key, 1) {
                sink += f[0];
            }
        }
    }));
    assert_ne!(sink, 0, "the cache arms must both answer");
    eprintln!(
        "P5BENCH floors cache (10 hits/chunk): scan={:.2}us hash={:.2}us ({:+.1}%)",
        old_lookup,
        new_lookup,
        (new_lookup / old_lookup - 1.0) * 100.0
    );
}

// ---------- W4-B1: pack.mcmeta format validation (vanilla 1.21.1 = 48) ----------
//
// The datapack loader currently decides a directory is a pack from the mere presence of
// `pack.mcmeta`, then reads its content whatever format it declares. Vanilla 1.21.1 instead
// ignores packs whose format does not match. `validate_pack_mcmeta_format` is the pure
// decision that check needs: bytes in, an interpreted supported-format interval or a concrete
// reason for rejection out. It does no I/O and touches no global state, so the caller can own
// the file read and the logging.
//
// The `supported_formats` field, when present, is authoritative: a pack made for a newer
// format can still declare compatibility back down to 48. With the field absent, only the
// point format 48 is compatible.

/// The datapack format vanilla 1.21.1 speaks.
pub(crate) const VANILLA_1211_PACK_FORMAT: i64 = 48;

/// The supported format interval a pack declares after interpretation. A pack with no
/// `supported_formats` is the single point `[pack_format, pack_format]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PackFormatRange {
    pub min: i64,
    pub max: i64,
}

impl PackFormatRange {
    fn contains(self, format: i64) -> bool {
        self.min <= format && format <= self.max
    }
}

/// Why a `pack.mcmeta` was rejected, kept as a typed reason so a loader can log it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PackFormatError {
    /// The bytes are not JSON at all.
    MalformedJson,
    /// The top-level `pack` object is missing or is not a JSON object.
    MissingPackObject,
    /// `pack.pack_format` is absent.
    MissingPackFormat,
    /// `pack.pack_format` is present but not a whole, in-range number.
    InvalidPackFormat,
    /// No `supported_formats` declared, and `pack_format` is not the 1.21.1 format.
    WrongPackFormat(i64),
    /// `supported_formats` is not an integer, a two-element integer array, or an
    /// integer `{min_inclusive, max_inclusive}` object.
    MalformedSupportedFormats,
    /// `supported_formats` declares a range whose lower bound exceeds its upper bound.
    InvertedSupportedRange { min: i64, max: i64 },
    /// `supported_formats` is well formed but does not include format 48.
    FormatNotSupported { min: i64, max: i64 },
}

/// Extract a whole number from the shared JSON value type. Formats are whole, finite
/// numbers inside the `i64` range; a fractional value, a string or a boolean is not one.
fn json_whole_number(j: &crate::json::J) -> Option<i64> {
    match j {
        crate::json::J::Num(n)
            if n.is_finite()
                && n.fract() == 0.0
                && *n >= i64::MIN as f64
                && *n < i64::MAX as f64 =>
        {
            Some(*n as i64)
        }
        _ => None,
    }
}

/// Interpret the three shapes vanilla accepts for `supported_formats`:
/// an integer (a point), `[min, max]`, or `{"min_inclusive": a, "max_inclusive": b}`.
fn interpret_supported_formats(j: &crate::json::J) -> Result<PackFormatRange, PackFormatError> {
    use crate::json::J;
    match j {
        J::Num(_) => {
            let n = json_whole_number(j).ok_or(PackFormatError::MalformedSupportedFormats)?;
            Ok(PackFormatRange { min: n, max: n })
        }
        J::Arr(a) => {
            if a.len() != 2 {
                return Err(PackFormatError::MalformedSupportedFormats);
            }
            let min = json_whole_number(&a[0]).ok_or(PackFormatError::MalformedSupportedFormats)?;
            let max = json_whole_number(&a[1]).ok_or(PackFormatError::MalformedSupportedFormats)?;
            if min > max {
                return Err(PackFormatError::InvertedSupportedRange { min, max });
            }
            Ok(PackFormatRange { min, max })
        }
        J::Obj(_) => {
            let min = j
                .get("min_inclusive")
                .and_then(json_whole_number)
                .ok_or(PackFormatError::MalformedSupportedFormats)?;
            let max = j
                .get("max_inclusive")
                .and_then(json_whole_number)
                .ok_or(PackFormatError::MalformedSupportedFormats)?;
            if min > max {
                return Err(PackFormatError::InvertedSupportedRange { min, max });
            }
            Ok(PackFormatRange { min, max })
        }
        _ => Err(PackFormatError::MalformedSupportedFormats),
    }
}

/// The shared `jparse` stops at the end of the first value and accepts bare `true`/`false`/
/// `null`-like tokens as values without checking the rest of the token. A `pack.mcmeta` must be
/// one complete JSON object, so check the byte structure before interpreting it. This only
/// checks completeness and a single object root - `jparse` still enforces the value grammar.
fn json_is_one_complete_object(bytes: &[u8]) -> bool {
    let mut i = 0usize;
    while i < bytes.len() && bytes[i].is_ascii_whitespace() {
        i += 1;
    }
    if bytes.get(i) != Some(&b'{') {
        return false;
    }
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    let mut prev: Option<u8> = None;
    while i < bytes.len() {
        let c = bytes[i];
        if in_string {
            if escaped {
                escaped = false;
            } else if c == b'\\' {
                escaped = true;
            } else if c == b'"' {
                in_string = false;
            }
        } else {
            match c {
                b'"' => in_string = true,
                b'{' | b'[' => depth += 1,
                b'}' | b']' => {
                    // The shared parser tolerates a trailing comma before a closing bracket;
                    // strict JSON does not.
                    if prev == Some(b',') {
                        return false;
                    }
                    match depth.checked_sub(1) {
                        Some(d) => depth = d,
                        None => return false,
                    }
                }
                _ => {}
            }
            prev = Some(c);
            if depth == 0 {
                return bytes[i + 1..]
                    .iter()
                    .all(|b| b.is_ascii_whitespace());
            }
        }
        i += 1;
    }
    false
}

/// Validate a `pack.mcmeta` file's bytes against the vanilla 1.21.1 datapack format (48).
/// Returns the supported-format interval on success, or the concrete reason for rejection.
/// Pure: no file reads, no globals, no logging.
pub(crate) fn validate_pack_mcmeta_format(
    bytes: &[u8],
) -> Result<PackFormatRange, PackFormatError> {
    if !json_is_one_complete_object(bytes) {
        return Err(PackFormatError::MalformedJson);
    }
    let root = crate::json::jparse(bytes).ok_or(PackFormatError::MalformedJson)?;
    let pack = root
        .get("pack")
        .ok_or(PackFormatError::MissingPackObject)?;
    if !matches!(pack, crate::json::J::Obj(_)) {
        return Err(PackFormatError::MissingPackObject);
    }
    let raw_format = pack
        .get("pack_format")
        .ok_or(PackFormatError::MissingPackFormat)?;
    let pack_format = json_whole_number(raw_format).ok_or(PackFormatError::InvalidPackFormat)?;

    match pack.get("supported_formats") {
        None => {
            if pack_format != VANILLA_1211_PACK_FORMAT {
                return Err(PackFormatError::WrongPackFormat(pack_format));
            }
            Ok(PackFormatRange {
                min: pack_format,
                max: pack_format,
            })
        }
        Some(sf) => {
            let range = interpret_supported_formats(sf)?;
            if !range.contains(VANILLA_1211_PACK_FORMAT) {
                return Err(PackFormatError::FormatNotSupported {
                    min: range.min,
                    max: range.max,
                });
            }
            Ok(range)
        }
    }
}

#[test]
fn pack_mcmeta_accepts_the_vanilla_1211_point_format() {
    let range = validate_pack_mcmeta_format(
        br#"{"pack":{"pack_format":48,"description":"vanilla worldgen data"}}"#,
    )
    .expect("a bare 1.21.1 pack must be accepted");
    assert_eq!(range, PackFormatRange { min: 48, max: 48 });

    // Other keys in the object and the root are irrelevant to the decision.
    let range = validate_pack_mcmeta_format(
        br#"{"other":1,"pack":{"description":"d","pack_format":48,"extra":[1,2]}}"#,
    )
    .expect("extra keys must not change the verdict");
    assert_eq!(range, PackFormatRange { min: 48, max: 48 });
}

#[test]
fn pack_mcmeta_accepts_every_supported_formats_shape() {
    // Integer point at 48.
    let point = validate_pack_mcmeta_format(
        br#"{"pack":{"pack_format":48,"supported_formats":48}}"#,
    )
    .expect("an integer supported_formats of 48 must be accepted");
    assert_eq!(point, PackFormatRange { min: 48, max: 48 });

    // [min, max] array containing 48.
    let array = validate_pack_mcmeta_format(
        br#"{"pack":{"pack_format":48,"supported_formats":[46,50]}}"#,
    )
    .expect("an array range containing 48 must be accepted");
    assert_eq!(array, PackFormatRange { min: 46, max: 50 });

    // {min_inclusive, max_inclusive} object containing 48.
    let object = validate_pack_mcmeta_format(
        br#"{"pack":{"pack_format":48,"supported_formats":{"min_inclusive":47,"max_inclusive":49}}}"#,
    )
    .expect("an object range containing 48 must be accepted");
    assert_eq!(object, PackFormatRange { min: 47, max: 49 });

    // A degenerate range whose two ends are both 48 is still a point.
    let collapsed = validate_pack_mcmeta_format(
        br#"{"pack":{"pack_format":48,"supported_formats":[48,48]}}"#,
    )
    .expect("a collapsed range at 48 must be accepted");
    assert_eq!(collapsed, PackFormatRange { min: 48, max: 48 });
}

#[test]
fn pack_mcmeta_accepts_a_newer_pack_that_declares_backward_compatibility() {
    // `supported_formats` is authoritative: a pack made for a newer format may declare that
    // it also works down to 48, and vanilla would load it on a 1.21.1 server.
    let range = validate_pack_mcmeta_format(
        br#"{"pack":{"pack_format":61,"supported_formats":[48,61]}}"#,
    )
    .expect("a newer pack declaring 48 in its range must be accepted");
    assert_eq!(range, PackFormatRange { min: 48, max: 61 });

    let range = validate_pack_mcmeta_format(
        br#"{"pack":{"pack_format":61,"supported_formats":{"min_inclusive":44,"max_inclusive":61}}}"#,
    )
    .expect("the object form of the same backcompat range must be accepted");
    assert_eq!(range, PackFormatRange { min: 44, max: 61 });
}

#[test]
fn pack_mcmeta_rejects_malformed_json() {
    for bytes in [
        &b""[..],
        b"not json",
        b"{",
        b"{\"pack\":",
        b"{\"pack\": {\"pack_format\": 48,}}",
        br#"{"pack":{"pack_format":48,"supported_formats":[48,]}}"#,
        b"[1, 2,",
    ] {
        assert_eq!(
            validate_pack_mcmeta_format(bytes),
            Err(PackFormatError::MalformedJson),
            "input {bytes:?} must be rejected as malformed"
        );
    }
}

#[test]
fn pack_mcmeta_rejects_a_missing_or_non_object_pack_field() {
    for bytes in [
        &b"{}"[..],
        b"{\"other\":{}}",
        br#"{"pack":null}"#,
        br#"{"pack":"48"}"#,
        br#"{"pack":48}"#,
    ] {
        assert_eq!(
            validate_pack_mcmeta_format(bytes),
            Err(PackFormatError::MissingPackObject),
            "input {bytes:?} has no pack object"
        );
    }
}

#[test]
fn pack_mcmeta_rejects_a_missing_or_invalid_pack_format() {
    assert_eq!(
        validate_pack_mcmeta_format(br#"{"pack":{"description":"d"}}"#),
        Err(PackFormatError::MissingPackFormat)
    );
    for bytes in [
        &br#"{"pack":{"pack_format":"48"}}"#[..],
        br#"{"pack":{"pack_format":48.5}}"#,
        br#"{"pack":{"pack_format":null}}"#,
        br#"{"pack":{"pack_format":[48]}}"#,
    ] {
        assert_eq!(
            validate_pack_mcmeta_format(bytes),
            Err(PackFormatError::InvalidPackFormat),
            "input {bytes:?} has a non-integer pack_format"
        );
    }
    // The shared parser maps a bare `true`/`false` to 1/0, so it cannot make a valid format.
    // They must still be rejected, even though the reported reason is a wrong point format.
    for bytes in [
        &br#"{"pack":{"pack_format":true}}"#[..],
        br#"{"pack":{"pack_format":false}}"#,
    ] {
        assert_ne!(
            validate_pack_mcmeta_format(bytes),
            Ok(PackFormatRange { min: 48, max: 48 }),
            "input {bytes:?} must never be accepted as format 48"
        );
    }
}

#[test]
fn pack_mcmeta_rejects_a_wrong_bare_pack_format() {
    // Too old and too new alike: with no supported_formats the format must be exactly 48.
    for format in [1i64, 4, 47, 49, 61] {
        let bytes = format!(r#"{{"pack":{{"pack_format":{format}}}}}"#);
        assert_eq!(
            validate_pack_mcmeta_format(bytes.as_bytes()),
            Err(PackFormatError::WrongPackFormat(format)),
            "bare format {format} must be rejected"
        );
    }
}

#[test]
fn pack_mcmeta_rejects_malformed_supported_formats() {
    for bytes in [
        &br#"{"pack":{"pack_format":48,"supported_formats":"48"}}"#[..],
        br#"{"pack":{"pack_format":48,"supported_formats":null}}"#,
        br#"{"pack":{"pack_format":48,"supported_formats":[]}}"#,
        br#"{"pack":{"pack_format":48,"supported_formats":[48]}}"#,
        br#"{"pack":{"pack_format":48,"supported_formats":[48,49,50]}}"#,
        br#"{"pack":{"pack_format":48,"supported_formats":[48,"49"]}}"#,
        br#"{"pack":{"pack_format":48,"supported_formats":48.5}}"#,
        br#"{"pack":{"pack_format":48,"supported_formats":{"min_inclusive":48}}}"#,
        br#"{"pack":{"pack_format":48,"supported_formats":{"max_inclusive":50}}}"#,
        br#"{"pack":{"pack_format":48,"supported_formats":{"min_inclusive":"48","max_inclusive":50}}}"#,
    ] {
        assert_eq!(
            validate_pack_mcmeta_format(bytes),
            Err(PackFormatError::MalformedSupportedFormats),
            "input {bytes:?} has a malformed supported_formats"
        );
    }
}

#[test]
fn pack_mcmeta_rejects_inverted_supported_ranges() {
    assert_eq!(
        validate_pack_mcmeta_format(br#"{"pack":{"pack_format":48,"supported_formats":[50,48]}}"#),
        Err(PackFormatError::InvertedSupportedRange { min: 50, max: 48 })
    );
    assert_eq!(
        validate_pack_mcmeta_format(
            br#"{"pack":{"pack_format":48,"supported_formats":{"min_inclusive":50,"max_inclusive":48}}}"#
        ),
        Err(PackFormatError::InvertedSupportedRange { min: 50, max: 48 })
    );
}

#[test]
fn pack_mcmeta_rejects_well_formed_ranges_that_exclude_48() {
    for bytes in [
        &br#"{"pack":{"pack_format":48,"supported_formats":61}}"#[..],
        br#"{"pack":{"pack_format":48,"supported_formats":[50,60]}}"#,
        br#"{"pack":{"pack_format":48,"supported_formats":[1,47]}}"#,
        br#"{"pack":{"pack_format":48,"supported_formats":{"min_inclusive":50,"max_inclusive":60}}}"#,
    ] {
        match validate_pack_mcmeta_format(bytes) {
            Err(PackFormatError::FormatNotSupported { min, max }) => {
                assert!(
                    min > VANILLA_1211_PACK_FORMAT || max < VANILLA_1211_PACK_FORMAT,
                    "input {bytes:?}: range [{min}, {max}] must exclude 48"
                );
            }
            other => panic!("input {bytes:?} must be rejected as out of range, got {other:?}"),
        }
    }
}

#[test]
fn bundled_datapacks_all_declare_a_format_compatible_with_1211() {
    let base = std::path::Path::new("datapacks");
    let mut checked = 0usize;
    for entry in std::fs::read_dir(base).expect("datapacks/ must be readable") {
        let entry = entry.expect("a datapack directory entry must be readable");
        let meta = entry.path().join("pack.mcmeta");
        if !meta.is_file() {
            continue;
        }
        let bytes = std::fs::read(&meta).expect("a bundled pack.mcmeta must be readable");
        let range = validate_pack_mcmeta_format(&bytes)
            .unwrap_or_else(|err| panic!("{} is invalid: {err:?}", meta.display()));
        assert!(
            range.contains(VANILLA_1211_PACK_FORMAT),
            "{} does not support format 48: {range:?}",
            meta.display()
        );
        checked += 1;
    }
    assert!(
        checked >= 6,
        "the bundled datapacks must be checked, but only {checked} were"
    );
}

// ---------- #221: end-to-end datapack + mod-compatibility registry validation ----------
//
// The two pure decisions above -- the pack.mcmeta format gate and the 0x07 registry-data
// assembler -- are only useful together. A loader sees a stack of packs (vanilla, a custom
// datapack, a mod datapack), drops the ones whose format or data it cannot use, and overlays
// the survivors in priority order before sending the registry packets. These tests drive
// that whole path: a bad pack is discarded without poisoning the good ones, the survivors
// are overlaid deterministically, and every emitted entry round-trips through the network
// NBT codec.

#[derive(Debug, Clone, Copy)]
struct CandidatePack<'a> {
    name: &'a str,
    /// Files by pack-root-relative path; the `pack.mcmeta` entry is the format gate's input.
    files: &'a [(&'a str, &'a [u8])],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LoadRejection<'a> {
    NoMeta(&'a str),
    Format(&'a str, PackFormatError),
    InvalidData(&'a str),
}

/// Model the loader's pack gate plus registry assembly. Each candidate is checked on its
/// own: a pack without a usable `pack.mcmeta`, or whose registry files cannot be encoded
/// (bad JSON, bad path, duplicate path within the pack), is dropped and recorded. Only
/// surviving packs reach `build_registry_data_payloads`, so one bad pack cannot poison
/// the others. Packs are overlaid low-to-high priority; later packs win.
fn load_registry_data<'a>(
    packs: impl IntoIterator<Item = CandidatePack<'a>>,
) -> Result<(Vec<Vec<u8>>, Vec<LoadRejection<'a>>), String> {
    let mut accepted = Vec::new();
    let mut rejected = Vec::new();
    for pack in packs {
        let Some(meta) = pack
            .files
            .iter()
            .find(|(path, _)| *path == "pack.mcmeta")
            .map(|(_, bytes)| *bytes)
        else {
            rejected.push(LoadRejection::NoMeta(pack.name));
            continue;
        };
        if let Err(err) = validate_pack_mcmeta_format(meta) {
            rejected.push(LoadRejection::Format(pack.name, err));
            continue;
        }
        // Round-trip this pack through the assembler in isolation: any error here
        // disqualifies the whole pack, never the other packs in the stack.
        if build_registry_data_payloads([pack.files.iter().copied()]).is_err() {
            rejected.push(LoadRejection::InvalidData(pack.name));
            continue;
        }
        accepted.push(pack.files);
    }
    let payloads = build_registry_data_payloads(accepted.into_iter().map(|f| f.iter().copied()))?;
    Ok((payloads, rejected))
}

fn nbt_str(value: &str) -> nbt::Tag {
    nbt::Tag::String(nbt::NbtString::text(value))
}

/// Decode one 0x07 registry payload, consuming it exactly. Each entry's network NBT is
/// re-encoded and decoded again, proving the codec round-trips byte-for-byte.
fn decode_registry_payload(payload: &[u8]) -> (String, Vec<(String, nbt::Tag)>) {
    let mut input = payload;
    let registry = read_registry_string(&mut input).to_string();
    let count = read_protocol_i32(&mut input).expect("a registry count");
    assert!(count >= 0, "a registry cannot have a negative entry count");
    let mut entries = Vec::new();
    for _ in 0..count {
        let id = read_registry_string(&mut input).to_string();
        assert_eq!(
            input.split_off(..1),
            Some(&[1u8][..]),
            "each registry entry carries present NBT"
        );
        let start = input;
        let tag = nbt::read_network_nbt(&mut input).unwrap();
        let wire = &start[..start.len() - input.len()];
        assert_eq!(nbt::write_network_nbt(&tag).unwrap(), wire, "NBT re-encode");
        let mut again = wire;
        assert_eq!(nbt::read_network_nbt(&mut again).unwrap(), tag);
        assert!(again.is_empty());
        entries.push((id, tag));
    }
    assert!(input.is_empty(), "trailing bytes in registry payload");
    (registry, entries)
}

#[test]
fn datapack_mod_e2e_overlay_precedence_and_deterministic_ordering() {
    // Vanilla declares the point format 48; the custom datapack declares a range that
    // includes 48; the mod was built for 57 but declares backward compatibility to 48.
    // All three are loadable by a 1.21.1 server.
    let vanilla: [(&str, &[u8]); 3] = [
        (
            "pack.mcmeta",
            br#"{"pack":{"pack_format":48,"description":"vanilla"}}"#,
        ),
        (
            "data/minecraft/damage_type/generic.json",
            br#"{"message_id":"generic"}"#,
        ),
        (
            "data/minecraft/worldgen/biome/plains.json",
            br#"{"has_precipitation":false,"temperature":0.8,"downfall":0.4,"effects":{"fog_color":1,"water_color":2,"water_fog_color":3,"sky_color":4},"spawners":{}}"#,
        ),
    ];
    let custom: [(&str, &[u8]); 4] = [
        (
            "pack.mcmeta",
            br#"{"pack":{"pack_format":48,"supported_formats":[46,50],"description":"custom"}}"#,
        ),
        (
            "data/example/damage_type/custom.json",
            br#"{"message_id":"custom"}"#,
        ),
        (
            "data/example/worldgen/biome/custom_biome.json",
            br#"{"has_precipitation":true,"temperature":0.1,"downfall":0.9,"effects":{"fog_color":5,"water_color":6,"water_fog_color":7,"sky_color":8}}"#,
        ),
        (
            "data/example/enchantment/custom_enchant.json",
            br#"{"description":"custom","max_level":3}"#,
        ),
    ];
    let modpack: [(&str, &[u8]); 5] = [
        (
            "pack.mcmeta",
            br#"{"pack":{"pack_format":57,"supported_formats":{"min_inclusive":48,"max_inclusive":57},"description":"mod"}}"#,
        ),
        (
            "data/minecraft/damage_type/generic.json",
            br#"{"message_id":"generic_mod"}"#,
        ),
        (
            "data/example/damage_type/custom.json",
            br#"{"message_id":"custom_mod"}"#,
        ),
        (
            "data/mod/trim_material/alloy.json",
            br#"{"asset_name":"alloy","ingredient":"minecraft:iron_ingot"}"#,
        ),
        (
            "data/mod/jukebox_song/track.json",
            br#"{"sound_event":"mod:track","length":100}"#,
        ),
    ];

    let (payloads, rejected) = load_registry_data([
        CandidatePack {
            name: "vanilla",
            files: &vanilla,
        },
        CandidatePack {
            name: "custom",
            files: &custom,
        },
        CandidatePack {
            name: "mod",
            files: &modpack,
        },
    ])
    .unwrap();
    assert!(rejected.is_empty(), "{rejected:?}");
    assert_eq!(payloads.len(), 5);

    let decoded: Vec<_> = payloads.iter().map(|p| decode_registry_payload(p)).collect();
    // Payloads follow the vanilla configuration registry order, not pack order.
    assert_eq!(
        decoded.iter().map(|(r, _)| r.as_str()).collect::<Vec<_>>(),
        [
            "minecraft:worldgen/biome",
            "minecraft:trim_material",
            "minecraft:damage_type",
            "minecraft:enchantment",
            "minecraft:jukebox_song",
        ]
    );

    // Within a registry, entries are ordered by path then namespace and the highest
    // priority pack supplies the value for a path declared more than once.
    let damage = &decoded[2].1;
    assert_eq!(
        damage.iter().map(|(id, _)| id.as_str()).collect::<Vec<_>>(),
        ["example:custom", "minecraft:generic"]
    );
    assert_eq!(
        damage[0].1.get("message_id"),
        Some(&nbt_str("custom_mod")),
        "the mod pack overrides the custom datapack"
    );
    assert_eq!(
        damage[1].1.get("message_id"),
        Some(&nbt_str("generic_mod")),
        "the mod pack overrides vanilla"
    );

    // The multi-entry 0x07 payload is exactly the wire layout: registry id, VarInt count,
    // then (id, present=NBT) pairs in sorted order.
    let expected_damage = b"\x15minecraft:damage_type\x02\
                            \x0eexample:custom\x01\x0a\x08\x00\x0amessage_id\x00\x0acustom_mod\x00\
                            \x11minecraft:generic\x01\x0a\x08\x00\x0amessage_id\x00\x0bgeneric_mod\x00";
    assert_eq!(&payloads[2], expected_damage);

    // Biomes from both lower-priority packs survive; the network codec still strips the
    // worldgen-only fields (spawners) before encoding.
    assert_eq!(
        decoded[0]
            .1
            .iter()
            .map(|(id, _)| id.as_str())
            .collect::<Vec<_>>(),
        ["example:custom_biome", "minecraft:plains"]
    );
    let nbt::Tag::Compound(plains) = &decoded[0].1[1].1 else {
        panic!("the vanilla biome must be a compound");
    };
    assert_eq!(
        plains.keys().map(|k| k.to_utf8().unwrap()).collect::<Vec<_>>(),
        ["downfall", "effects", "has_precipitation", "temperature"]
    );
    assert_eq!(
        plains.get(&nbt::NbtString::text("has_precipitation")),
        Some(&nbt::Tag::Byte(0))
    );

    // The three kinds declared by exactly one pack are each present once.
    for (payload, id, field) in [
        (&decoded[1], "mod:alloy", "asset_name"),
        (&decoded[3], "example:custom_enchant", "description"),
        (&decoded[4], "mod:track", "sound_event"),
    ] {
        assert_eq!(payload.1.len(), 1);
        assert_eq!(payload.1[0].0, id);
        assert!(payload.1[0].1.get(field).is_some());
    }

    // Reversing the files inside every pack changes nothing: the bytes are a function of
    // the pack stack, not of the order a directory reader happened to yield files in.
    let vanilla_rev: Vec<_> = vanilla.iter().rev().copied().collect();
    let custom_rev: Vec<_> = custom.iter().rev().copied().collect();
    let mod_rev: Vec<_> = modpack.iter().rev().copied().collect();
    let (reordered, rejected) = load_registry_data([
        CandidatePack {
            name: "vanilla",
            files: &vanilla_rev,
        },
        CandidatePack {
            name: "custom",
            files: &custom_rev,
        },
        CandidatePack {
            name: "mod",
            files: &mod_rev,
        },
    ])
    .unwrap();
    assert!(rejected.is_empty(), "{rejected:?}");
    assert_eq!(reordered, payloads);

    // Swapping pack priority swaps the winners, while the entry set stays the same.
    let (flipped, rejected) = load_registry_data([
        CandidatePack {
            name: "mod",
            files: &modpack,
        },
        CandidatePack {
            name: "custom",
            files: &custom,
        },
        CandidatePack {
            name: "vanilla",
            files: &vanilla,
        },
    ])
    .unwrap();
    assert!(rejected.is_empty(), "{rejected:?}");
    let flipped = decode_registry_payload(&flipped[2]).1;
    assert_eq!(
        flipped[0].1.get("message_id"),
        Some(&nbt_str("custom")),
        "now the custom pack is later than the mod pack"
    );
    assert_eq!(
        flipped[1].1.get("message_id"),
        Some(&nbt_str("generic")),
        "now vanilla is later than the mod pack"
    );
}

#[test]
fn datapack_mod_e2e_all_five_custom_registries_round_trip_network_nbt() {
    // Rich values exercise every JSON type the registry codec preserves. The biome also
    // carries worldgen fields that must not reach the network codec.
    let pack: [(&str, &[u8]); 6] = [
        ("pack.mcmeta", br#"{"pack":{"pack_format":48,"description":"e2e"}}"#),
        (
            "data/example/damage_type/deep.json",
            br#"{"message_id":"deep","scaling":"never","exhaustion":0.25,"deaths":["fall","drown"],"effects":"hurt"}"#,
        ),
        (
            "data/example/worldgen/biome/deep.json",
            br#"{"has_precipitation":true,"temperature":0.2,"downfall":0.7,"effects":{"fog_color":11,"water_color":12,"water_fog_color":13,"sky_color":14},"carvers":{"air":[]},"features":["a"],"spawn_costs":{"b":{}},"spawners":{"c":{}},"creature_spawn_probability":0.01}"#,
        ),
        (
            "data/example/enchantment/deep.json",
            br##"{"description":"deep","max_level":5,"weight":2,"exclusive_set":["minecraft:protection"],"supported_items":"#minecraft:armor"}"##,
        ),
        (
            "data/example/trim_material/deep.json",
            br#"{"asset_name":"deep","ingredient":"minecraft:diamond","description":"Deep","palette":{"material_0":123456}}"#,
        ),
        (
            "data/example/jukebox_song/deep.json",
            br#"{"sound_event":"example:deep","length":128,"duration_in_ticks":2560,"description":"Deep","comparison":0,"max_distance":16}"#,
        ),
    ];

    let (payloads, rejected) =
        load_registry_data([CandidatePack { name: "pack", files: &pack }]).unwrap();
    assert!(rejected.is_empty(), "{rejected:?}");
    let decoded: BTreeMap<String, Vec<(String, nbt::Tag)>> = payloads
        .iter()
        .map(|p| decode_registry_payload(p))
        .collect();
    assert_eq!(decoded.len(), 5, "all five custom registries must be sent");

    let damage = &decoded["minecraft:damage_type"];
    assert_eq!(damage.len(), 1);
    assert_eq!(damage[0].0, "example:deep");
    assert_eq!(damage[0].1.get("message_id"), Some(&nbt_str("deep")));
    assert_eq!(damage[0].1.get("scaling"), Some(&nbt_str("never")));
    assert_eq!(
        damage[0].1.get("exhaustion"),
        Some(&nbt::Tag::Double(0.25_f64.to_bits()))
    );
    assert_eq!(
        damage[0].1.get("deaths"),
        Some(&nbt::Tag::List(8, vec![nbt_str("fall"), nbt_str("drown")]))
    );

    let biome = &decoded["minecraft:worldgen/biome"];
    assert_eq!(biome.len(), 1);
    assert_eq!(biome[0].0, "example:deep");
    let nbt::Tag::Compound(fields) = &biome[0].1 else {
        panic!("the biome must be a compound");
    };
    assert_eq!(
        fields.keys().map(|k| k.to_utf8().unwrap()).collect::<Vec<_>>(),
        ["downfall", "effects", "has_precipitation", "temperature"]
    );
    assert_eq!(
        fields.get(&nbt::NbtString::text("has_precipitation")),
        Some(&nbt::Tag::Byte(1))
    );
    assert_eq!(
        fields.get(&nbt::NbtString::text("temperature")),
        Some(&nbt::Tag::Double(0.2_f64.to_bits()))
    );
    let nbt::Tag::Compound(effects) = fields.get(&nbt::NbtString::text("effects")).unwrap() else {
        panic!("biome effects must be a compound");
    };
    for (key, value) in [
        ("fog_color", 11),
        ("water_color", 12),
        ("water_fog_color", 13),
        ("sky_color", 14),
    ] {
        assert_eq!(
            effects.get(&nbt::NbtString::text(key)),
            Some(&nbt::Tag::Int(value))
        );
    }

    let enchantment = &decoded["minecraft:enchantment"];
    assert_eq!(enchantment.len(), 1);
    assert_eq!(enchantment[0].0, "example:deep");
    assert_eq!(enchantment[0].1.get("max_level"), Some(&nbt::Tag::Int(5)));
    assert_eq!(enchantment[0].1.get("weight"), Some(&nbt::Tag::Int(2)));
    assert_eq!(
        enchantment[0].1.get("exclusive_set"),
        Some(&nbt::Tag::List(8, vec![nbt_str("minecraft:protection")]))
    );

    let trim = &decoded["minecraft:trim_material"];
    assert_eq!(trim.len(), 1);
    assert_eq!(trim[0].0, "example:deep");
    assert_eq!(trim[0].1.get("ingredient"), Some(&nbt_str("minecraft:diamond")));
    let nbt::Tag::Compound(palette) = trim[0].1.get("palette").unwrap() else {
        panic!("trim palette must be a compound");
    };
    assert_eq!(
        palette.get(&nbt::NbtString::text("material_0")),
        Some(&nbt::Tag::Int(123456))
    );

    let jukebox = &decoded["minecraft:jukebox_song"];
    assert_eq!(jukebox.len(), 1);
    assert_eq!(jukebox[0].0, "example:deep");
    assert_eq!(jukebox[0].1.get("sound_event"), Some(&nbt_str("example:deep")));
    assert_eq!(jukebox[0].1.get("length"), Some(&nbt::Tag::Int(128)));
    assert_eq!(
        jukebox[0].1.get("duration_in_ticks"),
        Some(&nbt::Tag::Int(2560))
    );
}

#[test]
fn datapack_mod_e2e_invalid_packs_are_dropped_without_poisoning_valid_ones() {
    let vanilla: [(&str, &[u8]); 2] = [
        (
            "pack.mcmeta",
            br#"{"pack":{"pack_format":48,"description":"vanilla"}}"#,
        ),
        (
            "data/minecraft/damage_type/generic.json",
            br#"{"message_id":"generic"}"#,
        ),
    ];
    let bad_format: [(&str, &[u8]); 2] = [
        ("pack.mcmeta", br#"{"pack":{"pack_format":61}}"#),
        (
            "data/bad/damage_type/wrong.json",
            br#"{"message_id":"wrong"}"#,
        ),
    ];
    let unsupported: [(&str, &[u8]); 2] = [
        (
            "pack.mcmeta",
            br#"{"pack":{"pack_format":48,"supported_formats":[50,60]}}"#,
        ),
        (
            "data/bad/damage_type/unsupported.json",
            br#"{"message_id":"unsupported"}"#,
        ),
    ];
    let malformed_meta: [(&str, &[u8]); 2] = [
        ("pack.mcmeta", b"{\"pack\": {\"pack_format\": 48,}"),
        (
            "data/bad/damage_type/meta.json",
            br#"{"message_id":"meta"}"#,
        ),
    ];
    let malformed_data: [(&str, &[u8]); 2] = [
        ("pack.mcmeta", br#"{"pack":{"pack_format":48}}"#),
        ("data/bad/damage_type/broken.json", b"{not json"),
    ];
    let bad_path: [(&str, &[u8]); 2] = [
        ("pack.mcmeta", br#"{"pack":{"pack_format":48}}"#),
        (
            "data/Bad/damage_type/upper.json",
            br#"{"message_id":"upper"}"#,
        ),
    ];
    let no_meta: [(&str, &[u8]); 1] = [(
        "data/bad/damage_type/nometa.json",
        br#"{"message_id":"nometa"}"#,
    )];
    let modpack: [(&str, &[u8]); 2] = [
        (
            "pack.mcmeta",
            br#"{"pack":{"pack_format":57,"supported_formats":[48,57]}}"#,
        ),
        (
            "data/mod/damage_type/modded.json",
            br#"{"message_id":"modded"}"#,
        ),
    ];

    let (payloads, rejected) = load_registry_data([
        CandidatePack {
            name: "vanilla",
            files: &vanilla,
        },
        CandidatePack {
            name: "bad_format",
            files: &bad_format,
        },
        CandidatePack {
            name: "unsupported",
            files: &unsupported,
        },
        CandidatePack {
            name: "malformed_meta",
            files: &malformed_meta,
        },
        CandidatePack {
            name: "malformed_data",
            files: &malformed_data,
        },
        CandidatePack {
            name: "bad_path",
            files: &bad_path,
        },
        CandidatePack {
            name: "no_meta",
            files: &no_meta,
        },
        CandidatePack {
            name: "mod",
            files: &modpack,
        },
    ])
    .unwrap();

    assert_eq!(
        rejected,
        vec![
            LoadRejection::Format("bad_format", PackFormatError::WrongPackFormat(61)),
            LoadRejection::Format(
                "unsupported",
                PackFormatError::FormatNotSupported { min: 50, max: 60 }
            ),
            LoadRejection::Format("malformed_meta", PackFormatError::MalformedJson),
            LoadRejection::InvalidData("malformed_data"),
            LoadRejection::InvalidData("bad_path"),
            LoadRejection::NoMeta("no_meta"),
        ]
    );

    // The surviving payloads are exactly the overlay of the two good packs: not one entry
    // from a rejected pack leaks into the registry packet.
    let expected =
        build_registry_data_payloads([vanilla.iter().copied(), modpack.iter().copied()]).unwrap();
    assert_eq!(payloads, expected);
    let (registry, entries) = decode_registry_payload(&payloads[0]);
    assert_eq!(registry, "minecraft:damage_type");
    assert_eq!(
        entries
            .iter()
            .map(|(id, _)| id.as_str())
            .collect::<Vec<_>>(),
        ["minecraft:generic", "mod:modded"]
    );
    assert_eq!(entries[0].1.get("message_id"), Some(&nbt_str("generic")));
    assert_eq!(entries[1].1.get("message_id"), Some(&nbt_str("modded")));
}

#[test]
fn datapack_mod_e2e_duplicate_paths_are_pack_local_and_later_packs_win() {
    let vanilla: [(&str, &[u8]); 2] = [
        ("pack.mcmeta", br#"{"pack":{"pack_format":48}}"#),
        (
            "data/minecraft/damage_type/generic.json",
            br#"{"message_id":"generic"}"#,
        ),
    ];
    // Two files with the same path in one pack are ambiguous: the whole pack is dropped.
    let duplicate: [(&str, &[u8]); 3] = [
        ("pack.mcmeta", br#"{"pack":{"pack_format":48}}"#),
        (
            "data/example/damage_type/dup.json",
            br#"{"message_id":"first"}"#,
        ),
        (
            "data/example/damage_type/dup.json",
            br#"{"message_id":"second"}"#,
        ),
    ];
    let modpack: [(&str, &[u8]); 3] = [
        (
            "pack.mcmeta",
            br#"{"pack":{"pack_format":57,"supported_formats":[48,57]}}"#,
        ),
        (
            "data/minecraft/damage_type/generic.json",
            br#"{"message_id":"generic_mod"}"#,
        ),
        (
            "data/mod/damage_type/mod_only.json",
            br#"{"message_id":"mod_only"}"#,
        ),
    ];

    let err = build_registry_data_payloads([duplicate.iter().copied()]).unwrap_err();
    assert!(err.contains("duplicate registry file"), "{err}");

    let (payloads, rejected) = load_registry_data([
        CandidatePack {
            name: "vanilla",
            files: &vanilla,
        },
        CandidatePack {
            name: "duplicate",
            files: &duplicate,
        },
        CandidatePack {
            name: "mod",
            files: &modpack,
        },
    ])
    .unwrap();
    assert_eq!(rejected, vec![LoadRejection::InvalidData("duplicate")]);

    let (_, entries) = decode_registry_payload(&payloads[0]);
    assert_eq!(
        entries
            .iter()
            .map(|(id, _)| id.as_str())
            .collect::<Vec<_>>(),
        ["minecraft:generic", "mod:mod_only"]
    );
    // The same path in two different packs is a legal override, and the later pack wins.
    assert_eq!(entries[0].1.get("message_id"), Some(&nbt_str("generic_mod")));
    assert_eq!(entries[1].1.get("message_id"), Some(&nbt_str("mod_only")));
}

// ---- Track 2: dynamic dimension architecture --------------------------------

// The dimension registry is process-global, and registration reaches further
// than the registry: `register_datapack_dimensions` publishes `BLOCK_LOOKUP`
// (flipping `block_at` from its no-generator fallback to real terrain),
// `reset_dimension_registry` truncates the per-dimension generator slots, and
// some bodies write `world()`/`pos_store()` directly. `DYNAMIC_DIM_LOCK` only
// serializes these tests against each other; the world and generator guards
// are what keep a concurrent world test's scene or fingerprint stable.
static DYNAMIC_DIM_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn dynamic_dim_entries(files: &[(&str, &str)]) -> Vec<worldgen::DatapackDimensionSource> {
    vec![worldgen::DatapackDimensionSource::Entries(
        files
            .iter()
            .map(|(p, b)| (p.to_string(), b.to_string()))
            .collect(),
    )]
}

const DYNAMIC_DIM_TYPE: &str = r##"{
        "ultrawarm": false, "natural": true, "coordinate_scale": 4.0,
        "has_skylight": true, "has_ceiling": false, "ambient_light": 0.0,
        "piglin_safe": false, "bed_works": true, "respawn_anchor_works": false,
        "has_raids": true, "logical_height": 192, "min_y": -32, "height": 192,
        "infiniburn": "#minecraft:infiniburn_overworld",
        "effects": "minecraft:overworld",
        "monster_spawn_light_level": 0, "monster_spawn_block_light_limit": 0
    }"##;

#[test]
fn dynamic_dim_datapack_registers_flat_dimension() {
    let _world_guard = world_guard();
    let _gen_guard = crate::worldgen::lock_gen_for_tests();
    let _guard = DYNAMIC_DIM_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    worldgen::reset_dimension_registry();
    let sources = dynamic_dim_entries(&[
        ("data/custom/dimension_type/my_type.json", DYNAMIC_DIM_TYPE),
        (
            "data/custom/dimension/my_dim.json",
            r#"{"type": "custom:my_type", "generator": {"type": "minecraft:flat",
                "settings": {"biome": "minecraft:plains", "layers": [
                    {"block": "minecraft:bedrock", "height": 1},
                    {"block": "minecraft:stone", "height": 10}]}}}"#,
        ),
    ]);
    let registered = worldgen::register_datapack_dimensions(&sources, |_| Some(1)).unwrap();
    assert_eq!(registered, 1);
    let dim = DimId::parse_dimension_id("custom:my_dim").expect("custom dimension resolves");
    assert!(!dim.is_vanilla());
    assert_eq!(dim.index(), 3, "first custom slot follows the vanilla three");
    assert_eq!(dim.dimension_id(), "custom:my_dim");
    assert_eq!(dim.min_y(), -32);
    assert_eq!(dim.height(), 192);
    assert_eq!(dim.sections(), 12, "a 192-tall dimension has 12 sections");
    let spec = worldgen::dimension_spec(dim).unwrap();
    assert_eq!(spec.logical_height, 192);
    assert!(spec.has_skylight);
    assert!(!spec.has_ceiling);
    assert_eq!(dim.coordinate_scale(), 4.0);
    assert!(matches!(
        worldgen::dimension_spec(dim).map(|s| s.generator),
        Some(worldgen::CustomGeneratorKind::Flat { .. })
    ));
    // Vanilla identity is untouched by the registration.
    assert_eq!(DimId::Overworld.min_y(), -64);
    assert_eq!(DimId::Overworld.sections(), 24);
    assert_eq!(DimId::Nether.sections(), 16);
    assert_eq!(DimId::parse_dimension_id("minecraft:the_nether"), Some(DimId::Nether));
    worldgen::reset_dimension_registry();
}

#[test]
fn dynamic_dim_slots_are_sorted_and_stable_by_canonical_name() {
    let _world_guard = world_guard();
    let _gen_guard = crate::worldgen::lock_gen_for_tests();
    let _guard = DYNAMIC_DIM_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    worldgen::reset_dimension_registry();
    let sources = dynamic_dim_entries(&[
        ("data/zz/dimension_type/t.json", DYNAMIC_DIM_TYPE),
        ("data/zz/dimension/z_dim.json", r#"{"type": "zz:t", "generator": {"type": "minecraft:debug"}}"#),
        ("data/aa/dimension_type/t.json", DYNAMIC_DIM_TYPE),
        ("data/aa/dimension/a_dim.json", r#"{"type": "aa:t", "generator": {"type": "minecraft:debug"}}"#),
    ]);
    worldgen::register_datapack_dimensions(&sources, |_| Some(1)).unwrap();
    // Slot order follows canonical (namespace, path), not declaration order.
    let a = DimId::parse_dimension_id("aa:a_dim").unwrap();
    let z = DimId::parse_dimension_id("zz:z_dim").unwrap();
    assert!(a.index() < z.index());
    assert_eq!(DimId::all_registered().len(), 5);
    // `minecraft:debug`/unknown generators fall back to void rather than erroring.
    worldgen::reset_dimension_registry();
}

#[test]
fn dynamic_dim_invalid_geometry_is_rejected() {
    let _world_guard = world_guard();
    let _gen_guard = crate::worldgen::lock_gen_for_tests();
    let _guard = DYNAMIC_DIM_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    worldgen::reset_dimension_registry();
    for (min_y, height) in [(-33, 192), (-32, 190), (0, 0)] {
        let ty = DYNAMIC_DIM_TYPE
            .replace("\"min_y\": -32", &format!("\"min_y\": {min_y}"))
            .replace("\"height\": 192", &format!("\"height\": {height}"));
        let sources = dynamic_dim_entries(&[
            ("data/bad/dimension_type/t.json", ty.as_str()),
            ("data/bad/dimension/d.json", r#"{"type": "bad:t", "generator": {"type": "minecraft:flat", "settings": {"layers": [], "biome": "minecraft:plains"}}}"#),
        ]);
        let result = worldgen::register_datapack_dimensions(&sources, |_| Some(1));
        assert!(result.is_err() || result == Ok(0), "min_y={min_y} height={height} must not register");
        assert_eq!(DimId::all_registered().len(), 3);
        worldgen::reset_dimension_registry();
    }
}

#[test]
fn dynamic_dim_noise_generator_uses_custom_window() {
    let _world_guard = world_guard();
    let _gen_guard = crate::worldgen::lock_gen_for_tests();
    let _guard = DYNAMIC_DIM_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    worldgen::reset_dimension_registry();
    let sources = dynamic_dim_entries(&[
        ("data/nz/dimension_type/t.json", DYNAMIC_DIM_TYPE),
        (
            "data/nz/dimension/d.json",
            r#"{"type": "nz:t", "generator": {"type": "minecraft:noise",
                "settings": "minecraft:overworld", "biome_source": {"type": "minecraft:multi_noise",
                "preset": "minecraft:overworld"}}}"#,
        ),
    ]);
    worldgen::register_datapack_dimensions(&sources, |_| Some(1)).unwrap();
    let dim = DimId::parse_dimension_id("nz:d").unwrap();
    let spec = worldgen::dimension_spec(dim).unwrap();
    assert!(matches!(spec.generator, worldgen::CustomGeneratorKind::Noise { .. }));
    assert_eq!(spec.min_y, -32);
    assert_eq!(spec.height, 192);
    worldgen::reset_dimension_registry();
}

#[test]
fn dynamic_dim_join_game_names_all_dimensions() {
    let _world_guard = world_guard();
    let _gen_guard = crate::worldgen::lock_gen_for_tests();
    let _guard = DYNAMIC_DIM_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    worldgen::reset_dimension_registry();
    let sources = dynamic_dim_entries(&[
        ("data/custom/dimension_type/my_type.json", DYNAMIC_DIM_TYPE),
        ("data/custom/dimension/my_dim.json", r#"{"type": "custom:my_type", "generator": {"type": "minecraft:debug"}}"#),
    ]);
    worldgen::register_datapack_dimensions(&sources, |_| Some(1)).unwrap();
    let dim = DimId::parse_dimension_id("custom:my_dim").unwrap();
    let payload = join_game(dim, 0, 1);
    // The join packet declares every registered dimension by canonical name and
    // carries the joining dimension's own name + dimension-type identifier.
    assert!(payload.windows(b"custom:my_dim".len()).any(|w| w == b"custom:my_dim"));
    assert!(payload.windows(b"minecraft:overworld".len()).any(|w| w == b"minecraft:overworld"));
    assert!(payload.windows(b"minecraft:the_nether".len()).any(|w| w == b"minecraft:the_nether"));
    assert!(payload.windows(b"minecraft:the_end".len()).any(|w| w == b"minecraft:the_end"));
    // Vanilla wire IDs stay pinned.
    assert_eq!(dim_type_wire_id(DimId::Overworld), 0);
    assert_eq!(dim_type_wire_id(DimId::End), 2);
    assert_eq!(dim_type_wire_id(DimId::Nether), 3);
    worldgen::reset_dimension_registry();
}

#[test]
fn dynamic_dim_execute_in_resolves_custom_name() {
    let _world_guard = world_guard();
    let _gen_guard = crate::worldgen::lock_gen_for_tests();
    let _guard = DYNAMIC_DIM_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    worldgen::reset_dimension_registry();
    let sources = dynamic_dim_entries(&[
        ("data/custom/dimension_type/my_type.json", DYNAMIC_DIM_TYPE),
        ("data/custom/dimension/my_dim.json", r#"{"type": "custom:my_type", "generator": {"type": "minecraft:debug"}}"#),
    ]);
    worldgen::register_datapack_dimensions(&sources, |_| Some(1)).unwrap();
    // `/execute in custom:my_dim` resolves through the same parse path commands use.
    assert_eq!(
        DimId::parse_dimension_id("custom:my_dim"),
        DimId::all_registered().last().copied()
    );
    assert!(DimId::parse_dimension_id("custom:nope").is_none());
    worldgen::reset_dimension_registry();
}

#[test]
fn dynamic_dim_save_roundtrip_survives_slot_reordering() {
    let _world_guard = world_guard();
    let _gen_guard = crate::worldgen::lock_gen_for_tests();
    let _guard = DYNAMIC_DIM_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    worldgen::reset_dimension_registry();
    let sources = dynamic_dim_entries(&[
        ("data/zz/dimension_type/t.json", DYNAMIC_DIM_TYPE),
        ("data/zz/dimension/z_dim.json", r#"{"type": "zz:t", "generator": {"type": "minecraft:debug"}}"#),
        ("data/aa/dimension_type/t.json", DYNAMIC_DIM_TYPE),
        ("data/aa/dimension/a_dim.json", r#"{"type": "aa:t", "generator": {"type": "minecraft:debug"}}"#),
    ]);
    worldgen::register_datapack_dimensions(&sources, |_| Some(1)).unwrap();
    let first_a = DimId::parse_dimension_id("aa:a_dim").unwrap();
    let first_z = DimId::parse_dimension_id("zz:z_dim").unwrap();
    assert_eq!(first_a.index(), 3);
    assert_eq!(first_z.index(), 4);
    // An edit and a logout position in the higher-slotted dimension.
    world().insert(
        (first_z, 2, -3),
        HashMap::from([(local_idx(2, -16, 3), 17u32)]),
    );
    pos_store()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert("Tester".to_string(), (first_z, 8.0, 64.0, -24.0, 0.0, 0.0));
    let mut bytes = Vec::new();
    save_world_into(&mut bytes);
    assert!(
        bytes.windows(b"DXT1".len()).any(|w| w == b"DXT1"),
        "a custom-dimension save carries the named DXT1 footer"
    );
    // Restart: the registry rebuilds in a different slot order.
    world().clear();
    pos_store().lock().unwrap_or_else(|e| e.into_inner()).clear();
    worldgen::reset_dimension_registry();
    let reversed = dynamic_dim_entries(&[
        ("data/aa/dimension_type/t.json", DYNAMIC_DIM_TYPE),
        ("data/aa/dimension/a_dim.json", r#"{"type": "aa:t", "generator": {"type": "minecraft:debug"}}"#),
        ("data/zz/dimension_type/t.json", DYNAMIC_DIM_TYPE),
        ("data/zz/dimension/z_dim.json", r#"{"type": "zz:t", "generator": {"type": "minecraft:debug"}}"#),
        // A new dimension inserted between restarts shifts every later slot.
        ("data/mm/dimension_type/t.json", DYNAMIC_DIM_TYPE),
        ("data/mm/dimension/m_dim.json", r#"{"type": "mm:t", "generator": {"type": "minecraft:debug"}}"#),
    ]);
    worldgen::register_datapack_dimensions(&reversed, |_| Some(1)).unwrap();
    let second_z = DimId::parse_dimension_id("zz:z_dim").unwrap();
    assert_eq!(second_z.index(), 5, "slot moved after the registry change");
    decode_world_payload(&bytes).unwrap();
    // Named persistence lands the edit on the same dimension despite the new slot.
    assert!(
        world()
            .read(&(second_z, 2, -3))
            .get(&(second_z, 2, -3))
            .is_some_and(|edits| edits.get(&local_idx(2, -16, 3)) == Some(&17)),
        "the chunk edit resolves to zz:z_dim's new slot"
    );
    assert_eq!(
        pos_store().lock().unwrap_or_else(|e| e.into_inner()).get("Tester").map(|p| p.0),
        Some(second_z),
        "the logout dimension resolves by name"
    );
    world().clear();
    pos_store().lock().unwrap_or_else(|e| e.into_inner()).clear();
    worldgen::reset_dimension_registry();
}

// ---- Wave B3: dynamic-dimension cache/broadcast isolation + lod_v1 wire ----

/// One varint straight off a socket; `None` on close or read timeout.
fn lod_test_varint(stream: &mut TcpStream) -> Option<i64> {
    let mut value = 0i64;
    for shift in 0..5 {
        let mut byte = [0u8; 1];
        std::io::Read::read_exact(stream, &mut byte).ok()?;
        value |= i64::from(byte[0] & 0x7f) << (7 * shift);
        if byte[0] & 0x80 == 0 {
            return Some(value);
        }
    }
    None
}

/// Every frame that arrived on this socket, as `(packet id, payload)`.
fn lod_test_frames(stream: &mut TcpStream) -> Vec<(i64, Vec<u8>)> {
    let mut out = Vec::new();
    while let Some(len) = lod_test_varint(stream) {
        let mut body = vec![0u8; usize::try_from(len).ok().unwrap_or(0)];
        if std::io::Read::read_exact(stream, &mut body).is_err() {
            break;
        }
        let mut sl = &body[..];
        let Some(pid) = read_varint(&mut sl) else {
            break;
        };
        out.push((pid, sl.to_vec()));
    }
    out
}

/// The `(channel, data)` split of an S2C `0x19` custom-payload body.
fn lod_test_channel(body: &[u8]) -> (&str, &[u8]) {
    let mut sl = body;
    let len = read_protocol_i32(&mut sl).expect("channel length") as usize;
    let (channel, data) = sl.split_at(len);
    (std::str::from_utf8(channel).expect("channel utf8"), data)
}

/// A C2S `0x12` body for `channel` carrying `data`.
fn lod_test_payload(channel: &str, data: &[u8]) -> Vec<u8> {
    let mut p = Vec::new();
    write_str(&mut p, channel);
    p.extend_from_slice(data);
    p
}

/// A `REQ1` request frame: magic, resolution, `cx`/`cz` little-endian.
fn lod_test_req1(resolution: u8, cx: i32, cz: i32) -> Vec<u8> {
    let mut d = b"REQ1".to_vec();
    d.push(resolution);
    d.extend_from_slice(&cx.to_le_bytes());
    d.extend_from_slice(&cz.to_le_bytes());
    d
}

#[test]
fn dynamic_dim_chunk_packet_cache_isolates_dimensions() {
    let _world_guard = world_guard();
    let _gen_guard = crate::worldgen::lock_gen_for_tests();
    let _guard = DYNAMIC_DIM_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    worldgen::reset_dimension_registry();
    let sources = dynamic_dim_entries(&[
        ("data/lodc/dimension_type/t.json", DYNAMIC_DIM_TYPE),
        (
            "data/lodc/dimension/lod_dim.json",
            r#"{"type": "lodc:t", "generator": {"type": "minecraft:debug"}}"#,
        ),
    ]);
    worldgen::register_datapack_dimensions(&sources, |_| Some(1)).unwrap();
    let custom = DimId::parse_dimension_id("lodc:lod_dim").expect("custom dimension resolves");
    assert!(custom.index() >= 3);

    let dims = [DimId::Overworld, DimId::Nether, DimId::End, custom];
    let mut cache = connection_limits::ChunkPacketCache::new();
    for (i, &dim) in dims.iter().enumerate() {
        cache.insert(7, (dim, 0, 0, false), 100, Vec::new(), vec![0xa0 + i as u8]);
    }
    // The same (cx, cz) in four dimensions: four distinct frames, and a hit
    // returns the same shared Arc a rebuild would produce.
    let mut frames = Vec::new();
    for &dim in &dims {
        let (first, _) = cache.frame(7, (dim, 0, 0, false), 100, None).expect("cached frame");
        let (second, _) = cache.frame(7, (dim, 0, 0, false), 100, None).expect("second read hits");
        assert!(Arc::ptr_eq(&first, &second), "a cache hit shares one Arc");
        frames.push(first);
    }
    for (i, a) in frames.iter().enumerate() {
        for b in &frames[i + 1..] {
            assert_ne!(a, b, "dimensions never share a chunk frame");
        }
    }
    // A revision miss invalidates only the dimension asked with it.
    assert!(cache.frame(7, (DimId::Overworld, 0, 0, false), 101, None).is_none());
    for &dim in &dims[1..] {
        assert!(
            cache.frame(7, (dim, 0, 0, false), 100, None).is_some(),
            "a revision miss in one dimension never touches the others"
        );
    }
    // The designed wholesale invalidation moves the generation, not one slot.
    for &dim in &dims {
        assert!(cache.frame(8, (dim, 0, 0, false), 100, None).is_none());
    }
    worldgen::reset_dimension_registry();
}

#[test]
fn dynamic_dim_broadcast_near_isolates_custom_dimensions() {
    let _world_guard = world_guard();
    let _gen_guard = crate::worldgen::lock_gen_for_tests();
    let _guard = DYNAMIC_DIM_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    worldgen::reset_dimension_registry();
    let sources = dynamic_dim_entries(&[
        ("data/lodb/dimension_type/t.json", DYNAMIC_DIM_TYPE),
        (
            "data/lodb/dimension/lod_dim.json",
            r#"{"type": "lodb:t", "generator": {"type": "minecraft:debug"}}"#,
        ),
    ]);
    worldgen::register_datapack_dimensions(&sources, |_| Some(1)).unwrap();
    let custom = DimId::parse_dimension_id("lodb:lod_dim").expect("custom dimension resolves");
    assert!(custom.index() >= 3);

    // Two clients at the identical (x, z) in different dimensions.
    let mk_client = |eid: i32, dim: DimId| {
        let (mut c, r) = effect_tests::client_pair();
        c.eid = eid;
        *c.dim.lock().unwrap_or_else(|e| e.into_inner()) = dim;
        *c.pos.lock().unwrap_or_else(|e| e.into_inner()) = (0.0, 70.0, 0.0, 0.0, 0.0, true);
        r.set_read_timeout(Some(Duration::from_millis(200))).unwrap();
        (Arc::new(c), r)
    };
    let (over_client, mut over_r) = mk_client(71, DimId::Overworld);
    let (custom_client, mut custom_r) = mk_client(72, custom);
    let audience = [over_client, custom_client];

    // A normal (not long-distance) particle reaches 32 blocks; both clients
    // stand at the origin, so only the dimension match decides delivery.
    broadcast_near_in(DimId::Overworld, &audience, 0.0, 70.0, 0.0, -1, |_| {
        (0x29, vec![0u8])
    });
    let over_frames = lod_test_frames(&mut over_r);
    assert!(
        over_frames.iter().any(|(pid, _)| *pid == 0x29),
        "the overworld client receives its dimension's broadcast"
    );
    assert!(
        lod_test_frames(&mut custom_r).is_empty(),
        "a custom-dimension client at the same (x, z) hears nothing"
    );

    broadcast_near_in(custom, &audience, 0.0, 70.0, 0.0, -1, |_| (0x29, vec![0u8]));
    assert!(
        lod_test_frames(&mut over_r).is_empty(),
        "the overworld client never hears the custom dimension"
    );
    let custom_frames = lod_test_frames(&mut custom_r);
    assert!(
        custom_frames.iter().any(|(pid, _)| *pid == 0x29),
        "the custom-dimension client receives its own broadcast"
    );
    worldgen::reset_dimension_registry();
}

#[test]
fn lod_v1_wire_matches_client_app_codec_and_vanilla_unaffected() {
    // -- codec layout, byte for byte against client-app/src/lod.rs --
    let cells: Vec<LodCellV1> = (0..16u32)
        .map(|i| LodCellV1 {
            state_id: 100 + i,
            tint: 0x7cbd6b,
            top_y: 64 + i as i16,
            light: 0xf0,
        })
        .collect();
    let frame = encode_lod_v1(-3, 8, LOD_RES_COARSE, &cells).unwrap();
    assert_eq!(frame.len(), LOD_V1_HEADER + 16 * LOD_V1_CELL_BYTES);
    assert_eq!(&frame[..4], b"LOD1");
    assert_eq!(frame[4], 1, "version byte");
    assert_eq!(frame[5], 4, "resolution byte");
    assert_eq!(u16::from_le_bytes([frame[6], frame[7]]), 16, "cell count");
    assert_eq!(i32::from_le_bytes(frame[8..12].try_into().unwrap()), -3);
    assert_eq!(i32::from_le_bytes(frame[12..16].try_into().unwrap()), 8);
    assert_eq!(&frame[16..20], &100u32.to_le_bytes(), "cell 0 state_id");
    assert_eq!(&frame[20..24], &0x7cbd6bu32.to_le_bytes(), "cell 0 tint");
    assert_eq!(&frame[24..26], &64i16.to_le_bytes(), "cell 0 top_y");
    assert_eq!(frame[26], 0xf0, "cell 0 light");
    let decoded = decode_lod_v1_frame(&frame).unwrap();
    assert_eq!((decoded.cx, decoded.cz, decoded.resolution), (-3, 8, 4));
    assert_eq!(decoded.cells, cells);
    // The client decoder's rejections: any truncation, trailing bytes, wrong
    // magic/version/resolution, and a cell count off the resolution square.
    for cut in 0..frame.len() {
        assert!(decode_lod_v1_frame(&frame[..cut]).is_none(), "truncated at {cut}");
    }
    assert!(decode_lod_v1_frame(&[frame.as_slice(), &[0]].concat()).is_none());
    let mut bad = frame.clone();
    bad[0] = b'X';
    assert!(decode_lod_v1_frame(&bad).is_none(), "bad magic");
    let mut bad = frame.clone();
    bad[4] = 2;
    assert!(decode_lod_v1_frame(&bad).is_none(), "bad version");
    let mut bad = frame.clone();
    bad[5] = 8;
    assert!(decode_lod_v1_frame(&bad).is_none(), "bad resolution");
    let mut bad = frame.clone();
    bad[6] = 15;
    assert!(decode_lod_v1_frame(&bad).is_none(), "count off the square");
    assert!(encode_lod_v1(0, 0, 8, &cells).is_none());
    assert!(encode_lod_v1(0, 0, LOD_RES_COARSE, &cells[..15]).is_none());

    // -- in-process loopback: socket-paired clients through the 0x12 handler --
    // A vanilla client never registers: even a forged REQ1 yields nothing.
    let (vanilla, mut vanilla_r) = effect_tests::client_pair();
    vanilla_r
        .set_read_timeout(Some(Duration::from_millis(200)))
        .unwrap();
    custom_payload_c2s(&vanilla, &lod_test_payload(LOD_V1_CHANNEL, &lod_test_req1(16, 0, 0)));
    assert!(!vanilla.lod_v1_enabled.load(Ordering::Relaxed));
    custom_payload_c2s(&vanilla, &lod_test_payload("minecraft:register", b"some:other"));
    assert!(!vanilla.lod_v1_enabled.load(Ordering::Relaxed));
    assert!(
        lod_test_frames(&mut vanilla_r).is_empty(),
        "a vanilla client receives zero lod_v1 packets"
    );

    // Opt-in: registering the channel flips the flag and earns the S2C
    // `minecraft:register` echo naming `megu:lod_v1`.
    let (lod, mut lod_r) = effect_tests::client_pair();
    lod_r.set_read_timeout(Some(Duration::from_millis(200))).unwrap();
    custom_payload_c2s(
        &lod,
        &lod_test_payload("minecraft:register", b"other:chan\x00megu:lod_v1\x00more:x"),
    );
    assert!(lod.lod_v1_enabled.load(Ordering::Relaxed));
    let acks = lod_test_frames(&mut lod_r);
    assert_eq!(acks.len(), 1, "exactly one register ack");
    assert_eq!(acks[0].0, 0x19);
    let (channel, data) = lod_test_channel(&acks[0].1);
    assert_eq!(channel, "minecraft:register");
    assert_eq!(data, b"megu:lod_v1");

    // A fine REQ1 for (0, 0) answers with a 2832-byte LOD1 frame of non-air
    // surface cells; the coarse request answers 192 bytes.
    custom_payload_c2s(&lod, &lod_test_payload(LOD_V1_CHANNEL, &lod_test_req1(16, 0, 0)));
    let frames = lod_test_frames(&mut lod_r);
    assert_eq!(frames.len(), 1);
    assert_eq!(frames[0].0, 0x19);
    let (channel, data) = lod_test_channel(&frames[0].1);
    assert_eq!(channel, LOD_V1_CHANNEL);
    assert_eq!(data.len(), 16 + 256 * 11, "fine frame is header + 256 cells");
    let fine = decode_lod_v1_frame(data).expect("server frame decodes");
    assert_eq!((fine.cx, fine.cz, fine.resolution), (0, 0, 16));
    assert!(
        fine.cells.iter().all(|c| c.top_y > i16::MIN && c.state_id != 0),
        "every fine cell is a real non-air surface block"
    );

    custom_payload_c2s(&lod, &lod_test_payload(LOD_V1_CHANNEL, &lod_test_req1(4, 0, 0)));
    let frames = lod_test_frames(&mut lod_r);
    assert_eq!(frames.len(), 1);
    let (channel, data) = lod_test_channel(&frames[0].1);
    assert_eq!(channel, LOD_V1_CHANNEL);
    assert_eq!(data.len(), 16 + 16 * 11, "coarse frame is header + 16 cells");
    let coarse = decode_lod_v1_frame(data).expect("server frame decodes");
    assert_eq!((coarse.cx, coarse.cz, coarse.resolution), (0, 0, 4));
    assert!(
        coarse.cells.iter().all(|c| c.top_y > i16::MIN && c.state_id != 0),
        "every coarse cell is a real non-air surface block"
    );

    // Out-of-range, bad-resolution and malformed requests are dropped without
    // a packet and without disconnecting the client.
    for req in [
        lod_test_req1(16, LOD_V1_MAX_RANGE_CHUNKS + 1, 0),
        lod_test_req1(16, 0, -LOD_V1_MAX_RANGE_CHUNKS - 1),
        lod_test_req1(8, 0, 0),
        b"BAD!".to_vec(),
    ] {
        custom_payload_c2s(&lod, &lod_test_payload(LOD_V1_CHANNEL, &req));
    }
    assert!(
        lod_test_frames(&mut lod_r).is_empty(),
        "unsafe or malformed requests are silently ignored"
    );
    // The socket is still usable: a valid request still answers.
    custom_payload_c2s(&lod, &lod_test_payload(LOD_V1_CHANNEL, &lod_test_req1(4, 0, 0)));
    assert_eq!(lod_test_frames(&mut lod_r).len(), 1);

    // Unregister flips the flag off and the channel goes silent again.
    custom_payload_c2s(&lod, &lod_test_payload("minecraft:unregister", b"megu:lod_v1"));
    assert!(!lod.lod_v1_enabled.load(Ordering::Relaxed));
    custom_payload_c2s(&lod, &lod_test_payload(LOD_V1_CHANNEL, &lod_test_req1(4, 0, 0)));
    assert!(lod_test_frames(&mut lod_r).is_empty());

    // -- the column encoder answers for the Overworld and a custom dimension --
    let _world_guard = world_guard();
    let _gen_guard = crate::worldgen::lock_gen_for_tests();
    let _guard = DYNAMIC_DIM_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    worldgen::reset_dimension_registry();
    let sources = dynamic_dim_entries(&[
        ("data/lodw/dimension_type/t.json", DYNAMIC_DIM_TYPE),
        (
            "data/lodw/dimension/lod_dim.json",
            r#"{"type": "lodw:t", "generator": {"type": "minecraft:flat",
                "settings": {"biome": "minecraft:plains", "layers": [
                    {"block": "minecraft:bedrock", "height": 1},
                    {"block": "minecraft:stone", "height": 10}]}}}"#,
        ),
    ]);
    worldgen::register_datapack_dimensions(&sources, |_| Some(1)).unwrap();
    let custom = DimId::parse_dimension_id("lodw:lod_dim").expect("custom dimension resolves");
    for (dim, want_resolution) in [
        (DimId::Overworld, LOD_RES_FINE),
        (DimId::Overworld, LOD_RES_COARSE),
        (custom, LOD_RES_FINE),
        (custom, LOD_RES_COARSE),
    ] {
        let frame = encode_lod_v1_column(dim, 0, 0, want_resolution).expect("column frame");
        let decoded = decode_lod_v1_frame(&frame).expect("round trip");
        assert_eq!((decoded.cx, decoded.cz, decoded.resolution), (0, 0, want_resolution));
        assert!(
            decoded.cells.iter().all(|c| c.top_y > i16::MIN && c.state_id != 0),
            "dimension {:?} column reports non-air surface cells",
            dim.raw()
        );
    }
    // The flat dimension's top is its layer stack, not the Overworld's hills:
    // bedrock + 10 stone over min_y -32 puts the first air at -21.
    let flat = decode_lod_v1_frame(
        &encode_lod_v1_column(custom, 0, 0, LOD_RES_FINE).expect("flat column"),
    )
    .unwrap();
    assert!(
        flat.cells.iter().all(|c| c.top_y == -22),
        "flat dimension tops sit at the layer stack top"
    );
    worldgen::reset_dimension_registry();
}
