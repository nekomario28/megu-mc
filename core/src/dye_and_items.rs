// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
//! `dye_and_items`: the root-level, data-driven Vanilla 1.21.1 universal dye
//! and complete item-use interaction kernel.
//!
//! Every table in this module was extracted from the pinned vanilla 1.21.1
//! server jar (`sha1 59353fb40c36d304f2035d51e7d6e6baa98dc05c`, the same jar
//! `tools/bytecode_probe.sh` and `tools/provenance_check.py` pin) by reading
//! `javap -c` bytecode and the embedded datapack JSONs:
//!
//! * [`DyeColor`] — the 16-color table from `net.minecraft.world.item.DyeColor`
//!   `<clinit>` (texture-diffuse, firework and text/sign RGB triples).
//! * [`mix_dye_color`] — the exact `DyedItemColor.applyDyes` integer algorithm:
//!   channel averages renormalized by (average channel max / max channel avg).
//! * [`cauldron_wash_dyed_item`] — the `CauldronInteraction.DYED_ITEM` wash:
//!   clears the dyed component and decrements water level `3->2->1->0`.
//! * [`apply_dye_target`] — the universal applicator dispatcher covering every
//!   vanilla dye target: `Sign` (dye/`glow_ink_sac`/`ink_sac`/`honeycomb` via
//!   `SignApplicator.canApplyToSign` = `SignText.hasMessage`), `Sheep`,
//!   tamed `Wolf`/`Cat` collars, all dyeable block-craft families and
//!   `FireworkStar` primary/fade colors.
//! * [`ItemUseFamily`] + [`classify_item_use`] — exhaustive data-driven
//!   classification of all 1333 `minecraft:item` registry entries (protocol
//!   767, mirrored in `proto/src/data_v767.rs`) into eight families with zero
//!   uncategorized items. The literal coverage table is what
//!   `tools/item_interaction_audit.py` verifies against the registry.
//! * [`tool_block_transform`] — axe strip/`WeatheringCopper` scrape/unwax
//!   (`AxeItem.STRIPPABLES`, `HoneycombItem.WAXABLES`, `WeatheringCopper.NEXT_BY_BLOCK`),
//!   hoe `TILLABLES`, shovel `FLATTENABLES` + campfire extinguish, shears
//!   carve/harvest/stop-growth, `FlintAndSteelItem`/`FireChargeItem` ignite and
//!   `HoneycombItem` wax.
//! * [`food_properties`] — the complete `net.minecraft.world.food.Foods` table
//!   (nutrition, saturation modifier, always-edible, fast-eat, bowl/bottle
//!   remainder, `MobEffectInstance` duration/amplifier/probability).
//! * [`composter_chance`] — the complete `ComposterBlock.COMPOSTABLES` chance
//!   table (101 entries; note cake and pumpkin pie are 0.85, not 1.0).
//! * [`breed_foods_for`]/[`tame_foods_for`] — the datapack
//!   `data/minecraft/tags/item/*_food.json` tables embedded in the jar.
//! * [`equip_slot`]/[`spawn_egg_entity`]/[`is_music_disc`] — the remaining
//!   per-item interaction lookups the server wires through one call.
//!
//! The module is deliberately side-effect free: it returns *what vanilla would
//! decide* (`Option`/`Result` outcomes with `&'static str` block/item names),
//! and the live server in `src/` applies the decision to world state.

// ===========================================================================
// Mission 1 — the 16-color dye kernel
// ===========================================================================

/// One of vanilla's 16 `DyeColor` values, in registry order (`WHITE = 0`
/// through `BLACK = 15`). Vanilla stores three RGB ints per color:
/// `textureDiffuseColor` (sheep wool, collars, leather dye mixing — kept as an
/// opaque `ARGB32` with `0xFF000000 | rgb`), `fireworkColor` and `textColor`
/// (sign text and chat `§` formatting).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
#[repr(u8)]
pub enum DyeColor {
    White = 0,
    Orange = 1,
    Magenta = 2,
    LightBlue = 3,
    Yellow = 4,
    Lime = 5,
    Pink = 6,
    Gray = 7,
    LightGray = 8,
    Cyan = 9,
    Purple = 10,
    Blue = 11,
    Brown = 12,
    Green = 13,
    Red = 14,
    Black = 15,
}

/// `(name, texture_diffuse_rgb, firework_rgb, text_rgb)` — the literal ints
/// pushed by `DyeColor.<clinit>` in the pinned 1.21.1 jar. The constructor
/// stores `FastColor.ARGB32.opaque(diffuse)` i.e. `0xFF000000 | diffuse`.
const DYE_SPECS: [(&str, u32, u32, u32); 16] = [
    ("white", 0x00F9_FFFE, 0x00F0_F0F0, 0x00FF_FFFF),
    ("orange", 0x00F9_801D, 0x00EB_8844, 0x00FF_681F),
    ("magenta", 0x00C7_4EBD, 0x00C3_54CD, 0x00FF_00FF),
    ("light_blue", 0x003A_B3DA, 0x0066_89D3, 0x009A_C0CD),
    ("yellow", 0x00FE_D83D, 0x00DE_CF2A, 0x00FF_FF00),
    ("lime", 0x0080_C71F, 0x0041_CD34, 0x00BF_FF00),
    ("pink", 0x00F3_8BAA, 0x00D8_8198, 0x00FF_69B4),
    ("gray", 0x0047_4F52, 0x0043_4343, 0x0080_8080),
    ("light_gray", 0x009D_9D97, 0x00AB_ABAB, 0x00D3_D3D3),
    ("cyan", 0x0016_9C9C, 0x0028_7697, 0x0000_FFFF),
    ("purple", 0x0089_32B8, 0x007B_2FBE, 0x00A0_20F0),
    ("blue", 0x003C_44AA, 0x0025_3192, 0x0000_00FF),
    ("brown", 0x0083_5432, 0x0051_301A, 0x008B_4513),
    ("green", 0x005E_7C16, 0x003B_511A, 0x0000_FF00),
    ("red", 0x00B0_2E26, 0x00B3_312C, 0x00FF_0000),
    ("black", 0x001D_1D21, 0x001E_1B1B, 0x0000_0000),
];

impl DyeColor {
    /// All 16 colors in `DyeColor` registry order.
    pub const ALL: [DyeColor; 16] = [
        DyeColor::White,
        DyeColor::Orange,
        DyeColor::Magenta,
        DyeColor::LightBlue,
        DyeColor::Yellow,
        DyeColor::Lime,
        DyeColor::Pink,
        DyeColor::Gray,
        DyeColor::LightGray,
        DyeColor::Cyan,
        DyeColor::Purple,
        DyeColor::Blue,
        DyeColor::Brown,
        DyeColor::Green,
        DyeColor::Red,
        DyeColor::Black,
    ];

    /// Vanilla `DyeColor.getId()` — the 0..15 registry id (same as protocol
    /// dye metadata and the `white_dye..black_dye` item ordering).
    #[inline]
    pub const fn id(self) -> u8 {
        self as u8
    }

    /// The registry name without namespace (`"light_blue"`).
    #[inline]
    pub const fn name(self) -> &'static str {
        DYE_SPECS[self.id() as usize].0
    }

    /// The dye item id (`"minecraft:<name>_dye"`).
    pub const fn item_id(self) -> &'static str {
        const ITEM_IDS: [&str; 16] = [
            "minecraft:white_dye",
            "minecraft:orange_dye",
            "minecraft:magenta_dye",
            "minecraft:light_blue_dye",
            "minecraft:yellow_dye",
            "minecraft:lime_dye",
            "minecraft:pink_dye",
            "minecraft:gray_dye",
            "minecraft:light_gray_dye",
            "minecraft:cyan_dye",
            "minecraft:purple_dye",
            "minecraft:blue_dye",
            "minecraft:brown_dye",
            "minecraft:green_dye",
            "minecraft:red_dye",
            "minecraft:black_dye",
        ];
        ITEM_IDS[self.id() as usize]
    }

    /// `getTextureDiffuseColor` as `0x00RRGGBB` — the wool/collar/mixing color.
    #[inline]
    pub const fn texture_diffuse_rgb(self) -> u32 {
        DYE_SPECS[self.id() as usize].1
    }

    /// The exact int vanilla stores: `FastColor.ARGB32.opaque(rgb)` =
    /// `0xFF000000 | rgb`.
    #[inline]
    pub const fn texture_diffuse_argb(self) -> u32 {
        0xFF00_0000 | self.texture_diffuse_rgb()
    }

    /// `getFireworkColor` as `0x00RRGGBB` — written into firework stars.
    #[inline]
    pub const fn firework_rgb(self) -> u32 {
        DYE_SPECS[self.id() as usize].2
    }

    /// `getTextColor` as `0x00RRGGBB` — sign text and `§`-formatting color.
    #[inline]
    pub const fn text_rgb(self) -> u32 {
        DYE_SPECS[self.id() as usize].3
    }

    /// `DyeColor.byId` — `None` for `id >= 16`.
    #[inline]
    pub const fn by_id(id: u8) -> Option<DyeColor> {
        if id < 16 {
            // SAFETY-free: the table is dense in id order.
            Some(Self::ALL[id as usize])
        } else {
            None
        }
    }

    /// `DyeItem` → `DyeColor`. Accepts `"minecraft:<name>_dye"` or the bare
    /// `<name>_dye` path. (`cocoa_beans` intentionally does *not* resolve:
    /// in 1.21.1 it is a `BlockItem` planting cocoa pods, not a dye.)
    pub fn from_item(item: &str) -> Option<DyeColor> {
        let path = item.strip_prefix("minecraft:").unwrap_or(item);
        let dye = path.strip_suffix("_dye")?;
        Self::ALL.iter().copied().find(|c| c.name() == dye)
    }
}

/// `DyedItemColor.LEATHER_COLOR` = `0xFFA06540` — the default dyed color of
/// leather armor / leather horse armor / wolf armor, RGB form.
pub const LEATHER_DEFAULT_RGB: u32 = 0x00A0_6540;

/// Items that carry a `DYED_COLOR` component and accept [`mix_dye_color`]
/// results: the 4 leather armor pieces plus `leather_horse_armor` and
/// `wolf_armor` (both dyeable in 1.21.1).
pub fn is_dyeable_leather(item: &str) -> bool {
    let n = item.strip_prefix("minecraft:").unwrap_or(item);
    matches!(
        n,
        "leather_helmet"
            | "leather_chestplate"
            | "leather_leggings"
            | "leather_boots"
            | "leather_horse_armor"
            | "wolf_armor"
    )
}

/// The exact `DyedItemColor.applyDyes` integer algorithm from 1.21.1
/// (`cxk.a(cuq, List<ctj>)`): each contributing color — the existing dyed
/// component once, then each dye's `textureDiffuseColor` once — adds its
/// `ARGB32` channels to per-channel sums and its channel max to a separate
/// `maxSum`. Vanilla then computes integer channel averages and rescales them
/// by `avgMaxSum / max(avgChannels)` so the result keeps the brightness of
/// the brightest contributor.
///
/// `base` is the item's current `0xRRGGBB` (or `ARGB`) color; pass
/// `Some(LEATHER_DEFAULT_RGB)` for a never-dyed leather piece, `None` for a
/// pure-dye mix. Returns `0` when nothing contributed (vanilla would crash
/// there; callers pass at least one dye by construction).
pub fn mix_dye_color(base: Option<u32>, dyes: &[DyeColor]) -> u32 {
    let mut r_sum: u32 = 0;
    let mut g_sum: u32 = 0;
    let mut b_sum: u32 = 0;
    let mut max_sum: u32 = 0;
    let mut count: u32 = 0;
    let mut add = |argb: u32| {
        let r = (argb >> 16) & 0xFF;
        let g = (argb >> 8) & 0xFF;
        let b = argb & 0xFF;
        max_sum += r.max(g).max(b);
        r_sum += r;
        g_sum += g;
        b_sum += b;
        count += 1;
    };
    if let Some(c) = base {
        add(c);
    }
    for d in dyes {
        add(d.texture_diffuse_argb());
    }
    if count == 0 {
        return 0;
    }
    let mut avg_r = r_sum / count;
    let mut avg_g = g_sum / count;
    let mut avg_b = b_sum / count;
    let avg_max = max_sum as f32 / count as f32;
    let max_avg = avg_r.max(avg_g).max(avg_b) as f32;
    if max_avg == 0.0 {
        return 0;
    }
    avg_r = (avg_r as f32 * avg_max / max_avg) as u32;
    avg_g = (avg_g as f32 * avg_max / max_avg) as u32;
    avg_b = (avg_b as f32 * avg_max / max_avg) as u32;
    (avg_r << 16) | (avg_g << 8) | avg_b
}

/// `CauldronInteraction.DYED_ITEM` on a water cauldron: a dyed item's color is
/// cleared and the water level drops by one (`3 -> 2 -> 1 -> 0`). Returns the
/// post-wash `(color, water_level)`; `None` when there is no color to clear or
/// no water left (the interaction then does nothing).
pub fn cauldron_wash_dyed_item(color: Option<u32>, water_level: u8) -> Option<(Option<u32>, u8)> {
    if water_level == 0 || color.is_none() {
        return None;
    }
    Some((None, water_level - 1))
}

// ===========================================================================
// Mission 1b — universal dye application dispatcher
// ===========================================================================

/// The four vanilla `SignApplicator`/dye-item applicators, resolved from the
/// held item by [`dye_applicator_for_item`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DyeApplicator {
    /// Any `minecraft:<color>_dye` (or `cocoa_beans` as brown).
    Dye(DyeColor),
    /// `minecraft:ink_sac` — clears `hasGlowingText` on the clicked side.
    InkSac,
    /// `minecraft:glow_ink_sac` — sets `hasGlowingText` on the clicked side.
    GlowInkSac,
    /// `minecraft:honeycomb` — waxes the sign (and waxes copper blocks).
    Honeycomb,
}

/// Map a held item to its sign/dye applicator role, or `None`.
pub fn dye_applicator_for_item(item: &str) -> Option<DyeApplicator> {
    let path = item.strip_prefix("minecraft:").unwrap_or(item);
    match path {
        "ink_sac" => Some(DyeApplicator::InkSac),
        "glow_ink_sac" => Some(DyeApplicator::GlowInkSac),
        "honeycomb" => Some(DyeApplicator::Honeycomb),
        _ => DyeColor::from_item(path).map(DyeApplicator::Dye),
    }
}

/// A tamed collar target (`TamedWolf`/`TamedCat` per the spec).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CollaredPet {
    Wolf,
    Cat,
}

/// Every vanilla dyeable target, reduced to the state the rule evaluation
/// actually reads. The caller maps live world state into this enum; the kernel
/// never touches the world itself.
#[derive(Clone, Copy, Debug)]
pub enum DyeTarget<'a> {
    /// A sign side. `has_text` is `SignText.hasMessage(player)` for the
    /// clicked side; `text_color` is the side's current `DyeColor`;
    /// `glowing` is `hasGlowingText`; `waxed` is `isWaxed`.
    Sign {
        has_text: bool,
        text_color: Option<DyeColor>,
        glowing: bool,
        waxed: bool,
    },
    /// A sheep. Vanilla additionally requires `readyForShearing` (alive,
    /// adult); `sheared` covers the sheared check.
    Sheep { wool_color: DyeColor, sheared: bool },
    /// A tamed wolf or cat collar. `by_owner` is vanilla's
    /// `Entity.isOwnedBy(player)` owner gate — strangers cannot dye.
    Collar {
        pet: CollaredPet,
        color: DyeColor,
        by_owner: bool,
    },
    /// Any dyeable block-craft family member (`item` is the held/block item
    /// name); resolved through [`craft_dye_result`].
    BlockCraft { item: &'a str },
    /// A `firework_star` being assembled (adds primary colors) or an existing
    /// star taking fade colors. Vanilla allows at most 8 dyes per star.
    FireworkStar {
        primary_dyes: usize,
        fade_dyes: usize,
    },
}

/// What the dispatcher decided — the caller applies it to world state.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DyeApplied {
    /// Sign text color set (always applies when allowed — vanilla sets the
    /// color even when unchanged).
    SignColor(DyeColor),
    /// `hasGlowingText` set/cleared by glow/normal ink sac.
    SignGlow(bool),
    /// Sign waxed by honeycomb.
    SignWaxed,
    /// Sheep wool color set.
    SheepColor(DyeColor),
    /// Wolf/cat collar color set.
    CollarColor(DyeColor),
    /// Block-craft family member recolored — carries the resulting item name
    /// (`"red_wool"`, `"cyan_shulker_box"`, ...).
    CraftRecolor(&'static str),
    /// A dye added to a firework star's primary color list.
    FireworkPrimaryAdded(DyeColor),
    /// A dye added to a firework star's fade list.
    FireworkFadeAdded(DyeColor),
}

/// Why a dye application was rejected — one variant per vanilla rule, so the
/// caller can log a precise parity reason.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DyeReject {
    /// The held item is not a dye applicator at all.
    NotAnApplicator,
    /// The sign is waxed — `SignBlockEntity.updateText` fails for
    /// dye/ink/glow (`honeycomb` reports [`DyeReject::AlreadyApplied`]).
    SignWaxed,
    /// `SignApplicator.canApplyToSign` = `SignText.hasMessage` failed:
    /// the clicked side has no text, so no applicator (including wax) works.
    SignNoText,
    /// The operation is already in the requested state (sign already glowing/
    /// not-glowing, sign already waxed).
    AlreadyApplied,
    /// Target already carries this exact color (sheep, collars).
    SameColor,
    /// The sheep is sheared (`readyForShearing` fails).
    Sheared,
    /// `Entity.isOwnedBy` failed — only the owner may dye a collar.
    NotOwner,
    /// The item is not a member of a dyeable block-craft family.
    NotDyeable,
    /// The firework star already holds the vanilla maximum of 8 dyes in that
    /// list.
    FireworkFull,
}

/// The universal dye application dispatcher: evaluates the vanilla rule for
/// every dyeable target in one place and returns the decision.
pub fn apply_dye_target(
    target: &DyeTarget<'_>,
    applicator: DyeApplicator,
) -> Result<DyeApplied, DyeReject> {
    match *target {
        DyeTarget::Sign {
            has_text,
            text_color: _,
            glowing,
            waxed,
        } => match applicator {
            // Vanilla `SignBlock.useItemOn`: `canApplyToSign`
            // (`hasMessage` on the clicked side) gates every applicator,
            // then `updateText`/`setWaxed` performs the mutation.
            DyeApplicator::Honeycomb => {
                if !has_text {
                    Err(DyeReject::SignNoText)
                } else if waxed {
                    Err(DyeReject::AlreadyApplied)
                } else {
                    Ok(DyeApplied::SignWaxed)
                }
            }
            DyeApplicator::Dye(c) => {
                if waxed {
                    Err(DyeReject::SignWaxed)
                } else if !has_text {
                    Err(DyeReject::SignNoText)
                } else {
                    Ok(DyeApplied::SignColor(c))
                }
            }
            DyeApplicator::GlowInkSac => {
                if waxed {
                    Err(DyeReject::SignWaxed)
                } else if !has_text {
                    Err(DyeReject::SignNoText)
                } else if glowing {
                    Err(DyeReject::AlreadyApplied)
                } else {
                    Ok(DyeApplied::SignGlow(true))
                }
            }
            DyeApplicator::InkSac => {
                if waxed {
                    Err(DyeReject::SignWaxed)
                } else if !has_text {
                    Err(DyeReject::SignNoText)
                } else if !glowing {
                    Err(DyeReject::AlreadyApplied)
                } else {
                    Ok(DyeApplied::SignGlow(false))
                }
            }
        },
        DyeTarget::Sheep {
            wool_color,
            sheared,
        } => match applicator {
            DyeApplicator::Dye(c) => {
                if sheared {
                    Err(DyeReject::Sheared)
                } else if c == wool_color {
                    Err(DyeReject::SameColor)
                } else {
                    Ok(DyeApplied::SheepColor(c))
                }
            }
            _ => Err(DyeReject::NotAnApplicator),
        },
        DyeTarget::Collar {
            pet: _,
            color,
            by_owner,
        } => match applicator {
            DyeApplicator::Dye(c) => {
                if !by_owner {
                    Err(DyeReject::NotOwner)
                } else if c == color {
                    Err(DyeReject::SameColor)
                } else {
                    Ok(DyeApplied::CollarColor(c))
                }
            }
            _ => Err(DyeReject::NotAnApplicator),
        },
        DyeTarget::BlockCraft { item } => match applicator {
            DyeApplicator::Dye(c) => craft_dye_result(item, c)
                .map(DyeApplied::CraftRecolor)
                .ok_or(DyeReject::NotDyeable),
            _ => Err(DyeReject::NotAnApplicator),
        },
        DyeTarget::FireworkStar {
            primary_dyes,
            fade_dyes,
        } => match applicator {
            DyeApplicator::Dye(c) => {
                if primary_dyes < 8 {
                    Ok(DyeApplied::FireworkPrimaryAdded(c))
                } else if fade_dyes < 8 {
                    Ok(DyeApplied::FireworkFadeAdded(c))
                } else {
                    Err(DyeReject::FireworkFull)
                }
            }
            _ => Err(DyeReject::NotAnApplicator),
        },
    }
}

