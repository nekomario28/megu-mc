// parse the [k=v,...] suffix of a /give item argument; supported subset:
// enchantments={a:lvl,...}, damage, max_damage, unbreakable, custom_name,
// lore, max_stack_size, repair_cost. Unknown/unsupported keys are ignored
// (bounded: no silent half-support claims).
fn parse_item_comps(arg: &str) -> Comps {
    let Some(lb) = arg.find('[') else {
        return Vec::new();
    };
    let Some(rb) = arg.rfind(']') else {
        return Vec::new();
    };
    if rb <= lb {
        return Vec::new();
    }
    let mut out: Comps = Vec::new();
    // split on top-level commas only — {}, [], '' nesting must not break
    let body = &arg[lb + 1..rb];
    let mut kvs: Vec<String> = Vec::new();
    {
        let mut depth = 0i32;
        let mut q = '\0';
        let mut cur = String::new();
        for ch in body.chars() {
            if q != '\0' {
                cur.push(ch);
                if ch == q {
                    q = '\0';
                }
                continue;
            }
            match ch {
                '\'' | '"' => {
                    q = ch;
                    cur.push(ch)
                }
                '{' | '[' | '(' => {
                    depth += 1;
                    cur.push(ch)
                }
                '}' | ']' | ')' => {
                    depth -= 1;
                    cur.push(ch)
                }
                ',' if depth == 0 => {
                    kvs.push(std::mem::take(&mut cur));
                }
                _ => cur.push(ch),
            }
        }
        if !cur.is_empty() {
            kvs.push(cur);
        }
    }
    for kv in &kvs {
        let kv = kv.trim();
        let (k, v) = match kv.split_once('=') {
            Some(t) => t,
            None => (kv, ""),
        };
        let kn = k.trim().trim_start_matches("minecraft:");
        match kn {
            "enchantments" => {
                // {name:lvl,...} — vanilla 1.21.1 map form
                let inner = v.trim().trim_start_matches('{').trim_end_matches('}');
                let mut list: Vec<(i64, i64)> = enchantments_get(&out);
                for e in inner.split(',') {
                    if let Some((en, lv)) = e.rsplit_once(':') {
                        let en = en.trim().trim_matches('"').trim_matches('\'');
                        if let (Some(id), Ok(l)) = (enchant_id(en), lv.trim().parse::<i64>()) {
                            list.retain(|(x, _)| *x != id);
                            list.push((id, l.max(1)));
                        }
                    }
                }
                comp_set(&mut out, COMP_ENCHANTMENTS, enchantments_data(&list, true));
            }
            "damage" => {
                if let Ok(n) = v.parse() {
                    comp_set_varint(&mut out, COMP_DAMAGE, n)
                }
            }
            "max_damage" => {
                if let Ok(n) = v.parse() {
                    comp_set_varint(&mut out, COMP_MAX_DAMAGE, n)
                }
            }
            "max_stack_size" => {
                if let Ok(n) = v.parse() {
                    comp_set_varint(&mut out, 1, n)
                }
            }
            "repair_cost" => {
                if let Ok(n) = v.parse() {
                    comp_set_varint(&mut out, 16, n)
                }
            }
            "unbreakable" => comp_set(&mut out, COMP_UNBREAKABLE, vec![1u8]),
            "custom_name" => {
                let t = v.trim().trim_matches('\'').trim_matches('"');
                comp_set(&mut out, COMP_CUSTOM_NAME, nbt_string(t));
            }
            "potion_contents" => {
                let potion = v.trim().trim_matches(['\'', '"']);
                let potion = potion.strip_prefix("minecraft:").unwrap_or(potion);
                if let Some(id) = LIVE_POTIONS.iter().position(|&(name, _)| name == potion) {
                    let mut data = vec![1];
                    write_varint(&mut data, id as i64);
                    data.extend([0, 0]); // no custom color or custom effects
                    comp_set(&mut out, COMP_POTION_CONTENTS, data);
                }
            }
            "lore" => {
                // ['a','b',...] — list of anonymous-NBT string elements
                let inner = v.trim().trim_start_matches('[').trim_end_matches(']');
                let mut d = Vec::new();
                let parts: Vec<String> = inner
                    .split(',')
                    .map(|x| x.trim().trim_matches('\'').trim_matches('"').to_string())
                    .filter(|x| !x.is_empty())
                    .collect();
                write_varint(&mut d, parts.len() as i64);
                for t in parts {
                    d.extend_from_slice(&nbt_string(&t));
                }
                comp_set(&mut out, COMP_LORE, d);
            }
            _ => {}
        }
    }
    out
}

// /schedule: pending (function, due server-tick) — vanilla keeps
// scheduled functions in world data; persisted in the meta tail.
fn sched_fns() -> &'static Mutex<Vec<(String, u64)>> {
    static S: OnceLock<Mutex<Vec<(String, u64)>>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(Vec::new()))
}
// entity tags for /tag + selector tag=: keyed "p:<playername>" for
// players (persisted by name) and "e:<eid>" for mobs (persisted by
// mob ordinal, same scheme as sheared — eids are re-issued on load).
fn tags() -> &'static Mutex<HashMap<String, HashSet<String>>> {
    static T: OnceLock<Mutex<HashMap<String, HashSet<String>>>> = OnceLock::new();
    T.get_or_init(|| Mutex::new(HashMap::new()))
}

static FORCELOADED: OnceLock<Mutex<HashSet<(i32, i32)>>> = OnceLock::new();
fn forceloaded() -> &'static Mutex<HashSet<(i32, i32)>> {
    FORCELOADED.get_or_init(|| Mutex::new(HashSet::new()))
}

// ---------- name -> id tables (generated from minecraft-data 1.21.1) ----------

fn state_from_arg(arg: &str) -> Option<u32> {
    #[cfg(feature = "neoforge-events")]
    if let Some(state) = neoforge_block_state(arg) {
        return Some(state);
    }
    let (name, props) = match arg.split_once('[') {
        Some((n, rest)) => (
            n.trim_start_matches("minecraft:").to_string(),
            rest.trim_end_matches(']').to_string(),
        ),
        None => return name_to_state(arg),
    };
    let name: &str = &name;
    let base = name_to_state(name)?;
    if breg().is_some_and(|reg| redstone_component(reg, base)) {
        return modification_block_state(arg);
    }
    let get = |k: &str| -> Option<String> {
        props.split(',').find_map(|kv| {
            kv.trim()
                .strip_prefix(k)
                .and_then(|v| v.strip_prefix('=').map(|s| s.to_string()))
        })
    };
    if name == "piston" || name == "sticky_piston" {
        let lo = if name == "piston" { 2011u32 } else { 1992u32 };
        let fi = get("facing").map(|f| facing_idx(&f)).unwrap_or(0);
        let ext = match get("extended").as_deref() {
            Some("true") => 0,
            _ => 6,
        };
        return Some(lo + ext + fi);
    }
    if name == "piston_head" {
        let fi = get("facing").map(|f| facing_idx(&f)).unwrap_or(0);
        let ty = match get("type").as_deref() {
            Some("sticky") => 1,
            _ => 0,
        };
        return Some(2023 + fi * 4 + ty * 2);
    }
    if name == "wheat" {
        return Some(
            4278 + get("age")
                .and_then(|v| v.parse::<u32>().ok())
                .unwrap_or(0)
                .min(7),
        );
    }
    if door_base(base).is_some() {
        let mut st = base;
        if let Some(v) = get("open") {
            st = if v == "true" { st & !2 } else { st | 2 };
        }
        if let Some(v) = get("half") {
            st = if v == "upper" { st | 8 } else { st & !8 };
        }
        return Some(st);
    }
    if breg().is_some_and(|reg| reg.name_of(base).ends_with("_trapdoor")) {
        let mut st = base;
        if let Some(v) = get("open") {
            st = breg()?.with_prop(st, "open", &v);
        }
        if let Some(v) = get("facing") {
            st = breg()?.with_prop(st, "facing", &v);
        }
        if let Some(v) = get("half") {
            st = breg()?.with_prop(st, "half", &v);
        }
        if let Some(v) = get("waterlogged") {
            st = breg()?.with_prop(st, "waterlogged", &v);
        }
        return Some(st);
    }
    Some(base)
}
pub(crate) fn name_to_state(n: &str) -> Option<u32> {
    let n = n.strip_prefix("minecraft:").unwrap_or(n);
    Some(match n {
        "air" => 0,
        "stone" => 1,
        "granite" => 2,
        "polished_granite" => 3,
        "diorite" => 4,
        "polished_diorite" => 5,
        "andesite" => 6,
        "polished_andesite" => 7,
        "grass_block" => 9,
        "dirt" => 10,
        "coarse_dirt" => 11,
        "podzol" => 13,
        "cobblestone" => 14,
        "oak_planks" => 15,
        "spruce_planks" => 16,
        "birch_planks" => 17,
        "jungle_planks" => 18,
        "acacia_planks" => 19,
        "cherry_planks" => 20,
        "dark_oak_planks" => 21,
        "mangrove_planks" => 22,
        "bamboo_planks" => 23,
        "bamboo_mosaic" => 24,
        "oak_sapling" => 25,
        "spruce_sapling" => 27,
        "birch_sapling" => 29,
        "jungle_sapling" => 31,
        "acacia_sapling" => 33,
        "cherry_sapling" => 35,
        "dark_oak_sapling" => 37,
        "mangrove_propagule" => 44,
        "bedrock" => 79,
        "water" => 80,
        "lava" => 96,
        "sand" => 112,
        "suspicious_sand" => 113,
        "red_sand" => 117,
        "gravel" => 118,
        "suspicious_gravel" => 119,
        "gold_ore" => 123,
        "deepslate_gold_ore" => 124,
        "iron_ore" => 125,
        "deepslate_iron_ore" => 126,
        "coal_ore" => 127,
        "deepslate_coal_ore" => 128,
        "nether_gold_ore" => 129,
        "oak_log" => 131,
        "spruce_log" => 134,
        "birch_log" => 137,
        "jungle_log" => 140,
        "acacia_log" => 143,
        "cherry_log" => 146,
        "dark_oak_log" => 149,
        "mangrove_log" => 152,
        "mangrove_roots" => 155,
        "muddy_mangrove_roots" => 157,
        "bamboo_block" => 160,
        "stripped_spruce_log" => 163,
        "stripped_birch_log" => 166,
        "stripped_jungle_log" => 169,
        "stripped_acacia_log" => 172,
        "stripped_cherry_log" => 175,
        "stripped_dark_oak_log" => 178,
        "stripped_oak_log" => 181,
        "stripped_mangrove_log" => 184,
        "stripped_bamboo_block" => 187,
        "oak_wood" => 190,
        "spruce_wood" => 193,
        "birch_wood" => 196,
        "jungle_wood" => 199,
        "acacia_wood" => 202,
        "cherry_wood" => 205,
        "dark_oak_wood" => 208,
        "mangrove_wood" => 211,
        "stripped_oak_wood" => 214,
        "stripped_spruce_wood" => 217,
        "stripped_birch_wood" => 220,
        "stripped_jungle_wood" => 223,
        "stripped_acacia_wood" => 226,
        "stripped_cherry_wood" => 229,
        "stripped_dark_oak_wood" => 232,
        "stripped_mangrove_wood" => 235,
        "oak_leaves" => 264,
        "spruce_leaves" => 292,
        "birch_leaves" => 320,
        "jungle_leaves" => 348,
        "acacia_leaves" => 376,
        "cherry_leaves" => 404,
        "dark_oak_leaves" => 432,
        "mangrove_leaves" => 460,
        "azalea_leaves" => 488,
        "flowering_azalea_leaves" => 516,
        "sponge" => 517,
        "wet_sponge" => 518,
        "glass" => 519,
        "lapis_ore" => 520,
        "deepslate_lapis_ore" => 521,
        "lapis_block" => 522,
        "dispenser" => 524,
        "sandstone" => 535,
        "chiseled_sandstone" => 536,
        "cut_sandstone" => 537,
        "note_block" => 539,
        "white_bed" => 1691,
        "orange_bed" => 1707,
        "magenta_bed" => 1723,
        "light_blue_bed" => 1739,
        "yellow_bed" => 1755,
        "lime_bed" => 1771,
        "pink_bed" => 1787,
        "gray_bed" => 1803,
        "light_gray_bed" => 1819,
        "cyan_bed" => 1835,
        "purple_bed" => 1851,
        "blue_bed" => 1867,
        "brown_bed" => 1883,
        "green_bed" => 1899,
        "red_bed" => 1915,
        "black_bed" => 1931,
        "powered_rail" => 1957,
        "detector_rail" => 1981,
        "sticky_piston" => 1998,
        "cobweb" => 2004,
        "short_grass" => 2005,
        "fern" => 2006,
        "dead_bush" => 2007,
        "seagrass" => 2008,
        "tall_seagrass" => 2010,
        "piston" => 2017,
        "piston_head" => 2025,
        "white_wool" => 2047,
        "orange_wool" => 2048,
        "magenta_wool" => 2049,
        "light_blue_wool" => 2050,
        "yellow_wool" => 2051,
        "lime_wool" => 2052,
        "pink_wool" => 2053,
        "gray_wool" => 2054,
        "light_gray_wool" => 2055,
        "cyan_wool" => 2056,
        "purple_wool" => 2057,
        "blue_wool" => 2058,
        "brown_wool" => 2059,
        "green_wool" => 2060,
        "red_wool" => 2061,
        "black_wool" => 2062,
        "moving_piston" => 2063,
        "dandelion" => 2075,
        "torchflower" => 2076,
        "poppy" => 2077,
        "blue_orchid" => 2078,
        "allium" => 2079,
        "azure_bluet" => 2080,
        "red_tulip" => 2081,
        "orange_tulip" => 2082,
        "white_tulip" => 2083,
        "pink_tulip" => 2084,
        "oxeye_daisy" => 2085,
        "cornflower" => 2086,
        "wither_rose" => 2087,
        "lily_of_the_valley" => 2088,
        "brown_mushroom" => 2089,
        "red_mushroom" => 2090,
        "gold_block" => 2091,
        "iron_block" => 2092,
        "bricks" => 2093,
        "tnt" => 2095,
        "bookshelf" => 2096,
        "chiseled_bookshelf" => 2160,
        "mossy_cobblestone" => 2353,
        "obsidian" => 2354,
        "torch" => 2355,
        "wall_torch" => 2356,
        "fire" => 2391,
        "soul_fire" => 2872,
        "spawner" => 2873,
        "oak_stairs" => 2885,
        "chest" => 2955,
        "redstone_wire" => 4138,
        "diamond_ore" => 4274,
        "deepslate_diamond_ore" => 4275,
        "diamond_block" => 4276,
        "crafting_table" => 4277,
        "wheat" => 4278,
        "growth_lamp" => ST_GROWTH_LAMP,
        "farmland" => 4286,
        "furnace" => 4295,
        "oak_sign" => 4303,
        "spruce_sign" => 4335,
        "birch_sign" => 4367,
        "acacia_sign" => 4399,
        "cherry_sign" => 4431,
        "jungle_sign" => 4463,
        "dark_oak_sign" => 4495,
        "mangrove_sign" => 4527,
        "bamboo_sign" => 4559,
        "oak_door" => 4601,
        "ladder" => 4655,
        "rail" => 4663,
        "cobblestone_stairs" => 4693,
        "oak_wall_sign" => 4763,
        "spruce_wall_sign" => 4771,
        "birch_wall_sign" => 4779,
        "acacia_wall_sign" => 4787,
        "cherry_wall_sign" => 4795,
        "jungle_wall_sign" => 4803,
        "dark_oak_wall_sign" => 4811,
        "mangrove_wall_sign" => 4819,
        "bamboo_wall_sign" => 4827,
        "oak_hanging_sign" => 4867,
        "spruce_hanging_sign" => 4931,
        "birch_hanging_sign" => 4995,
        "acacia_hanging_sign" => 5059,
        "cherry_hanging_sign" => 5123,
        "jungle_hanging_sign" => 5187,
        "dark_oak_hanging_sign" => 5251,
        "crimson_hanging_sign" => 5315,
        "warped_hanging_sign" => 5379,
        "mangrove_hanging_sign" => 5443,
        "bamboo_hanging_sign" => 5507,
        "oak_wall_hanging_sign" => 5539,
        "spruce_wall_hanging_sign" => 5547,
        "birch_wall_hanging_sign" => 5555,
        "acacia_wall_hanging_sign" => 5563,
        "cherry_wall_hanging_sign" => 5571,
        "jungle_wall_hanging_sign" => 5579,
        "dark_oak_wall_hanging_sign" => 5587,
        "mangrove_wall_hanging_sign" => 5595,
        "crimson_wall_hanging_sign" => 5603,
        "warped_wall_hanging_sign" => 5611,
        "bamboo_wall_hanging_sign" => 5619,
        "lever" => 5635,
        "stone_pressure_plate" => 5651,
        "iron_door" => 5663,
        "oak_pressure_plate" => 5717,
        "spruce_pressure_plate" => 5719,
        "birch_pressure_plate" => 5721,
        "jungle_pressure_plate" => 5723,
        "acacia_pressure_plate" => 5725,
        "cherry_pressure_plate" => 5727,
        "dark_oak_pressure_plate" => 5729,
        "mangrove_pressure_plate" => 5731,
        "bamboo_pressure_plate" => 5733,
        "redstone_ore" => 5735,
        "deepslate_redstone_ore" => 5737,
        "redstone_torch" => 5738,
        "redstone_wall_torch" => 5740,
        "stone_button" => 5757,
        "snow" => 5772,
        "ice" => 5780,
        "snow_block" => 5781,
        "cactus" => 5782,
        "clay" => 5798,
        "sugar_cane" => 5799,
        "jukebox" => 5816,
        "oak_fence" => 5848,
        "netherrack" => 5849,
        "soul_sand" => 5850,
        "soul_soil" => 5851,
        "basalt" => 5853,
        "polished_basalt" => 5856,
        "soul_torch" => 5858,
        "soul_wall_torch" => 5859,
        "glowstone" => 5863,
        "nether_portal" => 5864,
        "carved_pumpkin" => 5866,
        "jack_o_lantern" => 5870,
        "cake" => 5874,
        "repeater" => 5884,
        "white_stained_glass" => 5945,
        "orange_stained_glass" => 5946,
        "magenta_stained_glass" => 5947,
        "light_blue_stained_glass" => 5948,
        "yellow_stained_glass" => 5949,
        "lime_stained_glass" => 5950,
        "pink_stained_glass" => 5951,
        "gray_stained_glass" => 5952,
        "light_gray_stained_glass" => 5953,
        "cyan_stained_glass" => 5954,
        "purple_stained_glass" => 5955,
        "blue_stained_glass" => 5956,
        "brown_stained_glass" => 5957,
        "green_stained_glass" => 5958,
        "red_stained_glass" => 5959,
        "black_stained_glass" => 5960,
        "oak_trapdoor" => 5976,
        "spruce_trapdoor" => 6040,
        "birch_trapdoor" => 6104,
        "jungle_trapdoor" => 6168,
        "acacia_trapdoor" => 6232,
        "cherry_trapdoor" => 6296,
        "dark_oak_trapdoor" => 6360,
        "mangrove_trapdoor" => 6424,
        "bamboo_trapdoor" => 6488,
        "stone_bricks" => 6537,
        "mossy_stone_bricks" => 6538,
        "cracked_stone_bricks" => 6539,
        "chiseled_stone_bricks" => 6540,
        "packed_mud" => 6541,
        "mud_bricks" => 6542,
        "infested_stone" => 6543,
        "infested_cobblestone" => 6544,
        "infested_stone_bricks" => 6545,
        "infested_mossy_stone_bricks" => 6546,
        "infested_cracked_stone_bricks" => 6547,
        "infested_chiseled_stone_bricks" => 6548,
        "brown_mushroom_block" => 6549,
        "red_mushroom_block" => 6613,
        "mushroom_stem" => 6677,
        "iron_bars" => 6772,
        "chain" => 6776,
        "glass_pane" => 6810,
        "pumpkin" => 6811,
        "melon" => 6812,
        "attached_pumpkin_stem" => 6813,
        "attached_melon_stem" => 6817,
        "pumpkin_stem" => 6821,
        "melon_stem" => 6829,
        "vine" => 6868,
        "glow_lichen" => 6996,
        "oak_fence_gate" => 7004,
        "brick_stairs" => 7040,
        "stone_brick_stairs" => 7120,
        "mud_brick_stairs" => 7200,
        "mycelium" => 7270,
        "lily_pad" => 7271,
        "nether_bricks" => 7272,
        "nether_brick_fence" => 7304,
        "nether_brick_stairs" => 7316,
        "nether_wart" => 7385,
        "enchanting_table" => 7389,
        "brewing_stand" => 7397,
        "cauldron" => 7398,
        "water_cauldron" => 7399,
        "lava_cauldron" => 7402,
        "powder_snow_cauldron" => 7403,
        "end_portal" => 7406,
        "end_portal_frame" => 7411,
        "end_stone" => 7415,
        "dragon_egg" => 7416,
        "redstone_lamp" => 7418,
        "cocoa" => 7419,
        "sandstone_stairs" => 7442,
        "emerald_ore" => 7511,
        "deepslate_emerald_ore" => 7512,
        "ender_chest" => 7514,
        "tripwire_hook" => 7530,
        "tripwire" => 7664,
        "emerald_block" => 7665,
        "spruce_stairs" => 7677,
        "birch_stairs" => 7757,
        "jungle_stairs" => 7837,
        "command_block" => 7912,
        "beacon" => 7918,
        "cobblestone_wall" => 7922,
        "mossy_cobblestone_wall" => 8246,
        "flower_pot" => 8567,
        "potted_torchflower" => 8568,
        "potted_oak_sapling" => 8569,
        "potted_spruce_sapling" => 8570,
        "potted_birch_sapling" => 8571,
        "potted_jungle_sapling" => 8572,
        "potted_acacia_sapling" => 8573,
        "potted_cherry_sapling" => 8574,
        "potted_dark_oak_sapling" => 8575,
        "potted_mangrove_propagule" => 8576,
        "potted_fern" => 8577,
        "potted_dandelion" => 8578,
        "potted_poppy" => 8579,
        "potted_blue_orchid" => 8580,
        "potted_allium" => 8581,
        "potted_azure_bluet" => 8582,
        "potted_red_tulip" => 8583,
        "potted_orange_tulip" => 8584,
        "potted_white_tulip" => 8585,
        "potted_pink_tulip" => 8586,
        "potted_oxeye_daisy" => 8587,
        "potted_cornflower" => 8588,
        "potted_lily_of_the_valley" => 8589,
        "potted_wither_rose" => 8590,
        "potted_red_mushroom" => 8591,
        "potted_brown_mushroom" => 8592,
        "potted_dead_bush" => 8593,
        "potted_cactus" => 8594,
        "carrots" => 8595,
        "potatoes" => 8603,
        "oak_button" => 8620,
        "spruce_button" => 8644,
        "birch_button" => 8668,
        "jungle_button" => 8692,
        "acacia_button" => 8716,
        "cherry_button" => 8740,
        "dark_oak_button" => 8764,
        "mangrove_button" => 8788,
        "bamboo_button" => 8812,
        "skeleton_skull" => 8843,
        "skeleton_wall_skull" => 8860,
        "wither_skeleton_skull" => 8883,
        "wither_skeleton_wall_skull" => 8900,
        "zombie_head" => 8923,
        "zombie_wall_head" => 8940,
        "player_head" => 8963,
        "player_wall_head" => 8980,
        "creeper_head" => 9003,
        "creeper_wall_head" => 9020,
        "dragon_head" => 9043,
        "dragon_wall_head" => 9060,
        "piglin_head" => 9083,
        "piglin_wall_head" => 9100,
        "anvil" => 9107,
        "chipped_anvil" => 9111,
        "damaged_anvil" => 9115,
        "trapped_chest" => 9120,
        "light_weighted_pressure_plate" => 9143,
        "heavy_weighted_pressure_plate" => 9159,
        "comparator" => 9176,
        "daylight_detector" => 9207,
        "redstone_block" => 9223,
        "nether_quartz_ore" => 9224,
        "hopper" => 9225,
        "quartz_block" => 9235,
        "chiseled_quartz_block" => 9236,
        "quartz_pillar" => 9238,
        "quartz_stairs" => 9251,
        "activator_rail" => 9333,
        "dropper" => 9345,
        "white_terracotta" => 9356,
        "orange_terracotta" => 9357,
        "magenta_terracotta" => 9358,
        "light_blue_terracotta" => 9359,
        "yellow_terracotta" => 9360,
        "lime_terracotta" => 9361,
        "pink_terracotta" => 9362,
        "gray_terracotta" => 9363,
        "light_gray_terracotta" => 9364,
        "cyan_terracotta" => 9365,
        "purple_terracotta" => 9366,
        "blue_terracotta" => 9367,
        "brown_terracotta" => 9368,
        "green_terracotta" => 9369,
        "red_terracotta" => 9370,
        "black_terracotta" => 9371,
        "white_stained_glass_pane" => 9403,
        "orange_stained_glass_pane" => 9435,
        "magenta_stained_glass_pane" => 9467,
        "light_blue_stained_glass_pane" => 9499,
        "yellow_stained_glass_pane" => 9531,
        "lime_stained_glass_pane" => 9563,
        "pink_stained_glass_pane" => 9595,
        "gray_stained_glass_pane" => 9627,
        "light_gray_stained_glass_pane" => 9659,
        "cyan_stained_glass_pane" => 9691,
        "purple_stained_glass_pane" => 9723,
        "blue_stained_glass_pane" => 9755,
        "brown_stained_glass_pane" => 9787,
        "green_stained_glass_pane" => 9819,
        "red_stained_glass_pane" => 9851,
        "black_stained_glass_pane" => 9883,
        "acacia_stairs" => 9895,
        "cherry_stairs" => 9975,
        "dark_oak_stairs" => 10055,
        "mangrove_stairs" => 10135,
        "bamboo_stairs" => 10215,
        "bamboo_mosaic_stairs" => 10295,
        "slime_block" => 10364,
        "barrier" => 10366,
        "light" => 10398,
        "iron_trapdoor" => 10414,
        "prismarine" => 10463,
        "prismarine_bricks" => 10464,
        "dark_prismarine" => 10465,
        "prismarine_stairs" => 10477,
        "prismarine_brick_stairs" => 10557,
        "dark_prismarine_stairs" => 10637,
        "prismarine_slab" => 10709,
        "prismarine_brick_slab" => 10715,
        "dark_prismarine_slab" => 10721,
        "sea_lantern" => 10724,
        "hay_block" => 10726,
        "white_carpet" => 10728,
        "orange_carpet" => 10729,
        "magenta_carpet" => 10730,
        "light_blue_carpet" => 10731,
        "yellow_carpet" => 10732,
        "lime_carpet" => 10733,
        "pink_carpet" => 10734,
        "gray_carpet" => 10735,
        "light_gray_carpet" => 10736,
        "cyan_carpet" => 10737,
        "purple_carpet" => 10738,
        "blue_carpet" => 10739,
        "brown_carpet" => 10740,
        "green_carpet" => 10741,
        "red_carpet" => 10742,
        "black_carpet" => 10743,
        "terracotta" => 10744,
        "coal_block" => 10745,
        "packed_ice" => 10746,
        "sunflower" => 10748,
        "lilac" => 10750,
        "rose_bush" => 10752,
        "peony" => 10754,
        "tall_grass" => 10756,
        "large_fern" => 10758,
        "white_banner" => 10759,
        "orange_banner" => 10775,
        "magenta_banner" => 10791,
        "light_blue_banner" => 10807,
        "yellow_banner" => 10823,
        "lime_banner" => 10839,
        "pink_banner" => 10855,
        "gray_banner" => 10871,
        "light_gray_banner" => 10887,
        "cyan_banner" => 10903,
        "purple_banner" => 10919,
        "blue_banner" => 10935,
        "brown_banner" => 10951,
        "green_banner" => 10967,
        "red_banner" => 10983,
        "black_banner" => 10999,
        "white_wall_banner" => 11015,
        "orange_wall_banner" => 11019,
        "magenta_wall_banner" => 11023,
        "light_blue_wall_banner" => 11027,
        "yellow_wall_banner" => 11031,
        "lime_wall_banner" => 11035,
        "pink_wall_banner" => 11039,
        "gray_wall_banner" => 11043,
        "light_gray_wall_banner" => 11047,
        "cyan_wall_banner" => 11051,
        "purple_wall_banner" => 11055,
        "blue_wall_banner" => 11059,
        "brown_wall_banner" => 11063,
        "green_wall_banner" => 11067,
        "red_wall_banner" => 11071,
        "black_wall_banner" => 11075,
        "red_sandstone" => 11079,
        "chiseled_red_sandstone" => 11080,
        "cut_red_sandstone" => 11081,
        "red_sandstone_stairs" => 11093,
        "oak_slab" => 11165,
        "spruce_slab" => 11171,
        "birch_slab" => 11177,
        "jungle_slab" => 11183,
        "acacia_slab" => 11189,
        "cherry_slab" => 11195,
        "dark_oak_slab" => 11201,
        "mangrove_slab" => 11207,
        "bamboo_slab" => 11213,
        "bamboo_mosaic_slab" => 11219,
        "stone_slab" => 11225,
        "smooth_stone_slab" => 11231,
        "sandstone_slab" => 11237,
        "cut_sandstone_slab" => 11243,
        "petrified_oak_slab" => 11249,
        "cobblestone_slab" => 11255,
        "brick_slab" => 11261,
        "stone_brick_slab" => 11267,
        "mud_brick_slab" => 11273,
        "nether_brick_slab" => 11279,
        "quartz_slab" => 11285,
        "red_sandstone_slab" => 11291,
        "cut_red_sandstone_slab" => 11297,
        "purpur_slab" => 11303,
        "smooth_stone" => 11306,
        "smooth_sandstone" => 11307,
        "smooth_quartz" => 11308,
        "smooth_red_sandstone" => 11309,
        "spruce_fence_gate" => 11317,
        "birch_fence_gate" => 11349,
        "jungle_fence_gate" => 11381,
        "acacia_fence_gate" => 11413,
        "cherry_fence_gate" => 11445,
        "dark_oak_fence_gate" => 11477,
        "mangrove_fence_gate" => 11509,
        "bamboo_fence_gate" => 11541,
        "spruce_fence" => 11597,
        "birch_fence" => 11629,
        "jungle_fence" => 11661,
        "acacia_fence" => 11693,
        "cherry_fence" => 11725,
        "dark_oak_fence" => 11757,
        "mangrove_fence" => 11789,
        "bamboo_fence" => 11821,
        "spruce_door" => 11833,
        "birch_door" => 11897,
        "jungle_door" => 11961,
        "acacia_door" => 12025,
        "cherry_door" => 12089,
        "dark_oak_door" => 12153,
        "mangrove_door" => 12217,
        "bamboo_door" => 12281,
        "end_rod" => 12338,
        "chorus_plant" => 12403,
        "chorus_flower" => 12404,
        "purpur_block" => 12410,
        "purpur_pillar" => 12412,
        "purpur_stairs" => 12425,
        "end_stone_bricks" => 12494,
        "torchflower_crop" => 12495,
        "pitcher_crop" => 12498,
        "pitcher_plant" => 12508,
        "beetroots" => 12509,
        "dirt_path" => 12513,
        "end_gateway" => 12514,
        "repeating_command_block" => 12521,
        "chain_command_block" => 12533,
        "frosted_ice" => 12539,
        "magma_block" => 12543,
        "nether_wart_block" => 12544,
        "red_nether_bricks" => 12545,
        "bone_block" => 12547,
        "structure_void" => 12549,
        "observer" => 12555,
        "shulker_box" => 12566,
        "white_shulker_box" => 12572,
        "orange_shulker_box" => 12578,
        "magenta_shulker_box" => 12584,
        "light_blue_shulker_box" => 12590,
        "yellow_shulker_box" => 12596,
        "lime_shulker_box" => 12602,
        "pink_shulker_box" => 12608,
        "gray_shulker_box" => 12614,
        "light_gray_shulker_box" => 12620,
        "cyan_shulker_box" => 12626,
        "purple_shulker_box" => 12632,
        "blue_shulker_box" => 12638,
        "brown_shulker_box" => 12644,
        "green_shulker_box" => 12650,
        "red_shulker_box" => 12656,
        "black_shulker_box" => 12662,
        "white_glazed_terracotta" => 12664,
        "orange_glazed_terracotta" => 12668,
        "magenta_glazed_terracotta" => 12672,
        "light_blue_glazed_terracotta" => 12676,
        "yellow_glazed_terracotta" => 12680,
        "lime_glazed_terracotta" => 12684,
        "pink_glazed_terracotta" => 12688,
        "gray_glazed_terracotta" => 12692,
        "light_gray_glazed_terracotta" => 12696,
        "cyan_glazed_terracotta" => 12700,
        "purple_glazed_terracotta" => 12704,
        "blue_glazed_terracotta" => 12708,
        "brown_glazed_terracotta" => 12712,
        "green_glazed_terracotta" => 12716,
        "red_glazed_terracotta" => 12720,
        "black_glazed_terracotta" => 12724,
        "white_concrete" => 12728,
        "orange_concrete" => 12729,
        "magenta_concrete" => 12730,
        "light_blue_concrete" => 12731,
        "yellow_concrete" => 12732,
        "lime_concrete" => 12733,
        "pink_concrete" => 12734,
        "gray_concrete" => 12735,
        "light_gray_concrete" => 12736,
        "cyan_concrete" => 12737,
        "purple_concrete" => 12738,
        "blue_concrete" => 12739,
        "brown_concrete" => 12740,
        "green_concrete" => 12741,
        "red_concrete" => 12742,
        "black_concrete" => 12743,
        "white_concrete_powder" => 12744,
        "orange_concrete_powder" => 12745,
        "magenta_concrete_powder" => 12746,
        "light_blue_concrete_powder" => 12747,
        "yellow_concrete_powder" => 12748,
        "lime_concrete_powder" => 12749,
        "pink_concrete_powder" => 12750,
        "gray_concrete_powder" => 12751,
        "light_gray_concrete_powder" => 12752,
        "cyan_concrete_powder" => 12753,
        "purple_concrete_powder" => 12754,
        "blue_concrete_powder" => 12755,
        "brown_concrete_powder" => 12756,
        "green_concrete_powder" => 12757,
        "red_concrete_powder" => 12758,
        "black_concrete_powder" => 12759,
        "kelp" => 12760,
        "kelp_plant" => 12786,
        "dried_kelp_block" => 12787,
        "turtle_egg" => 12788,
        "sniffer_egg" => 12800,
        "dead_tube_coral_block" => 12803,
        "dead_brain_coral_block" => 12804,
        "dead_bubble_coral_block" => 12805,
        "dead_fire_coral_block" => 12806,
        "dead_horn_coral_block" => 12807,
        "tube_coral_block" => 12808,
        "brain_coral_block" => 12809,
        "bubble_coral_block" => 12810,
        "fire_coral_block" => 12811,
        "horn_coral_block" => 12812,
        "dead_tube_coral" => 12813,
        "dead_brain_coral" => 12815,
        "dead_bubble_coral" => 12817,
        "dead_fire_coral" => 12819,
        "dead_horn_coral" => 12821,
        "tube_coral" => 12823,
        "brain_coral" => 12825,
        "bubble_coral" => 12827,
        "fire_coral" => 12829,
        "horn_coral" => 12831,
        "dead_tube_coral_fan" => 12833,
        "dead_brain_coral_fan" => 12835,
        "dead_bubble_coral_fan" => 12837,
        "dead_fire_coral_fan" => 12839,
        "dead_horn_coral_fan" => 12841,
        "tube_coral_fan" => 12843,
        "brain_coral_fan" => 12845,
        "bubble_coral_fan" => 12847,
        "fire_coral_fan" => 12849,
        "horn_coral_fan" => 12851,
        "dead_tube_coral_wall_fan" => 12853,
        "dead_brain_coral_wall_fan" => 12861,
        "dead_bubble_coral_wall_fan" => 12869,
        "dead_fire_coral_wall_fan" => 12877,
        "dead_horn_coral_wall_fan" => 12885,
        "tube_coral_wall_fan" => 12893,
        "brain_coral_wall_fan" => 12901,
        "bubble_coral_wall_fan" => 12909,
        "fire_coral_wall_fan" => 12917,
        "horn_coral_wall_fan" => 12925,
        "sea_pickle" => 12933,
        "blue_ice" => 12941,
        "conduit" => 12942,
        "bamboo_sapling" => 12944,
        "bamboo" => 12945,
        "potted_bamboo" => 12957,
        "void_air" => 12958,
        "cave_air" => 12959,
        "bubble_column" => 12960,
        "polished_granite_stairs" => 12973,
        "smooth_red_sandstone_stairs" => 13053,
        "mossy_stone_brick_stairs" => 13133,
        "polished_diorite_stairs" => 13213,
        "mossy_cobblestone_stairs" => 13293,
        "end_stone_brick_stairs" => 13373,
        "stone_stairs" => 13453,
        "smooth_sandstone_stairs" => 13533,
        "smooth_quartz_stairs" => 13613,
        "granite_stairs" => 13693,
        "andesite_stairs" => 13773,
        "red_nether_brick_stairs" => 13853,
        "polished_andesite_stairs" => 13933,
        "diorite_stairs" => 14013,
        "polished_granite_slab" => 14085,
        "smooth_red_sandstone_slab" => 14091,
        "mossy_stone_brick_slab" => 14097,
        "polished_diorite_slab" => 14103,
        "mossy_cobblestone_slab" => 14109,
        "end_stone_brick_slab" => 14115,
        "smooth_sandstone_slab" => 14121,
        "smooth_quartz_slab" => 14127,
        "granite_slab" => 14133,
        "andesite_slab" => 14139,
        "red_nether_brick_slab" => 14145,
        "polished_andesite_slab" => 14151,
        "diorite_slab" => 14157,
        "brick_wall" => 14163,
        "prismarine_wall" => 14487,
        "red_sandstone_wall" => 14811,
        "mossy_stone_brick_wall" => 15135,
        "granite_wall" => 15459,
        "stone_brick_wall" => 15783,
        "mud_brick_wall" => 16107,
        "nether_brick_wall" => 16431,
        "andesite_wall" => 16755,
        "red_nether_brick_wall" => 17079,
        "sandstone_wall" => 17403,
        "end_stone_brick_wall" => 17727,
        "diorite_wall" => 18051,
        "scaffolding" => 18403,
        "loom" => 18404,
        "barrel" => 18409,
        "smoker" => 18421,
        "blast_furnace" => 18429,
        "cartography_table" => 18436,
        "fletching_table" => 18437,
        "grindstone" => 18442,
        "lectern" => 18453,
        "smithing_table" => 18466,
        "stonecutter" => 18467,
        "bell" => 18472,
        "lantern" => 18506,
        "soul_lantern" => 18510,
        "campfire" => 18514,
        "soul_campfire" => 18546,
        "sweet_berry_bush" => 18575,
        "warped_stem" => 18580,
        "stripped_warped_stem" => 18583,
        "warped_hyphae" => 18586,
        "stripped_warped_hyphae" => 18589,
        "warped_nylium" => 18591,
        "warped_fungus" => 18592,
        "warped_wart_block" => 18593,
        "warped_roots" => 18594,
        "nether_sprouts" => 18595,
        "crimson_stem" => 18597,
        "stripped_crimson_stem" => 18600,
        "crimson_hyphae" => 18603,
        "stripped_crimson_hyphae" => 18606,
        "crimson_nylium" => 18608,
        "crimson_fungus" => 18609,
        "shroomlight" => 18610,
        "weeping_vines" => 18611,
        "weeping_vines_plant" => 18637,
        "twisting_vines" => 18638,
        "twisting_vines_plant" => 18664,
        "crimson_roots" => 18665,
        "crimson_planks" => 18666,
        "warped_planks" => 18667,
        "crimson_slab" => 18671,
        "warped_slab" => 18677,
        "crimson_pressure_plate" => 18681,
        "warped_pressure_plate" => 18683,
        "crimson_fence" => 18715,
        "warped_fence" => 18747,
        "crimson_trapdoor" => 18763,
        "warped_trapdoor" => 18827,
        "crimson_fence_gate" => 18883,
        "warped_fence_gate" => 18915,
        "crimson_stairs" => 18951,
        "warped_stairs" => 19031,
        "crimson_button" => 19109,
        "warped_button" => 19133,
        "crimson_door" => 19159,
        "warped_door" => 19223,
        "crimson_sign" => 19277,
        "warped_sign" => 19309,
        "crimson_wall_sign" => 19341,
        "warped_wall_sign" => 19349,
        "structure_block" => 19357,
        "jigsaw" => 19370,
        "composter" => 19372,
        "target" => 19381,
        "bee_nest" => 19397,
        "beehive" => 19421,
        "honey_block" => 19445,
        "honeycomb_block" => 19446,
        "netherite_block" => 19447,
        "ancient_debris" => 19448,
        "crying_obsidian" => 19449,
        "respawn_anchor" => 19450,
        "potted_crimson_fungus" => 19455,
        "potted_warped_fungus" => 19456,
        "potted_crimson_roots" => 19457,
        "potted_warped_roots" => 19458,
        "lodestone" => 19459,
        "blackstone" => 19460,
        "blackstone_stairs" => 19472,
        "blackstone_wall" => 19544,
        "blackstone_slab" => 19868,
        "polished_blackstone" => 19871,
        "polished_blackstone_bricks" => 19872,
        "cracked_polished_blackstone_bricks" => 19873,
        "chiseled_polished_blackstone" => 19874,
        "polished_blackstone_brick_slab" => 19878,
        "polished_blackstone_brick_stairs" => 19892,
        "polished_blackstone_brick_wall" => 19964,
        "gilded_blackstone" => 20285,
        "polished_blackstone_stairs" => 20297,
        "polished_blackstone_slab" => 20369,
        "polished_blackstone_pressure_plate" => 20373,
        "polished_blackstone_button" => 20383,
        "polished_blackstone_wall" => 20401,
        "chiseled_nether_bricks" => 20722,
        "cracked_nether_bricks" => 20723,
        "quartz_bricks" => 20724,
        "candle" => 20728,
        "white_candle" => 20744,
        "orange_candle" => 20760,
        "magenta_candle" => 20776,
        "light_blue_candle" => 20792,
        "yellow_candle" => 20808,
        "lime_candle" => 20824,
        "pink_candle" => 20840,
        "gray_candle" => 20856,
        "light_gray_candle" => 20872,
        "cyan_candle" => 20888,
        "purple_candle" => 20904,
        "blue_candle" => 20920,
        "brown_candle" => 20936,
        "green_candle" => 20952,
        "red_candle" => 20968,
        "black_candle" => 20984,
        "candle_cake" => 20998,
        "white_candle_cake" => 21000,
        "orange_candle_cake" => 21002,
        "magenta_candle_cake" => 21004,
        "light_blue_candle_cake" => 21006,
        "yellow_candle_cake" => 21008,
        "lime_candle_cake" => 21010,
        "pink_candle_cake" => 21012,
        "gray_candle_cake" => 21014,
        "light_gray_candle_cake" => 21016,
        "cyan_candle_cake" => 21018,
        "purple_candle_cake" => 21020,
        "blue_candle_cake" => 21022,
        "brown_candle_cake" => 21024,
        "green_candle_cake" => 21026,
        "red_candle_cake" => 21028,
        "black_candle_cake" => 21030,
        "amethyst_block" => 21031,
        "budding_amethyst" => 21032,
        "amethyst_cluster" => 21042,
        "large_amethyst_bud" => 21054,
        "medium_amethyst_bud" => 21066,
        "small_amethyst_bud" => 21078,
        "tuff" => 21081,
        "tuff_slab" => 21085,
        "tuff_stairs" => 21099,
        "tuff_wall" => 21171,
        "polished_tuff" => 21492,
        "polished_tuff_slab" => 21496,
        "polished_tuff_stairs" => 21510,
        "polished_tuff_wall" => 21582,
        "chiseled_tuff" => 21903,
        "tuff_bricks" => 21904,
        "tuff_brick_slab" => 21908,
        "tuff_brick_stairs" => 21922,
        "tuff_brick_wall" => 21994,
        "chiseled_tuff_bricks" => 22315,
        "calcite" => 22316,
        "tinted_glass" => 22317,
        "powder_snow" => 22318,
        "sculk_sensor" => 22320,
        "calibrated_sculk_sensor" => 22416,
        "sculk" => 22799,
        "sculk_vein" => 22927,
        "sculk_catalyst" => 22929,
        "sculk_shrieker" => 22937,
        "copper_block" => 22938,
        "exposed_copper" => 22939,
        "weathered_copper" => 22940,
        "oxidized_copper" => 22941,
        "copper_ore" => 22942,
        "deepslate_copper_ore" => 22943,
        "oxidized_cut_copper" => 22944,
        "weathered_cut_copper" => 22945,
        "exposed_cut_copper" => 22946,
        "cut_copper" => 22947,
        "oxidized_chiseled_copper" => 22948,
        "weathered_chiseled_copper" => 22949,
        "exposed_chiseled_copper" => 22950,
        "chiseled_copper" => 22951,
        "waxed_oxidized_chiseled_copper" => 22952,
        "waxed_weathered_chiseled_copper" => 22953,
        "waxed_exposed_chiseled_copper" => 22954,
        "waxed_chiseled_copper" => 22955,
        "oxidized_cut_copper_stairs" => 22967,
        "weathered_cut_copper_stairs" => 23047,
        "exposed_cut_copper_stairs" => 23127,
        "cut_copper_stairs" => 23207,
        "oxidized_cut_copper_slab" => 23279,
        "weathered_cut_copper_slab" => 23285,
        "exposed_cut_copper_slab" => 23291,
        "cut_copper_slab" => 23297,
        "waxed_copper_block" => 23300,
        "waxed_weathered_copper" => 23301,
        "waxed_exposed_copper" => 23302,
        "waxed_oxidized_copper" => 23303,
        "waxed_oxidized_cut_copper" => 23304,
        "waxed_weathered_cut_copper" => 23305,
        "waxed_exposed_cut_copper" => 23306,
        "waxed_cut_copper" => 23307,
        "waxed_oxidized_cut_copper_stairs" => 23319,
        "waxed_weathered_cut_copper_stairs" => 23399,
        "waxed_exposed_cut_copper_stairs" => 23479,
        "waxed_cut_copper_stairs" => 23559,
        "waxed_oxidized_cut_copper_slab" => 23631,
        "waxed_weathered_cut_copper_slab" => 23637,
        "waxed_exposed_cut_copper_slab" => 23643,
        "waxed_cut_copper_slab" => 23649,
        "copper_door" => 23663,
        "exposed_copper_door" => 23727,
        "oxidized_copper_door" => 23791,
        "weathered_copper_door" => 23855,
        "waxed_copper_door" => 23919,
        "waxed_exposed_copper_door" => 23983,
        "waxed_oxidized_copper_door" => 24047,
        "waxed_weathered_copper_door" => 24111,
        "copper_trapdoor" => 24179,
        "exposed_copper_trapdoor" => 24243,
        "oxidized_copper_trapdoor" => 24307,
        "weathered_copper_trapdoor" => 24371,
        "waxed_copper_trapdoor" => 24435,
        "waxed_exposed_copper_trapdoor" => 24499,
        "waxed_oxidized_copper_trapdoor" => 24563,
        "waxed_weathered_copper_trapdoor" => 24627,
        "copper_grate" => 24677,
        "exposed_copper_grate" => 24679,
        "weathered_copper_grate" => 24681,
        "oxidized_copper_grate" => 24683,
        "waxed_copper_grate" => 24685,
        "waxed_exposed_copper_grate" => 24687,
        "waxed_weathered_copper_grate" => 24689,
        "waxed_oxidized_copper_grate" => 24691,
        "copper_bulb" => 24695,
        "exposed_copper_bulb" => 24699,
        "weathered_copper_bulb" => 24703,
        "oxidized_copper_bulb" => 24707,
        "waxed_copper_bulb" => 24711,
        "waxed_exposed_copper_bulb" => 24715,
        "waxed_weathered_copper_bulb" => 24719,
        "waxed_oxidized_copper_bulb" => 24723,
        "lightning_rod" => 24743,
        "pointed_dripstone" => 24753,
        "dripstone_block" => 24768,
        "cave_vines" => 24770,
        "cave_vines_plant" => 24822,
        "spore_blossom" => 24823,
        "azalea" => 24824,
        "flowering_azalea" => 24825,
        "moss_carpet" => 24826,
        "pink_petals" => 24827,
        "moss_block" => 24843,
        "big_dripleaf" => 24845,
        "big_dripleaf_stem" => 24877,
        "small_dripleaf" => 24887,
        "hanging_roots" => 24901,
        "rooted_dirt" => 24902,
        "mud" => 24903,
        "deepslate" => 24905,
        "cobbled_deepslate" => 24907,
        "cobbled_deepslate_stairs" => 24919,
        "cobbled_deepslate_slab" => 24991,
        "cobbled_deepslate_wall" => 24997,
        "polished_deepslate" => 25318,
        "polished_deepslate_stairs" => 25330,
        "polished_deepslate_slab" => 25402,
        "polished_deepslate_wall" => 25408,
        "deepslate_tiles" => 25729,
        "deepslate_tile_stairs" => 25741,
        "deepslate_tile_slab" => 25813,
        "deepslate_tile_wall" => 25819,
        "deepslate_bricks" => 26140,
        "deepslate_brick_stairs" => 26152,
        "deepslate_brick_slab" => 26224,
        "deepslate_brick_wall" => 26230,
        "chiseled_deepslate" => 26551,
        "cracked_deepslate_bricks" => 26552,
        "cracked_deepslate_tiles" => 26553,
        "infested_deepslate" => 26555,
        "smooth_basalt" => 26557,
        "raw_iron_block" => 26558,
        "raw_copper_block" => 26559,
        "raw_gold_block" => 26560,
        "potted_azalea_bush" => 26561,
        "potted_flowering_azalea_bush" => 26562,
        "ochre_froglight" => 26564,
        "verdant_froglight" => 26567,
        "pearlescent_froglight" => 26570,
        "frogspawn" => 26572,
        "reinforced_deepslate" => 26573,
        "decorated_pot" => 26583,
        "crafter" => 26635,
        "trial_spawner" => 26644,
        "vault" => 26654,
        "heavy_core" => 26683,
        _ => return None,
    })
}

