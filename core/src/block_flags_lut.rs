// SPDX-License-Identifier: LGPL-3.0-or-later
//! `megu_core::block_flags_lut` — hot-loop lookup structures for block-state
//! metadata and per-section sparse block predicates.
//!
//! Two primitives share one file because they both exist to keep the
//! per-tick block-scan loops branch-light and allocation-free:
//!
//! * [`BlockFlagsLut`] — one `u64` flag word per block *state*, indexed
//!   directly by `state_id: u32`. A hot loop that used to compare
//!   `reg.name_of(state) == "minecraft:fire"` (or `"minecraft:end_stone"`,
//!   `"minecraft:lava"`, …) now runs the single-instruction test
//!   `(flags & MASK) != 0`. Each word carries 18 boolean flags plus two
//!   packed 4-bit fields (emission and light-opacity levels, `0..=15`).
//! * [`SectionBitTree4096`] — a hierarchical bit-tree tracking any sparse
//!   predicate (random-tickable, light emitters, fluids, block entities)
//!   over the 4 096 blocks of a `16³` section in 520 bytes: 64 leaf `u64`
//!   words plus one `u64` summary word. `for_each_set` visits only set
//!   indices in ascending order and skips a fully air/stone section in O(1).
#![forbid(unsafe_code)]

// ---------------------------------------------------------------------------
// Block flag word layout
// ---------------------------------------------------------------------------

/// Bit 0 — the state is air-like (`air`, `cave_air`, `void_air`).
pub const FLAG_AIR: u64 = 1 << 0;
/// Bit 1 — the state has solid collision geometry.
pub const FLAG_SOLID: u64 = 1 << 1;
/// Bit 2 — the state's shape occupies the full `1³` cube.
pub const FLAG_FULL_CUBE: u64 = 1 << 2;
/// Bit 3 — the state is solid-render and fully blocks light (opacity 15).
pub const FLAG_OPAQUE: u64 = 1 << 3;
/// Bit 4 — the state does not fully block light (opacity < 15); inverse of
/// [`FLAG_OPAQUE`]. Air, plants, glass, leaves, fluids, partial shapes.
pub const FLAG_TRANSPARENT: u64 = 1 << 4;
/// Bit 5 — the state is a fluid (water or lava family).
pub const FLAG_FLUID: u64 = 1 << 5;
/// Bit 6 — the state belongs to the water family.
pub const FLAG_WATER: u64 = 1 << 6;
/// Bit 7 — the state belongs to the lava family.
pub const FLAG_LAVA: u64 = 1 << 7;
/// Bit 8 — the state is a fire block (`fire`, `soul_fire`).
pub const FLAG_FIRE: u64 = 1 << 8;
/// Bit 9 — the state is in the vanilla `#minecraft:climbable` tag.
pub const FLAG_CLIMBABLE: u64 = 1 << 9;
/// Bit 10 — blocks may be placed over this state without breaking it
/// (vanilla `replaceable` material flag: fluids, fire, grass, snow, …).
pub const FLAG_REPLACEABLE: u64 = 1 << 10;
/// Bit 11 — the state receives random ticks (crops, fire, leaves, ice,
/// spreading soil/nylium, oxidizing copper, …).
pub const FLAG_RANDOM_TICKABLE: u64 = 1 << 11;
/// Bit 12 — the state can emit light (level in the packed emission nibble;
/// conditional emitters such as `furnace[lit=false]` carry the flag with a
/// level of 0).
pub const FLAG_LIGHT_EMITTER: u64 = 1 << 12;
/// Bit 13 — the state owns a block entity.
pub const FLAG_BLOCK_ENTITY: u64 = 1 << 13;
/// Bit 14 — redstone dust aims at / powers this state.
pub const FLAG_REDSTONE_CONNECT: u64 = 1 << 14;
/// Bit 15 — contact or exposure damages entities (lava, fire, magma,
/// cactus, campfire, powder snow, berry bush, wither rose, dripstone).
pub const FLAG_HAZARD: u64 = 1 << 15;
/// Bit 16 — the state is exactly `minecraft:end_stone`.
pub const FLAG_END_STONE: u64 = 1 << 16;
/// Bit 17 — the state obeys gravity when unsupported (sand, gravel,
/// concrete powder, anvils, dragon egg, scaffolding, pointed dripstone).
pub const FLAG_GRAVITY_AFFECTED: u64 = 1 << 17;