// ---- dyeable block-craft families -----------------------------------------

/// A dyeable block/item family. The recipe semantics differ per family; see
/// [`craft_dye_recipe`]. All ten families the spec lists are represented,
/// including the two whose 1.21.1 recipe is an initial-color craft rather
/// than a recolor (`ConcretePowder`) and the one with no 1.21.1 recipe at
/// all (`Bundle` — its recolor recipe arrived in 1.21.2).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CraftDyeKind {
    Wool,
    Carpet,
    Bed,
    Terracotta,
    StainedGlass,
    StainedGlassPane,
    Candle,
    ShulkerBox,
    ConcretePowder,
    Bundle,
}

/// Which of the ten dyeable families an item name belongs to.
/// `shulker_box` itself and `white_terracotta`/`terracotta`/`glass`/`glass_pane`
/// all resolve; colors are read off the `<color>_` prefix where present.
pub fn dyeable_craft_kind(item: &str) -> Option<CraftDyeKind> {
    let n = item.strip_prefix("minecraft:").unwrap_or(item);
    for c in DyeColor::ALL {
        let p = c.name();
        if let Some(rest) = n.strip_prefix(p) {
            let rest = rest.strip_prefix('_').unwrap_or(rest);
            return Some(match rest {
                "wool" => CraftDyeKind::Wool,
                "carpet" => CraftDyeKind::Carpet,
                "bed" => CraftDyeKind::Bed,
                "terracotta" => CraftDyeKind::Terracotta,
                "stained_glass" => CraftDyeKind::StainedGlass,
                "stained_glass_pane" => CraftDyeKind::StainedGlassPane,
                "candle" => CraftDyeKind::Candle,
                "shulker_box" => CraftDyeKind::ShulkerBox,
                "concrete_powder" => CraftDyeKind::ConcretePowder,
                _ => continue,
            });
        }
    }
    match n {
        "terracotta" => Some(CraftDyeKind::Terracotta),
        "glass" => Some(CraftDyeKind::StainedGlass),
        "glass_pane" => Some(CraftDyeKind::StainedGlassPane),
        "candle" => Some(CraftDyeKind::Candle),
        "shulker_box" => Some(CraftDyeKind::ShulkerBox),
        "bundle" => Some(CraftDyeKind::Bundle),
        _ => None,
    }
}

/// The vanilla 1.21.1 recipe shape for a dyeable family, extracted from
/// `data/minecraft/recipe/*.json` in the jar.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CraftDyeRecipe {
    /// `dye_<c>_{wool,carpet,bed}`: shapeless any-family-member + dye → 1
    /// recolored item (works on already-colored members too).
    SingleRecolor,
    /// `<c>_candle`: shapeless uncolored `candle` + dye → 1 colored candle.
    /// Vanilla offers no re-dye for colored candles.
    SingleUncoloredOnly,
    /// `crafting_special_shulkerboxcoloring`: any shulker box + dye →
    /// recolored, preserving contents.
    SpecialShulkerBox,
    /// `<c>_terracotta` / `<c>_stained_glass` /
    /// `<c>_stained_glass_pane_from_glass_pane`: shaped `###/#X#/###` — 8
    /// uncolored base items around 1 dye → 8 colored. Initial coloring only;
    /// already-colored members cannot be re-dyed in 1.21.1.
    EightAroundInitial,
    /// `<c>_concrete_powder`: shapeless dye + 4 sand + 4 gravel → 8 powder.
    /// An initial craft, never a recolor.
    InitialConcretePowder,
    /// Vanilla 1.21.1 has no recipe: `bundle` exists but bundle dyeing only
    /// arrived in 1.21.2. Present for exhaustive coverage.
    NoRecipe1211,
}

/// The recipe semantics for each dyeable family.
pub fn craft_dye_recipe(kind: CraftDyeKind) -> CraftDyeRecipe {
    match kind {
        CraftDyeKind::Wool | CraftDyeKind::Carpet | CraftDyeKind::Bed => {
            CraftDyeRecipe::SingleRecolor
        }
        CraftDyeKind::Candle => CraftDyeRecipe::SingleUncoloredOnly,
        CraftDyeKind::ShulkerBox => CraftDyeRecipe::SpecialShulkerBox,
        CraftDyeKind::Terracotta
        | CraftDyeKind::StainedGlass
        | CraftDyeKind::StainedGlassPane => CraftDyeRecipe::EightAroundInitial,
        CraftDyeKind::ConcretePowder => CraftDyeRecipe::InitialConcretePowder,
        CraftDyeKind::Bundle => CraftDyeRecipe::NoRecipe1211,
    }
}

/// Build `"<color>_<suffix>"` as a `'static str` without allocation: the 16
/// color names and the fixed family suffixes are all string literals, so each
/// combination is emitted as its own table row (16 rows per family).
fn craft_colored_name(color: DyeColor, suffix_rows: &'static [&'static str; 16]) -> &'static str {
    suffix_rows[color.id() as usize]
}

const WOOL_ITEMS: [&str; 16] = [
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
];
const CARPET_ITEMS: [&str; 16] = [
    "white_carpet",
    "orange_carpet",
    "magenta_carpet",
    "light_blue_carpet",
    "yellow_carpet",
    "lime_carpet",
    "pink_carpet",
    "gray_carpet",
    "light_gray_carpet",
    "cyan_carpet",
    "purple_carpet",
    "blue_carpet",
    "brown_carpet",
    "green_carpet",
    "red_carpet",
    "black_carpet",
];
const BED_ITEMS: [&str; 16] = [
    "white_bed",
    "orange_bed",
    "magenta_bed",
    "light_blue_bed",
    "yellow_bed",
    "lime_bed",
    "pink_bed",
    "gray_bed",
    "light_gray_bed",
    "cyan_bed",
    "purple_bed",
    "blue_bed",
    "brown_bed",
    "green_bed",
    "red_bed",
    "black_bed",
];
const TERRACOTTA_ITEMS: [&str; 16] = [
    "white_terracotta",
    "orange_terracotta",
    "magenta_terracotta",
    "light_blue_terracotta",
    "yellow_terracotta",
    "lime_terracotta",
    "pink_terracotta",
    "gray_terracotta",
    "light_gray_terracotta",
    "cyan_terracotta",
    "purple_terracotta",
    "blue_terracotta",
    "brown_terracotta",
    "green_terracotta",
    "red_terracotta",
    "black_terracotta",
];
const STAINED_GLASS_ITEMS: [&str; 16] = [
    "white_stained_glass",
    "orange_stained_glass",
    "magenta_stained_glass",
    "light_blue_stained_glass",
    "yellow_stained_glass",
    "lime_stained_glass",
    "pink_stained_glass",
    "gray_stained_glass",
    "light_gray_stained_glass",
    "cyan_stained_glass",
    "purple_stained_glass",
    "blue_stained_glass",
    "brown_stained_glass",
    "green_stained_glass",
    "red_stained_glass",
    "black_stained_glass",
];
const STAINED_GLASS_PANE_ITEMS: [&str; 16] = [
    "white_stained_glass_pane",
    "orange_stained_glass_pane",
    "magenta_stained_glass_pane",
    "light_blue_stained_glass_pane",
    "yellow_stained_glass_pane",
    "lime_stained_glass_pane",
    "pink_stained_glass_pane",
    "gray_stained_glass_pane",
    "light_gray_stained_glass_pane",
    "cyan_stained_glass_pane",
    "purple_stained_glass_pane",
    "blue_stained_glass_pane",
    "brown_stained_glass_pane",
    "green_stained_glass_pane",
    "red_stained_glass_pane",
    "black_stained_glass_pane",
];
const CANDLE_ITEMS: [&str; 16] = [
    "white_candle",
    "orange_candle",
    "magenta_candle",
    "light_blue_candle",
    "yellow_candle",
    "lime_candle",
    "pink_candle",
    "gray_candle",
    "light_gray_candle",
    "cyan_candle",
    "purple_candle",
    "blue_candle",
    "brown_candle",
    "green_candle",
    "red_candle",
    "black_candle",
];
const SHULKER_BOX_ITEMS: [&str; 16] = [
    "white_shulker_box",
    "orange_shulker_box",
    "magenta_shulker_box",
    "light_blue_shulker_box",
    "yellow_shulker_box",
    "lime_shulker_box",
    "pink_shulker_box",
    "gray_shulker_box",
    "light_gray_shulker_box",
    "cyan_shulker_box",
    "purple_shulker_box",
    "blue_shulker_box",
    "brown_shulker_box",
    "green_shulker_box",
    "red_shulker_box",
    "black_shulker_box",
];
const CONCRETE_POWDER_ITEMS: [&str; 16] = [
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
];

/// The resulting item name when a dye is applied to a family member under the
/// vanilla recipe, or `None` when the recipe does not apply (e.g. a colored
/// candle or terracotta — vanilla 1.21.1 cannot re-dye them — and `bundle`,
/// which has no 1.21.1 dye recipe at all).
pub fn craft_dye_result(item: &str, dye: DyeColor) -> Option<&'static str> {
    let n = item.strip_prefix("minecraft:").unwrap_or(item);
    match dyeable_craft_kind(n)? {
        CraftDyeKind::Wool => Some(craft_colored_name(dye, &WOOL_ITEMS)),
        CraftDyeKind::Carpet => Some(craft_colored_name(dye, &CARPET_ITEMS)),
        CraftDyeKind::Bed => Some(craft_colored_name(dye, &BED_ITEMS)),
        CraftDyeKind::Candle => {
            if n == "candle" {
                Some(craft_colored_name(dye, &CANDLE_ITEMS))
            } else {
                None // colored candles cannot be re-dyed in 1.21.1
            }
        }
        CraftDyeKind::ShulkerBox => Some(craft_colored_name(dye, &SHULKER_BOX_ITEMS)),
        CraftDyeKind::Terracotta => {
            if n == "terracotta" {
                Some(craft_colored_name(dye, &TERRACOTTA_ITEMS))
            } else {
                None // 8-around recipe colors only the uncolored base
            }
        }
        CraftDyeKind::StainedGlass => {
            if n == "glass" {
                Some(craft_colored_name(dye, &STAINED_GLASS_ITEMS))
            } else {
                None
            }
        }
        CraftDyeKind::StainedGlassPane => {
            if n == "glass_pane" {
                Some(craft_colored_name(dye, &STAINED_GLASS_PANE_ITEMS))
            } else {
                None
            }
        }
        CraftDyeKind::ConcretePowder | CraftDyeKind::Bundle => None,
    }
}

/// The colored-family name for a concrete powder color (the *result* of the
/// initial sand+gravel+dye craft — kept for callers that construct it).
pub fn concrete_powder_item(dye: DyeColor) -> &'static str {
    craft_colored_name(dye, &CONCRETE_POWDER_ITEMS)
}

// ===========================================================================
// Mission 2 — complete data-driven 1.21.1 item-use classification kernel
// ===========================================================================

/// The eight exhaustive item-use families. Every one of the 1333 vanilla
/// 1.21.1 items (the `minecraft:item` registry mirrored in
/// `proto/src/data_v767.rs` as `V767_ITEMS`) classifies into exactly one;
/// there is deliberately no "uncategorized" variant — `PureMaterial` is the
/// explicit no-right-click-action family.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum ItemUseFamily {
    /// Axe strip/scrape-copper/unwax, hoe till, shovel flatten/extinguish,
    /// shears carve/harvest/stop-growth, flint & steel / fire charge ignite,
    /// honeycomb wax. Rule evaluation: [`tool_block_transform`].
    ToolBlockTransform,
    /// Buckets (empty/water/lava/powder-snow/milk/fish-axolotl-tadpole),
    /// glass bottle (water/honey/dragon-breath pickup), bowl (stew extract).
    FluidContainer,
    /// Every vanilla 1.21.1 food & drink — see [`food_properties`] for
    /// nutrition / saturation modifier / fast-eat / always-edible / effects /
    /// remainder.
    Consumable,
    /// Bow, crossbow, trident, snowball, egg, ender pearl, experience bottle,
    /// wind charge, ender eye, splash/lingering potion, firework rocket
    /// (including elytra boost), fishing rod.
    RangedOrThrowable,
    /// Helmets/chestplates/leggings/boots, elytra, carved pumpkin, wearable
    /// heads, shield, horse armor, wolf armor — see [`equip_slot`].
    Equipable,
    /// Interactions whose target is an entity: lead, name tag, saddle, spawn
    /// eggs (all 80 + spawner override), the 16 dyes + ink sacs (entity/sign
    /// application via [`apply_dye_target`]), brush (suspicious sand/gravel
    /// archaeology + armadillo scutes), `bone` (wolf tame), and the two
    /// `*_on_a_stick` mount-boost items. Breed/tame food *items* keep their
    /// primary family — the rule surfaces through
    /// [`breed_foods_for`]/[`tame_foods_for`].
    EntityUse,
    /// Block items and world-facing specials: all placable blocks (incl.
    /// alias block items like seeds/string/redstone), bone meal, composter
    /// input, music discs (jukebox), goat horn, spyglass, compass→lodestone,
    /// writable/written book, map, armor stand, boats, minecarts, item
    /// frames, painting, end crystal, debug stick, trial keys, knowledge
    /// book use.
    WorldSpecial,
    /// Explicitly no right-click action: ingots, gems, sticks, strings,
    /// papers, books, ammo (arrow/tipped/spectral), mace, swords, pickaxes,
    /// bundle (inventory-scoped only), totem, sugar, coal, flint, etc.
    PureMaterial,
}