// ---------- datapacks (directories and ZIP files under datapacks/) ----------
// Lane P3 extended tag/recipe registries + command stores live in the included
// file (same flat scope — no `mod`, keeping the 44-module layer gate).
include!("datapack.rs");
struct Datapack {
    name: String,
    functions: HashMap<String, Vec<String>>,
    load: Vec<String>,
    tick: Vec<String>,
}
static PACKS: OnceLock<Mutex<Vec<Datapack>>> = OnceLock::new();
fn packs() -> &'static Mutex<Vec<Datapack>> {
    PACKS.get_or_init(|| Mutex::new(Vec::new()))
}

#[derive(Default)]
struct DatapackContent {
    packs: Vec<Datapack>,
    recipes: Vec<JsonRecipe>,
    recipe_ids: std::collections::BTreeSet<String>,
    smelts: Vec<SmeltRecipe>,
    registry: DatapackRegistry,
    /// `{"id": ..., "required": false}` tag members, deferred until every pack
    /// has contributed — `resolve_optional_tags` drops the unresolved ones.
    optional_tags: Vec<(String, String, String)>,
    loot: HashMap<String, Vec<LootPool>>,
    loot_raw: HashMap<String, String>,
    mob_loot: HashMap<i64, Vec<LootPool>>,
    item_mods: HashMap<String, Vec<LootFunc>>,
    advancements: HashMap<String, Advancement>,
    predicates: HashMap<String, String>,
}

impl DatapackContent {
    fn publish(mut self) {
        self.resolve_optional_tags();
        // `block`/`item` mirror into the legacy globals consumed elsewhere.
        let block_map = self
            .registry
            .tags
            .get("block")
            .cloned()
            .unwrap_or_default();
        let item_map = self
            .registry
            .tags
            .get("item")
            .cloned()
            .unwrap_or_default();
        // Recipes consult item tags while holding their own locks. Counts also
        // reads block tags under the recipe lock. Publish in that lock order.
        let mut recipes = json_recipes().lock().unwrap_or_else(|e| e.into_inner());
        let mut smelts = json_smelts().lock().unwrap_or_else(|e| e.into_inner());
        let mut blocks = block_tags().lock().unwrap_or_else(|e| e.into_inner());
        let mut items = item_tags().lock().unwrap_or_else(|e| e.into_inner());
        let mut loot = loot_tables().lock().unwrap_or_else(|e| e.into_inner());
        let mut loot_raw = raw_loot_tables().lock().unwrap_or_else(|e| e.into_inner());
        let mut mob_loot = mob_loot_tables().lock().unwrap_or_else(|e| e.into_inner());
        let mut mods = item_mods().lock().unwrap_or_else(|e| e.into_inner());
        let mut advs = advancements().lock().unwrap_or_else(|e| e.into_inner());
        let mut conditions = predicates().lock().unwrap_or_else(|e| e.into_inner());
        let mut ids = recipe_ids().lock().unwrap_or_else(|e| e.into_inner());
        let mut registry = datapack_registry().lock().unwrap_or_else(|e| e.into_inner());
        let mut active = packs().lock().unwrap_or_else(|e| e.into_inner());
        *recipes = self.recipes;
        *ids = self.recipe_ids;
        *smelts = self.smelts;
        *blocks = block_map;
        *items = item_map;
        *loot = self.loot;
        *loot_raw = self.loot_raw;
        *mob_loot = self.mob_loot;
        *mods = self.item_mods;
        *advs = self.advancements;
        *conditions = self.predicates;
        *registry = self.registry;
        *active = self.packs;
        CONTENT_REVISION.fetch_add(1, Ordering::Release);
    }

    fn read_entry(&mut self, pack: &mut Datapack, path: &str, body: &str) {
        let Some(("data", path)) = path.split_once('/') else {
            return;
        };
        let Some((ns, path)) = path.split_once('/') else {
            return;
        };
        let Some((kind, rel)) = path.split_once('/') else {
            return;
        };
        if let "function" | "functions" = kind {
            if let Some(name) = rel.strip_suffix(".mcfunction") {
                pack.functions.insert(
                    format!("{ns}:{name}"),
                    body.lines()
                        .map(str::trim)
                        .filter(|line| !line.is_empty() && !line.starts_with('#'))
                        .map(str::to_owned)
                        .collect(),
                );
            }
            return;
        }
        let Some(rel) = rel.strip_suffix(".json") else {
            return;
        };
        let key = format!("{ns}:{rel}");
        match kind {
            "tags" => {
                let Some((category, name)) = split_tag_path(rel) else {
                    return;
                };
                if category == "function" {
                    // Function hooks keep their existing wiring; only the
                    // vanilla `load`/`tick` hooks run, arbitrary function tags
                    // are still indexed below in the registry for `execute if`.
                    match name {
                        "load" | "tick" => {
                            let dst = if name == "load" {
                                &mut pack.load
                            } else {
                                &mut pack.tick
                            };
                            if let Some(values) = body.split("\"values\"").nth(1) {
                                dst.extend(
                                    values
                                        .split('"')
                                        .skip(1)
                                        .step_by(2)
                                        .filter(|m| m.starts_with('#') || m.contains(':'))
                                        .map(|m| {
                                            let m = m.trim_start_matches('#');
                                            if m.contains(':') {
                                                m.to_owned()
                                            } else {
                                                format!("minecraft:{m}")
                                            }
                                        }),
                                );
                            }
                        }
                        _ => {}
                    }
                }
                self.read_tag_entry(&category, &format!("{ns}:{name}"), body);
            }
            "recipe" | "recipes" => {
                self.recipe_ids.insert(key.clone());
                if self.read_extended_recipe(&key, body) {
                    return;
                }
                self.read_recipe(body)
            }
            "loot_table" | "loot_tables" => self.read_loot(key, rel, body),
            "item_modifier" | "item_modifiers" => {
                let funcs = parse_loot_funcs(body);
                if !funcs.is_empty() {
                    self.item_mods.insert(key, funcs);
                }
            }
            "advancement" | "advancements" => self.read_advancement(key, rel, body),
            "predicate" | "predicates" => {
                self.predicates.insert(key, body.to_owned());
            }
            _ => {}
        }
    }

    fn read_recipe(&mut self, body: &str) {
        let mk_ingr = |spec: &str| -> Option<Ingr> {
            if let Some(t) = jstr(spec, "tag") {
                let t = t.trim_start_matches('#');
                Some(Ingr::Tag(if t.contains(':') {
                    t.to_string()
                } else {
                    format!("minecraft:{}", t)
                }))
            } else if let Some(n) = jstr(spec, "item") {
                name_to_item(n.trim_start_matches("minecraft:")).map(Ingr::Item)
            } else {
                jstr(spec, "").and(None)
            } // object-less spec unsupported
        };
        let (out_id, out_cnt) = {
            let rblk = jblock(body, "result");
            let id = rblk
                .and_then(|b| jstr(b, "id"))
                .or_else(|| jstr(body, "result"));
            let cnt = rblk
                .and_then(|b| jnum(b, "count"))
                .or_else(|| jnum(body, "count"))
                .unwrap_or(1)
                .clamp(1, 64) as u8;
            (
                id.and_then(|i| name_to_item(i.trim_start_matches("minecraft:"))),
                cnt,
            )
        };
        let Some(out) = out_id else { return };
        if body.contains("crafting_shaped") {
            let mut rows: Vec<String> = Vec::new();
            if let Some(pa) = jarray(body, "pattern") {
                for m in pa.split('"').skip(1).step_by(2) {
                    if !m.is_empty() {
                        rows.push(m.to_string())
                    }
                }
            }
            let mut key: HashMap<char, Ingr> = HashMap::new();
            if let Some(kb) = jblock(body, "key") {
                let inner = &kb[1..kb.len() - 1];
                for entry in inner.split(',') {
                    let Some(col) = entry.find(':') else { continue };
                    let ch = entry[..col]
                        .trim()
                        .trim_matches('"')
                        .chars()
                        .next()
                        .unwrap_or(' ');
                    let spec = entry[col..].trim();
                    if let Some(ig) = mk_ingr(spec) {
                        key.insert(ch, ig);
                    }
                }
            }
            if !rows.is_empty() {
                self.recipes.push(JsonRecipe {
                    shaped: true,
                    rows,
                    key,
                    ing: Vec::new(),
                    out,
                    out_cnt,
                });
            }
        } else if body.contains("crafting_shapeless") {
            let mut ing: Vec<Ingr> = Vec::new();
            if let Some(ia) = jarray(body, "ingredients") {
                for spec in ia.split('{').skip(1) {
                    if let Some(ig) = mk_ingr(spec) {
                        ing.push(ig)
                    }
                }
                if ing.is_empty() {
                    // bare form: ["minecraft:x","minecraft:y"]
                    for m in ia.split('"').skip(1).step_by(2) {
                        if let Some(i) = name_to_item(m.trim_start_matches("minecraft:")) {
                            ing.push(Ingr::Item(i));
                        }
                    }
                }
            }
            if !ing.is_empty() {
                self.recipes.push(JsonRecipe {
                    shaped: false,
                    rows: Vec::new(),
                    key: HashMap::new(),
                    ing,
                    out,
                    out_cnt,
                });
            }
        } else if ["smelting", "blasting", "smoking", "campfire_cooking"]
            .iter()
            .any(|k| body.contains(k))
        {
            // "ingredient": {"item"|"tag":...} or bare "minecraft:x"
            let ig = jblock(body, "ingredient").and_then(mk_ingr).or_else(|| {
                jstr(body, "ingredient")
                    .and_then(|n| name_to_item(n.trim_start_matches("minecraft:")).map(Ingr::Item))
            });
            if let Some(ig) = ig {
                let xp = jstr(body, "experience")
                    .and_then(|v| v.parse::<f32>().ok())
                    .unwrap_or(0.0);
                self.smelts.push((ig, out, out_cnt, xp));
            }
        }
    }

    fn read_loot(&mut self, key: String, rel: &str, body: &str) {
        // #197: keep the raw JSON even when the bounded pool parser below
        // cannot use it, so generic consumers can look a table up by id.
        self.loot_raw.insert(key.clone(), body.to_owned());
        let mut pools: Vec<LootPool> = Vec::new();
        if let Some(pa) = jarray(body, "pools") {
            for pb in jobjects(pa) {
                let rolls = jnum(pb, "rolls")
                    .or_else(|| jblock(pb, "rolls").and_then(|b| jnum(b, "max")))
                    .unwrap_or(1)
                    .clamp(1, 64) as u8;
                let mut chance = 1.0f32;
                if let Some(ca) = jarray(pb, "conditions") {
                    for cb in jobjects(ca) {
                        if cb.contains("random_chance") {
                            if let Some(c) = jnum(cb, "chance") {
                                chance *= c as f32 / 100.0;
                            } else if let Some(c) =
                                jstr(cb, "chance").and_then(|x| x.parse::<f32>().ok())
                            {
                                chance *= c;
                            }
                        }
                    }
                }
                let mut entries: Vec<LootEntry> = Vec::new();
                if let Some(ea) = jarray(pb, "entries") {
                    for eb in jobjects(ea) {
                        if !eb.contains("\"item\"") && !eb.contains("item") {
                            continue;
                        }
                        let Some(n) = jstr(eb, "name") else { continue };
                        let Some(item) = name_to_item(n.trim_start_matches("minecraft:")) else {
                            continue;
                        };
                        let (mut lo, mut hi) = (1u8, 1u8);
                        let mut efuncs: Vec<LootFunc> = Vec::new();
                        if let Some(fa) = jarray(eb, "functions") {
                            for fb in jobjects(fa) {
                                if fb.contains("set_count") {
                                    if let Some(cb) = jblock(fb, "count") {
                                        lo = jnum(cb, "min").unwrap_or(1).clamp(0, 64) as u8;
                                        hi =
                                            jnum(cb, "max").unwrap_or(lo as i64).clamp(0, 64) as u8;
                                    } else if let Some(c) = jnum(fb, "count") {
                                        lo = c.clamp(0, 64) as u8;
                                        hi = lo;
                                    }
                                } else {
                                    efuncs.extend(parse_loot_funcs(fb));
                                }
                            }
                        }
                        entries.push(LootEntry {
                            item,
                            min: lo,
                            max: hi.max(lo),
                            funcs: efuncs,
                        });
                    }
                }
                let pfuncs: Vec<LootFunc> = jarray(pb, "functions")
                    .map(parse_loot_funcs)
                    .unwrap_or_default();
                if !entries.is_empty() {
                    pools.push(LootPool {
                        rolls,
                        chance,
                        entries,
                        funcs: pfuncs,
                    });
                }
            }
        }
        if !pools.is_empty() {
            if let Some(mob) = rel
                .strip_prefix("entities/")
                .and_then(|name| name.rsplit('/').next())
                .and_then(name_to_etype)
            {
                self.mob_loot.insert(
                    mob,
                    pools
                        .iter()
                        .map(|pool| LootPool {
                            rolls: pool.rolls,
                            chance: pool.chance,
                            funcs: pool.funcs.clone(),
                            entries: pool
                                .entries
                                .iter()
                                .map(|entry| LootEntry {
                                    item: entry.item,
                                    min: entry.min,
                                    max: entry.max,
                                    funcs: entry.funcs.clone(),
                                })
                                .collect(),
                        })
                        .collect(),
                );
            }
            self.loot.insert(key, pools);
        }
    }

    fn read_advancement(&mut self, key: String, rel: &str, body: &str) {
        let Some(cb) = jblock(body, "criteria") else {
            return;
        };
        let inner = &cb[1..cb.len() - 1];
        let mut criteria: Vec<(String, String, Option<i64>)> = Vec::new();
        for co in jobjects(inner) {
            // criterion key precedes the object: find its name backwards
            let Some(cpos) = inner.find(co) else { continue };
            let pre = &inner[..cpos];
            let cn = pre
                .rfind('"')
                .and_then(|e2| pre[..e2].rfind('"').map(|s2| &pre[s2 + 1..e2]))
                .unwrap_or("")
                .to_string();
            let tr = jstr(co, "trigger")
                .unwrap_or("")
                .trim_start_matches("minecraft:")
                .to_string();
            let want = jblock(co, "conditions")
                .and_then(|cd| jblock(cd, "entity"))
                .and_then(|en| jstr(en, "type"))
                .and_then(|t| name_to_etype(t.trim_start_matches("minecraft:")));
            criteria.push((cn, tr, want));
        }
        let title = jblock(body, "display")
            .and_then(|d| {
                jblock(d, "title")
                    .and_then(|t| jstr(t, "text").map(|x| x.to_string()))
                    .or_else(|| jstr(d, "title").map(|x| x.to_string()))
            })
            .unwrap_or_else(|| rel.to_owned());
        if !criteria.is_empty() {
            self.advancements
                .insert(key, Advancement { title, criteria });
        }
    }
}

const ZIP_MAX_BYTES: usize = 64 * 1024 * 1024;
const ZIP_MAX_ENTRY_BYTES: usize = 4 * 1024 * 1024;
const ZIP_MAX_TOTAL_BYTES: usize = 32 * 1024 * 1024;
const ZIP_MAX_ENTRIES: usize = 4096;

fn zip_u16(bytes: &[u8], offset: usize) -> usize {
    u16::from_le_bytes(bytes[offset..offset + 2].try_into().unwrap()) as usize
}

fn zip_u32(bytes: &[u8], offset: usize) -> usize {
    u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap()) as usize
}

// ZIP32, single disk. Validate the entire archive before exposing any resources.
fn zip_datapack_entries(bytes: &[u8]) -> Option<Vec<(String, Vec<u8>)>> {
    let end = bytes.len().checked_sub(22)?;
    let eocd = (end.saturating_sub(u16::MAX as usize)..=end)
        .rev()
        .find(|&pos| {
            bytes[pos..].starts_with(b"PK\x05\x06") && zip_u16(bytes, pos + 20) == end - pos
        })?;
    let footer = &bytes[eocd..];
    let count = zip_u16(footer, 10);
    let central_start = zip_u32(footer, 16);
    let mut central = bytes.get(central_start..eocd)?;
    if zip_u32(footer, 4) != 0
        || zip_u16(footer, 8) != count
        || count > ZIP_MAX_ENTRIES
        || zip_u32(footer, 12) != central.len()
    {
        return None;
    }

    let mut total = 0;
    let mut records = Vec::with_capacity(count);
    let mut ranges = Vec::with_capacity(count);
    let mut paths = HashSet::with_capacity(count);
    for _ in 0..count {
        let record = central.get(..46)?;
        if !record.starts_with(b"PK\x01\x02") || zip_u16(record, 34) != 0 {
            return None;
        }
        let flags = zip_u16(record, 8);
        let method = zip_u16(record, 10);
        let crc = zip_u32(record, 16);
        let compressed = zip_u32(record, 20);
        let size = zip_u32(record, 24);
        let name_end = 46 + zip_u16(record, 28);
        let name = central.get(46..name_end)?;
        central = central.get(name_end + zip_u16(record, 30) + zip_u16(record, 32)..)?;
        if !matches!(method, 0 | 8)
            || flags & !(0x0808 | if method == 8 { 6 } else { 0 }) != 0
            || size > ZIP_MAX_ENTRY_BYTES
            || size > ZIP_MAX_TOTAL_BYTES - total
        {
            return None;
        }
        total += size;

        let path = std::str::from_utf8(name).ok()?.replace('\\', "/");
        let plain_path = path.strip_suffix('/').unwrap_or(&path);
        let directory = plain_path.len() != path.len();
        if plain_path.contains(['\0', ':'])
            || plain_path
                .split('/')
                .any(|part| matches!(part, "" | "." | ".."))
            || !paths.insert(plain_path.to_owned())
            || (directory && size != 0)
        {
            return None;
        }

        let offset = zip_u32(record, 42);
        let local = bytes.get(offset..central_start)?;
        let header = local.get(..30)?;
        let local_name_end = 30 + zip_u16(header, 26);
        let data_start = local_name_end + zip_u16(header, 28);
        if !header.starts_with(b"PK\x03\x04")
            || zip_u16(header, 6) != flags
            || zip_u16(header, 8) != method
            || local.get(30..local_name_end)? != name
        {
            return None;
        }
        let descriptor = flags & 8 != 0;
        for (field, value) in [(14, crc), (18, compressed), (22, size)] {
            let local_value = zip_u32(header, field);
            if local_value != value && !(descriptor && local_value == 0) {
                return None;
            }
        }
        let data = local.get(data_start..)?.get(..compressed)?;
        let mut record_end = offset + data_start + compressed;
        if descriptor {
            let tail = bytes.get(record_end..central_start)?;
            let matches = |fields: &[u8]| {
                fields.len() >= 12
                    && zip_u32(fields, 0) == crc
                    && zip_u32(fields, 4) == compressed
                    && zip_u32(fields, 8) == size
            };
            record_end += if tail.strip_prefix(b"PK\x07\x08").is_some_and(matches) {
                16
            } else if matches(tail) {
                12
            } else {
                return None;
            };
        }
        ranges.push((offset, record_end));
        records.push((path, directory, method, crc, size, data));
    }
    ranges.sort_unstable();
    let mut next = 0;
    for (start, end) in ranges {
        if start != next {
            return None;
        }
        next = end;
    }
    if next != central_start || !central.is_empty() {
        return None;
    }

    let mut entries = Vec::with_capacity(count);
    for (path, directory, method, crc, size, data) in records {
        let body = if method == 0 {
            if data.len() != size {
                return None;
            }
            data.to_vec()
        } else {
            let mut decoder = flate2::read::DeflateDecoder::new(data);
            let mut body = Vec::new();
            (&mut decoder)
                .take(size as u64 + 1)
                .read_to_end(&mut body)
                .ok()?;
            if body.len() != size || decoder.total_in() != data.len() as u64 {
                return None;
            }
            body
        };
        let mut checksum = flate2::Crc::new();
        checksum.update(&body);
        if checksum.sum() as usize != crc {
            return None;
        }
        if !directory {
            entries.push((path, body));
        }
    }
    entries.sort_unstable_by(|(a, _), (b, _)| a.cmp(b));
    Some(entries)
}

fn datapack_sources(base: &std::path::Path) -> Vec<(String, std::path::PathBuf, bool)> {
    let Ok(entries) = std::fs::read_dir(base) else {
        return Vec::new();
    };
    let mut sources = Vec::new();
    for entry in entries.flatten() {
        let Some(name) = entry.file_name().into_string().ok() else {
            continue;
        };
        let path = entry.path();
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        let zip = kind.is_file() && path.extension().is_some_and(|ext| ext == "zip");
        if zip
            || (kind.is_dir() && (path.join("pack.mcmeta").is_file() || path.join("data").is_dir()))
        {
            sources.push((name, path, zip));
        }
    }
    sources.sort_unstable_by(|(a, _, _), (b, _, _)| a.cmp(b));
    sources
}

fn read_directory_datapack(
    base: &std::path::Path,
    pack: &mut Datapack,
    content: &mut DatapackContent,
) {
    let mut dirs = vec![base.join("data")];
    let mut files = Vec::new();
    while let Some(dir) = dirs.pop() {
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            let path = entry.path();
            if kind.is_dir() {
                dirs.push(path);
            } else if kind.is_file()
                && path
                    .extension()
                    .is_some_and(|ext| ext == "json" || ext == "mcfunction")
            {
                files.push(path);
            }
        }
    }
    files.sort_unstable();
    for file in files {
        let Ok(path) = file.strip_prefix(base) else {
            continue;
        };
        if let (Some(path), Ok(body)) = (path.to_str(), std::fs::read_to_string(&file)) {
            content.read_entry(pack, &path.replace('\\', "/"), &body);
        }
    }
}

const DATAPACK_DIRECTORY: &str = "datapacks";
static DATAPACK_RELOAD: Mutex<()> = Mutex::new(());
static DISABLED_DATAPACKS: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();

fn disabled_datapacks() -> &'static Mutex<HashSet<String>> {
    DISABLED_DATAPACKS.get_or_init(|| Mutex::new(HashSet::new()))
}

/// #165 — the explicit enabled-pack order vanilla keeps in its pack
/// repository: later packs apply over earlier ones, and `/datapack list
/// enabled` reports them in this sequence.
fn datapack_order() -> &'static Mutex<Vec<String>> {
    static ORDER: OnceLock<Mutex<Vec<String>>> = OnceLock::new();
    ORDER.get_or_init(|| Mutex::new(Vec::new()))
}

/// Where `/datapack enable <name>` files the pack. `first`/`last` insert at
/// the ends; `before`/`after` fail closed when the anchor pack is not enabled.
enum DatapackPlacement {
    First,
    Last,
    Before(String),
    After(String),
}

#[cfg(test)]
fn load_datapacks() {
    reload_datapacks(std::path::Path::new(DATAPACK_DIRECTORY), None, false).unwrap();
}

fn reload_datapacks(
    base: &std::path::Path,
    change: Option<(&str, bool, DatapackPlacement)>,
    run_load_functions: bool,
) -> Result<usize, String> {
    // Load functions can invoke commands, including another reload. Refuse an
    // overlapping reload while the current one publishes and runs its hooks.
    let _reload = DATAPACK_RELOAD
        .try_lock()
        .map_err(|_| "Datapack reload already in progress")?;
    let mut sources = datapack_sources(base);
    let mut disabled = disabled_datapacks().lock().unwrap_or_else(|e| e.into_inner()).clone();
    if let Some((name, enable, _)) = &change {
        if !sources.iter().any(|(candidate, _, _)| candidate == name) {
            return Err(format!("Unknown datapack '{name}'"));
        }
        if *enable && !disabled.remove(*name) {
            return Err(format!("Datapack '{name}' is already enabled"));
        }
        if !*enable && !disabled.insert(name.to_string()) {
            return Err(format!("Datapack '{name}' is already disabled"));
        }
    }
    // #165 — reconcile the explicit enabled-pack order with the set about to
    // load, apply any placement request, then sort the source list so load
    // order (and `list enabled`) matches it. Failing on a bad anchor leaves
    // every store untouched: `disabled` above is still only a local copy.
    {
        let enabled: HashSet<&str> = sources
            .iter()
            .map(|(name, _, _)| name.as_str())
            .filter(|name| !disabled.contains(*name))
            .collect();
        let mut order = datapack_order().lock().unwrap_or_else(|e| e.into_inner());
        order.retain(|name| enabled.contains(name.as_str()));
        for (name, _, _) in &sources {
            if enabled.contains(name.as_str()) && !order.iter().any(|o| o == name) {
                order.push(name.clone());
            }
        }
        if let Some((name, true, placement)) = &change {
            order.retain(|n| n != name);
            match placement {
                DatapackPlacement::First => order.insert(0, name.to_string()),
                DatapackPlacement::Last => order.push(name.to_string()),
                DatapackPlacement::Before(anchor) => {
                    let Some(i) = order.iter().position(|n| n == anchor) else {
                        return Err(format!("Pack '{anchor}' is not enabled"));
                    };
                    order.insert(i, name.to_string());
                }
                DatapackPlacement::After(anchor) => {
                    let Some(i) = order.iter().position(|n| n == anchor) else {
                        return Err(format!("Pack '{anchor}' is not enabled"));
                    };
                    order.insert(i + 1, name.to_string());
                }
            }
        }
        let rank: HashMap<&str, usize> = order
            .iter()
            .enumerate()
            .map(|(i, name)| (name.as_str(), i))
            .collect();
        sources.sort_by_key(|(name, _, _)| rank.get(name.as_str()).copied().unwrap_or(usize::MAX));
    }
    let mut content = DatapackContent::default();
    content
        .registry
        .tags
        .insert("block".to_owned(), builtin_block_tags());
    content
        .registry
        .tags
        .insert("item".to_owned(), builtin_item_tags());
    let mut dim_sources = Vec::new();
    for (name, path, zip) in sources {
        if disabled.contains(&name) {
            continue;
        }
        let mut pack = Datapack {
            name,
            functions: HashMap::new(),
            load: Vec::new(),
            tick: Vec::new(),
        };
        if zip {
            let mut bytes = Vec::new();
            let entries = std::fs::File::open(&path)
                .and_then(|file| file.take(ZIP_MAX_BYTES as u64 + 1).read_to_end(&mut bytes))
                .ok()
                .filter(|&len| len <= ZIP_MAX_BYTES)
                .and_then(|_| zip_datapack_entries(&bytes));
            let Some(entries) = entries else {
                eprintln!(
                    "[spine] rejecting invalid or unsupported datapack {}",
                    path.display()
                );
                continue;
            };
            dim_sources.push(worldgen::DatapackDimensionSource::Entries(
                entries
                    .iter()
                    .filter_map(|(path, body)| {
                        std::str::from_utf8(body)
                            .ok()
                            .map(|body| (path.clone(), body.to_string()))
                    })
                    .collect(),
            ));
            for (path, body) in entries {
                if let Ok(body) = std::str::from_utf8(&body) {
                    content.read_entry(&mut pack, &path, body);
                }
            }
        } else {
            dim_sources.push(worldgen::DatapackDimensionSource::Directory(
                path.clone(),
            ));
            read_directory_datapack(&path, &mut pack, &mut content);
        }
        eprintln!(
            "[spine] datapack {}: {} fn, load={:?}, tick={:?}",
            pack.name,
            pack.functions.len(),
            pack.load,
            pack.tick
        );
        content.packs.push(pack);
    }
    if let Some((name, true, _)) = change {
        if !content.packs.iter().any(|pack| pack.name == name) {
            return Err(format!("Could not load datapack '{name}'"));
        }
    }
    let count = content.packs.len();
    let loads: Vec<String> = content
        .packs
        .iter()
        .flat_map(|pack| pack.load.iter().cloned())
        .collect();
    {
        let mut disabled_packs = disabled_datapacks().lock().unwrap_or_else(|e| e.into_inner());
        content.publish();
        *disabled_packs = disabled;
    }
    if let Err(error) = worldgen::register_datapack_dimensions(&dim_sources, name_to_state)
    {
        eprintln!("[spine] {error}");
    }
    if run_load_functions {
        for function in loads {
            run_function(&function, None, &mut None);
        }
    }
    Ok(count)
}

