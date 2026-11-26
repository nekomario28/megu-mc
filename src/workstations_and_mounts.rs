// Phase P2-B/P2-C core: the four missing workstation engines (stonecutter,
// grindstone, loom, cartography table) and the mob-equipment / vehicle-mount
// engine with protocol-767 codecs. Everything is include!d at crate root and
// reuses the menus_and_items stack helpers (`Stack`, `Comps`, `comp_*`,
// `enchantments_*`, `slot_wire`); wiring into menus_and_items.rs and
// play_protocol.rs is a later drop-in, so every item is test-scoped until then.

// ---------- menu type ids ----------
// 1.21.1 `MenuType` registry order: crafter_3x3 occupies index 7, so the tail of
// the registry is smoker 22 / cartography_table 23 / stonecutter 24.
const WM_MENU_GRINDSTONE: i64 = 15;
const WM_MENU_LOOM: i64 = 18;
const WM_MENU_CARTOGRAPHY: i64 = 23;
const WM_MENU_STONECUTTER: i64 = 24;
// `minecraft:repair_cost` on the 1.21.1 component wire (same numbering as
// COMP_PRIOR_WORK); the grindstone result drops it.
const WM_COMP_REPAIR_COST: i64 = 16;
// `minecraft:map_id` component id on the 1.21.1 component wire.
const WM_COMP_MAP_ID: i64 = 26;

// ---------- shared enchant helpers ----------
fn wm_is_curse(enchant: i64) -> bool {
    // Vanilla EnchantmentTags.CURSE: binding_curse + vanishing_curse.
    matches!(enchant, 2 | 40)
}

fn wm_stored_enchantments_get(c: &Comps) -> Vec<(i64, i64)> {
    let mut out = Vec::new();
    if let Some(d) = comp_get(c, COMP_STORED_ENCHANTMENTS) {
        let (n, mut p) = read_varint_at(d, 0);
        for _ in 0..n.min(64) {
            if p + 2 > d.len() {
                break;
            }
            let (id, p2) = read_varint_at(d, p);
            let (lv, p3) = read_varint_at(d, p2);
            p = p3;
            out.push((id, lv));
        }
    }
    out
}

// Vanilla `Enchantment.Cost` = base + perLevelAboveFirst * (level - 1), the
// min_cost half of each vanilla enchantment definition (1.21.1 Enchantments
// bootstrap). Datapack enchants fall back to the 1 + 10*(level-1) default shape.
fn wm_enchant_min_cost(enchant: i64, level: i64) -> i64 {
    let lv = level.max(1);
    let (base, per_level) = match enchant {
        0 => (1, 0),    // aqua_affinity
        1 => (5, 8),    // bane_of_arthropods
        2 => (25, 0),   // binding_curse
        3 => (5, 8),    // blast_protection
        4 => (15, 9),   // breach
        5 => (25, 0),   // channeling
        6 => (5, 8),    // density
        7 => (10, 10),  // depth_strider
        8 => (1, 10),   // efficiency
        9 => (5, 6),    // feather_falling
        10 => (10, 20), // fire_aspect
        11 => (10, 8),  // fire_protection
        12 => (20, 0),  // flame
        13 => (15, 9),  // fortune
        14 => (10, 10), // frost_walker
        15 => (1, 8),   // impaling
        16 => (20, 0),  // infinity
        17 => (5, 20),  // knockback
        18 => (15, 9),  // looting
        19 => (12, 7),  // loyalty
        20 => (15, 9),  // luck_of_the_sea
        21 => (15, 9),  // lure
        22 => (25, 0),  // mending
        23 => (20, 0),  // multishot
        24 => (1, 10),  // piercing
        25 => (1, 10),  // power
        26 => (3, 6),   // projectile_protection
        27 => (1, 11),  // protection
        28 => (12, 20), // punch
        29 => (12, 20), // quick_charge
        30 => (10, 10), // respiration
        31 => (17, 7),  // riptide
        32 => (1, 11),  // sharpness
        33 => (15, 0),  // silk_touch
        34 => (5, 8),   // smite
        35 => (10, 10), // soul_speed
        36 => (5, 9),   // sweeping_edge
        37 => (25, 25), // swift_sneak
        38 => (10, 20), // thorns
        39 => (5, 8),   // unbreaking
        40 => (25, 0),  // vanishing_curse
        41 => (15, 9),  // wind_burst
        _ => (1, 10),
    };
    base + per_level * (lv - 1)
}

fn wm_stack_max_damage(st: &Stack) -> Option<i64> {
    comp_varint(&st.2, COMP_MAX_DAMAGE)
        .or_else(|| item_to_name(st.0).and_then(item_max_durability).map(i64::from))
        .filter(|max| *max > 0)
}

fn wm_stack_enchanted(st: &Stack) -> bool {
    !enchantments_get(&st.2).is_empty() || !wm_stored_enchantments_get(&st.2).is_empty()
}

// Merge a curse list into `keep`, holding the higher level per id (vanilla
// ItemEnchantments.merge semantics on the grindstone's curse-preserving path).
fn wm_merge_curse_levels(keep: &mut Vec<(i64, i64)>, extra: &[(i64, i64)]) {
    for &(id, lv) in extra.iter().filter(|(id, _)| wm_is_curse(*id)) {
        if let Some(e) = keep.iter_mut().find(|(k, _)| *k == id) {
            e.1 = e.1.max(lv);
        } else {
            keep.push((id, lv));
        }
    }
}

// Rewrite a stack keeping only curse enchantments (regular + stored), and drop
// repair_cost. Shared by the grindstone's single-item and merge paths.
fn wm_keep_only_curses(st: &Stack) -> Stack {
    let mut out = st.clone();
    let curses: Vec<(i64, i64)> = enchantments_get(&out.2)
        .into_iter()
        .filter(|(id, _)| wm_is_curse(*id))
        .collect();
    if curses.is_empty() {
        out.2.retain(|(ty, _)| *ty != COMP_ENCHANTMENTS);
    } else {
        comp_set(&mut out.2, COMP_ENCHANTMENTS, enchantments_data(&curses, true));
    }
    let stored_curses: Vec<(i64, i64)> = wm_stored_enchantments_get(&out.2)
        .into_iter()
        .filter(|(id, _)| wm_is_curse(*id))
        .collect();
    if stored_curses.is_empty() {
        out.2.retain(|(ty, _)| *ty != COMP_STORED_ENCHANTMENTS);
    } else {
        comp_set(
            &mut out.2,
            COMP_STORED_ENCHANTMENTS,
            enchantments_data(&stored_curses, true),
        );
    }
    out.2.retain(|(ty, _)| *ty != WM_COMP_REPAIR_COST);
    out
}

// ---------- stonecutter ----------
// Built-in vanilla stonecutting table (1.21.1 recipe set; input -> outputs with
// counts). Slab outputs are 2 per block; copper derivatives are 4/8 per block,
// matching vanilla's `*_from_*_stonecutting` recipe counts.
const WM_STONECUT: &[(&str, &[(&str, u8)])] = &[
    (
        "stone",
        &[
            ("stone_slab", 2),
            ("stone_stairs", 1),
            ("stone_bricks", 1),
            ("stone_brick_slab", 2),
            ("stone_brick_stairs", 1),
            ("stone_brick_wall", 1),
            ("chiseled_stone_bricks", 1),
        ],
    ),
    (
        "cobblestone",
        &[("cobblestone_slab", 2), ("cobblestone_stairs", 1), ("cobblestone_wall", 1)],
    ),
    (
        "mossy_cobblestone",
        &[
            ("mossy_cobblestone_slab", 2),
            ("mossy_cobblestone_stairs", 1),
            ("mossy_cobblestone_wall", 1),
        ],
    ),
    (
        "stone_bricks",
        &[
            ("stone_brick_slab", 2),
            ("stone_brick_stairs", 1),
            ("stone_brick_wall", 1),
            ("chiseled_stone_bricks", 1),
        ],
    ),
    (
        "mossy_stone_bricks",
        &[
            ("mossy_stone_brick_slab", 2),
            ("mossy_stone_brick_stairs", 1),
            ("mossy_stone_brick_wall", 1),
        ],
    ),
    ("smooth_stone", &[("smooth_stone_slab", 2)]),
    (
        "granite",
        &[
            ("granite_slab", 2),
            ("granite_stairs", 1),
            ("granite_wall", 1),
            ("polished_granite", 1),
            ("polished_granite_slab", 2),
            ("polished_granite_stairs", 1),
        ],
    ),
    ("polished_granite", &[("polished_granite_slab", 2), ("polished_granite_stairs", 1)]),
    (
        "diorite",
        &[
            ("diorite_slab", 2),
            ("diorite_stairs", 1),
            ("diorite_wall", 1),
            ("polished_diorite", 1),
            ("polished_diorite_slab", 2),
            ("polished_diorite_stairs", 1),
        ],
    ),
    ("polished_diorite", &[("polished_diorite_slab", 2), ("polished_diorite_stairs", 1)]),
    (
        "andesite",
        &[
            ("andesite_slab", 2),
            ("andesite_stairs", 1),
            ("andesite_wall", 1),
            ("polished_andesite", 1),
            ("polished_andesite_slab", 2),
            ("polished_andesite_stairs", 1),
        ],
    ),
    ("polished_andesite", &[("polished_andesite_slab", 2), ("polished_andesite_stairs", 1)]),
    (
        "deepslate",
        &[
            ("cobbled_deepslate", 1),
            ("polished_deepslate", 1),
            ("deepslate_bricks", 1),
            ("deepslate_tiles", 1),
            ("chiseled_deepslate", 1),
        ],
    ),
    (
        "cobbled_deepslate",
        &[
            ("cobbled_deepslate_slab", 2),
            ("cobbled_deepslate_stairs", 1),
            ("cobbled_deepslate_wall", 1),
            ("polished_deepslate", 1),
            ("deepslate_bricks", 1),
            ("deepslate_tiles", 1),
            ("chiseled_deepslate", 1),
        ],
    ),
    (
        "polished_deepslate",
        &[
            ("polished_deepslate_slab", 2),
            ("polished_deepslate_stairs", 1),
            ("polished_deepslate_wall", 1),
            ("deepslate_bricks", 1),
            ("deepslate_tiles", 1),
            ("chiseled_deepslate", 1),
        ],
    ),
    (
        "deepslate_bricks",
        &[
            ("deepslate_brick_slab", 2),
            ("deepslate_brick_stairs", 1),
            ("deepslate_brick_wall", 1),
            ("deepslate_tiles", 1),
            ("chiseled_deepslate", 1),
        ],
    ),
    (
        "deepslate_tiles",
        &[
            ("deepslate_tile_slab", 2),
            ("deepslate_tile_stairs", 1),
            ("deepslate_tile_wall", 1),
            ("deepslate_bricks", 1),
            ("chiseled_deepslate", 1),
        ],
    ),
    (
        "tuff",
        &[
            ("tuff_slab", 2),
            ("tuff_stairs", 1),
            ("tuff_wall", 1),
            ("chiseled_tuff", 1),
            ("polished_tuff", 1),
            ("tuff_bricks", 1),
        ],
    ),
    (
        "polished_tuff",
        &[
            ("polished_tuff_slab", 2),
            ("polished_tuff_stairs", 1),
            ("polished_tuff_wall", 1),
            ("tuff_bricks", 1),
        ],
    ),
    (
        "tuff_bricks",
        &[
            ("tuff_brick_slab", 2),
            ("tuff_brick_stairs", 1),
            ("tuff_brick_wall", 1),
            ("chiseled_tuff_bricks", 1),
        ],
    ),
    (
        "sandstone",
        &[
            ("sandstone_slab", 2),
            ("sandstone_stairs", 1),
            ("sandstone_wall", 1),
            ("cut_sandstone", 1),
            ("cut_sandstone_slab", 2),
            ("chiseled_sandstone", 1),
        ],
    ),
    ("cut_sandstone", &[("cut_sandstone_slab", 2)]),
    (
        "red_sandstone",
        &[
            ("red_sandstone_slab", 2),
            ("red_sandstone_stairs", 1),
            ("red_sandstone_wall", 1),
            ("cut_red_sandstone", 1),
            ("cut_red_sandstone_slab", 2),
            ("chiseled_red_sandstone", 1),
        ],
    ),
    ("cut_red_sandstone", &[("cut_red_sandstone_slab", 2)]),
    (
        "quartz_block",
        &[
            ("quartz_slab", 2),
            ("quartz_stairs", 1),
            ("quartz_bricks", 1),
            ("chiseled_quartz_block", 1),
            ("quartz_pillar", 1),
        ],
    ),
    ("bricks", &[("brick_slab", 2), ("brick_stairs", 1), ("brick_wall", 1)]),
    ("mud_bricks", &[("mud_brick_slab", 2), ("mud_brick_stairs", 1), ("mud_brick_wall", 1)]),
    (
        "nether_bricks",
        &[
            ("nether_brick_slab", 2),
            ("nether_brick_stairs", 1),
            ("nether_brick_wall", 1),
            ("chiseled_nether_bricks", 1),
        ],
    ),
    (
        "red_nether_bricks",
        &[("red_nether_brick_slab", 2), ("red_nether_brick_stairs", 1), ("red_nether_brick_wall", 1)],
    ),
    (
        "blackstone",
        &[
            ("blackstone_slab", 2),
            ("blackstone_stairs", 1),
            ("blackstone_wall", 1),
            ("polished_blackstone", 1),
            ("polished_blackstone_bricks", 1),
            ("chiseled_polished_blackstone", 1),
        ],
    ),
    (
        "polished_blackstone",
        &[
            ("polished_blackstone_slab", 2),
            ("polished_blackstone_stairs", 1),
            ("polished_blackstone_wall", 1),
            ("polished_blackstone_bricks", 1),
            ("chiseled_polished_blackstone", 1),
        ],
    ),
    (
        "polished_blackstone_bricks",
        &[
            ("polished_blackstone_brick_slab", 2),
            ("polished_blackstone_brick_stairs", 1),
            ("polished_blackstone_brick_wall", 1),
        ],
    ),
    ("basalt", &[("polished_basalt", 1)]),
    (
        "end_stone_bricks",
        &[("end_stone_brick_slab", 2), ("end_stone_brick_stairs", 1), ("end_stone_brick_wall", 1)],
    ),
    ("end_stone", &[("end_stone_bricks", 1)]),
    (
        "purpur_block",
        &[("purpur_slab", 2), ("purpur_stairs", 1), ("purpur_pillar", 1)],
    ),
    (
        "prismarine",
        &[("prismarine_slab", 2), ("prismarine_stairs", 1), ("prismarine_wall", 1)],
    ),
    ("prismarine_bricks", &[("prismarine_brick_slab", 2), ("prismarine_brick_stairs", 1)]),
    ("dark_prismarine", &[("dark_prismarine_slab", 2), ("dark_prismarine_stairs", 1)]),
];

