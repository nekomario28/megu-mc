use super::*;
use crate::feature::breg;

/// The block/item knowledge is spread across several hand-maintained tables, and
/// the gate the registry slice needs is also the tool that says whether they agree
/// today.
///
/// The first version of this test assumed `state_to_item` and `item_to_state` were
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
        let Some(st) = item_to_state(item) else { continue };
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
        let Some(item) = state_to_item(st) else { continue };
        with_drop += 1;
        if !(0..4096).contains(&item) {
            problems.push(format!("state {st} drops item {item}, outside the item space"));
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
    assert_eq!(
        hidden_states().len(),
        HIDDEN_BLOCK_NAMES.len(),
        "a name was dropped while building the hidden set"
    );
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
        "obsidian", "ancient_debris", "crying_obsidian", "deepslate", "iron_block",
        "diamond_block", "gold_block", "emerald_block", "netherite_block", "stone",
        "cobblestone", "end_stone", "netherrack", "spawner", "enchanting_table",
        "anvil", "cauldron", "hopper", "furnace", "chest",
    ];
    let flagged: Vec<&str> = watch
        .iter()
        .copied()
        .filter(|w| defaulted.contains(w))
        .collect();
    println!("DEFAULT_HARDNESS {} blocks default; flagged={flagged:?}", defaulted.len());
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
    let md = crate::feature::jparse_pub(&raw)
        .expect("the asset must parse")
        .into_arr()
        .expect("the asset is an array")
        .clone();
    let mut checked = 0usize;
    for b in &md {
        let lo = crate::feature::jnum_pub(b, "minStateId").expect("minStateId") as u32;
        let hi = crate::feature::jnum_pub(b, "maxStateId").expect("maxStateId") as u32;
        let h = crate::feature::jnum_pub(b, "hardness").unwrap_or(0.6) as f32;
        let name = crate::feature::jstr_pub(b, "name").unwrap_or_else(|| "?".into());
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
    let md = crate::feature::jparse_pub(&raw)
        .expect("the asset must parse")
        .into_arr()
        .expect("the asset is an array")
        .clone();
    let mut checked = 0usize;
    let mut needs_tool = 0usize;
    for b in &md {
        // Every tool id in the asset, so both the accepted and rejected cases run.
        let allowed: Vec<i64> = match b.get("harvestTools") {
            Some(crate::worldgen::J::Obj(entries)) => {
                entries.iter().filter_map(|(k, _)| k.parse::<i64>().ok()).collect()
            }
            _ => Vec::new(),
        };
        if !allowed.is_empty() {
            needs_tool += 1;
        }
        let lo = crate::feature::jnum_pub(b, "minStateId").expect("minStateId") as u32;
        let hi = crate::feature::jnum_pub(b, "maxStateId").expect("maxStateId") as u32;
        let name = crate::feature::jstr_pub(b, "name").unwrap_or_else(|| "?".into());
        for st in lo..=hi {
            if (st as usize) >= reg.state_count() {
                continue;
            }
            for item in [818i64, 819, 820, 825, 830, 835, 840, 845, 850] {
                let expected = allowed.is_empty() || allowed.contains(&item);
                assert_eq!(can_harvest(st as i64, item), expected, "state {st} of {name}, item {item}");
                checked += 1;
            }
        }
    }
    assert!(needs_tool > 100, "the asset must have tool-requiring blocks");
    assert!(checked > 100_000, "the check must cover the states, covered {checked}");
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
        assert_eq!(can_harvest(st as i64, item), expect, "{name} with item {item}");
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
    let md = crate::feature::jparse_pub(&raw)
        .expect("the asset must parse")
        .into_arr()
        .expect("the asset is an array")
        .clone();
    let mut checked = 0usize;
    let mut with_kind = 0usize;
    for b in &md {
        let material = b.get("material").map(|m| m.str().to_string()).unwrap_or_default();
        let from_material = material.split(';').find_map(|part| match part {
            "mineable/shovel" => Some(1i8),
            "mineable/pickaxe" => Some(2),
            "mineable/axe" => Some(3),
            "mineable/hoe" => Some(4),
            "mineable/sword" => Some(0),
            _ => None,
        });
        let from_tools = match b.get("harvestTools") {
            Some(crate::worldgen::J::Obj(entries)) => {
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
                if disagree { None } else { k }
            }
            _ => None,
        };
        let want = from_material.or(from_tools).unwrap_or(-1);
        if want >= 0 {
            with_kind += 1;
        }
        let lo = crate::feature::jnum_pub(b, "minStateId").expect("minStateId") as u32;
        let hi = crate::feature::jnum_pub(b, "maxStateId").expect("maxStateId") as u32;
        let name = crate::feature::jstr_pub(b, "name").unwrap_or_else(|| "?".into());
        for st in lo..=hi {
            if (st as usize) >= reg.state_count() {
                continue;
            }
            assert_eq!(reg.tool_kind_of(st), want, "state {st} of {name}");
            checked += 1;
        }
    }
    assert!(with_kind > 300, "the asset must name a tool for most blocks");
    assert!(checked > 20_000, "the check must cover the state space, covered {checked}");
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
