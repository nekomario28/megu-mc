use super::*;
use crate::block_registry::breg;

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
        opacity.iter().any(|&v| v == 15) && opacity.iter().any(|&v| v == 1),
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
    let chunks = world().lock().unwrap().len();
    let floors = sky_floors_cache().lock().unwrap().len();
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
        world().lock().unwrap().len()
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

    let dense = sky_spill(0, 0, &floor_of, &opacity);
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
    sky_spill_apply(0, 0, &floor_of, &opacity, &mut applied);
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
            sky_spill_apply(0, 0, &floor_of, &opacity, &mut out);
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
        let copied: Vec<HashMap<u32, u32>> = edited.iter().map(|c| c.clone()).collect();
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
