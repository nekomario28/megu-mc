use super::*;
use crate::block_registry::breg;

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

/// Luminance and opacity are per-state fields in pblocks.json. That file is a different
/// Minecraft version from blocks_1211.json - it has pale_oak_planks - so its state ids are
/// its own numbering and cannot be used as the global state. The registry pairs the two by
/// block name and state position, which is what this checks.
#[test]
fn light_values_come_from_the_asset_for_every_state() {
    let Some(reg) = breg() else {
        panic!("the block registry must load from assets/");
    };
    let raw = std::fs::read("assets/pblocks.json").expect("pblocks must be readable");
    let blocks = crate::json::jparse(&raw)
        .expect("pblocks must parse")
        .get("blocks")
        .expect("pblocks has blocks")
        .as_arr()
        .expect("blocks is an array")
        .clone();
    use std::collections::HashMap;
    let mut by_name: HashMap<String, &crate::json::J> = HashMap::new();
    for b in &blocks {
        if let Some(n) = crate::json::jstr(b, "name") {
            by_name.insert(n, b);
        }
    }

    let raw_md = std::fs::read("assets/blocks_1211.json").expect("blocks_1211 must be readable");
    let md = crate::json::jparse(&raw_md)
        .expect("blocks_1211 must parse")
        .as_arr()
        .expect("blocks_1211 is an array")
        .clone();
    let mut checked = 0usize;
    let mut emitters = 0usize;
    let mut no_states = Vec::new();
    for b in &md {
        let name = crate::json::jstr(b, "name").unwrap_or_default();
        let lo = crate::json::jnum(b, "minStateId").expect("minStateId") as u32;
        let hi = crate::json::jnum(b, "maxStateId").expect("maxStateId") as u32;
        let n = (hi - lo + 1) as usize;
        let Some(pb) = by_name.get(&name) else {
            continue;
        };
        let Some(states) = pb.get("states").and_then(|s| s.as_arr()) else {
            continue;
        };
        if states.len() != n {
            // Positional pairing is impossible for these; they are counted so the hole is
            // visible rather than silent.
            no_states.push(name);
            continue;
        }
        for (i, s) in states.iter().enumerate() {
            let st = lo + i as u32;
            if (st as usize) >= reg.state_count() {
                continue;
            }
            let lum = crate::json::jnum(s, "luminance").unwrap_or(0.0) as u8;
            let op = crate::json::jnum(s, "opacity").unwrap_or(0.0) as u8;
            assert_eq!(reg.luminance_of(st), lum, "luminance of {name} state {st}");
            assert_eq!(reg.opacity_of(st), op, "opacity of {name} state {st}");
            if lum > 0 {
                emitters += 1;
            }
            checked += 1;
        }
    }
    assert!(
        checked > 20_000,
        "the check must cover the state space, covered {checked}"
    );
    assert!(
        emitters > 100,
        "the asset must have light sources, found {emitters}"
    );
    // The two assets are different versions, so a handful of blocks cannot be paired.
    // If this grows, the pairing assumption has broken.
    assert!(
        no_states.len() <= 4,
        "too many blocks cannot be paired by position: {no_states:?}"
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