/// The master classification table: every `minecraft:item` registry name
/// (sans namespace, sorted) with its [`ItemUseFamily`]. Coverage is total —
/// `tools/item_interaction_audit.py` verifies that
/// `len(ITEM_USE_CLASSIFICATION) == 1333` equals the `V767_ITEMS` registry.
///
/// Generation rules (mirroring vanilla `Item`/`BlockItem` behavior):
/// food-component items → `Consumable`; buckets/bottles/bowl →
/// `FluidContainer`; throwable/ranged items → `RangedOrThrowable`;
/// equippable gear incl. heads/pumpkin/shield → `Equipable`; tool items with
/// block transforms + honeycomb + fire charge → `ToolBlockTransform`;
/// entity-target items incl. all `*_spawn_egg` → `EntityUse`; every item
/// whose name is a `minecraft:block` registry member (plus alias block items
/// `wheat_seeds`, `string`, `redstone`, `cocoa_beans`, ...) and world
/// specials → `WorldSpecial`; the rest → `PureMaterial`.
pub static ITEM_USE_CLASSIFICATION: &[(&str, ItemUseFamily)] = &[

    ("acacia_boat", ItemUseFamily::WorldSpecial), ("acacia_button", ItemUseFamily::WorldSpecial), ("acacia_chest_boat", ItemUseFamily::WorldSpecial), ("acacia_door", ItemUseFamily::WorldSpecial),
    ("acacia_fence", ItemUseFamily::WorldSpecial), ("acacia_fence_gate", ItemUseFamily::WorldSpecial), ("acacia_hanging_sign", ItemUseFamily::WorldSpecial), ("acacia_leaves", ItemUseFamily::WorldSpecial),
    ("acacia_log", ItemUseFamily::WorldSpecial), ("acacia_planks", ItemUseFamily::WorldSpecial), ("acacia_pressure_plate", ItemUseFamily::WorldSpecial), ("acacia_sapling", ItemUseFamily::WorldSpecial),
    ("acacia_sign", ItemUseFamily::WorldSpecial), ("acacia_slab", ItemUseFamily::WorldSpecial), ("acacia_stairs", ItemUseFamily::WorldSpecial), ("acacia_trapdoor", ItemUseFamily::WorldSpecial),
    ("acacia_wood", ItemUseFamily::WorldSpecial), ("activator_rail", ItemUseFamily::WorldSpecial), ("air", ItemUseFamily::PureMaterial), ("allay_spawn_egg", ItemUseFamily::EntityUse),
    ("allium", ItemUseFamily::WorldSpecial), ("amethyst_block", ItemUseFamily::WorldSpecial), ("amethyst_cluster", ItemUseFamily::WorldSpecial), ("amethyst_shard", ItemUseFamily::PureMaterial),
    ("ancient_debris", ItemUseFamily::WorldSpecial), ("andesite", ItemUseFamily::WorldSpecial), ("andesite_slab", ItemUseFamily::WorldSpecial), ("andesite_stairs", ItemUseFamily::WorldSpecial),
    ("andesite_wall", ItemUseFamily::WorldSpecial), ("angler_pottery_sherd", ItemUseFamily::PureMaterial), ("anvil", ItemUseFamily::WorldSpecial), ("apple", ItemUseFamily::Consumable),
    ("archer_pottery_sherd", ItemUseFamily::PureMaterial), ("armadillo_scute", ItemUseFamily::PureMaterial), ("armadillo_spawn_egg", ItemUseFamily::EntityUse), ("armor_stand", ItemUseFamily::WorldSpecial),
    ("arms_up_pottery_sherd", ItemUseFamily::PureMaterial), ("arrow", ItemUseFamily::PureMaterial), ("axolotl_bucket", ItemUseFamily::FluidContainer), ("axolotl_spawn_egg", ItemUseFamily::EntityUse),
    ("azalea", ItemUseFamily::WorldSpecial), ("azalea_leaves", ItemUseFamily::WorldSpecial), ("azure_bluet", ItemUseFamily::WorldSpecial), ("baked_potato", ItemUseFamily::Consumable),
    ("bamboo", ItemUseFamily::WorldSpecial), ("bamboo_block", ItemUseFamily::WorldSpecial), ("bamboo_button", ItemUseFamily::WorldSpecial), ("bamboo_chest_raft", ItemUseFamily::WorldSpecial),
    ("bamboo_door", ItemUseFamily::WorldSpecial), ("bamboo_fence", ItemUseFamily::WorldSpecial), ("bamboo_fence_gate", ItemUseFamily::WorldSpecial), ("bamboo_hanging_sign", ItemUseFamily::WorldSpecial),
    ("bamboo_mosaic", ItemUseFamily::WorldSpecial), ("bamboo_mosaic_slab", ItemUseFamily::WorldSpecial), ("bamboo_mosaic_stairs", ItemUseFamily::WorldSpecial), ("bamboo_planks", ItemUseFamily::WorldSpecial),
    ("bamboo_pressure_plate", ItemUseFamily::WorldSpecial), ("bamboo_raft", ItemUseFamily::WorldSpecial), ("bamboo_sign", ItemUseFamily::WorldSpecial), ("bamboo_slab", ItemUseFamily::WorldSpecial),
    ("bamboo_stairs", ItemUseFamily::WorldSpecial), ("bamboo_trapdoor", ItemUseFamily::WorldSpecial), ("barrel", ItemUseFamily::WorldSpecial), ("barrier", ItemUseFamily::WorldSpecial),
    ("basalt", ItemUseFamily::WorldSpecial), ("bat_spawn_egg", ItemUseFamily::EntityUse), ("beacon", ItemUseFamily::WorldSpecial), ("bedrock", ItemUseFamily::WorldSpecial),
    ("bee_nest", ItemUseFamily::WorldSpecial), ("bee_spawn_egg", ItemUseFamily::EntityUse), ("beef", ItemUseFamily::Consumable), ("beehive", ItemUseFamily::WorldSpecial),
    ("beetroot", ItemUseFamily::Consumable), ("beetroot_seeds", ItemUseFamily::WorldSpecial), ("beetroot_soup", ItemUseFamily::Consumable), ("bell", ItemUseFamily::WorldSpecial),
    ("big_dripleaf", ItemUseFamily::WorldSpecial), ("birch_boat", ItemUseFamily::WorldSpecial), ("birch_button", ItemUseFamily::WorldSpecial), ("birch_chest_boat", ItemUseFamily::WorldSpecial),
    ("birch_door", ItemUseFamily::WorldSpecial), ("birch_fence", ItemUseFamily::WorldSpecial), ("birch_fence_gate", ItemUseFamily::WorldSpecial), ("birch_hanging_sign", ItemUseFamily::WorldSpecial),
    ("birch_leaves", ItemUseFamily::WorldSpecial), ("birch_log", ItemUseFamily::WorldSpecial), ("birch_planks", ItemUseFamily::WorldSpecial), ("birch_pressure_plate", ItemUseFamily::WorldSpecial),
    ("birch_sapling", ItemUseFamily::WorldSpecial), ("birch_sign", ItemUseFamily::WorldSpecial), ("birch_slab", ItemUseFamily::WorldSpecial), ("birch_stairs", ItemUseFamily::WorldSpecial),
    ("birch_trapdoor", ItemUseFamily::WorldSpecial), ("birch_wood", ItemUseFamily::WorldSpecial), ("black_banner", ItemUseFamily::WorldSpecial), ("black_bed", ItemUseFamily::WorldSpecial),
    ("black_candle", ItemUseFamily::WorldSpecial), ("black_carpet", ItemUseFamily::WorldSpecial), ("black_concrete", ItemUseFamily::WorldSpecial), ("black_concrete_powder", ItemUseFamily::WorldSpecial),
    ("black_dye", ItemUseFamily::EntityUse), ("black_glazed_terracotta", ItemUseFamily::WorldSpecial), ("black_shulker_box", ItemUseFamily::WorldSpecial), ("black_stained_glass", ItemUseFamily::WorldSpecial),
    ("black_stained_glass_pane", ItemUseFamily::WorldSpecial), ("black_terracotta", ItemUseFamily::WorldSpecial), ("black_wool", ItemUseFamily::WorldSpecial), ("blackstone", ItemUseFamily::WorldSpecial),
    ("blackstone_slab", ItemUseFamily::WorldSpecial), ("blackstone_stairs", ItemUseFamily::WorldSpecial), ("blackstone_wall", ItemUseFamily::WorldSpecial), ("blade_pottery_sherd", ItemUseFamily::PureMaterial),
    ("blast_furnace", ItemUseFamily::WorldSpecial), ("blaze_powder", ItemUseFamily::PureMaterial), ("blaze_rod", ItemUseFamily::PureMaterial), ("blaze_spawn_egg", ItemUseFamily::EntityUse),
    ("blue_banner", ItemUseFamily::WorldSpecial), ("blue_bed", ItemUseFamily::WorldSpecial), ("blue_candle", ItemUseFamily::WorldSpecial), ("blue_carpet", ItemUseFamily::WorldSpecial),
    ("blue_concrete", ItemUseFamily::WorldSpecial), ("blue_concrete_powder", ItemUseFamily::WorldSpecial), ("blue_dye", ItemUseFamily::EntityUse), ("blue_glazed_terracotta", ItemUseFamily::WorldSpecial),
    ("blue_ice", ItemUseFamily::WorldSpecial), ("blue_orchid", ItemUseFamily::WorldSpecial), ("blue_shulker_box", ItemUseFamily::WorldSpecial), ("blue_stained_glass", ItemUseFamily::WorldSpecial),
    ("blue_stained_glass_pane", ItemUseFamily::WorldSpecial), ("blue_terracotta", ItemUseFamily::WorldSpecial), ("blue_wool", ItemUseFamily::WorldSpecial), ("bogged_spawn_egg", ItemUseFamily::EntityUse),
    ("bolt_armor_trim_smithing_template", ItemUseFamily::PureMaterial), ("bone", ItemUseFamily::EntityUse), ("bone_block", ItemUseFamily::WorldSpecial), ("bone_meal", ItemUseFamily::WorldSpecial),
    ("book", ItemUseFamily::PureMaterial), ("bookshelf", ItemUseFamily::WorldSpecial), ("bow", ItemUseFamily::RangedOrThrowable), ("bowl", ItemUseFamily::FluidContainer),
    ("brain_coral", ItemUseFamily::WorldSpecial), ("brain_coral_block", ItemUseFamily::WorldSpecial), ("brain_coral_fan", ItemUseFamily::WorldSpecial), ("bread", ItemUseFamily::Consumable),
    ("breeze_rod", ItemUseFamily::PureMaterial), ("breeze_spawn_egg", ItemUseFamily::EntityUse), ("brewer_pottery_sherd", ItemUseFamily::PureMaterial), ("brewing_stand", ItemUseFamily::WorldSpecial),
    ("brick", ItemUseFamily::PureMaterial), ("brick_slab", ItemUseFamily::WorldSpecial), ("brick_stairs", ItemUseFamily::WorldSpecial), ("brick_wall", ItemUseFamily::WorldSpecial),
    ("bricks", ItemUseFamily::WorldSpecial), ("brown_banner", ItemUseFamily::WorldSpecial), ("brown_bed", ItemUseFamily::WorldSpecial), ("brown_candle", ItemUseFamily::WorldSpecial),
    ("brown_carpet", ItemUseFamily::WorldSpecial), ("brown_concrete", ItemUseFamily::WorldSpecial), ("brown_concrete_powder", ItemUseFamily::WorldSpecial), ("brown_dye", ItemUseFamily::EntityUse),
    ("brown_glazed_terracotta", ItemUseFamily::WorldSpecial), ("brown_mushroom", ItemUseFamily::WorldSpecial), ("brown_mushroom_block", ItemUseFamily::WorldSpecial), ("brown_shulker_box", ItemUseFamily::WorldSpecial),
    ("brown_stained_glass", ItemUseFamily::WorldSpecial), ("brown_stained_glass_pane", ItemUseFamily::WorldSpecial), ("brown_terracotta", ItemUseFamily::WorldSpecial), ("brown_wool", ItemUseFamily::WorldSpecial),
    ("brush", ItemUseFamily::EntityUse), ("bubble_coral", ItemUseFamily::WorldSpecial), ("bubble_coral_block", ItemUseFamily::WorldSpecial), ("bubble_coral_fan", ItemUseFamily::WorldSpecial),
    ("bucket", ItemUseFamily::FluidContainer), ("budding_amethyst", ItemUseFamily::WorldSpecial), ("bundle", ItemUseFamily::PureMaterial), ("burn_pottery_sherd", ItemUseFamily::PureMaterial),
    ("cactus", ItemUseFamily::WorldSpecial), ("cake", ItemUseFamily::WorldSpecial), ("calcite", ItemUseFamily::WorldSpecial), ("calibrated_sculk_sensor", ItemUseFamily::WorldSpecial),
    ("camel_spawn_egg", ItemUseFamily::EntityUse), ("campfire", ItemUseFamily::WorldSpecial), ("candle", ItemUseFamily::WorldSpecial), ("carrot", ItemUseFamily::Consumable),
    ("carrot_on_a_stick", ItemUseFamily::EntityUse), ("cartography_table", ItemUseFamily::WorldSpecial), ("carved_pumpkin", ItemUseFamily::Equipable), ("cat_spawn_egg", ItemUseFamily::EntityUse),
    ("cauldron", ItemUseFamily::WorldSpecial), ("cave_spider_spawn_egg", ItemUseFamily::EntityUse), ("chain", ItemUseFamily::WorldSpecial), ("chain_command_block", ItemUseFamily::WorldSpecial),
    ("chainmail_boots", ItemUseFamily::Equipable), ("chainmail_chestplate", ItemUseFamily::Equipable), ("chainmail_helmet", ItemUseFamily::Equipable), ("chainmail_leggings", ItemUseFamily::Equipable),
    ("charcoal", ItemUseFamily::PureMaterial), ("cherry_boat", ItemUseFamily::WorldSpecial), ("cherry_button", ItemUseFamily::WorldSpecial), ("cherry_chest_boat", ItemUseFamily::WorldSpecial),
    ("cherry_door", ItemUseFamily::WorldSpecial), ("cherry_fence", ItemUseFamily::WorldSpecial), ("cherry_fence_gate", ItemUseFamily::WorldSpecial), ("cherry_hanging_sign", ItemUseFamily::WorldSpecial),
    ("cherry_leaves", ItemUseFamily::WorldSpecial), ("cherry_log", ItemUseFamily::WorldSpecial), ("cherry_planks", ItemUseFamily::WorldSpecial), ("cherry_pressure_plate", ItemUseFamily::WorldSpecial),
    ("cherry_sapling", ItemUseFamily::WorldSpecial), ("cherry_sign", ItemUseFamily::WorldSpecial), ("cherry_slab", ItemUseFamily::WorldSpecial), ("cherry_stairs", ItemUseFamily::WorldSpecial),
    ("cherry_trapdoor", ItemUseFamily::WorldSpecial), ("cherry_wood", ItemUseFamily::WorldSpecial), ("chest", ItemUseFamily::WorldSpecial), ("chest_minecart", ItemUseFamily::WorldSpecial),
    ("chicken", ItemUseFamily::Consumable), ("chicken_spawn_egg", ItemUseFamily::EntityUse), ("chipped_anvil", ItemUseFamily::WorldSpecial), ("chiseled_bookshelf", ItemUseFamily::WorldSpecial),
    ("chiseled_copper", ItemUseFamily::WorldSpecial), ("chiseled_deepslate", ItemUseFamily::WorldSpecial), ("chiseled_nether_bricks", ItemUseFamily::WorldSpecial), ("chiseled_polished_blackstone", ItemUseFamily::WorldSpecial),
    ("chiseled_quartz_block", ItemUseFamily::WorldSpecial), ("chiseled_red_sandstone", ItemUseFamily::WorldSpecial), ("chiseled_sandstone", ItemUseFamily::WorldSpecial), ("chiseled_stone_bricks", ItemUseFamily::WorldSpecial),
    ("chiseled_tuff", ItemUseFamily::WorldSpecial), ("chiseled_tuff_bricks", ItemUseFamily::WorldSpecial), ("chorus_flower", ItemUseFamily::WorldSpecial), ("chorus_fruit", ItemUseFamily::Consumable),
    ("chorus_plant", ItemUseFamily::WorldSpecial), ("clay", ItemUseFamily::WorldSpecial), ("clay_ball", ItemUseFamily::PureMaterial), ("clock", ItemUseFamily::PureMaterial),
    ("coal", ItemUseFamily::PureMaterial), ("coal_block", ItemUseFamily::WorldSpecial), ("coal_ore", ItemUseFamily::WorldSpecial), ("coarse_dirt", ItemUseFamily::WorldSpecial),
    ("coast_armor_trim_smithing_template", ItemUseFamily::PureMaterial), ("cobbled_deepslate", ItemUseFamily::WorldSpecial), ("cobbled_deepslate_slab", ItemUseFamily::WorldSpecial), ("cobbled_deepslate_stairs", ItemUseFamily::WorldSpecial),
    ("cobbled_deepslate_wall", ItemUseFamily::WorldSpecial), ("cobblestone", ItemUseFamily::WorldSpecial), ("cobblestone_slab", ItemUseFamily::WorldSpecial), ("cobblestone_stairs", ItemUseFamily::WorldSpecial),
    ("cobblestone_wall", ItemUseFamily::WorldSpecial), ("cobweb", ItemUseFamily::WorldSpecial), ("cocoa_beans", ItemUseFamily::WorldSpecial), ("cod", ItemUseFamily::Consumable),
    ("cod_bucket", ItemUseFamily::FluidContainer), ("cod_spawn_egg", ItemUseFamily::EntityUse), ("command_block", ItemUseFamily::WorldSpecial), ("command_block_minecart", ItemUseFamily::WorldSpecial),
    ("comparator", ItemUseFamily::WorldSpecial), ("compass", ItemUseFamily::WorldSpecial), ("composter", ItemUseFamily::WorldSpecial), ("conduit", ItemUseFamily::WorldSpecial),
    ("cooked_beef", ItemUseFamily::Consumable), ("cooked_chicken", ItemUseFamily::Consumable), ("cooked_cod", ItemUseFamily::Consumable), ("cooked_mutton", ItemUseFamily::Consumable),
    ("cooked_porkchop", ItemUseFamily::Consumable), ("cooked_rabbit", ItemUseFamily::Consumable), ("cooked_salmon", ItemUseFamily::Consumable), ("cookie", ItemUseFamily::Consumable),
    ("copper_block", ItemUseFamily::WorldSpecial), ("copper_bulb", ItemUseFamily::WorldSpecial), ("copper_door", ItemUseFamily::WorldSpecial), ("copper_grate", ItemUseFamily::WorldSpecial),
    ("copper_ingot", ItemUseFamily::PureMaterial), ("copper_ore", ItemUseFamily::WorldSpecial), ("copper_trapdoor", ItemUseFamily::WorldSpecial), ("cornflower", ItemUseFamily::WorldSpecial),
    ("cow_spawn_egg", ItemUseFamily::EntityUse), ("cracked_deepslate_bricks", ItemUseFamily::WorldSpecial), ("cracked_deepslate_tiles", ItemUseFamily::WorldSpecial), ("cracked_nether_bricks", ItemUseFamily::WorldSpecial),
    ("cracked_polished_blackstone_bricks", ItemUseFamily::WorldSpecial), ("cracked_stone_bricks", ItemUseFamily::WorldSpecial), ("crafter", ItemUseFamily::WorldSpecial), ("crafting_table", ItemUseFamily::WorldSpecial),
    ("creeper_banner_pattern", ItemUseFamily::PureMaterial), ("creeper_head", ItemUseFamily::Equipable), ("creeper_spawn_egg", ItemUseFamily::EntityUse), ("crimson_button", ItemUseFamily::WorldSpecial),
    ("crimson_door", ItemUseFamily::WorldSpecial), ("crimson_fence", ItemUseFamily::WorldSpecial), ("crimson_fence_gate", ItemUseFamily::WorldSpecial), ("crimson_fungus", ItemUseFamily::WorldSpecial),
    ("crimson_hanging_sign", ItemUseFamily::WorldSpecial), ("crimson_hyphae", ItemUseFamily::WorldSpecial), ("crimson_nylium", ItemUseFamily::WorldSpecial), ("crimson_planks", ItemUseFamily::WorldSpecial),
    ("crimson_pressure_plate", ItemUseFamily::WorldSpecial), ("crimson_roots", ItemUseFamily::WorldSpecial), ("crimson_sign", ItemUseFamily::WorldSpecial), ("crimson_slab", ItemUseFamily::WorldSpecial),
    ("crimson_stairs", ItemUseFamily::WorldSpecial), ("crimson_stem", ItemUseFamily::WorldSpecial), ("crimson_trapdoor", ItemUseFamily::WorldSpecial), ("crossbow", ItemUseFamily::RangedOrThrowable),
    ("crying_obsidian", ItemUseFamily::WorldSpecial), ("cut_copper", ItemUseFamily::WorldSpecial), ("cut_copper_slab", ItemUseFamily::WorldSpecial), ("cut_copper_stairs", ItemUseFamily::WorldSpecial),
    ("cut_red_sandstone", ItemUseFamily::WorldSpecial), ("cut_red_sandstone_slab", ItemUseFamily::WorldSpecial), ("cut_sandstone", ItemUseFamily::WorldSpecial), ("cut_sandstone_slab", ItemUseFamily::WorldSpecial),
    ("cyan_banner", ItemUseFamily::WorldSpecial), ("cyan_bed", ItemUseFamily::WorldSpecial), ("cyan_candle", ItemUseFamily::WorldSpecial), ("cyan_carpet", ItemUseFamily::WorldSpecial),
    ("cyan_concrete", ItemUseFamily::WorldSpecial), ("cyan_concrete_powder", ItemUseFamily::WorldSpecial), ("cyan_dye", ItemUseFamily::EntityUse), ("cyan_glazed_terracotta", ItemUseFamily::WorldSpecial),
    ("cyan_shulker_box", ItemUseFamily::WorldSpecial), ("cyan_stained_glass", ItemUseFamily::WorldSpecial), ("cyan_stained_glass_pane", ItemUseFamily::WorldSpecial), ("cyan_terracotta", ItemUseFamily::WorldSpecial),
    ("cyan_wool", ItemUseFamily::WorldSpecial), ("damaged_anvil", ItemUseFamily::WorldSpecial), ("dandelion", ItemUseFamily::WorldSpecial), ("danger_pottery_sherd", ItemUseFamily::PureMaterial),
    ("dark_oak_boat", ItemUseFamily::WorldSpecial), ("dark_oak_button", ItemUseFamily::WorldSpecial), ("dark_oak_chest_boat", ItemUseFamily::WorldSpecial), ("dark_oak_door", ItemUseFamily::WorldSpecial),
    ("dark_oak_fence", ItemUseFamily::WorldSpecial), ("dark_oak_fence_gate", ItemUseFamily::WorldSpecial), ("dark_oak_hanging_sign", ItemUseFamily::WorldSpecial), ("dark_oak_leaves", ItemUseFamily::WorldSpecial),
    ("dark_oak_log", ItemUseFamily::WorldSpecial), ("dark_oak_planks", ItemUseFamily::WorldSpecial), ("dark_oak_pressure_plate", ItemUseFamily::WorldSpecial), ("dark_oak_sapling", ItemUseFamily::WorldSpecial),
    ("dark_oak_sign", ItemUseFamily::WorldSpecial), ("dark_oak_slab", ItemUseFamily::WorldSpecial), ("dark_oak_stairs", ItemUseFamily::WorldSpecial), ("dark_oak_trapdoor", ItemUseFamily::WorldSpecial),
    ("dark_oak_wood", ItemUseFamily::WorldSpecial), ("dark_prismarine", ItemUseFamily::WorldSpecial), ("dark_prismarine_slab", ItemUseFamily::WorldSpecial), ("dark_prismarine_stairs", ItemUseFamily::WorldSpecial),
    ("daylight_detector", ItemUseFamily::WorldSpecial), ("dead_brain_coral", ItemUseFamily::WorldSpecial), ("dead_brain_coral_block", ItemUseFamily::WorldSpecial), ("dead_brain_coral_fan", ItemUseFamily::WorldSpecial),
    ("dead_bubble_coral", ItemUseFamily::WorldSpecial), ("dead_bubble_coral_block", ItemUseFamily::WorldSpecial), ("dead_bubble_coral_fan", ItemUseFamily::WorldSpecial), ("dead_bush", ItemUseFamily::WorldSpecial),
    ("dead_fire_coral", ItemUseFamily::WorldSpecial), ("dead_fire_coral_block", ItemUseFamily::WorldSpecial), ("dead_fire_coral_fan", ItemUseFamily::WorldSpecial), ("dead_horn_coral", ItemUseFamily::WorldSpecial),
    ("dead_horn_coral_block", ItemUseFamily::WorldSpecial), ("dead_horn_coral_fan", ItemUseFamily::WorldSpecial), ("dead_tube_coral", ItemUseFamily::WorldSpecial), ("dead_tube_coral_block", ItemUseFamily::WorldSpecial),
    ("dead_tube_coral_fan", ItemUseFamily::WorldSpecial), ("debug_stick", ItemUseFamily::WorldSpecial), ("decorated_pot", ItemUseFamily::WorldSpecial), ("deepslate", ItemUseFamily::WorldSpecial),
    ("deepslate_brick_slab", ItemUseFamily::WorldSpecial), ("deepslate_brick_stairs", ItemUseFamily::WorldSpecial), ("deepslate_brick_wall", ItemUseFamily::WorldSpecial), ("deepslate_bricks", ItemUseFamily::WorldSpecial),
    ("deepslate_coal_ore", ItemUseFamily::WorldSpecial), ("deepslate_copper_ore", ItemUseFamily::WorldSpecial), ("deepslate_diamond_ore", ItemUseFamily::WorldSpecial), ("deepslate_emerald_ore", ItemUseFamily::WorldSpecial),
    ("deepslate_gold_ore", ItemUseFamily::WorldSpecial), ("deepslate_iron_ore", ItemUseFamily::WorldSpecial), ("deepslate_lapis_ore", ItemUseFamily::WorldSpecial), ("deepslate_redstone_ore", ItemUseFamily::WorldSpecial),
    ("deepslate_tile_slab", ItemUseFamily::WorldSpecial), ("deepslate_tile_stairs", ItemUseFamily::WorldSpecial), ("deepslate_tile_wall", ItemUseFamily::WorldSpecial), ("deepslate_tiles", ItemUseFamily::WorldSpecial),
    ("detector_rail", ItemUseFamily::WorldSpecial), ("diamond", ItemUseFamily::PureMaterial), ("diamond_axe", ItemUseFamily::ToolBlockTransform), ("diamond_block", ItemUseFamily::WorldSpecial),
    ("diamond_boots", ItemUseFamily::Equipable), ("diamond_chestplate", ItemUseFamily::Equipable), ("diamond_helmet", ItemUseFamily::Equipable), ("diamond_hoe", ItemUseFamily::ToolBlockTransform),
    ("diamond_horse_armor", ItemUseFamily::Equipable), ("diamond_leggings", ItemUseFamily::Equipable), ("diamond_ore", ItemUseFamily::WorldSpecial), ("diamond_pickaxe", ItemUseFamily::PureMaterial),
    ("diamond_shovel", ItemUseFamily::ToolBlockTransform), ("diamond_sword", ItemUseFamily::PureMaterial), ("diorite", ItemUseFamily::WorldSpecial), ("diorite_slab", ItemUseFamily::WorldSpecial),
    ("diorite_stairs", ItemUseFamily::WorldSpecial), ("diorite_wall", ItemUseFamily::WorldSpecial), ("dirt", ItemUseFamily::WorldSpecial), ("dirt_path", ItemUseFamily::WorldSpecial),
    ("disc_fragment_5", ItemUseFamily::PureMaterial), ("dispenser", ItemUseFamily::WorldSpecial), ("dolphin_spawn_egg", ItemUseFamily::EntityUse), ("donkey_spawn_egg", ItemUseFamily::EntityUse),
    ("dragon_breath", ItemUseFamily::PureMaterial), ("dragon_egg", ItemUseFamily::WorldSpecial), ("dragon_head", ItemUseFamily::Equipable), ("dried_kelp", ItemUseFamily::Consumable),
    ("dried_kelp_block", ItemUseFamily::WorldSpecial), ("dripstone_block", ItemUseFamily::WorldSpecial), ("dropper", ItemUseFamily::WorldSpecial), ("drowned_spawn_egg", ItemUseFamily::EntityUse),
    ("dune_armor_trim_smithing_template", ItemUseFamily::PureMaterial), ("echo_shard", ItemUseFamily::PureMaterial), ("egg", ItemUseFamily::RangedOrThrowable), ("elder_guardian_spawn_egg", ItemUseFamily::EntityUse),
    ("elytra", ItemUseFamily::Equipable), ("emerald", ItemUseFamily::PureMaterial), ("emerald_block", ItemUseFamily::WorldSpecial), ("emerald_ore", ItemUseFamily::WorldSpecial),
    ("enchanted_book", ItemUseFamily::PureMaterial), ("enchanted_golden_apple", ItemUseFamily::Consumable), ("enchanting_table", ItemUseFamily::WorldSpecial), ("end_crystal", ItemUseFamily::WorldSpecial),
    ("end_portal_frame", ItemUseFamily::WorldSpecial), ("end_rod", ItemUseFamily::WorldSpecial), ("end_stone", ItemUseFamily::WorldSpecial), ("end_stone_brick_slab", ItemUseFamily::WorldSpecial),
    ("end_stone_brick_stairs", ItemUseFamily::WorldSpecial), ("end_stone_brick_wall", ItemUseFamily::WorldSpecial), ("end_stone_bricks", ItemUseFamily::WorldSpecial), ("ender_chest", ItemUseFamily::WorldSpecial),
    ("ender_dragon_spawn_egg", ItemUseFamily::EntityUse), ("ender_eye", ItemUseFamily::RangedOrThrowable), ("ender_pearl", ItemUseFamily::RangedOrThrowable), ("enderman_spawn_egg", ItemUseFamily::EntityUse),
    ("endermite_spawn_egg", ItemUseFamily::EntityUse), ("evoker_spawn_egg", ItemUseFamily::EntityUse), ("experience_bottle", ItemUseFamily::RangedOrThrowable), ("explorer_pottery_sherd", ItemUseFamily::PureMaterial),
    ("exposed_chiseled_copper", ItemUseFamily::WorldSpecial), ("exposed_copper", ItemUseFamily::WorldSpecial), ("exposed_copper_bulb", ItemUseFamily::WorldSpecial), ("exposed_copper_door", ItemUseFamily::WorldSpecial),
    ("exposed_copper_grate", ItemUseFamily::WorldSpecial), ("exposed_copper_trapdoor", ItemUseFamily::WorldSpecial), ("exposed_cut_copper", ItemUseFamily::WorldSpecial), ("exposed_cut_copper_slab", ItemUseFamily::WorldSpecial),
    ("exposed_cut_copper_stairs", ItemUseFamily::WorldSpecial), ("eye_armor_trim_smithing_template", ItemUseFamily::PureMaterial), ("farmland", ItemUseFamily::WorldSpecial), ("feather", ItemUseFamily::PureMaterial),
    ("fermented_spider_eye", ItemUseFamily::PureMaterial), ("fern", ItemUseFamily::WorldSpecial), ("filled_map", ItemUseFamily::PureMaterial), ("fire_charge", ItemUseFamily::ToolBlockTransform),
    ("fire_coral", ItemUseFamily::WorldSpecial), ("fire_coral_block", ItemUseFamily::WorldSpecial), ("fire_coral_fan", ItemUseFamily::WorldSpecial), ("firework_rocket", ItemUseFamily::RangedOrThrowable),
    ("firework_star", ItemUseFamily::PureMaterial), ("fishing_rod", ItemUseFamily::RangedOrThrowable), ("fletching_table", ItemUseFamily::WorldSpecial), ("flint", ItemUseFamily::PureMaterial),
    ("flint_and_steel", ItemUseFamily::ToolBlockTransform), ("flow_armor_trim_smithing_template", ItemUseFamily::PureMaterial), ("flow_banner_pattern", ItemUseFamily::PureMaterial), ("flow_pottery_sherd", ItemUseFamily::PureMaterial),
    ("flower_banner_pattern", ItemUseFamily::PureMaterial), ("flower_pot", ItemUseFamily::WorldSpecial), ("flowering_azalea", ItemUseFamily::WorldSpecial), ("flowering_azalea_leaves", ItemUseFamily::WorldSpecial),
    ("fox_spawn_egg", ItemUseFamily::EntityUse), ("friend_pottery_sherd", ItemUseFamily::PureMaterial), ("frog_spawn_egg", ItemUseFamily::EntityUse), ("frogspawn", ItemUseFamily::WorldSpecial),
    ("furnace", ItemUseFamily::WorldSpecial), ("furnace_minecart", ItemUseFamily::WorldSpecial), ("ghast_spawn_egg", ItemUseFamily::EntityUse), ("ghast_tear", ItemUseFamily::PureMaterial),
    ("gilded_blackstone", ItemUseFamily::WorldSpecial), ("glass", ItemUseFamily::WorldSpecial), ("glass_bottle", ItemUseFamily::FluidContainer), ("glass_pane", ItemUseFamily::WorldSpecial),
    ("glistering_melon_slice", ItemUseFamily::PureMaterial), ("globe_banner_pattern", ItemUseFamily::PureMaterial), ("glow_berries", ItemUseFamily::Consumable), ("glow_ink_sac", ItemUseFamily::EntityUse),
    ("glow_item_frame", ItemUseFamily::WorldSpecial), ("glow_lichen", ItemUseFamily::WorldSpecial), ("glow_squid_spawn_egg", ItemUseFamily::EntityUse), ("glowstone", ItemUseFamily::WorldSpecial),
    ("glowstone_dust", ItemUseFamily::PureMaterial), ("goat_horn", ItemUseFamily::WorldSpecial), ("goat_spawn_egg", ItemUseFamily::EntityUse), ("gold_block", ItemUseFamily::WorldSpecial),
    ("gold_ingot", ItemUseFamily::PureMaterial), ("gold_nugget", ItemUseFamily::PureMaterial), ("gold_ore", ItemUseFamily::WorldSpecial), ("golden_apple", ItemUseFamily::Consumable),
    ("golden_axe", ItemUseFamily::ToolBlockTransform), ("golden_boots", ItemUseFamily::Equipable), ("golden_carrot", ItemUseFamily::Consumable), ("golden_chestplate", ItemUseFamily::Equipable),
    ("golden_helmet", ItemUseFamily::Equipable), ("golden_hoe", ItemUseFamily::ToolBlockTransform), ("golden_horse_armor", ItemUseFamily::Equipable), ("golden_leggings", ItemUseFamily::Equipable),
    ("golden_pickaxe", ItemUseFamily::PureMaterial), ("golden_shovel", ItemUseFamily::ToolBlockTransform), ("golden_sword", ItemUseFamily::PureMaterial), ("granite", ItemUseFamily::WorldSpecial),
    ("granite_slab", ItemUseFamily::WorldSpecial), ("granite_stairs", ItemUseFamily::WorldSpecial), ("granite_wall", ItemUseFamily::WorldSpecial), ("grass_block", ItemUseFamily::WorldSpecial),
    ("gravel", ItemUseFamily::WorldSpecial), ("gray_banner", ItemUseFamily::WorldSpecial), ("gray_bed", ItemUseFamily::WorldSpecial), ("gray_candle", ItemUseFamily::WorldSpecial),
    ("gray_carpet", ItemUseFamily::WorldSpecial), ("gray_concrete", ItemUseFamily::WorldSpecial), ("gray_concrete_powder", ItemUseFamily::WorldSpecial), ("gray_dye", ItemUseFamily::EntityUse),
    ("gray_glazed_terracotta", ItemUseFamily::WorldSpecial), ("gray_shulker_box", ItemUseFamily::WorldSpecial), ("gray_stained_glass", ItemUseFamily::WorldSpecial), ("gray_stained_glass_pane", ItemUseFamily::WorldSpecial),
    ("gray_terracotta", ItemUseFamily::WorldSpecial), ("gray_wool", ItemUseFamily::WorldSpecial), ("green_banner", ItemUseFamily::WorldSpecial), ("green_bed", ItemUseFamily::WorldSpecial),
    ("green_candle", ItemUseFamily::WorldSpecial), ("green_carpet", ItemUseFamily::WorldSpecial), ("green_concrete", ItemUseFamily::WorldSpecial), ("green_concrete_powder", ItemUseFamily::WorldSpecial),
    ("green_dye", ItemUseFamily::EntityUse), ("green_glazed_terracotta", ItemUseFamily::WorldSpecial), ("green_shulker_box", ItemUseFamily::WorldSpecial), ("green_stained_glass", ItemUseFamily::WorldSpecial),
    ("green_stained_glass_pane", ItemUseFamily::WorldSpecial), ("green_terracotta", ItemUseFamily::WorldSpecial), ("green_wool", ItemUseFamily::WorldSpecial), ("grindstone", ItemUseFamily::WorldSpecial),
    ("guardian_spawn_egg", ItemUseFamily::EntityUse), ("gunpowder", ItemUseFamily::PureMaterial), ("guster_banner_pattern", ItemUseFamily::PureMaterial), ("guster_pottery_sherd", ItemUseFamily::PureMaterial),
    ("hanging_roots", ItemUseFamily::WorldSpecial), ("hay_block", ItemUseFamily::WorldSpecial), ("heart_of_the_sea", ItemUseFamily::PureMaterial), ("heart_pottery_sherd", ItemUseFamily::PureMaterial),
    ("heartbreak_pottery_sherd", ItemUseFamily::PureMaterial), ("heavy_core", ItemUseFamily::WorldSpecial), ("heavy_weighted_pressure_plate", ItemUseFamily::WorldSpecial), ("hoglin_spawn_egg", ItemUseFamily::EntityUse),
    ("honey_block", ItemUseFamily::WorldSpecial), ("honey_bottle", ItemUseFamily::Consumable), ("honeycomb", ItemUseFamily::ToolBlockTransform), ("honeycomb_block", ItemUseFamily::WorldSpecial),
    ("hopper", ItemUseFamily::WorldSpecial), ("hopper_minecart", ItemUseFamily::WorldSpecial), ("horn_coral", ItemUseFamily::WorldSpecial), ("horn_coral_block", ItemUseFamily::WorldSpecial),
    ("horn_coral_fan", ItemUseFamily::WorldSpecial), ("horse_spawn_egg", ItemUseFamily::EntityUse), ("host_armor_trim_smithing_template", ItemUseFamily::PureMaterial), ("howl_pottery_sherd", ItemUseFamily::PureMaterial),
    ("husk_spawn_egg", ItemUseFamily::EntityUse), ("ice", ItemUseFamily::WorldSpecial), ("infested_chiseled_stone_bricks", ItemUseFamily::WorldSpecial), ("infested_cobblestone", ItemUseFamily::WorldSpecial),
    ("infested_cracked_stone_bricks", ItemUseFamily::WorldSpecial), ("infested_deepslate", ItemUseFamily::WorldSpecial), ("infested_mossy_stone_bricks", ItemUseFamily::WorldSpecial), ("infested_stone", ItemUseFamily::WorldSpecial),
    ("infested_stone_bricks", ItemUseFamily::WorldSpecial), ("ink_sac", ItemUseFamily::EntityUse), ("iron_axe", ItemUseFamily::ToolBlockTransform), ("iron_bars", ItemUseFamily::WorldSpecial),
    ("iron_block", ItemUseFamily::WorldSpecial), ("iron_boots", ItemUseFamily::Equipable), ("iron_chestplate", ItemUseFamily::Equipable), ("iron_door", ItemUseFamily::WorldSpecial),
    ("iron_golem_spawn_egg", ItemUseFamily::EntityUse), ("iron_helmet", ItemUseFamily::Equipable), ("iron_hoe", ItemUseFamily::ToolBlockTransform), ("iron_horse_armor", ItemUseFamily::Equipable),
    ("iron_ingot", ItemUseFamily::PureMaterial), ("iron_leggings", ItemUseFamily::Equipable), ("iron_nugget", ItemUseFamily::PureMaterial), ("iron_ore", ItemUseFamily::WorldSpecial),
    ("iron_pickaxe", ItemUseFamily::PureMaterial), ("iron_shovel", ItemUseFamily::ToolBlockTransform), ("iron_sword", ItemUseFamily::PureMaterial), ("iron_trapdoor", ItemUseFamily::WorldSpecial),
    ("item_frame", ItemUseFamily::WorldSpecial), ("jack_o_lantern", ItemUseFamily::WorldSpecial), ("jigsaw", ItemUseFamily::WorldSpecial), ("jukebox", ItemUseFamily::WorldSpecial),
    ("jungle_boat", ItemUseFamily::WorldSpecial), ("jungle_button", ItemUseFamily::WorldSpecial), ("jungle_chest_boat", ItemUseFamily::WorldSpecial), ("jungle_door", ItemUseFamily::WorldSpecial),
    ("jungle_fence", ItemUseFamily::WorldSpecial), ("jungle_fence_gate", ItemUseFamily::WorldSpecial), ("jungle_hanging_sign", ItemUseFamily::WorldSpecial), ("jungle_leaves", ItemUseFamily::WorldSpecial),
    ("jungle_log", ItemUseFamily::WorldSpecial), ("jungle_planks", ItemUseFamily::WorldSpecial), ("jungle_pressure_plate", ItemUseFamily::WorldSpecial), ("jungle_sapling", ItemUseFamily::WorldSpecial),
    ("jungle_sign", ItemUseFamily::WorldSpecial), ("jungle_slab", ItemUseFamily::WorldSpecial), ("jungle_stairs", ItemUseFamily::WorldSpecial), ("jungle_trapdoor", ItemUseFamily::WorldSpecial),
    ("jungle_wood", ItemUseFamily::WorldSpecial), ("kelp", ItemUseFamily::WorldSpecial), ("knowledge_book", ItemUseFamily::WorldSpecial), ("ladder", ItemUseFamily::WorldSpecial),
    ("lantern", ItemUseFamily::WorldSpecial), ("lapis_block", ItemUseFamily::WorldSpecial), ("lapis_lazuli", ItemUseFamily::PureMaterial), ("lapis_ore", ItemUseFamily::WorldSpecial),
    ("large_amethyst_bud", ItemUseFamily::WorldSpecial), ("large_fern", ItemUseFamily::WorldSpecial), ("lava_bucket", ItemUseFamily::FluidContainer), ("lead", ItemUseFamily::EntityUse),
    ("leather", ItemUseFamily::PureMaterial), ("leather_boots", ItemUseFamily::Equipable), ("leather_chestplate", ItemUseFamily::Equipable), ("leather_helmet", ItemUseFamily::Equipable),
    ("leather_horse_armor", ItemUseFamily::Equipable), ("leather_leggings", ItemUseFamily::Equipable), ("lectern", ItemUseFamily::WorldSpecial), ("lever", ItemUseFamily::WorldSpecial),
    ("light", ItemUseFamily::WorldSpecial), ("light_blue_banner", ItemUseFamily::WorldSpecial), ("light_blue_bed", ItemUseFamily::WorldSpecial), ("light_blue_candle", ItemUseFamily::WorldSpecial),
    ("light_blue_carpet", ItemUseFamily::WorldSpecial), ("light_blue_concrete", ItemUseFamily::WorldSpecial), ("light_blue_concrete_powder", ItemUseFamily::WorldSpecial), ("light_blue_dye", ItemUseFamily::EntityUse),
    ("light_blue_glazed_terracotta", ItemUseFamily::WorldSpecial), ("light_blue_shulker_box", ItemUseFamily::WorldSpecial), ("light_blue_stained_glass", ItemUseFamily::WorldSpecial), ("light_blue_stained_glass_pane", ItemUseFamily::WorldSpecial),
    ("light_blue_terracotta", ItemUseFamily::WorldSpecial), ("light_blue_wool", ItemUseFamily::WorldSpecial), ("light_gray_banner", ItemUseFamily::WorldSpecial), ("light_gray_bed", ItemUseFamily::WorldSpecial),
    ("light_gray_candle", ItemUseFamily::WorldSpecial), ("light_gray_carpet", ItemUseFamily::WorldSpecial), ("light_gray_concrete", ItemUseFamily::WorldSpecial), ("light_gray_concrete_powder", ItemUseFamily::WorldSpecial),
    ("light_gray_dye", ItemUseFamily::EntityUse), ("light_gray_glazed_terracotta", ItemUseFamily::WorldSpecial), ("light_gray_shulker_box", ItemUseFamily::WorldSpecial), ("light_gray_stained_glass", ItemUseFamily::WorldSpecial),
    ("light_gray_stained_glass_pane", ItemUseFamily::WorldSpecial), ("light_gray_terracotta", ItemUseFamily::WorldSpecial), ("light_gray_wool", ItemUseFamily::WorldSpecial), ("light_weighted_pressure_plate", ItemUseFamily::WorldSpecial),
    ("lightning_rod", ItemUseFamily::WorldSpecial), ("lilac", ItemUseFamily::WorldSpecial), ("lily_of_the_valley", ItemUseFamily::WorldSpecial), ("lily_pad", ItemUseFamily::WorldSpecial),
    ("lime_banner", ItemUseFamily::WorldSpecial), ("lime_bed", ItemUseFamily::WorldSpecial), ("lime_candle", ItemUseFamily::WorldSpecial), ("lime_carpet", ItemUseFamily::WorldSpecial),
    ("lime_concrete", ItemUseFamily::WorldSpecial), ("lime_concrete_powder", ItemUseFamily::WorldSpecial), ("lime_dye", ItemUseFamily::EntityUse), ("lime_glazed_terracotta", ItemUseFamily::WorldSpecial),
    ("lime_shulker_box", ItemUseFamily::WorldSpecial), ("lime_stained_glass", ItemUseFamily::WorldSpecial), ("lime_stained_glass_pane", ItemUseFamily::WorldSpecial), ("lime_terracotta", ItemUseFamily::WorldSpecial),
    ("lime_wool", ItemUseFamily::WorldSpecial), ("lingering_potion", ItemUseFamily::RangedOrThrowable), ("llama_spawn_egg", ItemUseFamily::EntityUse), ("lodestone", ItemUseFamily::WorldSpecial),
    ("loom", ItemUseFamily::WorldSpecial), ("mace", ItemUseFamily::PureMaterial), ("magenta_banner", ItemUseFamily::WorldSpecial), ("magenta_bed", ItemUseFamily::WorldSpecial),
    ("magenta_candle", ItemUseFamily::WorldSpecial), ("magenta_carpet", ItemUseFamily::WorldSpecial), ("magenta_concrete", ItemUseFamily::WorldSpecial), ("magenta_concrete_powder", ItemUseFamily::WorldSpecial),
    ("magenta_dye", ItemUseFamily::EntityUse), ("magenta_glazed_terracotta", ItemUseFamily::WorldSpecial), ("magenta_shulker_box", ItemUseFamily::WorldSpecial), ("magenta_stained_glass", ItemUseFamily::WorldSpecial),
    ("magenta_stained_glass_pane", ItemUseFamily::WorldSpecial), ("magenta_terracotta", ItemUseFamily::WorldSpecial), ("magenta_wool", ItemUseFamily::WorldSpecial), ("magma_block", ItemUseFamily::WorldSpecial),
    ("magma_cream", ItemUseFamily::PureMaterial), ("magma_cube_spawn_egg", ItemUseFamily::EntityUse), ("mangrove_boat", ItemUseFamily::WorldSpecial), ("mangrove_button", ItemUseFamily::WorldSpecial),
    ("mangrove_chest_boat", ItemUseFamily::WorldSpecial), ("mangrove_door", ItemUseFamily::WorldSpecial), ("mangrove_fence", ItemUseFamily::WorldSpecial), ("mangrove_fence_gate", ItemUseFamily::WorldSpecial),
    ("mangrove_hanging_sign", ItemUseFamily::WorldSpecial), ("mangrove_leaves", ItemUseFamily::WorldSpecial), ("mangrove_log", ItemUseFamily::WorldSpecial), ("mangrove_planks", ItemUseFamily::WorldSpecial),
    ("mangrove_pressure_plate", ItemUseFamily::WorldSpecial), ("mangrove_propagule", ItemUseFamily::WorldSpecial), ("mangrove_roots", ItemUseFamily::WorldSpecial), ("mangrove_sign", ItemUseFamily::WorldSpecial),
    ("mangrove_slab", ItemUseFamily::WorldSpecial), ("mangrove_stairs", ItemUseFamily::WorldSpecial), ("mangrove_trapdoor", ItemUseFamily::WorldSpecial), ("mangrove_wood", ItemUseFamily::WorldSpecial),
    ("map", ItemUseFamily::WorldSpecial), ("medium_amethyst_bud", ItemUseFamily::WorldSpecial), ("melon", ItemUseFamily::WorldSpecial), ("melon_seeds", ItemUseFamily::WorldSpecial),
    ("melon_slice", ItemUseFamily::Consumable), ("milk_bucket", ItemUseFamily::FluidContainer), ("minecart", ItemUseFamily::WorldSpecial), ("miner_pottery_sherd", ItemUseFamily::PureMaterial),
    ("mojang_banner_pattern", ItemUseFamily::PureMaterial), ("mooshroom_spawn_egg", ItemUseFamily::EntityUse), ("moss_block", ItemUseFamily::WorldSpecial), ("moss_carpet", ItemUseFamily::WorldSpecial),
    ("mossy_cobblestone", ItemUseFamily::WorldSpecial), ("mossy_cobblestone_slab", ItemUseFamily::WorldSpecial), ("mossy_cobblestone_stairs", ItemUseFamily::WorldSpecial), ("mossy_cobblestone_wall", ItemUseFamily::WorldSpecial),
    ("mossy_stone_brick_slab", ItemUseFamily::WorldSpecial), ("mossy_stone_brick_stairs", ItemUseFamily::WorldSpecial), ("mossy_stone_brick_wall", ItemUseFamily::WorldSpecial), ("mossy_stone_bricks", ItemUseFamily::WorldSpecial),
    ("mourner_pottery_sherd", ItemUseFamily::PureMaterial), ("mud", ItemUseFamily::WorldSpecial), ("mud_brick_slab", ItemUseFamily::WorldSpecial), ("mud_brick_stairs", ItemUseFamily::WorldSpecial),
    ("mud_brick_wall", ItemUseFamily::WorldSpecial), ("mud_bricks", ItemUseFamily::WorldSpecial), ("muddy_mangrove_roots", ItemUseFamily::WorldSpecial), ("mule_spawn_egg", ItemUseFamily::EntityUse),
    ("mushroom_stem", ItemUseFamily::WorldSpecial), ("mushroom_stew", ItemUseFamily::Consumable), ("music_disc_11", ItemUseFamily::WorldSpecial), ("music_disc_13", ItemUseFamily::WorldSpecial),
    ("music_disc_5", ItemUseFamily::WorldSpecial), ("music_disc_blocks", ItemUseFamily::WorldSpecial), ("music_disc_cat", ItemUseFamily::WorldSpecial), ("music_disc_chirp", ItemUseFamily::WorldSpecial),
    ("music_disc_creator", ItemUseFamily::WorldSpecial), ("music_disc_creator_music_box", ItemUseFamily::WorldSpecial), ("music_disc_far", ItemUseFamily::WorldSpecial), ("music_disc_mall", ItemUseFamily::WorldSpecial),
    ("music_disc_mellohi", ItemUseFamily::WorldSpecial), ("music_disc_otherside", ItemUseFamily::WorldSpecial), ("music_disc_pigstep", ItemUseFamily::WorldSpecial), ("music_disc_precipice", ItemUseFamily::WorldSpecial),
    ("music_disc_relic", ItemUseFamily::WorldSpecial), ("music_disc_stal", ItemUseFamily::WorldSpecial), ("music_disc_strad", ItemUseFamily::WorldSpecial), ("music_disc_wait", ItemUseFamily::WorldSpecial),
    ("music_disc_ward", ItemUseFamily::WorldSpecial), ("mutton", ItemUseFamily::Consumable), ("mycelium", ItemUseFamily::WorldSpecial), ("name_tag", ItemUseFamily::EntityUse),
    ("nautilus_shell", ItemUseFamily::PureMaterial), ("nether_brick", ItemUseFamily::PureMaterial), ("nether_brick_fence", ItemUseFamily::WorldSpecial), ("nether_brick_slab", ItemUseFamily::WorldSpecial),
    ("nether_brick_stairs", ItemUseFamily::WorldSpecial), ("nether_brick_wall", ItemUseFamily::WorldSpecial), ("nether_bricks", ItemUseFamily::WorldSpecial), ("nether_gold_ore", ItemUseFamily::WorldSpecial),
    ("nether_quartz_ore", ItemUseFamily::WorldSpecial), ("nether_sprouts", ItemUseFamily::WorldSpecial), ("nether_star", ItemUseFamily::PureMaterial), ("nether_wart", ItemUseFamily::WorldSpecial),
    ("nether_wart_block", ItemUseFamily::WorldSpecial), ("netherite_axe", ItemUseFamily::ToolBlockTransform), ("netherite_block", ItemUseFamily::WorldSpecial), ("netherite_boots", ItemUseFamily::Equipable),
    ("netherite_chestplate", ItemUseFamily::Equipable), ("netherite_helmet", ItemUseFamily::Equipable), ("netherite_hoe", ItemUseFamily::ToolBlockTransform), ("netherite_ingot", ItemUseFamily::PureMaterial),
    ("netherite_leggings", ItemUseFamily::Equipable), ("netherite_pickaxe", ItemUseFamily::PureMaterial), ("netherite_scrap", ItemUseFamily::PureMaterial), ("netherite_shovel", ItemUseFamily::ToolBlockTransform),
    ("netherite_sword", ItemUseFamily::PureMaterial), ("netherite_upgrade_smithing_template", ItemUseFamily::PureMaterial), ("netherrack", ItemUseFamily::WorldSpecial), ("note_block", ItemUseFamily::WorldSpecial),
    ("oak_boat", ItemUseFamily::WorldSpecial), ("oak_button", ItemUseFamily::WorldSpecial), ("oak_chest_boat", ItemUseFamily::WorldSpecial), ("oak_door", ItemUseFamily::WorldSpecial),
    ("oak_fence", ItemUseFamily::WorldSpecial), ("oak_fence_gate", ItemUseFamily::WorldSpecial), ("oak_hanging_sign", ItemUseFamily::WorldSpecial), ("oak_leaves", ItemUseFamily::WorldSpecial),
    ("oak_log", ItemUseFamily::WorldSpecial), ("oak_planks", ItemUseFamily::WorldSpecial), ("oak_pressure_plate", ItemUseFamily::WorldSpecial), ("oak_sapling", ItemUseFamily::WorldSpecial),
    ("oak_sign", ItemUseFamily::WorldSpecial), ("oak_slab", ItemUseFamily::WorldSpecial), ("oak_stairs", ItemUseFamily::WorldSpecial), ("oak_trapdoor", ItemUseFamily::WorldSpecial),
    ("oak_wood", ItemUseFamily::WorldSpecial), ("observer", ItemUseFamily::WorldSpecial), ("obsidian", ItemUseFamily::WorldSpecial), ("ocelot_spawn_egg", ItemUseFamily::EntityUse),
    ("ochre_froglight", ItemUseFamily::WorldSpecial), ("ominous_bottle", ItemUseFamily::Consumable), ("ominous_trial_key", ItemUseFamily::WorldSpecial), ("orange_banner", ItemUseFamily::WorldSpecial),
    ("orange_bed", ItemUseFamily::WorldSpecial), ("orange_candle", ItemUseFamily::WorldSpecial), ("orange_carpet", ItemUseFamily::WorldSpecial), ("orange_concrete", ItemUseFamily::WorldSpecial),
    ("orange_concrete_powder", ItemUseFamily::WorldSpecial), ("orange_dye", ItemUseFamily::EntityUse), ("orange_glazed_terracotta", ItemUseFamily::WorldSpecial), ("orange_shulker_box", ItemUseFamily::WorldSpecial),
    ("orange_stained_glass", ItemUseFamily::WorldSpecial), ("orange_stained_glass_pane", ItemUseFamily::WorldSpecial), ("orange_terracotta", ItemUseFamily::WorldSpecial), ("orange_tulip", ItemUseFamily::WorldSpecial),
    ("orange_wool", ItemUseFamily::WorldSpecial), ("oxeye_daisy", ItemUseFamily::WorldSpecial), ("oxidized_chiseled_copper", ItemUseFamily::WorldSpecial), ("oxidized_copper", ItemUseFamily::WorldSpecial),
    ("oxidized_copper_bulb", ItemUseFamily::WorldSpecial), ("oxidized_copper_door", ItemUseFamily::WorldSpecial), ("oxidized_copper_grate", ItemUseFamily::WorldSpecial), ("oxidized_copper_trapdoor", ItemUseFamily::WorldSpecial),
    ("oxidized_cut_copper", ItemUseFamily::WorldSpecial), ("oxidized_cut_copper_slab", ItemUseFamily::WorldSpecial), ("oxidized_cut_copper_stairs", ItemUseFamily::WorldSpecial), ("packed_ice", ItemUseFamily::WorldSpecial),
    ("packed_mud", ItemUseFamily::WorldSpecial), ("painting", ItemUseFamily::WorldSpecial), ("panda_spawn_egg", ItemUseFamily::EntityUse), ("paper", ItemUseFamily::PureMaterial),
    ("parrot_spawn_egg", ItemUseFamily::EntityUse), ("pearlescent_froglight", ItemUseFamily::WorldSpecial), ("peony", ItemUseFamily::WorldSpecial), ("petrified_oak_slab", ItemUseFamily::WorldSpecial),
    ("phantom_membrane", ItemUseFamily::PureMaterial), ("phantom_spawn_egg", ItemUseFamily::EntityUse), ("pig_spawn_egg", ItemUseFamily::EntityUse), ("piglin_banner_pattern", ItemUseFamily::PureMaterial),
    ("piglin_brute_spawn_egg", ItemUseFamily::EntityUse), ("piglin_head", ItemUseFamily::Equipable), ("piglin_spawn_egg", ItemUseFamily::EntityUse), ("pillager_spawn_egg", ItemUseFamily::EntityUse),
    ("pink_banner", ItemUseFamily::WorldSpecial), ("pink_bed", ItemUseFamily::WorldSpecial), ("pink_candle", ItemUseFamily::WorldSpecial), ("pink_carpet", ItemUseFamily::WorldSpecial),
    ("pink_concrete", ItemUseFamily::WorldSpecial), ("pink_concrete_powder", ItemUseFamily::WorldSpecial), ("pink_dye", ItemUseFamily::EntityUse), ("pink_glazed_terracotta", ItemUseFamily::WorldSpecial),
    ("pink_petals", ItemUseFamily::WorldSpecial), ("pink_shulker_box", ItemUseFamily::WorldSpecial), ("pink_stained_glass", ItemUseFamily::WorldSpecial), ("pink_stained_glass_pane", ItemUseFamily::WorldSpecial),
    ("pink_terracotta", ItemUseFamily::WorldSpecial), ("pink_tulip", ItemUseFamily::WorldSpecial), ("pink_wool", ItemUseFamily::WorldSpecial), ("piston", ItemUseFamily::WorldSpecial),
    ("pitcher_plant", ItemUseFamily::WorldSpecial), ("pitcher_pod", ItemUseFamily::WorldSpecial), ("player_head", ItemUseFamily::Equipable), ("plenty_pottery_sherd", ItemUseFamily::PureMaterial),
    ("podzol", ItemUseFamily::WorldSpecial), ("pointed_dripstone", ItemUseFamily::WorldSpecial), ("poisonous_potato", ItemUseFamily::Consumable), ("polar_bear_spawn_egg", ItemUseFamily::EntityUse),
    ("polished_andesite", ItemUseFamily::WorldSpecial), ("polished_andesite_slab", ItemUseFamily::WorldSpecial), ("polished_andesite_stairs", ItemUseFamily::WorldSpecial), ("polished_basalt", ItemUseFamily::WorldSpecial),
    ("polished_blackstone", ItemUseFamily::WorldSpecial), ("polished_blackstone_brick_slab", ItemUseFamily::WorldSpecial), ("polished_blackstone_brick_stairs", ItemUseFamily::WorldSpecial), ("polished_blackstone_brick_wall", ItemUseFamily::WorldSpecial),
    ("polished_blackstone_bricks", ItemUseFamily::WorldSpecial), ("polished_blackstone_button", ItemUseFamily::WorldSpecial), ("polished_blackstone_pressure_plate", ItemUseFamily::WorldSpecial), ("polished_blackstone_slab", ItemUseFamily::WorldSpecial),
    ("polished_blackstone_stairs", ItemUseFamily::WorldSpecial), ("polished_blackstone_wall", ItemUseFamily::WorldSpecial), ("polished_deepslate", ItemUseFamily::WorldSpecial), ("polished_deepslate_slab", ItemUseFamily::WorldSpecial),
    ("polished_deepslate_stairs", ItemUseFamily::WorldSpecial), ("polished_deepslate_wall", ItemUseFamily::WorldSpecial), ("polished_diorite", ItemUseFamily::WorldSpecial), ("polished_diorite_slab", ItemUseFamily::WorldSpecial),
    ("polished_diorite_stairs", ItemUseFamily::WorldSpecial), ("polished_granite", ItemUseFamily::WorldSpecial), ("polished_granite_slab", ItemUseFamily::WorldSpecial), ("polished_granite_stairs", ItemUseFamily::WorldSpecial),
    ("polished_tuff", ItemUseFamily::WorldSpecial), ("polished_tuff_slab", ItemUseFamily::WorldSpecial), ("polished_tuff_stairs", ItemUseFamily::WorldSpecial), ("polished_tuff_wall", ItemUseFamily::WorldSpecial),
    ("popped_chorus_fruit", ItemUseFamily::PureMaterial), ("poppy", ItemUseFamily::WorldSpecial), ("porkchop", ItemUseFamily::Consumable), ("potato", ItemUseFamily::Consumable),
    ("potion", ItemUseFamily::Consumable), ("powder_snow_bucket", ItemUseFamily::FluidContainer), ("powered_rail", ItemUseFamily::WorldSpecial), ("prismarine", ItemUseFamily::WorldSpecial),
    ("prismarine_brick_slab", ItemUseFamily::WorldSpecial), ("prismarine_brick_stairs", ItemUseFamily::WorldSpecial), ("prismarine_bricks", ItemUseFamily::WorldSpecial), ("prismarine_crystals", ItemUseFamily::PureMaterial),
    ("prismarine_shard", ItemUseFamily::PureMaterial), ("prismarine_slab", ItemUseFamily::WorldSpecial), ("prismarine_stairs", ItemUseFamily::WorldSpecial), ("prismarine_wall", ItemUseFamily::WorldSpecial),
    ("prize_pottery_sherd", ItemUseFamily::PureMaterial), ("pufferfish", ItemUseFamily::Consumable), ("pufferfish_bucket", ItemUseFamily::FluidContainer), ("pufferfish_spawn_egg", ItemUseFamily::EntityUse),
    ("pumpkin", ItemUseFamily::WorldSpecial), ("pumpkin_pie", ItemUseFamily::Consumable), ("pumpkin_seeds", ItemUseFamily::WorldSpecial), ("purple_banner", ItemUseFamily::WorldSpecial),
    ("purple_bed", ItemUseFamily::WorldSpecial), ("purple_candle", ItemUseFamily::WorldSpecial), ("purple_carpet", ItemUseFamily::WorldSpecial), ("purple_concrete", ItemUseFamily::WorldSpecial),
    ("purple_concrete_powder", ItemUseFamily::WorldSpecial), ("purple_dye", ItemUseFamily::EntityUse), ("purple_glazed_terracotta", ItemUseFamily::WorldSpecial), ("purple_shulker_box", ItemUseFamily::WorldSpecial),
    ("purple_stained_glass", ItemUseFamily::WorldSpecial), ("purple_stained_glass_pane", ItemUseFamily::WorldSpecial), ("purple_terracotta", ItemUseFamily::WorldSpecial), ("purple_wool", ItemUseFamily::WorldSpecial),
    ("purpur_block", ItemUseFamily::WorldSpecial), ("purpur_pillar", ItemUseFamily::WorldSpecial), ("purpur_slab", ItemUseFamily::WorldSpecial), ("purpur_stairs", ItemUseFamily::WorldSpecial),
    ("quartz", ItemUseFamily::PureMaterial), ("quartz_block", ItemUseFamily::WorldSpecial), ("quartz_bricks", ItemUseFamily::WorldSpecial), ("quartz_pillar", ItemUseFamily::WorldSpecial),
    ("quartz_slab", ItemUseFamily::WorldSpecial), ("quartz_stairs", ItemUseFamily::WorldSpecial), ("rabbit", ItemUseFamily::Consumable), ("rabbit_foot", ItemUseFamily::PureMaterial),
    ("rabbit_hide", ItemUseFamily::PureMaterial), ("rabbit_spawn_egg", ItemUseFamily::EntityUse), ("rabbit_stew", ItemUseFamily::Consumable), ("rail", ItemUseFamily::WorldSpecial),
    ("raiser_armor_trim_smithing_template", ItemUseFamily::PureMaterial), ("ravager_spawn_egg", ItemUseFamily::EntityUse), ("raw_copper", ItemUseFamily::PureMaterial), ("raw_copper_block", ItemUseFamily::WorldSpecial),
    ("raw_gold", ItemUseFamily::PureMaterial), ("raw_gold_block", ItemUseFamily::WorldSpecial), ("raw_iron", ItemUseFamily::PureMaterial), ("raw_iron_block", ItemUseFamily::WorldSpecial),
    ("recovery_compass", ItemUseFamily::PureMaterial), ("red_banner", ItemUseFamily::WorldSpecial), ("red_bed", ItemUseFamily::WorldSpecial), ("red_candle", ItemUseFamily::WorldSpecial),
    ("red_carpet", ItemUseFamily::WorldSpecial), ("red_concrete", ItemUseFamily::WorldSpecial), ("red_concrete_powder", ItemUseFamily::WorldSpecial), ("red_dye", ItemUseFamily::EntityUse),
    ("red_glazed_terracotta", ItemUseFamily::WorldSpecial), ("red_mushroom", ItemUseFamily::WorldSpecial), ("red_mushroom_block", ItemUseFamily::WorldSpecial), ("red_nether_brick_slab", ItemUseFamily::WorldSpecial),
    ("red_nether_brick_stairs", ItemUseFamily::WorldSpecial), ("red_nether_brick_wall", ItemUseFamily::WorldSpecial), ("red_nether_bricks", ItemUseFamily::WorldSpecial), ("red_sand", ItemUseFamily::WorldSpecial),
    ("red_sandstone", ItemUseFamily::WorldSpecial), ("red_sandstone_slab", ItemUseFamily::WorldSpecial), ("red_sandstone_stairs", ItemUseFamily::WorldSpecial), ("red_sandstone_wall", ItemUseFamily::WorldSpecial),
    ("red_shulker_box", ItemUseFamily::WorldSpecial), ("red_stained_glass", ItemUseFamily::WorldSpecial), ("red_stained_glass_pane", ItemUseFamily::WorldSpecial), ("red_terracotta", ItemUseFamily::WorldSpecial),
    ("red_tulip", ItemUseFamily::WorldSpecial), ("red_wool", ItemUseFamily::WorldSpecial), ("redstone", ItemUseFamily::WorldSpecial), ("redstone_block", ItemUseFamily::WorldSpecial),
    ("redstone_lamp", ItemUseFamily::WorldSpecial), ("redstone_ore", ItemUseFamily::WorldSpecial), ("redstone_torch", ItemUseFamily::WorldSpecial), ("reinforced_deepslate", ItemUseFamily::WorldSpecial),
    ("repeater", ItemUseFamily::WorldSpecial), ("repeating_command_block", ItemUseFamily::WorldSpecial), ("respawn_anchor", ItemUseFamily::WorldSpecial), ("rib_armor_trim_smithing_template", ItemUseFamily::PureMaterial),
    ("rooted_dirt", ItemUseFamily::WorldSpecial), ("rose_bush", ItemUseFamily::WorldSpecial), ("rotten_flesh", ItemUseFamily::Consumable), ("saddle", ItemUseFamily::EntityUse),
    ("salmon", ItemUseFamily::Consumable), ("salmon_bucket", ItemUseFamily::FluidContainer), ("salmon_spawn_egg", ItemUseFamily::EntityUse), ("sand", ItemUseFamily::WorldSpecial),
    ("sandstone", ItemUseFamily::WorldSpecial), ("sandstone_slab", ItemUseFamily::WorldSpecial), ("sandstone_stairs", ItemUseFamily::WorldSpecial), ("sandstone_wall", ItemUseFamily::WorldSpecial),
    ("scaffolding", ItemUseFamily::WorldSpecial), ("scrape_pottery_sherd", ItemUseFamily::PureMaterial), ("sculk", ItemUseFamily::WorldSpecial), ("sculk_catalyst", ItemUseFamily::WorldSpecial),
    ("sculk_sensor", ItemUseFamily::WorldSpecial), ("sculk_shrieker", ItemUseFamily::WorldSpecial), ("sculk_vein", ItemUseFamily::WorldSpecial), ("sea_lantern", ItemUseFamily::WorldSpecial),
    ("sea_pickle", ItemUseFamily::WorldSpecial), ("seagrass", ItemUseFamily::WorldSpecial), ("sentry_armor_trim_smithing_template", ItemUseFamily::PureMaterial), ("shaper_armor_trim_smithing_template", ItemUseFamily::PureMaterial),
    ("sheaf_pottery_sherd", ItemUseFamily::PureMaterial), ("shears", ItemUseFamily::ToolBlockTransform), ("sheep_spawn_egg", ItemUseFamily::EntityUse), ("shelter_pottery_sherd", ItemUseFamily::PureMaterial),
    ("shield", ItemUseFamily::Equipable), ("short_grass", ItemUseFamily::WorldSpecial), ("shroomlight", ItemUseFamily::WorldSpecial), ("shulker_box", ItemUseFamily::WorldSpecial),
    ("shulker_shell", ItemUseFamily::PureMaterial), ("shulker_spawn_egg", ItemUseFamily::EntityUse), ("silence_armor_trim_smithing_template", ItemUseFamily::PureMaterial), ("silverfish_spawn_egg", ItemUseFamily::EntityUse),
    ("skeleton_horse_spawn_egg", ItemUseFamily::EntityUse), ("skeleton_skull", ItemUseFamily::Equipable), ("skeleton_spawn_egg", ItemUseFamily::EntityUse), ("skull_banner_pattern", ItemUseFamily::PureMaterial),
    ("skull_pottery_sherd", ItemUseFamily::PureMaterial), ("slime_ball", ItemUseFamily::PureMaterial), ("slime_block", ItemUseFamily::WorldSpecial), ("slime_spawn_egg", ItemUseFamily::EntityUse),
    ("small_amethyst_bud", ItemUseFamily::WorldSpecial), ("small_dripleaf", ItemUseFamily::WorldSpecial), ("smithing_table", ItemUseFamily::WorldSpecial), ("smoker", ItemUseFamily::WorldSpecial),
    ("smooth_basalt", ItemUseFamily::WorldSpecial), ("smooth_quartz", ItemUseFamily::WorldSpecial), ("smooth_quartz_slab", ItemUseFamily::WorldSpecial), ("smooth_quartz_stairs", ItemUseFamily::WorldSpecial),
    ("smooth_red_sandstone", ItemUseFamily::WorldSpecial), ("smooth_red_sandstone_slab", ItemUseFamily::WorldSpecial), ("smooth_red_sandstone_stairs", ItemUseFamily::WorldSpecial), ("smooth_sandstone", ItemUseFamily::WorldSpecial),
    ("smooth_sandstone_slab", ItemUseFamily::WorldSpecial), ("smooth_sandstone_stairs", ItemUseFamily::WorldSpecial), ("smooth_stone", ItemUseFamily::WorldSpecial), ("smooth_stone_slab", ItemUseFamily::WorldSpecial),
    ("sniffer_egg", ItemUseFamily::WorldSpecial), ("sniffer_spawn_egg", ItemUseFamily::EntityUse), ("snort_pottery_sherd", ItemUseFamily::PureMaterial), ("snout_armor_trim_smithing_template", ItemUseFamily::PureMaterial),
    ("snow", ItemUseFamily::WorldSpecial), ("snow_block", ItemUseFamily::WorldSpecial), ("snow_golem_spawn_egg", ItemUseFamily::EntityUse), ("snowball", ItemUseFamily::RangedOrThrowable),
    ("soul_campfire", ItemUseFamily::WorldSpecial), ("soul_lantern", ItemUseFamily::WorldSpecial), ("soul_sand", ItemUseFamily::WorldSpecial), ("soul_soil", ItemUseFamily::WorldSpecial),
    ("soul_torch", ItemUseFamily::WorldSpecial), ("spawner", ItemUseFamily::WorldSpecial), ("spectral_arrow", ItemUseFamily::PureMaterial), ("spider_eye", ItemUseFamily::Consumable),
    ("spider_spawn_egg", ItemUseFamily::EntityUse), ("spire_armor_trim_smithing_template", ItemUseFamily::PureMaterial), ("splash_potion", ItemUseFamily::RangedOrThrowable), ("sponge", ItemUseFamily::WorldSpecial),
    ("spore_blossom", ItemUseFamily::WorldSpecial), ("spruce_boat", ItemUseFamily::WorldSpecial), ("spruce_button", ItemUseFamily::WorldSpecial), ("spruce_chest_boat", ItemUseFamily::WorldSpecial),
    ("spruce_door", ItemUseFamily::WorldSpecial), ("spruce_fence", ItemUseFamily::WorldSpecial), ("spruce_fence_gate", ItemUseFamily::WorldSpecial), ("spruce_hanging_sign", ItemUseFamily::WorldSpecial),
    ("spruce_leaves", ItemUseFamily::WorldSpecial), ("spruce_log", ItemUseFamily::WorldSpecial), ("spruce_planks", ItemUseFamily::WorldSpecial), ("spruce_pressure_plate", ItemUseFamily::WorldSpecial),
    ("spruce_sapling", ItemUseFamily::WorldSpecial), ("spruce_sign", ItemUseFamily::WorldSpecial), ("spruce_slab", ItemUseFamily::WorldSpecial), ("spruce_stairs", ItemUseFamily::WorldSpecial),
    ("spruce_trapdoor", ItemUseFamily::WorldSpecial), ("spruce_wood", ItemUseFamily::WorldSpecial), ("spyglass", ItemUseFamily::WorldSpecial), ("squid_spawn_egg", ItemUseFamily::EntityUse),
    ("stick", ItemUseFamily::PureMaterial), ("sticky_piston", ItemUseFamily::WorldSpecial), ("stone", ItemUseFamily::WorldSpecial), ("stone_axe", ItemUseFamily::ToolBlockTransform),
    ("stone_brick_slab", ItemUseFamily::WorldSpecial), ("stone_brick_stairs", ItemUseFamily::WorldSpecial), ("stone_brick_wall", ItemUseFamily::WorldSpecial), ("stone_bricks", ItemUseFamily::WorldSpecial),
    ("stone_button", ItemUseFamily::WorldSpecial), ("stone_hoe", ItemUseFamily::ToolBlockTransform), ("stone_pickaxe", ItemUseFamily::PureMaterial), ("stone_pressure_plate", ItemUseFamily::WorldSpecial),
    ("stone_shovel", ItemUseFamily::ToolBlockTransform), ("stone_slab", ItemUseFamily::WorldSpecial), ("stone_stairs", ItemUseFamily::WorldSpecial), ("stone_sword", ItemUseFamily::PureMaterial),
    ("stonecutter", ItemUseFamily::WorldSpecial), ("stray_spawn_egg", ItemUseFamily::EntityUse), ("strider_spawn_egg", ItemUseFamily::EntityUse), ("string", ItemUseFamily::WorldSpecial),
    ("stripped_acacia_log", ItemUseFamily::WorldSpecial), ("stripped_acacia_wood", ItemUseFamily::WorldSpecial), ("stripped_bamboo_block", ItemUseFamily::WorldSpecial), ("stripped_birch_log", ItemUseFamily::WorldSpecial),
    ("stripped_birch_wood", ItemUseFamily::WorldSpecial), ("stripped_cherry_log", ItemUseFamily::WorldSpecial), ("stripped_cherry_wood", ItemUseFamily::WorldSpecial), ("stripped_crimson_hyphae", ItemUseFamily::WorldSpecial),
    ("stripped_crimson_stem", ItemUseFamily::WorldSpecial), ("stripped_dark_oak_log", ItemUseFamily::WorldSpecial), ("stripped_dark_oak_wood", ItemUseFamily::WorldSpecial), ("stripped_jungle_log", ItemUseFamily::WorldSpecial),
    ("stripped_jungle_wood", ItemUseFamily::WorldSpecial), ("stripped_mangrove_log", ItemUseFamily::WorldSpecial), ("stripped_mangrove_wood", ItemUseFamily::WorldSpecial), ("stripped_oak_log", ItemUseFamily::WorldSpecial),
    ("stripped_oak_wood", ItemUseFamily::WorldSpecial), ("stripped_spruce_log", ItemUseFamily::WorldSpecial), ("stripped_spruce_wood", ItemUseFamily::WorldSpecial), ("stripped_warped_hyphae", ItemUseFamily::WorldSpecial),
    ("stripped_warped_stem", ItemUseFamily::WorldSpecial), ("structure_block", ItemUseFamily::WorldSpecial), ("structure_void", ItemUseFamily::WorldSpecial), ("sugar", ItemUseFamily::PureMaterial),
    ("sugar_cane", ItemUseFamily::WorldSpecial), ("sunflower", ItemUseFamily::WorldSpecial), ("suspicious_gravel", ItemUseFamily::WorldSpecial), ("suspicious_sand", ItemUseFamily::WorldSpecial),
    ("suspicious_stew", ItemUseFamily::Consumable), ("sweet_berries", ItemUseFamily::Consumable), ("tadpole_bucket", ItemUseFamily::FluidContainer), ("tadpole_spawn_egg", ItemUseFamily::EntityUse),
    ("tall_grass", ItemUseFamily::WorldSpecial), ("target", ItemUseFamily::WorldSpecial), ("terracotta", ItemUseFamily::WorldSpecial), ("tide_armor_trim_smithing_template", ItemUseFamily::PureMaterial),
    ("tinted_glass", ItemUseFamily::WorldSpecial), ("tipped_arrow", ItemUseFamily::PureMaterial), ("tnt", ItemUseFamily::WorldSpecial), ("tnt_minecart", ItemUseFamily::WorldSpecial),
    ("torch", ItemUseFamily::WorldSpecial), ("torchflower", ItemUseFamily::WorldSpecial), ("torchflower_seeds", ItemUseFamily::WorldSpecial), ("totem_of_undying", ItemUseFamily::PureMaterial),
    ("trader_llama_spawn_egg", ItemUseFamily::EntityUse), ("trapped_chest", ItemUseFamily::WorldSpecial), ("trial_key", ItemUseFamily::WorldSpecial), ("trial_spawner", ItemUseFamily::WorldSpecial),
    ("trident", ItemUseFamily::RangedOrThrowable), ("tripwire_hook", ItemUseFamily::WorldSpecial), ("tropical_fish", ItemUseFamily::Consumable), ("tropical_fish_bucket", ItemUseFamily::FluidContainer),
    ("tropical_fish_spawn_egg", ItemUseFamily::EntityUse), ("tube_coral", ItemUseFamily::WorldSpecial), ("tube_coral_block", ItemUseFamily::WorldSpecial), ("tube_coral_fan", ItemUseFamily::WorldSpecial),
    ("tuff", ItemUseFamily::WorldSpecial), ("tuff_brick_slab", ItemUseFamily::WorldSpecial), ("tuff_brick_stairs", ItemUseFamily::WorldSpecial), ("tuff_brick_wall", ItemUseFamily::WorldSpecial),
    ("tuff_bricks", ItemUseFamily::WorldSpecial), ("tuff_slab", ItemUseFamily::WorldSpecial), ("tuff_stairs", ItemUseFamily::WorldSpecial), ("tuff_wall", ItemUseFamily::WorldSpecial),
    ("turtle_egg", ItemUseFamily::WorldSpecial), ("turtle_helmet", ItemUseFamily::Equipable), ("turtle_scute", ItemUseFamily::PureMaterial), ("turtle_spawn_egg", ItemUseFamily::EntityUse),
    ("twisting_vines", ItemUseFamily::WorldSpecial), ("vault", ItemUseFamily::WorldSpecial), ("verdant_froglight", ItemUseFamily::WorldSpecial), ("vex_armor_trim_smithing_template", ItemUseFamily::PureMaterial),
    ("vex_spawn_egg", ItemUseFamily::EntityUse), ("villager_spawn_egg", ItemUseFamily::EntityUse), ("vindicator_spawn_egg", ItemUseFamily::EntityUse), ("vine", ItemUseFamily::WorldSpecial),
    ("wandering_trader_spawn_egg", ItemUseFamily::EntityUse), ("ward_armor_trim_smithing_template", ItemUseFamily::PureMaterial), ("warden_spawn_egg", ItemUseFamily::EntityUse), ("warped_button", ItemUseFamily::WorldSpecial),
    ("warped_door", ItemUseFamily::WorldSpecial), ("warped_fence", ItemUseFamily::WorldSpecial), ("warped_fence_gate", ItemUseFamily::WorldSpecial), ("warped_fungus", ItemUseFamily::WorldSpecial),
    ("warped_fungus_on_a_stick", ItemUseFamily::EntityUse), ("warped_hanging_sign", ItemUseFamily::WorldSpecial), ("warped_hyphae", ItemUseFamily::WorldSpecial), ("warped_nylium", ItemUseFamily::WorldSpecial),
    ("warped_planks", ItemUseFamily::WorldSpecial), ("warped_pressure_plate", ItemUseFamily::WorldSpecial), ("warped_roots", ItemUseFamily::WorldSpecial), ("warped_sign", ItemUseFamily::WorldSpecial),
    ("warped_slab", ItemUseFamily::WorldSpecial), ("warped_stairs", ItemUseFamily::WorldSpecial), ("warped_stem", ItemUseFamily::WorldSpecial), ("warped_trapdoor", ItemUseFamily::WorldSpecial),
    ("warped_wart_block", ItemUseFamily::WorldSpecial), ("water_bucket", ItemUseFamily::FluidContainer), ("waxed_chiseled_copper", ItemUseFamily::WorldSpecial), ("waxed_copper_block", ItemUseFamily::WorldSpecial),
    ("waxed_copper_bulb", ItemUseFamily::WorldSpecial), ("waxed_copper_door", ItemUseFamily::WorldSpecial), ("waxed_copper_grate", ItemUseFamily::WorldSpecial), ("waxed_copper_trapdoor", ItemUseFamily::WorldSpecial),
    ("waxed_cut_copper", ItemUseFamily::WorldSpecial), ("waxed_cut_copper_slab", ItemUseFamily::WorldSpecial), ("waxed_cut_copper_stairs", ItemUseFamily::WorldSpecial), ("waxed_exposed_chiseled_copper", ItemUseFamily::WorldSpecial),
    ("waxed_exposed_copper", ItemUseFamily::WorldSpecial), ("waxed_exposed_copper_bulb", ItemUseFamily::WorldSpecial), ("waxed_exposed_copper_door", ItemUseFamily::WorldSpecial), ("waxed_exposed_copper_grate", ItemUseFamily::WorldSpecial),
    ("waxed_exposed_copper_trapdoor", ItemUseFamily::WorldSpecial), ("waxed_exposed_cut_copper", ItemUseFamily::WorldSpecial), ("waxed_exposed_cut_copper_slab", ItemUseFamily::WorldSpecial), ("waxed_exposed_cut_copper_stairs", ItemUseFamily::WorldSpecial),
    ("waxed_oxidized_chiseled_copper", ItemUseFamily::WorldSpecial), ("waxed_oxidized_copper", ItemUseFamily::WorldSpecial), ("waxed_oxidized_copper_bulb", ItemUseFamily::WorldSpecial), ("waxed_oxidized_copper_door", ItemUseFamily::WorldSpecial),
    ("waxed_oxidized_copper_grate", ItemUseFamily::WorldSpecial), ("waxed_oxidized_copper_trapdoor", ItemUseFamily::WorldSpecial), ("waxed_oxidized_cut_copper", ItemUseFamily::WorldSpecial), ("waxed_oxidized_cut_copper_slab", ItemUseFamily::WorldSpecial),
    ("waxed_oxidized_cut_copper_stairs", ItemUseFamily::WorldSpecial), ("waxed_weathered_chiseled_copper", ItemUseFamily::WorldSpecial), ("waxed_weathered_copper", ItemUseFamily::WorldSpecial), ("waxed_weathered_copper_bulb", ItemUseFamily::WorldSpecial),
    ("waxed_weathered_copper_door", ItemUseFamily::WorldSpecial), ("waxed_weathered_copper_grate", ItemUseFamily::WorldSpecial), ("waxed_weathered_copper_trapdoor", ItemUseFamily::WorldSpecial), ("waxed_weathered_cut_copper", ItemUseFamily::WorldSpecial),
    ("waxed_weathered_cut_copper_slab", ItemUseFamily::WorldSpecial), ("waxed_weathered_cut_copper_stairs", ItemUseFamily::WorldSpecial), ("wayfinder_armor_trim_smithing_template", ItemUseFamily::PureMaterial), ("weathered_chiseled_copper", ItemUseFamily::WorldSpecial),
    ("weathered_copper", ItemUseFamily::WorldSpecial), ("weathered_copper_bulb", ItemUseFamily::WorldSpecial), ("weathered_copper_door", ItemUseFamily::WorldSpecial), ("weathered_copper_grate", ItemUseFamily::WorldSpecial),
    ("weathered_copper_trapdoor", ItemUseFamily::WorldSpecial), ("weathered_cut_copper", ItemUseFamily::WorldSpecial), ("weathered_cut_copper_slab", ItemUseFamily::WorldSpecial), ("weathered_cut_copper_stairs", ItemUseFamily::WorldSpecial),
    ("weeping_vines", ItemUseFamily::WorldSpecial), ("wet_sponge", ItemUseFamily::WorldSpecial), ("wheat", ItemUseFamily::PureMaterial), ("wheat_seeds", ItemUseFamily::WorldSpecial),
    ("white_banner", ItemUseFamily::WorldSpecial), ("white_bed", ItemUseFamily::WorldSpecial), ("white_candle", ItemUseFamily::WorldSpecial), ("white_carpet", ItemUseFamily::WorldSpecial),
    ("white_concrete", ItemUseFamily::WorldSpecial), ("white_concrete_powder", ItemUseFamily::WorldSpecial), ("white_dye", ItemUseFamily::EntityUse), ("white_glazed_terracotta", ItemUseFamily::WorldSpecial),
    ("white_shulker_box", ItemUseFamily::WorldSpecial), ("white_stained_glass", ItemUseFamily::WorldSpecial), ("white_stained_glass_pane", ItemUseFamily::WorldSpecial), ("white_terracotta", ItemUseFamily::WorldSpecial),
    ("white_tulip", ItemUseFamily::WorldSpecial), ("white_wool", ItemUseFamily::WorldSpecial), ("wild_armor_trim_smithing_template", ItemUseFamily::PureMaterial), ("wind_charge", ItemUseFamily::RangedOrThrowable),
    ("witch_spawn_egg", ItemUseFamily::EntityUse), ("wither_rose", ItemUseFamily::WorldSpecial), ("wither_skeleton_skull", ItemUseFamily::Equipable), ("wither_skeleton_spawn_egg", ItemUseFamily::EntityUse),
    ("wither_spawn_egg", ItemUseFamily::EntityUse), ("wolf_armor", ItemUseFamily::Equipable), ("wolf_spawn_egg", ItemUseFamily::EntityUse), ("wooden_axe", ItemUseFamily::ToolBlockTransform),
    ("wooden_hoe", ItemUseFamily::ToolBlockTransform), ("wooden_pickaxe", ItemUseFamily::PureMaterial), ("wooden_shovel", ItemUseFamily::ToolBlockTransform), ("wooden_sword", ItemUseFamily::PureMaterial),
    ("writable_book", ItemUseFamily::WorldSpecial), ("written_book", ItemUseFamily::WorldSpecial), ("yellow_banner", ItemUseFamily::WorldSpecial), ("yellow_bed", ItemUseFamily::WorldSpecial),
    ("yellow_candle", ItemUseFamily::WorldSpecial), ("yellow_carpet", ItemUseFamily::WorldSpecial), ("yellow_concrete", ItemUseFamily::WorldSpecial), ("yellow_concrete_powder", ItemUseFamily::WorldSpecial),
    ("yellow_dye", ItemUseFamily::EntityUse), ("yellow_glazed_terracotta", ItemUseFamily::WorldSpecial), ("yellow_shulker_box", ItemUseFamily::WorldSpecial), ("yellow_stained_glass", ItemUseFamily::WorldSpecial),
    ("yellow_stained_glass_pane", ItemUseFamily::WorldSpecial), ("yellow_terracotta", ItemUseFamily::WorldSpecial), ("yellow_wool", ItemUseFamily::WorldSpecial), ("zoglin_spawn_egg", ItemUseFamily::EntityUse),
    ("zombie_head", ItemUseFamily::Equipable), ("zombie_horse_spawn_egg", ItemUseFamily::EntityUse), ("zombie_spawn_egg", ItemUseFamily::EntityUse), ("zombie_villager_spawn_egg", ItemUseFamily::EntityUse),
    ("zombified_piglin_spawn_egg", ItemUseFamily::EntityUse),

];