fn run_datapack_command(args: &[&str]) -> String {
    let base = std::path::Path::new(DATAPACK_DIRECTORY);
    let names: Vec<String> = match args {
        ["list"] | ["list", "enabled"] => packs()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .map(|p| p.name.clone())
            .collect(),
        ["list", "available"] => {
            let disabled = disabled_datapacks().lock().unwrap_or_else(|e| e.into_inner());
            datapack_sources(base)
                .into_iter()
                .filter_map(|(name, _, _)| disabled.contains(&name).then_some(name))
                .collect()
        }
        [action @ ("enable" | "disable"), name, rest @ ..] => {
            let name = name.strip_prefix("file/").unwrap_or(name);
            let enable = *action == "enable";
            // #165 — `enable` takes an optional placement: first|last
            // (default last) or before|after an already-enabled pack.
            let placement = if !enable {
                if !rest.is_empty() {
                    return "usage: /datapack disable <name>".into();
                }
                DatapackPlacement::Last
            } else {
                match rest {
                    [] | ["last"] => DatapackPlacement::Last,
                    ["first"] => DatapackPlacement::First,
                    ["before", anchor] => DatapackPlacement::Before(
                        anchor.strip_prefix("file/").unwrap_or(anchor).to_owned(),
                    ),
                    ["after", anchor] => DatapackPlacement::After(
                        anchor.strip_prefix("file/").unwrap_or(anchor).to_owned(),
                    ),
                    _ => {
                        return "usage: /datapack enable <name> [first|last|before <existing>|after <existing>]".into()
                    }
                }
            };
            return match reload_datapacks(base, Some((name, enable, placement)), true) {
                Ok(count) => format!(
                    "{} datapack '{name}'. Reloading! ({count} pack(s))",
                    if enable { "Enabled" } else { "Disabled" }
                ),
                Err(error) => error,
            };
        }
        _ => return "usage: /datapack list [enabled|available] | enable <name> [first|last|before <existing>|after <existing>] | disable <name>".into(),
    };
    format!("{} packs: {}", names.len(), names.join(", "))
}

// tiny json helpers for datapack files (no serde — dep-free loader)
fn jstr<'a>(body: &'a str, key: &str) -> Option<&'a str> {
    // value of "key": "..." or bare token
    let rest = body.split(&format!("\"{}\"", key)).nth(1)?;
    let rest = rest.trim_start().strip_prefix(':')?.trim_start();
    if let Some(q) = rest.strip_prefix('"') {
        return Some(&q[..q.find('"')?]);
    }
    Some(rest.split([',', '}', ']']).next()?.trim())
}
fn jnum(body: &str, key: &str) -> Option<i64> {
    jstr(body, key)?.parse().ok()
}
fn jblock<'a>(body: &'a str, key: &str) -> Option<&'a str> {
    // balanced-brace block after "key":
    let rest = body.split(&format!("\"{}\"", key)).nth(1)?;
    let start = rest.find('{')?;
    let mut d = 0i32;
    for (i, ch) in rest[start..].char_indices() {
        match ch {
            '{' => d += 1,
            '}' => {
                d -= 1;
                if d == 0 {
                    return Some(&rest[start..start + i + 1]);
                }
            }
            _ => {}
        }
    }
    None
}
// yields each top-level {...} object inside an array body
fn jobjects(arr: &str) -> Vec<&str> {
    let mut v = Vec::new();
    let mut d = 0i32;
    let mut st = 0usize;
    for (i, ch) in arr.char_indices() {
        match ch {
            '{' => {
                if d == 0 {
                    st = i
                }
                d += 1
            }
            '}' => {
                d -= 1;
                if d == 0 {
                    v.push(&arr[st..i + 1])
                }
            }
            _ => {}
        }
    }
    v
}
fn jarray<'a>(body: &'a str, key: &str) -> Option<&'a str> {
    let rest = body.split(&format!("\"{}\"", key)).nth(1)?;
    let start = rest.find('[')?;
    let mut d = 0i32;
    for (i, ch) in rest[start..].char_indices() {
        match ch {
            '[' => d += 1,
            ']' => {
                d -= 1;
                if d == 0 {
                    return Some(&rest[start + 1..start + i]);
                }
            }
            _ => {}
        }
    }
    None
}
fn lookup_function(name: &str) -> Option<Vec<String>> {
    let name = if name.contains(':') {
        name.to_string()
    } else {
        format!("minecraft:{}", name)
    };
    packs()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .find_map(|p| p.functions.get(&name).cloned())
}
fn run_function(name: &str, me: Option<Arc<Client>>, s: &mut Option<&Client>) -> usize {
    run_function_depth(name, me, s, 0, None)
}
/// `/function <name> [with] <compound>` argument parsing: the compound's fields
/// become the macro substitutions `$`-prefixed lines consume.
fn function_macro_args(args: &[&str]) -> Result<Option<BTreeMap<String, String>>, String> {
    let text = match args {
        [] => return Ok(None),
        ["with", rest @ ..] => rest.join(" "),
        rest => rest.join(" "),
    };
    let mut reader = brigadier::StringReader::new(&text);
    let Some(ExecTag::Compound(fields)) = execute_read_snbt(&mut reader, 0) else {
        return Err(format!("Expected macro arguments as a compound, got '{text}'"));
    };
    if reader.can_read() {
        return Err(format!("Unexpected trailing macro arguments '{text}'"));
    }
    let mut out = BTreeMap::new();
    for (name, tag) in fields {
        out.insert(
            String::from_utf16_lossy(&name.0),
            match tag {
                ExecTag::String(s) => String::from_utf16_lossy(&s.0),
                other => execute_tag_snbt(&other),
            },
        );
    }
    Ok(Some(out))
}
fn run_function_depth(
    name: &str,
    me: Option<Arc<Client>>,
    s: &mut Option<&Client>,
    depth: usize,
    macro_args: Option<&BTreeMap<String, String>>,
) -> usize {
    if depth > 16 {
        return 0;
    }
    let Some(lines) = lookup_function(name) else {
        return 0;
    };
    // Vanilla instantiates a macro-bearing function before running it: called
    // without arguments, or with an unbound `$(var)`, the whole call fails and
    // no line runs.
    if lines.iter().any(|line| line.starts_with('$')) {
        let Some(args) = macro_args else {
            return 0;
        };
        let mut expanded = Vec::with_capacity(lines.len());
        for line in &lines {
            expanded.push(if line.starts_with('$') {
                match expand_mcfunction_macro(line, args) {
                    Ok(line) => line,
                    Err(_) => return 0,
                }
            } else {
                line.clone()
            });
        }
        return run_function_lines(&expanded, me, s, depth);
    }
    run_function_lines(&lines, me, s, depth)
}
fn run_function_lines(
    lines: &[String],
    me: Option<Arc<Client>>,
    s: &mut Option<&Client>,
    depth: usize,
) -> usize {
    let mut n = 0;
    for line in lines {
        if let Some(rest) = line.strip_prefix("function ") {
            // `function <name> [with <compound>]` — a nested call may carry its
            // own macro arguments.
            let (inner, compound) = match rest.split_once(" with ") {
                Some((name, compound)) => (name.trim(), Some(compound.trim())),
                None => (rest.trim(), None),
            };
            let parsed = match compound {
                Some(compound) => match function_macro_args(&["with", compound]) {
                    Ok(parsed) => parsed,
                    Err(_) => continue,
                },
                None => None,
            };
            n += run_function_depth(inner, me.clone(), s, depth + 1, parsed.as_ref());
        } else {
            run_command(me.as_ref(), &format!("/{}", line), s);
            n += 1;
            // `/return` ends the calling function's remaining lines like
            // Vanilla's discarded frame.
            if *line == "return" || line.starts_with("return ") {
                break;
            }
        }
    }
    n
}

fn player_command_targets(target: &str, source: Option<&Arc<Client>>) -> Vec<Arc<Client>> {
    let ids: HashSet<i32> = resolve_selector(target, source)
        .into_iter()
        .filter_map(|id| id.parse().ok())
        .collect();
    client_snapshot()
        .into_iter()
        .filter(|c| ids.contains(&c.eid))
        .collect()
}

fn run_experience_command(args: &[&str], source: Option<&Arc<Client>>) -> String {
    if let ["query", target, unit @ ("points" | "levels")] = args {
        let targets = player_command_targets(target, source);
        let [player] = targets.as_slice() else {
            return "Expected one player".into();
        };
        let sessions = player_mechanism_sessions().lock().unwrap_or_else(|e| e.into_inner());
        let Some(session) = sessions.get(&player.eid) else {
            return "Player experience is unavailable".into();
        };
        let xp = PlayerXpState {
            level: session.xp_bar.0,
            progress: session.xp_bar.1,
            total_experience: 0,
        };
        let amount = if *unit == "levels" { xp.query_experience_levels() } else { xp.query_experience_points() };
        return format!("{} has {} experience {}", player.name, amount, unit);
    }
    let [action @ ("add" | "set"), target, amount, units @ ..] = args else {
        return "Usage: /xp add|set <target> <amount> [points|levels]".into();
    };
    let Ok(amount) = amount.parse::<i32>() else {
        return "Invalid experience amount".into();
    };
    let levels = match units {
        [] | ["points"] => false,
        ["levels"] => true,
        _ => return "Expected points or levels".into(),
    };
    if *action == "set" && amount < 0 {
        return "Experience amount must be nonnegative".into();
    }
    let targets = player_command_targets(target, source);
    if targets.is_empty() {
        return "No player found".into();
    }
    let mut changed = 0;
    for c in targets {
        let result = send_player_xp(&c, |xp| {
            if *action == "set" {
                xp.set(
                    amount,
                    if levels {
                        ExperienceUnit::Levels
                    } else {
                        ExperienceUnit::Points
                    },
                )
            } else {
                if levels {
                    xp.give_experience_levels(amount);
                    Ok(())
                } else {
                    add_player_xp_points(xp, amount)
                }
            }
        });
        changed += usize::from(result.is_ok());
    }
    if changed == 0 && *action == "set" {
        "Experience points must be below the amount required for the next level".into()
    } else if changed == 0 {
        "Experience level is too large to add points".into()
    } else {
        format!("{action} xp for {changed} target(s)")
    }
}

// Rules without a dedicated runtime consumer are kept in process-local world state.
const COMMAND_GAMERULE_DEFAULTS: &[(&str, bool, i32)] = &[
    ("announceAdvancements", true, 1),
    ("blockExplosionDropDecay", true, 1),
    ("commandBlockOutput", true, 1),
    ("commandModificationBlockLimit", false, 32768),
    ("disableElytraMovementCheck", true, 0),
    ("disableRaids", true, 0),
    ("doDaylightCycle", true, 1),
    ("doEntityDrops", true, 1),
    ("doFireTick", true, 1),
    ("doImmediateRespawn", true, 0),
    ("doInsomnia", true, 1),
    ("doLimitedCrafting", true, 0),
    ("doMobLoot", true, 1),
    ("doMobSpawning", true, 1),
    ("doPatrolSpawning", true, 1),
    ("doTileDrops", true, 1),
    ("doTraderSpawning", true, 1),
    ("doVinesSpread", true, 1),
    ("doWardenSpawning", true, 1),
    ("doWeatherCycle", true, 1),
    ("drowningDamage", true, 1),
    ("enderPearlsVanishOnDeath", true, 1),
    ("fallDamage", true, 1),
    ("fireDamage", true, 1),
    ("forgiveDeadPlayers", true, 1),
    ("freezeDamage", true, 1),
    ("globalSoundEvents", true, 1),
    ("keepInventory", true, 0),
    ("lavaSourceConversion", true, 0),
    ("logAdminCommands", true, 1),
    ("maxCommandChainLength", false, 65536),
    ("maxCommandForkCount", false, 65536),
    ("maxEntityCramming", false, 24),
    ("mobExplosionDropDecay", true, 1),
    ("mobGriefing", true, 1),
    ("naturalRegeneration", true, 1),
    ("playersNetherPortalCreativeDelay", false, 1),
    ("playersNetherPortalDefaultDelay", false, 80),
    ("playersSleepingPercentage", false, 100),
    ("projectilesCanBreakBlocks", true, 1),
    ("randomTickSpeed", false, 3),
    ("reducedDebugInfo", true, 0),
    ("sendCommandFeedback", true, 1),
    ("showDeathMessages", true, 1),
    ("snowAccumulationHeight", false, 1),
    ("spawnChunkRadius", false, 2),
    ("spawnRadius", false, 10),
    ("spectatorsGenerateChunks", true, 1),
    ("tntExplosionDropDecay", true, 0),
    ("universalAnger", true, 0),
    ("waterSourceConversion", true, 1),
];

fn command_gamerules() -> &'static Mutex<HashMap<&'static str, i32>> {
    static RULES: OnceLock<Mutex<HashMap<&'static str, i32>>> = OnceLock::new();
    RULES.get_or_init(|| Mutex::new(COMMAND_GAMERULE_DEFAULTS.iter()
        .map(|&(name, _, default)| (name, default)).collect()))
}

// Resolve core command targets through the same Brigadier snapshot as /execute.
fn resolve_selector(sel: &str, ctx: Option<&Arc<Client>>) -> Vec<String> {
    let (x, y, z) = ctx.map(|c| {
        let p = *c.pos.lock().unwrap_or_else(|e| e.into_inner());
        (p.0, p.1, p.2)
    }).unwrap_or((0.5, 16.0, 0.5));
    let source = ExecuteSource::new(&CmdCtx { me: ctx.cloned(), x, y, z });
    // Preserve the legacy extension that permits @r[type=...] for non-players.
    let random_entities = sel.strip_prefix("@r[").and_then(|args| args.strip_suffix(']'))
        .filter(|args| args.split(',').any(|part| part.trim_start().starts_with("type=")))
        .map(|args| {
            let sort = if args.split(',').any(|part| part.trim_start().starts_with("sort=")) { "" } else { ",sort=random" };
            let limit = if args.split(',').any(|part| part.trim_start().starts_with("limit=")) { "" } else { ",limit=1" };
            format!("@e[{args}{sort}{limit}]")
        });
    source.select(random_entities.as_deref().unwrap_or(sel)).unwrap_or_default().into_iter().filter_map(|entity| {
        match entity {
            ExecuteEntity::Player(player) => Some(player.eid.to_string()),
            ExecuteEntity::Mob(mob) => Some(format!("m:{}", mob.eid)),
            other @ ExecuteEntity::Other { .. } => {
                let key = other.tag_key();
                matches!(key.as_bytes().first(), Some(b'd' | b't' | b'a' | b'p' | b'b' | b'x')).then_some(key)
            }
        }
    }).collect()
}

// execution context: who + where (for ~ coords)
#[derive(Clone)]
struct CmdCtx {
    me: Option<Arc<Client>>,
    x: f64,
    y: f64,
    z: f64,
}

fn coord(tok: &str, base: f64) -> f64 {
    if let Some(off) = tok.strip_prefix('~') {
        base + off.parse::<f64>().unwrap_or(0.0)
    } else {
        tok.parse::<f64>().unwrap_or(0.0)
    }
}

fn spawn_yaw(token: Option<&str>, source: Option<&Arc<Client>>) -> Option<f32> {
    let Some(token) = token else {
        return Some(0.0);
    };
    let angle = if let Some(relative) = token.strip_prefix('~') {
        let yaw = source.map_or(0.0, |player| player.pos.lock().unwrap_or_else(|e| e.into_inner()).3);
        yaw + if relative.is_empty() {
            0.0
        } else {
            relative.parse::<f32>().ok()?
        }
    } else {
        token.parse::<f32>().ok()?
    };
    if !angle.is_finite() {
        return None;
    }
    let angle = angle % 360.0;
    Some(if angle >= 180.0 {
        angle - 360.0
    } else if angle < -180.0 {
        angle + 360.0
    } else {
        angle
    })
}

// ---------- scoreboard ----------
fn scoreboard_criterion_name(criterion: ScoreboardCriteria) -> &'static str {
    match criterion {
        ScoreboardCriteria::Dummy => "dummy",
        ScoreboardCriteria::Trigger => "trigger",
        ScoreboardCriteria::DeathCount => "deathCount",
        ScoreboardCriteria::PlayerKillCount => "playerKillCount",
        ScoreboardCriteria::TotalKillCount => "totalKillCount",
        ScoreboardCriteria::Health => "health",
    }
}

fn scoreboard() -> &'static Mutex<ScoreboardEngine> {
    static SCORES: OnceLock<Mutex<ScoreboardEngine>> = OnceLock::new();
    SCORES.get_or_init(|| Mutex::new(ScoreboardEngine::default()))
}

fn world_border() -> &'static Mutex<WorldBorderState> {
    static BORDER: OnceLock<Mutex<WorldBorderState>> = OnceLock::new();
    BORDER.get_or_init(|| Mutex::new(WorldBorderState::default()))
}

fn border_now_ms() -> u64 {
    static EPOCH: OnceLock<Instant> = OnceLock::new();
    EPOCH.get_or_init(Instant::now).elapsed().as_millis() as u64
}

fn run_world_border_command(ctx: &CmdCtx, args: &[&str]) -> String {
    let mut border = world_border().lock().unwrap_or_else(|e| e.into_inner());
    let result = WorldBorderCommand::parse(args, ctx.x, ctx.z)
        .and_then(|cmd| cmd.execute(&mut border, None, border_now_ms()));
    let feedback = match result {
        Ok(result) => {
            if let Some((id, body)) = result.packet {
                broadcast(-1, |_| (id, body.clone()));
            }
            result.feedback
        }
        Err(WorldBorderCommandError::Rejected(feedback)) => feedback,
        Err(WorldBorderCommandError::Syntax) => {
            return "Usage: /worldborder add|center|damage|get|set|warning".into()
        }
        Err(error) => return format!("Invalid world border argument: {error:?}"),
    };
    match (feedback.key, feedback.arguments.as_slice()) {
        ("commands.worldborder.set.immediate", [size]) => {
            format!("Set the world border to {size} blocks wide")
        }
        ("commands.worldborder.set.grow" | "commands.worldborder.set.shrink", [size, seconds]) => {
            format!("Resizing the world border to {size} blocks over {seconds} seconds")
        }
        ("commands.worldborder.get", [size]) => format!("The world border is {size} blocks wide"),
        ("commands.worldborder.center.success", [x, z]) => {
            format!("Set the world border center to {x}, {z}")
        }
        ("commands.worldborder.warning.distance.success", [blocks]) => {
            format!("Set the world border warning distance to {blocks} blocks")
        }
        ("commands.worldborder.warning.time.success", [seconds]) => {
            format!("Set the world border warning time to {seconds} seconds")
        }
        ("commands.worldborder.damage.amount.success", [amount]) => {
            format!("Set the world border damage to {amount} per block")
        }
        ("commands.worldborder.damage.buffer.success", [blocks]) => {
            format!("Set the world border damage buffer to {blocks} blocks")
        }
        (key, arguments) => [key, &arguments.join(" ")].join(" ").trim_end().into(),
    }
}

// Also used by the GP-7 packet encoder.
fn display_pkt(slot: i64, obj: &str) -> Vec<u8> {
    let mut p = Vec::new();
    write_varint(&mut p, slot);
    write_str(&mut p, obj);
    p
}

fn sync_scoreboard(w: &mut TcpStream, board: &ScoreboardEngine) -> std::io::Result<()> {
    for objective in board.objectives.values() {
        let (id, body) =
            encode_scoreboard_objective(ScoreboardObjectivePacket::Create(objective, None));
        send_packet(w, id, body)?;
    }
    for (objective, scores) in &board.scores {
        for (owner, score) in scores {
            let (id, body) = encode_scoreboard_score(owner, objective, score.value, None, None);
            send_packet(w, id, body)?;
        }
    }
    for (slot, objective) in board.display_slots.iter().enumerate() {
        if let Some(objective) = objective {
            send_packet(w, 0x57, display_pkt(slot as i64, objective))?;
        }
    }
    Ok(())
}

fn broadcast_score(board: &ScoreboardEngine, owner: &str, objective: &str) {
    if let Some(score) = board.get(owner, objective) {
        let (id, body) = encode_scoreboard_score(owner, objective, score, None, None);
        broadcast(-1, |_| (id, body.clone()));
    }
}

const SCOREBOARD_ENTRY_LIMIT: usize = 4096;

fn scoreboard_entry_available(board: &ScoreboardEngine, owner: &str, objective: &str) -> bool {
    board
        .scores
        .get(objective)
        .is_none_or(|scores| scores.len() < SCOREBOARD_ENTRY_LIMIT || scores.contains_key(owner))
}

fn apply_scoreboard_command(board: &mut ScoreboardEngine, args: &[&str]) -> Result<String, String> {
    // Vanilla accepts every tracked criteria name at parse time. The engine
    // models `health` natively; the remaining tracked aliases (xp, level,
    // food, air, armor) map onto the writable `dummy` criterion so the
    // objective is recognized instead of rejected.
    let mut rewritten_args;
    let args = match args {
        ["objectives", "add", _, criteria, ..]
            if ScoreboardCriteria::parse(criteria).is_none()
                && matches!(*criteria, "xp" | "level" | "food" | "air" | "armor") =>
        {
            rewritten_args = args.to_vec();
            rewritten_args[3] = "dummy";
            &rewritten_args[..]
        }
        _ => args,
    };
    let entries = match args {
        ["players", "operation", owner, objective, _, source, source_objective] => {
            &[(*owner, *objective), (*source, *source_objective)][..]
        }
        ["players", "set" | "add" | "remove" | "enable", owner, objective, ..] => {
            &[(*owner, *objective)][..]
        }
        _ => &[],
    };
    if entries
        .iter()
        .any(|&(owner, objective)| !scoreboard_entry_available(board, owner, objective))
    {
        return Err("Too many scores for objective".into());
    }
    if let [(first, objective), (second, other)] = entries {
        if objective == other
            && first != second
            && board.scores.get(*objective).is_some_and(|scores| {
                scores.len() == SCOREBOARD_ENTRY_LIMIT - 1
                    && !scores.contains_key(*first)
                    && !scores.contains_key(*second)
            })
        {
            return Err("Too many scores for objective".into());
        }
    }
    let reply = match args {
        ["objectives", "modify", name, "displayname", text @ ..] if !text.is_empty() => {
            let Some(objective) = board.objectives.get_mut(*name) else {
                return Err(format!("Unknown objective: {name}"));
            };
            objective.display_name = text.join(" ");
            format!("Changed display name of objective {name}")
        }
        ["objectives", "modify", name, "rendertype", kind] => {
            let render_type = match *kind {
                "integer" => ScoreboardRenderType::Integer,
                "hearts" => ScoreboardRenderType::Hearts,
                _ => return Err(format!("Unknown render type: {kind}")),
            };
            let Some(objective) = board.objectives.get_mut(*name) else {
                return Err(format!("Unknown objective: {name}"));
            };
            objective.render_type = render_type;
            format!("Changed render type of objective {name}")
        }
        ["players", "list"] => {
            let mut holders: Vec<String> = board
                .scores
                .values()
                .flat_map(|scores| scores.keys().cloned())
                .collect();
            holders.sort_unstable();
            holders.dedup();
            format!("{} score holders: {}", holders.len(), holders.join(", "))
        }
        ["players", "list", holder] => {
            let mut entries: Vec<(&str, i32)> = board
                .scores
                .iter()
                .filter_map(|(objective, scores)| {
                    scores.get(*holder).map(|score| (objective.as_str(), score.value))
                })
                .collect();
            entries.sort_unstable_by(|a, b| a.0.cmp(b.0));
            format!(
                "{} scores for {holder}: {}",
                entries.len(),
                entries
                    .iter()
                    .map(|(objective, value)| format!("[{value}] {objective}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        }
        _ => execute_scoreboard_command(board, args)?,
    };
    for &(owner, objective) in entries {
        broadcast_score(board, owner, objective);
    }
    let packet = match args {
        ["objectives", "add", name, ..] => {
            let objective = board.objective(name).unwrap();
            encode_scoreboard_objective(ScoreboardObjectivePacket::Create(objective, None))
        }
        ["objectives", "remove", name] => {
            encode_scoreboard_objective(ScoreboardObjectivePacket::Remove(name))
        }
        ["objectives", "setdisplay", slot, ..] => {
            let slot = ScoreboardDisplaySlot::parse(slot).unwrap();
            encode_scoreboard_display(slot, board.display(slot))
        }
        ["objectives", "modify", name, ..] => {
            // mode 2: display name + render type changed in place.
            encode_scoreboard_objective(ScoreboardObjectivePacket::Update(
                board.objective(name).unwrap(),
                None,
            ))
        }
        ["players", "reset", owner, objective @ ..] => {
            encode_scoreboard_reset(owner, objective.first().copied())
        }
        _ => return Ok(reply),
    };
    broadcast(-1, |_| (packet.0, packet.1.clone()));
    Ok(reply)
}

fn scoreboard_holders(holder: &str, source: Option<&Arc<Client>>) -> Vec<String> {
    if holder.starts_with('@') {
        player_command_targets(holder, source)
            .into_iter()
            .map(|c| c.name.clone())
            .collect()
    } else {
        vec![holder.chars().take(64).collect()]
    }
}

fn run_scoreboard_command(args: &[&str], source: Option<&Arc<Client>>) -> String {
    let ["players", _, holder, ..] = args else {
        return apply_scoreboard_command(&mut scoreboard().lock().unwrap_or_else(|e| e.into_inner()), args)
            .unwrap_or_else(|err| err);
    };
    let holders = scoreboard_holders(holder, source);
    let mut args = args.to_vec();
    let sources = if let ["players", "operation", _, _, _, from, _] = args.as_slice() {
        scoreboard_holders(from, source)
            .into_iter()
            .map(Some)
            .collect()
    } else {
        vec![None]
    };
    if holders.is_empty() || sources.is_empty() {
        return "No score holders found".into();
    }
    let mut replies = Vec::new();
    let mut board = scoreboard().lock().unwrap_or_else(|e| e.into_inner());
    for holder in &holders {
        args[2] = holder;
        for from in &sources {
            if let Some(from) = from {
                args[5] = from;
            }
            replies.push(apply_scoreboard_command(&mut board, &args).unwrap_or_else(|err| err));
        }
    }
    replies.join("; ")
}

fn run_trigger_command(args: &[&str], source: Option<&Arc<Client>>) -> String {
    let Some(player) = source else {
        return "trigger: player only".into();
    };
    let (objective, set, amount) = match args {
        [objective] => (*objective, false, 1),
        [objective, op @ ("set" | "add"), amount] => {
            let Ok(amount) = amount.parse::<i32>() else {
                return "Invalid trigger amount".into();
            };
            (*objective, *op == "set", amount)
        }
        _ => return "Usage: /trigger <objective> [add|set <amount>]".into(),
    };
    let mut board = scoreboard().lock().unwrap_or_else(|e| e.into_inner());
    let delta = if set {
        amount.wrapping_sub(board.get(&player.name, objective).unwrap_or(0))
    } else {
        amount
    };
    match board.trigger_add(&player.name, objective, delta) {
        Ok(value) => {
            broadcast_score(&board, &player.name, objective);
            format!("Triggered {objective}: now {value}")
        }
        Err(error) => scoreboard_command_error(error),
    }
}

fn facing_idx(v: &str) -> u32 {
    ["north", "east", "south", "west", "up", "down"]
        .iter()
        .position(|&x| x == v)
        .unwrap_or(0) as u32
}
fn block_family_range(name: &str) -> Option<(u32, u32)> {
    Some(match name {
        "water" => (80, 95),
        "lava" => (96, 111),
        "wheat" => (4278, 4285),
        "farmland" => (4286, 4293),
        "carrots" => (4294, 4301),
        "piston" => (2011, 2022),
        "sticky_piston" => (1992, 2003),
        "piston_head" => (2023, 2046),
        "tnt" => (2094, 2095),
        "lever" => (5626, 5649),
        "observer" => (12550, 12561),
        "dispenser" => (523, 534),
        "hopper" => (9225, 9234),
        "redstone_block" => (9223, 9223),
        "redstone_wire" => (2978, 4273),
        "iron_door" => (5652, 5715),
        "oak_sign" => (4302, 4333),
        "chest" => (2954, 2977),
        "barrel" => (18408, 18419),
        _ => return None,
    })
}
fn prop_match(name: &str, st: u32, k: &str, v: &str) -> bool {
    match name {
        _ if matches!(name, "wheat" | "carrots") => true, // age handled inline by caller
        "piston" => match k {
            "extended" => ((st - 2011) < 6) == (v == "true"),
            "facing" => (st - 2011) % 6 == facing_idx(v),
            _ => true,
        },
        "sticky_piston" => match k {
            "extended" => ((st - 1992) < 6) == (v == "true"),
            "facing" => (st - 1992) % 6 == facing_idx(v),
            _ => true,
        },
        "piston_head" => match k {
            "facing" => (st - 2023) / 4 == facing_idx(v),
            _ => true,
        },
        "water" => match k {
            "level" => (st - 80) == v.parse().unwrap_or(99),
            _ => true,
        },
        "lever" => match k {
            "powered" => ((st - 5626) & 1 == 0) == (v == "true"),
            _ => true,
        },
        "observer" => match k {
            "facing" => (st - 12550) % 6 == facing_idx(v),
            "powered" => ((st - 12550) / 6) == (v == "true") as u32,
            _ => true,
        },
        "hopper" => match k {
            "enabled" => (st < 9230) == (v == "true"),
            "facing" => {
                (st - 9225) % 5
                    == ["down", "north", "south", "west", "east"]
                        .iter()
                        .position(|&x| x == v)
                        .unwrap_or(0) as u32
            }
            _ => true,
        },
        _ => true,
    }
}
// block tags: vanilla `#namespace:tag` predicates — datapack-loaded
// members merge onto a small built-in set (vanilla replace:false
// semantics default to merging for non-vanilla namespaces)
fn block_tags() -> &'static Mutex<HashMap<String, Vec<String>>> {
    static T: OnceLock<Mutex<HashMap<String, Vec<String>>>> = OnceLock::new();
    T.get_or_init(|| Mutex::new(builtin_block_tags()))
}
fn builtin_block_tags() -> HashMap<String, Vec<String>> {
    [
        (
            "logs",
            &[
                "oak_log",
                "spruce_log",
                "birch_log",
                "jungle_log",
                "acacia_log",
                "dark_oak_log",
                "mangrove_log",
                "cherry_log",
                "crimson_stem",
                "warped_stem",
                "oak_wood",
                "spruce_wood",
                "birch_wood",
                "jungle_wood",
                "acacia_wood",
                "dark_oak_wood",
                "mangrove_wood",
                "cherry_wood",
                "crimson_hyphae",
                "warped_hyphae",
            ][..],
        ),
        (
            "planks",
            &[
                "oak_planks",
                "spruce_planks",
                "birch_planks",
                "jungle_planks",
                "acacia_planks",
                "dark_oak_planks",
                "mangrove_planks",
                "cherry_planks",
                "crimson_planks",
                "warped_planks",
            ][..],
        ),
        (
            "leaves",
            &[
                "oak_leaves",
                "spruce_leaves",
                "birch_leaves",
                "jungle_leaves",
                "acacia_leaves",
                "dark_oak_leaves",
                "mangrove_leaves",
                "cherry_leaves",
                "azalea_leaves",
            ][..],
        ),
        (
            "wool",
            &[
                "white_wool",
                "orange_wool",
                "magenta_wool",
                "light_blue_wool",
                "yellow_wool",
                "lime_wool",
                "pink_wool",
                "gray_wool",
                "light_gray_wool",
                "cyan_wool",
                "purple_wool",
                "blue_wool",
                "brown_wool",
                "green_wool",
                "red_wool",
                "black_wool",
            ][..],
        ),
        ("sand", &["sand", "red_sand"][..]),
        (
            "dirt",
            &[
                "dirt",
                "grass_block",
                "podzol",
                "mycelium",
                "coarse_dirt",
                "rooted_dirt",
                "dirt_path",
                "farmland",
                "mud",
                "muddy_mangrove_roots",
            ][..],
        ),
        (
            "flowers",
            &[
                "dandelion",
                "poppy",
                "allium",
                "cornflower",
                "torchflower",
                "sunflower",
                "lilac",
                "peony",
                "red_tulip",
                "orange_tulip",
                "white_tulip",
                "pink_tulip",
            ][..],
        ),
        ("crops", &["wheat", "carrots", "potatoes", "beetroots"][..]),
    ]
    .iter()
    .map(|(k, v)| {
        (
            format!("minecraft:{}", k),
            v.iter().map(|s| s.to_string()).collect(),
        )
    })
    .collect()
}

// datapack entity loot tables: data/<ns>/loot_table[s]/entities/<mob>.json
// loot functions / item modifiers: the bounded subset with a real
// consumer here. set_count folds into min/max at parse time.
#[derive(Clone)]
// the variants are the vanilla loot-function ids (set_name, set_lore, set_enchantments,
// set_damage); shortening them would hide which function each one is
#[allow(clippy::enum_variant_names)]
enum LootFunc {
    SetName(String),
    SetLore(Vec<String>, bool),        // replace | append
    SetEnchant(Vec<(i64, i64)>, bool), // entries, add?
    SetDamage(f32),                    // fraction of max damage
}
struct LootEntry {
    item: i64,
    min: u8,
    max: u8,
    funcs: Vec<LootFunc>,
}
struct LootPool {
    rolls: u8,
    chance: f32,
    entries: Vec<LootEntry>,
    funcs: Vec<LootFunc>,
}
fn parse_loot_funcs(json: &str) -> Vec<LootFunc> {
    // accepts an object {"function":...} or an array of them
    let t = json.trim();
    let objs: Vec<&str> = if t.starts_with('[') {
        jobjects(t)
    } else {
        vec![t]
    };
    let mut out = Vec::new();
    for o in objs {
        let Some(f) = jstr(o, "function") else {
            continue;
        };
        match f.trim_start_matches("minecraft:") {
            "set_name" => {
                let n = jblock(o, "name")
                    .and_then(|b| jstr(b, "text"))
                    .or_else(|| jstr(o, "name"))
                    .unwrap_or_default();
                if !n.is_empty() {
                    out.push(LootFunc::SetName(n.to_string()))
                }
            }
            "set_lore" => {
                let append = jstr(o, "mode")
                    .map(|m| m.contains("append"))
                    .unwrap_or(false);
                let mut ls = Vec::new();
                if let Some(la) = jarray(o, "lore") {
                    for lb2 in jobjects(la) {
                        let t = jstr(lb2, "text")
                            .map(|x| x.to_string())
                            .unwrap_or_else(|| lb2.trim_matches('"').to_string());
                        if !t.is_empty() {
                            ls.push(t)
                        }
                    }
                }
                out.push(LootFunc::SetLore(ls, append));
            }
            "set_enchantments" => {
                let add = o.contains("\"add\":true") || o.contains("\"add\": true");
                let mut ls = Vec::new();
                if let Some(eb) = jblock(o, "enchantments") {
                    let eb = eb.trim_start_matches('{').trim_end_matches('}');
                    for kv in eb.split(',') {
                        if let Some((en, lv)) = kv.rsplit_once(':') {
                            let en = en.trim().trim_matches('"').trim_start_matches("minecraft:");
                            if let (Some(id), Ok(l)) = (enchant_id(en), lv.trim().parse::<i64>()) {
                                ls.push((id, l.max(1)));
                            }
                        }
                    }
                }
                out.push(LootFunc::SetEnchant(ls, add));
            }
            "set_damage" => {
                // damage is a float fraction of max durability
                let mut val = jnum(o, "damage").map(|d| d as f32);
                if val.is_none() {
                    if let Some(ki) = o.find("\"damage\":") {
                        let tail = &o[ki + 9..];
                        let num: String = tail
                            .chars()
                            .take_while(|c| c.is_ascii_digit() || *c == '.' || *c == '-')
                            .collect();
                        val = num.parse::<f32>().ok();
                    }
                }
                if let Some(d) = val {
                    out.push(LootFunc::SetDamage(d))
                }
            }
            _ => {}
        }
    }
    out
}
fn apply_loot_funcs(st: &mut Stack, funcs: &[LootFunc]) {
    for f in funcs {
        match f {
            LootFunc::SetName(t) => comp_set(&mut st.2, COMP_CUSTOM_NAME, nbt_string(t)),
            LootFunc::SetLore(ls, append) => {
                let mut elems: Vec<Vec<u8>> = Vec::new();
                if *append {
                    if let Some(d) = comp_get(&st.2, COMP_LORE) {
                        let (n, mut p) = read_varint_at(d, 0);
                        for _ in 0..n.min(64) {
                            if p + 3 > d.len() || d[p] != 8 {
                                break;
                            }
                            let ln = u16::from_be_bytes([d[p + 1], d[p + 2]]) as usize;
                            let end = p + 3 + ln;
                            if end > d.len() {
                                break;
                            }
                            // Existing StringTag bytes may contain Java lone surrogates.
                            // Appending lore must not round-trip them through Rust String.
                            elems.push(d[p..end].to_vec());
                            p = end;
                        }
                    }
                }
                elems.extend(ls.iter().map(|text| nbt_string(text)));
                let mut d = Vec::new();
                write_varint(&mut d, elems.len() as i64);
                for tag in &elems {
                    d.extend_from_slice(tag);
                }
                comp_set(&mut st.2, COMP_LORE, d);
            }
            LootFunc::SetEnchant(ls, add) => {
                let mut cur = if *add {
                    enchantments_get(&st.2)
                } else {
                    Vec::new()
                };
                for (id, lv) in ls {
                    cur.retain(|(x, _)| x != id);
                    cur.push((*id, *lv));
                }
                comp_set(&mut st.2, COMP_ENCHANTMENTS, enchantments_data(&cur, true));
            }
            LootFunc::SetDamage(frac) => {
                if let Some(maxd) = tool_max_damage(st.0)
                    .or_else(|| armor_max_damage(st.0))
                    .or_else(|| comp_varint(&st.2, COMP_MAX_DAMAGE))
                {
                    comp_set_varint(&mut st.2, COMP_DAMAGE, (frac * maxd as f32) as i64);
                }
            }
        }
    }
}
// path-keyed tables for /loot (entities/* also mirror into mob_loot_tables)
fn loot_tables() -> &'static Mutex<HashMap<String, Vec<LootPool>>> {
    static R: OnceLock<Mutex<HashMap<String, Vec<LootPool>>>> = OnceLock::new();
    R.get_or_init(|| Mutex::new(HashMap::new()))
}
// #197: raw `data/<ns>/loot_table[s]/**.json` bodies keyed `ns:path`, kept so
// consumers that need fields the pool parser drops can resolve a table by
// resource id. Replaced wholesale on every datapack publish, so /reload and
// datapack enable/disable keep it in step with the parsed registries.
fn raw_loot_tables() -> &'static Mutex<HashMap<String, String>> {
    static R: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();
    R.get_or_init(|| Mutex::new(HashMap::new()))
}
/// The raw JSON body of the loot table `ns:path` (a bare path reads as
/// `minecraft:`), or `None` when no enabled datapack provides it.
// #197 seam: menus_and_items/chest_loot consume this once generic loot-table
// lookup lands; until then only tests exercise it.
#[allow(dead_code)]
fn loot_table_json(id: &str) -> Option<String> {
    let id = if id.contains(':') {
        id.to_owned()
    } else {
        format!("minecraft:{id}")
    };
    raw_loot_tables().lock().unwrap_or_else(|e| e.into_inner()).get(&id).cloned()
}
fn mob_loot_tables() -> &'static Mutex<HashMap<i64, Vec<LootPool>>> {
    static R: OnceLock<Mutex<HashMap<i64, Vec<LootPool>>>> = OnceLock::new();
    R.get_or_init(|| Mutex::new(HashMap::new()))
}
// item modifiers: data/<ns>/item_modifier/*.json -> parsed func list
fn item_mods() -> &'static Mutex<HashMap<String, Vec<LootFunc>>> {
    static R: OnceLock<Mutex<HashMap<String, Vec<LootFunc>>>> = OnceLock::new();
    R.get_or_init(|| Mutex::new(HashMap::new()))
}