// Copper family: each {plain,exposed,weathered,oxidized} + waxed mirror variant
// stonecuts into its chiseled/cut derivatives at the vanilla 4x multiplier.
const WM_COPPER_PREFIXES: &[&str] = &[
    "",
    "exposed_",
    "weathered_",
    "oxidized_",
    "waxed_",
    "waxed_exposed_",
    "waxed_weathered_",
    "waxed_oxidized_",
];

// Resolved-once recipe map (input name -> sorted (item id, count) list). The
// copper family shares one shape per weather/wax prefix; everything else comes
// from WM_STONECUT.
fn wm_stonecut_recipes(input: &str) -> Vec<(i64, u8)> {
    static RECIPES: OnceLock<HashMap<String, Vec<(i64, u8)>>> = OnceLock::new();
    let map = RECIPES.get_or_init(|| {
        let mut m: HashMap<String, Vec<(i64, u8)>> = HashMap::new();
        let mut insert = |key: String, outs: Vec<(String, u8)>| {
            m.insert(
                key,
                outs.into_iter()
                    .filter_map(|(name, count)| name_to_item(&name).map(|item| (item, count)))
                    .collect(),
            );
        };
        for &(input_name, outs) in WM_STONECUT {
            insert(
                input_name.to_string(),
                outs.iter().map(|(n, c)| (n.to_string(), *c)).collect(),
            );
        }
        for prefix in WM_COPPER_PREFIXES {
            insert(
                format!("{prefix}copper_block"),
                vec![
                    (format!("{prefix}chiseled_copper"), 4),
                    (format!("{prefix}cut_copper"), 4),
                    (format!("{prefix}cut_copper_slab"), 8),
                    (format!("{prefix}cut_copper_stairs"), 4),
                ],
            );
            insert(
                format!("{prefix}cut_copper"),
                vec![
                    (format!("{prefix}cut_copper_slab"), 2),
                    (format!("{prefix}cut_copper_stairs"), 1),
                ],
            );
        }
        m
    });
    let mut recipes = map.get(input).cloned().unwrap_or_default();
    recipes.sort_unstable_by_key(|&(item, _)| item);
    recipes
}

#[derive(Default)]
struct StonecutterMenu {
    input: Stack,
    selected_recipe: Option<usize>,
}

impl StonecutterMenu {
    fn screen() -> (i64, &'static str) {
        (WM_MENU_STONECUTTER, "Stonecutter")
    }

    #[cfg(test)]
    fn set_input(&mut self, st: Stack) {
        self.input = st;
        self.selected_recipe = None;
    }

    // Deterministic order: vanilla sends the client a sorted recipe list;
    // output item id is a stable order across recipe-table edits.
    #[cfg(test)]
    fn available_recipes(&self) -> Vec<(i64, u8)> {
        if self.input.1 == 0 {
            return Vec::new();
        }
        item_to_name(self.input.0)
            .map(wm_stonecut_recipes)
            .unwrap_or_default()
    }

    // Serverbound `ContainerButtonClick` (0x09) with the recipe index.
    #[cfg(test)]
    fn select_button(&mut self, index: usize) -> bool {
        if index < self.available_recipes().len() {
            self.selected_recipe = Some(index);
            true
        } else {
            self.selected_recipe = None;
            false
        }
    }

    #[cfg(test)]
    fn result(&self) -> Stack {
        self.selected_recipe
            .and_then(|i| self.available_recipes().get(i).copied())
            .map(|(item, count)| (item, count, Vec::new()))
            .unwrap_or_default()
    }

    #[cfg(test)]
    fn take_result(&mut self) -> Option<Stack> {
        let out = self.result();
        if out.1 == 0 || self.input.1 == 0 {
            return None;
        }
        self.input.1 -= 1;
        if self.input.1 == 0 {
            self.input = Stack::default();
            self.selected_recipe = None;
        }
        Some(out)
    }
}

// ---------- grindstone ----------
#[derive(Default)]
struct GrindstoneMenu {
    top: Stack,
    bottom: Stack,
}

// Vanilla `GrindstoneMenu.getExperienceFromItem`: the sum of `getMinCost` over
// every non-curse enchantment (stored enchantments count for enchanted books).
fn wm_grind_xp_base(st: &Stack) -> i64 {
    if st.1 == 0 {
        return 0;
    }
    let mut total = 0i64;
    for (id, lv) in enchantments_get(&st.2) {
        if !wm_is_curse(id) {
            total += wm_enchant_min_cost(id, lv);
        }
    }
    for (id, lv) in wm_stored_enchantments_get(&st.2) {
        if !wm_is_curse(id) {
            total += wm_enchant_min_cost(id, lv);
        }
    }
    total
}

impl GrindstoneMenu {
    fn screen() -> (i64, &'static str) {
        (WM_MENU_GRINDSTONE, "Repair & Disenchant")
    }

    #[cfg(test)]
    fn set_slots(&mut self, top: Stack, bottom: Stack) {
        self.top = top;
        self.bottom = bottom;
    }

    // `GrindstoneMenu.computeResult` (1.21.1): a lone item must carry
    // enchantments; a pair must be the same item with count 1 each. Pair merges
    // durability with the vanilla +5% of max bonus capped at max_damage, keeps
    // curses from both inputs, strips the rest, and clears repair_cost.
    fn compute(&self) -> Option<(Stack, i64)> {
        if self.top.1 == 0 && self.bottom.1 == 0 {
            return None;
        }
        let xp_base = wm_grind_xp_base(&self.top) + wm_grind_xp_base(&self.bottom);
        let book = name_to_item("enchanted_book").unwrap_or(-1);
        let plain_book = name_to_item("book").unwrap_or(-2);
        if self.top.1 != 0 && self.bottom.1 != 0 {
            if self.top.0 != self.bottom.0 || self.top.1 > 1 || self.bottom.1 > 1 {
                return None;
            }
            let mut out = wm_keep_only_curses(&self.top);
            if out.0 == book {
                out.0 = plain_book;
            }
            let mut keep = Vec::new();
            wm_merge_curse_levels(&mut keep, &enchantments_get(&self.top.2));
            wm_merge_curse_levels(&mut keep, &wm_stored_enchantments_get(&self.top.2));
            wm_merge_curse_levels(&mut keep, &enchantments_get(&self.bottom.2));
            wm_merge_curse_levels(&mut keep, &wm_stored_enchantments_get(&self.bottom.2));
            if out.0 == plain_book {
                if keep.is_empty() {
                    out.2.retain(|(ty, _)| *ty != COMP_STORED_ENCHANTMENTS);
                } else {
                    comp_set(&mut out.2, COMP_STORED_ENCHANTMENTS, enchantments_data(&keep, true));
                }
            } else if keep.is_empty() {
                out.2.retain(|(ty, _)| *ty != COMP_ENCHANTMENTS);
            } else {
                comp_set(&mut out.2, COMP_ENCHANTMENTS, enchantments_data(&keep, true));
            }
            if let Some(max) = wm_stack_max_damage(&self.top) {
                let left = max - comp_varint(&self.top.2, COMP_DAMAGE).unwrap_or(0);
                let right = max - comp_varint(&self.bottom.2, COMP_DAMAGE).unwrap_or(0);
                let combined = (left + right + max * 5 / 100).min(max);
                comp_set_varint(&mut out.2, COMP_DAMAGE, max - combined);
            }
            out.1 = 1;
            Some((out, xp_base))
        } else {
            let st = if self.top.1 != 0 { &self.top } else { &self.bottom };
            if !wm_stack_enchanted(st) {
                return None;
            }
            let mut out = wm_keep_only_curses(st);
            if out.0 == book {
                out.0 = plain_book;
            }
            Some((out, xp_base))
        }
    }

    // Deterministic roll of vanilla `getExperienceAmount`:
    // j = ceil(xp_base/2); xp = j + nextInt(j) -> [j, 2j-1]. `roll` in [0,1)
    // picks inside that range.
    fn take_result(&mut self, roll: f64) -> Option<(Stack, i64)> {
        if !(0.0..1.0).contains(&roll) {
            return None;
        }
        let (out, xp_base) = self.compute()?;
        let j = (xp_base + 1) / 2;
        let xp = if j > 0 { j + (roll * j as f64) as i64 } else { 0 };
        self.top = Stack::default();
        self.bottom = Stack::default();
        Some((out, xp))
    }
}

// ---------- loom ----------
const WM_DYE_COLORS: &[&str] = &[
    "white",
    "orange",
    "magenta",
    "light_blue",
    "yellow",
    "lime",
    "pink",
    "gray",
    "light_gray",
    "cyan",
    "purple",
    "blue",
    "brown",
    "green",
    "red",
    "black",
];

// Vanilla `#minecraft:no_item_required` banner patterns in registry order — the
// 33 shapes a loom offers without a banner-pattern item.
const WM_LOOM_PATTERNS: &[&str] = &[
    "square_bottom_left",
    "square_bottom_right",
    "square_top_left",
    "square_top_right",
    "stripe_bottom",
    "stripe_top",
    "stripe_left",
    "stripe_right",
    "stripe_center",
    "stripe_middle",
    "stripe_downright",
    "stripe_downleft",
    "small_stripes",
    "cross",
    "straight_cross",
    "triangle_bottom",
    "triangle_top",
    "triangles_bottom",
    "triangles_top",
    "diagonal_left",
    "diagonal_up_right",
    "diagonal_up_left",
    "diagonal_right",
    "circle",
    "rhombus",
    "half_vertical",
    "half_horizontal",
    "half_vertical_right",
    "half_horizontal_bottom",
    "border",
    "curly_border",
    "gradient",
    "gradient_up",
    "bricks",
];

const WM_MAX_BANNER_LAYERS: usize = 6;

#[derive(Clone, Debug, PartialEq, Eq)]
struct BannerLayer {
    pattern: String,
    color: u8, // DyeColor index 0..=15
}

#[derive(Default)]
struct LoomMenu {
    banner: Stack,
    dye: Stack,
    pattern_item: Stack,
    banner_layers: Vec<BannerLayer>,
    selected_pattern: Option<String>,
}