/// Classify one item (`"minecraft:bow"` or bare `"bow"`) into its family.
/// Total by construction: every registry name is in the table.
pub fn classify_item_use(item: &str) -> ItemUseFamily {
    let n = item.strip_prefix("minecraft:").unwrap_or(item);
    match ITEM_USE_CLASSIFICATION.binary_search_by(|(name, _)| name.cmp(&n)) {
        Ok(i) => ITEM_USE_CLASSIFICATION[i].1,
        // A name absent from the 1.21.1 registry cannot be a vanilla item —
        // report it as an inert material rather than a blind spot.
        Err(_) => ItemUseFamily::PureMaterial,
    }
}

// ---- ToolBlockTransform rule tables ---------------------------------------

/// `AxeItem.STRIPPABLES` — 21 entries from the jar `ImmutableMap` clinit.
static AXE_STRIP: &[(&str, &str)] = &[
    ("oak_log", "stripped_oak_log"),
    ("oak_wood", "stripped_oak_wood"),
    ("spruce_log", "stripped_spruce_log"),
    ("spruce_wood", "stripped_spruce_wood"),
    ("birch_log", "stripped_birch_log"),
    ("birch_wood", "stripped_birch_wood"),
    ("jungle_log", "stripped_jungle_log"),
    ("jungle_wood", "stripped_jungle_wood"),
    ("acacia_log", "stripped_acacia_log"),
    ("acacia_wood", "stripped_acacia_wood"),
    ("dark_oak_log", "stripped_dark_oak_log"),
    ("dark_oak_wood", "stripped_dark_oak_wood"),
    ("cherry_log", "stripped_cherry_log"),
    ("cherry_wood", "stripped_cherry_wood"),
    ("mangrove_log", "stripped_mangrove_log"),
    ("mangrove_wood", "stripped_mangrove_wood"),
    ("crimson_stem", "stripped_crimson_stem"),
    ("crimson_hyphae", "stripped_crimson_hyphae"),
    ("warped_stem", "stripped_warped_stem"),
    ("warped_hyphae", "stripped_warped_hyphae"),
    ("bamboo_block", "stripped_bamboo_block"),
];