// datapack advancements: id -> criteria set (name -> trigger[, entity type])
struct Advancement {
    title: String,
    criteria: Vec<(String, String, Option<i64>)>,
}
fn advancements() -> &'static Mutex<HashMap<String, Advancement>> {
    static R: OnceLock<Mutex<HashMap<String, Advancement>>> = OnceLock::new();
    R.get_or_init(|| Mutex::new(HashMap::new()))
}
// player -> granted adv ids (persisted); player -> adv -> completed criteria
fn adv_done() -> &'static Mutex<HashMap<String, HashSet<String>>> {
    static R: OnceLock<Mutex<HashMap<String, HashSet<String>>>> = OnceLock::new();
    R.get_or_init(|| Mutex::new(HashMap::new()))
}
fn player_advancements() -> &'static Mutex<AdvancementTracker> {
    static TRACKER: OnceLock<Mutex<AdvancementTracker>> = OnceLock::new();
    TRACKER.get_or_init(|| Mutex::new(AdvancementTracker::default()))
}

fn advancement_time_ms() -> i64 {
    // This is the protocol's earned-at timestamp, independent of simulation time.
    std::time::UNIX_EPOCH
        .elapsed()
        .unwrap_or_default()
        .as_millis() as i64
}

fn send_advancement_delta(tracker: &mut AdvancementTracker, c: &Client, reset: bool) {
    if let Some(delta) = tracker.take_delta(&c.name, reset) {
        let (id, body) = delta.into_packet();
        let mut socket = c.sock.lock().unwrap_or_else(|e| e.into_inner());
        if send_packet(&mut socket, id, body).is_err() {
            let _ = socket.shutdown(std::net::Shutdown::Both);
        }
    }
}

fn restore_player_advancements(tracker: &mut AdvancementTracker, name: &str) {
    if let Some(earned) = adv_done().lock().unwrap_or_else(|e| e.into_inner()).get(name) {
        let now = advancement_time_ms();
        for id in earned {
            // A legacy save records only completed IDs. Preserve precise criteria
            // and timestamps when this tracker already has progress for the ID.
            if !tracker
                .progress
                .get(name)
                .is_some_and(|rows| rows.contains_key(id.as_str()))
            {
                tracker.grant_only(name, id, now);
            }
        }
    }
}

fn run_advancement_command(args: &[&str], source: Option<&Arc<Client>>) -> String {
    let [action @ ("grant" | "revoke" | "query"), target, scope @ ..] = args else {
        return "Usage: /advancement grant|revoke|query <target> <scope>".into();
    };
    let targets = player_command_targets(target, source);
    if targets.is_empty() {
        return "No player found".into();
    }
    let mut tracker = player_advancements().lock().unwrap_or_else(|e| e.into_inner());
    if *action == "query" {
        return run_advancement_query(&tracker, &targets, scope);
    }
    let now = advancement_time_ms();
    let mut replies = Vec::new();
    let mut resolved = args.to_vec();
    for c in targets.iter() {
        resolved[1] = &c.name;
        let reply = execute_advancement_command(&mut tracker, &resolved, now);
        let datapack_ids = match scope {
            ["everything"] => advancements()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .keys()
                .filter(|id| tracker.definition(id).is_none())
                .cloned()
                .collect(),
            ["only", id] => {
                let id = if id.contains(':') {
                    (*id).into()
                } else {
                    format!("minecraft:{id}")
                };
                if tracker.definition(&id).is_none()
                    && advancements().lock().unwrap_or_else(|e| e.into_inner()).contains_key(&id)
                {
                    vec![id]
                } else {
                    Vec::new()
                }
            }
            _ => Vec::new(),
        };
        {
            let mut done = adv_done().lock().unwrap_or_else(|e| e.into_inner());
            let earned = done.entry(c.name.clone()).or_default();
            if reply.is_ok() {
                let progress = tracker.progress.get(&c.name);
                for definition in tracker.definitions {
                    // The legacy save records completion independently of parent visibility.
                    let complete = progress
                        .and_then(|rows| rows.get(definition.id))
                        .is_some_and(|criteria| {
                            definition
                                .requirements
                                .iter()
                                .all(|group| group.iter().any(|name| criteria.contains_key(name)))
                        });
                    if complete {
                        earned.insert(definition.id.to_owned());
                    } else {
                        earned.remove(definition.id);
                    }
                }
            }
            for id in &datapack_ids {
                if *action == "grant" {
                    earned.insert(id.clone());
                } else {
                    earned.remove(id);
                }
            }
        }
        send_advancement_delta(&mut tracker, c, false);
        replies.push(match reply {
            Err(_) if !datapack_ids.is_empty() => format!("{action} applied for {}", c.name),
            Ok(text) | Err(text) => text,
        });
    }
    replies.join("; ")
}

struct Bossbar {
    name: String,
    color: u8,
    style: u8,
    value: i32,
    max: i32,
    visible: bool,
    players: HashSet<[u8; 16]>,
}
impl Bossbar {
    fn progress(&self) -> f32 {
        (self.value as f32 / self.max as f32).clamp(0.0, 1.0)
    }

    fn shown_to(&self, player: &Client) -> bool {
        self.visible && self.players.contains(&player.uuid)
    }
}
fn bossbars() -> &'static Mutex<HashMap<String, Bossbar>> {
    static R: OnceLock<Mutex<HashMap<String, Bossbar>>> = OnceLock::new();
    R.get_or_init(|| Mutex::new(HashMap::new()))
}
fn bossbar_uuid(id: &str) -> [u8; 16] {
    // stable uuid: double FNV-ish hash of the id string
    let mut a: u64 = 0xcbf29ce484222325;
    let mut b: u64 = 0x9e3779b97f4a7c15;
    for ch in id.bytes() {
        a = (a ^ ch as u64).wrapping_mul(0x100000001b3);
        b = (b ^ ch as u64).wrapping_mul(0xc6a4a7935bd1e995);
    }
    let mut u = [0u8; 16];
    u[..8].copy_from_slice(&a.to_be_bytes());
    u[8..].copy_from_slice(&b.to_be_bytes());
    u
}
fn bossbar_pkt(id: &str, action: u8, bar: Option<&Bossbar>) -> Vec<u8> {
    let mut p = Vec::new();
    p.extend_from_slice(&bossbar_uuid(id));
    write_varint(&mut p, action as i64);
    match (action, bar) {
        (0, Some(b)) => {
            // add: title, health, color, dividers, flags
            p.extend_from_slice(&chat_nbt(&b.name));
            p.extend_from_slice(&b.progress().to_be_bytes());
            write_varint(&mut p, b.color as i64);
            write_varint(&mut p, b.style as i64);
            p.push(0);
        }
        (2, Some(b)) => p.extend_from_slice(&b.progress().to_be_bytes()),
        (3, Some(b)) => p.extend_from_slice(&chat_nbt(&b.name)),
        (4, Some(b)) => {
            write_varint(&mut p, b.color as i64);
            write_varint(&mut p, b.style as i64);
        }
        _ => {}
    }
    p
}
fn run_bossbar_command(args: &[&str], source: Option<&Arc<Client>>) -> String {
    const USAGE: &str = "usage: /bossbar add|remove|set|list|get";
    let mut bars = bossbars().lock().unwrap_or_else(|e| e.into_inner());
    if args == ["list"] {
        let mut ids: Vec<_> = bars.keys().cloned().collect();
        ids.sort();
        return format!("{} bossbars: {}", ids.len(), ids.join(", "));
    }
    let [operation, raw_id, rest @ ..] = args else {
        return USAGE.into();
    };
    let id = if raw_id.contains(':') {
        raw_id.to_string()
    } else {
        format!("minecraft:{raw_id}")
    };
    if *operation == "add" && !rest.is_empty() {
        if bars.contains_key(&id) {
            return format!("Bossbar '{id}' already exists");
        }
        bars.insert(
            id.clone(),
            Bossbar {
                name: rest.join(" "),
                color: 6,
                style: 0,
                value: 0,
                max: 100,
                visible: true,
                players: HashSet::new(),
            },
        );
        return format!("Created bossbar {id}");
    }
    let Some(bar) = bars.get_mut(&id) else {
        return format!("Unknown bossbar '{id}'");
    };
    let online = client_snapshot();
    let send = |player: &Client, action, bar: Option<&Bossbar>| {
        let _ = send_packet(
            &mut player.sock.lock().unwrap_or_else(|e| e.into_inner()),
            0x0a,
            bossbar_pkt(&id, action, bar),
        );
    };
    match (*operation, rest) {
        ("remove", []) => {
            for player in online.iter().filter(|p| bar.shown_to(p)) {
                send(player, 1, None);
            }
            bars.remove(&id);
            return format!("Removed bossbar {id}");
        }
        ("get", ["max"]) => return format!("Bossbar {id} max is {}", bar.max),
        ("get", ["value"]) => return format!("Bossbar {id} value is {}", bar.value),
        ("get", ["visible"]) => return format!("Bossbar {id} visible is {}", bar.visible),
        ("get", ["players"]) => {
            let mut names: Vec<_> = online
                .iter()
                .filter(|p| bar.players.contains(&p.uuid))
                .map(|p| p.name.as_str())
                .collect();
            names.sort_unstable();
            // The count leads the sentence so `execute_legacy_result` returns
            // it exactly — `/execute store result` sees the vanilla
            // player-count result rather than a bare success flag.
            return format!(
                "{} player(s) on bossbar {id}: {}",
                names.len(),
                names.join(", ")
            );
        }
        ("set", ["players", targets @ ..]) if targets.len() <= 1 => {
            let players = if let Some(selector) = targets.first() {
                let selected = resolve_selector(selector, source);
                let players: HashSet<_> = online
                    .iter()
                    .filter(|p| selected.iter().any(|eid| *eid == p.eid.to_string()))
                    .map(|p| p.uuid)
                    .collect();
                if players.is_empty() {
                    return "No players found".into();
                }
                players
            } else {
                HashSet::new()
            };
            let old = std::mem::replace(&mut bar.players, players);
            if bar.visible {
                for player in &online {
                    match (
                        old.contains(&player.uuid),
                        bar.players.contains(&player.uuid),
                    ) {
                        (false, true) => send(player, 0, Some(bar)),
                        (true, false) => send(player, 1, None),
                        _ => {}
                    }
                }
            }
        }
        ("set", ["visible", "true" | "false"]) => {
            let visible = rest[1] == "true";
            if bar.visible != visible {
                bar.visible = visible;
                for player in online.iter().filter(|p| bar.players.contains(&p.uuid)) {
                    send(player, if visible { 0 } else { 1 }, Some(bar));
                }
            }
        }
        ("set", [field @ ("value" | "max"), number]) => {
            let min = i32::from(*field == "max");
            let Ok(number) = number.parse::<i32>() else {
                return format!("{field} must be an integer of at least {min}");
            };
            if number < min {
                return format!("{field} must be an integer of at least {min}");
            }
            let previous = bar.progress();
            if *field == "max" {
                bar.max = number;
            } else {
                bar.value = number;
            }
            if bar.progress() != previous {
                for player in online.iter().filter(|p| bar.shown_to(p)) {
                    send(player, 2, Some(bar));
                }
            }
        }
        ("set", ["name", name @ ..]) if !name.is_empty() => {
            bar.name = name.join(" ");
            for player in online.iter().filter(|p| bar.shown_to(p)) {
                send(player, 3, Some(bar));
            }
        }
        ("set", [field @ ("color" | "style"), value]) => {
            if *field == "color" {
                let colors = ["pink", "blue", "red", "green", "yellow", "purple", "white"];
                let Some(color) = colors.iter().position(|name| name == value) else {
                    return "Unknown bossbar color".into();
                };
                bar.color = color as u8;
            } else {
                let styles = [
                    "progress",
                    "notched_6",
                    "notched_10",
                    "notched_12",
                    "notched_20",
                ];
                let Some(style) = styles.iter().position(|name| name == value) else {
                    return "Unknown bossbar style".into();
                };
                bar.style = style as u8;
            }
            for player in online.iter().filter(|p| bar.shown_to(p)) {
                send(player, 4, Some(bar));
            }
        }
        _ => return USAGE.into(),
    }
    format!("Updated bossbar {id}")
}

fn adv_prog() -> &'static Mutex<Advancements> {
    static R: OnceLock<Mutex<Advancements>> = OnceLock::new();
    R.get_or_init(|| Mutex::new(HashMap::new()))
}
// event -> mark matching criteria, grant when ALL criteria done, announce
/// Event name → the (advancement id, criterion name) pairs that can fire on it.
///
/// `adv_event` used to scan every loaded advancement (and every criterion of each) on
/// every event, and the tick loop fires the `tick` event once per player per tick.
/// Measured: `env/effects` — which is `effects::tick_effects` → `adv_event(name,
/// "tick")` — was 8.87 ms of a 9.3 ms tick body under one moving viewer, i.e. most of
/// the tick thread's CPU, for a lookup that can be a map hit.
///
/// The rest of the scan only exists to find advancements whose criteria all hold; an
/// advancement can only newly complete on an event that matches one of its criteria, so
/// visiting the matching ones is equivalent. The index follows the datapack revision.
type AdvancementEventIndex = HashMap<String, Vec<(String, String)>>;

fn adv_event(pname: &str, event: &str, etype: Option<i64>) {
    static INDEX: Mutex<Option<(u64, AdvancementEventIndex)>> = Mutex::new(None);
    let __t = tickprof::enabled().then(std::time::Instant::now);
    let mut grants: Vec<String> = Vec::new();
    {
        let advs = advancements().lock().unwrap_or_else(|e| e.into_inner());
        let revision = content_revision();
        let mut index = INDEX.lock().unwrap_or_else(|e| e.into_inner());
        if index.as_ref().is_none_or(|(cached, _)| *cached != revision) {
            let mut by_event = AdvancementEventIndex::new();
            for (id, advancement) in advs.iter() {
                for (criterion, trigger, _) in &advancement.criteria {
                    by_event
                        .entry(trigger.clone())
                        .or_default()
                        .push((id.clone(), criterion.clone()));
                }
            }
            *index = Some((revision, by_event));
        }
        let Some(candidates) = index.as_ref().and_then(|(_, by_event)| by_event.get(event)) else {
            return;
        };
        if adv_done()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(pname)
            .map(|g| g.len() >= advs.len() && advs.keys().all(|id| g.contains(id)))
            .unwrap_or(false)
        {
            if let Some(t) = __t {
                tickprof::add("adv/total", t.elapsed());
            }
            return;
        }
        let mut prog = adv_prog().lock().unwrap_or_else(|e| e.into_inner());
        let pp = prog.entry(pname.to_string()).or_default();
        // Only the advancements that can fire on this event: their criteria are the
        // only ones this call can complete. The completion test still walks *all* of an
        // advancement's criteria, so a partially satisfied one is judged as before.
        let done = adv_done().lock().unwrap_or_else(|e| e.into_inner());
        let granted = done.get(pname);
        // (advancement id -> the criteria this event satisfied), so an advancement
        // reached by several criteria is only walked once.
        let mut touched: HashMap<&str, ()> = HashMap::new();
        for (id, cn) in candidates {
            if granted.is_some_and(|g| g.contains(id)) {
                continue;
            }
            let Some(a) = advs.get(id) else { continue };
            if a.criteria
                .iter()
                .any(|(_, tr, want)| tr == event && (want.is_none() || *want == etype))
            {
                let p = pp.entry(id.clone()).or_default();
                for (cn2, tr, want) in &a.criteria {
                    if tr == event && (want.is_none() || *want == etype) {
                        p.insert(cn2.clone());
                    }
                }
                touched.insert(id.as_str(), ());
                let _ = cn;
            }
        }
        drop(done);
        for id in touched.keys() {
            let Some(a) = advs.get(*id) else { continue };
            let p = pp.entry((*id).to_string()).or_default();
            if !a.criteria.is_empty() && a.criteria.iter().all(|(cn, _, _)| p.contains(cn)) {
                grants.push((*id).to_string());
            }
        }
    }
    if let Some(t) = __t {
        tickprof::add("adv/total", t.elapsed());
    }
    for id in grants {
        {
            let mut tracker = player_advancements().lock().unwrap_or_else(|e| e.into_inner());
            let inserted = adv_done()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .entry(pname.to_string())
                .or_default()
                .insert(id.clone());
            if !inserted {
                continue;
            }
            if tracker.grant_only(pname, &id, advancement_time_ms()) > 0 {
                for c in client_snapshot().iter().filter(|c| c.name == pname) {
                    send_advancement_delta(&mut tracker, c, false);
                }
            }
        }
        let t = advancements()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&id)
            .map(|a| a.title.clone())
            .unwrap_or(id.clone());
        if GR_ANNOUNCE_ADVANCEMENTS.load(Ordering::Relaxed) != 0 {
            let line = system_chat(&format!("{} has made the advancement [{}]", pname, t));
            broadcast(-1, |_c| (0x6c, line.clone()));
        }
    }
}

fn check_modification_bounds(dim: DimId, (x, y, z): BlockPos) -> Result<(), String> {
    if (-30_000_000..30_000_000).contains(&x)
        && (-30_000_000..30_000_000).contains(&z)
        && (dim.min_y()..dim.min_y() + dim.height()).contains(&y)
    {
        Ok(())
    } else {
        Err("Block position is outside the world".into())
    }
}

fn block_modification_error(error: BlockModificationError) -> String {
    match error {
        BlockModificationError::VolumeLimit { volume, limit } => {
            format!("Too many blocks ({volume}, maximum {limit})")
        }
        BlockModificationError::DestinationOverflow => {
            "Clone destination is outside the world".into()
        }
        BlockModificationError::OverlappingClone => "Clone regions overlap".into(),
    }
}

#[cfg(feature = "neoforge-events")]
fn neoforge_block_state(name: &str) -> Option<u32> {
    java_compat::NEOFORGE
        .get()?
        .registry
        .runtime_id(landing_types::ModRegistry::Block, name)
}

fn modification_block_state(argument: &str) -> Option<u32> {
    let Some(registry) = breg() else {
        return state_from_arg(argument);
    };
    let Some((name, properties)) = argument.split_once('[') else {
        #[cfg(feature = "neoforge-events")]
        if let Some(state) = neoforge_block_state(argument) {
            return Some(state);
        }
        return registry.checked_state(argument, &[]);
    };
    let properties = properties.strip_suffix(']')?;
    if properties.is_empty() {
        return registry.checked_state(name, &[]);
    }
    let properties = properties
        .split(',')
        .map(|entry| {
            let (key, value) = entry.split_once('=')?;
            Some((key.to_owned(), value.to_owned()))
        })
        .collect::<Option<Vec<_>>>()?;
    registry.checked_state(name, &properties)
}

fn run_block_modification(ctx: &CmdCtx, cmd: &str, args: &[&str]) -> Result<String, String> {
    let origin = (
        ctx.x.floor() as i32,
        ctx.y.floor() as i32,
        ctx.z.floor() as i32,
    );
    let dim = cmd_dim(ctx);
    let read = |(x, y, z)| block_at_dim(dim, x, y, z);
    // commandModificationBlockLimit is a live gamerule: /fill and /clone read
    // it here, not the compiled default.
    let mod_limit = GR_COMMAND_MODIFICATION_BLOCK_LIMIT
        .load(Ordering::Relaxed)
        .clamp(1, i32::MAX) as usize;
    with_save_guard(|| {
        let changes = if cmd == "fill" {
            let fill = parse_fill_command(args, origin, modification_block_state)?;
            check_modification_bounds(dim, fill.from)?;
            check_modification_bounds(dim, fill.to)?;
            eval_fill(
                fill.from,
                fill.to,
                fill.state,
                fill.mode,
                mod_limit,
                read,
            )
        } else {
            let clone = parse_clone_command(args, origin, modification_block_state)?;
            check_modification_bounds(dim, clone.from)?;
            check_modification_bounds(dim, clone.to)?;
            check_modification_bounds(dim, clone.destination)?;
            let source = ModificationBox::between(clone.from, clone.to);
            let destination = ModificationBox::between(clone.destination, clone.destination);
            let far_corner = destination
                .at_offset(source.offset(source.max))
                .ok_or_else(|| {
                    block_modification_error(BlockModificationError::DestinationOverflow)
                })?;
            check_modification_bounds(dim, far_corner)?;
            eval_clone(
                clone.from,
                clone.to,
                clone.destination,
                clone.filter,
                clone.mode,
                mod_limit,
                |pos| {
                    let state = read(pos);
                    if clone.filter == CloneFilter::Masked
                        && breg().is_some_and(|reg| reg.is_air(state))
                    {
                        COMMAND_AIR_STATE
                    } else {
                        state
                    }
                },
            )
        }
        .map_err(block_modification_error)?;
        let count = changes.len();
        for BlockModification {
            pos: (x, y, z),
            state,
            destroyed,
        } in changes
        {
            if let Some(previous) = destroyed {
                set_block_dim(dim, x, y, z, COMMAND_AIR_STATE);
                // Item entities currently belong to the Overworld item simulation.
                // /fill destroy honors doTileDrops the way vanilla's
                // dropResources check does.
                if dim == DimId::Overworld && GR_DO_TILE_DROPS.load(Ordering::Relaxed) != 0 {
                    let mut drops = drops().lock().unwrap_or_else(|e| e.into_inner());
                    let remaining = DROP_CAP.saturating_sub(drops.len());
                    drops.extend(
                        block_drops(previous)
                            .into_iter()
                            .take(remaining)
                            .map(|item| Drop {
                                eid: NEXT_EID.fetch_add(1, Ordering::SeqCst),
                                x: f64::from(x) + 0.5,
                                y: f64::from(y) + 0.5,
                                z: f64::from(z) + 0.5,
                                item,
                                cnt: 1,
                                comps: Vec::new(),
                                age: 0,
                                target: None,
                                thrower: None,
                            }),
                    );
                }
            }
            place_block_dim(dim, x, y, z, state);
        }
        let action = if cmd == "fill" { "Filled" } else { "Cloned" };
        Ok(format!("{action} {count} blocks"))
    })
}

/// #97/#167 — attach the `{snbt}` payload `/setblock` parsed to the block
/// entity that owns it: `Items` lists file into the container store the chunk
/// packet serializes, and the typed block-entity kinds (beacon, lectern,
/// jukebox, banner, …) take the whole tag through the live-entity restore path.
fn setblock_attach_nbt(dim: DimId, x: i32, y: i32, z: i32, tag: ExecTag) {
    let ExecTag::Compound(mut fields) = tag else {
        return;
    };
    if let Some(items) = fields.remove(&NbtString::text("Items")) {
        if let Ok(slots) = items_nbt_to_stacks(&items) {
            if !slots.is_empty() {
                chests()
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(ContainerKey::Block((dim, x, y, z)), slots);
            }
        }
    }
    let Some(reg) = breg() else { return };
    let state = block_at_dim(dim, x, y, z);
    let Some(id) = live_block_entity_id(reg.name_of(state)) else {
        return;
    };
    block_entity_put(&mut fields, "id", block_entity_string(id));
    for (key, value) in [("x", x), ("y", y), ("z", z)] {
        block_entity_put(&mut fields, key, ExecTag::Int(value));
    }
    restore_live_block_entity((dim, x, y, z), &ExecTag::Compound(fields));
}

#[cfg(feature = "neoforge-events")]
fn run_neoforge_command(
    runtime: &java_compat::NeoForgeRuntime,
    ctx: &CmdCtx,
    literal: &str,
) -> Result<String, String> {
    let source = ctx.me.as_deref();
    let dim = cmd_dim(ctx);
    let pos = (
        ctx.x.floor() as i32,
        ctx.y.floor() as i32,
        ctx.z.floor() as i32,
    );
    check_modification_bounds(dim, pos)?;
    let outcome = runtime.event(java_compat::S1Event::Command {
        dim: dim.index() as u8,
        pos,
        name: source.map_or("Rcon", |client| client.name.as_str()).into(),
        literal: literal.into(),
    })?;
    let report = java_compat::apply_s1_outcome(outcome)?;
    let mut reply = Vec::new();
    let mut messages = Vec::new();
    for (eid, message) in report.system_messages {
        // The Java shim creates its synthetic command sender with entity ID 0.
        match (eid, source) {
            (0, Some(client)) => messages.push((client.eid, message)),
            (0, None) => reply.push(message),
            _ => messages.push((eid, message)),
        }
    }
    deliver_neoforge_messages(source, messages);
    Ok(reply.join("\n"))
}

/// Vanilla `TimeArgument`: a non-negative number with an optional `t`/`s`/`d`
/// suffix (1s = 20t, 1d = 24000t; a bare number is already ticks). Returns
/// `None` on anything the vanilla parser would reject — a malformed number,
/// a negative value, or a non-finite result.
fn time_argument_ticks(time: &str) -> Option<i64> {
    let (num, mult) = time
        .strip_suffix('t')
        .map(|n| (n, 1.0f64))
        .or_else(|| time.strip_suffix('s').map(|n| (n, TICKS_PER_SECOND as f64)))
        .or_else(|| time.strip_suffix('d').map(|n| (n, 24_000.0)))
        .unwrap_or((time, 1.0));
    let value = num.parse::<f64>().ok()?;
    (value.is_finite() && value >= 0.0).then(|| (value * mult) as i64)
}

fn schedule_delay_ticks(time: &str) -> u64 {
    let (num, mult) = time
        .strip_suffix('t')
        .map(|n| (n, 1.0f64))
        .or_else(|| time.strip_suffix('s').map(|n| (n, TICKS_PER_SECOND as f64)))
        .or_else(|| time.strip_suffix('d').map(|n| (n, 24_000.0)))
        .unwrap_or((time, 1.0));
    (num.parse::<f64>().unwrap_or(1.0) * mult) as u64
}

/// #96/#173 — vanilla `/weather` duration: a `TimeArgument` whose `t`/`s`/`d`
/// suffixes keep their usual tick meanings, but whose bare number is SECONDS
/// (the 1.21.1 parser reads a count of seconds and multiplies by 20 — not the
/// 4-tick quarter-second some ports use).
fn weather_time_argument(value: &str) -> Option<i64> {
    let (num, mult) = value
        .strip_suffix('t')
        .map(|n| (n, 1.0f64))
        .or_else(|| value.strip_suffix('s').map(|n| (n, TICKS_PER_SECOND as f64)))
        .or_else(|| value.strip_suffix('d').map(|n| (n, 24_000.0)))
        .unwrap_or((value, TICKS_PER_SECOND as f64));
    let parsed = num.parse::<f64>().ok()?;
    (parsed.is_finite() && parsed >= 0.0).then(|| (parsed * mult) as i64)
}

/// The duration vanilla rolls when `/weather` omits `<duration>`: uniform in
/// `ServerLevel.RAIN_DELAY` (clear), `RAIN_DURATION`, or `THUNDER_DURATION`.
/// Drawn from the seeded world stream so a replayed run sees the same rolls —
/// never the wall clock.
fn weather_default_ticks(mode: &str) -> i64 {
    let (lo, hi) = match mode {
        "rain" => (12_000u32, 24_000u32),
        "thunder" => (3_600, 15_600),
        _ => (12_000, 180_000),
    };
    i64::from(lo + world_rng::next_below(hi - lo + 1))
}

#[cfg(test)]
fn weather_duration_ticks(seconds: Option<&str>) -> i32 {
    seconds
        .and_then(weather_time_argument)
        .and_then(|v| i32::try_from(v).ok())
        .unwrap_or(6000)
}

fn add_world_time(current: u64, ticks: i64) -> u64 {
    current.wrapping_add(ticks as u64)
}

fn schedule_function(
    scheduled: &mut Vec<(String, u64)>,
    function: &str,
    due_tick: u64,
    append: bool,
) {
    if !append {
        scheduled.retain(|(name, _)| name != function);
    }
    scheduled.push((function.to_string(), due_tick));
}

fn clear_scheduled_functions(scheduled: &mut Vec<(String, u64)>, function: Option<&str>) -> usize {
    let before = scheduled.len();
    if let Some(function) = function {
        scheduled.retain(|(name, _)| name != function);
    } else {
        scheduled.clear();
    }
    before - scheduled.len()
}

fn run_command(me: Option<&Arc<Client>>, msg: &str, s: &mut Option<&Client>) {
    let ctx = match me {
        Some(c) => {
            let p = *c.pos.lock().unwrap_or_else(|e| e.into_inner());
            CmdCtx {
                me: Some(c.clone()),
                x: p.0,
                y: p.1,
                z: p.2,
            }
        }
        None => CmdCtx {
            me: None,
            x: 0.5,
            y: 16.0,
            z: 0.5,
        },
    };
    run_command_ctx(&ctx, msg, s);
}
fn command_allowed_for_player(cmd: &str, args: &[&str]) -> bool {
    command_allowed_at_level(cmd, 0)
        || (cmd == "time" && matches!(args.first().copied(), Some("query" | "get")))
}
fn command_usage(name: &str) -> String {
    // Exact usage for every literal `native_command_dispatcher` registers.
    // Anything missing here is not a command the server knows, so /help
    // reports it as unknown rather than pretending a usage line exists.
    let usage = match name.trim_start_matches('/') {
        "advancement" => "/advancement grant|revoke|query <target> everything|from|through|until|only <id> [<criterion>]",
        "attribute" => "/attribute <target> <attribute> get|base get|base set|modifier add|modifier remove|modifier value get ...",
        "ban" => "/ban <player> [<reason>]",
        "ban-ip" => "/ban-ip <target> [<reason>]",
        "banlist" => "/banlist [ips|players]",
        "bossbar" => "/bossbar add <id> <name> | remove <id> | list | set <id> name|color|style|value|max|visible|players [<targets>] | get <id> max|players|value|visible",
        "clear" => "/clear [<target>] [<item>] [<count>]",
        "clone" => "/clone <from> <to> <destination> [replace|masked|filtered <block>] [force|move|normal]",
        "counts" => "no arguments; debug entity/store census",
        "damage" => "/damage <target> <amount> [<damageType>] [at <pos>|by <entity> [from <cause>]]",
        "data" => "/data get|merge|modify|remove block <x> <y> <z>|entity <target>|storage <id> [<path>] [<nbt|source|scale>]",
        "datapack" => "/datapack list [enabled|available] | enable <name> | disable <name>",
        "debug" => "/debug start|stop | function <name>",
        "defaultgamemode" => "/defaultgamemode <survival|creative|adventure|spectator>",
        "deop" => "/deop <player>",
        "difficulty" => "/difficulty [peaceful|easy|normal|hard]",
        "effect" => "/effect give <target> <effect> [infinite|<seconds>] [<amplifier>] [<hideParticles>] | clear [<target>] [<effect>]",
        "enchant" => "/enchant <target> <enchantment> [<level>]",
        "execute" => "/execute <subcommand> ... run <command>",
        "experience" | "xp" => "/experience add|set <player> <amount> [points|levels] | query <player> points|levels",
        "fill" => "/fill <from> <to> <block> [replace|destroy|keep|hollow|outline]",
        "fillbiome" => "/fillbiome <from> <to> <biome> [replace <filter>]",
        "forceload" => "/forceload add|remove <x> <z> [<x2> <z2>] | remove all | query [<x> <z>]",
        "function" => "/function <name> [<arguments>]",
        "gamemode" => "/gamemode <survival|creative|adventure|spectator> [player]",
        "gamerule" => "/gamerule <rule> [<value>]",
        "give" => "/give <player> <item> [<count>]",
        "help" => "/help [<command>]",
        "item" => "/item modify|replace <target> <slot> ...",
        "jfr" => "/jfr start|stop",
        "kick" => "/kick <player> [<reason>]",
        "kill" => "/kill [<targets>]",
        "list" => "no arguments; list online players",
        "locate" => "/locate structure|biome|poi <id>",
        "loot" => "/loot spawn|give|insert|replace <target> <source>",
        "me" => "/me <action>",
        "msg" | "tell" | "w" => "/msg <player> <message>",
        "op" => "/op <player>",
        "pardon" => "/pardon <player>",
        "pardon-ip" => "/pardon-ip <target>",
        "particle" => "/particle <particle> [<x> <y> <z> [<dx> <dy> <dz> [<speed> [<count> [<mode> [<viewers>]]]]]]",
        "perf" => "/perf start|stop",
        "place" => "/place feature|template <id> [<pos>]",
        "player" => "/player <name> spawn|look|attack|use|stop|kill",
        "profile" => "/profile [<top_n>|reset]",
        "playsound" => "/playsound <sound> [<source>] [<target>] [<pos>] [<volume>] [<pitch>] [<minVolume>]",
        "publish" => "/publish [<allowCommands>] [<gamemode>] [<port>]",
        "random" => "/random value|roll <range> [<sequence>] | reset <sequence> [*]",
        "recipe" => "/recipe give|take <target> <recipe|*>",
        "reload" => "no arguments; reload datapacks",
        "return" => "/return [<value>] | fail",
        "ride" => "/ride <target> mount <vehicle> | /ride <target> dismount",
        "save-on" | "save-off" => "no arguments",
        "save-all" | "save" => "no arguments; force a world save",
        "say" => "/say <message>",
        "schedule" => "/schedule function <id> <time>[t|s|d] [append|replace] | /schedule clear [<id>]",
        "scoreboard" => "/scoreboard objectives add|remove|list|setdisplay|modify | players set|add|remove|reset|get|list|enable|operation",
        "seed" => "no arguments; print the world seed",
        "setblock" => "/setblock <x> <y> <z> <block> [replace|destroy|keep]",
        "setidletimeout" => "/setidletimeout <minutes>",
        "setworldspawn" => "/setworldspawn [x y z] [angle]",
        "spawnpoint" => "/spawnpoint [player] [x y z] [angle]",
        "spectate" => "/spectate [<target>] [<player>]",
        "spreadplayers" => "/spreadplayers <x> <z> <spreadDistance> <maxRange> [under <maxHeight>] <respectTeams> <targets>",
        "stop" => "no arguments; stop the server",
        "stopsound" => "/stopsound [<target>] [<source>] [<sound>]",
        "summon" => "/summon <entity> [<x> <y> <z>] [<nbt>]",
        "tag" => "/tag <target> add|remove <tag> | list",
        "team" => "/team list [<team>] | add|remove|empty <team> | join <team> [<members>] | leave <members> | modify <team> <option> [<value>]",
        "teammsg" | "tm" => "/teammsg <message>",
        "teleport" | "tp" => "/teleport [<target>] <destination|x> [<y> <z>] [<rotation>]",
        "tellraw" => "/tellraw <target> <json message>",
        "tick" => "/tick query|rate|freeze|unfreeze|step|sprint ...",
        "time" => "/time add <time>[t|s|d] | set <day|noon|night|midnight|ticks> | query <day|daytime|gametime>",
        "title" => "/title <target> title|subtitle|actionbar|times|clear|reset ...",
        "transfer" => "/transfer <hostname> [<port>] [<players>]",
        "trigger" => "/trigger <objective> [add|set <value>]",
        "weather" => "/weather clear|rain|thunder [<duration>]",
        "whitelist" => "/whitelist on|off|list|add|remove|reload [<player>]",
        "worldborder" => "/worldborder center|add|set|damage|get|warning|buffer ...",
        "?" => "/? [<command>]",
        _ => return "Unknown command or insufficient permissions".into(),
    };
    if usage.starts_with('/') {
        format!("Usage: {usage}")
    } else {
        format!("Usage: /{} {usage}", name.trim_start_matches('/'))
    }
}
fn allow_unsafe_player_admin() -> bool {
    std::env::var("MC_SPINE_ALLOW_PLAYER_ADMIN_COMMANDS")
        .ok()
        .as_deref()
        == Some("1")
}

const OPS_FILE: &str = "ops.json";
type Operators = std::collections::BTreeSet<String>;
static OPERATORS: Mutex<Operators> = Mutex::new(Operators::new());

/// The leveled view of `ops.json`: `OpsRegistry` accepts both the legacy flat
/// name array (read as level-4 operators, what the old schema meant) and the
/// Vanilla object form. `OPERATORS` stays the plain name set the older helpers
/// and fixtures already know; the two are written together.
fn ops_registry() -> &'static Mutex<OpsRegistry> {
    static OPS: OnceLock<Mutex<OpsRegistry>> = OnceLock::new();
    OPS.get_or_init(|| Mutex::new(OpsRegistry::default()))
}

fn ban_registry() -> &'static Mutex<BanRegistry> {
    static BANS: OnceLock<Mutex<BanRegistry>> = OnceLock::new();
    BANS.get_or_init(|| Mutex::new(BanRegistry::default()))
}

fn whitelist_registry() -> &'static Mutex<WhitelistRegistry> {
    static LIST: OnceLock<Mutex<WhitelistRegistry>> = OnceLock::new();
    LIST.get_or_init(|| Mutex::new(WhitelistRegistry::default()))
}

/// The recipe ids every loaded datapack declared (`data/<ns>/recipe/**.json`),
/// published with the same content swap as `json_recipes`.
fn recipe_ids() -> &'static Mutex<std::collections::BTreeSet<String>> {
    static IDS: OnceLock<Mutex<std::collections::BTreeSet<String>>> = OnceLock::new();
    IDS.get_or_init(|| Mutex::new(std::collections::BTreeSet::new()))
}

fn valid_player_name(name: &str) -> bool {
    (1..=16).contains(&name.len()) && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

/// Vanilla permission level `0..=4` for a player name: `4` for the console and
/// the unsafe test escape, the operator entry's level otherwise.
fn player_permission_level(name: &str) -> u8 {
    if allow_unsafe_player_admin() {
        return 4;
    }
    let level = ops_registry().lock().unwrap_or_else(|e| e.into_inner()).op_level_of(name);
    if level > 0 {
        return level;
    }
    // The legacy name set is written by `set_operator` alongside the registry;
    // a name present only there is still a level-4 operator.
    if OPERATORS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .contains(&name.to_ascii_lowercase())
    {
        return 4;
    }
    0
}

fn player_is_admin(name: &str) -> bool {
    // The server's offline login claims a name. The same case folding is used
    // by the login reservation and operator list. Vanilla's `hasPermission(2)`
    // is the gate that splits cheat commands from the everyone set.
    player_permission_level(name) >= 2
}

fn set_difficulty(difficulty: u8) {
    if difficulty > 3 {
        return;
    }
    DIFFICULTY.store(difficulty, Ordering::Relaxed);
    if difficulty == 0 {
        let gone: Vec<i32> = {
            let mut mobs = mobs().lock().unwrap_or_else(|e| e.into_inner());
            let gone: Vec<i32> = mobs
                .iter()
                .filter(|mob| {
                    entity_category_1211(mob.etype) == EntityMobCategory::Monster
                        && !matches!(mob.etype, 31 | 119) // Ender Dragon and Wither persist in Peaceful.
                })
                .map(|mob| mob.eid)
                .collect();
            mobs.retain(|mob| !gone.contains(&mob.eid));
            gone
        };
        for eid in gone {
            broadcast_seen_destroy(eid);
        }
    }
    let packet = vec![difficulty, 0]; // difficulty, difficulty-locked
    for client in client_snapshot() {
        if let Ok(mut sock) = client.sock.lock() {
            let _ = send_packet(&mut sock, 0x0b, packet.clone());
        }
    }
}

#[cfg(test)]
fn read_operators(path: &std::path::Path) -> std::io::Result<Operators> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Operators::new()),
        Err(error) => return Err(error),
    };
    let invalid = || {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "expected a JSON array of player names",
        )
    };
    let entries = text
        .trim()
        .strip_prefix('[')
        .and_then(|s| s.strip_suffix(']'))
        .ok_or_else(invalid)?
        .trim();
    if entries.is_empty() {
        return Ok(Operators::new());
    }
    entries
        .split(',')
        .map(|entry| {
            let name = entry
                .trim()
                .strip_prefix('"')
                .and_then(|s| s.strip_suffix('"'))
                .filter(|s| valid_player_name(s))
                .ok_or_else(invalid)?;
            Ok(name.to_ascii_lowercase())
        })
        .collect()
}