fn wm_banner_name(st: &Stack) -> Option<&'static str> {
    item_to_name(st.0).and_then(|n| {
        WM_DYE_COLORS
            .iter()
            .any(|c| n == format!("{c}_banner"))
            .then_some(n)
    })
}

fn wm_dye_color_index(st: &Stack) -> Option<u8> {
    item_to_name(st.0).and_then(|n| {
        WM_DYE_COLORS
            .iter()
            .position(|c| n == format!("{c}_dye"))
            .map(|i| i as u8)
    })
}

fn wm_pattern_item_name(st: &Stack) -> Option<&'static str> {
    item_to_name(st.0).and_then(|n| match n {
        "flower_banner_pattern" => Some("flower"),
        "creeper_banner_pattern" => Some("creeper"),
        "skull_banner_pattern" => Some("skull"),
        "mojang_banner_pattern" => Some("mojang"),
        "globe_banner_pattern" => Some("globe"),
        "piglin_banner_pattern" => Some("piglin"),
        "flow_banner_pattern" => Some("flow"),
        "guster_banner_pattern" => Some("guster"),
        _ => None,
    })
}

impl LoomMenu {
    fn screen() -> (i64, &'static str) {
        (WM_MENU_LOOM, "Loom")
    }

    #[cfg(test)]
    fn set_slots(&mut self, banner: Stack, dye: Stack, pattern_item: Stack) {
        self.set_banner(banner, Vec::new());
        self.dye = dye;
        self.pattern_item = pattern_item;
        self.selected_pattern = None;
    }

    // `layers` comes from the banner's banner_patterns component at wiring time.
    #[cfg(test)]
    fn set_banner(&mut self, banner: Stack, layers: Vec<BannerLayer>) {
        self.banner = banner;
        self.banner_layers = layers;
        self.selected_pattern = None;
    }

    // With a pattern item the loom offers exactly its pattern; without one it
    // offers the no-item-required list (vanilla LoomMenu.updateSelectablePatterns).
    fn selectable_patterns(&self) -> Vec<String> {
        if let Some(pattern) = wm_pattern_item_name(&self.pattern_item) {
            return vec![pattern.to_string()];
        }
        WM_LOOM_PATTERNS.iter().map(|p| p.to_string()).collect()
    }

    fn select_pattern(&mut self, pattern: &str) -> bool {
        if self.selectable_patterns().iter().any(|p| p == pattern) {
            self.selected_pattern = Some(pattern.to_string());
            true
        } else {
            self.selected_pattern = None;
            false
        }
    }

    // Vanilla layer cap: 6 via the loom; a banner-pattern item bypasses it.
    fn can_apply(&self) -> bool {
        if self.banner.1 != 1 || wm_banner_name(&self.banner).is_none() {
            return false;
        }
        if self.dye.1 == 0 || wm_dye_color_index(&self.dye).is_none() {
            return false;
        }
        if self.selected_pattern.is_none() {
            return false;
        }
        if wm_pattern_item_name(&self.pattern_item).is_none()
            && self.banner_layers.len() >= WM_MAX_BANNER_LAYERS
        {
            return false;
        }
        true
    }

    fn result(&self) -> Stack {
        if self.can_apply() {
            (self.banner.0, 1u8, self.banner.2.clone())
        } else {
            Default::default()
        }
    }

    // Takes the result: banner + dye are consumed (banner count must be 1), the
    // pattern item is reusable and stays, and the new layer is appended.
    fn take_result(&mut self) -> Option<(Stack, Vec<BannerLayer>)> {
        if !self.can_apply() {
            return None;
        }
        let mut layers = self.banner_layers.clone();
        layers.push(BannerLayer {
            pattern: self.selected_pattern.clone()?,
            color: wm_dye_color_index(&self.dye)?,
        });
        let out = (self.banner.0, 1u8, self.banner.2.clone());
        self.banner = Stack::default();
        self.dye.1 -= 1;
        if self.dye.1 == 0 {
            self.dye = Stack::default();
        }
        self.banner_layers = layers.clone();
        Some((out, layers))
    }
}

// ---------- cartography table ----------
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct MapState {
    map_id: i64,
    scale: u8, // vanilla 0..=4
    locked: bool,
}

#[derive(Default)]
struct CartographyMenu {
    map: Stack,
    addition: Stack,
    map_state: Option<MapState>,
}

impl CartographyMenu {
    fn screen() -> (i64, &'static str) {
        (WM_MENU_CARTOGRAPHY, "Cartography Table")
    }

    #[cfg(test)]
    fn set_map(&mut self, st: Stack, state: Option<MapState>) {
        self.map = st;
        self.map_state = state;
    }

    #[cfg(test)]
    fn set_addition(&mut self, st: Stack) {
        self.addition = st;
    }

    fn map_valid(&self) -> bool {
        self.map.1 == 1
            && item_to_name(self.map.0) == Some("filled_map")
            && self.map_state.is_some_and(|s| s.scale <= 4)
    }

    fn preview(&self) -> Option<(Stack, MapState)> {
        if !self.map_valid() || self.addition.1 == 0 {
            return None;
        }
        let state = self.map_state?;
        let filled = name_to_item("filled_map")?;
        match item_to_name(self.addition.0) {
            Some("paper") if !state.locked && state.scale < 4 => {
                // Zoom-out consumes the old map; the new id lands at take time.
                Some(((filled, 1, Vec::new()), MapState { scale: state.scale + 1, ..state }))
            }
            Some("map") => {
                // Clone: two filled copies, same id and scale.
                Some(((filled, 2, Vec::new()), state))
            }
            Some("glass_pane") if !state.locked => {
                Some(((filled, 1, Vec::new()), MapState { locked: true, ..state }))
            }
            _ => None,
        }
    }

    // `new_map_id` supplies the fresh map id a zoom needs; the caller owns the
    // allocator (vanilla `MapItemSavedData` counter).
    fn take_result(&mut self, new_map_id: i64) -> Option<(Stack, MapState)> {
        let (mut out, mut state) = self.preview()?;
        if item_to_name(self.addition.0) == Some("paper") {
            state.map_id = new_map_id;
        }
        let mut id_comp = Vec::new();
        comp_set_varint(&mut id_comp, WM_COMP_MAP_ID, state.map_id);
        out.2 = id_comp;
        self.map = Stack::default();
        self.addition.1 -= 1;
        if self.addition.1 == 0 {
            self.addition = Stack::default();
        }
        self.map_state = None;
        Some((out, state))
    }
}

// ---------- entity equipment (S2C 0x5b SetEquipment) ----------
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
enum EquipmentSlot {
    MainHand = 0,
    OffHand = 1,
    Feet = 2,
    Legs = 3,
    Chest = 4,
    Head = 5,
}

#[derive(Default)]
struct EntityEquipment {
    // Indexed by EquipmentSlot discriminant.
    slots: [Stack; 6],
}

impl EntityEquipment {
    fn set(&mut self, slot: EquipmentSlot, st: Stack) {
        self.slots[slot as usize] = st;
    }

    fn get(&self, slot: EquipmentSlot) -> &Stack {
        &self.slots[slot as usize]
    }
}

// Streaming form of `slot_read_wire` for stacks embedded mid-packet (SetEquipment
// entries are back-to-back, so the decoder must report how many bytes a stack
// consumed). The component-boundary table mirrors slot_read_wire exactly.
#[cfg(test)]
fn wm_stack_decode(b: &[u8]) -> Option<(Stack, usize)> {
    let mut p = 0usize;
    let cnt = slot_read_varint(b, &mut p)?;
    if cnt == 0 {
        return Some(((0, 0, Vec::new()), p));
    }
    if !(1..=127).contains(&cnt) {
        return None;
    }
    let item = slot_read_varint(b, &mut p)?;
    let nadd = slot_read_varint(b, &mut p)?;
    let nrem = slot_read_varint(b, &mut p)?;
    let mut comps: Comps = Vec::new();
    if !(0..=128).contains(&nadd) || !(0..=128).contains(&nrem) {
        return None;
    }
    if nrem != 0 {
        return None;
    }
    for _ in 0..nadd {
        let ty = slot_read_varint(b, &mut p)?;
        let start = p;
        match ty {
            1..=3 | 16 => {
                slot_read_varint(b, &mut p)?;
            }
            4 => {
                let value = *b.get(p)?;
                if value > 1 {
                    return None;
                }
                p += 1;
            }
            5 => {
                if p + 3 <= b.len() && b[p] == 8 {
                    let l = u16::from_be_bytes(b[p + 1..p + 3].try_into().unwrap_or_default()) as usize;
                    p += 3;
                    p = p.checked_add(l).filter(|end| *end <= b.len())?;
                } else {
                    return None;
                }
            }
            7 => {
                let n = slot_read_varint(b, &mut p)?;
                if !(0..=128).contains(&n) {
                    return None;
                }
                for _ in 0..n {
                    if p + 3 <= b.len() && b[p] == 8 {
                        let l = u16::from_be_bytes(b[p + 1..p + 3].try_into().unwrap_or_default()) as usize;
                        p += 3;
                        p = p.checked_add(l).filter(|end| *end <= b.len())?;
                    } else {
                        return None;
                    }
                }
            }
            9 | 23 => {
                let n = slot_read_varint(b, &mut p)?;
                if !(0..=256).contains(&n) {
                    return None;
                }
                for _ in 0..n {
                    slot_read_varint(b, &mut p)?;
                    slot_read_varint(b, &mut p)?;
                }
                let value = *b.get(p)?;
                if value > 1 {
                    return None;
                }
                p += 1;
            }
            COMP_POTION_CONTENTS => {
                let mut payload = b.get(p..)?;
                if read_merchant_bool(&mut payload)? {
                    read_protocol_i32(&mut payload)?;
                }
                if read_merchant_bool(&mut payload)? {
                    payload = payload.get(4..)?;
                }
                let effects = read_protocol_i32(&mut payload)?;
                if !(0..=256).contains(&effects) {
                    return None;
                }
                for _ in 0..effects {
                    read_protocol_i32(&mut payload)?; // effect registry id
                    read_live_potion_effect_details(&mut payload, 0)?;
                }
                p = b.len() - payload.len();
            }
            COMP_ARMOR_TRIM => {
                if slot_read_varint(b, &mut p)? == 0 || slot_read_varint(b, &mut p)? == 0 {
                    return None;
                }
                let value = *b.get(p)?;
                if value > 1 {
                    return None;
                }
                p += 1;
            }
            _ => return None,
        }
        if p <= b.len() && p > start {
            comps.push((ty, b[start..p].to_vec()));
        } else {
            return None;
        }
    }
    (0..=i32::MAX as i64)
        .contains(&item)
        .then_some(((item, cnt as u8, comps), p))
}

// `ClientboundSetEquipmentPacket` body: VarInt entity id, then one or more
// (slot byte, ItemStack) entries; bit 0x80 marks every entry except the last.
fn encode_set_equipment(eid: i32, eq: &EntityEquipment) -> Vec<u8> {
    let entries: Vec<(u8, &Stack)> = eq
        .slots
        .iter()
        .enumerate()
        .filter(|(_, st)| st.1 > 0)
        .map(|(i, st)| (i as u8, st))
        .collect();
    let mut p = Vec::new();
    write_varint(&mut p, i64::from(eid));
    for (i, (slot, st)) in entries.iter().enumerate() {
        p.push(slot | if i + 1 < entries.len() { 0x80 } else { 0 });
        p.extend_from_slice(&slot_wire(st));
    }
    p
}

#[cfg(test)]
fn decode_set_equipment(b: &[u8]) -> Option<(i32, Vec<(u8, Stack)>)> {
    let mut rest = b;
    let eid = read_protocol_i32(&mut rest)?;
    let mut entries = Vec::new();
    loop {
        let (&byte, tail) = rest.split_first()?;
        rest = tail;
        let slot = byte & 0x7f;
        if slot > 5 {
            return None;
        }
        let (st, used) = wm_stack_decode(rest)?;
        rest = &rest[used..];
        entries.push((slot, st));
        if byte & 0x80 == 0 {
            break;
        }
    }
    rest.is_empty().then_some((eid, entries))
}