/// `WeatheringCopper.NEXT_BY_BLOCK` — the 27-step oxidation chain
/// (9 copper families × 3 forward steps).
static COPPER_NEXT: &[(&str, &str)] = &[
    ("copper_block", "exposed_copper"),
    ("exposed_copper", "weathered_copper"),
    ("weathered_copper", "oxidized_copper"),
    ("chiseled_copper", "exposed_chiseled_copper"),
    ("exposed_chiseled_copper", "weathered_chiseled_copper"),
    ("weathered_chiseled_copper", "oxidized_chiseled_copper"),
    ("cut_copper", "exposed_cut_copper"),
    ("exposed_cut_copper", "weathered_cut_copper"),
    ("weathered_cut_copper", "oxidized_cut_copper"),
    ("cut_copper_slab", "exposed_cut_copper_slab"),
    ("exposed_cut_copper_slab", "weathered_cut_copper_slab"),
    ("weathered_cut_copper_slab", "oxidized_cut_copper_slab"),
    ("cut_copper_stairs", "exposed_cut_copper_stairs"),
    ("exposed_cut_copper_stairs", "weathered_cut_copper_stairs"),
    ("weathered_cut_copper_stairs", "oxidized_cut_copper_stairs"),
    ("copper_door", "exposed_copper_door"),
    ("exposed_copper_door", "weathered_copper_door"),
    ("weathered_copper_door", "oxidized_copper_door"),
    ("copper_trapdoor", "exposed_copper_trapdoor"),
    ("exposed_copper_trapdoor", "weathered_copper_trapdoor"),
    ("weathered_copper_trapdoor", "oxidized_copper_trapdoor"),
    ("copper_grate", "exposed_copper_grate"),
    ("exposed_copper_grate", "weathered_copper_grate"),
    ("weathered_copper_grate", "oxidized_copper_grate"),
    ("copper_bulb", "exposed_copper_bulb"),
    ("exposed_copper_bulb", "weathered_copper_bulb"),
    ("weathered_copper_bulb", "oxidized_copper_bulb"),
];