fn load_operators(path: &std::path::Path) -> std::io::Result<()> {
    // `OpsRegistry::load` accepts both the Vanilla object schema and the legacy
    // flat name array, so it is the single parse both views are fed from.
    let mut operators = OPERATORS.lock().unwrap_or_else(|e| e.into_inner());
    match OpsRegistry::load(path) {
        Ok(registry) => {
            *operators = registry
                .entries
                .iter()
                .map(|e| e.name.to_ascii_lowercase())
                .collect();
            *ops_registry().lock().unwrap_or_else(|e| e.into_inner()) = registry;
            Ok(())
        }
        Err(error) => {
            operators.clear();
            ops_registry().lock().unwrap_or_else(|e| e.into_inner()).entries.clear();
            Err(error)
        }
    }
}

fn set_operator(path: &std::path::Path, name: &str, enable: bool) -> std::io::Result<bool> {
    static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);
    let mut operators = OPERATORS.lock().unwrap_or_else(|e| e.into_inner());
    let mut proposed = operators.clone();
    let name = name.to_ascii_lowercase();
    let changed = if enable {
        proposed.insert(name.clone())
    } else {
        proposed.remove(&name)
    };
    if !changed {
        return Ok(false);
    }
    let temp = path.with_extension(format!(
        "json.{}.{}.tmp",
        std::process::id(),
        NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
    ));
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temp)?;
    let body = proposed
        .iter()
        .map(|name| format!("\"{name}\""))
        .collect::<Vec<_>>()
        .join(", ");
    let written = writeln!(file, "[{body}]").and_then(|_| file.sync_all());
    drop(file);
    if let Err(error) = written.and_then(|_| std::fs::rename(&temp, path)) {
        let _ = std::fs::remove_file(&temp);
        return Err(error);
    }
    *operators = proposed;
    {
        let mut registry = ops_registry().lock().unwrap_or_else(|e| e.into_inner());
        if enable {
            registry.set(&name, 4, false);
        } else {
            registry.remove(&name);
        }
    }
    Ok(true)
}

type PlayCommandBuilder = brigadier::ArgumentBuilder<()>;

#[derive(Default)]
struct CommandCompletions {
    players: Vec<String>,
    operators: Vec<String>,
    enabled_packs: Vec<String>,
    disabled_packs: Vec<String>,
}

fn completion_argument(
    name: &str,
    parser: brigadier::StringArgumentType,
    candidates: Option<&[String]>,
) -> PlayCommandBuilder {
    let argument = PlayCommandBuilder::argument(name, Box::new(parser)).executes(|_| Ok(0));
    if let Some(candidates) = candidates {
        let names = candidates.to_vec();
        argument.suggests(Arc::new(move |_, builder| {
            let prefix = builder.remaining().to_ascii_lowercase();
            for name in &names {
                if name.to_ascii_lowercase().starts_with(&prefix) {
                    builder.suggest(name);
                }
            }
            Ok(builder.build())
        }))
    } else {
        argument
    }
}

#[cfg(test)]
fn command_tree(admin: bool) -> brigadier::CommandNode<()> {
    command_tree_at_level(if admin { 4 } else { 0 })
}

/// The literal tree Vanilla's 1.21.1 `Commands` packet sends a source at
/// `level`: only literals whose `hasPermission` gate the level passes.
fn command_tree_at_level(level: u8) -> brigadier::CommandNode<()> {
    command_dispatcher_at_level(level, None).root().clone()
}

fn command_dispatcher_at_level(
    level: u8,
    completions: Option<&CommandCompletions>,
) -> brigadier::CommandDispatcher<()> {
    let dispatcher = native_command_dispatcher(level, completions);
    #[cfg(feature = "neoforge-events")]
    if let Some(runtime) = java_compat::NEOFORGE.get() {
        for name in &runtime.commands {
            dispatcher.register(PlayCommandBuilder::literal(name))
                .expect("validated NeoForge command literal");
        }
    }
    dispatcher
}

#[cfg(feature = "neoforge-events")]
fn exclude_native_neoforge_literals(literals: &mut Vec<String>) {
    let dispatcher = native_command_dispatcher(4, None);
    literals.retain(|name| dispatcher.root().child(name).is_none());
}

fn native_command_dispatcher(
    level: u8,
    completions: Option<&CommandCompletions>,
) -> brigadier::CommandDispatcher<()> {
    // Execution remains in run_command_ctx. Executable nodes describe the
    // syntax and allow the client to distinguish complete from partial input.
    let dispatcher = brigadier::CommandDispatcher::new();
    let literal = |name: &str| PlayCommandBuilder::literal(name);
    let done = |name: &str| literal(name).executes(|_| Ok(0));
    let choices = |node: PlayCommandBuilder, names: &[&str]| {
        names
            .iter()
            .fold(node, |parent, name| parent.then(done(name)))
    };
    let word = brigadier::StringArgumentType::word;
    let greedy = brigadier::StringArgumentType::greedy_string;
    let args = |name: &str| completion_argument(name, greedy(), None);
    let message = || completion_argument("message", greedy(), None);
    let player =
        || completion_argument("player", word(), completions.map(|c| c.players.as_slice()));
    let mut time = literal("time");
    for query in ["query", "get"] {
        time = time.then(choices(done(query), &["daytime", "gametime", "day"]));
    }
    if level >= 2 {
        let ticks = PlayCommandBuilder::argument(
            "ticks",
            Box::new(brigadier::LongArgumentType::long_at_least(0)),
        )
        .executes(|_| Ok(0));
        time =
            time.then(choices(literal("set"), &["day", "night", "noon", "midnight"]).then(ticks));
        time = time.then(
            literal("add")
                .then(PlayCommandBuilder::argument(
                    "amount",
                    Box::new(brigadier::LongArgumentType::long_arg()),
                ).executes(|_| Ok(0))),
        );
    }
    let register = |node: PlayCommandBuilder| {
        dispatcher
            .register(node)
            .expect("play command is a literal");
    };
    // Every literal below this point obeys Vanilla's per-command
    // `hasPermission(level)` gate, so a level-2 operator never sees the
    // level-3/4 literals its `required_permission_level` does not pass.
    let register_at_level = |name: &str, node: PlayCommandBuilder| {
        if command_allowed_at_level(name, level) {
            register(node);
        }
    };
    let help = literal("help").executes(|_| Ok(0)).then(completion_argument("command", word(), None));
    for node in [help, done("list"), time] {
        register(node);
    }
    register(done("me").then(message()));
    for name in ["msg", "tell", "w"] {
        register(literal(name).then(player().then(message())));
    }
    for name in ["teammsg", "tm"] {
        register(literal(name).then(message()));
    }
    register(literal("trigger").then(args("arguments")));
    if level < 2 {
        return dispatcher;
    }
    // `/?` is the help alias the issue tracker asks for. It keeps the default
    // level-2 gate (the pinned `help_reports_specific_usage_and_alias_is_read_only`
    // contract), so only operators see it advertised.
    register_at_level(
        "?",
        literal("?").executes(|_| Ok(0)).then(completion_argument("command", word(), None)),
    );
    for name in ["counts", "reload", "save", "save-all", "save-on", "save-off", "seed", "stop"] {
        register_at_level(name, done(name));
    }
    register_at_level(
        "kick",
        literal("kick").then(player().then(message())),
    );
    register_at_level(
        "setidletimeout",
        literal("setidletimeout").then(PlayCommandBuilder::argument(
            "minutes",
            Box::new(brigadier::IntegerArgumentType::integer_at_least(0)),
        ).executes(|_| Ok(0))),
    );
    register(["survival", "creative", "adventure", "spectator", "0", "1", "2", "3"]
        .iter().fold(literal("defaultgamemode"), |node, mode| node.then(done(mode))));
    register_at_level("op", literal("op").then(player()));
    register_at_level(
        "deop",
        literal("deop").then(completion_argument(
            "player",
            word(),
            completions.map(|c| c.operators.as_slice()),
        )),
    );
    // Vanilla level-3 moderation literals.
    for name in ["ban", "pardon"] {
        register_at_level(
            name,
            literal(name).then(player().then(message())),
        );
    }
    for name in ["ban-ip", "pardon-ip"] {
        register_at_level(
            name,
            literal(name).then(args("target").then(message())),
        );
    }
    register_at_level(
        "banlist",
        choices(done("banlist"), &["ips", "players"]),
    );
    register_at_level(
        "whitelist",
        choices(
            literal("whitelist"),
            &["on", "off", "list", "add", "remove", "reload"],
        )
        .then(player()),
    );
    // Vanilla level-2 literals wired this lane.
    register_at_level(
        "team",
        literal("team").then(args("arguments")),
    );
    for name in ["locate", "random", "return", "recipe"] {
        register_at_level(name, literal(name).then(args("arguments")));
    }
    // P3 commands are permission level 2 (vanilla `requires(2)`) — exposed to
    // ops through `register_at_level` like every other cheat command.
    for name in ["fillbiome", "place", "ride", "spectate"] {
        register_at_level(name, literal(name).then(args("arguments")));
    }
    register_at_level(
        "tick",
        choices(
            literal("tick"),
            &["query", "rate", "freeze", "unfreeze", "step", "sprint"],
        )
        .then(args("arguments")),
    );
    register(
        literal("datapack")
            .then(choices(done("list"), &["enabled", "available"]))
            .then(literal("enable").then(completion_argument(
                "name",
                greedy(),
                completions.map(|c| c.disabled_packs.as_slice()),
            )))
            .then(literal("disable").then(completion_argument(
                "name",
                greedy(),
                completions.map(|c| c.enabled_packs.as_slice()),
            ))),
    );
    register(choices(
        done("difficulty"),
        &["peaceful", "easy", "normal", "hard"],
    ));
    register(
        done("weather")
            .then(done("clear"))
            .then(
                done("rain").then(
                    PlayCommandBuilder::argument(
                        "duration",
                        Box::new(brigadier::IntegerArgumentType::integer()),
                    )
                    .executes(|_| Ok(0)),
                ),
            )
            .then(
                done("thunder").then(
                    PlayCommandBuilder::argument(
                        "duration",
                        Box::new(brigadier::IntegerArgumentType::integer()),
                    )
                    .executes(|_| Ok(0)),
                ),
            ),
    );
    register(
        [
            "survival",
            "creative",
            "adventure",
            "spectator",
            "0",
            "1",
            "2",
            "3",
        ]
        .iter()
        .fold(literal("gamemode"), |node, mode| {
            node.then(done(mode).then(player()))
        }),
    );
    register(done("kill").then(completion_argument(
        "targets",
        greedy(),
        completions.map(|c| c.players.as_slice()),
    )));
    register(
        literal("give")
            .then(player().then(args("item and count")))
            .then(args("targets, item and count")),
    );
    register(literal("say").then(args("message")));
    register(literal("function").then(args("function")));
    // The remaining legacy handlers accept shapes such as selectors, relative
    // coordinates and NBT which the protocol module cannot serialize as types.
    for name in [
        "advancement",
        "attribute",
        "bossbar",
        "clone",
        "damage",
        "data",
        "effect",
        "enchant",
        "execute",
        "experience",
        "fill",
        "forceload",
        "item",
        "loot",
        "particle",
        "player",
        "playsound",
        "schedule",
        "scoreboard",
        "setblock",
        "spreadplayers",
        "stopsound",
        "summon",
        "tag",
        "teleport",
        "tellraw",
        "title",
        "tp",
        "worldborder",
        "xp",
    ] {
        register_at_level(name, literal(name).then(args("arguments")));
    }
    for name in ["clear", "gamerule", "setworldspawn", "spawnpoint"] {
        register_at_level(name, done(name).then(args("arguments")));
    }
    // Vanilla's `/transfer` literal sits at level 3; `/profile` is the owner
    // diagnostic PERF-1's profiler feeds at level 4. `register_at_level` keys
    // on `required_permission_level` — which this lane cannot narrow — so both
    // gates are explicit here and enforced again inside dispatch.
    if level >= 3 {
        register(literal("transfer").then(args("arguments")));
    }
    if level >= 4 {
        register(literal("profile").then(args("arguments")));
    }
    // P8c cluster-C owner commands; `required_permission_level` already maps
    // all four to 4, so `register_at_level` advertises them at level 4 only.
    for name in ["debug", "jfr", "perf", "publish"] {
        register_at_level(name, literal(name).then(args("arguments")));
    }
    dispatcher
}

fn send_commands(socket: &mut TcpStream, admin: bool) -> std::io::Result<()> {
    send_commands_at_level(socket, if admin { 4 } else { 0 })
}

fn send_commands_at_level(socket: &mut TcpStream, level: u8) -> std::io::Result<()> {
    let payload = brigadier_wire::commands_payload(&command_tree_at_level(level))
        .map_err(std::io::Error::other)?;
    send_packet(
        socket,
        i64::from(brigadier_wire::CLIENTBOUND_COMMANDS),
        payload,
    )
}

fn update_client_commands(client: &Client) {
    let mut socket = client.sock.lock().unwrap_or_else(|e| e.into_inner());
    let _ = send_commands_at_level(&mut socket, player_permission_level(&client.name));
}

fn command_suggestion_response(
    request: &brigadier_wire::CommandSuggestionsRequest,
    level: u8,
    completions: &CommandCompletions,
) -> brigadier_wire::CommandSuggestionsResponse {
    let dispatcher = command_dispatcher_at_level(level, Some(completions));
    let mut reader = brigadier::StringReader::new(&request.text);
    if request.text.starts_with('/') {
        reader.skip();
    }
    let parse = dispatcher.parse_reader(reader, ());
    let suggestions = dispatcher.get_completion_suggestions(&parse);
    let mut response = brigadier_wire::CommandSuggestionsResponse::from_suggestions(
        request.transaction_id,
        &suggestions,
    );
    // Brigadier uses UTF-8 byte offsets internally. The client replaces a
    // substring of a Java String using UTF-16 indices.
    let end = response.start + response.length;
    response.length = request.text[response.start..end].encode_utf16().count();
    response.start = request.text[..response.start].encode_utf16().count();
    response
}

fn reply_to_command_suggestion(client: &Client, body: &[u8]) {
    let Ok(request) = brigadier_wire::parse_command_suggestions_request(body) else {
        return;
    };
    let level = player_permission_level(&client.name);
    let mut completions = CommandCompletions {
        players: client_snapshot().iter().map(|c| c.name.clone()).collect(),
        ..CommandCompletions::default()
    };
    if level >= 2 {
        completions.operators = ops_registry()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .names()
            .iter()
            .map(|name| name.to_string())
            .collect();
        completions.enabled_packs = packs()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .map(|p| p.name.clone())
            .collect();
        completions.disabled_packs = disabled_datapacks()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .cloned()
            .collect();
    }
    let response = command_suggestion_response(&request, level, &completions);
    if let Ok(payload) = brigadier_wire::command_suggestions_response_payload(&response) {
        let _ = send_packet(
            &mut client.sock.lock().unwrap_or_else(|e| e.into_inner()),
            i64::from(brigadier_wire::CLIENTBOUND_COMMAND_SUGGESTIONS),
            payload,
        );
    }
}

// ---------- /spreadplayers ----------
// Vanilla 1.21.1 SpreadPlayersCommand: targets are grouped into piles (one
// per scoreboard team when respectTeams holds, otherwise one per entity),
// the piles relax under the world RNG until every pair sits at least
// spreadDistance apart inside the center +/- maxRange square, and each pile
// then snaps to the safe surface. Team lookup goes through `p3b_team_of` —
// players key by name and mobs by their string UUID, matching the member
// keys `/team join` stores.
const SPREAD_MAX_ITERATIONS: usize = 10_000;

fn spreadplayers_f64() -> f64 {
    // Top 53 bits of the world stream -> uniform [0, 1).
    (world_rng::next_u64() >> 11) as f64 / ((1u64 << 53) as f64)
}

fn strict_coord(tok: &str, base: f64) -> Option<f64> {
    let v = if let Some(off) = tok.strip_prefix('~') {
        match off.parse::<f64>() {
            Ok(d) => base + d,
            Err(_) if off.is_empty() => base,
            Err(_) => return None,
        }
    } else if tok.starts_with('^') {
        return None;
    } else {
        tok.parse::<f64>().ok()?
    };
    v.is_finite().then_some(v)
}

fn strict_num(tok: &str) -> Option<f64> {
    tok.parse::<f64>().ok().filter(|v| v.is_finite())
}

enum SpreadTarget {
    Player(Arc<Client>),
    Mob(Arc<Mob>),
}

fn spreadplayers_team(target: &SpreadTarget) -> Option<String> {
    // Vanilla Entity.getTeam: players look up their name, every other entity
    // its string UUID — the same keys TeamRegistry.member_team holds.
    let member = match target {
        SpreadTarget::Player(client) => client.name.clone(),
        SpreadTarget::Mob(mob) => uuid_text(&mob.uuid),
    };
    p3b_team_of(&member)
}

fn spreadplayers_hazardous(reg: &block_registry::BReg, st: u32) -> bool {
    reg.is_liquid(st)
        || matches!(
            reg.name_of(st),
            "minecraft:fire" | "minecraft:soul_fire" | "minecraft:bubble_column"
        )
}

// Feet Y for a pile at block column (x, z), or None when the column offers no
// safe surface. Normal mode takes the highest motion-blocking block; `under`
// additionally requires the block to sit below maxHeight with two air blocks
// above it. A liquid or fire cell at feet/head height invalidates the column.
fn spreadplayers_ground_y(dim: DimId, x: i32, z: i32, under: Option<i32>) -> Option<i32> {
    let reg = breg()?;
    let min_y = dim.min_y();
    let top = min_y + dim.height() - 1;
    let mut y = match under {
        Some(h) => h.saturating_sub(1).min(top),
        None => top,
    };
    if y < min_y {
        return None;
    }
    while y >= min_y {
        let st = block_at_dim(dim, x, y, z);
        if reg.is_solid(st) {
            let feet = block_at_dim(dim, x, y + 1, z);
            let head = block_at_dim(dim, x, y + 2, z);
            let clear = if under.is_some() {
                reg.is_air(feet) && reg.is_air(head)
            } else {
                !spreadplayers_hazardous(reg, feet) && !spreadplayers_hazardous(reg, head)
            };
            if clear {
                return Some(y + 1);
            }
        }
        y -= 1;
    }
    None
}

fn spreadplayers_command(ctx: &CmdCtx, args: &[&str], me: Option<&Arc<Client>>) -> String {
    const USAGE: &str = "usage: /spreadplayers <x> <z> <spreadDistance> <maxRange> [under <maxHeight>] <respectTeams> <targets...>";
    let (Some(cx), Some(cz)) = (
        args.first().and_then(|t| strict_coord(t, ctx.x)),
        args.get(1).and_then(|t| strict_coord(t, ctx.z)),
    ) else {
        return USAGE.into();
    };
    let (Some(spread), Some(max_range)) = (
        args.get(2).and_then(|t| strict_num(t)),
        args.get(3).and_then(|t| strict_num(t)),
    ) else {
        return USAGE.into();
    };
    if spread < 0.0 {
        return "Distance is too small.".into();
    }
    // Vanilla requires maxRange >= spreadDistance + 1 so at least one valid
    // position can exist inside the square.
    if max_range < spread + 1.0 {
        return "Max range is too small.".into();
    }
    let dim = cmd_dim(ctx);
    let mut idx = 4;
    let under = if args.get(idx) == Some(&"under") {
        idx += 1;
        let Some(h) = args.get(idx).and_then(|t| t.parse::<i64>().ok()) else {
            return USAGE.into();
        };
        if !(i64::from(dim.min_y())..=i64::from(dim.min_y() + dim.height())).contains(&h) {
            return "Invalid max height".into();
        }
        idx += 1;
        Some(h as i32)
    } else {
        None
    };
    let Some(&respect_teams) = args.get(idx) else {
        return USAGE.into();
    };
    let respect_teams = match respect_teams {
        "true" => true,
        "false" => false,
        other => return format!("'{other}' is not true or false"),
    };
    idx += 1;
    if idx >= args.len() {
        return USAGE.into();
    }
    let cls = client_snapshot();
    let mob_snap = mobs().lock().unwrap_or_else(|e| e.into_inner()).clone();
    let mut targets: Vec<SpreadTarget> = Vec::new();
    for token in &args[idx..] {
        for eid in resolve_selector(token, me) {
            if let Ok(pid) = eid.parse::<i32>() {
                if let Some(c) = cls.iter().find(|c| c.eid == pid) {
                    if *c.dim.lock().unwrap_or_else(|e| e.into_inner()) == dim {
                        targets.push(SpreadTarget::Player(c.clone()));
                    }
                }
            } else if let Some(mid) = eid
                .strip_prefix("m:")
                .and_then(|e| e.parse::<i32>().ok())
            {
                if let Some(m) = mob_snap.iter().find(|m| m.eid == mid) {
                    if m.dim == dim {
                        targets.push(SpreadTarget::Mob(m.clone()));
                    }
                }
            }
        }
    }
    if targets.is_empty() {
        return "No entity found".into();
    }

    // Piles: respectTeams collapses members sharing a team key into one pile;
    // members without a team stay alone in their own pile.
    struct Pile {
        x: f64,
        z: f64,
        members: Vec<usize>,
    }
    let min_x = cx - max_range;
    let max_x = cx + max_range;
    let min_z = cz - max_range;
    let max_z = cz + max_range;
    let mut piles: Vec<Pile> = Vec::new();
    let mut team_piles: HashMap<String, usize> = HashMap::new();
    for (index, target) in targets.iter().enumerate() {
        if respect_teams {
            if let Some(team) = spreadplayers_team(target) {
                if let Some(&slot) = team_piles.get(&team) {
                    piles[slot].members.push(index);
                    continue;
                }
                team_piles.insert(team, piles.len());
            }
        }
        piles.push(Pile {
            x: min_x + spreadplayers_f64() * (max_x - min_x),
            z: min_z + spreadplayers_f64() * (max_z - min_z),
            members: vec![index],
        });
    }

    // Relaxation pass (vanilla spreadPositions): repelled piles step one unit
    // away from their close neighbours' centroid and clamp into the square;
    // a fully settled pass still has to find a safe surface per pile.
    let mut iterations = 0usize;
    let mut settled = false;
    while iterations < SPREAD_MAX_ITERATIONS && !settled {
        iterations += 1;
        let mut moved = false;
        for i in 0..piles.len() {
            let (px, pz) = (piles[i].x, piles[i].z);
            let mut close = 0usize;
            let mut rx = 0.0f64;
            let mut rz = 0.0f64;
            for j in 0..piles.len() {
                if i == j {
                    continue;
                }
                let dx = piles[j].x - px;
                let dz = piles[j].z - pz;
                if dx * dx + dz * dz < spread * spread {
                    close += 1;
                    rx += dx;
                    rz += dz;
                }
            }
            if close > 0 {
                let len = (rx * rx + rz * rz).sqrt();
                let mut nx = px / close as f64;
                let mut nz = pz / close as f64;
                if len > 0.0 {
                    nx -= rx / len;
                    nz -= rz / len;
                } else {
                    nx = min_x + spreadplayers_f64() * (max_x - min_x);
                    nz = min_z + spreadplayers_f64() * (max_z - min_z);
                }
                let nx = nx.clamp(min_x, max_x);
                let nz = nz.clamp(min_z, max_z);
                if nx != piles[i].x || nz != piles[i].z {
                    piles[i].x = nx;
                    piles[i].z = nz;
                    moved = true;
                }
            }
        }
        if !moved {
            let mut rerolled = false;
            for pile in piles.iter_mut() {
                let gx = pile.x.floor() as i32;
                let gz = pile.z.floor() as i32;
                if spreadplayers_ground_y(dim, gx, gz, under).is_none() {
                    pile.x = min_x + spreadplayers_f64() * (max_x - min_x);
                    pile.z = min_z + spreadplayers_f64() * (max_z - min_z);
                    rerolled = true;
                }
            }
            if !rerolled {
                settled = true;
            }
        }
    }
    if !settled {
        let what = if respect_teams { "teams" } else { "entities" };
        return format!(
            "Could not spread {} {} around {}, {} (too many for the available space)",
            piles.len(),
            what,
            cx,
            cz
        );
    }

    // All piles converged on safe columns; only now move anything.
    let mut placements: Vec<(f64, f64, f64)> = Vec::with_capacity(piles.len());
    for pile in &piles {
        let gx = pile.x.floor() as i32;
        let gz = pile.z.floor() as i32;
        let Some(y) = spreadplayers_ground_y(dim, gx, gz, under) else {
            return "Could not spread entities around the center (no safe surface)".into();
        };
        placements.push((pile.x, y as f64, pile.z));
    }
    for (pi, pile) in piles.iter().enumerate() {
        let (x, y, z) = placements[pi];
        let px = x.floor() + 0.5;
        let pz = z.floor() + 0.5;
        for &mi in &pile.members {
            match &targets[mi] {
                SpreadTarget::Player(c) => {
                    if *c.dim.lock().unwrap_or_else(|e| e.into_inner()) != dim {
                        continue;
                    }
                    let (yaw, pitch) = {
                        let p = c.pos.lock().unwrap_or_else(|e| e.into_inner());
                        (p.3, p.4)
                    };
                    teleport_in_dimension(c, &mut c.sock.lock().unwrap_or_else(|e| e.into_inner()), (px, y, pz, yaw, pitch));
                }
                SpreadTarget::Mob(m) => {
                    {
                        let mut p = m.pos.lock().unwrap_or_else(|e| e.into_inner());
                        p.0 = px;
                        p.1 = y;
                        p.2 = pz;
                    }
                    let (yaw, pitch) = {
                        let p = m.pos.lock().unwrap_or_else(|e| e.into_inner());
                        (p.3, p.4)
                    };
                    let tp = entity_teleport(m.eid, px, y, pz, yaw, pitch, true);
                    broadcast_seen(m.eid, |_c| (0x70, tp.clone()));
                }
            }
        }
    }
    let what = if respect_teams { "teams" } else { "entities" };
    let mut out = format!(
        "Successfully spread {} {} around {}, {}",
        piles.len(),
        what,
        cx,
        cz
    );
    if piles.len() > 1 {
        // Report the realised spacing like vanilla's average-distance line.
        let mut total = 0.0f64;
        for i in 0..placements.len() {
            let mut best = f64::MAX;
            for j in 0..placements.len() {
                if i != j {
                    let dx = placements[i].0 - placements[j].0;
                    let dz = placements[i].2 - placements[j].2;
                    best = best.min(dx * dx + dz * dz);
                }
            }
            total += best.sqrt();
        }
        out += &format!(
            " (average distance between {} is {:.2} blocks apart after {} iterations)",
            what,
            total / piles.len() as f64,
            iterations
        );
    }
    out
}

// ---------- /particle ----------
// world_particles 0x29 (minecraft-data 1.21.1): longDistance, x/y/z f64,
// offsetX/Y/Z f32, velocityOffset f32, amount i32, particle varint last.
fn particle_command(ctx: &CmdCtx, args: &[&str], me: Option<&Arc<Client>>) -> String {
    const USAGE: &str = "usage: /particle <name> [<pos>] | <name> <pos> <delta> <speed> <count> [force|normal] [<viewers>]";
    let Some(name) = args.first() else {
        return USAGE.into();
    };
    let Some(pid) = particle_id(name.trim_start_matches("minecraft:")) else {
        return format!("Unknown particle '{name}'");
    };
    let rest = &args[1..];
    let (px, py, pz, dx, dy, dz, speed, count, force, viewer_sel) = if rest.len() <= 3 {
        // Simple form: /particle <name> [<pos>] — a single particle at pos.
        let (px, py, pz) = match rest.len() {
            0 => (ctx.x, ctx.y, ctx.z),
            3 => {
                let (Some(x), Some(y), Some(z)) = (
                    strict_coord(rest[0], ctx.x),
                    strict_coord(rest[1], ctx.y),
                    strict_coord(rest[2], ctx.z),
                ) else {
                    return USAGE.into();
                };
                (x, y, z)
            }
            _ => return USAGE.into(),
        };
        (px, py, pz, 0.0f32, 0.0f32, 0.0f32, 0.0f32, 0i32, false, None)
    } else {
        // Full form: pos(3) delta(3) speed(1) count(1), then optional
        // force|normal and a single viewers selector.
        if rest.len() < 8 {
            return USAGE.into();
        }
        let (Some(px), Some(py), Some(pz)) = (
            strict_coord(rest[0], ctx.x),
            strict_coord(rest[1], ctx.y),
            strict_coord(rest[2], ctx.z),
        ) else {
            return USAGE.into();
        };
        let (Some(dx), Some(dy), Some(dz)) = (
            strict_coord(rest[3], ctx.x),
            strict_coord(rest[4], ctx.y),
            strict_coord(rest[5], ctx.z),
        ) else {
            return USAGE.into();
        };
        let Some(speed) = strict_num(rest[6]).filter(|v| *v >= 0.0) else {
            return USAGE.into();
        };
        let Some(count) = rest[7].parse::<i32>().ok().filter(|c| *c >= 0) else {
            return "Count must be a non-negative integer".into();
        };
        let mut i = 8;
        let mut force = false;
        if let Some(mode) = rest.get(i) {
            match *mode {
                "force" => {
                    force = true;
                    i += 1;
                }
                "normal" => i += 1,
                _ => return USAGE.into(),
            }
        }
        let viewer_sel = if i < rest.len() {
            if rest.len() - i != 1 {
                return USAGE.into();
            }
            Some(rest[i])
        } else {
            None
        };
        (
            px,
            py,
            pz,
            dx as f32,
            dy as f32,
            dz as f32,
            speed as f32,
            count,
            force,
            viewer_sel,
        )
    };

    let dim = cmd_dim(ctx);
    let range = if force { 512.0 } else { 32.0 };
    let range2 = range * range;
    let cls = client_snapshot();
    let in_range = |c: &Arc<Client>| {
        if *c.dim.lock().unwrap_or_else(|e| e.into_inner()) != dim {
            return false;
        }
        let p = *c.pos.lock().unwrap_or_else(|e| e.into_inner());
        let ddx = p.0 - px;
        let ddy = p.1 - py;
        let ddz = p.2 - pz;
        ddx * ddx + ddy * ddy + ddz * ddz < range2
    };
    let recipients: Vec<Arc<Client>> = match viewer_sel {
        Some(sel) => {
            let mut out: Vec<Arc<Client>> = Vec::new();
            for eid in resolve_selector(sel, me) {
                if let Ok(pid) = eid.parse::<i32>() {
                    if let Some(c) = cls.iter().find(|c| c.eid == pid) {
                        if in_range(c) && !out.iter().any(|o| o.eid == pid) {
                            out.push(c.clone());
                        }
                    }
                }
            }
            out
        }
        None => cls.iter().filter(|c| in_range(c)).cloned().collect(),
    };
    if recipients.is_empty() {
        return "No players in range".into();
    }
    let mut packet = Vec::new();
    packet.push(force as u8);
    packet.extend_from_slice(&px.to_be_bytes());
    packet.extend_from_slice(&py.to_be_bytes());
    packet.extend_from_slice(&pz.to_be_bytes());
    packet.extend_from_slice(&dx.to_be_bytes());
    packet.extend_from_slice(&dy.to_be_bytes());
    packet.extend_from_slice(&dz.to_be_bytes());
    packet.extend_from_slice(&speed.to_be_bytes());
    packet.extend_from_slice(&count.to_be_bytes());
    write_varint(&mut packet, pid as i64);
    for c in &recipients {
        if let Ok(mut w) = c.sock.lock() {
            let _ = send_packet(&mut w, 0x29, packet.clone());
        }
    }
    format!("Displayed {} {}", count, name)
}

// ---------- /playsound ----------
// sound_effect 0x68: ItemSoundHolder (registry id + 1 varint, matching
// sound_pkt_named), source varint, fixed-point x/y/z (x8 i32), volume f32,
// pitch f32, seed i64.
const SOUNDSOURCE_IDS: [(&str, i64); 10] = [
    ("master", 0),
    ("music", 1),
    ("record", 2),
    ("weather", 3),
    ("block", 4),
    ("hostile", 5),
    ("neutral", 6),
    ("player", 7),
    ("ambient", 8),
    ("voice", 9),
];

fn playsound_command(ctx: &CmdCtx, args: &[&str], me: Option<&Arc<Client>>) -> String {
    const USAGE: &str = "usage: /playsound <sound> [<source>] [<targets>] [<pos>] [<volume>] [<pitch>] [<minVolume>]";
    let Some(name) = args.first() else {
        return USAGE.into();
    };
    let Some(sid) = sound_id(name.trim_start_matches("minecraft:")) else {
        return format!("Unknown sound '{name}'");
    };
    let mut i = 1;
    let mut source = "master";
    if let Some(tok) = args.get(i) {
        if SOUNDSOURCE_IDS.iter().any(|(s, _)| s == tok) {
            source = *tok;
            i += 1;
        }
    }
    // Vanilla's argument chain is strictly ordered: a targets token must come
    // before any position, and each numeric argument requires the previous
    // ones. Anything left after the optional source is therefore the targets
    // selector; a bare number still parses as a (failing) player name there.
    let mut targets: Vec<Arc<Client>> = Vec::new();
    if let Some(tok) = args.get(i) {
        i += 1;
        let cls = client_snapshot();
        for eid in resolve_selector(tok, me) {
            if let Ok(pid) = eid.parse::<i32>() {
                if let Some(c) = cls.iter().find(|c| c.eid == pid) {
                    if !targets.iter().any(|o| o.eid == pid) {
                        targets.push(c.clone());
                    }
                }
            }
        }
        if targets.is_empty() {
            return "No players found".into();
        }
    } else if let Some(c) = me {
        targets.push(c.clone());
    }
    let rest = args.len() - i;
    if !matches!(rest, 0 | 3..=6) {
        return USAGE.into();
    }
    let (px, py, pz, volume, pitch, min_volume) = if rest == 0 {
        (ctx.x, ctx.y, ctx.z, 1.0f64, 1.0f64, 0.0f64)
    } else {
        let (Some(x), Some(y), Some(z)) = (
            strict_coord(args[i], ctx.x),
            strict_coord(args[i + 1], ctx.y),
            strict_coord(args[i + 2], ctx.z),
        ) else {
            return USAGE.into();
        };
        // Absent arguments fall back to vanilla defaults; a present but
        // unparsable token is a syntax error, not a default.
        let num = |k: usize, default: f64| match args.get(i + k) {
            Some(t) => strict_num(t),
            None => Some(default),
        };
        let (Some(volume), Some(pitch), Some(min_volume)) = (
            num(3, 1.0),
            num(4, 1.0),
            num(5, 0.0),
        ) else {
            return USAGE.into();
        };
        if !(volume >= 0.0) {
            return "Volume must be at least 0.0".into();
        }
        if !(0.0..=2.0).contains(&pitch) {
            return "Pitch must be between 0.0 and 2.0".into();
        }
        if !(0.0..=1.0).contains(&min_volume) {
            return "minVolume must be between 0.0 and 1.0".into();
        }
        (x, y, z, volume, pitch, min_volume)
    };
    let source_id = SOUNDSOURCE_IDS
        .iter()
        .find(|(s, _)| *s == source)
        .map(|(_, id)| *id)
        .unwrap_or(0);
    // Vanilla minVolume: players outside the 16*volume audible sphere get the
    // packet re-centred two blocks from themselves toward the source at
    // minVolume; the 767 packet carries no minVolume field, so the split is
        // applied here per recipient.
    let audible = 16.0 * volume;
    let mut sent = 0usize;
    for c in &targets {
        let p = *c.pos.lock().unwrap_or_else(|e| e.into_inner());
        let (dx, dy, dz) = (p.0 - px, p.1 - py, p.2 - pz);
        let dist = (dx * dx + dy * dy + dz * dz).sqrt();
        let (sx, sy, sz, vol) = if volume > 0.0 && dist <= audible {
            (px, py, pz, volume)
        } else if min_volume > 0.0 && dist > 0.0 {
            // Two blocks from the player toward the sound source.
            let k = 2.0 / dist;
            (
                p.0 + (px - p.0) * k,
                p.1 + (py - p.1) * k,
                p.2 + (pz - p.2) * k,
                min_volume,
            )
        } else {
            continue;
        };
        let mut packet = Vec::new();
        write_varint(&mut packet, sid as i64 + 1);
        write_varint(&mut packet, source_id);
        packet.extend_from_slice(&((sx * 8.0).floor() as i32).to_be_bytes());
        packet.extend_from_slice(&((sy * 8.0).floor() as i32).to_be_bytes());
        packet.extend_from_slice(&((sz * 8.0).floor() as i32).to_be_bytes());
        packet.extend_from_slice(&(vol as f32).to_be_bytes());
        packet.extend_from_slice(&(pitch as f32).to_be_bytes());
        packet.extend_from_slice(&(world_rng::next_u64() as i64).to_be_bytes());
        if let Ok(mut w) = c.sock.lock() {
            if send_packet(&mut w, 0x68, packet).is_ok() {
                sent += 1;
            }
        }
    }
    format!("Played sound to {sent} player(s)")
}

/// #98 — the one command-argument tokenizer: whitespace splits at the top
/// level only, so `"quoted strings"` (with `\"` escapes), `[...]` selector
/// brackets and `{...}` SNBT compounds survive as single tokens.
fn split_command_args(input: &str) -> Result<Vec<String>, String> {
    crate::brigadier::split_command_args_preserving_quotes(input)
}

// ---------- /tp (#175) ----------
/// One shared mover for `/tp` subjects: players take the real teleport packet
/// path (0x40, plus a respawn when the dimension changes), mobs write their
/// live position and get the entity-teleport broadcast (0x70), and fake actors
/// keep their simple pos record.
enum TpMover {
    Player(Arc<Client>),
    Mob(Arc<Mob>),
    Actor(Arc<Actor>),
}

impl TpMover {
    fn position(&self) -> (f64, f64, f64, f32, f32) {
        match self {
            Self::Player(client) => {
                let p = client.pos.lock().unwrap_or_else(|e| e.into_inner());
                (p.0, p.1, p.2, p.3, p.4)
            }
            Self::Mob(mob) => *mob.pos.lock().unwrap_or_else(|e| e.into_inner()),
            Self::Actor(actor) => {
                let p = actor.pos.lock().unwrap_or_else(|e| e.into_inner());
                (p.0, p.1, p.2, 0.0, 0.0)
            }
        }
    }