/// Mask covering every boolean flag bit; the two packed nibbles live above.
pub const FLAG_BITS_MASK: u64 = (1 << EMISSION_SHIFT) - 1;

/// Bit position of the packed 4-bit light-emission level (`0..=15`).
pub const EMISSION_SHIFT: u32 = 48;
/// Bit position of the packed 4-bit light-opacity level (`0..=15`).
pub const OPACITY_SHIFT: u32 = 52;
/// Mask selecting the packed emission nibble inside a flag word.
pub const EMISSION_MASK: u64 = 0xF << EMISSION_SHIFT;
/// Mask selecting the packed opacity nibble inside a flag word.
pub const OPACITY_MASK: u64 = 0xF << OPACITY_SHIFT;

/// Pack boolean `flags` plus `emission`/`opacity` levels into one word.
/// Levels are clamped to `0..=15`; stray bits outside [`FLAG_BITS_MASK`]
/// and the two nibbles are dropped.
pub const fn pack_word(flags: u64, emission: u8, opacity: u8) -> u64 {
    let e = if emission > 15 { 15 } else { emission } as u64;
    let o = if opacity > 15 { 15 } else { opacity } as u64;
    (flags & FLAG_BITS_MASK) | (e << EMISSION_SHIFT) | (o << OPACITY_SHIFT)
}

/// Extract the packed emission level `0..=15` from a flag word.
pub const fn word_emission(word: u64) -> u8 {
    ((word & EMISSION_MASK) >> EMISSION_SHIFT) as u8
}

/// Extract the packed light-opacity level `0..=15` from a flag word.
pub const fn word_opacity(word: u64) -> u8 {
    ((word & OPACITY_MASK) >> OPACITY_SHIFT) as u8
}

// ---------------------------------------------------------------------------
// BlockFlagsLut
// ---------------------------------------------------------------------------

/// Direct-indexed `state_id → u64` flag table built once from the registry's
/// names so hot loops never compare strings again.
///
/// [`BlockFlagsLut::flags`] is fail-closed: an out-of-range `state_id`
/// returns `0` (no flags, no emission, no opacity) instead of panicking.
#[derive(Debug, Clone, Default)]
pub struct BlockFlagsLut {
    words: Vec<u64>,
}

impl BlockFlagsLut {
    /// Build a LUT by classifying every registry name in order; the nth
    /// name becomes the flag word for `state_id == n`.
    pub fn from_names<'a, I>(names: I) -> Self
    where
        I: IntoIterator<Item = &'a str>,
    {
        Self {
            words: names.into_iter().map(classify_name).collect(),
        }
    }

    /// Build a LUT from pre-computed flag words (custom classification or
    /// deserialized metadata).
    pub fn from_words(words: Vec<u64>) -> Self {
        Self { words }
    }

    /// The flag word for `state_id`, or `0` when out of range.
    #[inline]
    pub fn flags(&self, state_id: u32) -> u64 {
        self.words.get(state_id as usize).copied().unwrap_or(0)
    }

    /// The single-instruction hot check: `(flags & mask) != 0`.
    #[inline]
    pub fn has(&self, state_id: u32, mask: u64) -> bool {
        self.flags(state_id) & mask != 0
    }

    /// Packed emission level `0..=15` for `state_id` (`0` out of range).
    #[inline]
    pub fn emission(&self, state_id: u32) -> u8 {
        word_emission(self.flags(state_id))
    }

    /// Packed light-opacity level `0..=15` for `state_id` (`0` out of range).
    #[inline]
    pub fn opacity(&self, state_id: u32) -> u8 {
        word_opacity(self.flags(state_id))
    }

    /// Number of indexed states.
    pub fn len(&self) -> usize {
        self.words.len()
    }