/// `HoneycombItem.WAXABLES` — all 36 waxings (9 families × 4 states).
static HONEYCOMB_WAX: &[(&str, &str)] = &[
    ("copper_block", "waxed_copper_block"),
    ("exposed_copper", "waxed_exposed_copper"),
    ("weathered_copper", "waxed_weathered_copper"),
    ("oxidized_copper", "waxed_oxidized_copper"),
    ("chiseled_copper", "waxed_chiseled_copper"),
    ("exposed_chiseled_copper", "waxed_exposed_chiseled_copper"),
    ("weathered_chiseled_copper", "waxed_weathered_chiseled_copper"),
    ("oxidized_chiseled_copper", "waxed_oxidized_chiseled_copper"),
    ("cut_copper", "waxed_cut_copper"),
    ("exposed_cut_copper", "waxed_exposed_cut_copper"),
    ("weathered_cut_copper", "waxed_weathered_cut_copper"),
    ("oxidized_cut_copper", "waxed_oxidized_cut_copper"),
    ("cut_copper_slab", "waxed_cut_copper_slab"),
    ("exposed_cut_copper_slab", "waxed_exposed_cut_copper_slab"),
    ("weathered_cut_copper_slab", "waxed_weathered_cut_copper_slab"),
    ("oxidized_cut_copper_slab", "waxed_oxidized_cut_copper_slab"),
    ("cut_copper_stairs", "waxed_cut_copper_stairs"),
    ("exposed_cut_copper_stairs", "waxed_exposed_cut_copper_stairs"),
    ("weathered_cut_copper_stairs", "waxed_weathered_cut_copper_stairs"),
    ("oxidized_cut_copper_stairs", "waxed_oxidized_cut_copper_stairs"),
    ("copper_door", "waxed_copper_door"),
    ("exposed_copper_door", "waxed_exposed_copper_door"),
    ("weathered_copper_door", "waxed_weathered_copper_door"),
    ("oxidized_copper_door", "waxed_oxidized_copper_door"),
    ("copper_trapdoor", "waxed_copper_trapdoor"),
    ("exposed_copper_trapdoor", "waxed_exposed_copper_trapdoor"),
    ("weathered_copper_trapdoor", "waxed_weathered_copper_trapdoor"),
    ("oxidized_copper_trapdoor", "waxed_oxidized_copper_trapdoor"),
    ("copper_grate", "waxed_copper_grate"),
    ("exposed_copper_grate", "waxed_exposed_copper_grate"),
    ("weathered_copper_grate", "waxed_weathered_copper_grate"),
    ("oxidized_copper_grate", "waxed_oxidized_copper_grate"),
    ("copper_bulb", "waxed_copper_bulb"),
    ("exposed_copper_bulb", "waxed_exposed_copper_bulb"),
    ("weathered_copper_bulb", "waxed_weathered_copper_bulb"),
    ("oxidized_copper_bulb", "waxed_oxidized_copper_bulb"),
];