    fn teleport(&self, dim: DimId, x: f64, y: f64, z: f64, yaw: f32, pitch: f32) -> bool {
        match self {
            Self::Player(client) => {
                if *client.dim.lock().unwrap_or_else(|e| e.into_inner()) != dim {
                    // A cross-dimension /tp is a real respawn, not a coordinate
                    // patch: the client needs 0x47 and a whole new window.
                    let view = *client.view.lock().unwrap_or_else(|e| e.into_inner());
                    switch_client_dimension(client, dim, (x, y, z, yaw, pitch), view);
                } else {
                    teleport_in_dimension(
                        client,
                        &mut client.sock.lock().unwrap_or_else(|e| e.into_inner()),
                        (x, y, z, yaw, pitch),
                    );
                }
                true
            }
            Self::Mob(mob) => {
                if mob.dim != dim {
                    return false;
                }
                *mob.pos.lock().unwrap_or_else(|e| e.into_inner()) = (x, y, z, yaw, pitch);
                let tp = entity_teleport(mob.eid, x, y, z, yaw, pitch, true);
                broadcast_seen(mob.eid, |_c| (0x70, tp.clone()));
                true
            }
            Self::Actor(actor) => {
                *actor.pos.lock().unwrap_or_else(|e| e.into_inner()) = (x, y, z);
                true
            }
        }
    }
}

/// A rotation argument token: absolute degrees or a `~`-relative offset from
/// each subject's current angle, matching vanilla `RotationArgument`.
enum TpAngle {
    Abs(f32),
    Rel(f32),
}

impl TpAngle {
    fn parse(tok: &str) -> Option<TpAngle> {
        if let Some(off) = tok.strip_prefix('~') {
            if off.is_empty() {
                return Some(TpAngle::Rel(0.0));
            }
            let v = off.parse::<f32>().ok()?;
            v.is_finite().then_some(TpAngle::Rel(v))
        } else {
            let v = tok.parse::<f32>().ok()?;
            v.is_finite().then_some(TpAngle::Abs(v))
        }
    }

    fn resolve(&self, base: f32) -> f32 {
        match self {
            Self::Abs(v) => *v,
            Self::Rel(off) => base + *off,
        }
    }
}

/// How the destination fixes each subject's final rotation.
enum TpRot {
    Keep,
    Set(TpAngle, TpAngle),
    /// Aim the subject's feet-anchor at this world point.
    Facing([f64; 3]),
}

/// Vanilla `Entity.lookAt` math (`btm`): `yaw = atan2(-dx, dz)`,
/// `pitch = atan2(-dy, hypot(dx, dz))` in degrees, wrapped to [-180, 180).
fn tp_look_angles(from: [f64; 3], to: [f64; 3]) -> (f32, f32) {
    let dx = to[0] - from[0];
    let dy = to[1] - from[1];
    let dz = to[2] - from[2];
    (
        tp_wrap_degrees((-dx).atan2(dz).to_degrees() as f32),
        tp_wrap_degrees((-dy).atan2(dx.hypot(dz)).to_degrees() as f32),
    )
}

fn tp_wrap_degrees(angle: f32) -> f32 {
    let wrapped = angle % 360.0;
    if wrapped >= 180.0 {
        wrapped - 360.0
    } else if wrapped < -180.0 {
        wrapped + 360.0
    } else {
        wrapped
    }
}

/// Resolve a single-entity argument (selector or player name) the way
/// `execute`'s entity args do. Vanilla requires destination entity args to
/// match exactly one entity.
fn tp_single_entity(
    ctx: &CmdCtx,
    token: &str,
) -> Result<ExecuteEntity, String> {
    let source = ExecuteSource::new(ctx);
    match source.select(token) {
        Err(e) => Err(e),
        Ok(entities) if entities.len() == 1 => Ok(entities.into_iter().next().unwrap()),
        Ok(entities) if entities.is_empty() => Err("No entity was found".into()),
        Ok(_) => Err("Only one entity is allowed".into()),
    }
}

/// A teleport destination for a single entity token that is neither a live
/// selector match nor a player — the fake actors used by parity fixtures.
fn tp_actor_destination(token: &str) -> Option<([f64; 3], DimId)> {
    actors().lock().unwrap_or_else(|e| e.into_inner()).get(token).map(|actor| {
        let p = *actor.pos.lock().unwrap_or_else(|e| e.into_inner());
        ([p.0, p.1, p.2], DimId::Overworld)
    })
}

fn run_teleport_command(ctx: &CmdCtx, args: &[&str], me: Option<&Arc<Client>>) -> String {
    const USAGE: &str = "usage: /tp [<targets>] (<entity>|<x> <y> <z> [[<yaw> <pitch>]|facing (<fx> <fy> <fz>|entity <entity> [feet|eyes])])";
    let Some((&first, rest)) = args.split_first() else {
        return USAGE.into();
    };
    // The first token is the *targets* argument whenever it is not itself a
    // coordinate and at least one further argument follows. `/tp <entity>`
    // (single token) teleports the executor to that entity.
    let first_is_coord = first.parse::<f64>().is_ok() || first.starts_with('~');
    let (subject_tokens, dest): (&[&str], &[&str]) = if rest.is_empty() || first_is_coord {
        (&[][..], &args[..])
    } else {
        (&args[..1], rest)
    };

    // ---- subjects -------------------------------------------------------
    let mut movers: Vec<TpMover> = Vec::new();
    if subject_tokens.is_empty() {
        if let Some(client) = me {
            movers.push(TpMover::Player(client.clone()));
        } else {
            return "No entity was found".into();
        }
    } else {
        let source = ExecuteSource::new(ctx);
        for token in subject_tokens {
            let selected = match source.select(token) {
                Ok(entities) => entities,
                Err(e) => return e,
            };
            if selected.is_empty() {
                if let Some(actor) = actors().lock().unwrap_or_else(|e| e.into_inner()).get(*token).cloned() {
                    movers.push(TpMover::Actor(actor));
                }
            }
            for entity in selected {
                match entity {
                    ExecuteEntity::Player(client) => movers.push(TpMover::Player(client)),
                    ExecuteEntity::Mob(mob) => movers.push(TpMover::Mob(mob)),
                    // Projectiles, items and other non-living entities have no
                    // live pos handle exposed to commands yet; vanilla moves
                    // them, we report them as unmatched.
                    ExecuteEntity::Other { .. } => {}
                }
            }
        }
    }
    if movers.is_empty() {
        return "No entity was found".into();
    }

    // ---- destination + rotation ------------------------------------------
    let (dest_pos, dest_dim, rot): ([f64; 3], DimId, TpRot) = if dest.len() == 1 {
        // `/tp <targets> <dest_entity>` — vanilla also adopts the
        // destination entity's own rotation.
        match tp_single_entity(ctx, dest[0]) {
            Ok(entity) => {
                let (pos, [yaw, pitch]) = entity.location();
                (pos, entity.dimension(), TpRot::Set(TpAngle::Abs(yaw as f32), TpAngle::Abs(pitch as f32)))
            }
            Err(e) => match tp_actor_destination(dest[0]) {
                Some((pos, dim)) => (pos, dim, TpRot::Keep),
                None => return e,
            },
        }
    } else if dest.len() >= 3 {
        let pos = [
            coord(dest[0], ctx.x),
            coord(dest[1], ctx.y),
            coord(dest[2], ctx.z),
        ];
        if !pos.into_iter().all(f64::is_finite) {
            return "Invalid position".into();
        }
        let rot = match dest.get(3) {
            None => TpRot::Keep,
            Some(&"facing") => match dest.get(4) {
                Some(&"entity") => {
                    if dest.len() > 7 {
                        return USAGE.into();
                    }
                    let Some(&sel) = dest.get(5) else {
                        return USAGE.into();
                    };
                    let anchor = dest.get(6).copied().unwrap_or("feet");
                    if !matches!(anchor, "feet" | "eyes") {
                        return USAGE.into();
                    }
                    let entity = match tp_single_entity(ctx, sel) {
                        Ok(entity) => entity,
                        Err(e) => return e,
                    };
                    let (mut face, _) = entity.location();
                    if anchor == "eyes" {
                        face[1] += entity.eye_height();
                    }
                    TpRot::Facing(face)
                }
                _ => {
                    if dest.len() != 7 {
                        return USAGE.into();
                    }
                    let face = [
                        coord(dest[4], ctx.x),
                        coord(dest[5], ctx.y),
                        coord(dest[6], ctx.z),
                    ];
                    if !face.into_iter().all(f64::is_finite) {
                        return "Invalid position".into();
                    }
                    TpRot::Facing(face)
                }
            },
            Some(&yaw_tok) => {
                if dest.len() != 5 {
                    return USAGE.into();
                }
                let (Some(yaw), Some(pitch)) =
                    (TpAngle::parse(yaw_tok), TpAngle::parse(dest[4]))
                else {
                    return USAGE.into();
                };
                TpRot::Set(yaw, pitch)
            }
        };
        (pos, cmd_dim(ctx), rot)
    } else {
        return USAGE.into();
    };

    // ---- apply ------------------------------------------------------------
    let mut n = 0usize;
    for mover in &movers {
        let (_, _, _, cur_yaw, cur_pitch) = mover.position();
        let [x, y, z] = dest_pos;
        let (yaw, pitch) = match &rot {
            TpRot::Keep => (cur_yaw, cur_pitch),
            TpRot::Set(y, p) => (
                tp_wrap_degrees(y.resolve(cur_yaw)),
                tp_wrap_degrees(p.resolve(cur_pitch)),
            ),
            // Vanilla aims from the subject's feet-anchor at the destination.
            TpRot::Facing(face) => tp_look_angles([x, y, z], *face),
        };
        if mover.teleport(dest_dim, x, y, z, yaw, pitch) {
            n += 1;
        }
    }
    format!("Teleported {} target(s)", n)
}

// ---------- /loot (#176) ----------
fn loot_table_pools(id: &str) -> Option<Vec<LootPool>> {
    let id = if id.contains(':') {
        id.to_owned()
    } else {
        format!("minecraft:{id}")
    };
    loot_tables().lock().unwrap_or_else(|e| e.into_inner()).get(&id).map(|pools| {
        pools
            .iter()
            .map(|pl| LootPool {
                rolls: pl.rolls,
                chance: pl.chance,
                funcs: pl.funcs.clone(),
                entries: pl
                    .entries
                    .iter()
                    .map(|e| LootEntry {
                        item: e.item,
                        min: e.min,
                        max: e.max,
                        funcs: e.funcs.clone(),
                    })
                    .collect(),
            })
            .collect()
    })
}

/// `/loot <slot>` spec shared by the `replace` targets — the same window-0
/// keys `/item` understands.
fn loot_slot_of(spec: &str, held: i16) -> Option<i16> {
    Some(match spec {
        "weapon.mainhand" => 36 + held,
        "weapon.offhand" => 45,
        "armor.head" => 5,
        "armor.chest" => 6,
        "armor.legs" => 7,
        "armor.feet" => 8,
        _ => {
            let n = spec
                .strip_prefix("hotbar.")
                .or_else(|| spec.strip_prefix("inventory."))
                .or_else(|| spec.strip_prefix("container."))?;
            n.parse::<i16>().ok().map(|x| 9 + x)?
        }
    })
}

/// Give a stack to a player exactly like `/give`: merge into a matching
/// partial stack first, then hotbar (36-44) before the main inventory (9-35).
fn loot_give_stack(client: &Arc<Client>, stack: &Stack) {
    let (item, cnt, comps) = stack;
    let mut inv = client.inv.lock().unwrap_or_else(|e| e.into_inner());
    let slot: i16 = (36..=44i16)
        .chain(9..=35)
        .find(|sl| {
            inv.get(sl).map(|(it, cn, cc)| {
                *it == *item && *cc == *comps
                    && (*cn as i64 + *cnt as i64) <= item_stack_max(*item, comps) as i64
            }).unwrap_or(false)
        })
        .or_else(|| (36..=44).chain(9..=35).find(|sl| !inv.contains_key(sl)))
        .unwrap_or(36);
    let st = if let Some(existing) = inv.get_mut(&slot) {
        existing.1 = existing.1.saturating_add(*cnt);
        existing.clone()
    } else {
        inv.insert(slot, stack.clone());
        stack.clone()
    };
    drop(inv);
    let mut p = vec![0u8];
    write_varint(&mut p, 0);
    p.extend_from_slice(&slot.to_be_bytes());
    p.extend_from_slice(&slot_wire(&st));
    if let Ok(mut w) = client.sock.lock() {
        let _ = send_packet(&mut w, 0x15, p);
    }
}

/// The capacity `/loot insert` fills into for the block at `pos` — the same
/// sizes `RandomizableContainer` uses, or `None` when the block is no
/// container at all (vanilla fails closed there).
fn loot_container_capacity(dim: DimId, x: i32, y: i32, z: i32) -> Option<i16> {
    if dim == DimId::Overworld && is_furnace_at(x, y, z) {
        return Some(3);
    }
    // `container_kind` only names 27-slot generic containers today.
    container_kind(block_at_dim(dim, x, y, z)).map(|_| 27)
}

/// Roll one `/loot` source clause into item stacks.
fn loot_roll_source(
    ctx: &CmdCtx,
    source: &[&str],
) -> Result<Vec<Stack>, String> {
    const USAGE: &str = "usage: /loot ... (loot <table>|kill <entity>|mine <x> <y> <z> [<tool>]|fish <table> <x> <y> <z> [<tool>])";
    let Some((&kind, rest)) = source.split_first() else {
        return Err(USAGE.into());
    };
    match kind {
        "loot" => {
            let [id] = rest else { return Err(USAGE.into()) };
            loot_table_pools(id)
                .map(|pools| roll_loot(&pools))
                .ok_or_else(|| format!("Unknown loot table '{id}'"))
        }
        "kill" => {
            let [entity] = rest else { return Err(USAGE.into()) };
            // A selector or entity name resolves through the same path as
            // execute's entity args; a bare type name falls back to the
            // registry like the old subset did.
            if entity.starts_with('@') {
                match tp_single_entity(ctx, entity) {
                    Ok(ExecuteEntity::Mob(mob)) => {
                        let burning = *mob.fire_ticks.lock().unwrap_or_else(|e| e.into_inner()) > 0;
                        Ok(mob_loot(mob.etype, burning))
                    }
                    Ok(_) => Err("Entity has no loot table".into()),
                    Err(e) => Err(e),
                }
            } else {
                match name_to_etype(entity.trim_start_matches("minecraft:")) {
                    Some(etype) => Ok(mob_loot(etype, false)),
                    None => Err(format!("Unknown entity '{entity}'")),
                }
            }
        }
        "mine" => {
            if !(3..=4).contains(&rest.len()) {
                return Err(USAGE.into());
            }
            let (x, y, z) = (
                coord(rest[0], ctx.x) as i32,
                coord(rest[1], ctx.y) as i32,
                coord(rest[2], ctx.z) as i32,
            );
            // Vanilla accepts `mainhand`/`offhand`/an item id; the drop table
            // here is the registry's, which already encodes silk-touch rules.
            if let Some(&tool) = rest.get(3) {
                let known = matches!(tool, "mainhand" | "offhand")
                    || name_to_item(tool.trim_start_matches("minecraft:")).is_some();
                if !known {
                    return Err(format!("Unknown item '{tool}'"));
                }
            }
            Ok(block_drops(block_at_dim(cmd_dim(ctx), x, y, z))
                .into_iter()
                .map(|item| (item, 1u8, Vec::new()))
                .collect())
        }
        "fish" => {
            if !(4..=5).contains(&rest.len()) {
                return Err(USAGE.into());
            }
            let id = rest[0];
            // rest[1..4] is the fishing position; rest[4] an optional tool.
            if let Some(&tool) = rest.get(4) {
                let known = matches!(tool, "mainhand" | "offhand")
                    || name_to_item(tool.trim_start_matches("minecraft:")).is_some();
                if !known {
                    return Err(format!("Unknown item '{tool}'"));
                }
            }
            loot_table_pools(id)
                .map(|pools| roll_loot(&pools))
                .ok_or_else(|| format!("Unknown loot table '{id}'"))
        }
        _ => Err(USAGE.into()),
    }
}