    /// Whether the table is empty.
    pub fn is_empty(&self) -> bool {
        self.words.is_empty()
    }
}

// ---------------------------------------------------------------------------
// Name classification
// ---------------------------------------------------------------------------

/// Classify one block-state name into a flag word.
///
/// Accepts `namespace:path`, optionally with `[property=value,…]` state
/// properties (`lit`, `charges`, `pickles`, `waterlogged`, `level` are
/// honoured for emission). Vanilla `minecraft:` semantics are assumed; the
/// namespace is ignored, so modded blocks named `mymod:soul_lantern` still
/// classify sensibly.
///
/// This is a **heuristic** for the common vanilla shapes: unrecognized
/// names default to `SOLID | FULL_CUBE | OPAQUE`, the modal vanilla block.
/// Where exact parity matters, prefer [`BlockFlagsLut::from_words`] fed by
/// generated registry data.
pub fn classify_name(name: &str) -> u64 {
    let path = name.rsplit(':').next().unwrap_or(name);
    let (base, props) = match path.split_once('[') {
        Some((b, p)) => (b, p),
        None => (path, ""),
    };

    // Air and fluids early-return: none of the general predicates apply.
    match base {
        "air" | "cave_air" | "void_air" => {
            return pack_word(FLAG_AIR | FLAG_TRANSPARENT | FLAG_REPLACEABLE, 0, 0);
        }
        "water" | "flowing_water" | "bubble_column" => {
            return pack_word(
                FLAG_FLUID | FLAG_WATER | FLAG_TRANSPARENT | FLAG_REPLACEABLE,
                0,
                1,
            );
        }
        "lava" | "flowing_lava" => {
            return pack_word(
                FLAG_FLUID
                    | FLAG_LAVA
                    | FLAG_TRANSPARENT
                    | FLAG_REPLACEABLE
                    | FLAG_HAZARD
                    | FLAG_LIGHT_EMITTER,
                15,
                1,
            );
        }
        _ => {}
    }
    classify_block(base, props)
}

/// Read `key=value` out of a `key=value,…]` state-property list.
fn prop_value<'a>(props: &'a str, key: &str) -> Option<&'a str> {
    props
        .trim_end_matches(']')
        .split(',')
        .filter_map(|pair| pair.split_once('='))
        .find(|(k, _)| k.trim() == key)
        .map(|(_, v)| v.trim())
}

/// Read a `u8` state property.
fn prop_u8(props: &str, key: &str) -> Option<u8> {
    prop_value(props, key)?.parse().ok()
}

fn classify_block(base: &str, props: &str) -> u64 {
    let mut flags: u64 = 0;

    // Geometry: solid collision + full-cube shape.
    let solid = !is_nonsolid(base);
    let full_cube = solid && !is_nonfull_shape(base);
    if solid {
        flags |= FLAG_SOLID;
    }
    if full_cube {
        flags |= FLAG_FULL_CUBE;
    }

    // Light: opacity mirrors vanilla `getLightBlock` — 15 for solid-render
    // cubes, 1 for the propagates-skylight overrides (leaves, ice, slime,
    // honey), 0 for clear transparents and every non-full shape.
    let opacity = match opacity_override(base) {
        Some(o) => o,
        None => {
            if full_cube {
                15
            } else {
                0
            }
        }
    };
    flags |= if opacity == 15 {
        FLAG_OPAQUE
    } else {
        FLAG_TRANSPARENT
    };

    let emission = emission_level(base, props);
    if emission > 0 || is_conditional_emitter(base) {
        flags |= FLAG_LIGHT_EMITTER;
    }

    if base == "end_stone" {
        flags |= FLAG_END_STONE;
    }
    if base == "fire" || base == "soul_fire" {
        flags |= FLAG_FIRE;
    }
    if is_replaceable(base) {
        flags |= FLAG_REPLACEABLE;
    }
    if is_climbable(base) {
        flags |= FLAG_CLIMBABLE;
    }
    if is_random_tickable(base) {
        flags |= FLAG_RANDOM_TICKABLE;
    }
    if is_hazard(base) {
        flags |= FLAG_HAZARD;
    }
    if is_gravity_affected(base) {
        flags |= FLAG_GRAVITY_AFFECTED;
    }
    if is_block_entity(base) {
        flags |= FLAG_BLOCK_ENTITY;
    }
    if is_redstone_connect(base) {
        flags |= FLAG_REDSTONE_CONNECT;
    }

    pack_word(flags, emission, opacity)
}