/// `HoeItem.TILLABLES`: `grass_block`/`dirt`/`dirt_path`/`coarse_dirt`/`rooted_dirt`.
/// The first three (plus coarse, per vanilla's `onlyIfAirAbove` predicate) need
/// air above; `rooted_dirt` always converts and drops one `hanging_roots`.
static HOE_TILL: &[(&str, &str, bool, Option<&str>)] = &[
    // (block, result_block, requires_air_above, bonus_drop)
    ("grass_block", "farmland", true, None),
    ("dirt_path", "farmland", true, None),
    ("dirt", "farmland", true, None),
    ("coarse_dirt", "dirt", true, None),
    ("rooted_dirt", "dirt", false, Some("hanging_roots")),
];

/// `ShovelItem.FLATTENABLES` — 6 blocks to `dirt_path` (`onlyIfAirAbove`).
static SHOVEL_FLATTEN: &[&str] = &[
    "grass_block",
    "dirt",
    "podzol",
    "coarse_dirt",
    "mycelium",
    "rooted_dirt",
];

/// The distinct tool actions the kernel reports. The caller knows the held
/// item's family; `tool_block_transform` joins tool + block → action.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ToolAction {
    /// Axe on a strippable log/wood/stem/hyphae/bamboo_block.
    Strip,
    /// Axe on unwaxed oxidized copper → previous oxidation stage.
    ScrapeCopper,
    /// Axe on waxed copper → same block unwaxed (checked before scrape).
    UnwaxCopper,
    /// Hoe on a tillable block → farmland or dirt.
    Till,
    /// Shovel on a flattenable block → dirt_path.
    Flatten,
    /// Shovel on a lit campfire/soul campfire → extinguishes it.
    ExtinguishCampfire,
    /// Shears on pumpkin → carved_pumpkin (+4 pumpkin seeds).
    CarvePumpkin,
    /// Shears on a full (`honey_level=5`) bee nest/beehive → honeycomb harvest.
    HarvestBeehive,
    /// Shears on a growing-plant tip (weeping/twisting vines, kelp, cave
    /// vines) → stops further growth.
    StopGrowthTip,
    /// Shears on tripwire → disarms it.
    DisarmTripwire,
    /// Flint & steel or fire charge → ignite (generic top fire, or the
    /// special block behaviors: relight campfire/candle, prime TNT).
    Ignite,
    /// Honeycomb on copper → waxed variant.
    WaxCopper,
    /// Honeycomb on any sign block → wax (via [`apply_dye_target`]).
    WaxSign,
}

/// The outcome of [`tool_block_transform`]: which action applies, the block
/// it produces (when the block itself changes), and a bonus item drop.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ToolBlockResult {
    pub action: ToolAction,
    /// The replacement block name when the action swaps the block
    /// (`stripped_oak_log`, `dirt_path`, `farmland`, `waxed_*`, `carved_pumpkin`,
    /// previous-oxidation copper). `None` when the world change is elsewhere
    /// (ignited air, harvested contents, stopped growth, waxed sign state).
    pub result_block: Option<&'static str>,
    /// Vanilla's `onlyIfAirAbove` predicate (hoe tills, shovel flattens):
    /// the transform requires air/replaceable above the target block.
    pub requires_air_above: bool,
    /// A bonus item the action drops (`hanging_roots` for rooted dirt,
    /// `pumpkin_seeds` for carving, `honeycomb` for beehive harvest).
    pub bonus_drop: Option<&'static str>,
}

const AXE_ITEMS: [&str; 6] = [
    "wooden_axe",
    "stone_axe",
    "golden_axe",
    "iron_axe",
    "diamond_axe",
    "netherite_axe",
];
const HOE_ITEMS: [&str; 6] = [
    "wooden_hoe",
    "stone_hoe",
    "golden_hoe",
    "iron_hoe",
    "diamond_hoe",
    "netherite_hoe",
];
const SHOVEL_ITEMS: [&str; 6] = [
    "wooden_shovel",
    "stone_shovel",
    "golden_shovel",
    "iron_shovel",
    "diamond_shovel",
    "netherite_shovel",
];

fn lookup<'a>(table: &'a [(&'a str, &'a str)], key: &str) -> Option<&'a str> {
    table.iter().find(|(k, _)| *k == key).map(|(_, v)| *v)
}

/// Axe scrape = inverse of [`COPPER_NEXT`] (only for unwaxed copper).
fn copper_previous(block: &str) -> Option<&'static str> {
    COPPER_NEXT
        .iter()
        .find(|(_, n)| *n == block)
        .map(|(p, _)| *p)
}

fn is_sign_block(block: &str) -> bool {
    // 10 woods × {sign, wall_sign, hanging_sign, wall_hanging_sign} — all end `_sign`.
    block.ends_with("_sign")
}

/// The unified tool→block dispatcher: evaluates which vanilla transform a
/// `tool` (item name) performs on `block` (block name). Order matches vanilla
/// for axes — wax removal first, then copper scrape, then stripping. Returns
/// `None` when the pair has no interaction.
pub fn tool_block_transform(tool: &str, block: &str) -> Option<ToolBlockResult> {
    let t = tool.strip_prefix("minecraft:").unwrap_or(tool);
    let b = block.strip_prefix("minecraft:").unwrap_or(block);
    let base = |action, result_block, requires_air_above, bonus_drop| ToolBlockResult {
        action,
        result_block,
        requires_air_above,
        bonus_drop,
    };
    if AXE_ITEMS.contains(&t) {
        // Vanilla order: waxed copper unwaxes first, then de-oxidize, then strip.
        if let Some(unwaxed) = HONEYCOMB_WAX
            .iter()
            .find(|(_, w)| *w == b)
            .map(|(u, _)| *u)
        {
            return Some(base(ToolAction::UnwaxCopper, Some(unwaxed), false, None));
        }
        if let Some(prev) = copper_previous(b) {
            return Some(base(ToolAction::ScrapeCopper, Some(prev), false, None));
        }
        if let Some(stripped) = lookup(AXE_STRIP, b) {
            return Some(base(ToolAction::Strip, Some(stripped), false, None));
        }
        return None;
    }
    if HOE_ITEMS.contains(&t) {
        return HOE_TILL.iter().find(|(k, _, _, _)| *k == b).map(
            |(_, res, air, drop)| base(ToolAction::Till, Some(res), *air, *drop),
        );
    }
    if SHOVEL_ITEMS.contains(&t) {
        if matches!(b, "campfire" | "soul_campfire") {
            // Applies to the lit state only — caller checks the `lit` property.
            return Some(base(ToolAction::ExtinguishCampfire, None, false, None));
        }
        if SHOVEL_FLATTEN.contains(&b) {
            return Some(base(ToolAction::Flatten, Some("dirt_path"), true, None));
        }
        return None;
    }
    if t == "shears" {
        match b {
            "pumpkin" => {
                return Some(base(ToolAction::CarvePumpkin, Some("carved_pumpkin"), false, Some("pumpkin_seeds")))
            }
            "bee_nest" | "beehive" => {
                // Vanilla harvests only at honey_level 5; the block entity gate
                // lives in `BeehiveBlock` — flagged via bonus_drop honeycomb.
                return Some(base(ToolAction::HarvestBeehive, None, false, Some("honeycomb")))
            }
            "tripwire" => {
                return Some(base(ToolAction::DisarmTripwire, None, false, None))
            }
            "weeping_vines" | "weeping_vines_plant" | "twisting_vines"
            | "twisting_vines_plant" | "kelp" | "kelp_plant" | "cave_vines"
            | "cave_vines_plant" => {
                return Some(base(ToolAction::StopGrowthTip, None, false, None))
            }
            _ => return None,
        }
    }
    if t == "flint_and_steel" || t == "fire_charge" {
        // Always an ignite attempt: special block behaviors (TNT prime,
        // campfire/candle relight) are keyed on the block name by the caller;
        // otherwise it is the generic top-surface fire.
        return Some(base(ToolAction::Ignite, None, false, None));
    }
    if t == "honeycomb" {
        if is_sign_block(b) {
            return Some(base(ToolAction::WaxSign, None, false, None));
        }
        if let Some(waxed) = lookup(HONEYCOMB_WAX, b) {
            return Some(base(ToolAction::WaxCopper, Some(waxed), false, None));
        }
        return None;
    }
    None
}

/// Sheep/mooshroom/snowman/bogged shear results for shears-on-entity
/// (the entity half of `ShearsItem` interactions).
/// Returns `(wool/item drop, resulting entity)` — e.g. mooshroom → 5 red
/// mushrooms and converts to `minecraft:cow`; snow_golem removes its pumpkin.
pub fn shearable_entity(entity: &str) -> Option<(&'static str, Option<&'static str>)> {
    let e = entity.strip_prefix("minecraft:").unwrap_or(entity);
    Some(match e {
        "sheep" => ("wool_of_current_color", None),
        "mushroom_cow" | "mooshroom" => ("red_mushroom", Some("minecraft:cow")),
        "snow_golem" => ("carved_pumpkin_removed", None),
        "bogged" => ("brown_mushroom_and_red_mushroom", None),
        _ => return None,
    })
}

// ---- Consumable: the full 1.21.1 Foods table -------------------------------

/// A single `MobEffectInstance` possibility from `FoodProperties.effects`
/// (effect id, duration in ticks, amplifier, probability).
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct FoodEffect {
    pub effect: &'static str,
    pub duration_ticks: i32,
    pub amplifier: u8,
    pub probability: f32,
}

/// `FoodProperties` for one item: nutrition, saturation modifier (vanilla
/// saturation = `nutrition * modifier * 2`), always-edible, fast-eat
/// (`eatSeconds` 0.8 vs the default 1.6), `usingConvertsTo` remainder and
/// effect list.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct FoodProperties {
    pub nutrition: u8,
    pub saturation_modifier: f32,
    pub can_always_eat: bool,
    /// `eatSeconds < 1.6` (`fast()` on the builder — dried kelp only).
    pub fast_eat: bool,
    /// The `usingConvertsTo` component remainder (`bowl` for stews); item-level
    /// remainders (honey bottle → glass bottle) live in [`item_use_remainder`].
    pub using_converts_to: Option<&'static str>,
    pub effects: &'static [FoodEffect],
}

const NO_EFFECTS: &[FoodEffect] = &[];

macro_rules! food {
    ($nut:expr, $sat:expr) => {
        FoodProperties {
            nutrition: $nut,
            saturation_modifier: $sat,
            can_always_eat: false,
            fast_eat: false,
            using_converts_to: None,
            effects: NO_EFFECTS,
        }
    };
    ($nut:expr, $sat:expr, always) => {
        FoodProperties {
            can_always_eat: true,
            ..food!($nut, $sat)
        }
    };
    ($nut:expr, $sat:expr, fast) => {
        FoodProperties {
            fast_eat: true,
            ..food!($nut, $sat)
        }
    };
    ($nut:expr, $sat:expr, conv $conv:expr) => {
        FoodProperties {
            using_converts_to: Some($conv),
            ..food!($nut, $sat)
        }
    };
    ($nut:expr, $sat:expr, conv $conv:expr, always) => {
        FoodProperties {
            using_converts_to: Some($conv),
            can_always_eat: true,
            ..food!($nut, $sat)
        }
    };
    ($nut:expr, $sat:expr, always, effs $effs:expr) => {
        FoodProperties {
            can_always_eat: true,
            effects: $effs,
            ..food!($nut, $sat)
        }
    };
    ($nut:expr, $sat:expr, effs $effs:expr) => {
        FoodProperties {
            effects: $effs,
            ..food!($nut, $sat)
        }
    };
}

/// `net.minecraft.world.food.Foods` — every constant from the jar clinit.
/// `potion`/`milk_bucket`/`honey_bottle` remainders are item-level, not
/// component-level (see [`item_use_remainder`]); `ominous_bottle` carries a
/// real `FoodProperties` in 1.21.1 (nutrition 1, saturation 0.1).
static FOODS: &[(&str, FoodProperties)] = &[
    ("apple", food!(4, 0.3)),
    ("baked_potato", food!(5, 0.6)),
    ("beef", food!(3, 0.3)),
    ("beetroot", food!(1, 0.6)),
    ("beetroot_soup", food!(6, 0.6, conv "bowl")),
    ("bread", food!(5, 0.6)),
    ("carrot", food!(3, 0.6)),
    (
        "chicken",
        food!(2, 0.3, effs &[FoodEffect {
            effect: "minecraft:hunger",
            duration_ticks: 600,
            amplifier: 0,
            probability: 0.3,
        }]),
    ),
    ("chorus_fruit", food!(4, 0.3, always)),
    ("cod", food!(2, 0.1)),
    ("cooked_beef", food!(8, 0.8)),
    ("cooked_chicken", food!(6, 0.6)),
    ("cooked_cod", food!(5, 0.6)),
    ("cooked_mutton", food!(6, 0.8)),
    ("cooked_porkchop", food!(8, 0.8)),
    ("cooked_rabbit", food!(5, 0.6)),
    ("cooked_salmon", food!(6, 0.8)),
    ("cookie", food!(2, 0.1)),
    ("dried_kelp", food!(1, 0.3, fast)),
    (
        "enchanted_golden_apple",
        food!(4, 1.2, always, effs &[
            FoodEffect {
                effect: "minecraft:regeneration",
                duration_ticks: 400,
                amplifier: 1,
                probability: 1.0,
            },
            FoodEffect {
                effect: "minecraft:resistance",
                duration_ticks: 6000,
                amplifier: 0,
                probability: 1.0,
            },
            FoodEffect {
                effect: "minecraft:fire_resistance",
                duration_ticks: 6000,
                amplifier: 0,
                probability: 1.0,
            },
            FoodEffect {
                effect: "minecraft:absorption",
                duration_ticks: 2400,
                amplifier: 3,
                probability: 1.0,
            },
        ]),
    ),
    (
        "golden_apple",
        food!(4, 1.2, always, effs &[
            FoodEffect {
                effect: "minecraft:regeneration",
                duration_ticks: 100,
                amplifier: 1,
                probability: 1.0,
            },
            FoodEffect {
                effect: "minecraft:absorption",
                duration_ticks: 2400,
                amplifier: 0,
                probability: 1.0,
            },
        ]),
    ),
    ("golden_carrot", food!(6, 1.2)),
    ("honey_bottle", food!(6, 0.1)),
    ("melon_slice", food!(2, 0.3)),
    ("mushroom_stew", food!(6, 0.6, conv "bowl")),
    ("mutton", food!(2, 0.3)),
    (
        "poisonous_potato",
        food!(2, 0.3, effs &[FoodEffect {
            effect: "minecraft:poison",
            duration_ticks: 100,
            amplifier: 0,
            probability: 0.6,
        }]),
    ),
    ("porkchop", food!(3, 0.3)),
    ("potato", food!(1, 0.3)),
    (
        "pufferfish",
        food!(1, 0.1, effs &[
            FoodEffect {
                effect: "minecraft:poison",
                duration_ticks: 1200,
                amplifier: 1,
                probability: 1.0,
            },
            FoodEffect {
                effect: "minecraft:hunger",
                duration_ticks: 300,
                amplifier: 2,
                probability: 1.0,
            },
            FoodEffect {
                effect: "minecraft:nausea",
                duration_ticks: 300,
                amplifier: 0,
                probability: 1.0,
            },
        ]),
    ),
    ("pumpkin_pie", food!(8, 0.3)),
    ("rabbit", food!(3, 0.3)),
    ("rabbit_stew", food!(10, 0.6, conv "bowl")),
    (
        "rotten_flesh",
        food!(4, 0.1, effs &[FoodEffect {
            effect: "minecraft:hunger",
            duration_ticks: 600,
            amplifier: 0,
            probability: 0.8,
        }]),
    ),
    ("salmon", food!(2, 0.1)),
    (
        "spider_eye",
        food!(2, 0.8, effs &[FoodEffect {
            effect: "minecraft:poison",
            duration_ticks: 100,
            amplifier: 0,
            probability: 1.0,
        }]),
    ),
    ("suspicious_stew", food!(6, 0.6, conv "bowl", always)),
    ("sweet_berries", food!(2, 0.1)),
    ("glow_berries", food!(2, 0.1)),
    ("tropical_fish", food!(1, 0.1)),
    ("ominous_bottle", food!(1, 0.1)),
];