// Vanilla `Mob.finalizeSpawn` equipment picks (weapon-only core; armor rolls are
// a P2-C follow-up). `roll` bits: low byte drives the drowned weapon chance,
// bits 8..16 the piglin sword/crossbow pick.
fn default_mob_equipment(entity_name: &str, difficulty: u8, roll: u64) -> EntityEquipment {
    let _ = difficulty;
    let mut eq = EntityEquipment::default();
    let name = entity_name.trim_start_matches("minecraft:");
    let stack = |n: &str| name_to_item(n).map(|id| (id, 1u8, Vec::new()));
    let main = match name {
        "skeleton" | "stray" | "bogged" => stack("bow"),
        "wither_skeleton" => stack("stone_sword"),
        "pillager" => stack("crossbow"),
        "vindicator" => stack("iron_axe"),
        "piglin" => {
            if (roll >> 8) & 1 == 0 {
                stack("golden_sword")
            } else {
                stack("crossbow")
            }
        }
        "zombified_piglin" => stack("golden_sword"),
        "drowned" => {
            // Java: ~6.25% trident, ~3.75% fishing rod on natural spawn.
            let pct = roll % 100;
            if pct < 6 {
                stack("trident")
            } else if pct < 10 {
                stack("fishing_rod")
            } else {
                None
            }
        }
        _ => None,
    };
    if let Some(st) = main {
        eq.set(EquipmentSlot::MainHand, st);
    }
    eq
}

// ---------- mount / passenger graph (S2C 0x5f SetPassengers + C2S vehicles) ----------
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum VehicleKind {
    Boat,
    ChestBoat,
    Minecart,
    Horse,
    Donkey,
    Mule,
    Pig,
    Strider,
    Camel,
    Llama,
}

#[derive(Clone, Copy, Debug, Default)]
struct VehicleEnv {
    on_water: bool,
    on_ice: bool,
    on_lava: bool,
    on_rail: bool,
    // LivingEntity MOVEMENT_SPEED attribute; used by rideable mobs.
    move_speed: f64,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum MountError {
    Full,
    SelfMount,
    Cycle,
    UnknownVehicle,
}

impl VehicleKind {
    fn max_passengers(self) -> usize {
        match self {
            Self::Boat | Self::ChestBoat | Self::Camel => 2,
            _ => 1,
        }
    }

    // Seat attachment heights (vanilla getPassengerAttachmentPoint scale, ~0.75
    // of the entity bounding height; index 1 is the rear/second seat).
    fn seat_offset(self, index: usize) -> [f64; 3] {
        let (y, back) = match self {
            Self::Boat | Self::ChestBoat => (0.35, -0.6),
            Self::Minecart => (0.15, 0.0),
            Self::Horse => (1.15, 0.0),
            Self::Donkey => (0.95, 0.0),
            Self::Mule => (1.05, 0.0),
            Self::Pig => (0.55, 0.0),
            Self::Strider => (0.9, 0.0),
            Self::Camel => (1.45, -0.35),
            Self::Llama => (1.1, 0.0),
        };
        if index == 0 {
            [0.0, y, 0.0]
        } else {
            [0.0, y, back]
        }
    }

    // Per-tick horizontal speed allowance feeding the anti-cheat carry bridge
    // (vanilla ServerGamePacketListenerImpl "moved too quickly" precedents,
    // per-tick scale), now sourced from the vanilla vehicle physics below:
    // boats sit at the paddled terminal thrust/(1-friction), carts are clamped
    // to getMaxSpeed on every path, and boosted mounts scale with their
    // MOVEMENT_SPEED attribute at the maximum 1 + 1.15 boost multiplier.
    fn max_step(self, env: &VehicleEnv) -> f64 {
        match self {
            Self::Boat | Self::ChestBoat => {
                // `on_ice` covers every icy support; the gate uses blue_ice's
                // 0.989 so a legal blue-ice slide can never trip it.
                let friction = if env.on_water {
                    WM_BOAT_FRICTION_SURFACE
                } else if env.on_ice {
                    wm_support_friction("blue_ice")
                } else {
                    wm_support_friction("")
                };
                // 1.25 slack covers surface transitions and entry momentum.
                wm_boat_terminal_speed(friction) * 1.25
            }
            // Both the on-rail (cot.c) and off-rail (cot.s) vanilla moves clamp
            // horizontal velocity to getMaxSpeed each tick.
            Self::Minecart if env.on_rail => wm_minecart_max_speed(env.on_water),
            Self::Minecart => wm_minecart_max_speed(env.on_water),
            Self::Pig => {
                wm_pig_ride_speed(env.move_speed.max(WM_PIG_BASE_SPEED), WM_BOOST_MAX)
            }
            Self::Strider => wm_strider_ride_speed(
                env.move_speed.max(WM_STRIDER_BASE_SPEED),
                !env.on_lava,
                WM_BOOST_MAX,
            ),
            Self::Horse | Self::Donkey | Self::Mule | Self::Llama | Self::Camel => {
                (2.4 * env.move_speed).clamp(0.05, 0.9)
            }
        }
    }

    fn dismount_offset(self) -> [f64; 3] {
        match self {
            Self::Boat | Self::ChestBoat => [0.7, 0.0, 0.0],
            Self::Minecart => [0.7, 0.0, 0.0],
            _ => [0.85, 0.0, 0.0],
        }
    }
}

// ---------- Vanilla 1.21.1 vehicle physics (cov/cot/chi/cfv/ckq) ----------

/// `Boat.Status` (cov$a): the hull's relation to its support, re-evaluated
/// every tick before `floatBoat` picks friction and buoyancy terms.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WmBoatStatus {
    InWater,
    UnderFlowingWater,
    #[cfg_attr(not(test), allow(dead_code))]
    UnderWater,
    OnLand,
    InAir,
}

const WM_BOAT_FRICTION_SURFACE: f64 = 0.9;
const WM_BOAT_FRICTION_SUBMERGED: f64 = 0.45;
/// `getDefaultGravity` for boats (cov.aZ); the buoyancy rise scales by it/0.65.
const WM_BOAT_GRAVITY: f64 = 0.04;
const WM_BOAT_FLOWING_SINK: f64 = -7.0e-4;
const WM_BOAT_SUBMERGED_LIFT: f64 = 0.01;

/// Vanilla block `getFriction` (BlockBehaviour.Properties#friction): the
/// support-block value an ON_LAND boat slides on.
fn wm_support_friction(block: &str) -> f64 {
    match block {
        "ice" | "packed_ice" | "frosted_ice" => 0.98,
        "blue_ice" => 0.989,
        "slime_block" => 0.8,
        "honey_block" | "soul_sand" => 0.4,
        _ => 0.6,
    }
}

/// `floatBoat` (cov.I) `invFriction` per status. ON_LAND uses the support
/// block's friction; vanilla's `landFriction /= 2` for a steering player is a
/// dead write — `getStatus` recomputes the field before it is read again.
fn wm_boat_friction(status: WmBoatStatus, land_friction: f64) -> f64 {
    match status {
        WmBoatStatus::InWater | WmBoatStatus::UnderFlowingWater | WmBoatStatus::InAir => {
            WM_BOAT_FRICTION_SURFACE
        }
        WmBoatStatus::UnderWater => WM_BOAT_FRICTION_SUBMERGED,
        WmBoatStatus::OnLand => land_friction,
    }
}

/// Steady-state horizontal speed of a paddled boat: thrust per tick divided by
/// the fraction of velocity lost to friction (`v*0.04 = v*(1-f)`).
fn wm_boat_terminal_speed(friction: f64) -> f64 {
    WM_BOAT_FORWARD_THRUST / (1.0 - friction)
}

/// The vanilla `d1` buoyancy term: hull submersion fraction on the surface,
/// a fixed 0.01 while UNDER_WATER, 0 otherwise.
fn wm_boat_lift(status: WmBoatStatus, submerged_fraction: f64) -> f64 {
    match status {
        WmBoatStatus::InWater => submerged_fraction,
        WmBoatStatus::UnderWater => WM_BOAT_SUBMERGED_LIFT,
        _ => 0.0,
    }
}

/// One `floatBoat` step (cov.I): `v = (vx*f, vy + buoyTerm, vz*f)` and
/// `deltaRotation *= f`; when `lift > 0` the y term is rescaled by 0.75 after
/// the buoyancy kick. Returns `(velocity, damped_delta_rotation)`.
fn wm_boat_float_tick(
    status: WmBoatStatus,
    land_friction: f64,
    gravity: f64,
    vel: [f64; 3],
    lift: f64,
    delta_rot: f64,
) -> ([f64; 3], f64) {
    let friction = wm_boat_friction(status, land_friction);
    let buoy_term = if status == WmBoatStatus::UnderFlowingWater {
        WM_BOAT_FLOWING_SINK
    } else {
        -gravity
    };
    let mut next = [vel[0] * friction, vel[1] + buoy_term, vel[2] * friction];
    let rot = delta_rot * friction;
    if lift > 0.0 {
        next[1] = (next[1] + lift * (WM_BOAT_GRAVITY / 0.65)) * 0.75;
    }
    (next, rot)
}

/// Paddle flags from `ServerboundPaddleBoatPacket` / steering input.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct WmBoatInput {
    left: bool,
    right: bool,
    up: bool,
    down: bool,
}

const WM_BOAT_PADDLE_YAW: f64 = 1.0;
const WM_BOAT_TURN_DRIFT: f64 = 0.005;
const WM_BOAT_FORWARD_THRUST: f64 = 0.04;
const WM_BOAT_REVERSE_THRUST: f64 = 0.005;

/// Vanilla `controlBoat` (cov.J): each rowing side paddle steps deltaRotation
/// by 1 degree/tick, one-sided paddling without row input drifts at 0.005,
/// `up` rows forward at 0.04, `down` backpaddles at 0.005. The thrust vector
/// uses the yaw *after* the rotation step. Returns `(yaw, delta_rot, vel)`.
fn wm_boat_control_tick(
    input: WmBoatInput,
    yaw: f64,
    delta_rot: f64,
    vel: [f64; 3],
) -> (f64, f64, [f64; 3]) {
    let mut rot = delta_rot;
    if input.left {
        rot -= WM_BOAT_PADDLE_YAW;
    }
    if input.right {
        rot += WM_BOAT_PADDLE_YAW;
    }
    let mut thrust = 0.0;
    if input.right != input.left && !input.up && !input.down {
        thrust += WM_BOAT_TURN_DRIFT;
    }
    let yaw = yaw + rot;
    if input.up {
        thrust += WM_BOAT_FORWARD_THRUST;
    }
    if input.down {
        thrust -= WM_BOAT_REVERSE_THRUST;
    }
    let rad = yaw.to_radians();
    (
        yaw,
        rot,
        [
            vel[0] + -rad.sin() * thrust,
            vel[1],
            vel[2] + rad.cos() * thrust,
        ],
    )
}

/// `AbstractMinecart.getMaxSpeed` (cot.p): `(inWater ? 4.0 : 8.0) / 20` per
/// tick — the vanilla 8 m/s rail cap, halved under water.
fn wm_minecart_max_speed(in_water: bool) -> f64 {
    (if in_water { 4.0 } else { 8.0 }) / 20.0
}

/// `applyNaturalSlowdown` (cot.t): `v *= (d, 0, d)` with `d = 0.997` when a
/// passenger rides, else `0.96`; under water every axis takes a further 0.95.
fn wm_minecart_slowdown(has_passenger: bool, in_water: bool, vel: [f64; 3]) -> [f64; 3] {
    let damp = if has_passenger { 0.997 } else { 0.96 };
    let mut next = [vel[0] * damp, 0.0, vel[2] * damp];
    if in_water {
        next = [next[0] * 0.95, next[1] * 0.95, next[2] * 0.95];
    }
    next
}

#[cfg(test)]
const WM_MINECART_SLOPE_BOOST: f64 = 0.0078125;

/// Acceleration applied along ascending rail shapes each tick; 0.2x in water.
#[cfg(test)]
fn wm_minecart_slope_boost(in_water: bool) -> f64 {
    WM_MINECART_SLOPE_BOOST * if in_water { 0.2 } else { 1.0 }
}

/// The straight `RailShape`s that can give the powered-rail static kick.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WmRailShape {
    NorthSouth,
    EastWest,
    Other,
}

/// Powered-rail tail of `moveAlongTrack` (cot.c): at speed above 0.01 the cart
/// gains 0.06 along its own direction; a standing cart is kicked 0.02 away
/// from a redstone-conductor wall along the rail axis.
fn wm_minecart_powered_boost(
    vx: f64,
    vz: f64,
    shape: WmRailShape,
    conductor_neg: bool,
    conductor_pos: bool,
) -> (f64, f64) {
    let len = vx.hypot(vz);
    if len > 0.01 {
        (vx + vx / len * 0.06, vz + vz / len * 0.06)
    } else {
        match shape {
            WmRailShape::EastWest => (
                if conductor_neg {
                    0.02
                } else if conductor_pos {
                    -0.02
                } else {
                    vx
                },
                vz,
            ),
            WmRailShape::NorthSouth => (
                vx,
                if conductor_neg {
                    0.02
                } else if conductor_pos {
                    -0.02
                } else {
                    vz
                },
            ),
            WmRailShape::Other => (vx, vz),
        }
    }
}