/// Per-name emission levels, honouring `lit`, `charges`, `pickles`,
/// `waterlogged` and `level` state properties where vanilla defines them.
fn emission_level(base: &str, props: &str) -> u8 {
    let lit = prop_value(props, "lit") == Some("true");
    match base {
        "fire"
        | "lava_cauldron"
        | "end_portal"
        | "end_gateway"
        | "glowstone"
        | "sea_lantern"
        | "shroomlight"
        | "jack_o_lantern"
        | "beacon"
        | "conduit"
        | "ochre_froglight"
        | "verdant_froglight"
        | "pearlescent_froglight" => 15,
        "lantern" => 15,
        "campfire" => {
            if prop_value(props, "lit") == Some("false") {
                0
            } else {
                15
            }
        }
        "torch" | "wall_torch" | "end_rod" => 14,
        "nether_portal" => 11,
        "soul_fire" | "soul_torch" | "soul_wall_torch" | "soul_lantern" | "crying_obsidian" => 10,
        "soul_campfire" => {
            if prop_value(props, "lit") == Some("false") {
                0
            } else {
                10
            }
        }
        "redstone_torch" | "redstone_wall_torch" | "ender_chest" | "glow_lichen" => 7,
        "sculk_catalyst" => 6,
        "amethyst_cluster" => 5,
        "large_amethyst_bud" => 4,
        "magma_block" => 3,
        "medium_amethyst_bud" => 2,
        "small_amethyst_bud" | "brewing_stand" | "dragon_egg" | "end_portal_frame"
        | "brown_mushroom" => 1,
        "light" => prop_u8(props, "level").unwrap_or(15).min(15),
        "furnace" | "smoker" | "blast_furnace" => {
            if lit {
                13
            } else {
                0
            }
        }
        "redstone_lamp" => {
            if lit {
                15
            } else {
                0
            }
        }
        "redstone_ore" | "deepslate_redstone_ore" => {
            if lit {
                9
            } else {
                0
            }
        }
        "sea_pickle" => {
            if prop_value(props, "waterlogged") == Some("true") {
                let pickles = u16::from(prop_u8(props, "pickles").unwrap_or(1));
                (3 * (pickles + 1)).min(15) as u8
            } else {
                0
            }
        }
        "respawn_anchor" => match prop_u8(props, "charges").unwrap_or(0) {
            1 => 3,
            2 => 7,
            3 => 11,
            4 => 15,
            _ => 0,
        },
        _ => {
            if base.ends_with("_candle") || base == "candle" {
                if lit {
                    3
                } else {
                    0
                }
            } else if base.ends_with("copper_bulb") {
                if lit {
                    15
                } else {
                    0
                }
            } else if base.ends_with("_torch") {
                // Modal fallback for torches not matched above (e.g.
                // copper_torch): soul/redstone torches are handled earlier.
                14
            } else {
                0
            }
        }
    }
}

/// Emitters whose level depends on a state property still carry
/// [`FLAG_LIGHT_EMITTER`] even when the property currently yields `0`.
fn is_conditional_emitter(base: &str) -> bool {
    matches!(
        base,
        "furnace"
            | "smoker"
            | "blast_furnace"
            | "redstone_lamp"
            | "redstone_ore"
            | "deepslate_redstone_ore"
            | "sea_pickle"
            | "respawn_anchor"
            | "light"
            | "campfire"
            | "soul_campfire"
    ) || base.ends_with("_candle")
        || base == "candle"
        || base.ends_with("copper_bulb")
}