/// `food_properties(item)` — the vanilla `FoodProperties` for one of the 41
/// `Foods`-carrying items, `None` for everything else.
pub fn food_properties(item: &str) -> Option<FoodProperties> {
    let n = item.strip_prefix("minecraft:").unwrap_or(item);
    FOODS.iter().find(|(k, _)| *k == n).map(|(_, f)| *f)
}

/// The effective remainder item after a use (`HoneyBottleItem` and the fish
/// buckets apply theirs at the item level, stews via `usingConvertsTo`).
pub fn item_use_remainder(item: &str) -> Option<&'static str> {
    let n = item.strip_prefix("minecraft:").unwrap_or(item);
    Some(match n {
        "honey_bottle" | "potion" | "ominous_bottle" => "glass_bottle",
        "milk_bucket" | "water_bucket" | "lava_bucket" | "powder_snow_bucket"
        | "cod_bucket" | "salmon_bucket" | "pufferfish_bucket"
        | "tropical_fish_bucket" | "axolotl_bucket" | "tadpole_bucket" => "bucket",
        "mushroom_stew" | "beetroot_soup" | "rabbit_stew" | "suspicious_stew" => "bowl",
        _ => return None,
    })
}

// ---- composter --------------------------------------------------------------

/// `ComposterBlock.COMPOSTABLES` — the complete 101-entry chance table from
/// the jar (`bootStrap`), as integer percents. Tiers: 30 (34 items), 50 (11),
/// 65 (43), 85 (11) and 100 (2 — `cake` and `pumpkin_pie` are the only 1.0
/// compostables; field-resolved from `ComposterBlock.bootStrap` bytecode).
static COMPOSTABLES: &[(&str, u8)] = &[
    // 30% — leaves, saplings, seeds, roots, small plants
    ("acacia_leaves", 30), ("acacia_sapling", 30), ("azalea_leaves", 30),
    ("beetroot_seeds", 30), ("birch_leaves", 30), ("birch_sapling", 30),
    ("cherry_leaves", 30), ("cherry_sapling", 30), ("dark_oak_leaves", 30),
    ("dark_oak_sapling", 30), ("dried_kelp", 30), ("glow_berries", 30),
    ("hanging_roots", 30), ("jungle_leaves", 30), ("jungle_sapling", 30),
    ("kelp", 30), ("mangrove_leaves", 30), ("mangrove_propagule", 30),
    ("mangrove_roots", 30), ("melon_seeds", 30), ("moss_carpet", 30),
    ("oak_leaves", 30), ("oak_sapling", 30), ("pink_petals", 30),
    ("pitcher_pod", 30), ("pumpkin_seeds", 30), ("seagrass", 30),
    ("short_grass", 30), ("small_dripleaf", 30), ("spruce_leaves", 30),
    ("spruce_sapling", 30), ("sweet_berries", 30), ("torchflower_seeds", 30),
    ("wheat_seeds", 30),
    // 50%
    ("cactus", 50), ("dried_kelp_block", 50), ("flowering_azalea_leaves", 50),
    ("glow_lichen", 50), ("melon_slice", 50), ("nether_sprouts", 50),
    ("sugar_cane", 50), ("tall_grass", 50), ("twisting_vines", 50),
    ("vine", 50), ("weeping_vines", 50),
    // 65%
    ("allium", 65), ("apple", 65), ("azalea", 65), ("azure_bluet", 65),
    ("beetroot", 65), ("big_dripleaf", 65), ("blue_orchid", 65),
    ("brown_mushroom", 65), ("carrot", 65), ("carved_pumpkin", 65),
    ("cocoa_beans", 65), ("cornflower", 65), ("crimson_fungus", 65),
    ("crimson_roots", 65), ("dandelion", 65), ("fern", 65),
    ("large_fern", 65), ("lilac", 65), ("lily_of_the_valley", 65),
    ("lily_pad", 65), ("melon", 65), ("moss_block", 65),
    ("mushroom_stem", 65), ("nether_wart", 65), ("orange_tulip", 65),
    ("oxeye_daisy", 65), ("peony", 65), ("pink_tulip", 65), ("poppy", 65),
    ("potato", 65), ("pumpkin", 65), ("red_mushroom", 65),
    ("red_tulip", 65), ("rose_bush", 65), ("sea_pickle", 65),
    ("shroomlight", 65), ("spore_blossom", 65), ("sunflower", 65),
    ("warped_fungus", 65), ("warped_roots", 65), ("wheat", 65),
    ("white_tulip", 65), ("wither_rose", 65),
    // 85%
    ("baked_potato", 85), ("bread", 85), ("brown_mushroom_block", 85),
    ("cookie", 85), ("flowering_azalea", 85),
    ("hay_block", 85), ("nether_wart_block", 85), ("pitcher_plant", 85),
    ("red_mushroom_block", 85), ("torchflower", 85),
    ("warped_wart_block", 85),
    // 100% — the only two full-chance compostables in 1.21.1
    ("cake", 100), ("pumpkin_pie", 100),
];

/// `composter_chance(item)` — vanilla's `COMPOSTABLES.getFloat(item)` as a
/// percent (`Some(30|50|65|85|100)`), `None` when the item is not compostable.
pub fn composter_chance(item: &str) -> Option<u8> {
    let n = item.strip_prefix("minecraft:").unwrap_or(item);
    COMPOSTABLES.iter().find(|(k, _)| *k == n).map(|(_, c)| *c)
}

// ---- EntityUse: breed/tame food tables and spawn eggs -----------------------

const SEEDS_6: &[&str] = &[
    "minecraft:wheat_seeds",
    "minecraft:melon_seeds",
    "minecraft:pumpkin_seeds",
    "minecraft:beetroot_seeds",
    "minecraft:torchflower_seeds",
    "minecraft:pitcher_pod",
];
const MEAT_TAG: &[&str] = &[
    "minecraft:beef",
    "minecraft:chicken",
    "minecraft:cooked_beef",
    "minecraft:cooked_chicken",
    "minecraft:cooked_mutton",
    "minecraft:cooked_porkchop",
    "minecraft:cooked_rabbit",
    "minecraft:mutton",
    "minecraft:porkchop",
    "minecraft:rabbit",
    "minecraft:rotten_flesh",
];
const FLOWERS_TAG: &[&str] = &[
    // #minecraft:small_flowers + #minecraft:tall_flowers + the direct entries
    // of #minecraft:flowers, expanded from the jar datapack JSONs.
    "minecraft:dandelion",
    "minecraft:poppy",
    "minecraft:blue_orchid",
    "minecraft:allium",
    "minecraft:azure_bluet",
    "minecraft:red_tulip",
    "minecraft:orange_tulip",
    "minecraft:white_tulip",
    "minecraft:pink_tulip",
    "minecraft:oxeye_daisy",
    "minecraft:cornflower",
    "minecraft:lily_of_the_valley",
    "minecraft:wither_rose",
    "minecraft:torchflower",
    "minecraft:sunflower",
    "minecraft:lilac",
    "minecraft:peony",
    "minecraft:rose_bush",
    "minecraft:pitcher_plant",
    "minecraft:flowering_azalea_leaves",
    "minecraft:flowering_azalea",
    "minecraft:mangrove_propagule",
    "minecraft:cherry_leaves",
    "minecraft:pink_petals",
    "minecraft:chorus_flower",
    "minecraft:spore_blossom",
];
const NOTHING: &[&str] = &[];

/// `breed_foods_for(entity)` — the exact `data/minecraft/tags/item/*_food.json`
/// contents from the jar for every breedable entity (`minecraft:` names; bare
/// names also accepted). Empty for entities that do not breed from items
/// (villagers breed on willingness, fish/golems don't breed).
pub fn breed_foods_for(entity: &str) -> &'static [&'static str] {
    let e = entity.strip_prefix("minecraft:").unwrap_or(entity);
    match e {
        "cow" | "sheep" | "goat" | "mushroom_cow" | "mooshroom" => {
            &["minecraft:wheat"]
        }
        "pig" => &[
            "minecraft:carrot",
            "minecraft:potato",
            "minecraft:beetroot",
        ],
        "chicken" => SEEDS_6,
        "rabbit" => &[
            "minecraft:carrot",
            "minecraft:golden_carrot",
            "minecraft:dandelion",
        ],
        // AbstractHorse: only golden apple/carrot trigger love mode; the rest
        // of HORSE_FOOD heals/grows — see `horse_feed_items` below.
        "horse" | "donkey" | "mule" => &[
            "minecraft:golden_apple",
            "minecraft:golden_carrot",
        ],
        "llama" | "trader_llama" => &["minecraft:hay_block"],
        "wolf" => MEAT_TAG,
        "cat" | "ocelot" => &["minecraft:cod", "minecraft:salmon"],
        "fox" => &["minecraft:sweet_berries", "minecraft:glow_berries"],
        "panda" => &["minecraft:bamboo"],
        "turtle" => &["minecraft:seagrass"],
        "axolotl" => &["minecraft:tropical_fish_bucket"],
        "frog" => &["minecraft:slime_ball"],
        "hoglin" => &["minecraft:crimson_fungus"],
        "strider" => &["minecraft:warped_fungus"],
        "bee" => FLOWERS_TAG,
        "armadillo" => &["minecraft:spider_eye"],
        "sniffer" => &["minecraft:torchflower_seeds"],
        "camel" => &["minecraft:cactus"],
        "piglin" => &["minecraft:porkchop", "minecraft:cooked_porkchop"],
        _ => NOTHING,
    }
}

/// `tame_foods_for(entity)` — items that tame (or gain trust for) an entity:
/// wolf→bone, cat/ocelot→cod+salmon (ocelot gains trust, does not tame),
/// parrot→any seeds. Horses/llamas tame by *mounting*; their
/// `horse_food`/`llama_food` only raise temper — exposed separately as
/// [`horse_feed_items`]/[`llama_feed_items`]. `cookie` kills parrots instead
/// of taming — see [`parrot_poisonous_foods`].
pub fn tame_foods_for(entity: &str) -> &'static [&'static str] {
    let e = entity.strip_prefix("minecraft:").unwrap_or(entity);
    match e {
        "wolf" => &["minecraft:bone"],
        "cat" | "ocelot" => &["minecraft:cod", "minecraft:salmon"],
        "parrot" => SEEDS_6,
        _ => NOTHING,
    }
}

/// `parrot_poisonous_food` — feeding a parrot a cookie kills it instantly.
pub fn parrot_poisonous_foods() -> &'static [&'static str] {
    &["minecraft:cookie"]
}

/// The full `horse_food` tag — heals/grows/raises temper; only golden apple
/// and golden carrot breed (see [`breed_foods_for`]).
pub fn horse_feed_items() -> &'static [&'static str] {
    &[
        "minecraft:wheat",
        "minecraft:sugar",
        "minecraft:hay_block",
        "minecraft:apple",
        "minecraft:golden_carrot",
        "minecraft:golden_apple",
        "minecraft:enchanted_golden_apple",
    ]
}

/// `llama_food` — wheat raises temper, hay block breeds.
pub fn llama_feed_items() -> &'static [&'static str] {
    &["minecraft:wheat", "minecraft:hay_block"]
}

/// `strider_tempt_items` — ridden-strider steering items.
pub fn strider_tempt_items() -> &'static [&'static str] {
    &["minecraft:warped_fungus", "minecraft:warped_fungus_on_a_stick"]
}

/// `spawn_egg_entity(item)` — the entity id a `*_spawn_egg` places
/// (`"minecraft:creeper_spawn_egg"`/`"creeper_spawn_egg"` → `"minecraft:creeper"`).
/// All 79 vanilla eggs resolve; also used on a monster spawner to override it.
pub fn spawn_egg_entity(item: &str) -> Option<&'static str> {
    let n = item.strip_prefix("minecraft:").unwrap_or(item);
    SPAWN_EGGS
        .iter()
        .find(|(i, _)| *i == n)
        .map(|(_, e)| *e)
}

/// All 79 spawn-egg → entity pairs (registry order within `data_v767.rs`).
static SPAWN_EGGS: &[(&str, &str)] = &[

    ("allay_spawn_egg","minecraft:allay"), ("armadillo_spawn_egg","minecraft:armadillo"),
    ("axolotl_spawn_egg","minecraft:axolotl"), ("bat_spawn_egg","minecraft:bat"),
    ("bee_spawn_egg","minecraft:bee"), ("blaze_spawn_egg","minecraft:blaze"),
    ("bogged_spawn_egg","minecraft:bogged"), ("breeze_spawn_egg","minecraft:breeze"),
    ("camel_spawn_egg","minecraft:camel"), ("cat_spawn_egg","minecraft:cat"),
    ("cave_spider_spawn_egg","minecraft:cave_spider"), ("chicken_spawn_egg","minecraft:chicken"),
    ("cod_spawn_egg","minecraft:cod"), ("cow_spawn_egg","minecraft:cow"),
    ("creeper_spawn_egg","minecraft:creeper"), ("dolphin_spawn_egg","minecraft:dolphin"),
    ("donkey_spawn_egg","minecraft:donkey"), ("drowned_spawn_egg","minecraft:drowned"),
    ("elder_guardian_spawn_egg","minecraft:elder_guardian"), ("ender_dragon_spawn_egg","minecraft:ender_dragon"),
    ("enderman_spawn_egg","minecraft:enderman"), ("endermite_spawn_egg","minecraft:endermite"),
    ("evoker_spawn_egg","minecraft:evoker"), ("fox_spawn_egg","minecraft:fox"),
    ("frog_spawn_egg","minecraft:frog"), ("ghast_spawn_egg","minecraft:ghast"),
    ("glow_squid_spawn_egg","minecraft:glow_squid"), ("goat_spawn_egg","minecraft:goat"),
    ("guardian_spawn_egg","minecraft:guardian"), ("hoglin_spawn_egg","minecraft:hoglin"),
    ("horse_spawn_egg","minecraft:horse"), ("husk_spawn_egg","minecraft:husk"),
    ("iron_golem_spawn_egg","minecraft:iron_golem"), ("llama_spawn_egg","minecraft:llama"),
    ("magma_cube_spawn_egg","minecraft:magma_cube"), ("mooshroom_spawn_egg","minecraft:mooshroom"),
    ("mule_spawn_egg","minecraft:mule"), ("ocelot_spawn_egg","minecraft:ocelot"),
    ("panda_spawn_egg","minecraft:panda"), ("parrot_spawn_egg","minecraft:parrot"),
    ("phantom_spawn_egg","minecraft:phantom"), ("pig_spawn_egg","minecraft:pig"),
    ("piglin_brute_spawn_egg","minecraft:piglin_brute"), ("piglin_spawn_egg","minecraft:piglin"),
    ("pillager_spawn_egg","minecraft:pillager"), ("polar_bear_spawn_egg","minecraft:polar_bear"),
    ("pufferfish_spawn_egg","minecraft:pufferfish"), ("rabbit_spawn_egg","minecraft:rabbit"),
    ("ravager_spawn_egg","minecraft:ravager"), ("salmon_spawn_egg","minecraft:salmon"),
    ("sheep_spawn_egg","minecraft:sheep"), ("shulker_spawn_egg","minecraft:shulker"),
    ("silverfish_spawn_egg","minecraft:silverfish"), ("skeleton_horse_spawn_egg","minecraft:skeleton_horse"),
    ("skeleton_spawn_egg","minecraft:skeleton"), ("slime_spawn_egg","minecraft:slime"),
    ("sniffer_spawn_egg","minecraft:sniffer"), ("snow_golem_spawn_egg","minecraft:snow_golem"),
    ("spider_spawn_egg","minecraft:spider"), ("squid_spawn_egg","minecraft:squid"),
    ("stray_spawn_egg","minecraft:stray"), ("strider_spawn_egg","minecraft:strider"),
    ("tadpole_spawn_egg","minecraft:tadpole"), ("trader_llama_spawn_egg","minecraft:trader_llama"),
    ("tropical_fish_spawn_egg","minecraft:tropical_fish"), ("turtle_spawn_egg","minecraft:turtle"),
    ("vex_spawn_egg","minecraft:vex"), ("villager_spawn_egg","minecraft:villager"),
    ("vindicator_spawn_egg","minecraft:vindicator"), ("wandering_trader_spawn_egg","minecraft:wandering_trader"),
    ("warden_spawn_egg","minecraft:warden"), ("witch_spawn_egg","minecraft:witch"),
    ("wither_skeleton_spawn_egg","minecraft:wither_skeleton"), ("wither_spawn_egg","minecraft:wither"),
    ("wolf_spawn_egg","minecraft:wolf"), ("zoglin_spawn_egg","minecraft:zoglin"),
    ("zombie_horse_spawn_egg","minecraft:zombie_horse"), ("zombie_spawn_egg","minecraft:zombie"),
    ("zombie_villager_spawn_egg","minecraft:zombie_villager"), ("zombified_piglin_spawn_egg","minecraft:zombified_piglin"),

];

// ---- Equipable / WorldSpecial helpers --------------------------------------

/// The equipment slot a right-click-equippable item targets (`Equipable`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum EquipSlot {
    Head,
    Chest,
    Legs,
    Feet,
    /// Horse-armor/wolf-armor/saddle style body slot (equipped on the entity).
    Body,
    /// Shield: held use blocks in vanilla; classified `Equipable` per spec
    /// (offhand destination when moved via inventory).
    Offhand,
}

/// `equip_slot(item)` — the vanilla `Equipable`/`EquipmentSlot` target for
/// every equippable item; `None` for non-equippables.
pub fn equip_slot(item: &str) -> Option<EquipSlot> {
    let n = item.strip_prefix("minecraft:").unwrap_or(item);
    Some(match n {
        "shield" => EquipSlot::Offhand,
        "saddle" | "iron_horse_armor" | "golden_horse_armor"
        | "diamond_horse_armor" | "leather_horse_armor" | "wolf_armor" => {
            EquipSlot::Body
        }
        "elytra" => EquipSlot::Chest,
        "carved_pumpkin" | "skeleton_skull" | "wither_skeleton_skull"
        | "zombie_head" | "creeper_head" | "player_head" | "dragon_head"
        | "piglin_head" | "turtle_helmet" => EquipSlot::Head,
        _ if n.ends_with("_helmet") => EquipSlot::Head,
        _ if n.ends_with("_chestplate") => EquipSlot::Chest,
        _ if n.ends_with("_leggings") => EquipSlot::Legs,
        _ if n.ends_with("_boots") => EquipSlot::Feet,
        _ => return None,
    })
}

/// `is_music_disc(item)` — the 19 vanilla 1.21.1 discs (jukebox-insertable).
pub fn is_music_disc(item: &str) -> bool {
    let n = item.strip_prefix("minecraft:").unwrap_or(item);
    n.starts_with("music_disc_")
}

/// The bucket contents kind for a `*_bucket` item, for cauldron/place logic.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BucketContent {
    Empty,
    Water,
    Lava,
    PowderSnow,
    Milk,
    /// Bucketable mob release — entity name carried.
    Mob(&'static str),
}

/// `bucket_content(item)` — the fluid/mob a bucket item carries.
pub fn bucket_content(item: &str) -> Option<BucketContent> {
    let n = item.strip_prefix("minecraft:").unwrap_or(item);
    Some(match n {
        "bucket" => BucketContent::Empty,
        "water_bucket" => BucketContent::Water,
        "lava_bucket" => BucketContent::Lava,
        "powder_snow_bucket" => BucketContent::PowderSnow,
        "milk_bucket" => BucketContent::Milk,
        "cod_bucket" => BucketContent::Mob("minecraft:cod"),
        "salmon_bucket" => BucketContent::Mob("minecraft:salmon"),
        "pufferfish_bucket" => BucketContent::Mob("minecraft:pufferfish"),
        "tropical_fish_bucket" => BucketContent::Mob("minecraft:tropical_fish"),
        "axolotl_bucket" => BucketContent::Mob("minecraft:axolotl"),
        "tadpole_bucket" => BucketContent::Mob("minecraft:tadpole"),
        _ => return None,
    })
}