/// An unpowered powered-rail brakes the cart (`$$7` in cot.c): below 0.03
/// blocks/tick it stops dead, otherwise half the horizontal speed is shed.
fn wm_minecart_rail_brake(vx: f64, vz: f64) -> (f64, f64) {
    if vx.hypot(vz) < 0.03 {
        (0.0, 0.0)
    } else {
        (vx * 0.5, vz * 0.5)
    }
}

/// `moveAlongTrack`'s move call: horizontal velocity (0.75x while ridden) is
/// clamped to the per-tick max speed; y is carried by the rail, not velocity.
fn wm_minecart_rail_move(
    vx: f64,
    vz: f64,
    has_passenger: bool,
    in_water: bool,
) -> (f64, f64) {
    let max = wm_minecart_max_speed(in_water);
    let scale = if has_passenger { 0.75 } else { 1.0 };
    (
        (scale * vx).clamp(-max, max),
        (scale * vz).clamp(-max, max),
    )
}

/// `DetectorRailBlock`: emits 15 while a minecart stands on it.
fn wm_detector_rail_signal(occupied: bool) -> u8 {
    if occupied {
        15
    } else {
        0
    }
}

/// `ItemBasedSteering` (cjp/cjq) shared by pig and strider boosts: the total
/// boost is `nextInt(841) + 140` ticks and the multiplier sweeps
/// `1 + 1.15 * sin(pi * t / total)`.
#[cfg(test)]
fn wm_boost_total_ticks(roll_0_840: u32) -> u32 {
    140 + roll_0_840
}

const WM_BOOST_MAX: f64 = 1.0 + 1.15;

fn wm_boost_multiplier(elapsed: u32, total: u32) -> f64 {
    if total == 0 {
        return 0.0;
    }
    1.0 + 1.15 * (elapsed as f64 / total as f64 * std::f64::consts::PI).sin()
}

/// Vanilla pig ride speed (cfv): `MOVEMENT_SPEED * 0.225 * boostFactor`, with
/// the 0.25 base attribute this is 0.05625 before the carrot-on-a-stick boost.
const WM_PIG_BASE_SPEED: f64 = 0.25;
const WM_PIG_RIDE_FACTOR: f64 = 0.225;

fn wm_pig_ride_speed(movement_speed: f64, boost_multiplier: f64) -> f64 {
    movement_speed * WM_PIG_RIDE_FACTOR * boost_multiplier
}

/// Vanilla strider ride speed (ckq):
/// `MOVEMENT_SPEED * (cold ? 0.35 : 0.55) * boostFactor`, on the 0.175 base attribute. A strider is cold whenever it
/// is not standing on warm lava — the shivering state.
const WM_STRIDER_BASE_SPEED: f64 = 0.175;
const WM_STRIDER_COLD_FACTOR: f64 = 0.35;
const WM_STRIDER_WARM_FACTOR: f64 = 0.55;

fn wm_strider_ride_speed(movement_speed: f64, cold: bool, boost_multiplier: f64) -> f64 {
    movement_speed * if cold { WM_STRIDER_COLD_FACTOR } else { WM_STRIDER_WARM_FACTOR }
        * boost_multiplier
}

/// Strider lava buoyancy (ckq): inside lava `v *= 0.5` then +0.05 to y.
fn wm_strider_lava_buoyancy(vel: [f64; 3]) -> [f64; 3] {
    [vel[0] * 0.5, vel[1] * 0.5 + 0.05, vel[2] * 0.5]
}

/// `AbstractHorse.handleStartJump` (chi.b(int)): the client's 0..=100 jump
/// meter clamps negatives to 0, maps <90 onto 0.4..0.8 and >=90 to 1.0.
/// Shared by every AbstractHorse descendant (horses, donkeys, mules, camels).
fn wm_horse_jump_scale(charge: i32) -> f64 {
    let charge = charge.max(0);
    if charge >= 90 {
        1.0
    } else {
        0.4 + 0.4 * charge as f64 / 90.0
    }
}

/// `executeRidersJump` vertical term: `getCustomJump() * scale *
/// getBlockJumpFactor() + getJumpBoostPower()`.
fn wm_horse_jump_velocity(
    jump_strength: f64,
    scale: f64,
    block_jump_factor: f64,
    jump_boost: f64,
) -> f64 {
    jump_strength * scale * block_jump_factor + jump_boost
}

/// `LivingEntity.getJumpBoostPower`: `0.1 * (amplifier + 1)` while the jump
/// boost effect is active.
fn wm_jump_boost_power(amplifier: Option<i32>) -> f64 {
    amplifier.map_or(0.0, |amp| 0.1 * (amp as f64 + 1.0))
}

/// The horizontal kick `executeRidersJump` adds while the rider steers forward
/// (`vec3.z > 0`): `(-0.4 * sin(yaw) * scale, 0, 0.4 * cos(yaw) * scale)`.
fn wm_horse_jump_horizontal(yaw: f64, scale: f64) -> (f64, f64) {
    let rad = yaw.to_radians();
    (-0.4 * rad.sin() * scale, 0.4 * rad.cos() * scale)
}

/// Spawn attribute rolls (chi.a/chi.b): JUMP_STRENGTH = `0.4 + 0.2 * (r1+r2+r3)`
/// and MOVEMENT_SPEED = `(0.45 + 0.3 * (r1+r2+r3)) * 0.25`.
#[cfg(test)]
fn wm_horse_spawn_jump_strength(rolls: [f64; 3]) -> f64 {
    0.4 + 0.2 * rolls.iter().sum::<f64>()
}

#[cfg(test)]
fn wm_horse_spawn_movement_speed(rolls: [f64; 3]) -> f64 {
    (0.45 + 0.3 * rolls.iter().sum::<f64>()) * 0.25
}

#[derive(Default)]
struct MountRegistry {
    kinds: HashMap<i32, VehicleKind>,
    positions: HashMap<i32, [f64; 3]>,
    passengers: HashMap<i32, Vec<i32>>, // vehicle eid -> seat order
    vehicle_of: HashMap<i32, i32>,      // passenger eid -> vehicle eid
}

impl MountRegistry {
    fn register_vehicle(&mut self, eid: i32, kind: VehicleKind, pos: [f64; 3]) {
        self.kinds.insert(eid, kind);
        self.positions.insert(eid, pos);
    }

    fn register_rider(&mut self, eid: i32, pos: [f64; 3]) {
        self.positions.insert(eid, pos);
    }

    fn mount(&mut self, vehicle: i32, passenger: i32) -> Result<(), MountError> {
        if vehicle == passenger {
            return Err(MountError::SelfMount);
        }
        if !self.kinds.contains_key(&vehicle) {
            return Err(MountError::UnknownVehicle);
        }
        if self.vehicle_of.contains_key(&passenger) {
            return Err(MountError::Cycle);
        }
        // An ancestor chain vehicle -> ... -> passenger would close a loop.
        let mut cursor = vehicle;
        let mut steps = 0;
        while let Some(&parent) = self.vehicle_of.get(&cursor) {
            if parent == passenger {
                return Err(MountError::Cycle);
            }
            cursor = parent;
            steps += 1;
            if steps > 64 {
                return Err(MountError::Cycle);
            }
        }
        let seats = self.passengers.entry(vehicle).or_default();
        if seats.len() >= self.kinds[&vehicle].max_passengers() {
            return Err(MountError::Full);
        }
        seats.push(passenger);
        self.vehicle_of.insert(passenger, vehicle);
        Ok(())
    }

    // Returns the safe dismount position (vehicle pos + lateral offset).
    fn dismount(&mut self, passenger: i32) -> Option<[f64; 3]> {
        let vehicle = self.vehicle_of.remove(&passenger)?;
        if let Some(seats) = self.passengers.get_mut(&vehicle) {
            seats.retain(|&e| e != passenger);
        }
        let base = *self.positions.get(&vehicle)?;
        let off = self.kinds.get(&vehicle).map(|k| k.dismount_offset()).unwrap_or([0.7, 0.0, 0.0]);
        let at = [base[0] + off[0], base[1] + off[1], base[2] + off[2]];
        self.positions.insert(passenger, at);
        Some(at)
    }

    fn passengers_of(&self, vehicle: i32) -> &[i32] {
        self.passengers.get(&vehicle).map(Vec::as_slice).unwrap_or(&[])
    }

    // `ClientboundSetPassengersPacket` body: VarInt vehicle, VarInt count, ids.
    fn encode_set_passengers(&self, vehicle: i32) -> Vec<u8> {
        let seats = self.passengers_of(vehicle);
        let mut p = Vec::new();
        write_varint(&mut p, i64::from(vehicle));
        write_varint(&mut p, seats.len() as i64);
        for &e in seats {
            write_varint(&mut p, i64::from(e));
        }
        p
    }