/// Opacity overrides for blocks whose `lightBlock` differs from the
/// full-cube / non-full default (`propagatesSkylightDown == false` gives 1).
fn opacity_override(base: &str) -> Option<u8> {
    if base == "tinted_glass" {
        return Some(15);
    }
    if base.ends_with("_leaves")
        || matches!(
            base,
            "ice" | "frosted_ice" | "slime_block" | "honey_block" | "mangrove_roots"
        )
    {
        return Some(1);
    }
    if base.ends_with("_stained_glass")
        || matches!(base, "glass" | "barrier" | "monster_spawner" | "beacon")
    {
        return Some(0);
    }
    None
}

/// No collision geometry: plants, wires, webs, portals, and the climbable
/// vine family.
fn is_nonsolid(base: &str) -> bool {
    if matches!(
        base,
        "redstone_wire"
            | "tripwire"
            | "tripwire_hook"
            | "lever"
            | "cobweb"
            | "powder_snow"
            | "sugar_cane"
            | "nether_wart"
            | "sweet_berry_bush"
            | "cocoa"
            | "fire"
            | "soul_fire"
            | "nether_portal"
            | "end_portal"
            | "end_gateway"
            | "light"
            | "structure_void"
            | "moving_piston"
            | "frogspawn"
            | "spore_blossom"
            | "glow_lichen"
            | "sculk_vein"
            | "hanging_roots"
            | "vine"
            | "weeping_vines"
            | "weeping_vines_plant"
            | "twisting_vines"
            | "twisting_vines_plant"
            | "cave_vines"
            | "cave_vines_plant"
            | "short_grass"
            | "tall_grass"
            | "fern"
            | "large_fern"
            | "dead_bush"
            | "seagrass"
            | "tall_seagrass"
            | "kelp"
            | "kelp_plant"
            | "wheat"
            | "carrots"
            | "potatoes"
            | "beetroots"
            | "torchflower_crop"
            | "pitcher_crop"
            | "bamboo_sapling"
            | "mangrove_propagule"
            | "chorus_flower"
            | "chorus_plant"
            | "azalea"
            | "flowering_azalea"
            | "small_dripleaf"
            | "dandelion"
            | "poppy"
            | "allium"
            | "cornflower"
            | "wither_rose"
            | "sunflower"
            | "lilac"
            | "peony"
            | "rose_bush"
            | "torchflower"
            | "pitcher_plant"
            | "pink_petals"
            | "wildflowers"
            | "leaf_litter"
            | "brown_mushroom"
            | "red_mushroom"
            | "lily_of_the_valley"
            | "nether_sprouts"
            | "warped_roots"
            | "crimson_roots"
            | "warped_fungus"
            | "crimson_fungus"
            | "closed_eyeblossom"
            | "open_eyeblossom"
            | "resin_clump"
            | "grass"
    ) {
        return true;
    }
    if base == "mangrove_roots" || base == "muddy_mangrove_roots" || base == "mushroom_stem" {
        return false;
    }
    base.ends_with("_torch")
        || base.ends_with("_button")
        || base.ends_with("_pressure_plate")
        || base.ends_with("_sapling")
        || base.ends_with("_sign")
        || base.ends_with("_banner")
        || base.ends_with("_sprouts")
        || base.ends_with("_coral_fan")
        || base.ends_with("_wall_fan")
        || base.ends_with("_stem")
        || base.ends_with("_rail")
        || base.ends_with("_tulip")
        || base.ends_with("_orchid")
        || base.ends_with("_bluet")
        || base.ends_with("_daisy")
        || base.ends_with("_roots")
        || base.ends_with("_fungus")
        || base.ends_with("_coral")
        || base == "torch"
        || base.starts_with("potted_")
}