/// `replace` consumes an optional <count> between the slot spec and the
/// source: a bare integer token, which cannot itself start a source clause.
fn loot_take_count<'a>(rest: &'a [&'a str]) -> (Option<usize>, &'a [&'a str]) {
    match rest.first() {
        Some(tok) if tok.parse::<usize>().is_ok() && rest.len() > 1 => {
            (tok.parse::<usize>().ok(), &rest[1..])
        }
        _ => (None, rest),
    }
}

fn run_loot_command(ctx: &CmdCtx, args: &[&str], me: Option<&Arc<Client>>) -> String {
    const USAGE: &str = "usage: /loot (give <players>|insert <x> <y> <z>|replace block <x> <y> <z> <slot> [<count>]|replace entity <targets> <slot> [<count>]|spawn <x> <y> <z>) <source>";
    let Some(&target) = args.first() else {
        return USAGE.into();
    };
    match target {
        "give" => {
            let [selector, source @ ..] = &args[1..] else {
                return USAGE.into();
            };
            if source.is_empty() {
                return USAGE.into();
            }
            let mut players: Vec<Arc<Client>> = Vec::new();
            for eid in resolve_selector(selector, me) {
                if let Ok(id) = eid.parse::<i32>() {
                    if let Some(c) = client_snapshot()
                        .into_iter()
                        .find(|c| c.eid == id)
                    {
                        players.push(c);
                    }
                }
            }
            if players.is_empty() {
                return "No players found".into();
            }
            let items = match loot_roll_source(ctx, source) {
                Ok(items) => items,
                Err(e) => return e,
            };
            let mut total = 0usize;
            for c in &players {
                for st in &items {
                    loot_give_stack(c, st);
                    total += 1;
                }
            }
            format!("Gave {} item stack(s)", total)
        }
        "insert" => {
            let (Some(x), Some(y), Some(z)) =
                (args.get(1), args.get(2), args.get(3))
            else {
                return USAGE.into();
            };
            let (bx, by, bz) = (
                coord(x, ctx.x) as i32,
                coord(y, ctx.y) as i32,
                coord(z, ctx.z) as i32,
            );
            let source = &args[4..];
            if source.is_empty() {
                return USAGE.into();
            }
            let dim = cmd_dim(ctx);
            let Some(capacity) = loot_container_capacity(dim, bx, by, bz) else {
                return "The target block is not a container".into();
            };
            let items = match loot_roll_source(ctx, source) {
                Ok(items) => items,
                Err(e) => return e,
            };
            let key = ContainerKey::Block((dim, bx, by, bz));
            let mut chests = chests().lock().unwrap_or_else(|e| e.into_inner());
            let slots = chests.entry(key).or_default();
            let mut n = 0usize;
            for st in &items {
                // vanilla `insert` merges partials first, then the first free
                // slot — same fill order as hopper insertion.
                let merge = (0..capacity).find(|sl| {
                    slots.get(sl).map(|(it, cn, cc)| {
                        *it == st.0 && *cc == st.2
                            && (*cn as i64 + st.1 as i64)
                                <= item_stack_max(st.0, &st.2) as i64
                    }).unwrap_or(false)
                });
                let slot = merge.or_else(|| (0..capacity).find(|sl| !slots.contains_key(sl)));
                let Some(slot) = slot else { continue };
                if let Some(existing) = slots.get_mut(&slot) {
                    existing.1 = existing.1.saturating_add(st.1);
                } else {
                    slots.insert(slot, st.clone());
                }
                n += 1;
            }
            format!("Inserted {} item stack(s)", n)
        }
        "replace" => {
            match args.get(1).copied() {
                Some("block") => {
                    let (Some(x), Some(y), Some(z), Some(&slot_spec)) =
                        (args.get(2), args.get(3), args.get(4), args.get(5))
                    else {
                        return USAGE.into();
                    };
                    let Some(slot) = slot_spec
                        .strip_prefix("container.")
                        .and_then(|v| v.parse::<i16>().ok())
                    else {
                        return USAGE.into();
                    };
                    let (count, source) = loot_take_count(&args[6..]);
                    if source.is_empty() {
                        return USAGE.into();
                    }
                    let (bx, by, bz) = (
                        coord(x, ctx.x) as i32,
                        coord(y, ctx.y) as i32,
                        coord(z, ctx.z) as i32,
                    );
                    let dim = cmd_dim(ctx);
                    let Some(capacity) = loot_container_capacity(dim, bx, by, bz) else {
                        return "The target block is not a container".into();
                    };
                    let items = match loot_roll_source(ctx, source) {
                        Ok(items) => items,
                        Err(e) => return e,
                    };
                    let limit = count.unwrap_or(1).min(items.len());
                    let mut n = 0usize;
                    let mut chests = chests().lock().unwrap_or_else(|e| e.into_inner());
                    let slots = chests.entry(ContainerKey::Block((dim, bx, by, bz))).or_default();
                    for (i, st) in items.iter().take(limit).enumerate() {
                        let s = slot + i as i16;
                        if s < 0 || s >= capacity {
                            break;
                        }
                        slots.insert(s, st.clone());
                        n += 1;
                    }
                    format!("Replaced {} slot(s)", n)
                }
                Some("entity") => {
                    let (Some(&selector), Some(&slot_spec)) = (args.get(2), args.get(3))
                    else {
                        return USAGE.into();
                    };
                    let (count, source) = loot_take_count(&args[4..]);
                    if source.is_empty() {
                        return USAGE.into();
                    }
                    let items = match loot_roll_source(ctx, source) {
                        Ok(items) => items,
                        Err(e) => return e,
                    };
                    let limit = count.unwrap_or(1).min(items.len());
                    let mut n = 0usize;
                    let cls = client_snapshot();
                    for eid in resolve_selector(selector, me) {
                        let Ok(id) = eid.parse::<i32>() else { continue };
                        let Some(c) = cls.iter().find(|c| c.eid == id) else { continue };
                        for (i, st) in items.iter().take(limit).enumerate() {
                            let spec = if i == 0 {
                                slot_spec.to_owned()
                            } else {
                                // `replace entity` counts consecutive
                                // container.N slots for the <count> form.
                                match slot_spec.strip_prefix("container.").and_then(|v| v.parse::<i16>().ok()) {
                                    Some(base) => format!("container.{}", base + i as i16),
                                    None => slot_spec.to_owned(),
                                }
                            };
                            let Some(slot) = loot_slot_of(&spec, *c.held.lock().unwrap_or_else(|e| e.into_inner())) else { continue };
                            {
                                let mut inv = c.inv.lock().unwrap_or_else(|e| e.into_inner());
                                inv.insert(slot, st.clone());
                                *c.armor_pts.lock().unwrap_or_else(|e| e.into_inner()) = armor_pts_of(&inv);
                            }
                            let mut p = vec![0u8];
                            write_varint(&mut p, 0);
                            p.extend_from_slice(&slot.to_be_bytes());
                            p.extend_from_slice(&slot_wire(st));
                            if let Ok(mut w) = c.sock.lock() {
                                let _ = send_packet(&mut w, 0x15, p);
                            }
                            n += 1;
                        }
                    }
                    format!("Replaced {} slot(s)", n)
                }
                _ => USAGE.into(),
            }
        }
        "spawn" => {
            let (Some(x), Some(y), Some(z)) =
                (args.get(1), args.get(2), args.get(3))
            else {
                return USAGE.into();
            };
            let source = &args[4..];
            if source.is_empty() {
                return USAGE.into();
            }
            let (lx, ly, lz) = (coord(x, ctx.x), coord(y, ctx.y), coord(z, ctx.z));
            let items = match loot_roll_source(ctx, source) {
                Ok(items) => items,
                Err(e) => return e,
            };
            let n = items.len();
            for (item, cnt, ic) in items {
                drops().lock().unwrap_or_else(|e| e.into_inner()).push(Drop {
                    eid: NEXT_EID.fetch_add(1, Ordering::SeqCst),
                    x: lx,
                    y: ly,
                    z: lz,
                    item,
                    cnt,
                    comps: ic,
                    age: 0,
                    target: None,
                    thrower: None,
                });
            }
            format!("Dropped {} item stack(s)", n)
        }
        _ => USAGE.into(),
    }
}

fn run_command_ctx(ctx: &CmdCtx, msg: &str, s: &mut Option<&Client>) -> String {
    let me = ctx.me.as_ref();
    let Some(input) = msg.strip_prefix('/') else { return "Expected a command starting with /".into() };
    let first_word = input.split_whitespace().next().unwrap_or("");
    let tokens = if matches!(first_word, "say" | "execute") {
        // `say`/`execute` re-join their arguments anyway, and greedy vanilla
        // text may carry an unmatched quote the strict tokenizer would reject.
        input.split_whitespace().map(str::to_owned).collect()
    } else {
        match split_command_args(input) {
            Ok(tokens) => tokens,
            Err(error) => return format!("Invalid command: {error}"),
        }
    };
    let cmd = tokens.first().map(String::as_str).unwrap_or("");
    let args: Vec<&str> = tokens.iter().skip(1).map(String::as_str).collect();
    // Vanilla `CommandSourceStack.hasPermission`: the console runs at level 4,
    // players at their ops.json level. `command_allowed_for_player` keeps the
    // argument-aware level-0 exceptions (e.g. `/time query`) on top.
    let level = me.map_or(4, |player| player_permission_level(&player.name));
    if !command_allowed_at_level(cmd, level) && !command_allowed_for_player(cmd, &args) {
        let reply = "You do not have permission to run that command";
        if let Some(client) = s {
            let _ = send_packet(&mut client.sock.lock().unwrap_or_else(|e| e.into_inner()), 0x6c, system_chat(reply));
        }
        return reply.into();
    }
    #[cfg(feature = "neoforge-events")]
    let mod_command = java_compat::NEOFORGE.get().filter(|runtime| {
        runtime.commands.binary_search_by(|name| name.as_str().cmp(cmd)).is_ok()
    });
    #[cfg(feature = "neoforge-events")]
    if let Some(runtime) = mod_command {
        let reply = if !args.is_empty() {
            format!("usage: /{cmd}")
        } else {
            run_neoforge_command(runtime, ctx, cmd).unwrap_or_else(|error| {
                eprintln!("[mod-compatibility] NeoForge command {cmd}: {error}");
                format!("NeoForge command failed: {cmd}")
            })
        };
        if !reply.is_empty() {
            if let Some(client) = s {
                let _ = send_packet(&mut client.sock.lock().unwrap_or_else(|e| e.into_inner()), 0x6c, system_chat(&reply));
            }
        }
        return reply;
    }
    let reply = match cmd {
        "execute" => run_execute(ctx, &args, s),
        "time" => {
            if args.first() == Some(&"add") {
                if let Some(ticks) = args.get(1).and_then(|n| time_argument_ticks(n)) {
                    let current = WORLD_TIME.load(Ordering::Relaxed);
                    let updated = add_world_time(current, ticks);
                    WORLD_TIME.store(updated, Ordering::Relaxed);
                    format!("Set the time to {}", updated)
                } else {
                    "usage: /time add <ticks>".into()
                }
            } else if args.first() == Some(&"set") {
                let v = match args.get(1).copied().unwrap_or("") {
                    "day" => 1000, "night" => 13000, "noon" => 6000, "midnight" => 18000,
                    n => n.parse::<u64>().unwrap_or(1000),
                };
                WORLD_TIME.store(v, Ordering::Relaxed);
                format!("Set time to {}", v)
            } else if matches!(args.first().copied(), Some("query") | Some("get")) {
                match args.get(1).copied().unwrap_or("daytime") {
                    "daytime" => format!("The time is {}", WORLD_TIME.load(Ordering::Relaxed) % 24000),
                    "day" => format!("The day is {}", WORLD_TIME.load(Ordering::Relaxed) / 24000),
                    _ => format!("The gametime is {}", WORLD_TIME.load(Ordering::Relaxed)),
                }
            } else { "usage: /time add <ticks> | set <n|day|night> | query daytime|gametime|day".into() }
        }
        "difficulty" => {
            match args.first().copied().unwrap_or("") {
                "peaceful" | "easy" | "normal" | "hard" => {
                    let d = match args[0] { "peaceful" => 0u8, "easy" => 1, "hard" => 3, _ => 2 };
                    set_difficulty(d);
                    format!("Set difficulty to {}", args[0])
                }
                "" => format!("The difficulty is {}", ["peaceful","easy","normal","hard"][DIFFICULTY.load(Ordering::Relaxed) as usize]),
                _ => "usage: /difficulty [peaceful|easy|normal|hard]".into(),
            }
        }
        "save-off" => {
            let was_enabled = AUTOSAVE_ENABLED.swap(false, Ordering::Relaxed);
            if was_enabled { "Automatic saving is now disabled".into() }
            else { "Automatic saving is already disabled".into() }
        }
        "save-on" => {
            let was_enabled = AUTOSAVE_ENABLED.swap(true, Ordering::Relaxed);
            if was_enabled { "Automatic saving is already enabled".into() }
            else { "Automatic saving is now enabled".into() }
        }
        "defaultgamemode" => {
            let (mode, value) = match args.first().copied().unwrap_or("") {
                "survival" | "0" => ("survival", 0),
                "creative" | "1" => ("creative", 1),
                "adventure" | "2" => ("adventure", 2),
                "spectator" | "3" => ("spectator", 3),
                _ => return "usage: /defaultgamemode survival|creative|adventure|spectator".into(),
            };
            DEFAULT_GAME_MODE.store(value, Ordering::Relaxed);
            format!("The default game mode is now {mode}")
        }
        "help" | "?" if !args.is_empty() => {
            // /help <command>: exact usage for commands the caller may run,
            // Vanilla's "Unknown command or insufficient permissions" otherwise.
            let name = args[0]
                .trim_start_matches('/')
                .rsplit(':')
                .next()
                .unwrap_or("");
            if name.is_empty() || !command_allowed_at_level(name, level) {
                "Unknown command or insufficient permissions".into()
            } else {
                command_usage(name)
            }
        }
        "help" | "?" => {
            let names = command_tree_at_level(level).children().into_iter()
                .map(|node| node.name().to_owned()).collect::<Vec<_>>();
            format!("commands: {}", names.join(" "))
        }
        "spawnpoint" => {
            if !matches!(args.len(), 0 | 1 | 3..=5) {
                return command_usage(cmd);
            }
            let (nm, xyz): (String, &[&str]) = if args.len() >= 4 {
                (args[0].to_string(), &args[1..])
            } else if args.len() >= 3 { (ctx.me.as_ref().map(|c| c.name.clone()).unwrap_or_default(), &args[0..]) }
              else { (args.first().copied().unwrap_or("").to_string(), &args[0..0]) };
            let t: Option<Arc<Client>> = if nm.is_empty() { ctx.me.clone() }
                else { clients().lock().unwrap_or_else(|e| e.into_inner()).iter().find(|c| c.name == nm).cloned() };
            match t {
                None => format!("unknown player '{nm}'"),
                Some(c) => {
                    let (x, y, z) = if xyz.len() >= 3 {
                        (coord(xyz[0], ctx.x), coord(xyz[1], ctx.y), coord(xyz[2], ctx.z))
                    } else { let p = c.pos.lock().unwrap_or_else(|e| e.into_inner()); (p.0, p.1, p.2) };
                    let Some(yaw) = spawn_yaw(xyz.get(3).copied(), me) else {
                        return "Invalid spawn angle".into();
                    };
                    let dim = cmd_dim(ctx);
                    let mut spawn_dim = c.spawn_dim.lock().unwrap_or_else(|e| e.into_inner());
                    *c.spawn_pt.lock().unwrap_or_else(|e| e.into_inner()) = Some((x, y, z, yaw));
                    *spawn_dim = dim;
                    format!("Set spawn point to {x}, {y}, {z} facing {yaw} for {}", c.name)
                }
            }
        }
        "setworldspawn" => {
            if cmd_dim(ctx) != DimId::Overworld {
                return "World spawn can only be set in the Overworld".into();
            }
            if !matches!(args.len(), 0 | 3 | 4) {
                return command_usage(cmd);
            }
            let (x, y, z) = if args.len() >= 3 {
                (coord(args[0], ctx.x), coord(args[1], ctx.y), coord(args[2], ctx.z))
            } else { (ctx.x, ctx.y, ctx.z) };
            let Some(yaw) = spawn_yaw(args.get(3).copied(), me) else {
                return "Invalid spawn angle".into();
            };
            let spawn = (x, y, z, yaw);
            *WORLD_SPAWN.lock().unwrap_or_else(|e| e.into_inner()) = spawn;
            fanout_in(
                &client_snapshot(),
                |player| *player.dim.lock().unwrap_or_else(|e| e.into_inner()) == DimId::Overworld,
                |_| (0x56, spawn_position_packet(spawn)),
            );
            format!("Set the world spawn point to {x}, {y}, {z} facing {yaw}")
        }
        "op" | "deop" => match args.as_slice() {
            [name] if valid_player_name(name) => {
                match set_operator(std::path::Path::new(OPS_FILE), name, cmd == "op") {
                    Ok(true) => {
                        for client in client_snapshot().iter().filter(|c| c.name.eq_ignore_ascii_case(name)) {
                            update_client_commands(client);
                        }
                        if cmd == "op" { format!("Made {name} a server operator") }
                        else { format!("Made {name} no longer a server operator") }
                    }
                    Ok(false) if cmd == "op" => format!("{name} is already a server operator"),
                    Ok(false) => format!("{name} is not a server operator"),
                    Err(error) => format!("Could not save {OPS_FILE}: {error}"),
                }
            }
            _ => format!("usage: /{cmd} <player>"),
        },
        "reload" if args.is_empty() => {
            match reload_datapacks(std::path::Path::new(DATAPACK_DIRECTORY), None, true) {
                Ok(count) => {
                    // Naturally refresh [anticheat] from server.toml (if present) and
                    // auto-hidden #c:ores / #forge:ores / #c:chests tags on vanilla /reload.
                    ac_reload_quiet();
                    refresh_hidden_tags_from_datapacks(std::path::Path::new(DATAPACK_DIRECTORY));
                    let ops = load_operators(std::path::Path::new(OPS_FILE));
                    for client in client_snapshot() {
                        update_client_commands(&client);
                    }
                    match ops {
                        Ok(()) => format!("Reloading! ({count} pack(s))"),
                        Err(error) => format!("Reloading! ({count} pack(s)); could not load {OPS_FILE}: {error}"),
                    }
                }
                Err(error) => error,
            }
        }
        "reload" => "usage: /reload".into(),
        "tp" | "teleport" => run_teleport_command(ctx, &args, me),
        "say" => {
            let line = format!("[Server] {}", args.join(" "));
            let pl = system_chat(&line);
            broadcast(-1, |_c| (0x6c, pl.clone()));
            return String::new();
        }
        "me" => {
            if args.is_empty() {
                return "usage: /me <action>".into();
            }
            let sender = me.map(|client| client.name.as_str()).unwrap_or("Server");
            let payload = system_chat(&format!("* {sender} {}", args.join(" ")));
            broadcast(-1, |_c| (0x6c, payload.clone()));
            return String::new();
        }
        "msg" | "tell" | "w" => {
            let Some((target_name, words)) = args.split_first() else {
                return "usage: /msg <player> <message>".into();
            };
            if words.is_empty() {
                return "usage: /msg <player> <message>".into();
            }
            let target = client_snapshot()
                .into_iter()
                .find(|client| client.name.eq_ignore_ascii_case(target_name));
            let Some(target) = target else {
                return format!("That player does not exist: {target_name}");
            };
            let sender = me.map(|client| client.name.as_str()).unwrap_or("Server");
            let text = words.join(" ");
            let target_message = system_chat(&format!("{sender} whispers to you: {text}"));
            let _ = send_packet(&mut target.sock.lock().unwrap_or_else(|e| e.into_inner()), 0x6c, target_message);
            let reply = format!("You whisper to {}: {}", target.name, text);
            if let Some(client) = *s {
                let _ = send_packet(&mut client.sock.lock().unwrap_or_else(|e| e.into_inner()), 0x6c, system_chat(&reply));
            }
            return String::new();
        }
        "kill" => {
            let mut killed = 0;
            let targets = match args.first() {
                Some(sel) if sel.starts_with('@') => resolve_selector(sel, me),
                Some(nm) => clients().lock().unwrap_or_else(|e| e.into_inner()).iter()
                    .find(|c| c.name == *nm).map(|c| vec![c.eid.to_string()])
                    .unwrap_or_default(),
                _ => me.map(|c| vec![c.eid.to_string()]).unwrap_or_default(),
            };
            for t in targets {
                if let Some(eid) = t.strip_prefix("m:").and_then(|e| e.parse::<i32>().ok()) {
                    let info = mobs().lock().unwrap_or_else(|e| e.into_inner()).iter()
                        .find(|m| m.eid == eid)
                        .map(|m| (m.etype, *m.pos.lock().unwrap_or_else(|e| e.into_inner())));
                    if let Some((et, (mx, my, mz, ..))) = info { kill_mob(eid, et, mx, my, mz); }
                    killed += 1;
                } else if let Some(eid) = t.strip_prefix("d:").and_then(|e| e.parse::<i32>().ok()) {
                    drops().lock().unwrap_or_else(|e| e.into_inner()).retain(|d| d.eid != eid);
                    broadcast_seen_destroy(eid);
                    killed += 1;
                } else if let Some(eid) = t.strip_prefix("t:").and_then(|e| e.parse::<i32>().ok()) {
                    primed().lock().unwrap_or_else(|e| e.into_inner()).retain(|t2| t2.eid != eid);
                    broadcast_seen_destroy(eid);
                    killed += 1;
                } else if let Some(eid) = t.strip_prefix("a:").and_then(|e| e.parse::<i32>().ok()) {
                    arrows().lock().unwrap_or_else(|e| e.into_inner()).retain(|x| x.eid != eid);
                    broadcast_seen_destroy(eid);
                    killed += 1;
                } else if let Some(eid) = t.strip_prefix("p:").and_then(|e| e.parse::<i32>().ok()) {
                    pearls().lock().unwrap_or_else(|e| e.into_inner()).retain(|x| x.eid != eid);
                    broadcast_seen_destroy(eid);
                    killed += 1;
                } else if let Some(eid) = t.strip_prefix("b:").and_then(|e| e.parse::<i32>().ok()) {
                    bobbers().lock().unwrap_or_else(|e| e.into_inner()).retain(|x| x.eid != eid);
                    broadcast_seen_destroy(eid);
                    killed += 1;
                } else if let Some(eid) = t.strip_prefix("x:").and_then(|e| e.parse::<i32>().ok()) {
                    xps().lock().unwrap_or_else(|e| e.into_inner()).retain(|x| x.eid != eid);
                    broadcast_seen_destroy(eid);
                    killed += 1;
                } else if let Ok(eid) = t.parse::<i32>() {
                    let cls: Vec<Arc<Client>> = client_snapshot();
                    if let Some(c) = cls.iter().find(|c| c.eid == eid) {
                        *c.hp.lock().unwrap_or_else(|e| e.into_inner()) = 0.0;
                        if let Ok(mut w) = c.sock.lock() { let _ = send_packet(&mut w, 0x5d, health_pkt_c(0.0, c)); }
                        killed += 1;
                    }
                }
            }
            format!("Killed {}", killed)
        }
        "gamemode" => {
            let gm = args.first().copied().unwrap_or("");
            let (v, flags) = match gm {
                "survival"|"0" => (0f32, 0x00u8), "creative"|"1" => (1.0, 0x0f),
                "adventure"|"2" => (2.0, 0x00), "spectator"|"3" => (3.0, 0x00),
                _ => return "usage: /gamemode survival|creative|adventure|spectator [player]".into() };
            // optional second arg: another player (or actor record)
            let target: Option<Arc<Client>> = if args.len() >= 2 {
                if actors().lock().unwrap_or_else(|e| e.into_inner()).contains_key(args[1]) {
                    return format!("Set gamemode of {} to {}", args[1], args[0])
                }
                match clients().lock().unwrap_or_else(|e| e.into_inner()).iter().find(|c| c.name == args[1]).cloned() {
                    Some(c) => Some(c),
                    None => return format!("unknown player '{}'", args[1]),
                }
            } else { me.cloned() };
            match target {
                Some(c) => {
                    *c.gm.lock().unwrap_or_else(|e| e.into_inner()) = v as u8;
                    let mut g = vec![3u8]; g.extend_from_slice(&v.to_be_bytes());
                    let mut ab = vec![flags]; ab.extend_from_slice(&0.05f32.to_be_bytes()); ab.extend_from_slice(&0.1f32.to_be_bytes());
                    if let Ok(mut w) = c.sock.lock() {
                        let _ = send_packet(&mut w, 0x22, g);
                        let _ = send_packet(&mut w, 0x38, ab);
                    }
                    // Gamemode feeds the anti-xray lane: switching to spectator
                    // reveals, switching back re-arms the hysteresis.
                    update_underground_lane(&c);
                    format!("Set {}'s gamemode to {}", c.name, gm)
                }
                None => "player only".into(),
            }
        }
        "save-all" | "save" => {
            // Flush any queued autosave first so an older pending snapshot
            // cannot be written over this one.
            shutdown_world_save();
            "Saved the world".into()
        }
        "kick" => {
            let Some(target_name) = args.first() else {
                return "usage: /kick <player> [reason]".into();
            };
            let target = client_snapshot()
                .into_iter()
                .find(|client| client.name.eq_ignore_ascii_case(target_name));
            let Some(target) = target else {
                return format!("That player does not exist: {target_name}");
            };
            let reason = if args.len() > 1 {
                args[1..].join(" ")
            } else {
                "Kicked by an operator.".to_owned()
            };
            let mut socket = target.sock.lock().unwrap_or_else(|e| e.into_inner());
            play_disconnect(&mut socket, &reason);
            let _ = socket.shutdown(std::net::Shutdown::Both);
            format!("Kicked {}: {}", target.name, reason)
        }
        "setidletimeout" => {
            let Some(minutes) = args.first().and_then(|value| value.parse::<u32>().ok()) else {
                return "usage: /setidletimeout <minutes>".into();
            };
            connection_limits::set_idle_timeout(Duration::from_secs(u64::from(minutes) * 60));
            format!("Set idle timeout to {minutes} minutes")
        }
        // Console/RCON-only diagnostic (Vanilla has no `status` literal, so it
        // deliberately never enters `command_tree_at_level` — player tab
        // completion stays 100% Vanilla). The console is the only source that
        // reaches this arm: it runs at level 4 with `me.is_none()`; a player
        // who types the word still gets the unknown-command reply.
        "status" if ctx.me.is_none() => {
            // RCON carries the reply in its response frame; the stdin console
            // sees it via the shared `[cmd]` echo below.
            format_operator_status(&operator_health_snapshot())
        }
        "stop" => {
            shutdown_world_save();
            std::thread::spawn(|| {
                std::thread::sleep(Duration::from_millis(150)); // flush reply
                stop_lan_announcer();
                disconnect_all_clients("Server closed");
                std::thread::sleep(Duration::from_millis(100));
                #[cfg(feature = "neoforge-events")]
                if let Some(runtime) = java_compat::NEOFORGE.get() {
                    runtime.cleanup_staged_mods();
                }
                std::process::exit(0);
            });
            "Stopping the server".into()
        }
        "clear" => run_clear_command(&args, me),
        "effect" => run_effect_command(&args, me),
        "xp" | "experience" => run_experience_command(&args, me),
        "setblock" => {
            if args.len() < 4 {
                return "usage: /setblock x y z <block>[states][{nbt}] [destroy|keep|replace]".into();
            }
            let (x, y, z) = (coord(args[0], ctx.x) as i32, coord(args[1], ctx.y) as i32, coord(args[2], ctx.z) as i32);
            let dim = cmd_dim(ctx);
            if let Err(e) = check_modification_bounds(dim, (x, y, z)) {
                return e;
            }
            // #97/#167 — the block arg may carry [states] plus a glued {snbt}
            // block-entity payload; a bare `{...}` token works the same way.
            let block_arg = args[3];
            let (block_name, mut nbt_text) = match block_arg.split_once('{') {
                Some((name, rest)) => (name, Some(format!("{{{rest}"))),
                None => (block_arg, None),
            };
            let mut mode_index = 4;
            if nbt_text.is_none() && args.get(4).is_some_and(|a| a.starts_with('{')) {
                nbt_text = Some(args[4].to_owned());
                mode_index = 5;
            }
            let mode = args.get(mode_index).copied().unwrap_or("replace");
            if args.len() > mode_index + 1 || !matches!(mode, "destroy" | "keep" | "replace") {
                return "usage: /setblock x y z <block>[states][{nbt}] [destroy|keep|replace]".into();
            }
            let nbt = match &nbt_text {
                Some(text) => {
                    let mut reader = brigadier::StringReader::new(text);
                    match execute_read_snbt(&mut reader, 0) {
                        Some(tag @ ExecTag::Compound(_)) if !reader.can_read() => Some(tag),
                        _ => return format!("Invalid block entity data '{text}'"),
                    }
                }
                None => None,
            };
            // Fail closed: an unresolved block id is an error, never silent air.
            let Some(st) = state_from_arg(block_name) else {
                return format!("Unknown block '{}'", block_name);
            };
            let old = block_at_dim(dim, x, y, z);
            let old_is_air = breg().map_or(old == COMMAND_AIR_STATE, |reg| reg.is_air(old));
            if mode == "keep" && !old_is_air {
                return "Could not set the block".into();
            }
            if mode == "destroy" && !old_is_air {
                set_block_dim(dim, x, y, z, COMMAND_AIR_STATE);
                // vanilla destroyBlock: LevelEvent 2001 (break particles + sound)
                // then the block's item drop under doTileDrops.
                let mut p = Vec::new();
                p.extend_from_slice(&2001i32.to_be_bytes());
                write_pos(&mut p, x, y, z);
                p.extend_from_slice(&(old as i32).to_be_bytes());
                p.push(0);
                broadcast_near(dim, f64::from(x) + 0.5, f64::from(y) + 0.5, f64::from(z) + 0.5, -1, |_c| {
                    (0x28, p.clone())
                });
                if dim == DimId::Overworld && GR_DO_TILE_DROPS.load(Ordering::Relaxed) != 0 {
                    let mut drops = drops().lock().unwrap_or_else(|e| e.into_inner());
                    let remaining = DROP_CAP.saturating_sub(drops.len());
                    drops.extend(
                        block_drops(old)
                            .into_iter()
                            .take(remaining)
                            .map(|item| Drop {
                                eid: NEXT_EID.fetch_add(1, Ordering::SeqCst),
                                x: f64::from(x) + 0.5,
                                y: f64::from(y) + 0.5,
                                z: f64::from(z) + 0.5,
                                item,
                                cnt: 1,
                                comps: Vec::new(),
                                age: 0,
                                target: None,
                                thrower: None,
                            }),
                    );
                }
            }
            place_block_dim(dim, x, y, z, st);
            if let Some(tag) = nbt {
                setblock_attach_nbt(dim, x, y, z, tag);
            }
            format!("Set block at {} {} {} in {}", x, y, z, dim.dimension_id())
        }
        "summon" => {
            if args.len() < 4 {
                return "usage: /summon <entity> x y z".into();
            }
            let Some(etype) = name_to_etype(args[0]) else {
                return format!("Unknown entity {}", args[0]);
            };
            let (x, y, z) = (coord(args[1], ctx.x), coord(args[2], ctx.y), coord(args[3], ctx.z));
            if ![x, y, z].into_iter().all(f64::is_finite)
                || x.abs() >= 30_000_000.0 || z.abs() >= 30_000_000.0 || y.abs() > 30_000_000.0
            {
                return "Invalid summon position".into();
            }
            let dim = cmd_dim(ctx);
            if etype == 106 && dim == DimId::Overworld {
                let eid = NEXT_EID.fetch_add(1, Ordering::SeqCst);
                primed().lock().unwrap_or_else(|e| e.into_inner()).push(Primed { eid, x, y, z, left: server_ticks(TNT_FUSE_GT) as i32 });
            } else if !insert_live_mob(etype, dim, (x, y, z, 0.0, 0.0), default_mob_hp(args[0]), None) {
                return "mob cap reached".into();
            }
            format!("Summoned {}", args[0])
        }
        "give" => {
            // /give <target> <item>[comps] [count] — the item arg may carry a
            // [k=v,...] component suffix (supported subset in parse_item_comps)
            let (targets, item_arg) = if args.first().map(|a| a.starts_with('@')).unwrap_or(false) {
                (resolve_selector(args[0], me), args.get(1).copied().unwrap_or(""))
            } else if args.first().map(|a| name_to_item(a.split('[').next().unwrap_or(a)).is_some()).unwrap_or(false) {
                (me.map(|c| vec![c.eid.to_string()]).unwrap_or_default(), args.first().copied().unwrap_or(""))
            } else {
                let name = args.first().copied().unwrap_or("");
                let t = { clients().lock().unwrap_or_else(|e| e.into_inner()).iter().find(|c| c.name == name).map(|c| c.eid.to_string()) };
                (t.into_iter().collect(), args.get(1).copied().unwrap_or(""))
            };
            {
            let item_name = item_arg.split('[').next().unwrap_or(item_arg);
            let comps = parse_item_comps(item_arg);
            let mut gave_names: Vec<String> = Vec::new();
            {
            let cls: Vec<Arc<Client>> = client_snapshot();
            let mut gave = 0;
            for t in &targets {
                if let Some(c) = cls.iter().find(|c| c.eid.to_string() == *t) {
                if let Some(item) = name_to_item(item_name) {
                    let cnt = args.iter().rev().find_map(|a| a.parse::<u8>().ok()).unwrap_or(1);
                    gave_names.push(c.name.clone());
                    let st: Stack = (item, cnt, comps.clone());
                    let mut inv = c.inv.lock().unwrap_or_else(|e| e.into_inner());
                    // vanilla fills hotbar (36-44) before main inv (9-35), and
                    // merges into a matching partial stack first
                    let slot: i16 = (36..=44i16).chain(9..=35)
                        .find(|sl| inv.get(sl).map(|(it, cn, cc)| *it == item
                            && *cc == comps && (*cn as i64 + cnt as i64) <= item_stack_max(item, &comps) as i64).unwrap_or(false))
                        .or_else(|| (36..=44).chain(9..=35).find(|sl| !inv.contains_key(sl)))
                        .unwrap_or(36);
                    let st = if let Some(st0) = inv.get_mut(&slot) { st0.1 = st0.1.saturating_add(cnt); st0.clone() } else { inv.insert(slot, st.clone()); st };
                    let mut p = vec![0u8]; write_varint(&mut p, 0);
                    p.extend_from_slice(&slot.to_be_bytes());
                    p.extend_from_slice(&slot_wire(&st));
                    if let Ok(mut w) = c.sock.lock() { let _ = send_packet(&mut w, 0x15, p); }
                    gave += 1;
                }
                }
            }
            drop(cls);
            for n in &gave_names { adv_event(n, "inventory_changed", None); }
            format!("Gave {} x{} to {} target(s)", item_name,
                args.iter().rev().find_map(|a| a.parse::<u8>().ok()).unwrap_or(1), gave)
            }
            }
        }
        "enchant" => {
            // /enchant <target> <enchantment> [level] — enchants the held item
            let (targets, en_name) = if args.first().map(|a| a.starts_with('@')).unwrap_or(false) {
                (resolve_selector(args[0], me), args.get(1).copied().unwrap_or(""))
            } else {
                let name = args.first().copied().unwrap_or("");
                let t = { clients().lock().unwrap_or_else(|e| e.into_inner()).iter().find(|c| c.name == name).map(|c| c.eid.to_string()) };
                (t.into_iter().collect(), args.get(1).copied().unwrap_or(""))
            };
            let Some(eid) = enchant_id(en_name) else {
                return format!("Unknown enchantment: {}", en_name)
            };
            let lvl = match args.as_slice() {
                [_, _] => 1,
                [_, _, level] => match level.parse::<i64>() {
                    Ok(level) => level,
                    Err(_) => return "Invalid enchantment level".into(),
                },
                _ => return "Usage: /enchant <target> <enchantment> [level]".into(),
            };
            if lvl < 1 || lvl > enchant_max_level(eid) {
                return format!("Level {} not supported for {} (max {})", lvl, en_name, enchant_max_level(eid))
            }
            let mut resync: Vec<Arc<Client>> = Vec::new();
            let mut done = 0;
            {
                let cls: Vec<Arc<Client>> = client_snapshot();
                for t in &targets {
                    let Some(c) = cls.iter().find(|c| c.eid.to_string() == *t) else { continue };
                    let slot = 36 + *c.held.lock().unwrap_or_else(|e| e.into_inner());
                    let mut inv = c.inv.lock().unwrap_or_else(|e| e.into_inner());
                    let Some(st) = inv.get_mut(&slot) else { continue };
                    if !enchant_applicable(st.0, eid) { continue }
                    let cur = enchantments_get(&st.2);
                    if cur.iter().any(|(o, _)| enchant_conflicts(eid, *o)) { continue }
                    let mut list = cur; list.retain(|(x, _)| *x != eid); list.push((eid, lvl));
                    comp_set(&mut st.2, COMP_ENCHANTMENTS, enchantments_data(&list, true));
                    drop(inv);
                    resync.push(c.clone()); done += 1;
                }
            }
            for c in &resync { held_slot_resync(c); }
            format!("Enchanted {} item(s)", done)
        }
        "item" => {
            // /item modify|replace entity <sel> <slot> <modifier|with item>
            //            block <x> <y> <z> <slot> ...
            // slots (window-0 keys): weapon.mainhand, weapon.offhand,
            // armor.head|chest|legs|feet, hotbar.N, inventory.N, container.N
            let slot_of = |sp: &str, held: i16| -> Option<i16> {
                Some(match sp {
                    "weapon.mainhand" => 36 + held,
                    "weapon.offhand" => 45,
                    "armor.head" => 5, "armor.chest" => 6,
                    "armor.legs" => 7, "armor.feet" => 8,
                    _ => {
                        {
                            let n = sp.strip_prefix("hotbar.").or_else(|| sp.strip_prefix("inventory.")).or_else(|| sp.strip_prefix("container."))?;
                            n.parse::<i16>().ok().map(|x| 9 + x)?
                        }
                    }
                })
            };
            let Some(action) = args.first().copied() else { return "usage: /item modify|replace ...".into() };
            match action {
                "replace" => {
                    // carpet-actor held-item fast path: replace entity
                    // <actor> weapon.mainhand with <item> [count]
                    if args.get(1) == Some(&"entity") && args.len() >= 6
                        && !clients().lock().unwrap_or_else(|e| e.into_inner()).iter().any(|c| c.name == args[2])
                        && actors().lock().unwrap_or_else(|e| e.into_inner()).contains_key(args[2]) {
                        let a = actors().lock().unwrap_or_else(|e| e.into_inner());
                        let a = a.get(args[2]).unwrap();
                        let it = name_to_item(args.get(5).copied().unwrap_or("")
                            .split('[').next().unwrap_or("")).unwrap_or(0);
                        let cnt = args.get(6).and_then(|v| v.parse::<u8>().ok()).unwrap_or(1);
                        *a.held.lock().unwrap_or_else(|e| e.into_inner()) = (it, cnt, parse_item_comps(args[5]));
                        return "ok".into();
                    }
                    if args.get(1) == Some(&"entity") && args.len() >= 6 {
                        let tgts = resolve_selector(args[2], me);
                        let (slot_sp, item_arg) = (args[3], args[5]);
                        let cnt = args.get(6).and_then(|a| a.parse::<u8>().ok()).unwrap_or(1);
                        let item_name = item_arg.split('[').next().unwrap_or(item_arg);
                        let comps = parse_item_comps(item_arg);
                        let Some(item) = name_to_item(item_name) else {
                            return format!("Unknown item: {}", item_name)
                        };
                        let mut done = 0;
                        {
                            let cls: Vec<Arc<Client>> = client_snapshot();
                            for t in &tgts {
                                let Some(c) = cls.iter().find(|c| c.eid.to_string() == *t) else { continue };
                                let Some(slot) = slot_of(slot_sp, *c.held.lock().unwrap_or_else(|e| e.into_inner())) else { continue };
                                {
                                    let mut inv = c.inv.lock().unwrap_or_else(|e| e.into_inner());
                                    inv.insert(slot, (item, cnt, comps.clone()));
                                    *c.armor_pts.lock().unwrap_or_else(|e| e.into_inner()) = armor_pts_of(&inv);
                                }
                                {   // resync the touched slot (held fast path + generic)
                                    let mut p = vec![0u8]; write_varint(&mut p, 0);
                                    p.extend_from_slice(&slot.to_be_bytes());
                                    p.extend_from_slice(&slot_wire(&(item, cnt, comps.clone())));
                                    if let Ok(mut w) = c.sock.lock() { let _ = send_packet(&mut w, 0x15, p); }
                                }
                                done += 1;
                            }
                        }
                        format!("Replaced item on {} target(s)", done)
                    } else if args.get(1) == Some(&"block") && args.len() >= 8 {
                        // item replace block x y z container.N with <item>[comps] [count]
                        let (bx, by, bz) = (coord(args[2], ctx.x) as i32, coord(args[3], ctx.y) as i32, coord(args[4], ctx.z) as i32);
                        let slot: i16 = args[5].strip_prefix("container.").and_then(|v| v.parse().ok()).unwrap_or(0);
                        let item_arg = args.get(7).copied().unwrap_or("");
                        let comps = parse_item_comps(item_arg);
                        let item = name_to_item(item_arg.split('[').next().unwrap_or(item_arg)).unwrap_or(0);
                        let cnt = args.get(8).and_then(|v| v.parse::<u8>().ok()).unwrap_or(1);
                        let key = (bx, by, bz);
                        if is_furnace_at(bx, by, bz) && slot <= 2 {
                            let mut fs = furnaces().lock().unwrap_or_else(|e| e.into_inner());
                            let f = fs.entry(ow_block(key)).or_insert(Furnace { input: (0,0,Vec::new()), fuel: (0,0,Vec::new()), output: (0,0,Vec::new()), burn_left: 0, progress: 0, dirty: false, xp: 0.0, recipe_cache: CachedLookup::default(), fuel_cache: CachedLookup::default() });
                            let cell = match slot { 0 => &mut f.input, 1 => &mut f.fuel, _ => &mut f.output };
                            *cell = (item, cnt, comps); f.dirty = true;
                        } else {
                            unpack_block_loot_if_needed(ow_block(key));
                            let mut ch = chests().lock().unwrap_or_else(|e| e.into_inner());
                            let m = ch.entry(ContainerKey::ow(key)).or_default();
                            if item == 0 { m.remove(&slot); } else { m.insert(slot, (item, cnt, comps)); }
                        }
                        format!("Replaced slot {} in block", slot)
                    } else { "usage: /item replace entity|block ... with <item>[comps] [count]".into() }
                }
                "modify" => {
                    if args.get(1) == Some(&"entity") && args.len() >= 5 {
                        let tgts = resolve_selector(args[2], me);
                        let (slot_sp, mod_id) = (args[3], args[4]);
                        let fns = item_mods().lock().unwrap_or_else(|e| e.into_inner()).get(mod_id).cloned();
                        let Some(fns) = fns else { return format!("Unknown item modifier: {}", mod_id) };
                        let mut done = 0;
                        {
                            let cls: Vec<Arc<Client>> = client_snapshot();
                            for t in &tgts {
                                let Some(c) = cls.iter().find(|c| c.eid.to_string() == *t) else { continue };
                                let Some(slot) = slot_of(slot_sp, *c.held.lock().unwrap_or_else(|e| e.into_inner())) else { continue };
                                let mut inv = c.inv.lock().unwrap_or_else(|e| e.into_inner());
                                if let Some(st) = inv.get_mut(&slot) { apply_loot_funcs(st, &fns); done += 1; }
                                let ns = inv.get(&slot).cloned().unwrap_or((0, 0, Vec::new()));
                                drop(inv);
                                {   let mut p = vec![0u8]; write_varint(&mut p, 0);
                                    p.extend_from_slice(&slot.to_be_bytes());
                                    p.extend_from_slice(&slot_wire(&ns));
                                    if let Ok(mut w) = c.sock.lock() { let _ = send_packet(&mut w, 0x15, p); } }
                            }
                        }
                        format!("Modified item on {} target(s)", done)
                    } else if args.get(1) == Some(&"block") && args.len() >= 7 {
                        let (bx, by, bz) = (coord(args[2], ctx.x) as i32, coord(args[3], ctx.y) as i32, coord(args[4], ctx.z) as i32);
                        let mod_id = args[6];
                        let fns = item_mods().lock().unwrap_or_else(|e| e.into_inner()).get(mod_id).cloned();
                        let Some(fns) = fns else { return format!("Unknown item modifier: {}", mod_id) };
                        let cslot: i16 = args[5].trim_start_matches("container.").parse().unwrap_or(-1);
                        unpack_block_loot_if_needed(ow_block((bx, by, bz)));
                        let mut ch = chests().lock().unwrap_or_else(|e| e.into_inner());
                        let ok = ch.get_mut(&ContainerKey::ow((bx, by, bz))).and_then(|m| m.get_mut(&cslot))
                            .map(|st| { apply_loot_funcs(st, &fns); }).is_some();
                        (if ok { "Modified item".to_string() } else { "No item in slot".to_string() }).to_string()
                    } else { "usage: /item modify entity|block ... <modifier>".into() }
                }
                _ => "usage: /item modify|replace ...".into(),
            }
        }
        "weather" => {
            // #96/#173 — vanilla `/weather clear|rain|thunder [<duration>]`. An
            // omitted duration is a seeded roll in vanilla's per-mode range,
            // and a malformed one fails closed instead of clearing silently.
            let w = args.first().copied().unwrap_or("");
            if !matches!(w, "clear" | "rain" | "thunder") || args.len() > 2 {
                return "usage: /weather clear|rain|thunder [<duration>]".into();
            }
            let ticks: i64 = match args.get(1) {
                Some(raw) => match weather_time_argument(raw) {
                    Some(t) => t,
                    None => return "usage: /weather clear|rain|thunder [<duration>]".into(),
                },
                None => weather_default_ticks(w),
            };
            let dur = ticks.min(i64::from(i32::MAX)) as i32;
            let (code, ticks) = match w { "rain" => (1u8, dur), "thunder" => (1, dur), _ => (2u8, 0) };
            RAINING.store(ticks.max(0) as u64, Ordering::Relaxed);
            THUNDERING.store(if w == "thunder" { ticks.max(0) as u64 } else { 0 }, Ordering::Relaxed);
            broadcast(-1, move |_c| { let mut p = vec![8u8]; p.extend_from_slice(&(if w == "thunder" { 1f32 } else { 0f32 }).to_be_bytes()); (0x22, p.clone()) });
            broadcast(-1, move |_c| { let mut p = vec![code]; p.extend_from_slice(&ticks.to_be_bytes()); (0x22, p.clone()) });
            format!("Weather: {}", w)
        }
        "function" => {
            if let Some(name) = args.first() {
                match function_macro_args(&args[1..]) {
                    Ok(macro_args) => format!(
                        "ran {} commands",
                        run_function_depth(name, me.cloned(), s, 0, macro_args.as_ref())
                    ),
                    Err(error) => error,
                }
            } else { "usage: /function <ns:name> [with <compound>]".into() }
        }
        "loot" => run_loot_command(ctx, &args, me),
        "playsound" => playsound_command(ctx, &args, me),
        "title" => {
            if args.len() < 3 { return "usage: /title <targets> title|subtitle|actionbar|times <text>".into(); }
            // /title <targets> title|subtitle|actionbar|times <text|i s o>
            let targets = resolve_selector(args.first().copied().unwrap_or(""), me);
            let mode = args.get(1).copied().unwrap_or("title");
            let text = args.get(2..).unwrap_or_default().join(" ");
            let pkt_id: i64 = match mode { "title" => 0x65, "subtitle" => 0x63, "actionbar" => 0x4c, _ => -1 };
            if mode == "times" {
                let (a, b, c): (i32, i32, i32) = (args.get(2).and_then(|v| v.parse().ok()).unwrap_or(10),
                                 args.get(3).and_then(|v| v.parse().ok()).unwrap_or(70),
                                 args.get(4).and_then(|v| v.parse().ok()).unwrap_or(20));
                let mut p = Vec::new();
                p.extend_from_slice(&a.to_be_bytes()); p.extend_from_slice(&b.to_be_bytes()); p.extend_from_slice(&c.to_be_bytes());
                let mut n = 0;
                for e in &targets {
                    let found = clients().lock().unwrap_or_else(|e| e.into_inner()).iter().find(|c2| c2.eid.to_string() == *e).cloned();
                    if let Some(c2) = found {
                        if let Ok(mut w) = c2.sock.lock() { let _ = send_packet(&mut w, 0x66, p.clone()); n += 1; }
                    }
                }
                return format!("Set title times for {} player(s)", n);
            }
            if pkt_id < 0 { return "usage: /title <targets> title|subtitle|actionbar|times <text>".into() }
            // accept raw json {"text":".."} or plain text
            let txt = if text.starts_with('{') {
                jstr(&text, "text").unwrap_or(&text).to_string() } else { text };
            let payload = chat_nbt(&txt);
            let mut n = 0;
            for e in &targets {
                let found = clients().lock().unwrap_or_else(|e| e.into_inner()).iter().find(|c2| c2.eid.to_string() == *e).cloned();
                    if let Some(c2) = found {
                    if let Ok(mut w) = c2.sock.lock() { let _ = send_packet(&mut w, pkt_id, payload.clone()); n += 1; }
                }
            }
            format!("Title {} to {} player(s)", mode, n)
        }
        "particle" => particle_command(ctx, &args, me),
        "spreadplayers" => spreadplayers_command(ctx, &args, me),
        "trigger" => run_trigger_command(&args, me),
        "attribute" => run_attribute_command(&args, me),
        "tellraw" => {
            // /tellraw <targets> <json-component>
            let targets = resolve_selector(args.first().copied().unwrap_or(""), me);
            let payload = json_component_nbt(&args[1..].join(" "));
            let mut n = 0;
            for e in &targets {
                let found = clients().lock().unwrap_or_else(|e| e.into_inner()).iter().find(|c| c.eid.to_string() == *e).cloned();
                if let Some(c) = found {
                    let mut p = payload.clone(); p.push(0);
                    if let Ok(mut w) = c.sock.lock() { let _ = send_packet(&mut w, 0x6c, p); n += 1; }
                }
            }
            format!("Sent to {} player(s)", n)
        }
        "stopsound" => {
            // /stopsound <targets> [source] [sound]
            let targets = resolve_selector(args.first().copied().unwrap_or(""), me);
            let snd = args.get(2).copied().unwrap_or("");
            let mut n = 0;
            for e in &targets {
                let found = clients().lock().unwrap_or_else(|e| e.into_inner()).iter().find(|c| c.eid.to_string() == *e).cloned();
                if let Some(c) = found {
                    let mut p = Vec::new();
                    if snd.is_empty() { p.push(0u8); }
                    else { p.push(2u8);
                        let sn = if snd.contains(':') { snd.to_string() } else { format!("minecraft:{}", snd) };
                        write_str(&mut p, &sn); }
                    if let Ok(mut w) = c.sock.lock() { let _ = send_packet(&mut w, 0x6a, p); n += 1; }
                }
            }
            format!("Stopped sound for {} player(s)", n)
        }
        "bossbar" => run_bossbar_command(&args, me),
        "advancement" => run_advancement_command(&args, me),
        "datapack" => run_datapack_command(&args),
        "worldborder" => run_world_border_command(ctx, &args),
        "scoreboard" => run_scoreboard_command(&args, me),
        "fill" | "clone" => run_block_modification(ctx, cmd, &args).unwrap_or_else(|error| error),
        "forceload" => {
            // forceload add|remove <x1> <z1> [x2 z2] | query — keeps chunks ticking with no players online
            match args.first().copied().unwrap_or("") {
                "remove" if args.get(1) == Some(&"all") => {
                    let mut forced = forceloaded().lock().unwrap_or_else(|e| e.into_inner());
                    let removed = forced.len();
                    forced.clear();
                    format!("Removed {} forceloaded chunk(s)", removed)
                }
                "add" | "remove" if args.len() >= 3 => {
                    let add = args[0] == "add";
                    let (x1, z1) = (coord(args[1], ctx.x) as i32, coord(args[2], ctx.z) as i32);
                    let (x2, z2) = if args.len() >= 5 { (coord(args[3], ctx.x) as i32, coord(args[4], ctx.z) as i32) } else { (x1, z1) };
                    let (cx1, cx2) = (x1.div_euclid(16).min(x2.div_euclid(16)), x1.div_euclid(16).max(x2.div_euclid(16)));
                    let (cz1, cz2) = (z1.div_euclid(16).min(z2.div_euclid(16)), z1.div_euclid(16).max(z2.div_euclid(16)));
                    let mut f = forceloaded().lock().unwrap_or_else(|e| e.into_inner());
                    let mut n = 0;
                    for cx in cx1..=cx2 { for cz in cz1..=cz2 {
                        if add { n += f.insert((cx, cz)) as usize } else { n += f.remove(&(cx, cz)) as usize }
                    }}
                    format!("Marked chunk for forceload ({} chunks)", n)
                }
                "query" if args.len() >= 3 => {
                    let (x, z) = (coord(args[1], ctx.x) as i32, coord(args[2], ctx.z) as i32);
                    let chunk = (x.div_euclid(16), z.div_euclid(16));
                    if forceloaded().lock().unwrap_or_else(|e| e.into_inner()).contains(&chunk) {
                        format!("Chunk {}, {} is force loaded", chunk.0, chunk.1)
                    } else {
                        format!("Chunk {}, {} is not force loaded", chunk.0, chunk.1)
                    }
                }
                "query" => format!("{} forceloaded chunk(s)", forceloaded().lock().unwrap_or_else(|e| e.into_inner()).len()),
                _ => "usage: /forceload add|remove x z [x2 z2] | query".into(),
            }
        }
        "gamerule" => {
            let Some(name) = args.first() else {
                let rules = command_gamerules().lock().unwrap_or_else(|e| e.into_inner());
                return COMMAND_GAMERULE_DEFAULTS.iter().map(|&(name, is_bool, _)| {
                    let value = gamerule_atomic_value(name).unwrap_or(rules[name]);
                    let value = if is_bool { (value != 0).to_string() } else { value.to_string() };
                    format!("{name}={value}")
                }).collect::<Vec<_>>().join(", ");
            };
            let Some(&(canonical, is_bool, _)) = COMMAND_GAMERULE_DEFAULTS.iter()
                .find(|(rule, _, _)| rule.eq_ignore_ascii_case(name)) else {
                return format!("unknown gamerule '{name}'");
            };
            if args.len() > 2 { return format!("usage: /gamerule {name} [value]"); }
            let mut rules = command_gamerules().lock().unwrap_or_else(|e| e.into_inner());
            let value = if let Some(raw) = args.get(1) {
                let value = if is_bool {
                    match *raw { "true" => 1, "false" => 0,
                        _ => return format!("usage: /gamerule {name} true|false") }
                } else {
                    match raw.parse::<i32>() { Ok(value) => value,
                        Err(_) => return format!("usage: /gamerule {name} <int>") }
                };
                gamerule_atomic_store(canonical, value);
                rules.insert(canonical, value);
                value
            } else {
                gamerule_atomic_value(canonical).unwrap_or(rules[canonical])
            };
            let value = if is_bool { (value != 0).to_string() } else { value.to_string() };
            if args.len() == 2 { format!("Gamerule {canonical} is now set to: {value}") }
            else { format!("{canonical} = {value}") }
        }
        "tag" => run_tag_command(&args, me),
        "damage" => run_damage_command(ctx, &args, me),
        "schedule" => {
            // schedule function <fn> <time>[t|s|d] — t=game ticks(20/s), s=seconds
            if args.first().copied() == Some("clear") {
                if args.len() == 1 {
                    let mut scheduled = sched_fns().lock().unwrap_or_else(|e| e.into_inner());
                    let removed = clear_scheduled_functions(&mut scheduled, None);
                    format!("Removed {} scheduled function(s)", removed)
                } else if args.len() == 2 {
                    let mut scheduled = sched_fns().lock().unwrap_or_else(|e| e.into_inner());
                    let removed = clear_scheduled_functions(&mut scheduled, Some(args[1]));
                    format!("Removed {} scheduled function(s)", removed)
                } else {
                    "usage: /schedule clear [<function>]".into()
                }
            } else if args.first().copied() == Some("function") && (args.len() == 3 || args.len() == 4) {
                let f = args[1];
                if time_argument_ticks(args[2]).is_none() {
                    return "usage: /schedule function <fn> <time>[t|s|d] [append|replace]".into();
                }
                let off = schedule_delay_ticks(args[2]);
                let t2 = TICK.load(Ordering::Relaxed);
                let replace = match args.get(3).copied().unwrap_or("replace") {
                    "append" => false,
                    "replace" => true,
                    _ => return "usage: /schedule function <fn> <time>[t|s|d] [append|replace]".into(),
                };
                let mut scheduled = sched_fns().lock().unwrap_or_else(|e| e.into_inner());
                schedule_function(&mut scheduled, f, t2 + off.max(1), !replace);
                format!("Scheduled function '{}' in {} ticks", f, { off.max(1) })
            } else { "usage: /schedule clear [<function>] | function <fn> <time>[t|s|d] [append|replace]".into() }
        }
        "counts" => {
            // debug: live entity/store census (non-vanilla, fixture aid)
            format!("mobs={} drops={} xps={} arrows={} bobbers={} pearls={} primed={} chests={} furnaces={} signs={}",
                mobs().lock().unwrap_or_else(|e| e.into_inner()).len(), drops().lock().unwrap_or_else(|e| e.into_inner()).len(),
                xps().lock().unwrap_or_else(|e| e.into_inner()).len(), arrows().lock().unwrap_or_else(|e| e.into_inner()).len(),
                bobbers().lock().unwrap_or_else(|e| e.into_inner()).len(), pearls().lock().unwrap_or_else(|e| e.into_inner()).len(),
                primed().lock().unwrap_or_else(|e| e.into_inner()).len(), chests().lock().unwrap_or_else(|e| e.into_inner()).len(),
                furnaces().lock().unwrap_or_else(|e| e.into_inner()).len(), signs().lock().unwrap_or_else(|e| e.into_inner()).len())
                + &format!(" recipes={} tags={}", json_recipes().lock().unwrap_or_else(|e| e.into_inner()).len(),
                    block_tags().lock().unwrap_or_else(|e| e.into_inner()).len())
        }
                "player" => {
            // carpet-actor subset: spawn / look at / attack|use continuous / stop / kill
            let name = args.first().copied().unwrap_or("").to_string();
            let sub = args.get(1).copied().unwrap_or("");
            match sub {
                "spawn" => {
                    let mut acts = actors().lock().unwrap_or_else(|e| e.into_inner());
                    if acts.len() < 64 && !acts.contains_key(&name) {
                        acts.insert(name.clone(), Arc::new(Actor {
                            name: name.clone(), eid: NEXT_EID.fetch_add(1, Ordering::SeqCst),
                            pos: Mutex::new((ctx.x, ctx.y, ctx.z)), look: Mutex::new(None),
                            action: Mutex::new(""), held: Mutex::new((0, 0, Vec::new())), inv: Mutex::new(HashMap::new()),
                        }));
                        format!("Spawned {}", name)
                    } else { format!("{} already spawned", name) }
                }
                "look" => {
                    if let Some(a) = actors().lock().unwrap_or_else(|e| e.into_inner()).get(&name) {
                        *a.look.lock().unwrap_or_else(|e| e.into_inner()) = Some((coord(args.get(3).copied().unwrap_or("~"), ctx.x) as i32,
                            coord(args.get(4).copied().unwrap_or("~"), ctx.y) as i32,
                            coord(args.get(5).copied().unwrap_or("~"), ctx.z) as i32));
                        format!("{} looking", name)
                    } else { format!("no actor {}", name) }
                }
                "attack" | "use" => {
                    if let Some(a) = actors().lock().unwrap_or_else(|e| e.into_inner()).get(&name) {
                        *a.action.lock().unwrap_or_else(|e| e.into_inner()) = if sub == "attack" { "attack" } else { "use" };
                        format!("{} -> {}", name, sub)
                    } else { format!("no actor {}", name) }
                }
                "stop" => { if let Some(a) = actors().lock().unwrap_or_else(|e| e.into_inner()).get(&name) { *a.action.lock().unwrap_or_else(|e| e.into_inner()) = ""; } format!("{} stopped", name) }
                "kill" => { actors().lock().unwrap_or_else(|e| e.into_inner()).remove(&name); format!("Killed {}", name) }
                _ => format!("player: unknown subcommand {}", sub),
            }
        }
        "data" => run_data_command(ctx, &args),
        // `/profile` is owner-level (4) and `/transfer` moderator-level (3);
        // both handlers re-check `level` because `required_permission_level`
        // defaults unmapped names to 2 and a level-2 `.mcfunction` frame must
        // never reach either command.
        "profile" => run_profile_command(level, &args),
        "transfer" => run_transfer_command(ctx, level, &args),
        // P8c cluster-C diagnostics are owner-level (4) too; each handler
        // re-checks `level` like `run_profile_command` does.
        "debug" => run_debug_command(ctx, level, &args, s),
        "jfr" => run_jfr_command(level, &args),
        "perf" => run_perf_command(level, &args),
        "publish" => run_publish_command(level, &args),
        "list" => {
            let c = clients().lock().unwrap_or_else(|e| e.into_inner());
            let names: Vec<String> = c.iter().map(|x| x.name.clone()).collect();
            format!("{} players: {}", names.len(), names.join(", "))
        }
        "ban" | "ban-ip" | "banlist" | "pardon" | "pardon-ip" | "whitelist" => {
            run_server_ops_command(ctx, cmd, &args)
        }
        "team" | "teammsg" | "tm" | "tick" | "locate" | "random" | "return" | "ride"
        | "spectate" | "fillbiome" | "place" => {
            exec_p3b_command(ctx, cmd, &args, s)
        }
        "recipe" => run_recipe_command(ctx, &args),
        "seed" => WORLD_SEED.load(Ordering::Relaxed).to_string(),
        "" => "".into(),
        _ => format!("Unknown command: {}", cmd),
    };
    if !reply.is_empty() {
        if let Some(client) = s {
            // sendCommandFeedback suppresses success output to the executor —
            // errors still surface so a mistyped command is never silently eaten.
            if GR_SEND_COMMAND_FEEDBACK.load(Ordering::Relaxed) != 0
                || command_reply_is_error(&reply)
            {
                let _ = send_packet(&mut client.sock.lock().unwrap_or_else(|e| e.into_inner()), 0x6c, system_chat(&reply));
            }
        } else {
            eprintln!("[cmd] {}", reply);
        }
    }
    reply
}

/// Disconnect one online player the way `/kick` does.
fn kick_client(client: &Arc<Client>, text: &str) {
    let mut socket = client.sock.lock().unwrap_or_else(|e| e.into_inner());
    play_disconnect(&mut socket, text);
    let _ = socket.shutdown(std::net::Shutdown::Both);
}

/// `/ban`, `/ban-ip`, `/banlist`, `/pardon`, `/pardon-ip`, `/whitelist` against
/// the live registries in the server working directory (where the Vanilla-named
/// files live). After a mutation, online players who no longer pass Vanilla's
/// `canPlayerLogin` order are dropped: a fresh player/IP ban disconnects the
/// matching clients, and an enforced whitelist disconnects everyone who is not
/// whitelisted or an operator.
fn run_server_ops_command(ctx: &CmdCtx, cmd: &str, args: &[&str]) -> String {
    let input = std::iter::once(cmd)
        .chain(args.iter().copied())
        .collect::<Vec<_>>()
        .join(" ");
    let source = ctx
        .me
        .as_ref()
        .map(|c| c.name.clone())
        .unwrap_or_else(|| "Server".to_owned());
    // `created` wants a real timestamp (entries persist across restarts):
    // reuse the advancement lane's epoch read rather than add a clock site.
    let now = advancement_time_ms().to_string();
    let dir = std::path::Path::new(".");
    // First ops command loads the persisted files once, so a `/whitelist add`
    // or `/ban` cannot overwrite entries the files already held.
    static OPS_FILES_LOADED: AtomicBool = AtomicBool::new(false);
    if !OPS_FILES_LOADED.swap(true, Ordering::Relaxed) {
        match BanRegistry::load(dir) {
            Ok(bans) => *ban_registry().lock().unwrap_or_else(|e| e.into_inner()) = bans,
            Err(error) => eprintln!("[spine] could not load ban lists: {error}"),
        }
        match WhitelistRegistry::load(&dir.join(WHITELIST_FILE)) {
            Ok(list) => *whitelist_registry().lock().unwrap_or_else(|e| e.into_inner()) = list,
            Err(error) => eprintln!("[spine] could not load {WHITELIST_FILE}: {error}"),
        }
    }
    let (reply, kicks) = {
        let mut bans = ban_registry().lock().unwrap_or_else(|e| e.into_inner());
        let mut whitelist = whitelist_registry().lock().unwrap_or_else(|e| e.into_inner());
        exec_server_ops_command(&input, &mut bans, &mut whitelist, dir, &source, &now)
    };
    let ip_ban_reason = |client: &Arc<Client>| -> Option<String> {
        let ip = client.sock.lock().unwrap_or_else(|e| e.into_inner()).peer_addr().ok()?.ip().to_string();
        ban_registry()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .ip_ban(&ip)
            .map(|ban| {
                format!(
                    "Your IP address is banned from this server.\nReason: {}",
                    ban.reason
                )
            })
    };
    for client in client_snapshot() {
        if kicks
            .iter()
            .any(|name| client.name.eq_ignore_ascii_case(name))
        {
            let text = ban_registry()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .player_ban(&client.name)
                .map(|ban| {
                    format!("You are banned from this server.\nReason: {}", ban.reason)
                })
                .unwrap_or_else(|| "You are banned from this server.".to_owned());
            kick_client(&client, &text);
        } else if cmd == "ban-ip" {
            if let Some(text) = ip_ban_reason(&client) {
                kick_client(&client, &text);
            }
        }
    }
    if whitelist_registry().lock().unwrap_or_else(|e| e.into_inner()).enabled {
        for client in client_snapshot() {
            let admitted = whitelist_registry().lock().unwrap_or_else(|e| e.into_inner()).contains(&client.name)
                || ops_registry().lock().unwrap_or_else(|e| e.into_inner()).is_op(&client.name);
            if !admitted {
                kick_client(&client, "You are not white-listed on this server!");
            }
        }
    }
    reply
}

/// `/recipe give|take <targets> <recipe|*>` — applies the delta to each target's
/// `PlayerRecipeBook` and pushes the S2C `0x41` updates the outcome carries.
fn run_recipe_command(ctx: &CmdCtx, args: &[&str]) -> String {
    let &[verb, targets, recipe] = args else {
        return "usage: /recipe <give|take> <targets> <recipe|*>".into();
    };
    let verb = match verb {
        "give" => RecipeCommandVerb::Give,
        "take" => RecipeCommandVerb::Take,
        _ => return "usage: /recipe <give|take> <targets> <recipe|*>".into(),
    };
    let universe: Vec<String> = recipe_ids().lock().unwrap_or_else(|e| e.into_inner()).iter().cloned().collect();
    if recipe != "*" && !universe.iter().any(|id| id == recipe) {
        return format!("Unknown recipe '{recipe}'");
    }
    let targets = player_command_targets(targets, ctx.me.as_ref());
    if targets.is_empty() {
        return "No player found".into();
    }
    let mut books = recipe_books().lock().unwrap_or_else(|e| e.into_inner());
    let mut total_changed = 0usize;
    let mut players_changed = 0usize;
    for client in &targets {
        let book = books
            .entry(client.uuid)
            .or_insert_with(PlayerRecipeBook::new);
        let Some(outcome) = exec_recipe_command(book, verb, &universe, recipe) else {
            return format!("Unknown recipe '{recipe}'");
        };
        if !outcome.changed.is_empty() {
            players_changed += 1;
            total_changed += outcome.changed.len();
            for payload in outcome.packets {
                let _ = send_packet(&mut client.sock.lock().unwrap_or_else(|e| e.into_inner()), 0x41, payload);
            }
        }
    }
    let single = targets.len() == 1;
    let name = targets[0].name.as_str();
    match (verb, total_changed, single) {
        (RecipeCommandVerb::Give, 0, _) => "No new recipes could be given".into(),
        (RecipeCommandVerb::Give, n, true) => format!("Gave {n} new recipes to {name}"),
        (RecipeCommandVerb::Give, n, false) => format!("Gave {n} new recipes to {players_changed} players"),
        (RecipeCommandVerb::Take, 0, _) => "No recipes could be taken".into(),
        (RecipeCommandVerb::Take, n, true) => format!("Took {n} recipes from {name}"),
        (RecipeCommandVerb::Take, n, false) => format!("Took {n} recipes from {players_changed} players"),
    }
}

/// Whether a command reply is a failure the player should see even when
/// `sendCommandFeedback` is off. The replies are plain strings, so the check
/// names the failure phrasings the handlers actually produce.
fn command_reply_is_error(reply: &str) -> bool {
    let lower = reply.to_ascii_lowercase();
    [
        "usage:",
        "unknown command",
        "unknown gamerule",
        "unknown ",
        "invalid ",
        "expected ",
        "cannot ",
        "can't ",
        "could not",
        "no player",
        "no item",
        "no block",
        "not found",
        "outside the world",
        "too many blocks",
        "failed",
        "error",
        "permission",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
}

#[cfg(test)]
mod security_command_tests {
    use super::command_allowed_for_player;
    #[test]
    fn clients_can_run_social_and_read_only_commands() {
        for cmd in ["help", "list"] {
            assert!(command_allowed_for_player(cmd, &[]), "{cmd}");
        }
        for cmd in ["me", "msg", "tell", "w"] {
            assert!(command_allowed_for_player(cmd, &[]), "{cmd}");
        }
        assert!(command_allowed_for_player("time", &["query", "daytime"]));
        for (cmd, args) in [
            ("stop", vec![]),
            ("give", vec!["Steve", "diamond"]),
            ("gamemode", vec!["creative"]),
            ("setblock", vec!["0", "64", "0", "stone"]),
            ("execute", vec!["run", "stop"]),
            ("function", vec!["spine:hello"]),
            ("time", vec!["set", "night"]),
            ("scoreboard", vec!["players", "set"]),
            ("seed", vec![]),
            ("", vec![]),
        ] {
            assert!(!command_allowed_for_player(cmd, &args), "{cmd}");
        }
    }

    #[test]
    fn privileged_commands_are_advertised_only_to_operators() {
        let regular = super::command_tree(false);
        let operator = super::command_tree(true);
        for name in ["me", "msg", "tell", "w"] {
            assert!(regular.child(name).is_some(), "{name}");
        }
        for name in ["kick", "setidletimeout"] {
            assert!(regular.child(name).is_none(), "{name}");
            assert!(operator.child(name).is_some(), "{name}");
        }
    }
}

#[cfg(test)]
mod p5f_parity_tests {
    // Lane P5f — issues #96, #97, #98, #165, #167, #173, #175, #176, plus the
    // /forceload query result fix for `execute store result`.
    use super::*;
    use std::net::TcpStream;

    fn p5f_ctx(me: Option<Arc<Client>>) -> CmdCtx {
        CmdCtx {
            me,
            x: 0.5,
            y: 16.0,
            z: 0.5,
        }
    }

    fn p5f_run(ctx: &CmdCtx, msg: &str) -> String {
        run_command_ctx(ctx, msg, &mut None)
    }

    fn p5f_client(eid: i32, name: &str, x: f64, y: f64, z: f64) -> (Arc<Client>, TcpStream) {
        let (mut client, receiver) = crate::effect_tests::client_pair();
        client.eid = eid;
        client.name = name.into();
        *client.pos.lock().unwrap_or_else(|e| e.into_inner()) = (x, y, z, 0.0, 0.0, true);
        (Arc::new(client), receiver)
    }

    fn p5f_loot_table(item: i64) -> Vec<LootPool> {
        vec![LootPool {
            rolls: 1,
            chance: 1.0,
            funcs: Vec::new(),
            entries: vec![LootEntry {
                item,
                min: 1,
                max: 1,
                funcs: Vec::new(),
            }],
        }]
    }

    // #96/#173 — vanilla `/weather` reads a TimeArgument whose bare number is
    // SECONDS (*20 ticks), and an omitted duration is a seeded roll in the
    // per-mode vanilla range.
    #[test]
    fn p5f_parity_weather_time_argument_and_default_ranges() {
        assert_eq!(weather_time_argument("60"), Some(1200));
        assert_eq!(weather_time_argument("20t"), Some(20));
        assert_eq!(weather_time_argument("60s"), Some(1200));
        assert_eq!(weather_time_argument("2d"), Some(48_000));
        assert_eq!(weather_time_argument("0.5s"), Some(10));
        assert_eq!(weather_time_argument("bogus"), None);
        assert_eq!(weather_time_argument("-1"), None);
        assert_eq!(weather_time_argument("NaN"), None);
        assert_eq!(weather_time_argument(""), None);
        for (mode, lo, hi) in [
            ("clear", 12_000i64, 180_000i64),
            ("rain", 12_000, 24_000),
            ("thunder", 3_600, 15_600),
        ] {
            let rolled = weather_default_ticks(mode);
            assert!(
                (lo..=hi).contains(&rolled),
                "{mode} default {rolled} outside {lo}..={hi}"
            );
        }
    }

    #[test]
    fn p5f_parity_weather_command_durations_state_and_fail_closed() {
        let _world = world_guard();
        let ctx = p5f_ctx(None);
        assert_eq!(p5f_run(&ctx, "/weather rain 30"), "Weather: rain");
        assert_eq!(RAINING.load(Ordering::Relaxed), 600);
        assert_eq!(THUNDERING.load(Ordering::Relaxed), 0);
        assert_eq!(p5f_run(&ctx, "/weather thunder 10s"), "Weather: thunder");
        assert_eq!(THUNDERING.load(Ordering::Relaxed), 200);
        assert_eq!(RAINING.load(Ordering::Relaxed), 200);
        assert_eq!(p5f_run(&ctx, "/weather rain 20t"), "Weather: rain");
        assert_eq!(RAINING.load(Ordering::Relaxed), 20);
        // An omitted duration is a seeded roll inside the vanilla range.
        assert_eq!(p5f_run(&ctx, "/weather rain"), "Weather: rain");
        let rolled = RAINING.load(Ordering::Relaxed);
        assert!((12_000..=24_000).contains(&rolled), "rain default {rolled}");
        assert_eq!(p5f_run(&ctx, "/weather clear"), "Weather: clear");
        assert_eq!(RAINING.load(Ordering::Relaxed), 0);
        assert_eq!(THUNDERING.load(Ordering::Relaxed), 0);
        // Bad mode, bad duration, and extra args all fail closed.
        assert!(p5f_run(&ctx, "/weather bogus").starts_with("usage:"));
        assert!(p5f_run(&ctx, "/weather rain -5").starts_with("usage:"));
        assert!(p5f_run(&ctx, "/weather rain bogus").starts_with("usage:"));
        assert!(p5f_run(&ctx, "/weather rain 5 extra").starts_with("usage:"));
        assert!(p5f_run(&ctx, "/weather").starts_with("usage:"));
    }

    // #97/#167 — unknown blocks fail closed; keep/destroy/replace and the
    // {snbt} block-entity payload all behave like vanilla.
    #[test]
    fn p5f_parity_setblock_fails_closed_on_unknown_and_bad_args() {
        let _world = world_guard();
        let ctx = p5f_ctx(None);
        assert_eq!(
            p5f_run(&ctx, "/setblock 0 60 0 minecraft:not_a_real_block"),
            "Unknown block 'minecraft:not_a_real_block'"
        );
        assert!(p5f_run(&ctx, "/setblock 0 60 0").starts_with("usage:"));
        assert!(p5f_run(&ctx, "/setblock 0 60 0 stone sideways").starts_with("usage:"));
        // Unbalanced glue never reaches the parser: the tokenizer rejects it.
        assert!(p5f_run(&ctx, "/setblock 0 60 0 stone{oops").starts_with("Invalid command:"));
        // Balanced but malformed SNBT reaches the parser and fails closed.
        assert!(p5f_run(&ctx, "/setblock 0 60 0 stone{oops}").starts_with("Invalid block entity data"));
        assert_eq!(
            p5f_run(&ctx, "/setblock 0 400 0 stone"),
            "Block position is outside the world"
        );
    }

    #[test]
    fn p5f_parity_setblock_keep_destroy_replace_and_snbt() {
        if crate::test_support::isolated_world_test() {
            return;
        }
        let ctx = p5f_ctx(None);
        let stone = name_to_state("minecraft:stone").unwrap();
        let gold = name_to_state("minecraft:gold_block").unwrap();
        let pos = (30, 80, 30);
        assert!(p5f_run(&ctx, "/setblock 30 80 30 minecraft:stone").starts_with("Set block"));
        // keep refuses to overwrite a non-air block, works on air.
        assert_eq!(
            p5f_run(&ctx, "/setblock 30 80 30 minecraft:gold_block keep"),
            "Could not set the block"
        );
        assert_eq!(block_at_dim(DimId::Overworld, pos.0, pos.1, pos.2), stone);
        assert!(p5f_run(&ctx, "/setblock 31 80 30 minecraft:gold_block keep").starts_with("Set block"));
        assert_eq!(block_at_dim(DimId::Overworld, 31, 80, 30), gold);
        // destroy drops the old block then replaces it.
        let drops_before = drops().lock().unwrap_or_else(|e| e.into_inner()).len();
        assert!(p5f_run(&ctx, "/setblock 30 80 30 minecraft:gold_block destroy").starts_with("Set block"));
        assert_eq!(block_at_dim(DimId::Overworld, pos.0, pos.1, pos.2), gold);
        let gained = drops().lock().unwrap_or_else(|e| e.into_inner()).len() - drops_before;
        assert!(gained >= 1, "destroy should have dropped the old block");
        // replace is the default.
        assert!(p5f_run(&ctx, "/setblock 31 80 30 minecraft:stone").starts_with("Set block"));
        assert_eq!(block_at_dim(DimId::Overworld, 31, 80, 30), stone);
        // A glued {snbt} payload files Items into the container store.
        let reply = p5f_run(
            &ctx,
            r#"/setblock 32 80 30 minecraft:chest{Items:[{id:"minecraft:stone",Count:3b,Slot:2b}]}"#,
        );
        assert!(reply.starts_with("Set block"), "{reply}");
        let slots = chests()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&ContainerKey::Block((DimId::Overworld, 32, 80, 30)))
            .cloned()
            .unwrap_or_default();
        let stone_item = name_to_item("stone").unwrap();
        assert_eq!(slots.get(&2).map(|s| (s.0, s.1)), Some((stone_item, 3)));
        // A bare {snbt} token works the same way.
        let reply = p5f_run(
            &ctx,
            r#"/setblock 33 80 30 minecraft:chest {Items:[{id:"minecraft:diamond",Count:1b,Slot:0b}]} keep"#,
        );
        assert!(reply.starts_with("Set block"), "{reply}");
        let slots = chests()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&ContainerKey::Block((DimId::Overworld, 33, 80, 30)))
            .cloned()
            .unwrap_or_default();
        assert_eq!(
            slots.get(&0).map(|s| (s.0, s.1)),
            Some((name_to_item("diamond").unwrap(), 1))
        );
    }

    // #98 — one quote/bracket/brace-aware tokenizer for every command path.
    #[test]
    fn p5f_parity_split_command_args_quotes_brackets_and_braces() {
        let args = split_command_args(
            r#"setblock 0 64 0 chest{Items:[{id:"minecraft:stone"}]} keep"#,
        )
        .unwrap();
        assert_eq!(
            args,
            [
                "setblock",
                "0",
                "64",
                "0",
                r#"chest{Items:[{id:"minecraft:stone"}]}"#,
                "keep"
            ]
        );
        let args = split_command_args(r#"tp @e[name="My Mob",limit=1] 0 64 0"#).unwrap();
        assert_eq!(args[1], r#"@e[name="My Mob",limit=1]"#);
        let args = split_command_args("give @s \"quoted item\"").unwrap();
        assert_eq!(args[2], "\"quoted item\"");
        let args = split_command_args("give @s \"say \\\"hi\\\"\"").unwrap();
        assert_eq!(args[2], "\"say \\\"hi\\\"\"");
        assert!(split_command_args("setblock 0 0 0 stone{").is_err());
        assert!(split_command_args("tp @e[name=a 0 0 0").is_err());
        // The dispatcher uses it too: an unbalanced quote errors instead of
        // silently splitting mid-token.
        let _world = world_guard();
        let ctx = p5f_ctx(None);
        assert!(p5f_run(&ctx, "/setblock 0 60 0 \"stone").starts_with("Invalid command:"));
        // ...and execute tails resolve the same way: the SNBT token survives.
        let err = p5f_run(&ctx, "/execute run setblock 0 60 0 chest{Items:[");
        assert!(err.starts_with("Invalid command:"), "{err}");
    }

    // #165 — `/datapack enable` files the pack at the requested position and
    // `/datapack list enabled` reports the explicit order.
    #[test]
    fn p5f_parity_datapack_enable_ordering() {
        if crate::test_support::isolated_world_test() {
            return;
        }
        let work = std::env::temp_dir().join(format!("spine-p5f-dp-{}", std::process::id()));
        let root = work.join("datapacks");
        for pack in ["alpha", "beta", "gamma"] {
            std::fs::create_dir_all(root.join(format!("{pack}/data/p5f/function"))).unwrap();
        }
        let previous = std::env::current_dir().unwrap();
        std::env::set_current_dir(&work).unwrap();
        let ctx = p5f_ctx(None);
        assert_eq!(p5f_run(&ctx, "/reload"), "Reloading! (3 pack(s))");
        assert_eq!(p5f_run(&ctx, "/datapack list enabled"), "3 packs: alpha, beta, gamma");

        // first|last file at the ends.
        assert_eq!(
            p5f_run(&ctx, "/datapack disable beta"),
            "Disabled datapack 'beta'. Reloading! (2 pack(s))"
        );
        assert!(p5f_run(&ctx, "/datapack enable beta first").starts_with("Enabled datapack 'beta'"));
        assert_eq!(p5f_run(&ctx, "/datapack list enabled"), "3 packs: beta, alpha, gamma");
        assert!(p5f_run(&ctx, "/datapack disable beta").starts_with("Disabled datapack"));
        assert!(p5f_run(&ctx, "/datapack enable beta last").starts_with("Enabled datapack 'beta'"));
        assert_eq!(p5f_run(&ctx, "/datapack list enabled"), "3 packs: alpha, gamma, beta");

        // before|after anchor inside the enabled set.
        assert!(p5f_run(&ctx, "/datapack disable beta").starts_with("Disabled datapack"));
        assert!(p5f_run(&ctx, "/datapack enable beta before gamma").starts_with("Enabled datapack 'beta'"));
        assert_eq!(p5f_run(&ctx, "/datapack list enabled"), "3 packs: alpha, beta, gamma");
        assert!(p5f_run(&ctx, "/datapack disable gamma").starts_with("Disabled datapack"));
        assert!(p5f_run(&ctx, "/datapack enable gamma after beta").starts_with("Enabled datapack 'gamma'"));
        assert_eq!(p5f_run(&ctx, "/datapack list enabled"), "3 packs: alpha, beta, gamma");

        // A non-enabled anchor fails closed — nothing is enabled or reordered.
        assert!(p5f_run(&ctx, "/datapack disable beta").starts_with("Disabled datapack"));
        assert_eq!(
            p5f_run(&ctx, "/datapack enable beta before delta"),
            "Pack 'delta' is not enabled"
        );
        assert_eq!(p5f_run(&ctx, "/datapack list enabled"), "2 packs: alpha, gamma");
        assert_eq!(p5f_run(&ctx, "/datapack list available"), "1 packs: beta");
        assert_eq!(
            p5f_run(&ctx, "/datapack enable beta before beta"),
            "Pack 'beta' is not enabled"
        );
        assert_eq!(
            p5f_run(&ctx, "/datapack enable beta sideways"),
            "usage: /datapack enable <name> [first|last|before <existing>|after <existing>]"
        );
        assert!(p5f_run(&ctx, "/datapack enable beta").starts_with("Enabled datapack 'beta'"));
        assert_eq!(p5f_run(&ctx, "/datapack list enabled"), "3 packs: alpha, gamma, beta");

        std::env::set_current_dir(&previous).unwrap();
        std::fs::remove_dir_all(&work).unwrap();
    }

    // #175 — entity destination, explicit rotation, and `facing` all compute
    // the exact vanilla angles and move every supported subject kind.
    #[test]
    fn p5f_parity_tp_look_angles_match_vanilla() {
        // yaw = atan2(-dx, dz); pitch = atan2(-dy, hypot(dx, dz)) in degrees.
        let (yaw, pitch) = tp_look_angles([0.0, 64.0, 0.0], [0.0, 64.0, 10.0]);
        assert_eq!((yaw, pitch), (0.0, 0.0));
        let (yaw, pitch) = tp_look_angles([0.0, 64.0, 0.0], [10.0, 64.0, 0.0]);
        assert_eq!((yaw, pitch), (-90.0, 0.0));
        let (yaw, pitch) = tp_look_angles([0.0, 64.0, 0.0], [0.0, 60.0, 0.0]);
        assert_eq!((yaw, pitch), (0.0, 90.0));
        let (yaw, pitch) = tp_look_angles([0.0, 64.0, 0.0], [0.0, 68.0, 0.0]);
        assert_eq!((yaw, pitch), (0.0, -90.0));
        let (yaw, _) = tp_look_angles([0.0, 0.0, 0.0], [-1.0, 0.0, -1.0]);
        assert_eq!(yaw, 135.0);
        assert_eq!(TpAngle::parse("~").map(|a| a.resolve(30.0)), Some(30.0));
        assert_eq!(TpAngle::parse("~15").map(|a| a.resolve(30.0)), Some(45.0));
        assert_eq!(TpAngle::parse("45").map(|a| a.resolve(30.0)), Some(45.0));
        assert!(TpAngle::parse("bogus").is_none());
        assert!(TpAngle::parse("NaN").is_none());
    }

    #[test]
    fn p5f_parity_tp_coordinates_rotation_facing_and_entity_dest() {
        let _world = world_guard();
        let (a, _ra) = p5f_client(9101, "p5f-a", 0.5, 16.0, 0.5);
        let (b, _rb) = p5f_client(9102, "p5f-b", 100.5, 70.0, -50.5);
        clients().lock().unwrap_or_else(|e| e.into_inner()).extend([a.clone(), b.clone()]);
        let ctx = p5f_ctx(None);

        // coords + explicit rotation
        assert_eq!(p5f_run(&ctx, "/tp p5f-a 10 64 -20 90 -45"), "Teleported 1 target(s)");
        let p = *a.pos.lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!((p.0, p.1, p.2), (10.0, 64.0, -20.0));
        assert_eq!((p.3, p.4), (90.0, -45.0));
        // no rotation keeps the current one
        assert_eq!(p5f_run(&ctx, "/tp p5f-a 1 65 2"), "Teleported 1 target(s)");
        let p = *a.pos.lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!((p.0, p.1, p.2), (1.0, 65.0, 2.0));
        assert_eq!((p.3, p.4), (90.0, -45.0));
        // relative rotation ~ wraps like vanilla
        assert_eq!(p5f_run(&ctx, "/tp p5f-a 1 65 2 ~90 ~"), "Teleported 1 target(s)");
        let p = *a.pos.lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!((p.3, p.4), (-180.0, -45.0));

        // facing a point: aimed from the subject's feet at the new position
        assert_eq!(p5f_run(&ctx, "/tp p5f-a 0 64 0 facing 0 64 10"), "Teleported 1 target(s)");
        let p = *a.pos.lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!((p.3, p.4), (0.0, 0.0));
        assert_eq!(p5f_run(&ctx, "/tp p5f-a 0 64 0 facing 10 64 0"), "Teleported 1 target(s)");
        let p = *a.pos.lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!((p.3, p.4), (-90.0, 0.0));
        assert_eq!(p5f_run(&ctx, "/tp p5f-a 0 64 0 facing 0 60 0"), "Teleported 1 target(s)");
        let p = *a.pos.lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!((p.3, p.4), (0.0, 90.0));

        // facing entity feet vs eyes: the eyes anchor adds the eye height.
        let (bx, by, bz) = (100.5, 70.0, -50.5);
        let (expect_yaw, expect_pitch) =
            tp_look_angles([0.0, 64.0, 0.0], [bx, by, bz]);
        assert_eq!(p5f_run(&ctx, "/tp p5f-a 0 64 0 facing entity p5f-b feet"), "Teleported 1 target(s)");
        let p = *a.pos.lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!((p.3, p.4), (expect_yaw, expect_pitch));
        let (expect_yaw, expect_pitch) =
            tp_look_angles([0.0, 64.0, 0.0], [bx, by + 1.62, bz]);
        assert_eq!(p5f_run(&ctx, "/tp p5f-a 0 64 0 facing entity p5f-b eyes"), "Teleported 1 target(s)");
        let p = *a.pos.lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!((p.3, p.4), (expect_yaw, expect_pitch));

        // entity destination adopts the destination's position AND rotation
        *b.pos.lock().unwrap_or_else(|e| e.into_inner()) = (5.0, 65.0, 7.0, 45.0, -30.0, true);
        assert_eq!(p5f_run(&ctx, "/tp p5f-a p5f-b"), "Teleported 1 target(s)");
        let p = *a.pos.lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!((p.0, p.1, p.2, p.3, p.4), (5.0, 65.0, 7.0, 45.0, -30.0));
        // selector subjects and selector destination
        assert_eq!(
            p5f_run(&ctx, "/tp @a[name=p5f-a] @a[name=p5f-b]"),
            "Teleported 1 target(s)"
        );
        let p = *a.pos.lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!((p.0, p.1, p.2), (5.0, 65.0, 7.0));

        // mobs move and broadcast the entity-teleport packet path
        assert!(insert_live_mob(
            name_to_etype("zombie").unwrap(),
            DimId::Overworld,
            (1.0, 16.0, 1.0, 0.0, 0.0),
            20.0,
            None,
        ));
        let zeid = mobs().lock().unwrap_or_else(|e| e.into_inner()).last().unwrap().eid;
        assert_eq!(p5f_run(&ctx, "/tp @e[type=zombie] p5f-b"), "Teleported 1 target(s)");
        let zombie = mobs()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .find(|m| m.eid == zeid)
            .cloned()
            .unwrap();
        let zp = *zombie.pos.lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!((zp.0, zp.1, zp.2), (5.0, 65.0, 7.0));
        assert_eq!((zp.3, zp.4), (45.0, -30.0));

        // fake actors keep working as subjects
        {
            let mut actors = actors().lock().unwrap_or_else(|e| e.into_inner());
            actors.insert(
                "p5f-actor".to_owned(),
                Arc::new(Actor {
                    name: "p5f-actor".into(),
                    eid: 9901,
                    pos: Mutex::new((0.0, 0.0, 0.0)),
                    look: Mutex::new(None),
                    action: Mutex::new(""),
                    held: Mutex::new((0, 0, Vec::new())),
                    inv: Mutex::new(HashMap::new()),
                }),
            );
        }
        assert_eq!(p5f_run(&ctx, "/tp p5f-actor 8 70 9"), "Teleported 1 target(s)");
        assert_eq!(
            *actors().lock().unwrap_or_else(|e| e.into_inner()).get("p5f-actor").unwrap().pos.lock().unwrap_or_else(|e| e.into_inner()),
            (8.0, 70.0, 9.0)
        );
        actors().lock().unwrap_or_else(|e| e.into_inner()).remove("p5f-actor");

        // failure modes
        assert!(insert_live_mob(
            name_to_etype("zombie").unwrap(),
            DimId::Overworld,
            (2.0, 16.0, 2.0, 0.0, 0.0),
            20.0,
            None,
        ));
        let zeid2 = mobs().lock().unwrap_or_else(|e| e.into_inner()).last().unwrap().eid;
        assert_eq!(
            p5f_run(&ctx, "/tp @e[type=zombie] @e[type=zombie]"),
            "Only one entity is allowed"
        );
        assert_eq!(p5f_run(&ctx, "/tp p5f-missing 0 0 0"), "No entity was found");
        assert!(p5f_run(&ctx, "/tp").starts_with("usage:"));
        assert!(p5f_run(&ctx, "/tp p5f-a 0 64 0 facing entity p5f-b eyes junk").starts_with("usage:"));
        assert!(p5f_run(&ctx, "/tp p5f-a 0 64 0 facing 0 0").starts_with("usage:"));

        mobs().lock().unwrap_or_else(|e| e.into_inner()).retain(|m| m.eid != zeid && m.eid != zeid2);
        clients()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|c| c.eid != a.eid && c.eid != b.eid);
    }

    // #176 — every target mode x every source mode parses and dispatches.
    #[test]
    fn p5f_parity_loot_spawn_give_replace_entity_sources() {
        let _world = world_guard();
        let ctx = p5f_ctx(None);
        let diamond = name_to_item("diamond").unwrap();
        let emerald = name_to_item("emerald").unwrap();
        loot_tables()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert("p5f:fixed".to_owned(), p5f_loot_table(diamond));

        // spawn + loot table source
        let before = drops().lock().unwrap_or_else(|e| e.into_inner()).len();
        assert_eq!(
            p5f_run(&ctx, "/loot spawn 5 64 5 loot p5f:fixed"),
            "Dropped 1 item stack(s)"
        );
        let drop = drops().lock().unwrap_or_else(|e| e.into_inner()).pop().unwrap();
        assert_eq!((drop.item, drop.cnt), (diamond, 1));
        assert_eq!(drops().lock().unwrap_or_else(|e| e.into_inner()).len(), before);

        // spawn + fish source (table + position + tool)
        assert_eq!(
            p5f_run(&ctx, "/loot spawn 5 64 5 fish p5f:fixed 0 64 0 minecraft:fishing_rod"),
            "Dropped 1 item stack(s)"
        );
        let drop = drops().lock().unwrap_or_else(|e| e.into_inner()).pop().unwrap();
        assert_eq!((drop.item, drop.cnt), (diamond, 1));
        assert_eq!(drops().lock().unwrap_or_else(|e| e.into_inner()).len(), before);

        // spawn + kill by entity type name: the cow fallback drops its pair.
        let before = drops().lock().unwrap_or_else(|e| e.into_inner()).len();
        let reply = p5f_run(&ctx, "/loot spawn 5 64 5 kill minecraft:cow");
        assert!(reply.starts_with("Dropped"), "{reply}");
        let gained = drops().lock().unwrap_or_else(|e| e.into_inner()).len() - before;
        assert!(gained >= 1, "cow kill source should drop something");
        drops().lock().unwrap_or_else(|e| e.into_inner()).truncate(before);

        // kill via a live entity selector
        assert!(insert_live_mob(
            name_to_etype("pig").unwrap(),
            DimId::Overworld,
            (3.0, 16.0, 3.0, 0.0, 0.0),
            10.0,
            None,
        ));
        let peid = mobs().lock().unwrap_or_else(|e| e.into_inner()).last().unwrap().eid;
        let before = drops().lock().unwrap_or_else(|e| e.into_inner()).len();
        let reply = p5f_run(&ctx, "/loot spawn 5 64 5 kill @e[type=pig]");
        assert!(reply.starts_with("Dropped"), "{reply}");
        assert!(drops().lock().unwrap_or_else(|e| e.into_inner()).len() > before);
        drops().lock().unwrap_or_else(|e| e.into_inner()).truncate(before);
        assert_eq!(
            p5f_run(&ctx, "/loot spawn 5 64 5 kill @e[type=zombie]"),
            "No entity was found"
        );

        // give source into a player inventory
        let (c, _rc) = p5f_client(9201, "p5f-loot", 0.5, 16.0, 0.5);
        clients().lock().unwrap_or_else(|e| e.into_inner()).push(c.clone());
        assert_eq!(
            p5f_run(&ctx, "/loot give p5f-loot loot p5f:fixed"),
            "Gave 1 item stack(s)"
        );
        assert!(c
            .inv
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .any(|(it, cnt, _)| *it == diamond && *cnt == 1));

        // replace entity <slot> writes the first rolled stack
        loot_tables()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert("p5f:emerald".to_owned(), p5f_loot_table(emerald));
        assert_eq!(
            p5f_run(&ctx, "/loot replace entity p5f-loot weapon.mainhand loot p5f:emerald"),
            "Replaced 1 slot(s)"
        );
        let held = 36 + *c.held.lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!(
            c.inv.lock().unwrap_or_else(|e| e.into_inner()).get(&held).map(|s| (s.0, s.1)),
            Some((emerald, 1))
        );
        clients().lock().unwrap_or_else(|e| e.into_inner()).retain(|cl| cl.eid != c.eid);
        mobs().lock().unwrap_or_else(|e| e.into_inner()).retain(|m| m.eid != peid);
        loot_tables().lock().unwrap_or_else(|e| e.into_inner()).remove("p5f:fixed");
        loot_tables().lock().unwrap_or_else(|e| e.into_inner()).remove("p5f:emerald");

        // unknown table / entity fail closed
        assert_eq!(
            p5f_run(&ctx, "/loot spawn 0 64 0 loot p5f:missing"),
            "Unknown loot table 'p5f:missing'"
        );
        assert_eq!(
            p5f_run(&ctx, "/loot spawn 0 64 0 kill minecraft:not_an_entity"),
            "Unknown entity 'minecraft:not_an_entity'"
        );
        assert!(p5f_run(&ctx, "/loot").starts_with("usage:"));
        assert!(p5f_run(&ctx, "/loot spawn 0 64 0").starts_with("usage:"));
    }

    #[test]
    fn p5f_parity_loot_insert_replace_block_and_mine() {
        if crate::test_support::isolated_world_test() {
            return;
        }
        let ctx = p5f_ctx(None);
        let diamond = name_to_item("diamond").unwrap();
        loot_tables()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert("p5f:chest".to_owned(), p5f_loot_table(diamond));

        // insert requires a real container block
        assert_eq!(
            p5f_run(&ctx, "/loot insert 40 80 40 loot p5f:chest"),
            "The target block is not a container"
        );
        assert!(p5f_run(&ctx, "/setblock 40 80 40 minecraft:chest").starts_with("Set block"));
        assert_eq!(
            p5f_run(&ctx, "/loot insert 40 80 40 loot p5f:chest"),
            "Inserted 1 item stack(s)"
        );
        let slots = chests()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&ContainerKey::Block((DimId::Overworld, 40, 80, 40)))
            .cloned()
            .unwrap_or_default();
        assert_eq!(slots.get(&0).map(|s| (s.0, s.1)), Some((diamond, 1)));

        // replace block <slot> writes exactly that slot
        assert_eq!(
            p5f_run(&ctx, "/loot replace block 40 80 40 container.5 loot p5f:chest"),
            "Replaced 1 slot(s)"
        );
        let slots = chests()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&ContainerKey::Block((DimId::Overworld, 40, 80, 40)))
            .cloned()
            .unwrap_or_default();
        assert_eq!(slots.get(&5).map(|s| (s.0, s.1)), Some((diamond, 1)));

        // mine rolls the block's drop table
        assert!(p5f_run(&ctx, "/setblock 41 80 40 minecraft:diamond_ore").starts_with("Set block"));
        let before = drops().lock().unwrap_or_else(|e| e.into_inner()).len();
        let reply = p5f_run(&ctx, "/loot spawn 42 80 40 mine 41 80 40");
        assert!(reply.starts_with("Dropped"), "{reply}");
        let gained: Vec<Drop> = drops().lock().unwrap_or_else(|e| e.into_inner()).split_off(before);
        assert!(!gained.is_empty(), "mining diamond_ore should drop");

        // spawn + mine with an explicit tool token parses too
        let reply = p5f_run(&ctx, "/loot spawn 42 80 40 mine 41 80 40 minecraft:diamond_pickaxe");
        assert!(reply.starts_with("Dropped"), "{reply}");
        drops().lock().unwrap_or_else(|e| e.into_inner()).truncate(before);
        assert_eq!(
            p5f_run(&ctx, "/loot spawn 42 80 40 mine 41 80 40 minecraft:not_a_tool"),
            "Unknown item 'minecraft:not_a_tool'"
        );

        loot_tables().lock().unwrap_or_else(|e| e.into_inner()).remove("p5f:chest");
    }

    // Bonus — `execute store result` now sees 1 vs 0 for /forceload query.
    #[test]
    fn p5f_parity_forceload_query_execute_store_result() {
        let _world = world_guard();
        let ctx = p5f_ctx(None);
        let previous = std::mem::replace(&mut *forceloaded().lock().unwrap_or_else(|e| e.into_inner()), HashSet::new());
        assert_eq!(
            p5f_run(&ctx, "/scoreboard objectives add p5f dummy"),
            "Added objective p5f"
        );
        // not force-loaded -> 0
        assert_eq!(
            p5f_run(&ctx, "/execute store result score holder p5f run forceload query 7 9"),
            "Stored 0 in p5f for holder"
        );
        assert_eq!(scoreboard().lock().unwrap_or_else(|e| e.into_inner()).get("holder", "p5f"), Some(0));
        // force-loaded -> 1
        assert!(p5f_run(&ctx, "/forceload add 7 9").contains("force"));
        assert_eq!(
            p5f_run(&ctx, "/execute store result score holder p5f run forceload query 7 9"),
            "Stored 1 in p5f for holder"
        );
        assert_eq!(scoreboard().lock().unwrap_or_else(|e| e.into_inner()).get("holder", "p5f"), Some(1));
        // direct mapping sanity for the bare-count query as well
        assert_eq!(
            execute_legacy_result("forceload", "Chunk 7, 9 is force loaded"),
            Some(1)
        );
        assert_eq!(
            execute_legacy_result("forceload", "Chunk 7, 9 is not force loaded"),
            Some(0)
        );
        assert_eq!(
            execute_legacy_result("forceload", "3 forceloaded chunk(s)"),
            Some(3)
        );
        p5f_run(&ctx, "/forceload remove all");
        scoreboard().lock().unwrap_or_else(|e| e.into_inner()).remove_objective("p5f");
        *forceloaded().lock().unwrap_or_else(|e| e.into_inner()) = previous;
    }
}