    // Applies a MoveVehicle target through the vanilla anti-cheat gate: the
    // target is clamped to the vanilla bounds (x/z +-3e7, y +-2e7) and the
    // per-tick step must not exceed the kind's context speed. On success the
    // vehicle moves and every seated passenger is re-seated at its attachment
    // offset; the returned carry_delta feeds ac_note_carried_motion for the
    // controlling passenger so vehicle motion never trips the mover check.
    fn step_vehicle_move(
        &mut self,
        vehicle: i32,
        pkt: &MoveVehiclePacket,
        env: &VehicleEnv,
    ) -> Result<VehicleMoveStep, VehicleReject> {
        let Some(&kind) = self.kinds.get(&vehicle) else {
            return Err(VehicleReject::UnknownVehicle);
        };
        let target = [
            pkt.x.clamp(-3.0e7, 3.0e7),
            pkt.y.clamp(-2.0e7, 2.0e7),
            pkt.z.clamp(-3.0e7, 3.0e7),
        ];
        let pos = self.positions.get(&vehicle).copied().unwrap_or(target);
        let delta = [
            target[0] - pos[0],
            target[1] - pos[1],
            target[2] - pos[2],
        ];
        let step = (delta[0] * delta[0] + delta[2] * delta[2]).sqrt();
        if step > kind.max_step(env) + 1.0e-9 {
            return Err(VehicleReject::TooQuickly);
        }
        self.positions.insert(vehicle, target);
        let seats = self.passengers_of(vehicle).to_vec();
        for (i, e) in seats.iter().enumerate() {
            let off = kind.seat_offset(i);
            self.positions.insert(
                *e,
                [target[0] + off[0], target[1] + off[1], target[2] + off[2]],
            );
        }
        Ok(VehicleMoveStep {
            carry_delta: (delta[0], delta[1], delta[2]),
            passengers_moved: seats.len(),
            pos: target,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum VehicleReject {
    UnknownVehicle,
    TooQuickly,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct VehicleMoveStep {
    carry_delta: (f64, f64, f64),
    passengers_moved: usize,
    pos: [f64; 3],
}

// `ServerboundMoveVehiclePacket` (C2S 0x1e): x,y,z f64 + yaw,pitch f32.
// Non-finite coordinates are the vanilla invalid_vehicle_movement disconnect.
#[derive(Clone, Copy, Debug, PartialEq)]
struct MoveVehiclePacket {
    x: f64,
    y: f64,
    z: f64,
    yaw: f32,
    pitch: f32,
}

fn decode_move_vehicle(b: &[u8]) -> Option<MoveVehiclePacket> {
    if b.len() != 32 {
        return None;
    }
    let pkt = MoveVehiclePacket {
        x: f64::from_be_bytes(b[0..8].try_into().unwrap_or_default()),
        y: f64::from_be_bytes(b[8..16].try_into().unwrap_or_default()),
        z: f64::from_be_bytes(b[16..24].try_into().unwrap_or_default()),
        yaw: f32::from_be_bytes(b[24..28].try_into().unwrap_or_default()),
        pitch: f32::from_be_bytes(b[28..32].try_into().unwrap_or_default()),
    };
    (pkt.x.is_finite() && pkt.y.is_finite() && pkt.z.is_finite() && pkt.yaw.is_finite() && pkt.pitch.is_finite())
        .then_some(pkt)
}

// `ServerboundPaddleBoatPacket` (C2S 0x1f): two bools.
#[derive(Clone, Copy, Debug, PartialEq)]
struct PaddleBoatPacket {
    left: bool,
    right: bool,
}

fn decode_paddle_boat(b: &[u8]) -> Option<PaddleBoatPacket> {
    (b.len() == 2).then(|| PaddleBoatPacket {
        left: b[0] != 0,
        right: b[1] != 0,
    })
}

// `ServerboundPlayerInputPacket` (C2S 0x26): f32 sideways, f32 forward, u8
// flags — bit0 jump, bit1 unmount/shift.
#[derive(Clone, Copy, Debug, PartialEq)]
struct PlayerInputPacket {
    sideways: f32,
    forward: f32,
    jump: bool,
    unmount: bool,
}

fn decode_player_input(b: &[u8]) -> Option<PlayerInputPacket> {
    (b.len() == 9).then(|| {
        let sideways = f32::from_be_bytes(b[0..4].try_into().unwrap_or_default());
        let forward = f32::from_be_bytes(b[4..8].try_into().unwrap_or_default());
        PlayerInputPacket {
            sideways,
            forward,
            jump: b[8] & 0x01 != 0,
            unmount: b[8] & 0x02 != 0,
        }
    })
    .filter(|p| p.sideways.is_finite() && p.forward.is_finite())
}

#[cfg(test)]
fn decode_set_passengers(b: &[u8]) -> Option<(i32, Vec<i32>)> {
    let mut rest = b;
    let vehicle = read_protocol_i32(&mut rest)?;
    let count = read_protocol_i32(&mut rest)?;
    if !(0..=64).contains(&count) {
        return None;
    }
    let mut out = Vec::with_capacity(count as usize);
    for _ in 0..count {
        out.push(read_protocol_i32(&mut rest)?);
    }
    rest.is_empty().then_some((vehicle, out))
}

#[cfg(test)]
mod workstations_and_mounts_tests {
    use super::*;

    fn item(name: &str) -> i64 {
        name_to_item(name).expect(name)
    }

    fn st(name: &str, count: u8, comps: Comps) -> Stack {
        (item(name), count, comps)
    }

    fn ench(list: &[(i64, i64)]) -> Comps {
        vec![(COMP_ENCHANTMENTS, enchantments_data(list, true))]
    }

    #[test]
    fn stonecutter_lists_recipes_and_crafts_selected_output() {
        assert_eq!(StonecutterMenu::screen().0, 24);
        let mut menu = StonecutterMenu::default();
        menu.set_input(st("stone", 1, Vec::new()));
        let recipes = menu.available_recipes();
        assert!(recipes.contains(&(item("stone_bricks"), 1)));
        assert!(recipes.contains(&(item("stone_slab"), 2)));
        assert!(recipes.contains(&(item("chiseled_stone_bricks"), 1)));
        let mut sorted = recipes.clone();
        sorted.sort();
        assert_eq!(recipes, sorted, "recipe list must be deterministic-sorted");
        // Button select previews the result; take consumes one input.
        let slab = recipes.iter().position(|&(i, _)| i == item("stone_slab")).unwrap();
        assert!(menu.select_button(slab));
        assert_eq!(menu.result(), (item("stone_slab"), 2, Vec::new()));
        let out = menu.take_result().unwrap();
        assert_eq!(out, (item("stone_slab"), 2, Vec::new()));
        assert_eq!(menu.input.1, 0);
        // Out-of-range button clears the selection instead of crafting.
        menu.set_input(st("stone", 1, Vec::new()));
        assert!(!menu.select_button(recipes.len()));
        assert_eq!(menu.take_result(), None);
        // Copper keeps the vanilla 4x cut multiplier (8x for slabs).
        menu.set_input(st("copper_block", 1, Vec::new()));
        let copper = menu.available_recipes();
        assert!(copper.contains(&(item("cut_copper"), 4)));
        assert!(copper.contains(&(item("cut_copper_slab"), 8)));
        assert!(copper.contains(&(item("chiseled_copper"), 4)));
        // Non-stonecuttable input yields no recipes.
        menu.set_input(st("dirt", 1, Vec::new()));
        assert!(menu.available_recipes().is_empty());
    }

    #[test]
    fn grindstone_repairs_with_5pct_bonus_strips_enchants_preserves_curses_and_refunds_xp() {
        assert_eq!(GrindstoneMenu::screen().0, 15);
        // Single enchanted item: non-curse stripped, curse kept, repair_cost cleared.
        let mut comps = ench(&[(32, 5), (2, 1)]); // sharpness V + binding I
        comp_set_varint(&mut comps, WM_COMP_REPAIR_COST, 7);
        let mut menu = GrindstoneMenu::default();
        menu.set_slots((item("iron_sword"), 1, comps), Stack::default());
        let (out, xp) = menu.take_result(0.5).unwrap();
        assert_eq!(enchantments_get(&out.2), vec![(2, 1)]);
        assert!(comp_varint(&out.2, WM_COMP_REPAIR_COST).is_none());
        // xp_base = min_cost(sharpness V) = 1 + 11*4 = 45 -> j = ceil(45/2) = 23.
        let j = (45 + 1) / 2;
        assert_eq!(xp, j + (0.5 * j as f64) as i64);
        assert!((j..=2 * j - 1).contains(&xp));

        // Two identical damageable items: durability merge + 5% of max bonus.
        let damaged = |dmg: i64, list: &[(i64, i64)]| {
            let mut c = ench(list);
            comp_set_varint(&mut c, COMP_MAX_DAMAGE, 250);
            comp_set_varint(&mut c, COMP_DAMAGE, dmg);
            (item("iron_sword"), 1u8, c)
        };
        let mut pair = GrindstoneMenu::default();
        pair.set_slots(damaged(200, &[(32, 2), (40, 1)]), damaged(200, &[(39, 3)]));
        let (merged, xp2) = pair.compute().unwrap();
        // remaining = 50 + 50 + floor(250*5/100)=12 -> 112 -> damage = 250-112.
        assert_eq!(comp_varint(&merged.2, COMP_DAMAGE), Some(250 - 112));
        // Only the bottom's vanishing curse survives the merge; sharpness/unbreaking go.
        assert_eq!(enchantments_get(&merged.2), vec![(40, 1)]);
        // xp_base = min_cost(sharpness II)=12 + min_cost(unbreaking III)=21 -> 33.
        assert_eq!(xp2, 33);
        let (_, xp2) = pair.take_result(0.0).unwrap();
        assert_eq!(xp2, (33 + 1) / 2);
        assert!(pair.top.1 == 0 && pair.bottom.1 == 0);

        // A non-enchanted single item is rejected, as is a mismatched pair.
        let mut lone = GrindstoneMenu::default();
        lone.set_slots(st("iron_sword", 1, Vec::new()), Stack::default());
        assert_eq!(lone.compute(), None);
        let mut mixed = GrindstoneMenu::default();
        mixed.set_slots(st("iron_sword", 1, Vec::new()), st("iron_axe", 1, Vec::new()));
        assert_eq!(mixed.compute(), None);
    }

    #[test]
    fn loom_applies_patterns_up_to_six_layers_and_preserves_pattern_item() {
        assert_eq!(LoomMenu::screen().0, 18);
        let mut menu = LoomMenu::default();
        menu.set_slots(
            st("white_banner", 1, Vec::new()),
            st("red_dye", 3, Vec::new()),
            Stack::default(),
        );
        assert_eq!(menu.selectable_patterns().len(), WM_LOOM_PATTERNS.len());
        assert!(!menu.select_pattern("creeper")); // needs the pattern item
        assert!(menu.select_pattern("border"));
        assert!(menu.can_apply());
        assert_eq!(menu.result().0, item("white_banner"));
        let (out, layers) = menu.take_result().unwrap();
        assert_eq!(out.0, item("white_banner"));
        assert_eq!(
            layers,
            vec![BannerLayer { pattern: "border".to_string(), color: 14 }]
        );
        assert_eq!(menu.dye.1, 2);

        // Fill to the 6-layer cap with free patterns (re-seat banner + dye).
        for pattern in ["cross", "circle", "border", "triangle_top", "triangle_bottom"] {
            let cur = menu.banner_layers.clone();
            menu.set_banner(st("white_banner", 1, Vec::new()), cur);
            menu.dye = st("red_dye", 1, Vec::new());
            menu.pattern_item = Stack::default();
            assert!(menu.select_pattern(pattern));
            menu.take_result().unwrap();
        }
        assert_eq!(menu.banner_layers.len(), 6);
        menu.set_banner(st("white_banner", 1, Vec::new()), menu.banner_layers.clone());
        menu.dye = st("red_dye", 1, Vec::new());
        assert!(menu.select_pattern("bricks"));
        assert!(!menu.can_apply(), "7th free-pattern layer must be rejected");
        // A banner-pattern item bypasses the cap and is not consumed.
        menu.pattern_item = st("creeper_banner_pattern", 1, Vec::new());
        assert_eq!(menu.selectable_patterns(), vec!["creeper".to_string()]);
        assert!(menu.select_pattern("creeper"));
        let (_, layers) = menu.take_result().unwrap();
        assert_eq!(layers.len(), 7);
        assert_eq!(layers.last().unwrap().pattern, "creeper");
        assert_eq!(layers.last().unwrap().color, 14);
        assert_eq!(menu.pattern_item.1, 1, "pattern item must be preserved");
    }

    #[test]
    fn cartography_zooms_clones_and_locks_maps() {
        assert_eq!(CartographyMenu::screen().0, 23);
        let state = MapState { map_id: 7, scale: 2, locked: false };
        // Zoom out on paper: scale + 1, fresh map id.
        let mut zoom = CartographyMenu::default();
        zoom.set_map(st("filled_map", 1, Vec::new()), Some(state));
        zoom.set_addition(st("paper", 4, Vec::new()));
        assert!(zoom.map_valid());
        assert!(zoom.preview().is_some());
        let (out, out_state) = zoom.take_result(42).unwrap();
        assert_eq!(out.1, 1);
        assert_eq!(out_state.scale, 3);
        assert_eq!(out_state.map_id, 42);
        assert_eq!(comp_varint(&out.2, WM_COMP_MAP_ID), Some(42));
        assert_eq!(zoom.addition.1, 3);
        // Clone on an empty map: two copies, same id and scale.
        let mut clone = CartographyMenu::default();
        clone.set_map(st("filled_map", 1, Vec::new()), Some(state));
        clone.set_addition(st("map", 1, Vec::new()));
        let (out, out_state) = clone.take_result(99).unwrap();
        assert_eq!(out.1, 2);
        assert_eq!(out_state.map_id, 7);
        assert_eq!(out_state.scale, 2);
        // Lock on a glass pane.
        let mut lock = CartographyMenu::default();
        lock.set_map(st("filled_map", 1, Vec::new()), Some(state));
        lock.set_addition(st("glass_pane", 1, Vec::new()));
        let (_, out_state) = lock.take_result(99).unwrap();
        assert!(out_state.locked);
        // Locked or max-scale maps refuse paper; glass refuses a locked map.
        for (state, add) in [
            (MapState { locked: true, ..state }, "paper"),
            (MapState { scale: 4, ..state }, "paper"),
            (MapState { locked: true, ..state }, "glass_pane"),
        ] {
            let mut menu = CartographyMenu::default();
            menu.set_map(st("filled_map", 1, Vec::new()), Some(state));
            menu.set_addition(st(add, 1, Vec::new()));
            assert_eq!(menu.preview(), None);
        }
        // A stacked or non-filled input and a useless addition are rejected.
        let mut menu = CartographyMenu::default();
        menu.set_map(st("filled_map", 2, Vec::new()), Some(state));
        menu.set_addition(st("paper", 1, Vec::new()));
        assert_eq!(menu.preview(), None);
        menu.set_map(st("stick", 1, Vec::new()), None);
        assert_eq!(menu.preview(), None);
    }

    #[test]
    fn set_equipment_0x5b_and_set_passengers_0x5f_wire_roundtrip_exact() {
        // 0x5b: entity VarInt + (slot | 0x80, stack)* entries, last unflagged.
        let sword = (
            item("iron_sword"),
            1,
            vec![(COMP_ENCHANTMENTS, enchantments_data(&[(32, 5)], true))],
        );
        let helmet = st("iron_helmet", 1, Vec::new());
        let mut eq = EntityEquipment::default();
        eq.set(EquipmentSlot::MainHand, sword.clone());
        eq.set(EquipmentSlot::Head, helmet.clone());
        assert_eq!(eq.get(EquipmentSlot::MainHand), &sword);
        for slot in [
            EquipmentSlot::OffHand,
            EquipmentSlot::Feet,
            EquipmentSlot::Legs,
            EquipmentSlot::Chest,
        ] {
            eq.set(slot, st("iron_ingot", 1, Vec::new()));
            assert_eq!(eq.get(slot).0, item("iron_ingot"));
            eq.set(slot, Stack::default());
        }
        let pkt = encode_set_equipment(1337, &eq);
        let mut expect = Vec::new();
        write_varint(&mut expect, 1337);
        expect.push(0x80); // slot MainHand, more entries follow
        expect.extend_from_slice(&slot_wire(&sword));
        expect.push(0x05); // slot Head, last entry: no 0x80
        expect.extend_from_slice(&slot_wire(&helmet));
        assert_eq!(pkt, expect);
        let (eid, entries) = decode_set_equipment(&pkt).unwrap();
        assert_eq!(eid, 1337);
        assert_eq!(entries, vec![(0, sword), (5, helmet)]);
        // Rejects an out-of-range slot and a truncated tail.
        assert_eq!(decode_set_equipment(&[0x86, 0x7f]), None);
        assert_eq!(decode_set_equipment(&pkt[..pkt.len() - 1]), None);

        // 0x5f: VarInt vehicle + VarInt count + VarInt passenger ids.
        let mut reg = MountRegistry::default();
        reg.register_vehicle(77, VehicleKind::Boat, [1.0, 62.0, 1.0]);
        reg.register_rider(10, [1.0, 62.0, 1.0]);
        reg.register_rider(11, [1.0, 62.0, 1.0]);
        reg.mount(77, 10).unwrap();
        reg.mount(77, 11).unwrap();
        let body = reg.encode_set_passengers(77);
        let mut expect = Vec::new();
        write_varint(&mut expect, 77);
        write_varint(&mut expect, 2);
        write_varint(&mut expect, 10);
        write_varint(&mut expect, 11);
        assert_eq!(body, expect);
        assert_eq!(decode_set_passengers(&body), Some((77, vec![10, 11])));
        let mut tail = body.clone();
        tail.push(0);
        assert_eq!(decode_set_passengers(&tail), None);
    }

    #[test]
    fn mount_registry_enforces_capacity_prevents_cycles_and_decodes_vehicle_packets() {
        // Capacity: boat seats 2, minecart seats 1.
        let mut reg = MountRegistry::default();
        reg.register_vehicle(1, VehicleKind::Boat, [0.0, 62.0, 0.0]);
        for rider in 10..=12 {
            reg.register_rider(rider, [0.0, 62.0, 0.0]);
        }
        assert_eq!(reg.mount(1, 10), Ok(()));
        assert_eq!(reg.mount(1, 11), Ok(()));
        assert_eq!(reg.mount(1, 12), Err(MountError::Full));
        let mut carts = MountRegistry::default();
        carts.register_vehicle(2, VehicleKind::Minecart, [5.0, 10.0, 5.0]);
        assert_eq!(carts.mount(2, 10), Ok(()));
        assert_eq!(carts.mount(2, 11), Err(MountError::Full));
        // Capacity table: boat/chest-boat/camel take two riders, the rest one.
        for (kind, seats) in [
            (VehicleKind::Boat, 2),
            (VehicleKind::ChestBoat, 2),
            (VehicleKind::Minecart, 1),
            (VehicleKind::Horse, 1),
            (VehicleKind::Donkey, 1),
            (VehicleKind::Mule, 1),
            (VehicleKind::Pig, 1),
            (VehicleKind::Strider, 1),
            (VehicleKind::Camel, 2),
            (VehicleKind::Llama, 1),
        ] {
            assert_eq!(kind.max_passengers(), seats);
            let _ = kind.seat_offset(0);
            let _ = kind.dismount_offset();
            let _ = kind.max_step(&VehicleEnv::default());
        }
        // Self-mount and riding-while-riding are rejected.
        let mut chain = MountRegistry::default();
        chain.register_vehicle(1, VehicleKind::Boat, [0.0, 62.0, 0.0]);
        chain.register_vehicle(3, VehicleKind::Horse, [0.0, 62.0, 0.0]);
        chain.register_vehicle(4, VehicleKind::Minecart, [0.0, 62.0, 0.0]);
        assert_eq!(chain.mount(1, 1), Err(MountError::SelfMount));
        assert_eq!(chain.mount(99, 10), Err(MountError::UnknownVehicle));
        assert_eq!(chain.mount(1, 3), Ok(())); // horse can ride the boat
        assert_eq!(chain.mount(3, 1), Err(MountError::Cycle)); // boat can't ride its own vehicle
        assert_eq!(chain.mount(4, 3), Err(MountError::Cycle)); // 3 already rides 1

        // C2S 0x1e MoveVehicle: f64 xyz + f32 yaw/pitch, finite only.
        let mut raw = Vec::new();
        raw.extend_from_slice(&1.5f64.to_be_bytes());
        raw.extend_from_slice(&64.25f64.to_be_bytes());
        raw.extend_from_slice(&(-3.0f64).to_be_bytes());
        raw.extend_from_slice(&90.0f32.to_be_bytes());
        raw.extend_from_slice(&(-15.5f32).to_be_bytes());
        let mv = decode_move_vehicle(&raw).unwrap();
        assert_eq!(
            mv,
            MoveVehiclePacket { x: 1.5, y: 64.25, z: -3.0, yaw: 90.0, pitch: -15.5 }
        );
        let mut nan = raw.clone();
        nan[..8].copy_from_slice(&f64::NAN.to_be_bytes());
        assert_eq!(decode_move_vehicle(&nan), None);
        assert_eq!(decode_move_vehicle(&raw[..31]), None);
        // C2S 0x1f PaddleBoat / 0x26 PlayerInput.
        let paddle = decode_paddle_boat(&[1, 0]).unwrap();
        assert!(paddle.left && !paddle.right);
        assert_eq!(decode_paddle_boat(&[1]), None);
        let mut input = Vec::new();
        input.extend_from_slice(&(-0.98f32).to_be_bytes());
        input.extend_from_slice(&1.0f32.to_be_bytes());
        input.push(0x03);
        let inp = decode_player_input(&input).unwrap();
        assert_eq!(inp.sideways, -0.98);
        assert_eq!(inp.forward, 1.0);
        assert!(inp.jump && inp.unmount);
        assert_eq!(decode_player_input(&input[..8]), None);

        // Vehicle step validates speed, reseats passengers, returns carry delta.
        let mut ride = MountRegistry::default();
        ride.register_vehicle(1, VehicleKind::Boat, [0.0, 62.0, 0.0]);
        ride.register_rider(10, [0.0, 62.0, 0.0]);
        ride.mount(1, 10).unwrap();
        let water = VehicleEnv { on_water: true, ..Default::default() };
        let step = ride
            .step_vehicle_move(
                1,
                &MoveVehiclePacket { x: 0.3, y: 62.0, z: 0.0, yaw: 0.0, pitch: 0.0 },
                &water,
            )
            .unwrap();
        assert_eq!(step.carry_delta, (0.3, 0.0, 0.0));
        assert_eq!(step.passengers_moved, 1);
        assert_eq!(step.pos, [0.3, 62.0, 0.0]);
        // Rider reseated at the boat seat offset (y + 0.35).
        assert_eq!(ride.positions[&10], [0.3, 62.35, 0.0]);
        // A too-fast claim is rejected and nothing moves.
        let far = MoveVehiclePacket { x: 3.0, y: 62.0, z: 0.0, yaw: 0.0, pitch: 0.0 };
        assert_eq!(ride.step_vehicle_move(1, &far, &water), Err(VehicleReject::TooQuickly));
        assert_eq!(ride.positions[&1], [0.3, 62.0, 0.0]);
        // Attribute-scaled mount: a 0.2-speed horse allows ~0.48 b/t.
        let mut stable = MountRegistry::default();
        stable.register_vehicle(5, VehicleKind::Horse, [0.0, 64.0, 0.0]);
        let land = VehicleEnv { move_speed: 0.2, ..Default::default() };
        let hop = MoveVehiclePacket { x: 0.4, y: 64.0, z: 0.0, yaw: 0.0, pitch: 0.0 };
        assert_eq!(
            stable.step_vehicle_move(5, &hop, &land).unwrap().carry_delta,
            (0.4, 0.0, 0.0)
        );
        let gallop = MoveVehiclePacket { x: 1.0, y: 64.0, z: 0.0, yaw: 0.0, pitch: 0.0 };
        assert_eq!(
            stable.step_vehicle_move(5, &gallop, &land),
            Err(VehicleReject::TooQuickly)
        );
        // Dismount returns the safe offset position and frees the seat.
        let at = ride.dismount(10).unwrap();
        assert_eq!(at, [1.0, 62.0, 0.0]);
        assert_eq!(ride.positions[&10], at);
        assert!(ride.passengers_of(1).is_empty());
    }

    #[test]
    fn default_mob_equipment_assigns_vanilla_weapons() {
        let bow = item("bow");
        assert_eq!(
            *default_mob_equipment("skeleton", 3, 0).get(EquipmentSlot::MainHand),
            (bow, 1, Vec::new())
        );
        assert_eq!(
            default_mob_equipment("wither_skeleton", 3, 0).get(EquipmentSlot::MainHand).0,
            item("stone_sword")
        );
        assert_eq!(
            default_mob_equipment("pillager", 3, 0).get(EquipmentSlot::MainHand).0,
            item("crossbow")
        );
        assert_eq!(
            default_mob_equipment("zombified_piglin", 3, 0).get(EquipmentSlot::MainHand).0,
            item("golden_sword")
        );
        // Piglin: bit 8 of the roll picks golden sword vs crossbow.
        assert_eq!(
            default_mob_equipment("piglin", 3, 0).get(EquipmentSlot::MainHand).0,
            item("golden_sword")
        );
        assert_eq!(
            default_mob_equipment("piglin", 3, 1 << 8).get(EquipmentSlot::MainHand).0,
            item("crossbow")
        );
        // Drowned: low roll percentile yields trident, next band a fishing rod.
        assert_eq!(
            default_mob_equipment("drowned", 3, 0).get(EquipmentSlot::MainHand).0,
            item("trident")
        );
        assert_eq!(
            default_mob_equipment("drowned", 3, 7).get(EquipmentSlot::MainHand).0,
            item("fishing_rod")
        );
        assert_eq!(
            default_mob_equipment("drowned", 3, 50).get(EquipmentSlot::MainHand).1,
            0
        );
    }

    #[test]
    fn p4b_parity_boat_friction_surfaces_and_terminal_speeds() {
        // cov.I invFriction: water/flowing/air share 0.9, submerged is 0.45,
        // land reads the support block friction.
        for status in [
            WmBoatStatus::InWater,
            WmBoatStatus::UnderFlowingWater,
            WmBoatStatus::InAir,
        ] {
            assert_eq!(wm_boat_friction(status, 0.6), 0.9);
        }
        assert_eq!(wm_boat_friction(WmBoatStatus::UnderWater, 0.6), 0.45);
        assert_eq!(wm_boat_friction(WmBoatStatus::OnLand, 0.6), 0.6);
        // Support-block friction table: water < land < ice < blue ice speed.
        assert_eq!(wm_support_friction("ice"), 0.98);
        assert_eq!(wm_support_friction("packed_ice"), 0.98);
        assert_eq!(wm_support_friction("frosted_ice"), 0.98);
        assert_eq!(wm_support_friction("blue_ice"), 0.989);
        assert_eq!(wm_support_friction("slime_block"), 0.8);
        assert_eq!(wm_support_friction("honey_block"), 0.4);
        assert_eq!(wm_support_friction("soul_sand"), 0.4);
        assert_eq!(wm_support_friction("stone"), 0.6);
        // Paddled terminal = 0.04 / (1 - f): 0.4 water, 0.1 land, 2.0 ice.
        assert!((wm_boat_terminal_speed(0.9) - 0.4).abs() < 1e-9);
        assert!((wm_boat_terminal_speed(0.6) - 0.1).abs() < 1e-9);
        assert!((wm_boat_terminal_speed(0.98) - 2.0).abs() < 1e-9);
        assert!((wm_boat_terminal_speed(0.989) - 0.04 / 0.011).abs() < 1e-9);
        assert!(
            wm_boat_terminal_speed(0.989) > wm_boat_terminal_speed(0.98),
            "blue ice slides faster than ice"
        );
        // floatBoat step: horizontal damped, gravity sinks, buoyancy lifts.
        let (v, rot) = wm_boat_float_tick(
            WmBoatStatus::InWater,
            0.6,
            WM_BOAT_GRAVITY,
            [1.0, 0.0, 0.0],
            0.0,
            2.0,
        );
        assert!((v[0] - 0.9).abs() < 1e-12 && (v[1] + 0.04).abs() < 1e-12);
        assert!((rot - 1.8).abs() < 1e-12, "deltaRotation scales by friction");
        let (sub, _) = wm_boat_float_tick(
            WmBoatStatus::UnderWater,
            0.6,
            WM_BOAT_GRAVITY,
            [1.0, 0.0, 0.0],
            wm_boat_lift(WmBoatStatus::UnderWater, 0.0),
            0.0,
        );
        assert!((sub[0] - 0.45).abs() < 1e-12);
        assert!(
            (sub[1] - ((0.01 * (0.04 / 0.65) - 0.04) * 0.75)).abs() < 1e-12,
            "submerged lift = 0.01 then the whole y term * 0.75"
        );
        let (land, _) = wm_boat_float_tick(
            WmBoatStatus::OnLand,
            0.989,
            WM_BOAT_GRAVITY,
            [1.0, 0.0, 0.0],
            0.0,
            0.0,
        );
        assert!((land[0] - 0.989).abs() < 1e-12);
        assert_eq!(wm_boat_lift(WmBoatStatus::InWater, 0.3), 0.3);
        assert_eq!(wm_boat_lift(WmBoatStatus::OnLand, 0.3), 0.0);
        // Anti-cheat gate: water terminal * 1.25 slack; ice supports beat water.
        let water = VehicleEnv {
            on_water: true,
            ..Default::default()
        };
        assert!((VehicleKind::Boat.max_step(&water) - 0.5).abs() < 1e-9);
        let ice = VehicleEnv {
            on_ice: true,
            ..Default::default()
        };
        assert!(VehicleKind::Boat.max_step(&ice) > VehicleKind::Boat.max_step(&water));
    }

    #[test]
    fn p4b_parity_boat_single_paddle_yaw_and_thrust() {
        // One right paddle: +1 degree/tick and a gentle 0.005 drift.
        let (yaw, rot, v) = wm_boat_control_tick(
            WmBoatInput {
                right: true,
                ..Default::default()
            },
            0.0,
            0.0,
            [0.0; 3],
        );
        assert_eq!(yaw, 1.0);
        assert_eq!(rot, 1.0);
        let rad = 1.0f64.to_radians();
        assert!((v[0] - -rad.sin() * 0.005).abs() < 1e-12);
        assert!((v[2] - rad.cos() * 0.005).abs() < 1e-12);
        // Left paddle turns the other way.
        let (yaw, _, _) = wm_boat_control_tick(
            WmBoatInput {
                left: true,
                ..Default::default()
            },
            0.0,
            0.0,
            [0.0; 3],
        );
        assert_eq!(yaw, -1.0);
        // Two paddles cancel the turn and add no drift.
        let (yaw, _, v) = wm_boat_control_tick(
            WmBoatInput {
                left: true,
                right: true,
                ..Default::default()
            },
            0.0,
            0.0,
            [0.0; 3],
        );
        assert_eq!(yaw, 0.0);
        assert_eq!(v, [0.0; 3]);
        // Rowing forward gives the full 0.04 thrust along the new yaw; the
        // turn drift does not stack while `up` is held.
        let (_, _, v) = wm_boat_control_tick(
            WmBoatInput {
                left: true,
                up: true,
                ..Default::default()
            },
            0.0,
            0.0,
            [0.0; 3],
        );
        let rad = (-1.0f64).to_radians();
        assert!((v[0] - -rad.sin() * 0.04).abs() < 1e-12);
        assert!((v[2] - rad.cos() * 0.04).abs() < 1e-12);
        // Backpaddle pushes backward at 0.005.
        let (_, _, v) = wm_boat_control_tick(
            WmBoatInput {
                down: true,
                ..Default::default()
            },
            0.0,
            0.0,
            [0.0; 3],
        );
        assert!((v[2] + 0.005).abs() < 1e-12);
    }

    #[test]
    fn p4b_parity_minecart_powered_rail_cap_and_acceleration() {
        // 8 m/s cap dry, 4 m/s submerged (cot.p).
        assert_eq!(wm_minecart_max_speed(false), 0.4);
        assert_eq!(wm_minecart_max_speed(true), 0.2);
        // Powered rail adds 0.06 along the current direction when moving.
        let (x, z) = wm_minecart_powered_boost(0.3, 0.4, WmRailShape::EastWest, false, false);
        assert!((x - (0.3 + 0.3 / 0.5 * 0.06)).abs() < 1e-12);
        assert!((z - (0.4 + 0.4 / 0.5 * 0.06)).abs() < 1e-12);
        // A parked cart is kicked 0.02 away from a conductor along the axis.
        assert_eq!(
            wm_minecart_powered_boost(0.0, 0.0, WmRailShape::EastWest, true, false),
            (0.02, 0.0)
        );
        assert_eq!(
            wm_minecart_powered_boost(0.0, 0.0, WmRailShape::EastWest, false, true),
            (-0.02, 0.0)
        );
        assert_eq!(
            wm_minecart_powered_boost(0.0, 0.0, WmRailShape::NorthSouth, true, false),
            (0.0, 0.02)
        );
        assert_eq!(
            wm_minecart_powered_boost(0.0, 0.0, WmRailShape::NorthSouth, false, true),
            (0.0, -0.02)
        );
        // No conductor and no non-straight shape => no kick.
        assert_eq!(
            wm_minecart_powered_boost(0.0, 0.0, WmRailShape::EastWest, false, false),
            (0.0, 0.0)
        );
        assert_eq!(
            wm_minecart_powered_boost(0.0, 0.0, WmRailShape::Other, true, true),
            (0.0, 0.0)
        );
        // The rail move clamps every axis to the 8 m/s cap; a passenger
        // scales velocity by 0.75 before the clamp.
        assert_eq!(wm_minecart_rail_move(0.9, -0.5, false, false), (0.4, -0.4));
        let (rx, rz) = wm_minecart_rail_move(0.4, 0.0, true, false);
        assert!((rx - 0.3).abs() < 1e-12 && rz == 0.0);
        // The mount gate on rails is the exact cap.
        let rail = VehicleEnv {
            on_rail: true,
            ..Default::default()
        };
        assert_eq!(VehicleKind::Minecart.max_step(&rail), 0.4);
        // applyNaturalSlowdown: 0.96 free/0.997 ridden, y zeroed, +0.95 in water.
        let damped = wm_minecart_slowdown(false, false, [0.4, 0.5, 0.4]);
        assert!((damped[0] - 0.4 * 0.96).abs() < 1e-12 && damped[1] == 0.0);
        let wet = wm_minecart_slowdown(true, true, [0.4, 0.5, 0.4]);
        assert!((wet[0] - 0.4 * 0.997 * 0.95).abs() < 1e-12);
        // Ascending-rail boost constant, 0.2x under water.
        assert_eq!(wm_minecart_slope_boost(false), 0.0078125);
        assert!((wm_minecart_slope_boost(true) - 0.0078125 * 0.2).abs() < 1e-15);
    }

    #[test]
    fn p4b_parity_minecart_unpowered_brake_and_detector_signal() {
        // Unpowered powered-rail: *0.5 above 0.03, dead stop below it.
        assert_eq!(wm_minecart_rail_brake(0.4, 0.0), (0.2, 0.0));
        assert_eq!(wm_minecart_rail_brake(0.03, 0.0), (0.015, 0.0));
        assert_eq!(wm_minecart_rail_brake(0.029, 0.0), (0.0, 0.0));
        // Detector rail emits 15 only while a cart occupies it.
        assert_eq!(wm_detector_rail_signal(true), 15);
        assert_eq!(wm_detector_rail_signal(false), 0);
    }

    #[test]
    fn p4b_parity_horse_jump_charge_curve() {
        // handleStartJump: clamp to 0, <90 -> 0.4..0.8, >=90 -> 1.0.
        assert_eq!(wm_horse_jump_scale(-10), 0.4);
        assert_eq!(wm_horse_jump_scale(0), 0.4);
        assert!((wm_horse_jump_scale(45) - 0.6).abs() < 1e-12);
        assert!((wm_horse_jump_scale(89) - (0.4 + 0.4 * 89.0 / 90.0)).abs() < 1e-12);
        assert_eq!(wm_horse_jump_scale(90), 1.0);
        assert_eq!(wm_horse_jump_scale(100), 1.0);
        // executeRidersJump: strength * scale * blockFactor + jump boost.
        assert!((wm_horse_jump_velocity(0.7, 0.6, 1.0, 0.0) - 0.42).abs() < 1e-12);
        assert!((wm_horse_jump_velocity(0.7, 1.0, 1.0, 0.2) - 0.9).abs() < 1e-12);
        assert_eq!(wm_jump_boost_power(None), 0.0);
        assert!((wm_jump_boost_power(Some(1)) - 0.2).abs() < 1e-12);
        // Forward kick (-0.4 sin * s, 0, 0.4 cos * s): yaw 90 -> -0.4*s on x.
        let (hx, hz) = wm_horse_jump_horizontal(90.0, 0.5);
        assert!((hx + 0.2).abs() < 1e-12 && hz.abs() < 1e-9);
        // Spawn rolls: jump 0.4 + 0.2*3r, speed (0.45 + 0.3*3r) * 0.25.
        assert!((wm_horse_spawn_jump_strength([0.0; 3]) - 0.4).abs() < 1e-12);
        assert!((wm_horse_spawn_jump_strength([1.0; 3]) - 1.0).abs() < 1e-12);
        assert!((wm_horse_spawn_movement_speed([0.0; 3]) - 0.1125).abs() < 1e-12);
        assert!((wm_horse_spawn_movement_speed([1.0; 3]) - 0.3375).abs() < 1e-9);
    }

    #[test]
    fn p4b_parity_pig_strider_boost_and_lava_temperature() {
        // ItemBasedSteering: total 140..=980 ticks, 1 + 1.15*sin(pi*t/total).
        assert_eq!(wm_boost_total_ticks(0), 140);
        assert_eq!(wm_boost_total_ticks(840), 980);
        assert_eq!(wm_boost_multiplier(0, 0), 0.0);
        assert!((wm_boost_multiplier(0, 980) - 1.0).abs() < 1e-12);
        assert!((wm_boost_multiplier(490, 980) - (1.0 + 1.15)).abs() < 1e-12);
        assert!((wm_boost_multiplier(980, 980) - 1.0).abs() < 1e-9);
        // Pig: MOVEMENT_SPEED * 0.225 * boost; 0.25 base -> 0.05625.
        assert!((wm_pig_ride_speed(0.25, 1.0) - 0.05625).abs() < 1e-12);
        assert!((wm_pig_ride_speed(0.25, WM_BOOST_MAX) - 0.05625 * 2.15).abs() < 1e-9);
        // Strider: cold 0.35 off lava, warm 0.55 on lava (0.175 base attr).
        assert!((wm_strider_ride_speed(0.175, true, 1.0) - 0.06125).abs() < 1e-12);
        assert!((wm_strider_ride_speed(0.175, false, 1.0) - 0.09625).abs() < 1e-12);
        assert!(
            wm_strider_ride_speed(0.175, false, 1.0)
                > wm_strider_ride_speed(0.175, true, 1.0),
            "warm lava strider outruns a shivering one"
        );
        // Lava buoyancy halves velocity then adds 0.05 upward.
        assert_eq!(wm_strider_lava_buoyancy([0.4, -0.2, 0.4]), [0.2, -0.05, 0.2]);
        // The mount gates source the boosted vanilla speeds.
        let pig = VehicleEnv {
            move_speed: 0.25,
            ..Default::default()
        };
        assert!((VehicleKind::Pig.max_step(&pig) - 0.05625 * 2.15).abs() < 1e-9);
        let warm = VehicleEnv {
            on_lava: true,
            move_speed: 0.175,
            ..Default::default()
        };
        let cold = VehicleEnv {
            move_speed: 0.175,
            ..Default::default()
        };
        assert!(VehicleKind::Strider.max_step(&warm) > VehicleKind::Strider.max_step(&cold));
    }
}