/// Solid but not a `1³` shape.
fn is_nonfull_shape(base: &str) -> bool {
    matches!(
        base,
        "farmland"
            | "dirt_path"
            | "soul_sand"
            | "mud"
            | "cactus"
            | "cake"
            | "snow"
            | "hopper"
            | "composter"
            | "grindstone"
            | "lectern"
            | "stonecutter"
            | "enchanting_table"
            | "end_portal_frame"
            | "brewing_stand"
            | "bell"
            | "chain"
            | "lantern"
            | "soul_lantern"
            | "conduit"
            | "end_rod"
            | "lightning_rod"
            | "flower_pot"
            | "ladder"
            | "scaffolding"
            | "bamboo"
            | "lily_pad"
            | "campfire"
            | "soul_campfire"
            | "piston_head"
            | "big_dripleaf"
            | "pointed_dripstone"
            | "turtle_egg"
            | "sniffer_egg"
            | "sea_pickle"
            | "amethyst_cluster"
            | "small_amethyst_bud"
            | "medium_amethyst_bud"
            | "large_amethyst_bud"
            | "daylight_detector"
            | "decorated_pot"
            | "heavy_core"
            | "vault"
            | "trial_spawner"
            | "dragon_egg"
            | "chest"
            | "trapped_chest"
            | "ender_chest"
            | "iron_bars"
            | "candle"
            | "repeater"
            | "comparator"
            | "sculk_sensor"
            | "calibrated_sculk_sensor"
    ) || base.ends_with("_slab")
        || base.ends_with("_stairs")
        || base.ends_with("_fence")
        || base.ends_with("_fence_gate")
        || base.ends_with("_wall")
        || base.ends_with("_pane")
        || base.ends_with("_trapdoor")
        || base.ends_with("_door")
        || base.ends_with("_bed")
        || base.ends_with("_carpet")
        || base.ends_with("_cauldron")
        || base.ends_with("_skull")
        || base.ends_with("_head")
        || base.ends_with("_candle")
        || base.ends_with("_anvil")
        || base.ends_with("_bars")
        || base.starts_with("potted_")
}

/// Vanilla `replaceable` material flag: placement swaps the block out.
fn is_replaceable(base: &str) -> bool {
    matches!(
        base,
        "fire"
            | "soul_fire"
            | "short_grass"
            | "grass"
            | "tall_grass"
            | "fern"
            | "large_fern"
            | "dead_bush"
            | "seagrass"
            | "tall_seagrass"
            | "nether_sprouts"
            | "snow"
            | "vine"
            | "glow_lichen"
            | "sculk_vein"
            | "hanging_roots"
            | "structure_void"
            | "warped_roots"
            | "crimson_roots"
    )
}

/// The vanilla `#minecraft:climbable` tag, verbatim.
fn is_climbable(base: &str) -> bool {
    matches!(
        base,
        "ladder"
            | "scaffolding"
            | "vine"
            | "weeping_vines"
            | "weeping_vines_plant"
            | "twisting_vines"
            | "twisting_vines_plant"
            | "cave_vines"
            | "cave_vines_plant"
    )
}

/// States vanilla feeds to `randomTick` — crops, spread, growth, decay,
/// oxidation, dripping and hatching.
fn is_random_tickable(base: &str) -> bool {
    if matches!(
        base,
        "wheat"
            | "carrots"
            | "potatoes"
            | "beetroots"
            | "melon_stem"
            | "pumpkin_stem"
            | "torchflower_crop"
            | "pitcher_crop"
            | "nether_wart"
            | "cocoa"
            | "sugar_cane"
            | "cactus"
            | "bamboo"
            | "bamboo_sapling"
            | "kelp"
            | "chorus_flower"
            | "sweet_berry_bush"
            | "cave_vines"
            | "weeping_vines"
            | "twisting_vines"
            | "vine"
            | "mangrove_propagule"
            | "brown_mushroom"
            | "red_mushroom"
            | "fire"
            | "ice"
            | "frosted_ice"
            | "grass_block"
            | "mycelium"
            | "crimson_nylium"
            | "warped_nylium"
            | "budding_amethyst"
            | "turtle_egg"
            | "sniffer_egg"
            | "frogspawn"
            | "pointed_dripstone"
            | "farmland"
            | "cauldron"
            | "water_cauldron"
            | "lava_cauldron"
            | "powder_snow_cauldron"
    ) || base.ends_with("_sapling")
        || base.ends_with("_leaves")
    {
        return true;
    }
    // Copper family weathers by random tick; waxed and ore/raw forms do not.
    if base.starts_with("waxed_") {
        return false;
    }
    matches!(
        base,
        "copper_block" | "exposed_copper" | "weathered_copper" | "oxidized_copper"
    ) || base.contains("cut_copper")
        || base.contains("chiseled_copper")
        || base.contains("copper_bulb")
        || base.contains("copper_door")
        || base.contains("copper_grate")
        || base.contains("copper_trapdoor")
}

/// Contact or exposure damages entities.
fn is_hazard(base: &str) -> bool {
    matches!(
        base,
        "fire"
            | "soul_fire"
            | "magma_block"
            | "cactus"
            | "campfire"
            | "soul_campfire"
            | "powder_snow"
            | "sweet_berry_bush"
            | "wither_rose"
            | "pointed_dripstone"
    )
}

/// Falls when the supporting block below is removed.
fn is_gravity_affected(base: &str) -> bool {
    matches!(
        base,
        "sand"
            | "red_sand"
            | "gravel"
            | "suspicious_sand"
            | "suspicious_gravel"
            | "anvil"
            | "chipped_anvil"
            | "damaged_anvil"
            | "dragon_egg"
            | "scaffolding"
            | "pointed_dripstone"
    ) || base.ends_with("_concrete_powder")
}

/// States that own a block entity.
fn is_block_entity(base: &str) -> bool {
    matches!(
        base,
        "barrel"
            | "furnace"
            | "blast_furnace"
            | "smoker"
            | "hopper"
            | "dropper"
            | "dispenser"
            | "beacon"
            | "conduit"
            | "enchanting_table"
            | "brewing_stand"
            | "lectern"
            | "bell"
            | "jukebox"
            | "monster_spawner"
            | "trial_spawner"
            | "vault"
            | "moving_piston"
            | "comparator"
            | "daylight_detector"
            | "bee_nest"
            | "beehive"
            | "flower_pot"
            | "structure_block"
            | "jigsaw_block"
            | "campfire"
            | "soul_campfire"
            | "suspicious_sand"
            | "suspicious_gravel"
            | "decorated_pot"
            | "end_portal"
            | "end_gateway"
            | "sculk_sensor"
            | "calibrated_sculk_sensor"
            | "sculk_shrieker"
            | "chiseled_bookshelf"
            | "crafter"
    ) || base.ends_with("chest")
        || base.ends_with("_shulker_box")
        || base.ends_with("_sign")
        || base.ends_with("_banner")
        || base.ends_with("_bed")
        || base.ends_with("_skull")
        || (base.ends_with("_head") && base != "piston_head")
        || base.ends_with("command_block")
        || base.starts_with("potted_")
}

/// Redstone dust points at / powers the state.
fn is_redstone_connect(base: &str) -> bool {
    matches!(
        base,
        "redstone_wire"
            | "redstone_torch"
            | "redstone_wall_torch"
            | "repeater"
            | "comparator"
            | "redstone_block"
            | "observer"
            | "piston"
            | "sticky_piston"
            | "lever"
            | "tripwire_hook"
            | "target"
            | "note_block"
            | "tnt"
            | "powered_rail"
            | "detector_rail"
            | "activator_rail"
            | "trapped_chest"
            | "daylight_detector"
            | "lectern"
            | "redstone_lamp"
            | "dropper"
            | "dispenser"
            | "hopper"
            | "lightning_rod"
            | "crafter"
            | "bell"
            | "sculk_sensor"
            | "calibrated_sculk_sensor"
    ) || base.ends_with("_button")
        || base.ends_with("_pressure_plate")
        || base.ends_with("command_block")
        || base.ends_with("copper_bulb")
        || base.ends_with("_door")
        || base.ends_with("_trapdoor")
        || base.ends_with("_fence_gate")
}

// ---------------------------------------------------------------------------
// SectionBitTree4096
// ---------------------------------------------------------------------------

/// Blocks per `16³` section.
pub const SECTION_VOLUME: usize = 4096;
/// Leaf `u64` words covering [`SECTION_VOLUME`].
pub const SECTION_WORDS: usize = SECTION_VOLUME / 64;

/// Hierarchical bit index over the 4 096 blocks of a `16³` section:
/// block index → leaf word (level 0/1) → summary word (level 2).
///
/// One summary bit per leaf word means [`SectionBitTree4096::is_empty`],
/// [`SectionBitTree4096::set`], [`SectionBitTree4096::clear`] and the
/// section-skip inside [`SectionBitTree4096::for_each_set`] are all O(1),
/// and iteration touches only words that actually contain set bits — an
/// all-air or all-stone predicate costs a single `summary == 0` test.
///
/// Total size is exactly 520 bytes (64 leaf words + 1 summary word); a test
/// pins that contract.
pub struct SectionBitTree4096 {
    leaves: [u64; SECTION_WORDS],
    summary: u64,
}

impl SectionBitTree4096 {
    /// An all-clear tree.
    pub const fn new() -> Self {
        Self {
            leaves: [0; SECTION_WORDS],
            summary: 0,
        }
    }

    /// Set bit `index` (`0..4096`); returns `true` when newly set.
    /// Out-of-range indices are a fail-closed no-op.
    #[inline]
    pub fn set(&mut self, index: usize) -> bool {
        if index >= SECTION_VOLUME {
            return false;
        }
        let word = index >> 6;
        let bit = 1u64 << (index & 63);
        if self.leaves[word] & bit != 0 {
            return false;
        }
        self.leaves[word] |= bit;
        self.summary |= 1u64 << word;
        true
    }

    /// Clear bit `index`; returns `true` when a set bit was cleared.
    /// Out-of-range indices are a fail-closed no-op.
    #[inline]
    pub fn clear(&mut self, index: usize) -> bool {
        if index >= SECTION_VOLUME {
            return false;
        }
        let word = index >> 6;
        let bit = 1u64 << (index & 63);
        if self.leaves[word] & bit == 0 {
            return false;
        }
        self.leaves[word] &= !bit;
        if self.leaves[word] == 0 {
            self.summary &= !(1u64 << word);
        }
        true
    }

    /// Whether bit `index` is set (`false` out of range).
    #[inline]
    pub fn is_set(&self, index: usize) -> bool {
        index < SECTION_VOLUME && self.leaves[index >> 6] & (1u64 << (index & 63)) != 0
    }

    /// O(1) via the summary word: `true` when no bit is set.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.summary == 0
    }

    /// Number of set bits, counted over non-empty leaf words only.
    pub fn count(&self) -> usize {
        let mut summary = self.summary;
        let mut n = 0usize;
        while summary != 0 {
            let word = summary.trailing_zeros() as usize;
            summary &= summary - 1;
            n += self.leaves[word].count_ones() as usize;
        }
        n
    }

    /// Number of leaf words holding at least one set bit — the diagnostic
    /// proving iteration cost scales with density, not volume.
    #[inline]
    pub fn non_empty_leaf_count(&self) -> u32 {
        self.summary.count_ones()
    }

    /// Visit every set index in ascending order, `0..4096`.
    ///
    /// Runs `trailing_zeros` descent over the summary word first, so empty
    /// leaf words — and entirely empty trees — are skipped without
    /// examining a single block bit.
    pub fn for_each_set(&self, mut f: impl FnMut(usize)) {
        let mut summary = self.summary;
        while summary != 0 {
            let word = summary.trailing_zeros() as usize;
            summary &= summary - 1;
            let mut bits = self.leaves[word];
            while bits != 0 {
                let bit = bits.trailing_zeros() as usize;
                bits &= bits - 1;
                f((word << 6) | bit);
            }
        }
    }

    /// Clear every bit.
    pub fn reset(&mut self) {
        self.leaves = [0; SECTION_WORDS];
        self.summary = 0;
    }
}

impl Default for SectionBitTree4096 {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for SectionBitTree4096 {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SectionBitTree4096")
            .field("count", &self.count())
            .field("non_empty_leaves", &self.non_empty_leaf_count())
            .finish()
    }
}
