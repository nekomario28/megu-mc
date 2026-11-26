// feature.rs — datapack-driven placement and configured-feature execution for
// megu. Loads placed/configured feature JSON and biome feature lists, runs
// the vanilla placement-modifier chain per chunk (3x3 neighborhood, population
// seeds) and writes block-state overrides into Gen::feat_c.
//
// The block registry, the placement providers/predicates and the generation
// context now live in block_registry.rs, generation_providers.rs and
// generation_context.rs; this module composes them and owns the tree/feature
// registry. State ids still come from assets/blocks_1211.json.

use crate::block_registry::*;
use crate::generation_context::FC;
use crate::generation_math::*;
use crate::generation_providers::*;
use crate::json::{jarr, jbool, jf32, jint, jnum, jparse, js, jstr, J};
use crate::trace_env::{env_flag, env_str};
use crate::worldgen::*;
use std::collections::{HashMap, HashSet};
#[cfg(test)]
use std::sync::atomic::Ordering;
use std::sync::Arc;

/// One End crystal an End spike generated but the block feature pipeline cannot
/// itself materialize (entities live above worldgen). The runtime drains this
/// queue when the End chunk becomes live.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct EndCrystalPlacement {
    pub x: i32,
    pub y: i32,
    pub z: i32,
    pub yaw: f32,
    pub guarded: bool,
}

type EndCrystalQueue = (Vec<EndCrystalPlacement>, HashSet<(i32, i32, i32)>);
static GENERATED_END_CRYSTALS: std::sync::OnceLock<std::sync::Mutex<EndCrystalQueue>> =
    std::sync::OnceLock::new();

fn generated_end_crystals() -> &'static std::sync::Mutex<EndCrystalQueue> {
    GENERATED_END_CRYSTALS.get_or_init(|| std::sync::Mutex::new((Vec::new(), HashSet::new())))
}

fn stage_end_crystal(placement: EndCrystalPlacement) {
    let mut queue = generated_end_crystals()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if queue.1.insert((placement.x, placement.y, placement.z)) {
        queue.0.push(placement);
    }
}

/// Drain every generated End crystal exactly once, in generation order.
pub(crate) fn take_staged_end_crystals() -> Vec<EndCrystalPlacement> {
    let mut queue = generated_end_crystals()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    queue.1.clear();
    std::mem::take(&mut queue.0)
}

/// Alias named for the live-world drain site.
pub(crate) fn drain_generated_end_crystals() -> Vec<EndCrystalPlacement> {
    take_staged_end_crystals()
}

#[cfg(all(test, megu_vanilla_data))]
#[path = "generation_contract_tests.rs"]
mod generation_contract_tests;

// ---------- fast block bitsets and pre-resolved tables ----------

const FEAT_BITSET_WORDS: usize = 24;

#[derive(Clone)]
struct FeatBlockBitset {
    bits: [u64; FEAT_BITSET_WORDS],
    overflow: bool,
}

impl FeatBlockBitset {
    fn empty() -> Self {
        Self {
            bits: [0; FEAT_BITSET_WORDS],
            overflow: false,
        }
    }

    fn insert_block_id(&mut self, bi: i32) {
        if bi < 0 {
            return;
        }
        let u = bi as usize;
        let w = u >> 6;
        if w < FEAT_BITSET_WORDS {
            self.bits[w] |= 1u64 << (u & 63);
        } else {
            self.overflow = true;
        }
    }

    fn from_tag(reg: &BReg, tag: &str) -> Self {
        let mut out = Self::empty();
        for &bi in reg.tag_members(tag) {
            out.insert_block_id(bi as i32);
        }
        out
    }

    fn from_names<'a>(reg: &BReg, names: impl IntoIterator<Item = &'a str>) -> Self {
        let mut out = Self::empty();
        for name in names {
            let st = reg.state_of(name, None);
            if reg.name_of(st) == name {
                if let Some(bi) = reg.block_id_of(st) {
                    out.insert_block_id(bi);
                }
            }
        }
        out
    }

    #[inline]
    fn contains_bid(&self, bi: i32) -> bool {
        let u = bi as usize;
        let w = u >> 6;
        w < FEAT_BITSET_WORDS && (self.bits[w] & (1u64 << (u & 63))) != 0
    }

    #[inline]
    fn contains_state(&self, reg: &BReg, st: u32) -> bool {
        reg.block_id_of(st).is_some_and(|bi| self.contains_bid(bi))
    }
}

enum FastRT {
    True,
    BlockId(i32),
    BlockSet(Box<FeatBlockBitset>),
    State(u32),
    Fallback(RT),
}

impl FastRT {
    fn compile(rt: RT, reg: &BReg) -> Self {
        match rt {
            RT::True => Self::True,
            RT::BlockState(s) => Self::State(s),
            RT::Tag(t) => {
                let bs = FeatBlockBitset::from_tag(reg, &t);
                if bs.overflow {
                    Self::Fallback(RT::Tag(t))
                } else {
                    Self::BlockSet(Box::new(bs))
                }
            }
            RT::Block(b) => {
                let st = reg.state_of(&b, None);
                if reg.name_of(st) == b {
                    if let Some(bi) = reg.block_id_of(st) {
                        return Self::BlockId(bi);
                    }
                }
                Self::Fallback(RT::Block(b))
            }
            other => Self::Fallback(other),
        }
    }

    #[inline]
    fn matches(&self, reg: &BReg, st: u32, y: i32, r: &mut Xor) -> bool {
        match self {
            Self::True => true,
            Self::State(s) => st == *s,
            Self::BlockId(bi) => reg.block_id_of(st) == Some(*bi),
            Self::BlockSet(bs) => bs.contains_state(reg, st),
            Self::Fallback(rt) => test_rt(rt, reg, st, y, r),
        }
    }
}

struct FastOre {
    size: i32,
    discard: f32,
    targets: Vec<(FastRT, u32)>,
    scattered: bool,
}

struct FeatTables {
    air_bid: i32,
    cobble: u32,
    mossy: u32,
    cave_air: u32,
    chest: u32,
    spawner: u32,
    chest_bid: i32,
    spawner_bid: i32,
    ice: u32,
    snow: u32,
    snow_layer_8: u32,
    snow_bid: i32,
    water_bid: i32,
    magma_bid: i32,
    seagrass: u32,
    tall_seagrass_lo: u32,
    tall_seagrass_hi: u32,
    kelp_plant: u32,
    kelp_ages: [u32; 4],
    kelp_bid: i32,
    snow_cannot_survive: FeatBlockBitset,
    snow_can_survive: FeatBlockBitset,
    features_cannot_replace: FeatBlockBitset,
    snowy_blocks: FeatBlockBitset,
}

impl FeatTables {
    fn build(reg: &BReg) -> Self {
        let bid = |name: &str| reg.block_id_of(reg.state_of(name, None)).unwrap_or(-1);
        let cobble = reg.state_of("minecraft:cobblestone", None);
        let mossy = reg.state_of("minecraft:mossy_cobblestone", None);
        let cave_air = reg.state_of("minecraft:cave_air", None);
        let chest = reg.state_of("minecraft:chest", None);
        let spawner = reg.state_of("minecraft:spawner", None);
        let ice = reg.state_of("minecraft:ice", None);
        let snow = reg.state_of("minecraft:snow", None);
        let snow_layer_8 = reg.with_prop(snow, "layers", "8");
        let seagrass = reg.state_of("minecraft:seagrass", None);
        let half = |v: &str| J::Obj(vec![("half".to_string(), J::Str(v.to_string()))]);
        let tall_seagrass_lo = reg.state_of("minecraft:tall_seagrass", Some(&half("lower")));
        let tall_seagrass_hi = reg.state_of("minecraft:tall_seagrass", Some(&half("upper")));
        let kelp_plant = reg.state_of("minecraft:kelp_plant", None);
        let kelp_base = reg.state_of("minecraft:kelp", None);
        let kelp_ages = [
            reg.with_prop(kelp_base, "age", "20"),
            reg.with_prop(kelp_base, "age", "21"),
            reg.with_prop(kelp_base, "age", "22"),
            reg.with_prop(kelp_base, "age", "23"),
        ];
        let mut snowy_blocks = FeatBlockBitset::empty();
        let mut st = 0u32;
        let mut last_bid = -1i32;
        while let Some(b) = reg.block_id_of(st) {
            if b != last_bid {
                last_bid = b;
                if reg.get_prop(st, "snowy").is_some() {
                    snowy_blocks.insert_block_id(b);
                }
            }
            st += 1;
        }
        Self {
            air_bid: bid("minecraft:air"),
            cobble,
            mossy,
            cave_air,
            chest,
            spawner,
            chest_bid: reg.block_id_of(chest).unwrap_or(-1),
            spawner_bid: reg.block_id_of(spawner).unwrap_or(-1),
            ice,
            snow,
            snow_layer_8,
            snow_bid: reg.block_id_of(snow).unwrap_or(-1),
            water_bid: bid("minecraft:water"),
            magma_bid: bid("minecraft:magma_block"),
            seagrass,
            tall_seagrass_lo,
            tall_seagrass_hi,
            kelp_plant,
            kelp_ages,
            kelp_bid: reg.block_id_of(kelp_base).unwrap_or(-1),
            snow_cannot_survive: FeatBlockBitset::from_tag(
                reg,
                "minecraft:snow_layer_cannot_survive_on",
            ),
            snow_can_survive: FeatBlockBitset::from_tag(reg, "minecraft:snow_layer_can_survive_on"),
            features_cannot_replace: FeatBlockBitset::from_tag(
                reg,
                "minecraft:features_cannot_replace",
            ),
            snowy_blocks,
        }
    }
}

fn feat_tables(reg: &BReg) -> &'static FeatTables {
    static TABLES: std::sync::OnceLock<FeatTables> = std::sync::OnceLock::new();
    TABLES.get_or_init(|| FeatTables::build(reg))
}

// ---------- configured features ----------

enum FRef {
    /// a *configured* feature, referenced by name
    Name(String),
    /// a *placed* feature, referenced by name. The configured-feature contract
    /// (random_selector, simple_random_selector, weighted_random_selector,
    /// sequence, ...) holds `Holder<PlacedFeature>`, so the referenced feature's
    /// placement modifiers run too - with the same RNG. Treating the reference as
    /// a bare configured feature dropped those modifiers, which silently removed
    /// e.g. `block_predicate_filter(would_survive)`: every random_selector tree
    /// candidate then grew a tree where vanilla rejected it.
    Placed(String),
    /// Pre-resolved index into `FReg::cfg_vec`.
    CfgIdx(usize),
    /// Pre-resolved `(placed_vec index, ball_mask)`.
    PlacedIdx(usize, u128),
    Inline(Box<Cf>),
}
struct Pf {
    feat: FRef,
    mods: Vec<PM>,
    name: String,
}

enum Cf {
    EndIsland,
    Fossil(Box<Fossil>),
    ChorusPlant,
    Glowstone,
    /// `minecraft:basalt_pillar` (`ebj`, `BasaltPillarFeature`), config `{}`.
    ///
    /// The pillar **hangs**: the cell above the origin must be solid and the origin
    /// itself air (measured, not assumed: `jd.d()` is above in 1.21.1). From there a
    /// basalt column runs down through air. At every level it also tries one sideways
    /// block per horizontal direction, and a direction that draws `nextInt(10) == 0`
    /// is dropped for the rest of the column — the native `flag = flag && place(...)`
    /// short-circuits, so both the write and the RNG draw disappear. At the lowest
    /// level four `nextBoolean` side blocks may hang, then a 7x7 base patch fills where
    /// `nextInt(10) < 10 - |dx|*|dz|`, each cell walked down at most three cells to its
    /// support. Writes outside the build height are refused, as native `setBlock` does.
    BasaltPillar,
    /// `minecraft:void_start_platform` (`edv`), config `{}`: only in the origin's own
    /// chunk when that chunk is within chebyshev 1 of chunk (0,0), writes the start
    /// platform at `origin.y + 3` over the chunk's 16x16 columns, skipping columns
    /// further than chebyshev 16 from the fixed start (8,8). The native sets
    /// `(x, y, z) = (zLoop, y, xLoop)` — transposed — which is the same square.
    VoidStartPlatform,
    /// `minecraft:end_gateway` (`eca`), config `{exact}`: a 3x5x3 box where the origin
    /// becomes `end_gateway`, the origin's whole y layer becomes air, the straight
    /// above/below cells and the axis lines become bedrock, and the rest air. The
    /// exit-position block entity is not modelled (no worldgen block entities yet),
    /// so a placed gateway keeps its default exit.
    EndGateway,
    /// `minecraft:huge_brown_mushroom` (`ecm` over `ebg`), config `{cap_provider,
    /// stem_provider, foliage_radius}`. Measured: the height is `nextInt(3) + 4`,
    /// doubled when the *following* `nextInt(12)` is 0; the origin's below must be in
    /// the `dirt` **tag** (mycelium passes, stone does not) or `mushroom_grow_block`;
    /// every cell of the stem and of the cap layers (centre column up to `dy == 3`,
    /// the full radius square above) must be air or in the `leaves` tag; the stem runs
    /// `0..height`, written only where the cell is not solid-render, and the cap sits
    /// at `y + height` as a `2r+1` square minus its four corners, its `west`/`east`/
    /// `north`/`south` properties overwritten from the cell's edge role.
    HugeBrownMushroom {
        cap: Bsp,
        stem: Bsp,
        radius: i32,
    },
    /// `minecraft:huge_red_mushroom` (`ecp` over `ebg`), config `{cap_provider,
    /// stem_provider, foliage_radius}`. It shares `ebg`'s height draw, trunk and ground
    /// test with the brown mushroom and differs in two measured places: the cap is four
    /// layers `height-3 ..= height` (top layer one radius smaller, corners kept only on
    /// that top layer, `up = dy >= height-1`, `west = dx < -(r-2)` and the rotations),
    /// and the position check is the **centre column only**, because
    /// `getTreeRadiusForHeight(-1, -1, r, dy)` is 0 for every `dy >= 0` for this species.
    HugeRedMushroom {
        cap: Bsp,
        stem: Bsp,
        radius: i32,
    },
    /// `minecraft:pointed_dripstone` (`eda`), config `{chance_of_taller_dripstone,
    /// chance_of_directional_spread, chance_of_spread_radius2, chance_of_spread_radius3}`.
    ///
    /// Measured from `eda` and `ebz` (`DripstoneUtils`), then confirmed cell by cell: the
    /// origin needs a dripstone base (`dripstone_block` or the `dripstone_replaceable_blocks`
    /// tag) below and/or above; both sides is a `nextBoolean` between hanging and standing,
    /// neither refuses. The support cell and its horizontal neighbours become
    /// `dripstone_block` where the tag allows it (three nested `!(nextFloat() > chance)`
    /// gates, one draw each per horizontal direction NORTH/EAST/SOUTH/WEST, then up to two
    /// `Direction.getRandom` steps), and the tip itself is `2` cells only when
    /// `nextFloat() < chance_of_taller_dripstone` *and* the cell beyond the tip is empty or
    /// water. `waterlogged` comes from the real fluid state, not a block-name test.
    PointedDripstone(PointedDripstoneCfg),
    /// `minecraft:coral_tree`/`coral_claw`/`coral_mushroom` (`ebu`/`ebr`/`ebt` over `ebs`),
    /// config `{}`. All three share `CoralFeature.place`: one coral block is drawn from the
    /// `coral_blocks` tag (pack order) before the shape runs, so even a refused placement
    /// consumes that draw. A piece is written only where the cell is water (or in the
    /// `corals` tag) *and* the cell above it is water; writing one may put a coral plant
    /// (25%), a sea pickle (5%) above it, and wall fans on a 20% per horizontal direction.
    Coral(CoralShape),
    /// `minecraft:huge_fungus` (`eco`), config `{valid_base_block, stem_state, hat_state,
    /// decor_state, replaceable_blocks, planted}`.
    ///
    /// Measured from `eco` (and `edx` for the vines): the origin's **below** must be the
    /// config's base block; the height is `nextInt(4..=13)`, doubled on a following
    /// `nextInt(12) == 0`, and (when not `planted`) refused if it would pass the
    /// generation depth. `!planted && nextFloat() < 0.06` makes a 3x3 stem (`huge`), whose
    /// corner cells only get a stem on a 10% roll. The hat is the top
    /// `min(nextInt(1 + height/3) + 5, height)` rows, its radius falling from 3/2 to 1/2 by
    /// row, with a shroomlight/hat/decor roll per cell that depends on the cell's role
    /// (interior, corner, edge). A hat that *is* `nether_wart_block` (the crimson config)
    /// also sprouts weeping-vine columns; the warped config, whose hat is
    /// `warped_wart_block`, never reaches that branch, which is why the fixtures carry both
    /// configs. `planted` configurations clear a non-air block *below* the cell before
    /// writing.
    HugeFungus(HugeFungusCfg),
    /// `minecraft:basalt_columns` (`ebi`), config `{height, reach}` (both IntProviders).
    ///
    /// Measured from `ebi`: the origin must be air, or lava at/below sea level, with a
    /// non-air block below that is not in the fixed cannot-place-on list; `height` is sampled
    /// once, `nextFloat() < 0.9` picks the clustered variant (radius cap 5 / 50 attempts vs
    /// 8 / 15), and each of the `attempts` draws is three `nextInt` (x, y, z) inside a
    /// `radius +-1` box, with `reach` sampled once per surviving candidate. A candidate only
    /// contributes when `height - manhattan(origin, pos) >= 0`; its column then writes basalt
    /// upward from a surface found by walking down (over an ocean) or up (over land) at most
    /// `dist` cells, `remaining - dist / 2 + 1` blocks, continuing through basalt and stopping
    /// at anything else.
    BasaltColumns {
        height: IP,
        reach: IP,
    },
    /// `minecraft:sculk_patch` (`edi`), config `{charge_count, amount_per_charge,
    /// spread_attempts, growth_rounds, spread_rounds, extra_rare_growths, catalyst_chance}`.
    ///
    /// The only feature that carries a subsystem: a world-generation `SculkSpreader` whose
    /// charge cursors walk the 18 face/edge neighbours of the patch, turning
    /// `sculk_replaceable_world_gen` cells into `sculk`/`sculk_vein`, decaying charge by
    /// distance and (world-gen) never merging cursors. The vein spread itself is
    /// **deterministic** - `MultifaceSpreader.spreadAll` maps `Direction.stream()` twice and
    /// draws no random - which is why the port's vein order is exact. `levelEvent`
    /// (sound/particle) and block-shape updates are not modelled.
    SculkPatch(SculkPatchCfg),
    /// `minecraft:twisting_vines` (`edr`), config `{spread_width, spread_height,
    /// max_height}`: the origin needs air with netherrack, warped nylium or warped
    /// wart block below; then `spread_width^2` columns each walk down to their ground
    /// and grow upward, the last cell `twisting_vines` with age 17..25 and the ones
    /// under it `twisting_vines_plant`.
    TwistingVines {
        width: i32,
        height: i32,
        max_height: i32,
    },
    /// `minecraft:weeping_vines` (`edx`), config `{}`: the origin needs air with
    /// netherrack or nether wart block *above*, then a 200-step wart-block sweep and
    /// a 100-step vine sweep run; each column grows downward, ending in
    /// `weeping_vines` with age 17..25.
    WeepingVines,
    /// `minecraft:netherrack_replace_blobs` (`ede`), config `{target, state, radius}`:
    /// the origin is clamped into the build height, walked down to the first `target`
    /// block, and three `radius` samples bound a manhattan ball in which every `target`
    /// cell becomes `state`. Measured: `radius` is sampled three times, not once.
    NetherrackReplaceBlobs {
        target: u32,
        state: u32,
        radius: IP,
    },
    /// `minecraft:delta_feature` (`ebv`), config `{contents, rim, size, rim_size}`:
    /// with `nextFloat() < 0.9` a rim is drawn, then two `size` samples bound the
    /// manhattan ball. A cell qualifies when it is neither `contents` nor in the
    /// fixed cannot-replace list, every horizontal/down neighbour is non-air and the
    /// cell above is air. The rim goes at the cell, `contents` at the rim offset.
    DeltaFeature {
        contents: u32,
        rim: u32,
        size: IP,
        rim_size: IP,
    },
    /// `minecraft:nether_forest_vegetation` (`ecx`): the origin's *below* block must be
    /// in the `minecraft:nylium` tag, then `spread_width^2` offsets are tried, each
    /// drawing three pairs of `nextInt` (x, y, z), sampling the state provider and
    /// placing only where the cell is empty, above the floor and the state's own
    /// `canSurvive` accepts the support (measured: only nylium accepts the nether
    /// plants; netherrack, soul sand, soul soil and warped wart block do not).
    NetherForestVegetation {
        bsp: Bsp,
        width: i32,
        height: i32,
    },
    /// `minecraft:end_platform` (`ecc`), config `{}`: obsidian at `dy = -1` and air
    /// at `dy = 0..2` over a 5x5 box (x from the dz offset, z from the dx offset),
    /// skipping cells that already hold the target *block*.
    EndPlatform,
    /// `minecraft:end_spike` (`edo`, `SpikeFeature`), config `{crystal_invulnerable,
    /// spikes, crystal_beam_target?}`. When `spikes` is empty, derives the 10-pillar
    /// dragon ring from the level's world seed via `LegacyRandomSource` (`Lcg`)
    /// masked with `0xFFFF`. Each spike whose center falls in the origin chunk places
    /// an obsidian cylinder from `min_y` up to `height` (clearing to air above `height`
    /// up to `height + 10` where `y > 65`), an optional 5x4x5 `iron_bars` cage with
    /// explicit edge connections when `guarded`, consumes one crystal-yaw `next_f32()`
    /// draw, and sets `bedrock` at `height` and default `fire` at `height + 1`.
    EndSpike(EndSpikeCfg),
    #[cfg_attr(any(not(test), not(megu_vanilla_data)), allow(dead_code))]
    Ore {
        size: i32,
        discard: f32,
        targets: Vec<(RT, u32)>,
        scattered: bool,
    },
    OreFast(Box<FastOre>),
    Disk {
        half: i32,
        radius: IP,
        bsp: Bsp,
        target: Pr,
    },
    SimpleBlock {
        to: Bsp,
        on: Vec<String>,
        in_: Vec<String>,
        under: Vec<String>,
    },
    Spring {
        fluid: u32,
        fluid_id: i8,
        req_below: bool,
        rock: i32,
        hole: i32,
        valid: HashSet<String>,
        valid_bits: Box<FeatBlockBitset>,
    },
    /// `minecraft:sea_pickle` (`edj`, `SeaPickleFeature`): `count` attempts, each
    /// drawing two `nextInt(8)` apart for an offset in +-8, taking the OCEAN_FLOOR
    /// height there, then drawing the pickle count 1..4 *before* the two survival
    /// tests (the target must be water, and the block below must satisfy
    /// `SeaPickleBlock.mayPlaceOn`: a non-empty UP collision face, or magma).
    SeaPickle {
        count: IP,
    },
    /// `minecraft:ice_spike` (`ecq`, `IceSpikeFeature`), config `{}`: walk down
    /// through air to the first block, require it to be snow, then stack `radius`
    /// that shrinks linearly over `nextInt(4) + 7` layers, with a rare floating
    /// spike and a downward drill that replaces snow/powder/air/ice with packed ice
    /// until y = 50.
    IceSpike,
    /// `minecraft:root_system` (`edg`, `RootSystemFeature`): from the origin, walk
    /// up while the position still passes `allowed_tree_position` and has
    /// `required_vertical_space_for_tree` of air-or-water above it, then try to run
    /// the nested placed feature (the "tree"); on success the same column gets a
    /// root column of `rooted_dirt` up to that height, and then a field of hanging
    /// roots is scattered around the origin.
    RootSystem(Box<RootSystemCfg>),
    /// `minecraft:desert_well` (`ebw`), config `{}`: walk down from the origin's
    /// above-cell while it is empty, require `sand` (the config's
    /// `BlockPredicate.matchesBlocks(SAND)`), then build a slab base plate, a water
    /// column with a sand floor, a rim, four posts, a slab roof on sandstone pillars,
    /// and two `suspicious_sand` blocks at a random horizontal neighbour.
    ///
    /// Known gap, the same one the port's chests carry: the blocks are placed but the
    /// archaeology loot table is not attached to the suspicious sand (no block-entity
    /// loot support), so brushing it yields nothing.
    DesertWell,
    /// `minecraft:large_dripstone` (`ecu`), config `eel`.
    LargeDripstone(Box<LargeDripstoneCfg>),
    /// `minecraft:dripstone_cluster` (`eby`), config `eeg`.
    DripstoneCluster(Box<DripstoneClusterCfg>),
    /// `minecraft:iceberg` (`ecr`), config `{state}`.
    Iceberg {
        state: u32,
    },
    Geode(Box<GeodeCfg>),
    RandSel(Vec<(Pf, f32)>, Option<Box<Pf>>),
    /// `minecraft:random_patch` (`edc`): `tries` times, pick a position by drawing
    /// `nextInt(xz+1) - nextInt(xz+1)` per horizontal axis and the same with
    /// `y_spread + 1` vertically, then run the nested *placed* feature there (its own
    /// modifiers, on this RNG). Skipping the type left every grass/flower/mushroom
    /// patch unplaced (measured: 34 of the pack's 60 configured-feature types had no
    /// arm, and `short_grass` was 0 cells against vanilla's 102).
    RandomPatch {
        feature: Box<Pf>,
        tries: i32,
        xz: i32,
        y: i32,
    },
    /// `minecraft:seagrass` (`edk`): draw dx/dz in ±8, take the OCEAN_FLOOR height
    /// there, and if that cell is water draw `nextDouble() < probability` to choose
    /// tall or short seagrass. The block's own survival rule (`SeagrassBlock`, `dmx`)
    /// is "the block below has a sturdy top face and is not magma" - not the dirt rule.
    Seagrass {
        prob: f32,
    },
    /// Kelp grows upward from OCEAN_FLOOR (`BlockPos.above`, obfuscated `d`), with one
    /// height draw and an age draw only when placing the head (`KelpFeature`, `ecs`).
    Kelp,
    SimpleRand(Vec<Pf>),
    WeightedSel(Vec<(Pf, i32)>),
    BoolSel(Pf, Pf),
    Seq(Vec<Pf>),
    VegPatch {
        depth: IP,
        extra_bottom: f32,
        extra_edge: f32,
        replaceable: String,
        ground: Bsp,
        floor: bool,
        chance: f32,
        vfeat: Box<Pf>,
        xz: IP,
        vrange: i32,
        waterlog: bool,
    },
    BlockColumn {
        layers: Vec<(IP, Bsp)>,
        down: bool,
        allowed: Option<Pr>,
        prio: bool,
    },
    BlockPile(Bsp),
    Lake {
        fluid: Bsp,
        barrier: Bsp,
    },
    BlueIce,
    BlockBlob {
        state: u32,
    },
    UnderMagma {
        floor: i32,
        prob: f32,
        radius: i32,
    },
    FreezeTop,
    /// `minecraft:bonus_chest` (`ebo`, `BonusChestFeature`), config `{}`.
    ///
    /// A one-shot scan of the origin chunk's 16x16 for the first cell that is air or has an
    /// empty collision shape at `MOTION_BLOCKING_NO_LEAVES`, a chest with the spawn-bonus loot
    /// table there, and a torch on each horizontal neighbour whose support can hold one.
    /// Both coordinate lists are shuffled first, so the cell is the fixture, not the origin.
    BonusChest,
    Multiface(Box<MultifaceCfg>),
    Bamboo(f32),
    Vines,
    MonsterRoom,
    Fallen {
        trunk: Bsp,
    },
    Tree(Box<crate::tree::TreeExtra>),
    Unknown(#[expect(dead_code)] String),
}

pub(crate) struct MultifaceCfg {
    block_id: i32,
    base_state: u32,
    waterlogged_base: u32,
    on: HashSet<String>,
    on_bits: FeatBlockBitset,
    dirs: Vec<u8>,
    search: i32,
    spread: f32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct EndSpike {
    pub center_x: i32,
    pub center_z: i32,
    pub radius: i32,
    pub height: i32,
    pub guarded: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct EndSpikeCfg {
    pub crystal_invulnerable: bool,
    pub spikes: Vec<EndSpike>,
    pub crystal_beam_target: Option<(i32, i32, i32)>,
}

fn name_list(j: &J, k: &str) -> Vec<String> {
    match js(j, k) {
        Some(J::Str(s)) => vec![ns_name(s)],
        Some(J::Arr(v)) => v.iter().map(|x| ns_name(x.str())).collect(),
        _ => vec![],
    }
}
fn pf_any(j: &J) -> Option<Pf> {
    match j {
        J::Str(s) => Some(Pf {
            feat: FRef::Placed(ns_name(s)),
            mods: vec![],
            name: String::new(),
        }),
        J::Obj(_) => p_pf(j),
        _ => None,
    }
}
fn p_pf(j: &J) -> Option<Pf> {
    let fref = match js(j, "feature")? {
        J::Str(s) => FRef::Name(ns_name(s)),
        o @ J::Obj(_) => {
            if js(o, "feature").is_some() {
                // placed-feature-shaped inline dict: wrap with no modifiers
                return p_pf(o);
            }
            FRef::Inline(Box::new(p_cf(o)?))
        }
        _ => return None,
    };
    let mut mods = vec![];
    if let Some(ms) = jarr(j, "placement") {
        for m in ms {
            match p_mod(m) {
                Some(pm) => mods.push(pm),
                None => {
                    // A dropped modifier silently changes every later draw, so
                    // make it visible instead of guessing.
                    if env_flag!("PF_UNKNOWN") {
                        eprintln!(
                            "PFUNKNOWN {} modifier={}",
                            jstr(j, "feature").unwrap_or_default(),
                            jstr(m, "type").unwrap_or_default()
                        );
                    }
                    return None;
                }
            }
        }
    }
    Some(Pf {
        feat: fref,
        mods,
        name: String::new(),
    })
}

/// `minecraft:geode` (`eck`, `GeodeFeature`), read from the decompiled 1.21.1
/// class and its config records (`eej`/`dyv`/`dyx`/`dyw` in the mappings):
///
/// - `distribution_points.sample` points are drawn, each offset by
///   `outer_wall_distance` on all three axes, rejected outright when more than
///   `invalid_blocks_threshold` of them land in air or in `invalid_blocks`;
/// - `d0 = points / outer_wall_distance.max` and the layer thresholds are
///   `1/sqrt(layer + d0)` for inner/middle/outer (the crack uses
///   `base_crack_size + random.nextDouble()/2` and adds `d0` only when there are
///   more than three points);
/// - every block of the `[min_gen_offset, max_gen_offset]` cube gets
///   `d6 = sum over points of 1/sqrt(distSqr + point_offset) + noise` where the
///   *noise term is added once per point*, `d7` the same over the crack points;
/// - the cube is walked in `BlockPos.betweenClosed` order, i.e. x fastest and z
///   slowest; the layer providers draw from the feature RNG per block, so that
///   order *is* the draw order;
/// - `d6 < d4` is outside, then the branches are `d1` (the filling provider: the
///   hollow core, and with a crack the six neighbours are replaced by their
///   fluid), `d2` (inner, or the alternate inner at `use_alternate_layer0_chance`
///   - that same draw also decides whether the block becomes an inner placement
///     to hang a crystal on), `d3` (middle) and `d4` (outer);
/// - a block is only written where the existing block is *not* in
///   `cannot_replace`, but the provider is asked for its state either way, which
///   is where the draw happens;
/// - crystals are placed from `inner_placements`, aimed at each of the six
///   directions in turn, `WATERLOGGED` from the target's fluid, and stop at the
///   first direction whose target is air or a water source.
struct GeodeCfg {
    filling: Bsp,
    inner: Bsp,
    alt_inner: Bsp,
    middle: Bsp,
    outer: Bsp,
    placements: Vec<u32>,
    cannot_replace: String,
    invalid: String,
    layers: [f64; 4], // filling, inner, middle, outer
    crack_chance: f64,
    crack_size: f64,
    crack_offset: i32,
    use_alt_layer0: f64,
    use_potential_placements: f64,
    placements_require_alt: bool,
    outer_wall_distance: IP,
    distribution_points: IP,
    point_offset: IP,
    min_gen_offset: i32,
    max_gen_offset: i32,
    noise_multiplier: f64,
    invalid_blocks_threshold: i32,
}

// ---------- large dripstone (`ecu`) ----------

/// `LargeDripstoneConfiguration` (`eel`): nine fields, all used.
pub(crate) struct LargeDripstoneCfg {
    /// `floor_to_ceiling_search_range` (codec default 30), the span scan range.
    range: i32,
    /// `column_radius`: the widest the spike may be, clamped by the cave's height.
    radius: IP,
    /// `height_scale`: sampled once per spike, scales the whole taper.
    height_scale: FP,
    /// `max_column_radius_to_cave_height_ratio`: caps the radius from the span height.
    ratio: f32,
    /// `stalactite_bluntness` / `stalagmite_bluntness`: sampled per direction.
    stalactite_bluntness: FP,
    stalagmite_bluntness: FP,
    /// `wind_speed`: sampled only when both spikes are big enough to sway.
    wind_speed: FP,
    /// `min_radius_for_wind` / `min_bluntness_for_wind`: the sway gate.
    min_radius_for_wind: i32,
    min_bluntness_for_wind: f32,
}

/// `DripstoneUtils.getDripstoneHeight(double,double,double,double)` (`ebz.a`): the
/// taper both dripstone features use. `d` is the distance from the column's axis,
/// `max_radius` its radius, `height_scale` and `bluntness` the config's samples.
/// Vanilla also declares an unused local `0.384` here; the literal appears twice in the
/// expression, which is where that came from.
fn drip_taper(mut d: f64, max_radius: f64, height_scale: f64, bluntness: f64) -> f64 {
    if d < bluntness {
        d = bluntness;
    }
    let t = d / max_radius * 0.384;
    let a = 0.75 * t.powf(1.3333333333333333);
    let b = t.powf(0.6666666666666666);
    let c = 0.3333333333333333 * t.ln();
    let v = height_scale * (a - b - c);
    v.max(0.0) / 0.384 * max_radius
}

/// `DripstoneUtils.isCircleMostlyEmbeddedInStone` (`ebz.a(dds,jd,int)`): the support
/// test both spike searches use - the centre must be solid, and six sample points per
/// unit of radius around it must all be solid too. The loop accumulates in f32 and the
/// step depends on the radius, so the number of samples is part of the behaviour.
fn drip_circle_is_stone(ctx: &mut FC, pos: (i32, i32, i32), radius: i32) -> bool {
    if drip_is_empty_or_water_or_lava(ctx, pos) {
        return false;
    }
    let step = 6.0f32 / radius as f32;
    let mut a = 0.0f32;
    while a < (std::f64::consts::PI * 2.0) as f32 {
        let dx = (mth_cos(a) * radius as f32) as i32;
        let dz = (mth_sin(a) * radius as f32) as i32;
        if drip_is_empty_or_water_or_lava(ctx, (pos.0 + dx, pos.1, pos.2 + dz)) {
            return false;
        }
        a += step;
    }
    true
}

/// `LargeDripstoneFeature$WindOffsetter` (`ecu$b`): the sway. `none()` carries no
/// vector (so every offset is the identity); otherwise the displacement is the wind
/// vector scaled by the distance from the feature's own y, floored.
struct DripWind {
    y: i32,
    v: Option<(f64, f64)>,
}
impl DripWind {
    fn none() -> DripWind {
        DripWind { y: 0, v: None }
    }
    fn new(y: i32, rng: &mut Xor, speed: f32) -> DripWind {
        let a = mth_next_f32(rng, 0.0, std::f32::consts::PI);
        // the components are f32 products widened to the Vec3's doubles
        DripWind {
            y,
            v: Some(((mth_cos(a) * speed) as f64, (mth_sin(a) * speed) as f64)),
        }
    }
    fn at(&self, p: (i32, i32, i32)) -> (i32, i32, i32) {
        match self.v {
            None => p,
            Some((vx, vz)) => {
                let dy = (self.y - p.1) as f64;
                (
                    p.0 + (vx * dy).floor() as i32,
                    p.1,
                    p.2 + (vz * dy).floor() as i32,
                )
            }
        }
    }
}

/// `LargeDripstoneFeature$LargeDripstone` (`ecu$a`): one spike, either hanging from
/// the span's ceiling (`up == false`) or standing on its floor (`up == true`).
struct DripSpike {
    pos: (i32, i32, i32),
    up: bool,
    radius: i32,
    /// The sampled bluntness / height scale, widened to f64 as the fields are.
    bluntness: f64,
    height_scale: f64,
}
impl DripSpike {
    /// `getHeight(float)` (`ecu$a.a(F)`): truncate the taper at a distance from the axis.
    fn height_at(&self, d: f32) -> i32 {
        drip_taper(
            d as f64,
            self.radius as f64,
            self.height_scale,
            self.bluntness,
        ) as i32
    }
    /// `isValid(eel)` (`ecu$a.a(eel)`): the wind only blows on big, blunt spikes.
    fn valid(&self, cfg: &LargeDripstoneCfg) -> bool {
        self.radius >= cfg.min_radius_for_wind
            && self.bluntness >= cfg.min_bluntness_for_wind as f64
    }
    /// `LargeDripstoneFeature$LargeDripstone.place(WorldGenLevel, WindOffsetter)`
    /// (`ecu$a.a(dds,ecu$b)`): find the solid cell the spike grows out of, walking away
    /// from the cave (down for a stalagmite, up for a stalactite) at most
    /// `min(10, getHeight(0))` steps, halving the radius when nothing is found.
    fn find_base(&mut self, ctx: &mut FC, wind: &DripWind) -> bool {
        let Some(reg) = breg() else { return false };
        while self.radius > 1 {
            let mut p = self.pos;
            let steps = self.height_at(0.0).min(10);
            for _ in 0..steps {
                if reg.name_of(ctx.get(p.0, p.1, p.2)) == "minecraft:lava" {
                    return false;
                }
                if drip_circle_is_stone(ctx, wind.at(p), self.radius) {
                    self.pos = p;
                    return true;
                }
                p.1 += if self.up { -1 } else { 1 };
            }
            self.radius /= 2;
        }
        false
    }
    /// `LargeDripstoneFeature$LargeDripstone.placeDripstone(WorldGenLevel,
    /// RandomSource, WindOffsetter)` (`ecu$a.a(dds,ayw,ecu$b)`): the body. Every cell of
    /// the disc gets a taper height, a 1-in-5 chance of a random 0.8..1.0 multiplier, and
    /// dripstone blocks down (or up) until the taper runs out - stopping if it walks
    /// into base stone after having placed something.
    fn build(&self, ctx: &mut FC, rng: &mut Xor, wind: &DripWind) {
        let Some(reg) = breg() else { return };
        let drip = reg.state_of("minecraft:dripstone_block", None);
        for dx in -self.radius..=self.radius {
            for dz in -self.radius..=self.radius {
                // `Mth.sqrt(float)` is a double sqrt narrowed to float
                let dist = ((dx * dx + dz * dz) as f64).sqrt() as f32;
                if dist > self.radius as f32 {
                    continue;
                }
                let mut h = self.height_at(dist);
                if h <= 0 {
                    continue;
                }
                if rng.next_f32() < 0.2 {
                    h = (h as f32 * mth_next_f32(rng, 0.8, 1.0)) as i32;
                }
                let mut p = (self.pos.0 + dx, self.pos.1, self.pos.2 + dz);
                let mut placed = false;
                let surface = if self.up {
                    ctx.hm_ws(p.0, p.2)
                } else {
                    i32::MAX
                };
                let mut i = 0;
                while i < h && p.1 < surface {
                    let q = wind.at(p);
                    let st = ctx.get(q.0, q.1, q.2);
                    // `ebz.b(dcx,jd)` - two arguments, so `isEmptyOrWaterOrLava`, not the
                    // one-argument `ebz.b(dtc)` (`isDripstoneBase`): the spike is built
                    // *in* the empty cell and stops when it reaches base stone.
                    if drip_is_empty_or_water_or_lava(ctx, q) {
                        placed = true;
                        ctx.set(q.0, q.1, q.2, drip);
                    } else if placed && reg.in_tag(st, "minecraft:base_stone_overworld") {
                        break;
                    }
                    p.1 += if self.up { 1 } else { -1 };
                    i += 1;
                }
            }
        }
    }
}

/// `minecraft:large_dripstone` (`ecu`, `LargeDripstoneFeature`).
///
/// The origin's column must be a *two-sided* `Column` (both a floor and a ceiling) at
/// least 4 cells tall - that is the cave the spikes grow into. The radius is the cave
/// height times `max_column_radius_to_cave_height_ratio`, clamped into the config's
/// `column_radius` range, and then drawn once. Two spikes are made from it (a
/// stalactite at `ceiling - 1` and a stalagmite at `floor + 1`), each finding its own
/// solid base, and the wind offsetter is built only if both are big and blunt enough.
///
/// Draw order: the radius, then the first spike's bluntness + height scale, then the
/// second's, then the wind's speed + angle (only when the gate passes), then the builds.
///
/// Note: the jar also carries `LargeDripstoneFeature.placeColumn`
/// (`ecu.a(dds,jd,dyo$b,ecu$b)`), which writes `Blocks.DIAMOND_BLOCK`, `GOLD_BLOCK` and
/// `CREEPER_HEAD` - and nothing calls it (no call site for its descriptor exists in the
/// class, and `place` only calls the two methods above). It is dead in the reference
/// build, so it is deliberately not ported; what *is* live writes `dripstone_block`.
fn gen_large_dripstone(
    ctx: &mut FC,
    rng: &mut Xor,
    origin: (i32, i32, i32),
    cfg: &LargeDripstoneCfg,
) {
    if !drip_is_empty_or_water(ctx, origin) {
        return;
    }
    let Some(col) = drip_column(ctx, origin, cfg.range, DripEnd::DripstoneBaseOrLava) else {
        return;
    };
    let (Some(ceiling), Some(floor)) = (col.ceiling, col.floor) else {
        return;
    };
    let height = ceiling - floor - 1;
    if height < 4 {
        return;
    }
    let est = (height as f32 * cfg.ratio) as i32;
    let est = est.clamp(cfg.radius.min_inclusive(), cfg.radius.max_inclusive());
    let radius = rng.next_inbetween(cfg.radius.min_inclusive(), est);

    let mut stalactite = DripSpike {
        pos: (origin.0, ceiling - 1, origin.2),
        up: false,
        radius,
        bluntness: cfg.stalactite_bluntness.get(rng) as f64,
        height_scale: cfg.height_scale.get(rng) as f64,
    };
    let mut stalagmite = DripSpike {
        pos: (origin.0, floor + 1, origin.2),
        up: true,
        radius,
        bluntness: cfg.stalagmite_bluntness.get(rng) as f64,
        height_scale: cfg.height_scale.get(rng) as f64,
    };
    let wind = if stalactite.valid(cfg) && stalagmite.valid(cfg) {
        let speed = cfg.wind_speed.get(rng);
        DripWind::new(origin.1, rng, speed)
    } else {
        DripWind::none()
    };

    let ok_hang = stalactite.find_base(ctx, &wind);
    let ok_stand = stalagmite.find_base(ctx, &wind);
    if ok_hang {
        stalactite.build(ctx, rng, &wind);
    }
    if ok_stand {
        stalagmite.build(ctx, rng, &wind);
    }
}

// ---------- dripstone cluster (`eby`) ----------

/// `DripstoneClusterConfiguration` (`eeg`). Every field is used, and three of them are
/// providers whose draw count depends on the pack's chosen type.
pub(crate) struct DripstoneClusterCfg {
    /// `floor_to_ceiling_search_range`: how far the empty/water span is scanned.
    range: i32,
    /// `height`: the tip length before the column's own span caps it.
    height: IP,
    /// `radius`: drawn twice (x, then z), so a cluster is an ellipse.
    radius: IP,
    /// `max_stalagmite_stalactite_height_diff`: how far the two tips may differ.
    max_diff: i32,
    /// `height_deviation`: the Gaussian deviation of a tip's length.
    height_deviation: i32,
    /// `dripstone_block_layer_thickness`: layers of dripstone block behind a tip.
    thickness: IP,
    /// `density`: `nextFloat() > density` gives the column no tips at all.
    density: FP,
    /// `wetness`: chance the floor cell becomes a water pool.
    wetness: FP,
    /// `chance_of_dripstone_column_at_max_distance_from_center`.
    chance: f32,
    /// `max_distance_from_edge_affecting_chance_of_dripstone_column`.
    max_dist_edge: i32,
    /// `max_distance_from_center_affecting_height_bias`.
    max_dist_center: i32,
}

/// A configured block-state JSON (`{"Name": ..., "Properties": {...}}`) as a state id.
fn bs_state(j: &J) -> Option<u32> {
    let (name, props) = bs_parts(j)?;
    Some(breg()?.state_of(&name, props))
}

/// `HugeFungusConfiguration` (`ecn`): four block states, a `replaceable_blocks` predicate and
/// the `planted` flag.
pub(crate) struct HugeFungusCfg {
    base: u32,
    stem: u32,
    hat: u32,
    decor: u32,
    /// `replaceable_blocks` (`eaf`): the second chance `canPlace(..., true)` accepts, after
    /// `BlockState::canBeReplaced` itself.
    replaceable: Pr,
    planted: bool,
}

/// `PointedDripstoneConfiguration` (`eer`): four float chances, all defaults present in the
/// codec, which is why the pack's configs may omit any of them.
#[derive(Clone, Copy)]
pub(crate) struct PointedDripstoneCfg {
    /// `chance_of_taller_dripstone`: a two-cell tip instead of one.
    taller: f32,
    /// `chance_of_directional_spread`: per horizontal direction, one `nextFloat` gate.
    directional: f32,
    /// `chance_of_spread_radius2`: a second cell one `Direction.getRandom` step away.
    radius2: f32,
    /// `chance_of_spread_radius3`: a third cell one more step away.
    radius3: f32,
}

/// `Column` (`dyo`): the span of empty/water cells a column sits in. `ceiling` is the
/// first neither-empty-nor-water cell found scanning **up**, `floor` the one found
/// scanning down - so `floor < pos.y < ceiling` whenever both are present.
#[derive(Clone, Copy)]
struct DripCol {
    ceiling: Option<i32>,
    floor: Option<i32>,
}
impl DripCol {
    /// `Column.getHeight()`: only a two-sided column has one.
    fn height(&self) -> Option<i32> {
        match (self.ceiling, self.floor) {
            (Some(c), Some(f)) => Some(c - f - 1),
            _ => None,
        }
    }
}

/// `minecraft:dripstone_cluster` (`eby`, `DripstoneClusterFeature`).
///
/// A cluster is an ellipse of columns (the radius is drawn separately for x and z).
/// Each column finds the empty/water span it sits in, may turn its floor cell into a
/// water pool, then may hang a stalactite from the span's ceiling and stand a
/// stalagmite on its floor. When the two tips would overlap, their meeting point is
/// drawn and both lengths are shortened to it.
///
/// Draw order per column (it matters - the pack's `wetness` and `density` are
/// providers, and both `clamped_normal` and `uniform` floats draw):
/// `nextFloat < wetness`, `nextDouble < chance`, [thickness, height gate + Gaussian],
/// `nextDouble < chance`, [thickness, (randomBetweenInclusive | height gate +
/// Gaussian)], (meeting point), `nextBoolean`.
fn gen_dripstone_cluster(
    ctx: &mut FC,
    rng: &mut Xor,
    origin: (i32, i32, i32),
    cfg: &DripstoneClusterCfg,
) {
    if !drip_is_empty_or_water(ctx, origin) {
        return;
    }
    let height = cfg.height.get(rng);
    let wetness = cfg.wetness.get(rng);
    let density = cfg.density.get(rng);
    let rx = cfg.radius.get(rng);
    let rz = cfg.radius.get(rng);
    for dx in -rx..=rx {
        for dz in -rz..=rz {
            let chance = drip_chance(rx, rz, dx, dz, cfg);
            let p = (origin.0 + dx, origin.1, origin.2 + dz);
            drip_place_column(
                ctx,
                rng,
                p,
                dx,
                dz,
                wetness,
                chance as f64,
                height,
                density,
                cfg,
            );
        }
    }
}

/// `DripstoneUtils.isEmptyOrWater(LevelAccessor, BlockPos)` (`ebz.a(dcx,jd)`), which is
/// also the `start` predicate of the column scan.
fn drip_is_empty_or_water(ctx: &mut FC, pos: (i32, i32, i32)) -> bool {
    let Some(reg) = breg() else { return false };
    let st = ctx.get(pos.0, pos.1, pos.2);
    reg.is_air(st) || reg.name_of(st) == "minecraft:water"
}

/// `DripstoneUtils.isNeitherEmptyNorWater` (`ebz.d`): the column scan's `end`
/// predicate. Note lava counts as neither, so a lava lake can end a column.
fn drip_is_neither_empty_nor_water(ctx: &mut FC, pos: (i32, i32, i32)) -> bool {
    let Some(reg) = breg() else { return false };
    let st = ctx.get(pos.0, pos.1, pos.2);
    !reg.is_air(st) && reg.name_of(st) != "minecraft:water"
}

/// The `end` predicate of a `Column` scan: the cluster stops at the first cell that is
/// neither empty nor water, the large dripstone at the first dripstone base or lava
/// cell (`LargeDripstoneFeature` passes `DripstoneUtils::isDripstoneBaseOrLava`).
#[derive(Clone, Copy, PartialEq)]
enum DripEnd {
    NeitherEmptyNorWater,
    DripstoneBaseOrLava,
}

/// One direction of `Column.scanDirection` (`dyo.a`): walk while the cell is still
/// empty-or-water (at most `range - 1` steps), then accept the cell we stopped on only
/// if it satisfies the end predicate.
fn drip_scan(
    ctx: &mut FC,
    pos: (i32, i32, i32),
    range: i32,
    up: bool,
    end: DripEnd,
) -> Option<i32> {
    let mut y = pos.1;
    let mut i = 1;
    while i < range && drip_is_empty_or_water(ctx, (pos.0, y, pos.2)) {
        y += if up { 1 } else { -1 };
        i += 1;
    }
    let hit = match end {
        DripEnd::NeitherEmptyNorWater => drip_is_neither_empty_nor_water(ctx, (pos.0, y, pos.2)),
        DripEnd::DripstoneBaseOrLava => drip_is_dripstone_base_or_lava(ctx, (pos.0, y, pos.2)),
    };
    if hit {
        Some(y)
    } else {
        None
    }
}

/// `Column.getColumnAt` (`dyo.a`): the up scan is the ceiling, the down scan the floor
/// (that is the field order of `Column.create(floor, ceiling)`).
fn drip_column(ctx: &mut FC, pos: (i32, i32, i32), range: i32, end: DripEnd) -> Option<DripCol> {
    if !drip_is_empty_or_water(ctx, pos) {
        return None;
    }
    let ceiling = drip_scan(ctx, pos, range, true, end);
    let floor = drip_scan(ctx, pos, range, false, end);
    Some(DripCol { ceiling, floor })
}

/// `DripstoneUtils.isEmptyOrWaterOrLava` (`ebz.b(dcx,jd)`): the circle test's predicate.
fn drip_is_empty_or_water_or_lava(ctx: &mut FC, pos: (i32, i32, i32)) -> bool {
    let Some(reg) = breg() else { return false };
    let st = ctx.get(pos.0, pos.1, pos.2);
    reg.is_air(st) || reg.name_of(st) == "minecraft:water" || reg.name_of(st) == "minecraft:lava"
}

/// `DripstoneUtils.isDripstoneBase(BlockState)` (`ebz.b(dtc)`): dripstone block, or a
/// block in `dripstone_replaceable_blocks` (which includes the overworld base stones).
fn drip_is_dripstone_base(reg: &BReg, st: u32) -> bool {
    reg.name_of(st) == "minecraft:dripstone_block"
        || reg.in_tag(st, "minecraft:dripstone_replaceable_blocks")
}

/// `DripstoneUtils.isDripstoneBaseOrLava(BlockState)` (`ebz.a(dtc)`): the large
/// dripstone's column end predicate.
fn drip_is_dripstone_base_or_lava(ctx: &mut FC, pos: (i32, i32, i32)) -> bool {
    let Some(reg) = breg() else { return false };
    let st = ctx.get(pos.0, pos.1, pos.2);
    drip_is_dripstone_base(reg, st) || reg.name_of(st) == "minecraft:lava"
}

/// `DripstoneClusterFeature.getChanceOfStalagmiteOrStalactite`
/// (`eby.a(int,int,int,int,eeg)`): the config's chance at the cluster's edge rising to
/// 1.0 `max_dist_edge` cells inside it (float `clampedMap` - the casts are in the
/// bytecode too).
fn drip_chance(rx: i32, rz: i32, dx: i32, dz: i32, cfg: &DripstoneClusterCfg) -> f32 {
    let a = rx - dx.abs();
    let b = rz - dz.abs();
    let m = a.min(b);
    clamped_map_f32(m as f32, 0.0, cfg.max_dist_edge as f32, cfg.chance, 1.0)
}

/// `DripstoneClusterFeature.getDripstoneHeight` (`eby.a(ayw,int,int,float,int,eeg)`):
/// a `nextFloat` gate against the density, then a biased Gaussian whose mean falls from
/// half the maximum at the cluster's centre to zero at `max_dist_center`.
fn drip_height(
    rng: &mut Xor,
    dx: i32,
    dz: i32,
    density: f32,
    max: i32,
    cfg: &DripstoneClusterCfg,
) -> i32 {
    if rng.next_f32() > density {
        return 0;
    }
    let dist = dx.abs() + dz.abs();
    let mean = clamped_map_f64(
        dist as f64,
        0.0,
        cfg.max_dist_center as f64,
        max as f64 / 2.0,
        0.0,
    ) as f32;
    fp_clamped_normal(rng, mean, cfg.height_deviation as f32, 0.0, max as f32) as i32
}

/// `DripstoneClusterFeature.canBeAdjacentToWater` (`eby.a(dcx,jd)`): base stone, or a
/// water fluid state (the port models water as a block, so the fluid test is a name
/// test).
fn drip_can_be_adjacent_to_water(ctx: &mut FC, pos: (i32, i32, i32)) -> bool {
    let Some(reg) = breg() else { return false };
    let st = ctx.get(pos.0, pos.1, pos.2);
    reg.in_tag(st, "minecraft:base_stone_overworld") || reg.name_of(st) == "minecraft:water"
}

/// `DripstoneClusterFeature.isLava` (`eby.a(dcz,jd)`). Note the decompiled call sites
/// cast to `dcz` (LevelReader) to pick this overload: `eby` has a *second* `a(dcx,jd)`
/// (`canBeAdjacentToWater`), and reading the wrong one makes every stone ceiling reject
/// its own stalactite - which is how this was found (the fixture grew nothing).
fn drip_is_lava(ctx: &mut FC, pos: (i32, i32, i32)) -> bool {
    let Some(reg) = breg() else { return false };
    reg.name_of(ctx.get(pos.0, pos.1, pos.2)) == "minecraft:lava"
}

/// `DripstoneClusterFeature.canPlacePool` (`eby.b`). The water-below test is *negated*
/// in the bytecode as it appears here: a cell that already has water under it is
/// rejected, and the cell must instead be base stone / water on all four sides and
/// above.
fn drip_can_place_pool(ctx: &mut FC, pos: (i32, i32, i32)) -> bool {
    let Some(reg) = breg() else { return false };
    let n = reg.name_of(ctx.get(pos.0, pos.1, pos.2));
    if n == "minecraft:water"
        || n == "minecraft:dripstone_block"
        || n == "minecraft:pointed_dripstone"
    {
        return false;
    }
    if reg.name_of(ctx.get(pos.0, pos.1 - 1, pos.2)) == "minecraft:water" {
        return false;
    }
    // `Direction.Plane.HORIZONTAL` is NORTH, EAST, SOUTH, WEST.
    for (dx, dz) in [(0, -1), (1, 0), (0, 1), (-1, 0)] {
        if !drip_can_be_adjacent_to_water(ctx, (pos.0 + dx, pos.1, pos.2 + dz)) {
            return false;
        }
    }
    drip_can_be_adjacent_to_water(ctx, (pos.0, pos.1 + 1, pos.2))
}

/// `DripstoneUtils.placeDripstoneBlockIfPossible` (`ebz.c(dcx,jd)`): a block in the
/// `dripstone_replaceable_blocks` tag becomes a dripstone block, and nothing else does.
/// The boolean *is* the loop's continue condition in `replaceBlocks...`.
fn drip_place_dripstone_block(ctx: &mut FC, pos: (i32, i32, i32)) -> bool {
    let Some(reg) = breg() else { return false };
    let st = ctx.get(pos.0, pos.1, pos.2);
    if reg.in_tag(st, "minecraft:dripstone_replaceable_blocks") {
        // native `setBlock` refuses out-of-height cells but the method still reports true
        if !ctx.out_of_h(pos.1) {
            let block = reg.state_of("minecraft:dripstone_block", None);
            ctx.set(pos.0, pos.1, pos.2, block);
        }
        true
    } else {
        false
    }
}

/// `DripstoneClusterFeature.replaceBlocksWithDripstoneBlocks` (`eby.a(dds,jd,int,ji)`).
fn drip_replace_blocks_with_dripstone_blocks(
    ctx: &mut FC,
    pos: (i32, i32, i32),
    thickness: i32,
    up: bool,
) {
    let mut p = pos;
    for _ in 0..thickness {
        if !drip_place_dripstone_block(ctx, p) {
            return;
        }
        p.1 += if up { 1 } else { -1 };
    }
}

/// `DripstoneUtils.createPointedDripstone` (`ebz.a(ji,dtz)`).
fn drip_pointed(reg: &BReg, up: bool, thickness: &str) -> u32 {
    let st = reg.state_of("minecraft:pointed_dripstone", None);
    let st = reg.with_prop(st, "vertical_direction", if up { "up" } else { "down" });
    reg.with_prop(st, "thickness", thickness)
}

/// `DripstoneUtils.buildBaseToTipColumn` (`ebz.a(ji,int,boolean,Consumer)`): base,
/// then `height - 3` middles, then frustum, then the tip (`tip_merge` when the two
/// tips of a column meet). Each layer is placed where the consumer's mutable position
/// currently is, then the position moves on.
fn drip_build_base_to_tip(ctx: &mut FC, pos: (i32, i32, i32), up: bool, height: i32, merge: bool) {
    let Some(reg) = breg() else { return };
    let mut steps: Vec<&str> = Vec::new();
    if height >= 3 {
        steps.push("base");
        steps.extend(std::iter::repeat_n("middle", (height - 3) as usize));
    }
    if height >= 2 {
        steps.push("frustum");
    }
    if height >= 1 {
        steps.push(if merge { "tip_merge" } else { "tip" });
    }
    let dir = if up { 1 } else { -1 };
    let mut m = pos;
    for t in steps {
        let st = drip_pointed(reg, up, t);
        // `state.setValue(WATERLOGGED, level.isWaterAt(m))`
        let wl = reg.name_of(ctx.get(m.0, m.1, m.2)) == "minecraft:water";
        let st = reg.with_prop(st, "waterlogged", if wl { "true" } else { "false" });
        // native `setBlock` refuses cells outside the build height (a tip at the top of the
        // world is cut off); done here because every drip feature writes through this helper
        if !ctx.out_of_h(m.1) {
            ctx.set(m.0, m.1, m.2, st);
        }
        m.1 += dir;
    }
}

/// `DripstoneUtils.growPointedDripstone` (`ebz.a(dcx,jd,ji,int,boolean)`): the tip only
/// grows when the cell opposite the growth direction is dripstone block or in the
/// `dripstone_replaceable_blocks` tag (`isDripstoneBase`).
fn drip_grow_pointed_dripstone(
    ctx: &mut FC,
    pos: (i32, i32, i32),
    up: bool,
    height: i32,
    merge: bool,
) {
    let Some(reg) = breg() else { return };
    let base = ctx.get(pos.0, pos.1 + if up { -1 } else { 1 }, pos.2);
    if !drip_is_dripstone_base(reg, base) {
        return;
    }
    drip_build_base_to_tip(ctx, pos, up, height, merge);
}

/// `DripstoneClusterFeature.placeColumn` (`eby.a(dds,ayw,jd,int,int,float,double,int,
/// float,eeg)`) - one column of the cluster.
#[allow(clippy::too_many_arguments)]
fn drip_place_column(
    ctx: &mut FC,
    rng: &mut Xor,
    pos: (i32, i32, i32),
    dx: i32,
    dz: i32,
    wetness: f32,
    chance: f64,
    height: i32,
    density: f32,
    cfg: &DripstoneClusterCfg,
) {
    let Some(reg) = breg() else { return };
    let Some(col) = drip_column(ctx, pos, cfg.range, DripEnd::NeitherEmptyNorWater) else {
        return;
    };
    let (ceiling, floor) = (col.ceiling, col.floor);
    if ceiling.is_none() && floor.is_none() {
        return;
    }

    let wet = rng.next_f32() < wetness;
    let col2 = match (wet, floor) {
        (true, Some(f)) if drip_can_place_pool(ctx, (pos.0, f, pos.2)) => {
            // The floor cell itself becomes water and the column's floor moves one
            // down, so a later stalagmite stands in the pool.
            let water = reg.state_of("minecraft:water", None);
            ctx.set(pos.0, f, pos.2, water);
            DripCol {
                ceiling,
                floor: Some(f - 1),
            }
        }
        _ => col,
    };
    let floor2 = col2.floor;

    let roll1 = rng.next_f64() < chance;
    let h_ceil = match ceiling {
        Some(c) if roll1 && !drip_is_lava(ctx, (pos.0, c, pos.2)) => {
            let t = cfg.thickness.get(rng);
            drip_replace_blocks_with_dripstone_blocks(ctx, (pos.0, c, pos.2), t, true);
            let max = match floor2 {
                Some(f) => height.min(c - f),
                None => height,
            };
            drip_height(rng, dx, dz, density, max, cfg)
        }
        _ => 0,
    };

    let roll2 = rng.next_f64() < chance;
    let h_floor = match floor2 {
        Some(f) if roll2 && !drip_is_lava(ctx, (pos.0, f, pos.2)) => {
            let t = cfg.thickness.get(rng);
            drip_replace_blocks_with_dripstone_blocks(ctx, (pos.0, f, pos.2), t, false);
            match ceiling {
                Some(_) => (h_ceil + rng.next_inbetween(-cfg.max_diff, cfg.max_diff)).max(0),
                None => drip_height(rng, dx, dz, density, height, cfg),
            }
        }
        _ => 0,
    };

    // If the two tips would meet, draw the meeting point inside the span both can
    // reach and shorten each side to it.
    let (h1, h2) = match (ceiling, floor2) {
        (Some(c), Some(f)) if c - h_ceil <= f + h_floor => {
            let lo = (c - h_ceil).max(f + 1);
            let hi = (f + h_floor).min(c - 1);
            let pick = rng.next_inbetween(lo, hi + 1);
            (c - pick, pick - 1 - f)
        }
        _ => (h_ceil, h_floor),
    };

    let merge = rng.next_bool()
        && h1 > 0
        && h2 > 0
        && col2.height().is_some()
        && h1 + h2 == col2.height().unwrap();

    if let Some(c) = ceiling {
        drip_grow_pointed_dripstone(ctx, (pos.0, c - 1, pos.2), false, h1, merge);
    }
    if let Some(f) = floor2 {
        drip_grow_pointed_dripstone(ctx, (pos.0, f + 1, pos.2), true, h2, merge);
    }
}

/// `minecraft:iceberg` (`ecr`, `IcebergFeature`), config `{state}` (packed_ice for
/// `iceberg_packed`, blue_ice for `iceberg_blue`). The origin is moved to sea level,
/// then two shapes are possible: a "tall" one (`flag1`, chosen by a `nextDouble`) and
/// a flat one, each built from a height-dependent radius and then carved back with a
/// signed-distance test (a circle for the flat shape, an ellipse rotated by
/// `angle` for the tall one).
///
/// Letters resolved in the mappings first - this feature is dense with them:
/// `Blocks.a/G/L/dN/dO/dP/iC/mW` are AIR / WATER / GRAVEL / SNOW / ICE / SNOW_BLOCK /
/// PACKED_ICE / BLUE_ICE; `ChunkGenerator.f()` is `getSeaLevel()`; `RandomSource.h()`
/// is `nextBoolean()`, `.i()` is `nextFloat()`, `.j()` is `nextDouble()`;
/// `Mth.a(FFF)` is `clamp`; `BlockPos.f()/.g()/.h()/.i()` are north/south/west/east.
///
/// Every branch that draws is kept as the reference has it (a `||` still
/// short-circuits, a `nextInt` inside an `if` is still inside it), because the
/// iceberg is drawn from a single stream across all of its helpers.
#[allow(clippy::too_many_arguments)]
fn gen_iceberg(ctx: &mut FC, rng: &mut Xor, origin: (i32, i32, i32), state: u32) {
    let pos = (origin.0, ctx.g.sea, origin.2);
    let fire = rng.next_f64() > 0.7;
    let angle = rng.next_f64() * 2.0 * std::f64::consts::PI;
    let i = 11 - rng.next_bound(5);
    let j = 3 + rng.next_bound(3);
    let tall = rng.next_f64() > 0.7;
    let mut l = if tall {
        rng.next_bound(6) + 6
    } else {
        rng.next_bound(15) + 3
    };
    if !tall && rng.next_f64() > 0.9 {
        l += rng.next_bound(19) + 7;
    }
    let i1 = (l + rng.next_bound(11)).min(18);
    let j1 = (l + rng.next_bound(7) - rng.next_bound(5)).min(11);
    let k1 = if tall { i } else { 11 };

    for dx in -k1..k1 {
        for dz in -k1..k1 {
            for y in 0..l {
                let r = if tall {
                    iceberg_radius_round(y, l, j1)
                } else {
                    iceberg_radius_ellipse(rng, y, l, j1)
                };
                if tall || dx < r {
                    iceberg_block(
                        ctx, rng, pos, l, dx, y, dz, r, k1, tall, j, angle, fire, state,
                    );
                }
            }
        }
    }
    iceberg_smooth(ctx, pos, j1, l, tall, i);
    for dx in -k1..k1 {
        for dz in -k1..k1 {
            let mut y = -1;
            while y > -i1 {
                // `Mth.floor(k1 * (1 - y^2 / (i1 * 8)))`: the tall shape's narrowing
                // underside
                let r = if tall {
                    (k1 as f64 * (1.0 - (y as f64).powi(2) / (i1 as f64 * 8.0))).floor() as i32
                } else {
                    k1
                };
                let r2 = iceberg_radius_steep(rng, -y, i1, j1);
                if dx < r2 {
                    iceberg_block(
                        ctx, rng, pos, i1, dx, y, dz, r2, r, tall, j, angle, fire, state,
                    );
                }
                y -= 1;
            }
        }
    }
    let top = if tall {
        rng.next_f64() > 0.1
    } else {
        rng.next_f64() > 0.7
    };
    if top {
        iceberg_place_top(ctx, rng, j1, l, pos, tall, i, angle, j);
    }
}

/// `IcebergFeature.heightDependentRadiusEllipse`: one `nextFloat`, plus a
/// `nextInt(5)`/`nextInt(6)` pair only on the tall branch.
fn iceberg_radius_ellipse(rng: &mut Xor, y: i32, max_height: i32, scale: i32) -> i32 {
    let f = 3.5 - rng.next_f32();
    let mut f1 = (1.0 - (y as f64).powi(2) / (max_height as f64 * f as f64)) as f32 * scale as f32;
    if max_height > 15 + rng.next_bound(5) {
        let v = if y < 3 + rng.next_bound(6) { y / 2 } else { y };
        f1 = (1.0 - v as f32 / (max_height as f32 * f * 0.4)) * scale as f32;
    }
    (f1 / 2.0).floor() as i32
}

/// `heightDependentRadiusRound`: no draws.
fn iceberg_radius_round(y: i32, max_height: i32, scale: i32) -> i32 {
    let f1 = (1.0 - (y as f64).powi(2) / max_height as f64) as f32 * scale as f32;
    (f1 / 2.0).floor() as i32
}

/// `heightDependentRadiusSteep`: one `nextFloat`.
fn iceberg_radius_steep(rng: &mut Xor, y: i32, max_height: i32, scale: i32) -> i32 {
    let f = 1.0 + rng.next_f32() / 2.0;
    let f1 = (1.0 - y as f32 / (max_height as f32 * f)) * scale as f32;
    (f1 / 2.0).floor() as i32
}

/// `isIcebergState`: the feature's own three materials.
fn is_iceberg_state(reg: &BReg, st: u32) -> bool {
    matches!(
        reg.name_of(st),
        "minecraft:packed_ice" | "minecraft:snow_block" | "minecraft:blue_ice"
    )
}

/// `signedDistanceEllipse`: the rotated ellipse the tall shape carves with.
fn iceberg_ellipse_dist(
    dx: i32,
    dz: i32,
    offset: (i32, i32, i32),
    r1: i32,
    r2: i32,
    angle: f64,
) -> f64 {
    let x = (dx - offset.0) as f64;
    let z = (dz - offset.2) as f64;
    (((x * angle.cos() - z * angle.sin()) / r1 as f64).powi(2)
        + ((x * angle.sin() + z * angle.cos()) / r2 as f64).powi(2))
        - 1.0
}

/// `signedDistanceCircle`: one `nextFloat` per call.
fn iceberg_circle_dist(dx: i32, dz: i32, offset: (i32, i32, i32), r: i32, rng: &mut Xor) -> f64 {
    let f = 10.0 * rng.next_f32().clamp(0.2, 0.8) as f64 / r as f64;
    f + (dx - offset.0).pow(2) as f64 + (dz - offset.2).pow(2) as f64 - (r as f64).powi(2)
}

/// `heightDependentRadiusOffset`: no draws.
fn iceberg_offset(y: i32, max_height: i32, j: i32) -> i32 {
    if y > 0 && max_height - y <= 3 {
        j - (4 - (max_height - y))
    } else {
        j
    }
}

/// `placeIcebergBlock`: the write gate, with its `nextDouble` and `nextInt` draws.
#[allow(clippy::too_many_arguments)]
fn iceberg_place_block(
    ctx: &mut FC,
    rng: &mut Xor,
    pos: (i32, i32, i32),
    level: i32,
    max_height: i32,
    tall: bool,
    fire: bool,
    state: u32,
) {
    let Some(reg) = breg() else { return };
    let cur = ctx.get(pos.0, pos.1, pos.2);
    let n = reg.name_of(cur);
    if !(reg.is_air(cur)
        || n == "minecraft:snow_block"
        || n == "minecraft:ice"
        || n == "minecraft:water")
    {
        return;
    }
    let roll = if !tall { true } else { rng.next_f64() > 0.05 };
    let d = if tall { 3 } else { 2 };
    let snow = fire
        && n != "minecraft:water"
        && (level as f64)
            <= rng.next_bound((max_height / d).max(1)) as f64 + max_height as f64 * 0.6
        && roll;
    let st = if snow {
        reg.state_of("minecraft:snow_block", None)
    } else {
        state
    };
    ctx.set(pos.0, pos.1, pos.2, st);
}

/// `generateIcebergBlock` (the ellipse/circle test plus the depth guard).
#[allow(clippy::too_many_arguments)]
fn iceberg_block(
    ctx: &mut FC,
    rng: &mut Xor,
    pos: (i32, i32, i32),
    max_height: i32,
    dx: i32,
    y: i32,
    dz: i32,
    r: i32,
    radius_scale: i32,
    tall: bool,
    j: i32,
    angle: f64,
    fire: bool,
    state: u32,
) {
    let d0 = if tall {
        iceberg_ellipse_dist(
            dx,
            dz,
            pos,
            radius_scale,
            iceberg_offset(y, max_height, j),
            angle,
        )
    } else {
        iceberg_circle_dist(dx, dz, pos, r, rng)
    };
    if d0 >= 0.0 {
        return;
    }
    let p = (pos.0 + dx, pos.1 + y, pos.2 + dz);
    let d1 = if tall {
        -0.5
    } else {
        -6.0 - rng.next_bound(3) as f64
    };
    if d0 > d1 && rng.next_f64() > 0.9 {
        return;
    }
    iceberg_place_block(ctx, rng, p, max_height - y, max_height, tall, fire, state);
}

/// `IcebergFeature.smooth`: removes single ice cells whose neighbour count is too
/// low, and cells whose block below is air.
fn iceberg_smooth(
    ctx: &mut FC,
    pos: (i32, i32, i32),
    radius: i32,
    max_height: i32,
    tall: bool,
    i: i32,
) {
    let Some(reg) = breg() else { return };
    let r = if tall { i } else { radius / 2 };
    let air = reg.state_of("minecraft:air", None);
    for dx in -r..=r {
        for dz in -r..=r {
            for y in 0..=max_height {
                let p = (pos.0 + dx, pos.1 + y, pos.2 + dz);
                let cur = ctx.get(p.0, p.1, p.2);
                let n = reg.name_of(cur);
                if !(is_iceberg_state(reg, cur) || n == "minecraft:snow") {
                    continue;
                }
                if ctx.is_air(p.0, p.1 - 1, p.2) {
                    ctx.set(p.0, p.1, p.2, air);
                    ctx.set(p.0, p.1 - 1, p.2, air);
                } else if is_iceberg_state(reg, cur) {
                    let neighbours = [
                        ctx.get(p.0 - 1, p.1, p.2),
                        ctx.get(p.0 + 1, p.1, p.2),
                        ctx.get(p.0, p.1, p.2 - 1),
                        ctx.get(p.0, p.1, p.2 + 1),
                    ];
                    let empty = neighbours
                        .iter()
                        .filter(|&&s| !is_iceberg_state(reg, s))
                        .count();
                    if empty >= 3 {
                        ctx.set(p.0, p.1, p.2, air);
                    }
                }
            }
        }
    }
}

/// `placeTop` and its per-ring helper: the pointed cap above sea level.
#[allow(clippy::too_many_arguments)]
fn iceberg_place_top(
    ctx: &mut FC,
    rng: &mut Xor,
    radius: i32,
    max_height: i32,
    pos: (i32, i32, i32),
    tall: bool,
    i: i32,
    angle: f64,
    j: i32,
) {
    let sx = if rng.next_bool() { -1 } else { 1 };
    let sz = if rng.next_bool() { -1 } else { 1 };
    let mut r1 = rng.next_bound((radius / 2 - 2).max(1));
    if rng.next_bool() {
        r1 = radius / 2 + 1 - rng.next_bound((radius - radius / 2 - 1).max(1));
    }
    let mut r2 = rng.next_bound((radius / 2 - 2).max(1));
    if rng.next_bool() {
        r2 = radius / 2 + 1 - rng.next_bound((radius - radius / 2 - 1).max(1));
    }
    if tall {
        r1 = rng.next_bound((i - 5).max(1));
        r2 = r1;
    }
    let offset = (sx * r1, 0, sz * r2);
    let a = if tall {
        angle + std::f64::consts::FRAC_PI_2
    } else {
        rng.next_f64() * 2.0 * std::f64::consts::PI
    };
    for y in 0..max_height - 3 {
        let r = iceberg_radius_ellipse(rng, y, max_height, radius);
        iceberg_top_ring(ctx, r, y, pos, false, a, offset, i, j);
    }
    let mut y = -1;
    while y > -max_height + rng.next_bound(5) {
        let r = iceberg_radius_steep(rng, -y, max_height, radius);
        iceberg_top_ring(ctx, r, y, pos, true, a, offset, i, j);
        y -= 1;
    }
}

/// The inner ring helper of `placeTop`: it may leave water (the tall shape) or air.
#[allow(clippy::too_many_arguments)]
fn iceberg_top_ring(
    ctx: &mut FC,
    radius: i32,
    y: i32,
    pos: (i32, i32, i32),
    tall: bool,
    angle: f64,
    offset: (i32, i32, i32),
    i: i32,
    j: i32,
) {
    let Some(reg) = breg() else { return };
    let r1 = radius + 1 + i / 3;
    let r2 = (radius - 3).min(3) + j / 2 - 1;
    let air = reg.state_of("minecraft:air", None);
    let water = reg.state_of("minecraft:water", None);
    let snow = reg.state_of("minecraft:snow", None);
    for dx in -r1..r1 {
        for dz in -r1..r1 {
            let d = iceberg_ellipse_dist(dx, dz, offset, r1, r2, angle);
            if d >= 0.0 {
                continue;
            }
            let p = (pos.0 + dx, pos.1 + y, pos.2 + dz);
            let cur = ctx.get(p.0, p.1, p.2);
            if !(is_iceberg_state(reg, cur) || reg.name_of(cur) == "minecraft:snow_block") {
                continue;
            }
            if tall {
                ctx.set(p.0, p.1, p.2, water);
            } else {
                ctx.set(p.0, p.1, p.2, air);
                if ctx.get(p.0, p.1 - 1, p.2) == snow {
                    ctx.set(p.0, p.1 - 1, p.2, air);
                }
            }
        }
    }
}

/// `minecraft:desert_well` (`ebw`). Letters resolved in the mappings first:
/// `Blocks.I/J/aV/jF/G` are SAND / SUSPICIOUS_SAND / SANDSTONE / SANDSTONE_SLAB /
/// WATER, `BlockPos.d()`/`e()` are `above()`/`below()`, and `.f()/.g()/.h()/.i()` are
/// north/south/west/east - that order matters, because the two suspicious sand blocks
/// come from `nextInt(5)` over `[origin, east, south, west, north]`.
fn gen_desert_well(ctx: &mut FC, rng: &mut Xor, pos: (i32, i32, i32)) {
    let Some(reg) = breg() else { return };
    let sand = reg.state_of("minecraft:sand", None);
    let slab = reg.state_of("minecraft:sandstone_slab", None);
    let sandstone = reg.state_of("minecraft:sandstone", None);
    let water = reg.state_of("minecraft:water", None);
    let suspicious = reg.state_of("minecraft:suspicious_sand", None);
    let min_y = ctx.g.min_y;

    let mut p = (pos.0, pos.1 + 1, pos.2);
    while p.1 > min_y + 2 && ctx.is_air(p.0, p.1, p.2) {
        p.1 -= 1;
    }
    if ctx.get(p.0, p.1, p.2) != sand {
        return;
    }
    // the footprint needs ground: two empty cells under one column reject the well
    for dx in -2..=2 {
        for dz in -2..=2 {
            if ctx.is_air(p.0 + dx, p.1 - 1, p.2 + dz) && ctx.is_air(p.0 + dx, p.1 - 2, p.2 + dz) {
                return;
            }
        }
    }

    for dy in -2..=0 {
        for dx in -2..=2 {
            for dz in -2..=2 {
                ctx.set(p.0 + dx, p.1 + dy, p.2 + dz, slab);
            }
        }
    }
    ctx.set(p.0, p.1, p.2, water);
    const HORIZ: [(i32, i32); 4] = [(1, 0), (0, 1), (-1, 0), (0, -1)];
    for (dx, dz) in HORIZ {
        ctx.set(p.0 + dx, p.1, p.2 + dz, water);
    }
    let below = (p.0, p.1 - 1, p.2);
    ctx.set(below.0, below.1, below.2, sand);
    for (dx, dz) in HORIZ {
        ctx.set(below.0 + dx, below.1, below.2 + dz, sand);
    }

    for dx in -2i32..=2 {
        for dz in -2i32..=2 {
            if dx.abs() == 2 || dz.abs() == 2 {
                ctx.set(p.0 + dx, p.1 + 1, p.2 + dz, slab);
            }
        }
    }
    for (dx, dz) in [(2, 0), (-2, 0), (0, 2), (0, -2)] {
        ctx.set(p.0 + dx, p.1 + 1, p.2 + dz, slab);
    }
    for dx in -1..=1 {
        for dz in -1..=1 {
            let st = if dx == 0 && dz == 0 { sandstone } else { slab };
            ctx.set(p.0 + dx, p.1 + 4, p.2 + dz, st);
        }
    }
    for y in 1..=3 {
        for (dx, dz) in [(-1, -1), (-1, 1), (1, -1), (1, 1)] {
            ctx.set(p.0 + dx, p.1 + y, p.2 + dz, sandstone);
        }
    }

    let shaft = [
        p,
        (p.0 + 1, p.1, p.2),
        (p.0, p.1, p.2 + 1),
        (p.0 - 1, p.1, p.2),
        (p.0, p.1, p.2 - 1),
    ];
    let i1 = rng.next_bound(shaft.len() as i32) as usize;
    let first = (shaft[i1].0, shaft[i1].1 + 1, shaft[i1].2);
    ctx.set(first.0, first.1, first.2, suspicious);
    ctx.push_brushable(
        first,
        "minecraft:archaeology/desert_well",
        Some(feature_block_pos_as_long(first)),
    );
    let i2 = rng.next_bound(shaft.len() as i32) as usize;
    let second = (shaft[i2].0, shaft[i2].1 + 2, shaft[i2].2);
    ctx.set(second.0, second.1, second.2, suspicious);
    ctx.push_brushable(
        second,
        "minecraft:archaeology/desert_well",
        Some(feature_block_pos_as_long(second)),
    );
}

fn feature_block_pos_as_long((x, y, z): (i32, i32, i32)) -> i64 {
    ((i64::from(x) & 0x3FF_FFFF) << 38)
        | ((i64::from(z) & 0x3FF_FFFF) << 12)
        | (i64::from(y) & 0xFFF)
}

/// `minecraft:root_system` (`edg`). Obfuscated letters resolved in the official
/// mappings before writing this, because three of them are not guessable:
/// `BlockPos.d()`/`e()` are `above()`/`below()`, `BlockState.i()` is `isAir()`,
/// `BlockState.e()` is `isSolid()`, `LevelReader.u(pos)` is `isEmptyBlock`, and
/// `FluidTags.a`/`b` are WATER/LAVA.
struct RootSystemCfg {
    feature: Pf,
    required_vspace: i32,
    root_radius: i32,
    root_replaceable: String,
    root_state: Bsp,
    root_attempts: i32,
    root_column_max_height: i32,
    hanging_radius: i32,
    hanging_span: i32,
    hanging_state: Bsp,
    hanging_attempts: i32,
    water_for_tree: i32,
    allowed_tree_position: Pr,
}

/// `getFluidState(pos).is(FluidTags.WATER)`: the port knows liquid *blocks*, not
/// fluid states, so a waterlogged solid would not count here. In the azalea and
/// sulfur pool configs the vertical space is open water, so the two agree.
fn is_water_fluid(reg: &BReg, st: u32) -> bool {
    reg.is_liquid(st) && reg.name_of(st).starts_with("minecraft:water")
}

fn is_lava_fluid(reg: &BReg, st: u32) -> bool {
    reg.is_liquid(st) && reg.name_of(st).starts_with("minecraft:lava")
}

/// `RootSystemFeature.isValidVerticalState`: air, or water within
/// `allowed_vertical_water_for_tree` blocks of the tree's base.
fn root_valid_vertical(reg: &BReg, st: u32, i: i32, max_water: i32) -> bool {
    if reg.is_air(st) {
        return true;
    }
    i < max_water && is_water_fluid(reg, st)
}

fn root_has_space_for_tree(
    ctx: &mut FC,
    c: &RootSystemCfg,
    reg: &BReg,
    from: (i32, i32, i32),
) -> bool {
    let mut p = from;
    for i in 1..=c.required_vspace {
        p.1 += 1;
        let st = ctx.get(p.0, p.1, p.2);
        if !root_valid_vertical(reg, st, i, c.water_for_tree) {
            return false;
        }
    }
    true
}

/// One layer of the root column: `root_placement_attempts` tries, each drawing
/// dx and dz in +-`root_radius` *from the layer's centre* (`MutableBlockPos.move`
/// resets to the position it is passed), replacing anything in the
/// `root_replaceable` tag with the provider's state.
fn root_place_layer(
    ctx: &mut FC,
    rng: &mut Xor,
    c: &RootSystemCfg,
    reg: &BReg,
    x: i32,
    y: i32,
    z: i32,
) {
    for _ in 0..c.root_attempts {
        let q = (
            x + rng.next_bound(c.root_radius) - rng.next_bound(c.root_radius),
            y,
            z + rng.next_bound(c.root_radius) - rng.next_bound(c.root_radius),
        );
        if reg.in_tag(ctx.get(q.0, q.1, q.2), &c.root_replaceable) {
            let st = c.root_state.get(ctx, rng, q.0, q.1, q.2);
            ctx.set(q.0, q.1, q.2, st);
        }
    }
}

fn root_place_column(
    ctx: &mut FC,
    rng: &mut Xor,
    c: &RootSystemCfg,
    reg: &BReg,
    from: (i32, i32, i32),
    to_y: i32,
) {
    for y in from.1..to_y {
        root_place_layer(ctx, rng, c, reg, from.0, y, from.2);
    }
}

/// The hanging-root pass around the origin: each try draws a 3-axis offset
/// (dx, dy, dz, each `nextInt(span) - nextInt(span)`), and places only into an
/// empty cell whose block *above* is sturdy on its bottom face - which is also
/// what `HangingRootsBlock.canSurvive` checks, so one test covers both.
fn root_place_hanging(
    ctx: &mut FC,
    rng: &mut Xor,
    c: &RootSystemCfg,
    reg: &BReg,
    origin: (i32, i32, i32),
) {
    for _ in 0..c.hanging_attempts {
        let q = (
            origin.0 + rng.next_bound(c.hanging_radius) - rng.next_bound(c.hanging_radius),
            origin.1 + rng.next_bound(c.hanging_span) - rng.next_bound(c.hanging_span),
            origin.2 + rng.next_bound(c.hanging_radius) - rng.next_bound(c.hanging_radius),
        );
        if !ctx.is_air(q.0, q.1, q.2) {
            continue;
        }
        let st = c.hanging_state.get(ctx, rng, q.0, q.1, q.2);
        let above = (q.0, q.1 + 1, q.2);
        if reg.side_solid(ctx.get(above.0, above.1, above.2), 0) {
            ctx.set(q.0, q.1, q.2, st);
        }
    }
}

fn gen_root_system(ctx: &mut FC, rng: &mut Xor, pos: (i32, i32, i32), c: &RootSystemCfg) {
    let Some(reg) = breg() else { return };
    if !ctx.is_air(pos.0, pos.1, pos.2) {
        return;
    }
    let mut cursor = pos;
    let mut placed_tree = false;
    // `placeHangingRoots` runs only when the tree was placed (vanilla's
    // `if (placeRootsAndTree(...))`), so the draws after it are conditional too
    for i in 0..c.root_column_max_height {
        cursor.1 += 1;
        if !test_pred(
            &c.allowed_tree_position,
            reg,
            ctx,
            cursor.0,
            cursor.1,
            cursor.2,
        ) {
            continue;
        }
        if !root_has_space_for_tree(ctx, c, reg, cursor) {
            continue;
        }
        let below = (cursor.0, cursor.1 - 1, cursor.2);
        let bst = ctx.get(below.0, below.1, below.2);
        // `getFluidState(below).is(LAVA) || !getBlockState(below).isSolid()`
        if is_lava_fluid(reg, bst) || !reg.is_solid(bst) {
            return;
        }
        // The port's features do not report success, and `ctx.wr` is a *map*: writing
        // over a cell that is already recorded does not change its length. Compose the
        // whole map instead of counting it, so a tree that only overwrites still counts.
        let before = ctx.wr.clone();
        gen_pf(&c.feature, freg(), ctx, rng, cursor);
        if ctx.wr != before {
            root_place_column(ctx, rng, c, reg, pos, pos.1 + i);
            placed_tree = true;
            break;
        }
    }
    if placed_tree {
        root_place_hanging(ctx, rng, c, reg, pos);
    }
}

/// `Feature.safeSetBlock` / `GeodeFeature.placeBlock`: skip protected existing
/// blocks; the caller has already consumed its provider/RNG draws.
fn safe_feature_set(ctx: &mut FC, p: (i32, i32, i32), st: u32, cannot_replace: &str) {
    let Some(reg) = breg() else { return };
    if ctx.out_of_h(p.1) {
        return;
    }
    let cur = ctx.get(p.0, p.1, p.2);
    let blocked = if cannot_replace == "minecraft:features_cannot_replace" {
        let ft = feat_tables(reg);
        if !ft.features_cannot_replace.overflow {
            ft.features_cannot_replace.contains_state(reg, cur)
        } else {
            reg.in_tag(cur, cannot_replace)
        }
    } else {
        reg.in_tag(cur, cannot_replace)
    };
    if !blocked {
        ctx.set(p.0, p.1, p.2, st);
    }
}

/// `EndGatewayFeature` (`eca`). See [`Cf::EndGateway`].
fn gen_end_gateway(ctx: &mut FC, pos: (i32, i32, i32)) {
    let Some(reg) = breg() else { return };
    let gateway = reg.state_of("minecraft:end_gateway", None);
    let bedrock = reg.state_of("minecraft:bedrock", None);
    let air = reg.state_of("minecraft:air", None);
    for dx in -1i32..=1 {
        for dy in -2i32..=2 {
            for dz in -1i32..=1 {
                let (x, y, z) = (pos.0 + dx, pos.1 + dy, pos.2 + dz);
                let state = if dx == 0 && dy == 0 && dz == 0 {
                    gateway
                } else if dy == 0 {
                    air
                } else if (dy.abs() == 2 && dx == 0 && dz == 0)
                    || ((dx == 0 || dz == 0) && dy.abs() != 2)
                {
                    bedrock
                } else {
                    air
                };
                // native `setBlock` refuses cells outside the build height
                if !ctx.out_of_h(y) {
                    ctx.set(x, y, z, state);
                }
            }
        }
    }
}

/// `SpikeFeature.SpikeCacheLoader.load(Long)` (`edo$b`) paired with `SpikeFeature.getSpikesForLevel`.
/// Uses `LegacyRandomSource` (`Lcg`) seeded with the world seed, masks `nextLong()` with `0xFFFF`,
/// and shuffles `[0..10]` using `Util.toShuffledList(..., RandomSource.create(key))`.
fn end_spikes_for_seed(world_seed: u64) -> Vec<EndSpike> {
    let mut seed_rng = Lcg::new(world_seed);
    let key = (seed_rng.next_i64() as u64) & 0xFFFF;
    let mut order = [0i32, 1, 2, 3, 4, 5, 6, 7, 8, 9];
    let mut shuf_rng = Lcg::new(key);
    let mut i = order.len();
    while i > 1 {
        let j = shuf_rng.next_bound(i as i32) as usize;
        order.swap(i - 1, j);
        i -= 1;
    }
    let mut spikes = Vec::with_capacity(10);
    for (idx, &l) in order.iter().enumerate() {
        let angle = 2.0 * (-std::f64::consts::PI + (std::f64::consts::PI / 10.0) * (idx as f64));
        let center_x = (42.0 * angle.cos()).floor() as i32;
        let center_z = (42.0 * angle.sin()).floor() as i32;
        let radius = 2 + l / 3;
        let height = 76 + l * 3;
        let guarded = l == 1 || l == 2;
        spikes.push(EndSpike {
            center_x,
            center_z,
            radius,
            height,
            guarded,
        });
    }
    spikes
}

/// `SpikeFeature.placeSpike` (`edo.a(ddl, ayw, efc, edo$a)`).
fn place_end_spike(ctx: &mut FC, rng: &mut Xor, spike: &EndSpike) {
    let Some(reg) = breg() else { return };
    let obsidian = reg.state_of("minecraft:obsidian", None);
    let air = reg.state_of("minecraft:air", None);
    let bedrock = reg.state_of("minecraft:bedrock", None);
    // `BaseFireBlock.getState(level, crystalPos)`: bedrock below is not in `soul_fire_base_blocks`
    // and the pillar top has no flammable neighbours, so native resolves default `minecraft:fire`.
    let fire = reg.state_of("minecraft:fire", None);
    let iron_bars_base = reg.state_of("minecraft:iron_bars", None);

    let r = spike.radius;
    let cx = spike.center_x;
    let cz = spike.center_z;
    let r2_plus_1 = i64::from(r) * i64::from(r) + 1;

    for x in (cx - r)..=(cx + r) {
        for y in ctx.g.min_y..=(spike.height + 10) {
            for z in (cz - r)..=(cz + r) {
                let dx = i64::from(x - cx);
                let dz = i64::from(z - cz);
                if dx * dx + dz * dz <= r2_plus_1 && y < spike.height {
                    if !ctx.out_of_h(y) {
                        ctx.set(x, y, z, obsidian);
                    }
                } else if y > 65 && !ctx.out_of_h(y) {
                    ctx.set(x, y, z, air);
                }
            }
        }
    }

    if spike.guarded {
        for dx in -2i32..=2 {
            for dz in -2i32..=2 {
                for dy in 0i32..=3 {
                    let is_x_edge = dx.abs() == 2;
                    let is_z_edge = dz.abs() == 2;
                    let is_top = dy == 3;
                    if is_x_edge || is_z_edge || is_top {
                        let connect_x = dx == -2 || dx == 2 || is_top;
                        let connect_z = dz == -2 || dz == 2 || is_top;
                        let mut st = iron_bars_base;
                        if connect_x && dz != -2 {
                            st = reg.with_prop(st, "north", "true");
                        }
                        if connect_x && dz != 2 {
                            st = reg.with_prop(st, "south", "true");
                        }
                        if connect_z && dx != -2 {
                            st = reg.with_prop(st, "west", "true");
                        }
                        if connect_z && dx != 2 {
                            st = reg.with_prop(st, "east", "true");
                        }
                        let y = spike.height + dy;
                        if !ctx.out_of_h(y) {
                            ctx.set(cx + dx, y, cz + dz, st);
                        }
                    }
                }
            }
        }
    }

    // `EndCrystal` entity creation consumes `random.nextFloat() * 360.0F` for `yRot`.
    // Blocks are all this feature may write, so the entity spawn crosses the
    // worldgen boundary as an explicit staged record instead of evaporating.
    let crystal_yaw = rng.next_f32() * 360.0;
    if !ctx.out_of_h(spike.height) {
        ctx.set(cx, spike.height, cz, bedrock);
    }
    if !ctx.out_of_h(spike.height + 1) {
        ctx.set(cx, spike.height + 1, cz, fire);
    }
    stage_end_crystal(EndCrystalPlacement {
        x: cx,
        y: spike.height + 1,
        z: cz,
        yaw: crystal_yaw,
        guarded: spike.guarded,
    });
}

/// `SpikeFeature.place` (`edo.a(ecg)`). See [`Cf::EndSpike`].
fn gen_end_spike(ctx: &mut FC, rng: &mut Xor, pos: (i32, i32, i32), cfg: &EndSpikeCfg) {
    let derived;
    let spikes = if cfg.spikes.is_empty() {
        derived = end_spikes_for_seed(ctx.g.wseed());
        derived.as_slice()
    } else {
        cfg.spikes.as_slice()
    };
    let origin_cx = pos.0 >> 4;
    let origin_cz = pos.2 >> 4;
    for spike in spikes {
        if origin_cx == (spike.center_x >> 4) && origin_cz == (spike.center_z >> 4) {
            place_end_spike(ctx, rng, spike);
        }
    }
}

/// Which huge mushroom a config names. `ecm` (brown) and `ecp` (red) both extend `ebg`
/// and differ only in the radius `getTreeRadiusForHeight` reports and in `makeCap`.
#[derive(Clone, Copy)]
enum HugeMushroom {
    /// `ecm`: one `2r+1` square at `y + height`; the position check covers the full
    /// radius above `dy == 3`.
    Brown,
    /// `ecp`: four layers `height-3 ..= height`; the position check covers the centre
    /// column only (see [`huge_mushroom_layer_radius`]).
    Red,
}

/// `ebg.getTreeRadiusForHeight(b, radius, y)` — the radius the position check uses on
/// layer `y`. The brown cap ignores `b` and switches to the full radius above `y == 3`.
/// The red cap compares the layer against `b`, and every caller passes `b == -1`
/// (`a(-1, -1, foliageRadius, dy)`), so it reports `0` for every `dy >= 0`: the red
/// position check inspects the centre column and nothing else. Reusing the brown
/// envelope for red rejects mushrooms that vanilla grows.
fn huge_mushroom_layer_radius(kind: HugeMushroom, b: i32, radius: i32, y: i32) -> i32 {
    match kind {
        HugeMushroom::Brown => {
            if y > 3 {
                radius
            } else {
                0
            }
        }
        HugeMushroom::Red => {
            if (y < b && y >= b - 3) || y == b {
                radius
            } else {
                0
            }
        }
    }
}

/// `AbstractHugeMushroomFeature` (`ebg`): the shared height draw, position check and
/// trunk; the cap is the species' own. See [`Cf::HugeBrownMushroom`] and
/// [`Cf::HugeRedMushroom`].
fn gen_huge_mushroom(
    ctx: &mut FC,
    rng: &mut Xor,
    pos: (i32, i32, i32),
    cap: &Bsp,
    stem: &Bsp,
    radius: i32,
    kind: HugeMushroom,
) {
    let Some(reg) = breg() else { return };
    // `ebg.getTreeHeight`: `nextInt(3) + 4`, doubled on a following `nextInt(12) == 0`.
    let mut height = rng.next_bound(3) + 4;
    if rng.next_bound(12) == 0 {
        height *= 2;
    }
    let (x, y, z) = pos;
    // `isValidPosition`: `y >= minBuildHeight + 1 && y + height + 1 < maxBuildHeight`.
    if y < ctx.g.world_min_y + 1 || y + height + 1 >= ctx.g.world_min_y + ctx.g.world_height {
        return;
    }
    let below = ctx.get(x, y - 1, z);
    // `Feature.isDirt` (`ece.b`) is the DIRT **tag**, not the `dirt` block: mycelium,
    // podzol and mud pass, which the fixtures' mycelium scenes measure.
    if !reg.in_tag(below, "minecraft:dirt") && !reg.in_tag(below, "minecraft:mushroom_grow_block") {
        return;
    }
    // The envelope is per species: `b` is the second argument the caller hard-codes.
    for dy in 0..=height {
        let k = huge_mushroom_layer_radius(kind, -1, radius, dy);
        for dx in -k..=k {
            for dz in -k..=k {
                if !is_air_or_leaves(reg, ctx.get(x + dx, y + dy, z + dz)) {
                    return;
                }
            }
        }
    }
    // `placeTrunk`: the provider is sampled only where the cell is not solid-render.
    for dy in 0..height {
        if !reg.is_solid_render(ctx.get(x, y + dy, z)) {
            let st = stem.get(ctx, rng, x, y, z);
            ctx.set(x, y + dy, z, st);
        }
    }
    match kind {
        HugeMushroom::Brown => brown_mushroom_cap(ctx, rng, pos, cap, radius, height),
        HugeMushroom::Red => red_mushroom_cap(ctx, rng, pos, cap, radius, height),
    }
}

/// `HugeBrownMushroomFeature.makeCap` (`ecm`): a `2r+1` square minus the four corners
/// at `y = origin.y + height`.
fn brown_mushroom_cap(
    ctx: &mut FC,
    rng: &mut Xor,
    pos: (i32, i32, i32),
    cap: &Bsp,
    radius: i32,
    height: i32,
) {
    let Some(reg) = breg() else { return };
    let (x, y, z) = pos;
    let r = radius;
    for dx in -r..=r {
        for dz in -r..=r {
            let west = dx == -r;
            let east = dx == r;
            let north = dz == -r;
            let south = dz == r;
            let dx_edge = west || east;
            let dz_edge = north || south;
            if dx_edge && dz_edge {
                continue;
            }
            let (cx, cy, cz) = (x + dx, y + height, z + dz);
            if reg.is_solid_render(ctx.get(cx, cy, cz)) {
                continue;
            }
            let mut st = cap.get(ctx, rng, x, y, z);
            // The native overwrites all four rim properties, but only when the
            // provider's state has all four (`hasProperty`). A straight edge is true,
            // an edge cell next to a corner also gets the perpendicular one.
            if reg.get_prop(st, "west").is_some()
                && reg.get_prop(st, "east").is_some()
                && reg.get_prop(st, "north").is_some()
                && reg.get_prop(st, "south").is_some()
            {
                let rim = |on: bool| if on { "true" } else { "false" };
                st = reg.with_prop(st, "west", rim(west || (dz_edge && dx == 1 - r)));
                st = reg.with_prop(st, "east", rim(east || (dz_edge && dx == r - 1)));
                st = reg.with_prop(st, "north", rim(north || (dx_edge && dz == 1 - r)));
                st = reg.with_prop(st, "south", rim(south || (dx_edge && dz == r - 1)));
            }
            ctx.set(cx, cy, cz, st);
        }
    }
}

/// `HugeRedMushroomFeature.makeCap` (`ecp`): four layers `height-3 ..= height`.
///
/// Measured from the bytecode and the fixture: the top layer's radius is `radius - 1`;
/// a layer below the top keeps only the cells on exactly one edge (`dx_edge == dz_edge`
/// is the skip condition, so corners and interior both go); and the five properties are
/// set from the layer and the cell — `up = dy >= height - 1`, `west = dx < -(radius-2)`,
/// `east = dx > radius-2`, and the same for `north`/`south` over `dz`. The threshold is
/// `foliageRadius - 2`, not the layer radius, so it does not move with the top layer.
fn red_mushroom_cap(
    ctx: &mut FC,
    rng: &mut Xor,
    pos: (i32, i32, i32),
    cap: &Bsp,
    radius: i32,
    height: i32,
) {
    let Some(reg) = breg() else { return };
    let (x, y, z) = pos;
    let rim = radius - 2;
    for dy in (height - 3)..=height {
        let r = if dy < height { radius } else { radius - 1 };
        for dx in -r..=r {
            for dz in -r..=r {
                let dx_edge = dx == -r || dx == r;
                let dz_edge = dz == -r || dz == r;
                // The guard is `dx_edge == dz_edge`, not "both are edges": on a layer
                // below the top it keeps only the cells on exactly one edge (the rim,
                // 12 cells for r = 2) and drops the corners *and* the interior, which
                // the stem column passes through. The top layer keeps everything.
                if dy < height && dx_edge == dz_edge {
                    continue;
                }
                let (cx, cy, cz) = (x + dx, y + dy, z + dz);
                if reg.is_solid_render(ctx.get(cx, cy, cz)) {
                    continue;
                }
                let mut st = cap.get(ctx, rng, x, y, z);
                if reg.get_prop(st, "west").is_some()
                    && reg.get_prop(st, "east").is_some()
                    && reg.get_prop(st, "north").is_some()
                    && reg.get_prop(st, "south").is_some()
                    && reg.get_prop(st, "up").is_some()
                {
                    let side = |on: bool| if on { "true" } else { "false" };
                    st = reg.with_prop(st, "up", side(dy >= height - 1));
                    st = reg.with_prop(st, "west", side(dx < -rim));
                    st = reg.with_prop(st, "east", side(dx > rim));
                    st = reg.with_prop(st, "north", side(dz < -rim));
                    st = reg.with_prop(st, "south", side(dz > rim));
                }
                ctx.set(cx, cy, cz, st);
            }
        }
    }
}

/// The three coral shapes; `CoralFeature` (`ebs`) is their shared base.
#[derive(Clone, Copy)]
pub(crate) enum CoralShape {
    Tree,
    Claw,
    Mushroom,
}

/// `Direction.Plane.HORIZONTAL`: NORTH, EAST, SOUTH, WEST, with its offset and the
/// `facing` value a wall fan takes. The order is what the shuffles and the per-direction
/// draws index into.
const HORIZONTAL: [(i32, i32, &str); 4] = [
    (0, -1, "north"),
    (1, 0, "east"),
    (0, 1, "south"),
    (-1, 0, "west"),
];

/// `BuiltInRegistries.BLOCK.getRandomElement(tag, random)` (`Util.getRandom` over the tag's
/// ordered member list), as a default block state.
fn random_tag_block(reg: &BReg, tag: &str, rng: &mut Xor) -> Option<u32> {
    let members = reg.tag_members(tag);
    if members.is_empty() {
        return None;
    }
    let bi = members[rng.next_bound(members.len() as i32) as usize];
    Some(reg.default_state(bi))
}

/// `Util.shuffle(list, random)`: Fisher-Yates from the end, `nextInt(i)` for i = size..2, so
/// the last element is never drawn for. The coral plane shuffle, the claw's
/// `shuffledCopy(Stream)`, the multiface spread order and the bonus chest's two
/// `Util.toShuffledList` passes over the origin chunk's coordinate ranges all go through
/// this, because the number and order of those draws is itself a fixture.
fn feature_util_shuffle<T>(list: &mut [T], rng: &mut Xor) {
    for i in (2..=list.len()).rev() {
        let j = rng.next_bound(i as i32) as usize;
        list.swap(j, i - 1);
    }
}

/// Index into [`HORIZONTAL`] one step clockwise / counter-clockwise (N -> E -> S -> W).
fn horizontal_clockwise(i: usize) -> usize {
    (i + 1) % 4
}
fn horizontal_counter_clockwise(i: usize) -> usize {
    (i + 3) % 4
}

/// `CoralFeature.place` (`ebs.a`): draw the shape's coral block, then run the shape.
fn gen_coral(ctx: &mut FC, rng: &mut Xor, pos: (i32, i32, i32), shape: CoralShape) {
    let Some(reg) = breg() else { return };
    let Some(state) = random_tag_block(reg, "minecraft:coral_blocks", rng) else {
        return;
    };
    match shape {
        CoralShape::Tree => coral_tree(ctx, rng, pos, state),
        CoralShape::Claw => coral_claw(ctx, rng, pos, state),
        CoralShape::Mushroom => coral_mushroom(ctx, rng, pos, state),
    }
}

/// `CoralFeature.placeCoralBlock` (`ebs.b`).
///
/// The precondition is the pair: this cell is water or in the `corals` tag, and the cell
/// above it is water. Then the block, a possible plant/pickle above it, and one wall-fan
/// draw per horizontal direction. Every write goes through `setBlock`, which refuses
/// out-of-height cells (a coral at the top of the world is cut off).
fn coral_piece(ctx: &mut FC, rng: &mut Xor, pos: (i32, i32, i32), state: u32) -> bool {
    let Some(reg) = breg() else { return false };
    let (x, y, z) = pos;
    let at = ctx.get(x, y, z);
    let above = (x, y + 1, z);
    let at_above = ctx.get(above.0, above.1, above.2);
    let water = |st: u32| reg.name_of(st) == "minecraft:water";
    if !(water(at) || reg.in_tag(at, "minecraft:corals")) || !water(at_above) {
        return false;
    }
    if !ctx.out_of_h(y) {
        ctx.set(x, y, z, state);
    }
    if rng.next_f32() < 0.25 {
        if let Some(plant) = random_tag_block(reg, "minecraft:corals", rng) {
            if !ctx.out_of_h(above.1) {
                ctx.set(above.0, above.1, above.2, plant);
            }
        }
    } else if rng.next_f32() < 0.05 {
        // `SeaPickleBlock` default state keeps `waterlogged=true`; only `pickles` moves
        let pickles = (rng.next_bound(4) + 1).to_string();
        let st = reg.with_prop(
            reg.state_of("minecraft:sea_pickle", None),
            "pickles",
            &pickles,
        );
        if !ctx.out_of_h(above.1) {
            ctx.set(above.0, above.1, above.2, st);
        }
    }
    for (dx, dz, facing) in HORIZONTAL {
        if rng.next_f32() < 0.2 {
            let p = (x + dx, y, z + dz);
            if reg.name_of(ctx.get(p.0, p.1, p.2)) == "minecraft:water" {
                if let Some(fan) = random_tag_block(reg, "minecraft:wall_corals", rng) {
                    // wall fans carry `facing`; the plain fans in `corals` do not
                    let st = if reg.get_prop(fan, "facing").is_some() {
                        reg.with_prop(fan, "facing", facing)
                    } else {
                        fan
                    };
                    if !ctx.out_of_h(p.1) {
                        ctx.set(p.0, p.1, p.2, st);
                    }
                }
            }
        }
    }
    true
}

/// `CoralTreeFeature` (`ebu`): a trunk of 1-3 pieces, then 2-4 horizontal branches of 2-6
/// pieces with a 25% sideways kink after the first step and after every two straight ones.
fn coral_tree(ctx: &mut FC, rng: &mut Xor, pos: (i32, i32, i32), state: u32) {
    let mut m = pos;
    let trunk = rng.next_bound(3) + 1;
    for _ in 0..trunk {
        if !coral_piece(ctx, rng, m, state) {
            return; // the native `place` still reports success
        }
        m.1 += 1;
    }
    let top = m;
    let branches = rng.next_bound(3) + 2;
    let mut dirs = HORIZONTAL;
    feature_util_shuffle(&mut dirs, rng);
    for &(dx, dz, _) in dirs.iter().take(branches as usize) {
        m = top;
        m.0 += dx;
        m.2 += dz;
        let len = rng.next_bound(5) + 2;
        let mut seg = 0;
        let mut i = 0;
        while i < len && coral_piece(ctx, rng, m, state) {
            seg += 1;
            m.1 += 1;
            if i == 0 || (seg >= 2 && rng.next_f32() < 0.25) {
                m.0 += dx;
                m.2 += dz;
                seg = 0;
            }
            i += 1;
        }
    }
}

/// `CoralClawFeature` (`ebr`): the origin, then 2-3 arms. One arm follows the drawn
/// direction, the others step up and pick between their direction and UP; each arm then
/// runs 2-5 (main) or 3-5 (side) steps along the original direction, stepping up on a 25%
/// roll.
fn coral_claw(ctx: &mut FC, rng: &mut Xor, pos: (i32, i32, i32), state: u32) {
    if !coral_piece(ctx, rng, pos, state) {
        return; // the native `place` reports failure here
    }
    let dir = rng.next_bound(4) as usize; // `Plane.HORIZONTAL.getRandom`
    let n = rng.next_bound(2) + 2;
    let mut arms = [
        dir,
        horizontal_clockwise(dir),
        horizontal_counter_clockwise(dir),
    ];
    feature_util_shuffle(&mut arms, rng);
    // the tail of every arm runs along the *drawn* direction, not along the arm
    let (ddx, ddz, _) = HORIZONTAL[dir];
    for &arm in arms.iter().take(n as usize) {
        let (adx, adz, _) = HORIZONTAL[arm];
        let mut m = pos;
        let len1 = rng.next_bound(2) + 1;
        m.0 += adx;
        m.2 += adz;
        // the draw order is the native's: the side arms pick their growth direction *before*
        // their length, the main arm draws only the length
        let (grow, len2) = if arm == dir {
            ((adx, 0, adz), rng.next_bound(3) + 2)
        } else {
            m.1 += 1;
            let g = if rng.next_bound(2) == 0 {
                (adx, 0, adz)
            } else {
                (0, 1, 0)
            };
            (g, rng.next_bound(3) + 3)
        };
        let mut i = 0;
        while i < len1 && coral_piece(ctx, rng, m, state) {
            m.0 += grow.0;
            m.1 += grow.1;
            m.2 += grow.2;
            i += 1;
        }
        // `move(grow.getOpposite())`, then up: the arm turns back and climbs one
        m.0 -= grow.0;
        m.1 -= grow.1;
        m.2 -= grow.2;
        m.1 += 1;
        for _ in 0..len2 {
            m.0 += ddx;
            m.2 += ddz;
            if !coral_piece(ctx, rng, m, state) {
                break;
            }
            if rng.next_f32() < 0.25 {
                m.1 += 1;
            }
        }
    }
}

/// `CoralMushroomFeature` (`ebt`): a `(dx+1) x (dy+1) x (dz+1)` box (each 3-5) whose shell
/// is thinned by a 10% roll, lowered by `len` (1-3).
fn coral_mushroom(ctx: &mut FC, rng: &mut Xor, pos: (i32, i32, i32), state: u32) {
    // The native's first draw bounds the **y** offset and its second the **x** offset; the
    // loop order is x (outer), y (middle), z (inner), and the shell test below uses the same
    // pairing. Reading the bounds as (dx, dy, dz) in draw order shifts the whole cap.
    let yb = rng.next_bound(3) + 3; // $$4
    let xb = rng.next_bound(3) + 3; // $$5
    let zb = rng.next_bound(3) + 3; // $$6
    let len = rng.next_bound(3) + 1; // $$7
    for lx in 0..=xb {
        for ly in 0..=yb {
            for lz in 0..=zb {
                // the native `if` body is empty: the boolean *is* the skip, and the piece is
                // written as its last conjunct, so the roll only happens on the shell
                let shell = (lx != 0 && lx != xb || ly != 0 && ly != yb)
                    && (lz != 0 && lz != zb || ly != 0 && ly != yb)
                    && (lx != 0 && lx != xb || lz != 0 && lz != zb)
                    && (lx == 0 || lx == xb || ly == 0 || ly == yb || lz == 0 || lz == zb);
                if shell && rng.next_f32().partial_cmp(&0.1) != Some(std::cmp::Ordering::Less) {
                    coral_piece(ctx, rng, (pos.0 + lx, pos.1 + ly - len, pos.2 + lz), state);
                }
            }
        }
    }
}

/// `HugeFungusFeature` (`eco`). See [`Cf::HugeFungus`].
fn gen_huge_fungus(ctx: &mut FC, rng: &mut Xor, pos: (i32, i32, i32), c: &HugeFungusCfg) {
    let Some(reg) = breg() else { return };
    let (x, y, z) = pos;
    // `getValidBasePos`: the block *below* the origin is the config's base block
    if reg.name_of(ctx.get(x, y - 1, z)) != reg.name_of(c.base) {
        return;
    }
    let mut height = rng.next_inbetween(4, 13);
    if rng.next_bound(12) == 0 {
        height *= 2;
    }
    // A `LevelHeightAccessor` question (build height), not `ChunkGenerator.getGenDepth()`:
    // 1.21.1 gives the Nether 0/128 in `noise_settings` and 0/256 in
    // `dimension_type/the_nether.json`, and this bound is the level's.
    if !c.planted && y + height + 1 >= ctx.g.world_min_y + ctx.g.world_height {
        return;
    }
    let huge = !c.planted && rng.next_f32() < 0.06;
    let air = reg.state_of("minecraft:air", None);
    hf_set(ctx, pos, air);
    huge_fungus_stem(ctx, rng, c, pos, height, huge);
    huge_fungus_hat(ctx, rng, c, pos, height, huge);
}

/// `setBlock`, which refuses cells outside the build height.
fn hf_set(ctx: &mut FC, p: (i32, i32, i32), st: u32) {
    if !ctx.out_of_h(p.1) {
        ctx.set(p.0, p.1, p.2, st);
    }
}

/// `HugeFungusFeature.isReplaceable`: `BlockState::canBeReplaced`, or the config's
/// `replaceable_blocks` predicate when the caller allows it. The port's `is_replaceable` is
/// the asset's replaceable bit, which is the same property the block models carry.
fn hf_can_place(ctx: &mut FC, p: (i32, i32, i32), c: &HugeFungusCfg, allow: bool) -> bool {
    let Some(reg) = breg() else { return false };
    if reg.is_replaceable(ctx.get(p.0, p.1, p.2)) {
        return true;
    }
    allow && test_pred(&c.replaceable, reg, ctx, p.0, p.1, p.2)
}

/// `HugeFungusFeature.placeStem`.
fn huge_fungus_stem(
    ctx: &mut FC,
    rng: &mut Xor,
    c: &HugeFungusCfg,
    base: (i32, i32, i32),
    height: i32,
    huge: bool,
) {
    let Some(reg) = breg() else { return };
    let air = reg.state_of("minecraft:air", None);
    let r: i32 = if huge { 1 } else { 0 };
    for dx in -r..=r {
        for dz in -r..=r {
            let corner = huge && dx.abs() == r && dz.abs() == r;
            for dy in 0..height {
                let p = (base.0 + dx, base.1 + dy, base.2 + dz);
                if !hf_can_place(ctx, p, c, true) {
                    continue;
                }
                if c.planted {
                    if !reg.is_air(ctx.get(p.0, p.1 - 1, p.2)) {
                        hf_set(ctx, p, air); // `destroyBlock(pos, true)` in worldgen
                    }
                    hf_set(ctx, p, c.stem);
                } else if corner {
                    if rng.next_f32() < 0.1 {
                        hf_set(ctx, p, c.stem);
                    }
                } else {
                    hf_set(ctx, p, c.stem);
                }
            }
        }
    }
}

/// `HugeFungusFeature.placeHat`: the top `layers` rows, each a square whose radius comes from
/// the row's distance to the top (`2` or `3` below `height - nextInt(3)`, widened by one for
/// `huge`), every cell rolled by its role.
fn huge_fungus_hat(
    ctx: &mut FC,
    rng: &mut Xor,
    c: &HugeFungusCfg,
    base: (i32, i32, i32),
    height: i32,
    huge: bool,
) {
    let Some(reg) = breg() else { return };
    let wart = reg.name_of(c.hat) == "minecraft:nether_wart_block";
    let layers = (rng.next_bound(1 + height / 3) + 5).min(height);
    let start = height - layers;
    let air = reg.state_of("minecraft:air", None);
    for dy in start..=height {
        let mut r = if dy < height - rng.next_bound(3) {
            2
        } else {
            1
        };
        if layers > 8 && dy < start + 4 {
            r = 3;
        }
        if huge {
            r += 1;
        }
        for dx in -r..=r {
            for dz in -r..=r {
                let x_edge = dx == -r || dx == r;
                let z_edge = dz == -r || dz == r;
                let interior = !x_edge && !z_edge && dy != height;
                let corner = x_edge && z_edge;
                let lower = dy < start + 3;
                let p = (base.0 + dx, base.1 + dy, base.2 + dz);
                if !hf_can_place(ctx, p, c, false) {
                    continue;
                }
                if c.planted && !reg.is_air(ctx.get(p.0, p.1 - 1, p.2)) {
                    hf_set(ctx, p, air);
                }
                if lower {
                    if !interior {
                        huge_fungus_hat_block(ctx, rng, p, c.hat, wart);
                    }
                } else if interior {
                    huge_fungus_hat_drop(ctx, rng, c, p, 0.1, 0.2, if wart { 0.1 } else { 0.0 });
                } else if corner {
                    huge_fungus_hat_drop(ctx, rng, c, p, 0.01, 0.7, if wart { 0.083 } else { 0.0 });
                } else {
                    huge_fungus_hat_drop(
                        ctx,
                        rng,
                        c,
                        p,
                        5.0e-4,
                        0.98,
                        if wart { 0.07 } else { 0.0 },
                    );
                }
            }
        }
    }
}

/// `HugeFungusFeature.placeHatBlock`: a hat cell when the cell *below* already carries the hat
/// block, otherwise a 15% chance of one; a wart-block hat can sprout a weeping vine.
fn huge_fungus_hat_block(ctx: &mut FC, rng: &mut Xor, p: (i32, i32, i32), hat: u32, wart: bool) {
    let Some(reg) = breg() else { return };
    // the native checks the cell **below** (`jd.e()`), so a hat block extends the cap upward
    if reg.name_of(ctx.get(p.0, p.1 - 1, p.2)) == reg.name_of(hat) {
        hf_set(ctx, p, hat);
    } else if rng.next_f32() < 0.15 {
        hf_set(ctx, p, hat);
        if wart && rng.next_bound(11) == 0 {
            huge_fungus_weeping_vines(ctx, rng, p);
        }
    }
}

/// `HugeFungusFeature.placeHatDropBlock`: decor, then hat, then (rarely) a weeping vine; the
/// three chances are the cell's role.
fn huge_fungus_hat_drop(
    ctx: &mut FC,
    rng: &mut Xor,
    c: &HugeFungusCfg,
    p: (i32, i32, i32),
    decor_chance: f32,
    hat_chance: f32,
    vine_chance: f32,
) {
    if rng.next_f32() < decor_chance {
        hf_set(ctx, p, c.decor);
    } else if rng.next_f32() < hat_chance {
        hf_set(ctx, p, c.hat);
        if rng.next_f32() < vine_chance {
            huge_fungus_weeping_vines(ctx, rng, p);
        }
    }
}

/// `HugeFungusFeature.placeWeepingVines`: a vine hangs from the cell below the hat when that
/// cell is empty; the length doubles on one `nextInt(7) == 0`.
fn huge_fungus_weeping_vines(ctx: &mut FC, rng: &mut Xor, p: (i32, i32, i32)) {
    let Some(reg) = breg() else { return };
    let m = (p.0, p.1 - 1, p.2);
    if !reg.is_air(ctx.get(m.0, m.1, m.2)) {
        return;
    }
    let mut len = rng.next_inbetween(1, 5);
    if rng.next_bound(7) == 0 {
        len *= 2;
    }
    weeping_vines_column(ctx, rng, m, len, 23, 25);
}

/// `WeepingVinesFeature.placeWeepingVinesColumn` (`edx.a`): body cells while the cell below is
/// empty, the last empty cell (or the last one before a non-empty cell) the aged tip.
fn weeping_vines_column(
    ctx: &mut FC,
    rng: &mut Xor,
    pos: (i32, i32, i32),
    height: i32,
    min_age: i32,
    max_age: i32,
) {
    let Some(reg) = breg() else { return };
    let mut m = pos;
    for i in 0..=height {
        if reg.is_air(ctx.get(m.0, m.1, m.2)) {
            if i == height || !reg.is_air(ctx.get(m.0, m.1 - 1, m.2)) {
                let age = rng.next_inbetween(min_age, max_age).to_string();
                let st = reg.with_prop(reg.state_of("minecraft:weeping_vines", None), "age", &age);
                hf_set(ctx, m, st);
                break;
            }
            hf_set(ctx, m, reg.state_of("minecraft:weeping_vines_plant", None));
        }
        m.1 -= 1;
    }
}

/// `BasaltColumnsFeature` (`ebi`). See [`Cf::BasaltColumns`].
fn gen_basalt_columns(
    ctx: &mut FC,
    rng: &mut Xor,
    pos: (i32, i32, i32),
    height_ip: &IP,
    reach_ip: &IP,
) {
    let Some(reg) = breg() else { return };
    let sea = ctx.g.sea;
    // the guard is the *mutable* overload: air (or ocean lava) here and a buildable block below
    if !bc_can_start(ctx, reg, sea, pos) {
        return;
    }
    let height = height_ip.get(rng);
    // `nextFloat() < 0.9` picks the clustered variant: a smaller radius and more attempts
    let clustered = rng.next_f32() < 0.9;
    let radius = height.min(if clustered { 5 } else { 8 });
    let attempts = if clustered { 50 } else { 15 };
    for _ in 0..attempts {
        // `BlockPos.randomBetweenClosed`: three nextInt per candidate, x then y then z - and
        // the y draw happens even though the range is one cell (`nextInt(1)`), so dropping it
        // shifts every later draw (it did: the candidate set came out interleaved).
        let x = pos.0 - radius + rng.next_bound(2 * radius + 1);
        let y = pos.1 + rng.next_bound(1);
        let z = pos.2 - radius + rng.next_bound(2 * radius + 1);
        let remaining = height - (x - pos.0).abs() - (y - pos.1).abs() - (z - pos.2).abs();
        if remaining >= 0 {
            bc_place_column(ctx, reg, sea, (x, y, z), remaining, reach_ip.get(rng));
        }
    }
}

/// The fixed `CANNOT_PLACE_ON` list (`ebi.a`): the blocks a column may not start on.
const BC_CANNOT_PLACE_ON: [&str; 10] = [
    "minecraft:lava",
    "minecraft:bedrock",
    "minecraft:magma_block",
    "minecraft:soul_sand",
    "minecraft:nether_bricks",
    "minecraft:nether_brick_fence",
    "minecraft:nether_brick_stairs",
    "minecraft:nether_wart",
    "minecraft:chest",
    "minecraft:spawner",
];

/// `ebi.a(dcx,int,jd)`: air, or lava at or below sea level (the ocean case).
fn bc_air_or_lava_ocean(ctx: &mut FC, reg: &BReg, sea: i32, p: (i32, i32, i32)) -> bool {
    let st = ctx.get(p.0, p.1, p.2);
    reg.is_air(st) || (reg.name_of(st) == "minecraft:lava" && p.1 <= sea)
}

/// `ebi.a(dcx,int,jd$a)`: the above, plus a non-air cell below that may be built on.
fn bc_can_start(ctx: &mut FC, reg: &BReg, sea: i32, p: (i32, i32, i32)) -> bool {
    if !bc_air_or_lava_ocean(ctx, reg, sea, p) {
        return false;
    }
    // `ebi.a(dcx,int,jd)`: `!world.getBlockState(pos.below()).isAir() &&
    // !CANNOT_PLACE_ON.contains(...)`. `isAir()` is the block predicate, so
    // `cave_air` and `void_air` are not support either - comparing the state name
    // against `minecraft:air` accepted both of them as a column base.
    let below_st = ctx.get(p.0, p.1 - 1, p.2);
    !reg.is_air(below_st) && !BC_CANNOT_PLACE_ON.contains(&reg.name_of(below_st))
}

/// `BasaltColumnsFeature.placeColumn`: the square of `reach` around a candidate, a surface per
/// column, and basalt upward from it.
fn bc_place_column(
    ctx: &mut FC,
    reg: &BReg,
    sea: i32,
    pos: (i32, i32, i32),
    remaining: i32,
    reach: i32,
) {
    let basalt = reg.state_of("minecraft:basalt", None);
    for dx in -reach..=reach {
        for dz in -reach..=reach {
            let p = (pos.0 + dx, pos.1, pos.2 + dz);
            let dist = dx.abs() + dz.abs();
            // The native's condition here is the *air* test (`a(dcx,int,jd)`), not the
            // can-start test: an air/ocean cell walks **down** to its surface, anything else
            // walks **up** to the first air cell.
            let surface = if bc_air_or_lava_ocean(ctx, reg, sea, p) {
                bc_find_surface(ctx, reg, sea, p, dist)
            } else {
                bc_find_air(ctx, reg, p, dist)
            };
            let Some(mut m) = surface else { continue };
            let mut left = remaining - dist / 2;
            while left >= 0 {
                if bc_air_or_lava_ocean(ctx, reg, sea, m) {
                    if !ctx.out_of_h(m.1) {
                        ctx.set(m.0, m.1, m.2, basalt);
                    }
                    m.1 += 1;
                } else if reg.name_of(ctx.get(m.0, m.1, m.2)) == "minecraft:basalt" {
                    m.1 += 1;
                } else {
                    break;
                }
                left -= 1;
            }
        }
    }
}

/// `ebi.a(dcx,int,jd$a,int)`: walk **down** at most `dist` cells for the first cell that can
/// start a column - the *can-start* test, i.e. air/ocean-lava **and** a buildable block below,
/// not merely an air cell. That is why a column never grows out over open air: it walks to the
/// bottom of the world and gives up.
fn bc_find_surface(
    ctx: &mut FC,
    reg: &BReg,
    sea: i32,
    pos: (i32, i32, i32),
    dist: i32,
) -> Option<(i32, i32, i32)> {
    let mut m = pos;
    let mut left = dist;
    while m.1 > ctx.g.min_y + 1 && left > 0 {
        left -= 1;
        if bc_can_start(ctx, reg, sea, m) {
            return Some(m);
        }
        m.1 -= 1;
    }
    None
}

/// `ebi.a(dcx,jd$a,int)`: walk **up** at most `dist` cells for the first air cell that is not
/// under a cannot-place-on block.
fn bc_find_air(
    ctx: &mut FC,
    reg: &BReg,
    pos: (i32, i32, i32),
    dist: i32,
) -> Option<(i32, i32, i32)> {
    let mut m = pos;
    let mut left = dist;
    let top = ctx.g.world_min_y + ctx.g.world_height;
    while m.1 < top && left > 0 {
        left -= 1;
        let st = ctx.get(m.0, m.1, m.2);
        if BC_CANNOT_PLACE_ON.contains(&reg.name_of(st)) {
            return None;
        }
        if reg.is_air(st) {
            return Some(m);
        }
        m.1 += 1;
    }
    None
}

#[cfg(test)]
mod basalt_sea_tests {
    #[cfg(megu_vanilla_data)]
    use super::*;

    #[test]
    #[cfg(megu_vanilla_data)]
    fn basalt_columns_use_the_generators_sea_level() {
        let reg = breg().expect("block registry");
        let lava = reg.state_of("minecraft:lava", None);
        let stone = reg.state_of("minecraft:stone", None);
        let basalt = reg.state_of("minecraft:basalt", None);

        let placed = |sea, y| {
            let mut g = Gen::load(0, crate::name_to_state).expect("datapack");
            g.sea = sea;
            let mut ctx = FC::new(&mut g);
            ctx.test_default_air = true;
            for x in 0..=16 {
                for z in 0..=16 {
                    ctx.set(x, y - 1, z, stone);
                    ctx.set(x, y, z, lava);
                }
            }
            let mut rng = Xor::seed_worldgen(42);
            gen_basalt_columns(&mut ctx, &mut rng, (8, y, 8), &IP::Const(4), &IP::Const(1));
            ctx.wr.values().filter(|&&st| st == basalt).count()
        };

        assert_eq!(
            placed(31, 40),
            0,
            "lava above Nether sea must reject columns"
        );
        assert!(
            placed(31, 28) > 0,
            "lava below Nether sea must admit columns"
        );
        assert!(
            placed(45, 40) > 0,
            "the generator's sea level controls lava"
        );
    }
}

/// `SculkPatchConfiguration` (`eez`).
pub(crate) struct SculkPatchCfg {
    charge_count: i32,
    amount_per_charge: i32,
    spread_attempts: i32,
    growth_rounds: i32,
    spread_rounds: i32,
    extra_rare_growths: IP,
    catalyst_chance: f32,
}

/// `SculkSpreader.createWorldGenSpreader()` (`dmu.b()` = `new dmu(true, bU, 50, 1, 5, 10)`):
/// growthSpawnCost, noGrowthRadius, chargeDecayRate, additionalDecayRate. `isWorldGeneration` is
/// true, which means cursors are **never merged** (see [`SculkSpreader::update`]).
const SPC_GROWTH_SPAWN_COST: i32 = 50;
const SPC_NO_GROWTH_RADIUS: i32 = 1;
const SPC_DECAY_RATE: i32 = 5;
const SPC_ADDITIONAL_DECAY_RATE: i32 = 10;
const SPC_MAX_CURSORS: usize = 32;
const SPC_MAX_CHARGE: i32 = 1000;
/// `BlockPos.closerThan(pos, 15.0)` is `distSqr < 225`.
const SPC_ORIGIN_RADIUS_SQ: i64 = 225;

const SPC_VEIN: &str = "minecraft:sculk_vein";

thread_local! {
    /// Counts `SculkSpreader.updateCursors` calls, matching the reference agent's counter.
    ///
    /// Thread-local like the trace sink: a process-wide counter made a test's trace start at
    /// whatever round another test had reached (measured: `r=16` as the first line instead of `r=0`).
    /// A CLI run has one thread, so its numbering is unchanged.
    static SPC_ROUND: std::cell::Cell<i32> = const { std::cell::Cell::new(0) };
}

thread_local! {
    /// Where the sculk cursor trace goes: a test installs a **thread-local** sink and reads the lines
    /// back in-process, the CLI gets them on stderr with `SPC_CURSOR_TRACE=1`.
    ///
    /// Thread-local rather than a global with a lock, because the suite runs its tests on threads: a
    /// process-wide sink let a concurrently running sculk test's cursors land in this test's trace
    /// (measured: 5310 lines instead of 5150, and no line matched). A thread-local sink cannot be
    /// polluted by another test, because population runs on the thread that asked for it.
    static SPC_TRACE_SINK: std::cell::RefCell<Option<Vec<String>>> =
        const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
#[cfg_attr(not(megu_vanilla_data), allow(dead_code))]
pub(crate) fn spc_trace_begin() {
    SPC_TRACE_SINK.with(|s| *s.borrow_mut() = Some(Vec::new()));
}

#[cfg(test)]
#[cfg_attr(not(megu_vanilla_data), allow(dead_code))]
pub(crate) fn spc_trace_take() -> Vec<String> {
    SPC_TRACE_SINK
        .with(|s| s.borrow_mut().take())
        .unwrap_or_default()
}

/// One `SCURSOR` line: identical to the reference agent's `SculkLog.cursor` output.
fn spc_trace(line: String) {
    let collected = SPC_TRACE_SINK.with(|s| match s.borrow_mut().as_mut() {
        Some(lines) => {
            lines.push(line.clone());
            true
        }
        None => false,
    });
    if !collected && std::env::var("SPC_CURSOR_TRACE").is_ok() {
        eprintln!("{line}");
    }
}

/// Which `SculkBehaviour` a state carries: `SculkBlock`, `SculkVeinBlock`, or the default
/// (everything else, including air).
#[derive(Clone, Copy, PartialEq, Eq)]
enum SpcKind {
    Sculk,
    Vein,
    Other,
}

fn spc_kind(reg: &BReg, st: u32) -> SpcKind {
    match reg.name_of(st) {
        "minecraft:sculk" => SpcKind::Sculk,
        SPC_VEIN => SpcKind::Vein,
        _ => SpcKind::Other,
    }
}

fn spc_has_face(reg: &BReg, st: u32, d: u8) -> bool {
    reg.get_prop(st, multiface_property(d)).as_deref() == Some("true")
}

/// `MultifaceBlock.pack(Collection<Direction>)` as the 6-bit face mask (`dmu$a` stores it).
fn spc_face_mask(reg: &BReg, st: u32) -> u8 {
    let mut mask = 0u8;
    for d in 0..6u8 {
        if spc_has_face(reg, st, d) {
            mask |= 1 << d;
        }
    }
    mask
}

/// `SculkSpreader.ChargeCursor` (`dmu$a`).
#[derive(Clone)]
struct SpcCursor {
    pos: (i32, i32, i32),
    charge: i32,
    decay_delay: i32,
    update_delay: i32,
    facings: Option<u8>,
}

impl SpcCursor {
    fn new(pos: (i32, i32, i32), charge: i32) -> Self {
        SpcCursor {
            pos,
            charge,
            decay_delay: 1,
            update_delay: 0,
            facings: None,
        }
    }
}

/// `SculkSpreader` (`dmu`), world-generation flavour.
struct SculkSpreader {
    cursors: Vec<SpcCursor>,
}

impl SculkSpreader {
    fn new() -> Self {
        SculkSpreader {
            cursors: Vec::new(),
        }
    }

    /// `dmu.a(jd,int)`: split the amount into <=1000 chunks, keeping at most 32 cursors
    /// (the split still runs past the cap).
    fn add_cursor(&mut self, pos: (i32, i32, i32), mut amount: i32) {
        while amount > 0 {
            let part = amount.min(SPC_MAX_CHARGE);
            if self.cursors.len() < SPC_MAX_CURSORS {
                self.cursors.push(SpcCursor::new(pos, part));
            }
            amount -= part;
        }
    }

    fn clear(&mut self) {
        self.cursors.clear();
    }

    /// `dmu.a(dcx,jd,ayw,boolean)`: update every cursor and keep the survivors. A world-gen
    /// spreader does **not** merge cursors that land on the same position (the merge branch is
    /// behind `!isWorldGeneration()`), so duplicates stay.
    fn update(&mut self, ctx: &mut FC, rng: &mut Xor, origin: (i32, i32, i32), spread_only: bool) {
        // The reference agent's `SculkLog.round()` counts every `updateCursors` call the same
        // way, and `i` is the cursor's index in this round's list: a cursor that reaches charge
        // 0 is dropped, so without those two numbers the two traces cannot be aligned.
        let round = SPC_ROUND.with(|r| {
            let v = r.get();
            r.set(v + 1);
            v
        });
        let traced = std::env::var("SPC_CURSOR_TRACE").is_ok()
            || SPC_TRACE_SINK.with(|s| s.borrow().is_some());
        let mut kept = Vec::with_capacity(self.cursors.len());
        for (idx, mut cursor) in std::mem::take(&mut self.cursors).into_iter().enumerate() {
            if traced {
                spc_trace(format!(
                    "SCURSOR r={round} i={idx} {} {} {} {} {} {} {}",
                    cursor.pos.0,
                    cursor.pos.1,
                    cursor.pos.2,
                    cursor.charge,
                    cursor.decay_delay,
                    cursor.update_delay,
                    cursor.facings.map_or(-1, |m| m as i32)
                ));
            }
            spc_cursor_update(ctx, rng, &mut cursor, origin, spread_only);
            if cursor.charge > 0 {
                kept.push(cursor);
            }
        }
        self.cursors = kept;
    }
}

/// `ChargeCursor.update` (`dmu$a.a(dcx,jd,ayw,dmu,boolean)`).
fn spc_cursor_update(
    ctx: &mut FC,
    rng: &mut Xor,
    cursor: &mut SpcCursor,
    origin: (i32, i32, i32),
    spread_only: bool,
) {
    if cursor.charge <= 0 {
        return;
    }
    if cursor.update_delay > 0 {
        cursor.update_delay -= 1;
        return;
    }
    let Some(reg) = breg() else { return };
    let (x, y, z) = cursor.pos;
    let mut state = ctx.get(x, y, z);
    let mut kind = spc_kind(reg, state);
    if spread_only && spc_attempt_spread_vein(ctx, reg, cursor) {
        // SculkBehaviour.canChangeBlockStateOnSpread defaults to true (including
        // vein); only SculkBlock overrides it to false.
        if kind != SpcKind::Sculk {
            state = ctx.get(x, y, z);
            kind = spc_kind(reg, state);
        }
        // `levelEvent(3006, ...)` is sound/particles only; not modelled
    }
    cursor.charge = spc_spread_one(ctx, reg, rng, cursor, origin, kind, spread_only);
    if cursor.charge <= 0 {
        spc_decay(ctx, reg, cursor, kind, state);
        return;
    }
    if let Some(next) = spc_find_next_sculk(ctx, reg, rng, cursor.pos) {
        spc_decay(ctx, reg, cursor, kind, state);
        cursor.pos = next;
        // `pos.closerThan(new Vec3i(origin.x, pos.y, origin.z), 15.0)`: the y term is the
        // cursor's own, so it cancels and only the horizontal distance is bounded.
        let dx = i64::from(cursor.pos.0 - origin.0);
        let dz = i64::from(cursor.pos.2 - origin.2);
        if dx * dx + dz * dz >= SPC_ORIGIN_RADIUS_SQ {
            cursor.charge = 0;
            return;
        }
        state = ctx.get(cursor.pos.0, cursor.pos.1, cursor.pos.2);
    }
    // note: this runs whether or not `next` was found, against whatever `state` holds
    if spc_kind(reg, state) != SpcKind::Other {
        // availableFaces returns a set even for non-multiface sculk: empty is
        // known (Some(0)), not the uninitialized cursor's unknown (None).
        cursor.facings = Some(spc_face_mask(reg, state));
    }
    cursor.decay_delay = spc_decay_delay(kind, cursor.decay_delay);
    cursor.update_delay = 1;
}

/// `SculkBlock.spreadOneCharge` (`dmq.a`), the vein's (`dmv.a`) and the default's
/// (`dmp$1.a`) — the cursor's new charge.
fn spc_spread_one(
    ctx: &mut FC,
    reg: &BReg,
    rng: &mut Xor,
    cursor: &SpcCursor,
    origin: (i32, i32, i32),
    kind: SpcKind,
    spread_only: bool,
) -> i32 {
    match kind {
        SpcKind::Sculk => {
            // The native label `one-charge-sculk` hooks `SculkBlock.attemptUseCharge`, so it belongs
            // inside this arm - not at the function entry, where it would also count the vein and
            // default paths (that mistiming is why the first count came out 5485 against 6316).
            spc_trace("SCMARK one-charge-sculk".to_string());
            if cursor.charge == 0 || rng.next_bound(SPC_DECAY_RATE) != 0 {
                return cursor.charge;
            }
            // `pos.closerThan(origin, noGrowthRadius)` with radius 1 is "the same cell"
            let within = cursor.pos == origin;
            if !within && spc_can_place_growth(ctx, reg, cursor.pos) {
                if rng.next_bound(SPC_GROWTH_SPAWN_COST) < cursor.charge {
                    let above = (cursor.pos.0, cursor.pos.1 + 1, cursor.pos.2);
                    let st = spc_growth_state(ctx, reg, rng, above, true);
                    if !ctx.out_of_h(above.1) {
                        ctx.set(above.0, above.1, above.2, st);
                    }
                }
                return (cursor.charge - SPC_GROWTH_SPAWN_COST).max(0);
            }
            if rng.next_bound(SPC_ADDITIONAL_DECAY_RATE) != 0 {
                return cursor.charge;
            }
            cursor.charge
                - if within {
                    1
                } else {
                    spc_decrease_charge(cursor.pos, origin, cursor.charge)
                }
        }
        SpcKind::Vein => {
            // `one-charge-vein` hooks `SculkVeinBlock.attemptUseCharge`; `convert` hooks
            // `SculkVeinBlock.attemptPlaceSculk`, which is what `spc_vein_convert` implements.
            spc_trace("SCMARK one-charge-vein".to_string());
            if spread_only && spc_vein_convert(ctx, reg, rng, cursor.pos) {
                return cursor.charge - 1;
            }
            if rng.next_bound(SPC_DECAY_RATE) == 0 {
                (cursor.charge as f32 * 0.5).floor() as i32
            } else {
                cursor.charge
            }
        }
        // `dmp$1`: `decayDelay > 0 ? charge : 0`
        SpcKind::Other => {
            if cursor.decay_delay > 0 {
                cursor.charge
            } else {
                0
            }
        }
    }
}

/// `SculkBlock.getDecayPenalty`: square the float excess of the double distance sqrt,
/// divide by the integer square of `24 - noGrowthRadius`, then truncate the float penalty.
fn spc_decrease_charge(pos: (i32, i32, i32), origin: (i32, i32, i32), charge: i32) -> i32 {
    let (dx, dy, dz) = (
        f64::from(pos.0 - origin.0),
        f64::from(pos.1 - origin.1),
        f64::from(pos.2 - origin.2),
    );
    // Mth.k(float) is square, not sqrt. Java casts Math.sqrt's result to float first.
    let excess = (dx * dx + dy * dy + dz * dz).sqrt() as f32 - SPC_NO_GROWTH_RADIUS as f32;
    let over = excess * excess;
    // Mth.h(int) is square(int); the ceil overloads take float/double.
    let denom = (24 - SPC_NO_GROWTH_RADIUS).pow(2) as f32;
    let ratio = 1.0f32.min(over / denom);
    (1).max((charge as f32 * ratio * 0.5) as i32)
}

/// `SculkBlock.canPlaceGrowth` (`dmq.a(dcx,jd)`): the cell **above** the cursor is air or a
/// water source - the growth target - and at most two sensor/shrieker cells exist
/// in the `-4..4 x 0..2 x -4..4` box.
fn spc_can_place_growth(ctx: &mut FC, reg: &BReg, pos: (i32, i32, i32)) -> bool {
    // `$$1.d()` is `above()`, and the growth branch writes at `$$7.d()` too: the cell the
    // check inspects is the growth target, not the cell below the cursor.
    let target = ctx.get(pos.0, pos.1 + 1, pos.2);
    let water_source = reg.name_of(target) == "minecraft:water";
    if !reg.is_air(target) && !water_source {
        return false;
    }
    let mut seen = 0;
    for dx in -4i32..=4 {
        for dy in 0i32..=2 {
            for dz in -4i32..=4 {
                let name = reg.name_of(ctx.get(pos.0 + dx, pos.1 + dy, pos.2 + dz));
                // Blocks.qQ = SCULK_SENSOR; qS, not qQ, is SCULK.
                if name == "minecraft:sculk_sensor" || name == "minecraft:sculk_shrieker" {
                    seen += 1;
                    if seen > 2 {
                        return false;
                    }
                }
            }
        }
    }
    true
}

/// `SculkBlock.getSculkState` (`dmq.a(dcx,jd,ayw,boolean)`): a shrieker on one `nextInt(11)`
/// in eleven, `sculk_sensor` otherwise; `WATERLOGGED` when the cell holds **any** fluid.
fn spc_growth_state(
    ctx: &mut FC,
    reg: &BReg,
    rng: &mut Xor,
    pos: (i32, i32, i32),
    world_gen: bool,
) -> u32 {
    let mut st = if rng.next_bound(11) == 0 {
        reg.with_prop(
            reg.state_of("minecraft:sculk_shrieker", None),
            "can_summon",
            if world_gen { "true" } else { "false" },
        )
    } else {
        // Blocks.qQ in SculkBlock.getRandomGrowthState is SCULK_SENSOR.
        reg.state_of("minecraft:sculk_sensor", None)
    };
    if reg.get_prop(st, "waterlogged").is_some()
        && reg.fluid_name(ctx.get(pos.0, pos.1, pos.2)) != "minecraft:empty"
    {
        st = reg.with_prop(st, "waterlogged", "true");
    }
    st
}

/// `SculkBehaviour.attemptSpreadVein` (`dmp.a(dcx,jd,dtc,Collection,boolean)`): the vein,
/// the default and the `v_` implementations.
fn spc_attempt_spread_vein(ctx: &mut FC, reg: &BReg, cursor: &SpcCursor) -> bool {
    let (x, y, z) = cursor.pos;
    let state = ctx.get(x, y, z);
    match spc_kind(reg, state) {
        // `SculkBlock` (`dmq`) and `SculkVeinBlock` (`dmv`) do **not** override the five-argument
        // `attemptSpreadVein`, so the interface default runs: it ignores `facings` and spreads
        // with `SculkVeinBlock.c()` - **all three** spread types. Measured against the native
        // agent: the `spreadAll` of a cursor standing on a vein reports `types=3`, and that is
        // where its neighbour-cell placements come from. Treating this like the default
        // behaviour (SAME_POSITION only) was the port's first divergence.
        SpcKind::Sculk | SpcKind::Vein => spc_spread_all(
            ctx,
            reg,
            cursor.pos,
            state,
            &[0, 1, 2],
            "spread-behaviour-3type",
        ),
        // Every other block uses `dmp$1`: null facings -> `m()` (SAME_POSITION only), empty
        // facings -> the interface default (all three), otherwise the static four-argument
        // overload with the faces the cursor stored.
        SpcKind::Other => match cursor.facings {
            None => spc_spread_all(ctx, reg, cursor.pos, state, &[0], "spread-cursor-1type"),
            Some(0) => spc_spread_all(
                ctx,
                reg,
                cursor.pos,
                state,
                &[0, 1, 2],
                "spread-cursor-3type",
            ),
            Some(mask) => {
                if !reg.is_air(state) && reg.fluid_name(state) != "minecraft:water" {
                    return false;
                }
                spc_vein_attempt_spread(ctx, reg, cursor.pos, state, mask)
            }
        },
    }
}

/// Write one sculk-family cell, naming the call site when asked to.
///
/// `SPC_TRACE_CELL=x,y,z` prints every write to that cell with its call site, and
/// `SPC_TRACE_ALL=1` prints all of them. The order and the call site are what localise a
/// divergence: the first write that leaves the reference's cell set is where two otherwise
/// identical-looking implementations part, and *which* call site produced it says which
/// predicate to read next.
fn spc_set(ctx: &mut FC, p: (i32, i32, i32), st: u32, tag: &str) {
    let all = std::env::var("SPC_TRACE_ALL").is_ok();
    if all || std::env::var("SPC_TRACE_CELL").as_deref() == Ok(&format!("{},{},{}", p.0, p.1, p.2))
    {
        eprintln!("SPCSET {} {} {} state={} at={tag}", p.0, p.1, p.2, st);
    }
    if !ctx.out_of_h(p.1) {
        ctx.set(p.0, p.1, p.2, st);
    }
}

/// `MultifaceSpreader.spreadAll` (`dkz.a(dtc,dcx,jd,boolean)`): `from` in `Direction.values()`
/// order (filtered by `isOtherBlockValidAsSource`), then `to` in `Direction.values()` order.
///
/// **No random source is involved**: `spreadAll` maps `Direction.stream()` twice and counts the
/// placements, so the vein's spread order is fully deterministic. (The shuffled list belongs to
/// the *other* overload, `spread(state, level, pos, from, random, spreadOnly)`, which the
/// multiface feature uses and the sculk path never calls.)
fn spc_spread_all(
    ctx: &mut FC,
    reg: &BReg,
    pos: (i32, i32, i32),
    state: u32,
    types: &[u8],
    tag: &str,
) -> bool {
    // `SSPREADALL types=<n>` hooks MultifaceSpreader.spreadAll at entry.
    // n counts spread types (same-position, same-plane, wrap-around), not directions.
    spc_trace(format!("SSPREADALL types={}", types.len()));
    let mut any = false;
    let is_vein = spc_kind(reg, state) == SpcKind::Vein;
    for from in 0..6u8 {
        // `config.isOtherBlockValidAsSource(state, from)` = `config.a(state) || hasFace(from)`
        if is_vein && !spc_has_face(reg, state, from) {
            continue;
        }
        for to in 0..6u8 {
            if spc_spread(ctx, reg, pos, state, from, to, types, tag) {
                any = true;
            }
        }
    }
    any
}

/// `MultifaceSpreader.spread(state, level, pos, from, to, spreadOnly)` (`dkz.a`): the axis gate,
/// the source gate, then the spread types in order, taking the first candidate the predicate
/// accepts and placing there (a failed placement does not fall through to the next type).
#[expect(clippy::too_many_arguments)]
fn spc_spread(
    ctx: &mut FC,
    reg: &BReg,
    pos: (i32, i32, i32),
    state: u32,
    from: u8,
    to: u8,
    types: &[u8],
    tag: &str,
) -> bool {
    if to / 2 == from / 2 {
        return false;
    }
    // MultifaceSpreader: spread FROM a present face TOWARD an absent face.
    let is_vein = spc_kind(reg, state) == SpcKind::Vein;
    if is_vein && !(spc_has_face(reg, state, from) && !spc_has_face(reg, state, to)) {
        return false;
    }
    for t in types {
        let (target, face) = match t {
            0 => (pos, to),
            1 => (multiface_relative(pos, to), from),
            _ => (
                multiface_relative(multiface_relative(pos, to), from),
                to ^ 1,
            ),
        };
        let watch = std::env::var("SPC_TRACE_CELL").as_deref()
            == Ok(&format!("{},{},{}", target.0, target.1, target.2));
        let ok = spc_can_spread(ctx, reg, pos, target, face);
        if watch {
            eprintln!(
                "SPCPRED target={target:?} pos={pos:?} from={from} to={to} type={t} face={face} state={} passes={ok} tag={tag}",
                reg.name_of(ctx.get(target.0, target.1, target.2))
            );
        }
        if !ok {
            continue;
        }
        return match multiface_state(ctx, reg, target, SPC_VEIN, face) {
            Some(st) => {
                spc_set(ctx, target, st, tag);
                true
            }
            None => false,
        };
    }
    false
}

/// The spread predicate (`dkz$a.a(dcc,jd,dkz$c)`, `dmv$a.a(dcc,jd,jd,ji,dtc)` and
/// `MultifaceBlock.a(dcc,dtc,jd,ji)`).
fn spc_can_spread(
    ctx: &mut FC,
    reg: &BReg,
    pos: (i32, i32, i32),
    target: (i32, i32, i32),
    dir: u8,
) -> bool {
    let state = ctx.get(target.0, target.1, target.2);
    if !spc_vein_can_spread(ctx, reg, pos, target, dir, state) {
        return false;
    }
    spc_multiface_can_place(ctx, reg, state, target, dir)
}

/// `SculkVeinSpreaderConfig.canSpread` (`dmv$a.a(dcc,jd,jd,ji,dtc)`).
fn spc_vein_can_spread(
    ctx: &mut FC,
    reg: &BReg,
    pos: (i32, i32, i32),
    target: (i32, i32, i32),
    dir: u8,
    state: u32,
) -> bool {
    let (tdx, tdy, tdz) = dir_off(dir);
    let nb = ctx.get(target.0 + tdx, target.1 + tdy, target.2 + tdz);
    let name = reg.name_of(nb);
    // `dga.bQ` is `moving_piston`, not `sculk_sensor` (measured from the mappings): the
    // neighbour in the face direction is rejected when it is sculk, sculk_catalyst or a
    // moving piston.
    if matches!(
        name,
        "minecraft:sculk" | "minecraft:sculk_catalyst" | "minecraft:moving_piston"
    ) {
        return false;
    }
    let manhattan = (target.0 - pos.0).abs() + (target.1 - pos.1).abs() + (target.2 - pos.2).abs();
    if manhattan == 2 {
        // SculkVeinSpreaderConfig checks source.relative(face.getOpposite()).
        let (ox, oy, oz) = dir_off(dir ^ 1);
        let side = (pos.0 + ox, pos.1 + oy, pos.2 + oz);
        let side_st = ctx.get(side.0, side.1, side.2);
        if face_sturdy(reg, side_st, dir, Support::Full) {
            return false;
        }
    }
    let fluid = reg.fluid_name(state);
    if fluid != "minecraft:empty" && fluid != "minecraft:water" {
        return false;
    }
    if reg.in_tag(state, "minecraft:fire") {
        return false;
    }
    // `state.canBeReplaced() || super.canSpread(...)` (`dkz$a`)
    // `reg.is_replaceable` is the asset's replaceable bit, the same property
    // `Feature.isReplaceable` uses (`huge_fungus`).
    reg.is_replaceable(state)
        || reg.is_air(state)
        || reg.name_of(state) == SPC_VEIN
        || (reg.name_of(state) == "minecraft:water" && reg.fluid_name(state) == "minecraft:water")
}

/// `MultifaceBlock.a(dcc,dtc,jd,ji)`: the face must be enabled for this block, the target must
/// not already be this block with that face, and the neighbour behind the face must support it.
fn spc_multiface_can_place(
    ctx: &mut FC,
    reg: &BReg,
    state: u32,
    target: (i32, i32, i32),
    dir: u8,
) -> bool {
    // sculk vein enables all six faces (`MultifaceBlock`'s direction flags)
    if reg.name_of(state) == SPC_VEIN && spc_has_face(reg, state, dir) {
        return false;
    }
    let (dx, dy, dz) = dir_off(dir);
    let nb = ctx.get(target.0 + dx, target.1 + dy, target.2 + dz);
    reg.supports_multiface(nb, dir)
}

/// `SculkVeinBlock.attemptSpreadVein` (`dmv.a(dcx,jd,dtc,Collection<Direction>)`).
fn spc_vein_attempt_spread(
    ctx: &mut FC,
    reg: &BReg,
    pos: (i32, i32, i32),
    state: u32,
    mask: u8,
) -> bool {
    // SculkVeinBlock.regrow: this path already existed, but was missing its native marker.
    spc_trace("SCMARK attempt-vein".to_string());
    let mut vein = reg.state_of(SPC_VEIN, None);
    let mut any = false;
    for d in 0..6u8 {
        if mask & (1 << d) == 0 {
            continue;
        }
        let (dx, dy, dz) = dir_off(d);
        let nb = (pos.0 + dx, pos.1 + dy, pos.2 + dz);
        if reg.supports_multiface(ctx.get(nb.0, nb.1, nb.2), d) {
            vein = reg.with_prop(vein, multiface_property(d), "true");
            any = true;
        }
    }
    if !any {
        return false;
    }
    // `if (!state.getFluidState().isEmpty())` — any fluid, water or not
    if reg.fluid_name(state) != "minecraft:empty" {
        vein = reg.with_prop(vein, "waterlogged", "true");
    }
    spc_set(ctx, pos, vein, "attempt");
    true
}

/// `SculkVeinBlock.a(dmu,dcx,jd,ayw)` (private): turn a vein into sculk where a
/// `sculk_replaceable_world_gen` neighbour sits behind one of its faces, then clean up the
/// veins around the new sculk cell and let veins spread from it.
fn spc_vein_convert(ctx: &mut FC, reg: &BReg, rng: &mut Xor, pos: (i32, i32, i32)) -> bool {
    // The native label `convert` hooks `SculkVeinBlock.attemptPlaceSculk`, which is this function.
    spc_trace("SCMARK convert".to_string());
    let state = ctx.get(pos.0, pos.1, pos.2);
    // `Direction.allShuffled(random)` — this one *does* draw (five nextInt for six directions)
    let mut dirs: Vec<u8> = (0..6u8).collect();
    feature_util_shuffle(&mut dirs, rng);
    for d in dirs {
        if !spc_has_face(reg, state, d) {
            continue;
        }
        let (dx, dy, dz) = dir_off(d);
        let nb = (pos.0 + dx, pos.1 + dy, pos.2 + dz);
        if !reg.in_tag(
            ctx.get(nb.0, nb.1, nb.2),
            "minecraft:sculk_replaceable_world_gen",
        ) {
            continue;
        }
        let sculk = reg.state_of("minecraft:sculk", None);
        spc_set(ctx, nb, sculk, "convert");
        // `this.e.a(sculkState, level, newPos, worldGen)`: veins spread from the new sculk cell
        // (spreader `c()`, all three spread types). This call is what sustains the patch: with
        // it removed the whole capture collapses from 429 sculk-family cells to 16 (measured),
        // because the vein -> sculk conversion chain has no new veins to convert.
        spc_spread_all(ctx, reg, nb, sculk, &[0, 1, 2], "spread-post-convert");
        let opposite = d ^ 1;
        for around in 0..6u8 {
            if around == opposite {
                continue;
            }
            let (ax, ay, az) = dir_off(around);
            let p = (nb.0 + ax, nb.1 + ay, nb.2 + az);
            let st = ctx.get(p.0, p.1, p.2);
            if reg.name_of(st) == SPC_VEIN {
                spc_vein_decay(ctx, reg, p, st);
            }
        }
        return true;
    }
    false
}

/// `SculkVeinBlock.a(dcx,dtc,jd,ayw)`: drop the faces whose neighbour became sculk, and turn a
/// vein with no faces left into water or air.
fn spc_vein_decay(ctx: &mut FC, reg: &BReg, pos: (i32, i32, i32), st: u32) {
    let mut st = st;
    for d in 0..6u8 {
        if !spc_has_face(reg, st, d) {
            continue;
        }
        let (dx, dy, dz) = dir_off(d);
        if reg.name_of(ctx.get(pos.0 + dx, pos.1 + dy, pos.2 + dz)) == "minecraft:sculk" {
            st = reg.with_prop(st, multiface_property(d), "false");
        }
    }
    if (0..6u8).all(|d| !spc_has_face(reg, st, d)) {
        st = if reg.fluid_name(ctx.get(pos.0, pos.1, pos.2)) == "minecraft:empty" {
            reg.state_of("minecraft:air", None)
        } else {
            reg.state_of("minecraft:water", None)
        };
    }
    spc_set(ctx, pos, st, "decay");
}

/// `SculkBehaviour.decay` (`dmp.a(dcx,dtc,jd,ayw)`): only the vein behaviour does anything, and
/// its body is [`spc_vein_decay`]. The native caller passes its captured state:
/// conversion may have spread extra faces onto this cell in the meantime.
fn spc_decay(ctx: &mut FC, reg: &BReg, cursor: &SpcCursor, kind: SpcKind, st: u32) {
    if kind != SpcKind::Vein {
        return;
    }
    if reg.name_of(st) == SPC_VEIN {
        spc_vein_decay(ctx, reg, cursor.pos, st);
    }
}

/// `SculkBehaviour.decayDelay` (`j_`): `SculkBlock` and `SculkVeinBlock` both inherit the
/// interface default (`1`); only the *default* behaviour - the anonymous class in `dmp`, used
/// for every other block - returns `max(x - 1, 0)`. Reading the override the other way round
/// makes every cursor standing on a vein decay, which is where the earlier "the port decays
/// veins vanilla keeps" observation came from.
fn spc_decay_delay(kind: SpcKind, delay: i32) -> i32 {
    match kind {
        SpcKind::Other => (delay - 1).max(0),
        _ => 1,
    }
}

/// `BlockState.isFaceSturdy(level, pos, dir, supportType)`, from the pinned 1.21.1 table.
/// `Support::Full` is vanilla's three-argument overload; `Support::Center` is the
/// `canSupportCenter` question a torch or a plant asks.
///
/// The body used to be `is_solid(st) && side_solid(st, dir)`, which added a `blocksMotion`
/// gate vanilla does not have and denied the support on more states than native does. The
/// per-face counts live with the table that owns them
/// (`evidence/foundation/block-state-facts-20261116/README.md`) rather than here: an earlier
/// inline breakdown attributed every face's states to DOWN and did not add up. Shared by the
/// sculk cursor, the block pile's `tryPlaceBlock` and the bonus chest's torches, so it carries
/// no single feature's name.
fn face_sturdy(reg: &BReg, st: u32, dir: u8, support: Support) -> bool {
    reg.is_face_sturdy(st, dir, support)
}

/// The 18 face/edge neighbours, in `BlockPos.betweenClosed(-1,-1,-1, 1,1,1)` order with the
/// centre removed.
///
/// The *order* is the draw: `findNextSculk` shuffles this list, so a different base order is a
/// different permutation for the same random stream. Measured through the pinned jar:
/// `betweenClosed(-1,-1,-1, 1,1,1)` yields `(-1,-1,-1) (0,-1,-1) (1,-1,-1) (-1,0,-1) ...` —
/// **x varies fastest**, then y, then z. The port iterated x outermost, which is why its first
/// divergent cursor picked a different neighbour while every earlier write matched exactly.
fn spc_offsets() -> Vec<(i32, i32, i32)> {
    let mut out = Vec::with_capacity(18);
    for z in -1i32..=1 {
        for y in -1i32..=1 {
            for x in -1i32..=1 {
                if (x == 0 || y == 0 || z == 0) && !(x == 0 && y == 0 && z == 0) {
                    out.push((x, y, z));
                }
            }
        }
    }
    out
}

/// `ChargeCursor.findNextSculk` (`dmu$a.a(dcx,jd,ayw)`): the 18 face/edge neighbours in shuffled
/// order, keeping the last reachable `SculkBehaviour` cell, and stopping early when it has
/// substrate access.
fn spc_find_next_sculk(
    ctx: &mut FC,
    reg: &BReg,
    rng: &mut Xor,
    pos: (i32, i32, i32),
) -> Option<(i32, i32, i32)> {
    let mut offsets = spc_offsets();
    feature_util_shuffle(&mut offsets, rng);
    let mut target = pos;
    for off in offsets {
        let p = (pos.0 + off.0, pos.1 + off.1, pos.2 + off.2);
        let st = ctx.get(p.0, p.1, p.2);
        if spc_kind(reg, st) == SpcKind::Other {
            continue;
        }
        if !spc_cursor_can_spread(ctx, reg, pos, p) {
            continue;
        }
        target = p;
        if spc_vein_has_substrate_access(ctx, reg, p, st) {
            break;
        }
    }
    (target != pos).then_some(target)
}

/// `ChargeCursor.canSpread` (`dmu$a.a(dcx,jd,jd)`): adjacent is free, otherwise the direction
/// on each shared axis must have an open face.
fn spc_cursor_can_spread(
    ctx: &mut FC,
    reg: &BReg,
    from: (i32, i32, i32),
    to: (i32, i32, i32),
) -> bool {
    let (dx, dy, dz) = (to.0 - from.0, to.1 - from.1, to.2 - from.2);
    if dx.abs() + dy.abs() + dz.abs() == 1 {
        return true;
    }
    // `Direction.fromAxisAndDirection(axis, sign)`: DOWN/UP, NORTH/SOUTH, WEST/EAST
    let axis_dir =
        |delta: i32, negative: u8, positive: u8| if delta < 0 { negative } else { positive };
    let mut open = |dir: u8| {
        let (ox, oy, oz) = dir_off(dir);
        let p = (from.0 + ox, from.1 + oy, from.2 + oz);
        let st = ctx.get(p.0, p.1, p.2);
        !face_sturdy(reg, st, dir ^ 1, Support::Full)
    };
    if dx == 0 {
        open(axis_dir(dy, 0, 1)) || open(axis_dir(dz, 2, 3))
    } else if dy == 0 {
        open(axis_dir(dx, 4, 5)) || open(axis_dir(dz, 2, 3))
    } else {
        open(axis_dir(dx, 4, 5)) || open(axis_dir(dy, 0, 1))
    }
}

/// `SculkVeinBlock.a(dcx,dtc,jd)` (`hasSubstrateAccess`): a face of the vein points at a block
/// in `sculk_replaceable`.
fn spc_vein_has_substrate_access(ctx: &mut FC, reg: &BReg, pos: (i32, i32, i32), st: u32) -> bool {
    for d in 0..6u8 {
        if !spc_has_face(reg, st, d) {
            continue;
        }
        let (dx, dy, dz) = dir_off(d);
        if reg.in_tag(
            ctx.get(pos.0 + dx, pos.1 + dy, pos.2 + dz),
            "minecraft:sculk_replaceable",
        ) {
            return true;
        }
    }
    false
}

/// `SculkPatchFeature.canSpreadFrom` (`edi.a(dcx,jd)`).
fn spc_can_spread_from(ctx: &mut FC, reg: &BReg, pos: (i32, i32, i32)) -> bool {
    let st = ctx.get(pos.0, pos.1, pos.2);
    if spc_kind(reg, st) != SpcKind::Other {
        return true;
    }
    let water_source =
        reg.name_of(st) == "minecraft:water" && reg.fluid_name(st) == "minecraft:water";
    if !reg.is_air(st) && !water_source {
        return false;
    }
    for d in 0..6u8 {
        let (dx, dy, dz) = dir_off(d);
        let nb = ctx.get(pos.0 + dx, pos.1 + dy, pos.2 + dz);
        if reg.is_collision_shape_full_block(nb) {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod sculk_patch_entry_tests {
    #[cfg(megu_vanilla_data)]
    use super::*;

    #[test]
    #[cfg(megu_vanilla_data)]
    fn sculk_patch_entry_requires_full_collision_shape() {
        let reg = breg().expect("block registry");
        let path = reg.state_of("minecraft:dirt_path", None);
        assert!(reg.is_face_sturdy(path, 0, Support::Full));
        assert!(!reg.is_collision_shape_full_block(path));

        let mut g = Gen::load(0, crate::name_to_state).expect("datapack");
        let mut ctx = FC::new(&mut g);
        ctx.test_default_air = true;
        let origin = (0, 64, 0);
        ctx.set(0, 65, 0, path);
        assert!(!spc_can_spread_from(&mut ctx, reg, origin));

        ctx.set(0, 65, 0, reg.state_of("minecraft:stone", None));
        assert!(spc_can_spread_from(&mut ctx, reg, origin));

        ctx.set(0, 65, 0, reg.state_of("minecraft:air", None));
        ctx.set(0, 64, 0, reg.state_of("minecraft:sculk_vein", None));
        assert!(spc_can_spread_from(&mut ctx, reg, origin));
    }

    #[test]
    #[cfg(megu_vanilla_data)]
    fn sculk_patch_entry_rejects_flowing_water() {
        let reg = breg().expect("block registry");
        let source = reg.with_prop(reg.state_of("minecraft:water", None), "level", "0");
        let flowing = reg.with_prop(source, "level", "4");
        assert_eq!(reg.fluid_name(source), "minecraft:water");
        assert_eq!(reg.fluid_name(flowing), "minecraft:flowing_water");

        let mut g = Gen::load(0, crate::name_to_state).expect("datapack");
        let mut ctx = FC::new(&mut g);
        ctx.test_default_air = true;
        let origin = (0, 64, 0);
        ctx.set(0, 63, 0, reg.state_of("minecraft:stone", None));
        ctx.set(0, 64, 0, flowing);
        assert!(!spc_can_spread_from(&mut ctx, reg, origin));
        ctx.set(0, 64, 0, source);
        assert!(spc_can_spread_from(&mut ctx, reg, origin));
    }
}

/// `SculkPatchFeature.place` (`edi.a(ecg)`).
fn gen_sculk_patch(ctx: &mut FC, rng: &mut Xor, pos: (i32, i32, i32), c: &SculkPatchCfg) {
    // Patch marker, in the reference agent's `SPATCH` format (`SculkLog.patch`, hooked into
    // `SculkPatchFeature.place`): the trace of a run is split by patch via these, because
    // captures order patches differently and number rounds globally. Logged at entry, before
    // `canSpreadFrom`, exactly like the agent's hook.
    spc_trace(format!("SPATCH {} {} {}", pos.0, pos.1, pos.2));
    let Some(reg) = breg() else { return };
    if !spc_can_spread_from(ctx, reg, pos) {
        return;
    }
    let mut spreader = SculkSpreader::new();
    let rounds = c.spread_rounds + c.growth_rounds;
    for i in 0..rounds {
        for _ in 0..c.charge_count {
            spreader.add_cursor(pos, c.amount_per_charge);
        }
        let spread_only = i < c.spread_rounds;
        for _ in 0..c.spread_attempts {
            spreader.update(ctx, rng, pos, spread_only);
        }
        spreader.clear();
    }
    // Vanilla `BlockStateBase.isCollisionShapeFullBlock(level, below)` gates catalyst
    // placement. Consume the chance draw first, even when the support shape is not full.
    if rng.next_f32() <= c.catalyst_chance
        && reg.is_collision_shape_full_block(ctx.get(pos.0, pos.1 - 1, pos.2))
    {
        let st = reg.state_of("minecraft:sculk_catalyst", None);
        if !ctx.out_of_h(pos.1) {
            ctx.set(pos.0, pos.1, pos.2, st);
        }
    }
    let rare = c.extra_rare_growths.get(rng);
    for _ in 0..rare {
        let p = (
            pos.0 + rng.next_bound(5) - 2,
            pos.1,
            pos.2 + rng.next_bound(5) - 2,
        );
        if reg.is_air(ctx.get(p.0, p.1, p.2)) {
            let below = ctx.get(p.0, p.1 - 1, p.2);
            if face_sturdy(reg, below, 1, Support::Full) {
                let st = reg.with_prop(
                    reg.state_of("minecraft:sculk_shrieker", None),
                    "can_summon",
                    "true",
                );
                if !ctx.out_of_h(p.1) {
                    ctx.set(p.0, p.1, p.2, st);
                }
            }
        }
    }
}

/// `PointedDripstoneFeature` (`eda`). See [`Cf::PointedDripstone`].
fn gen_pointed_dripstone(
    ctx: &mut FC,
    rng: &mut Xor,
    pos: (i32, i32, i32),
    c: &PointedDripstoneCfg,
) {
    let Some(reg) = breg() else { return };
    let (x, y, z) = pos;
    // `getTipDirection`: `isDripstoneBase(below)` and `isDripstoneBase(above)`; DOWN (hang)
    // and UP (stand) are both possible, and `nextBoolean` is drawn only in that case.
    let below = ctx.get(x, y - 1, z);
    let above = ctx.get(x, y + 1, z);
    let up_base = drip_is_dripstone_base(reg, below);
    let down_base = drip_is_dripstone_base(reg, above);
    let up = if up_base && down_base {
        !rng.next_bool() // `nextBoolean() ? DOWN : UP`
    } else if up_base {
        true
    } else if down_base {
        false
    } else {
        return;
    };
    let base = if up { (x, y - 1, z) } else { (x, y + 1, z) };
    pointed_dripstone_patch(ctx, rng, base, c);
    // the tip is 2 cells when the taller draw passes and the cell past it is empty or water
    let beyond = if up { (x, y + 1, z) } else { (x, y - 1, z) };
    let taller = rng.next_f32() < c.taller && drip_is_empty_or_water(ctx, beyond);
    let height = if taller { 2 } else { 1 };
    drip_grow_pointed_dripstone(ctx, pos, up, height, false);
}

/// `PointedDripstoneFeature.createPatchOfDripstoneBlocks` (`eda.a(dcx,ayw,jd,eer)`).
///
/// The three gates are `!(nextFloat() > chance)` in the bytecode, and the draw happens
/// before the test either way; the spread steps use `Direction.getRandom`, which is
/// `VALUES[nextInt(6)]` over DOWN, UP, NORTH, SOUTH, WEST, EAST (vertical included).
fn pointed_dripstone_patch(
    ctx: &mut FC,
    rng: &mut Xor,
    pos: (i32, i32, i32),
    c: &PointedDripstoneCfg,
) {
    drip_place_dripstone_block(ctx, pos);
    // `Direction.Plane.HORIZONTAL`: NORTH, EAST, SOUTH, WEST
    for (dx, dz) in [(0, -1), (1, 0), (0, 1), (-1, 0)] {
        let outside = rng.next_f32() > c.directional;
        if outside {
            continue;
        }
        let p1 = (pos.0 + dx, pos.1, pos.2 + dz);
        drip_place_dripstone_block(ctx, p1);
        let outside = rng.next_f32() > c.radius2;
        if outside {
            continue;
        }
        let d = direction_random(rng);
        let p2 = (p1.0 + d.0, p1.1 + d.1, p1.2 + d.2);
        drip_place_dripstone_block(ctx, p2);
        let outside = rng.next_f32() > c.radius3;
        if outside {
            continue;
        }
        let d = direction_random(rng);
        drip_place_dripstone_block(ctx, (p2.0 + d.0, p2.1 + d.1, p2.2 + d.2));
    }
}

/// `Direction.getRandom(RandomSource)` (`ji.b(ayw)`): `VALUES[nextInt(6)]`, where the
/// declaration order is DOWN, UP, NORTH, SOUTH, WEST, EAST. Vertical steps are reachable,
/// so a patch cell can sit one layer above or below its seed.
fn direction_random(rng: &mut Xor) -> (i32, i32, i32) {
    const VALUES: [(i32, i32, i32); 6] = [
        (0, -1, 0),
        (0, 1, 0),
        (0, 0, -1),
        (0, 0, 1),
        (-1, 0, 0),
        (1, 0, 0),
    ];
    VALUES[rng.next_bound(6) as usize]
}

/// Nether "wart plant" ground: netherrack, warped nylium, warped wart block.
/// Measured from the native `isInvalidPlacement` checks of the two vine features.
fn is_vine_ground(name: &str) -> bool {
    matches!(
        name,
        "minecraft:netherrack" | "minecraft:warped_nylium" | "minecraft:warped_wart_block"
    )
}

/// The shared native growth helper, parameterised by direction.
///
/// `TwistingVinesFeature` walks up from `for i = 1`; `WeepingVinesFeature` walks down
/// from `for i = 0`, so the loop bounds differ by one and the support test looks at
/// the opposite neighbour. Both write the plant body until the last cell, which gets
/// the age property.
#[allow(clippy::too_many_arguments)]
fn grow_vines(
    ctx: &mut FC,
    rng: &mut Xor,
    pos: (i32, i32, i32),
    height: i32,
    down: bool,
    body: u32,
    tip: &str,
) {
    let Some(reg) = breg() else { return };
    let (x, mut y, z) = pos;
    let (start, step) = if down { (0, -1) } else { (1, 1) };
    let mut i = start;
    while i <= height {
        if ctx.is_air(x, y, z) {
            let beyond = if down { y - 1 } else { y + 1 };
            if i == height || !ctx.is_air(x, beyond, z) {
                let age = rng.next_inbetween(17, 25).to_string();
                let st = reg.with_prop(reg.state_of(tip, None), "age", &age);
                ctx.set(x, y, z, st);
                break;
            }
            ctx.set(x, y, z, body);
        }
        y += step;
        i += 1;
    }
}

/// `TwistingVinesFeature` (`edr`). See [`Cf::TwistingVines`].
fn gen_twisting_vines(
    ctx: &mut FC,
    rng: &mut Xor,
    pos: (i32, i32, i32),
    width: i32,
    height: i32,
    max_height: i32,
) {
    let Some(reg) = breg() else { return };
    if !ctx.is_air(pos.0, pos.1, pos.2)
        || !is_vine_ground(reg.name_of(ctx.get(pos.0, pos.1 - 1, pos.2)))
    {
        return;
    }
    let body = reg.state_of("minecraft:twisting_vines_plant", None);
    for _ in 0..(width * width) {
        let dx = rng.next_inbetween(-width, width);
        let dy = rng.next_inbetween(-height, height);
        let dz = rng.next_inbetween(-width, width);
        let (x, mut y, z) = (pos.0 + dx, pos.1 + dy, pos.2 + dz);
        // walk down to the ground, as the native helper does
        let mut found = false;
        loop {
            y -= 1;
            if ctx.out_of_h(y) {
                break;
            }
            if !ctx.is_air(x, y, z) {
                y += 1;
                found = true;
                break;
            }
        }
        if !found || !ctx.is_air(x, y, z) || !is_vine_ground(reg.name_of(ctx.get(x, y - 1, z))) {
            continue;
        }
        let mut col = rng.next_inbetween(1, max_height);
        if rng.next_bound(6) == 0 {
            col *= 2;
        }
        if rng.next_bound(5) == 0 {
            col = 1;
        }
        grow_vines(
            ctx,
            rng,
            (x, y, z),
            col,
            false,
            body,
            "minecraft:twisting_vines",
        );
    }
}

/// `WeepingVinesFeature` (`edx`). See [`Cf::WeepingVines`].
fn gen_weeping_vines(ctx: &mut FC, rng: &mut Xor, pos: (i32, i32, i32)) {
    let Some(reg) = breg() else { return };
    if !ctx.is_air(pos.0, pos.1, pos.2) {
        return;
    }
    let above = reg.name_of(ctx.get(pos.0, pos.1 + 1, pos.2));
    if !matches!(
        above,
        "minecraft:netherrack" | "minecraft:nether_wart_block"
    ) {
        return;
    }
    let wart = reg.state_of("minecraft:nether_wart_block", None);
    // the wart sweep: 200 attempts, keeping only cells with exactly one wart neighbour
    ctx.set(pos.0, pos.1, pos.2, wart);
    for _ in 0..200 {
        let dx = rng.next_bound(6) - rng.next_bound(6);
        let dy = rng.next_bound(2) - rng.next_bound(5);
        let dz = rng.next_bound(6) - rng.next_bound(6);
        let (x, y, z) = (pos.0 + dx, pos.1 + dy, pos.2 + dz);
        if !ctx.is_air(x, y, z) {
            continue;
        }
        let mut adjacent = 0;
        for (ax, ay, az) in [
            (0, -1, 0),
            (0, 1, 0),
            (0, 0, -1),
            (0, 0, 1),
            (-1, 0, 0),
            (1, 0, 0),
        ] {
            let name = reg.name_of(ctx.get(x + ax, y + ay, z + az));
            if matches!(name, "minecraft:netherrack" | "minecraft:nether_wart_block") {
                adjacent += 1;
            }
            if adjacent > 1 {
                break;
            }
        }
        if adjacent == 1 {
            ctx.set(x, y, z, wart);
        }
    }
    let body = reg.state_of("minecraft:weeping_vines_plant", None);
    for _ in 0..100 {
        let dx = rng.next_bound(8) - rng.next_bound(8);
        let dy = rng.next_bound(2) - rng.next_bound(7);
        let dz = rng.next_bound(8) - rng.next_bound(8);
        let (x, y, z) = (pos.0 + dx, pos.1 + dy, pos.2 + dz);
        if !ctx.is_air(x, y, z) {
            continue;
        }
        let up = reg.name_of(ctx.get(x, y + 1, z));
        if !matches!(up, "minecraft:netherrack" | "minecraft:nether_wart_block") {
            continue;
        }
        let mut col = rng.next_inbetween(1, 8);
        if rng.next_bound(6) == 0 {
            col *= 2;
        }
        if rng.next_bound(5) == 0 {
            col = 1;
        }
        grow_vines(
            ctx,
            rng,
            (x, y, z),
            col,
            true,
            body,
            "minecraft:weeping_vines",
        );
    }
}

/// `BlockPos.withinManhattan`, in the native order (measured, not assumed): cells
/// sorted by manhattan distance, then `x`, then `y`, then `|z|` with the positive
/// offset before the negative one. The order matters for features that write while
/// they iterate, because a later candidate can read an earlier write.
pub(crate) fn within_manhattan(xr: i32, yr: i32, zr: i32) -> Vec<(i32, i32, i32)> {
    let mut cells = Vec::new();
    for x in -xr..=xr {
        for y in -yr..=yr {
            for z in -zr..=zr {
                let d = x.abs() + y.abs() + z.abs();
                cells.push((d, x, y, z));
            }
        }
    }
    cells.sort_by_key(|&(d, x, y, z)| (d, x, y, z.abs(), i32::from(z < 0)));
    cells.into_iter().map(|(_, x, y, z)| (x, y, z)).collect()
}

/// `ReplaceBlobsFeature` (`ede`). See [`Cf::NetherrackReplaceBlobs`].
fn gen_netherrack_replace_blobs(
    ctx: &mut FC,
    rng: &mut Xor,
    pos: (i32, i32, i32),
    target: u32,
    state: u32,
    radius: &IP,
) {
    let Some(reg) = breg() else { return };
    let target = reg.name_of(target);
    // `origin.mutable().clamp(Direction.UP, minY + 1, maxY - 1)`
    let mut y = pos.1.clamp(
        ctx.g.world_min_y + 1,
        ctx.g.world_min_y + ctx.g.world_height - 1,
    );
    let mut found = None;
    while y > ctx.g.world_min_y + 1 {
        if reg.name_of(ctx.get(pos.0, y, pos.2)) == target {
            found = Some((pos.0, y, pos.2));
            break;
        }
        y -= 1;
    }
    let Some(p) = found else { return };
    let (i, j, k) = (radius.get(rng), radius.get(rng), radius.get(rng));
    let max = i.max(j).max(k);
    for (dx, dy, dz) in within_manhattan(i, j, k) {
        if dx.abs() + dy.abs() + dz.abs() > max {
            break;
        }
        let (x, y, z) = (p.0 + dx, p.1 + dy, p.2 + dz);
        if reg.name_of(ctx.get(x, y, z)) == target {
            ctx.set(x, y, z, state);
        }
    }
}

/// `DeltaFeature` (`ebv`). See [`Cf::DeltaFeature`].
fn gen_delta_feature(
    ctx: &mut FC,
    rng: &mut Xor,
    pos: (i32, i32, i32),
    contents: u32,
    rim: u32,
    size: &IP,
    rim_size: &IP,
) {
    let Some(reg) = breg() else { return };
    // `nextDouble() < 0.9` (the native constant is a double, and `j()` is nextDouble)
    let flag = rng.next_f64() < 0.9;
    let (rim_x, rim_z) = if flag {
        (rim_size.get(rng), rim_size.get(rng))
    } else {
        (0, 0)
    };
    let has_rim = flag && rim_x != 0 && rim_z != 0;
    let (size_x, size_z) = (size.get(rng), size.get(rng));
    let max_size = size_x.max(size_z);
    // bedrock, nether_bricks, fence, stairs, nether_wart, chest, spawner
    const CANNOT: [&str; 7] = [
        "minecraft:bedrock",
        "minecraft:nether_bricks",
        "minecraft:nether_brick_fence",
        "minecraft:nether_brick_stairs",
        "minecraft:nether_wart",
        "minecraft:chest",
        "minecraft:spawner",
    ];
    let is_delta = |ctx: &mut FC, reg: &BReg, x: i32, y: i32, z: i32| -> bool {
        let name = reg.name_of(ctx.get(x, y, z));
        if name == reg.name_of(contents) || CANNOT.contains(&name) {
            return false;
        }
        for (dx, dy, dz) in [
            (0, -1, 0),
            (0, 1, 0),
            (0, 0, -1),
            (0, 0, 1),
            (-1, 0, 0),
            (1, 0, 0),
        ] {
            // air anywhere but above rejects; the cell above must be air
            let empty = ctx.is_air(x + dx, y + dy, z + dz);
            if empty != (dy == 1) {
                return false;
            }
        }
        true
    };
    for (dx, _, dz) in within_manhattan(size_x, 0, size_z) {
        if dx.abs() + dz.abs() > max_size {
            break;
        }
        let (x, z) = (pos.0 + dx, pos.2 + dz);
        if !is_delta(ctx, reg, x, pos.1, z) {
            continue;
        }
        if has_rim {
            ctx.set(x, pos.1, z, rim);
        }
        let (qx, qz) = (x + rim_x, z + rim_z);
        if is_delta(ctx, reg, qx, pos.1, qz) {
            ctx.set(qx, pos.1, qz, contents);
        }
    }
}

/// `NetherForestVegetationFeature` (`ecx`). See [`Cf::NetherForestVegetation`].
fn gen_nether_forest_vegetation(
    ctx: &mut FC,
    rng: &mut Xor,
    pos: (i32, i32, i32),
    bsp: &Bsp,
    width: i32,
    height: i32,
) {
    let Some(reg) = breg() else { return };
    if !reg.in_tag(ctx.get(pos.0, pos.1 - 1, pos.2), "minecraft:nylium") {
        return;
    }
    let y = pos.1;
    if y < ctx.g.world_min_y + 1 || y + 1 >= ctx.g.world_min_y + ctx.g.world_height {
        return;
    }
    for _ in 0..(width * width) {
        let x = pos.0 + rng.next_bound(width) - rng.next_bound(width);
        let yy = y + rng.next_bound(height) - rng.next_bound(height);
        let z = pos.2 + rng.next_bound(width) - rng.next_bound(width);
        let st = bsp.get(ctx, rng, x, yy, z);
        if ctx.is_air(x, yy, z)
            && yy > ctx.g.min_y
            && crate::generation_providers::can_place(reg, ctx, reg.name_of(st), x, yy, z)
        {
            ctx.set(x, yy, z, st);
        }
    }
}

/// `VoidStartPlatformFeature` (`edv`), config `{}`. See [`Cf::VoidStartPlatform`].
fn gen_void_start_platform(ctx: &mut FC, pos: (i32, i32, i32)) {
    let Some(reg) = breg() else { return };
    let cobble = reg.state_of("minecraft:cobblestone", None);
    let stone = reg.state_of("minecraft:stone", None);
    let (cx, cz) = (pos.0 >> 4, pos.2 >> 4);
    // `ChunkPos(START)` is chunk (0,0) because START is (8, 3, 8).
    if cx.abs().max(cz.abs()) > 1 {
        return;
    }
    let start = (8, pos.1 + 3, 8);
    // Measured cells: x spans the chunk's x range and z its z range, so the native
    // `ChunkPos` accessors the body reads are the missing quadrature: iterating either
    // way gives the same set because the chebyshev test is symmetric in (x, z).
    for x in cx * 16..cx * 16 + 16 {
        for z in cz * 16..cz * 16 + 16 {
            if (start.0 - x).abs().max((start.2 - z).abs()) <= 16 {
                let state = if (x, start.1, z) == start {
                    cobble
                } else {
                    stone
                };
                ctx.set(x, start.1, z, state);
            }
        }
    }
}

fn gen_end_platform(ctx: &mut FC, pos: (i32, i32, i32)) {
    let Some(reg) = breg() else { return };
    let obsidian = reg.state_of("minecraft:obsidian", None);
    let air = reg.state_of("minecraft:air", None);
    for dx in -2i32..=2 {
        for dz in -2i32..=2 {
            for dy in -1i32..3 {
                let (x, y, z) = (pos.0 + dz, pos.1 + dy, pos.2 + dx);
                let want = if dy == -1 { obsidian } else { air };
                // `getBlockState(p).is(block)`: compare the block, not the state
                if reg.name_of(ctx.get(x, y, z)) != reg.name_of(want) {
                    ctx.set(x, y, z, want);
                }
            }
        }
    }
}

/// `BonusChestFeature` (`ebo`), config `{}`. See [`Cf::BonusChest`].
///
/// Read from `ebo.a(ecg)` in the pinned jar. The scan runs over the **origin's own
/// chunk** (`new ChunkPos(context.origin())`), and both coordinate lists are shuffled
/// before the first heightmap read — `Util.toShuffledList(rangeClosed(minBlockX,
/// maxBlockX))` for X, then the same for Z — so 30 `nextInt` draws precede any read,
/// and the walk is the shuffled cross product with X outermost.
///
/// A cell is usable when `isEmptyBlock(pos) || getCollisionShape(level, pos).isEmpty()`,
/// which is why a plant (no collision shape) does not stop the chest while a leaf
/// block, which `MOTION_BLOCKING_NO_LEAVES` does not count as a surface, does.
/// The chest is written in its default state — there is no orientation pass — and
/// `RandomizableContainer.setBlockEntityLootTable` then draws one `nextLong`. That
/// draw happens because `WorldGenRegion.setBlock` leaves a `DUMMY` block-entity tag
/// for the new state (`arb.a(jd,dtc,int,int)`), and `WorldGenRegion.getBlockEntity`
/// materialises the chest from it, so the `instanceof RandomizableContainer` guard
/// in `bqz.a(dcc,ayw,jd,akq)` passes rather than skipping the seed.
///
/// The torches are **not** gated on the neighbour being empty: the native body asks
/// only `torch.canSurvive(level, pos)` — `canSupportCenter(level, pos.below(), UP)` —
/// and writes over whatever sits there. The support test is the pinned full UP face.
/// Vanilla asks with `SupportType.CENTER`, which additionally accepts a support whose
/// 2x2 centre column covers the face (a fence post); no generated bonus-chest surface
/// is one, so the two predicates agree on every cell this feature reaches.
fn gen_bonus_chest(ctx: &mut FC, rng: &mut Xor, pos: (i32, i32, i32)) {
    let Some(reg) = breg() else { return };
    let (cx, cz) = (pos.0 >> 4, pos.2 >> 4);
    let mut xs: Vec<i32> = (cx * 16..cx * 16 + 16).collect();
    let mut zs: Vec<i32> = (cz * 16..cz * 16 + 16).collect();
    feature_util_shuffle(&mut xs, rng);
    feature_util_shuffle(&mut zs, rng);
    let chest = reg.state_of("minecraft:chest", None);
    let torch = reg.state_of("minecraft:torch", None);
    // `Direction.Plane.HORIZONTAL` iterates north, east, south, west: the plane
    // enum's `a` constant (`dcd`'s sibling `ji$c.a`) holds `{ji.c, ji.f, ji.d,
    // ji.e}`, the direction enum's own order with the vertical halves dropped — not
    // `ji.c..ji.f`. The four cells are independent, but the list is written in the
    // native order rather than a remembered one.
    const SIDES: [(i32, i32); 4] = [(0, -1), (1, 0), (0, 1), (-1, 0)];
    for &x in &xs {
        for &z in &zs {
            let y = ctx.heightmap(HeightmapKind::MotionBlockingNoLeaves, x, z);
            if ctx.out_of_h(y) {
                // A column filled to the build limit puts the first free cell outside the
                // world. The native `setBlock` is refused there (`WorldGenRegion` checks
                // `isOutsideBuildHeight` first), so no chest is written, no block entity
                // exists to take the loot seed, and no torch survives - the walk still ends.
                return;
            }
            let cur = ctx.get(x, y, z);
            if !(reg.is_air(cur) || reg.is_collision_empty(cur)) {
                continue;
            }
            ctx.set(x, y, z, chest);
            let loot_seed = rng.next_i64();
            ctx.push_loot_container(
                (x, y, z),
                crate::generation_context::ContainerKind::Chest,
                "minecraft:chests/spawn_bonus_chest".to_owned(),
                Some(loot_seed),
            );
            for (dx, dz) in SIDES {
                let below = ctx.get(x + dx, y - 1, z + dz);
                // `canSupportCenter(level, pos.below(), UP)` is `SupportType.CENTER`, not FULL:
                // a fence post or a wall under the torch supports it while covering no whole
                // face, and the two planes differ on 9,940 states for UP alone.
                if face_sturdy(reg, below, 1, Support::Center) {
                    ctx.set(x + dx, y, z + dz, torch);
                }
            }
            return;
        }
    }
}

/// `BasaltPillarFeature` (`ebj`), config `{}`. See [`Cf::BasaltPillar`].
///
/// The four sideways attempts are listed in the native order — north, south, west,
/// east (`ji.c`, `ji.d`, `ji.e`, `ji.f`, read from the enum's own names) — and the
/// short circuit is reproduced literally: once a direction draws `nextInt(10) == 0`
/// it is never attempted (or drawn) again down the rest of the column.
fn gen_basalt_pillar(ctx: &mut FC, rng: &mut Xor, pos: (i32, i32, i32)) {
    let Some(reg) = breg() else { return };
    let basalt = reg.state_of("minecraft:basalt", None);
    if !ctx.is_air(pos.0, pos.1, pos.2) || ctx.is_air(pos.0, pos.1 + 1, pos.2) {
        return;
    }
    // north, south, west, east
    const SIDES: [(i32, i32); 4] = [(0, -1), (0, 1), (-1, 0), (1, 0)];
    let mut hang = [true; 4];
    let (x, mut y, z) = pos;
    while ctx.is_air(x, y, z) {
        if ctx.out_of_h(y) {
            return;
        }
        ctx.set(x, y, z, basalt);
        for (i, (dx, dz)) in SIDES.iter().enumerate() {
            if !hang[i] {
                continue;
            }
            if rng.next_bound(10) != 0 {
                ctx.set(x + dx, y, z + dz, basalt);
            } else {
                hang[i] = false;
            }
        }
        y -= 1;
    }
    y += 1;
    for (dx, dz) in SIDES {
        if rng.next_bool() {
            ctx.set(x + dx, y, z + dz, basalt);
        }
    }
    y -= 1;
    for dx in -3i32..=3 {
        for dz in -3i32..=3 {
            if rng.next_bound(10) >= 10 - (dx.abs() * dz.abs()) {
                continue;
            }
            let (mx, mut my, mz) = (x + dx, y, z + dz);
            let mut left = 3;
            while ctx.is_air(mx, my - 1, mz) {
                my -= 1;
                left -= 1;
                if left <= 0 {
                    break;
                }
            }
            if !ctx.is_air(mx, my - 1, mz) {
                ctx.set(mx, my, mz, basalt);
            }
        }
    }
}

/// `Feature.isAirOrLeaves`: air, or any block in the vanilla leaves tag.
fn is_air_or_leaves(reg: &BReg, st: u32) -> bool {
    reg.is_air(st) || reg.in_tag(st, "minecraft:leaves")
}

/// `IceSpikeFeature` (`ecq`). The `&&`/`||` short circuits matter: the corner
/// `nextFloat()` is drawn only for edge cells, so the stream depends on the shape
/// the loop has reached.
fn gen_ice_spike(ctx: &mut FC, rng: &mut Xor, pos: (i32, i32, i32)) {
    let Some(reg) = breg() else { return };
    // `Blocks.dP`/`dO`/`iC` in the bytecode: snow_block, ice, packed_ice
    let snow = reg.state_of("minecraft:snow_block", None);
    let ice_block = reg.state_of("minecraft:ice", None);
    let packed = reg.state_of("minecraft:packed_ice", None);
    let min_y = ctx.g.min_y;

    let mut p = pos;
    while ctx.is_air(p.0, p.1, p.2) && p.1 > min_y + 2 {
        p.1 -= 1;
    }
    if ctx.get(p.0, p.1, p.2) != snow {
        return;
    }
    p.1 += rng.next_bound(4);
    let height = rng.next_bound(4) + 7;
    let radius_max = height / 4 + rng.next_bound(2);
    if radius_max > 1 && rng.next_bound(60) == 0 {
        p.1 += 10 + rng.next_bound(30);
    }

    for h in 0..height {
        let f = (1.0 - h as f32 / height as f32) * radius_max as f32;
        let r = f.floor() as i32;
        for dx in -r..=r {
            let fx = dx.abs() as f32 - 0.25;
            for dz in -r..=r {
                let fz = dz.abs() as f32 - 0.25;
                // Java negates float comparisons (`!(a > b)`), which is true for NaN
                // where Rust's `<=` is false, so the negation is kept and only named
                // out of clippy's way - the shape of the comparison is the reference's.
                let outside = fx * fx + fz * fz > f * f;
                let inside = (dx == 0 && dz == 0) || !outside;
                let edge = dx != -r && dx != r && dz != -r && dz != r;
                // the draw happens only when the cell is on the rim (Java `||`)
                let passes = if edge {
                    true
                } else {
                    let roll = rng.next_f32() > 0.75;
                    !roll
                };
                if !(inside && passes) {
                    continue;
                }
                for (yy, sign) in [(h, 1), (h, -1)] {
                    if sign < 0 && (h == 0 || r <= 1) {
                        continue;
                    }
                    let q = (p.0 + dx, p.1 + yy * sign, p.2 + dz);
                    let at = ctx.get(q.0, q.1, q.2);
                    if is_air_or_leaves(reg, at) || at == snow || at == ice_block {
                        ctx.set(q.0, q.1, q.2, packed);
                    }
                }
            }
        }
    }

    let bottom_r = (radius_max - 1).clamp(0, 1);
    for dx in -bottom_r..=bottom_r {
        for dz in -bottom_r..=bottom_r {
            let mut q = (p.0 + dx, p.1 - 1, p.2 + dz);
            let mut budget = 50;
            if dx.abs() == 1 && dz.abs() == 1 {
                budget = rng.next_bound(5);
            }
            while q.1 > 50 {
                let at = ctx.get(q.0, q.1, q.2);
                if !(is_air_or_leaves(reg, at) || at == snow || at == ice_block || at == packed) {
                    break;
                }
                ctx.set(q.0, q.1, q.2, packed);
                q.1 -= 1;
                budget -= 1;
                if budget <= 0 {
                    // `BlockPos.c(int)` is `below(int)`: the drill skips *down*, and
                    // that is also what ends the loop (a jump up never reaches 50).
                    q.1 -= rng.next_bound(5) + 1;
                    budget = rng.next_bound(5);
                }
            }
        }
    }
}

fn gen_geode(ctx: &mut FC, rng: &mut Xor, origin: (i32, i32, i32), g: &GeodeCfg) {
    let Some(reg) = breg() else { return };
    let trace = env_flag!("GEODE_TRACE");
    if trace {
        eprintln!("GEODE {} {} {}", origin.0, origin.1, origin.2);
    }
    let points_n = g.distribution_points.get(rng);
    let noise = crate::generation_math::normal_new_l(ctx.g.wseed(), -4, &[1.0]);
    let d0 = points_n as f64 / g.outer_wall_distance.max_inclusive() as f64;
    let d1 = 1.0 / g.layers[0].sqrt();
    let d2 = 1.0 / (g.layers[1] + d0).sqrt();
    let d3 = 1.0 / (g.layers[2] + d0).sqrt();
    let d4 = 1.0 / (g.layers[3] + d0).sqrt();
    let d5 =
        1.0 / (g.crack_size + rng.next_f64() / 2.0 + if points_n > 3 { d0 } else { 0.0 }).sqrt();
    let has_crack = (rng.next_f32() as f64) < g.crack_chance;

    let mut points: Vec<((i32, i32, i32), i32)> = Vec::with_capacity(points_n.max(0) as usize);
    let mut invalid = 0;
    for _ in 0..points_n.max(0) {
        let p = (
            origin.0 + g.outer_wall_distance.get(rng),
            origin.1 + g.outer_wall_distance.get(rng),
            origin.2 + g.outer_wall_distance.get(rng),
        );
        let st = ctx.get(p.0, p.1, p.2);
        if trace {
            eprintln!("GREAD {} {} {} state={st}", p.0, p.1, p.2);
        }
        if reg.is_air(st) || reg.in_tag(st, &g.invalid) {
            invalid += 1;
            if invalid > g.invalid_blocks_threshold {
                return;
            }
        }
        points.push((p, g.point_offset.get(rng)));
    }

    let mut crack_points: Vec<(i32, i32, i32)> = Vec::new();
    if has_crack {
        let ext = points_n * 2 + 1;
        let offsets = match rng.next_bound(4) {
            0 => [(ext, 7, 0), (ext, 5, 0), (ext, 1, 0)],
            1 => [(0, 7, ext), (0, 5, ext), (0, 1, ext)],
            2 => [(ext, 7, ext), (ext, 5, ext), (ext, 1, ext)],
            _ => [(0, 7, 0), (0, 5, 0), (0, 1, 0)],
        };
        for (a, b, c) in offsets {
            crack_points.push((origin.0 + a, origin.1 + b, origin.2 + c));
        }
    }

    let dist_sqr = |p: (i32, i32, i32), q: (i32, i32, i32)| {
        let (dx, dy, dz) = ((p.0 - q.0) as f64, (p.1 - q.1) as f64, (p.2 - q.2) as f64);
        dx * dx + dy * dy + dz * dz
    };
    let mut inner_placements: Vec<(i32, i32, i32)> = Vec::new();
    // `BlockPos.betweenClosed` decodes its linear index as `x = i % width`,
    // `y = (i / width) % height`, `z = i / (width * height)`: x is the *fastest*
    // axis and z the slowest. The layer providers draw from the feature RNG per
    // block, so the iteration order is the draw order.
    for z in (origin.2 + g.min_gen_offset)..=(origin.2 + g.max_gen_offset) {
        for y in (origin.1 + g.min_gen_offset)..=(origin.1 + g.max_gen_offset) {
            for x in (origin.0 + g.min_gen_offset)..=(origin.0 + g.max_gen_offset) {
                let at = (x, y, z);
                let noise_v = noise.get(x as f64, y as f64, z as f64) * g.noise_multiplier;
                let mut d6 = 0.0;
                for (p, off) in points.iter() {
                    d6 += 1.0 / (dist_sqr(at, *p) + *off as f64).sqrt() + noise_v;
                }
                let mut d7 = 0.0;
                for cp in crack_points.iter() {
                    d7 += 1.0 / (dist_sqr(at, *cp) + g.crack_offset as f64).sqrt() + noise_v;
                }
                if d6 < d4 {
                    continue;
                }
                if has_crack && d7 >= d5 && d6 < d1 {
                    let st = g.filling.get(ctx, rng, x, y, z);
                    safe_feature_set(ctx, at, st, &g.cannot_replace);
                    for (dx, dy, dz) in [
                        (0, -1, 0),
                        (0, 1, 0),
                        (0, 0, -1),
                        (0, 0, 1),
                        (-1, 0, 0),
                        (1, 0, 0),
                    ] {
                        let np = (x + dx, y + dy, z + dz);
                        if !ctx.out_of_h(np.1) {
                            // `FluidState.createLegacyBlock()` on the neighbour, then
                            // `setBlock(..., 0)`: a dry neighbour is left alone (air
                            // included), a fluid block keeps its own level, and a
                            // waterlogged solid holds source water so it becomes a plain
                            // water block.
                            let ns = ctx.get(np.0, np.1, np.2);
                            let fluid = reg.fluid_name(ns);
                            let fluid_id = match fluid {
                                "minecraft:water" => Some(0),
                                "minecraft:flowing_water" => Some(1),
                                "minecraft:lava" => Some(2),
                                "minecraft:flowing_lava" => Some(3),
                                _ => None,
                            };
                            if let Some(fluid_id) = fluid_id {
                                // Vanilla schedules the neighbour's existing fluid for a
                                // zero-delay tick here. It does not collapse a waterlogged
                                // solid into a bare liquid block: the solid keeps its shape
                                // and the scheduled fluid tick computes the real update.
                                ctx.g.schedule_generated_fluid(np, fluid_id);
                            }
                        }
                    }
                } else if d6 >= d1 {
                    let st = g.filling.get(ctx, rng, x, y, z);
                    safe_feature_set(ctx, at, st, &g.cannot_replace);
                } else if d6 >= d2 {
                    let alt = (rng.next_f32() as f64) < g.use_alt_layer0;
                    let st = if alt {
                        g.alt_inner.get(ctx, rng, x, y, z)
                    } else {
                        g.inner.get(ctx, rng, x, y, z)
                    };
                    safe_feature_set(ctx, at, st, &g.cannot_replace);
                    if (!g.placements_require_alt || alt)
                        && (rng.next_f32() as f64) < g.use_potential_placements
                    {
                        inner_placements.push(at);
                    }
                } else if d6 >= d3 {
                    let st = g.middle.get(ctx, rng, x, y, z);
                    safe_feature_set(ctx, at, st, &g.cannot_replace);
                } else {
                    let st = g.outer.get(ctx, rng, x, y, z);
                    safe_feature_set(ctx, at, st, &g.cannot_replace);
                }
            }
        }
    }

    if g.placements.is_empty() {
        return;
    }
    const DIRS: [(i32, i32, i32, &str); 6] = [
        (0, -1, 0, "down"),
        (0, 1, 0, "up"),
        (0, 0, -1, "north"),
        (0, 0, 1, "south"),
        (-1, 0, 0, "west"),
        (1, 0, 0, "east"),
    ];
    for p in inner_placements {
        let mut st = g.placements[rng.next_bound(g.placements.len() as i32) as usize];
        for (dx, dy, dz, dir) in DIRS {
            if reg.has_prop(reg.name_of(st), "facing") {
                st = reg.with_prop(st, "facing", dir);
            }
            let np = (p.0 + dx, p.1 + dy, p.2 + dz);
            if ctx.out_of_h(np.1) {
                continue;
            }
            let ns = ctx.get(np.0, np.1, np.2);
            if reg.has_prop(reg.name_of(st), "waterlogged") {
                // `FluidState.isSource()`, not "is this block a liquid": a
                // waterlogged solid holds source water and logs as true.
                let source = matches!(reg.fluid_name(ns), "minecraft:water" | "minecraft:lava");
                st = reg.with_prop(st, "waterlogged", if source { "true" } else { "false" });
            }
            // `BuddingAmethystBlock.canClusterGrowAtState`: air, or a *water* block
            // holding a full 8 units (a source). Lava is not a growth target.
            let growable = reg.is_air(ns)
                || (reg.name_of(ns) == "minecraft:water"
                    && reg.get_prop(ns, "level").is_some_and(|l| l == "0"));
            if growable {
                safe_feature_set(ctx, np, st, &g.cannot_replace);
                break;
            }
        }
    }
}

/// FossilFeature's two immutable templates share one placement origin and RNG.
/// Resource resolution happens while loading the configured feature, not per attempt.
struct Fossil {
    base: Vec<crate::structure_template::Template>,
    overlay: Vec<crate::structure_template::Template>,
    base_processors: Vec<crate::structure_template::Processor>,
    overlay_processors: Vec<crate::structure_template::Processor>,
    max_empty_corners: i32,
}
impl Fossil {
    fn parse(j: &J) -> Option<Self> {
        use crate::structure_template::{Processor, Template};
        let reg = breg()?;
        let resource = |kind: &str, id: &str, ext: &str| {
            let bytes = Gen::dp_resource(kind, id, ext);
            if bytes.is_none() && env_flag!("CF_UNPARSED") {
                eprintln!("FOSSIL_RESOURCE_UNRESOLVED kind={kind} id={id}");
            }
            bytes
        };
        let templates = |key: &str| -> Option<Vec<Template>> {
            jarr(j, key)?
                .iter()
                .map(|v| {
                    let J::Str(id) = v else { return None };
                    let bytes = resource("structure", id, "nbt")?;
                    let (_, root) = crate::nbt::read_gzip(&bytes, 16 * 1024 * 1024).ok()?;
                    Template::load(&root, reg).ok()
                })
                .collect()
        };
        let processors = |key: &str| {
            let value = js(j, key)?;
            let decoded;
            let value = if let J::Str(id) = value {
                decoded = jparse(&resource("worldgen/processor_list", id, "json")?)?;
                &decoded
            } else {
                value
            };
            Processor::list(value, reg).ok()
        };
        let base = templates("fossil_structures")?;
        let overlay = templates("overlay_structures")?;
        if base.is_empty() || base.len() != overlay.len() {
            return None;
        }
        let base_processors = processors("fossil_processors")?;
        let overlay_processors = processors("overlay_processors")?;
        jnum(j, "max_empty_corners_allowed")?;
        let max_empty_corners = jint(j, "max_empty_corners_allowed", 0);
        if !(0..=7).contains(&max_empty_corners) {
            return None;
        }
        for (templates, processors) in [(&base, &base_processors), (&overlay, &overlay_processors)]
        {
            for template in templates {
                template.validate_solid_placement(processors, reg).ok()?;
            }
        }
        Some(Self {
            base,
            overlay,
            base_processors,
            overlay_processors,
            max_empty_corners,
        })
    }

    fn place(&self, ctx: &mut FC, rng: &mut Xor, pos: (i32, i32, i32)) -> bool {
        use crate::structure_template::Placement;
        use crate::structure_transform::{Mirror, Rotation};
        let rotation = Rotation::from_index(rng.next_bound(4) as u8).unwrap();
        let index = rng.next_bound(self.base.len() as i32) as usize;
        let base = &self.base[index];
        let (sx, sy, sz) = base.size;
        let (rx, rz) = match rotation {
            Rotation::Clockwise90 | Rotation::Counterclockwise90 => (sz, sx),
            _ => (sx, sz),
        };
        let centered = (
            pos.0.wrapping_add(rx.wrapping_neg() / 2),
            pos.2.wrapping_add(rz.wrapping_neg() / 2),
        );
        let mut floor = pos.1;
        for dx in 0..rx {
            for dz in 0..rz {
                floor = floor
                    .min(ctx.hm_oc_wg(centered.0.wrapping_add(dx), centered.1.wrapping_add(dz)));
            }
        }
        let y = (floor.wrapping_sub(15).wrapping_sub(rng.next_bound(10))).max(ctx.g.min_y + 10);
        let offset = match rotation {
            Rotation::None => (0, 0),
            Rotation::Clockwise90 => (sz.wrapping_sub(1), 0),
            Rotation::Clockwise180 => (sx.wrapping_sub(1), sz.wrapping_sub(1)),
            Rotation::Counterclockwise90 => (0, sx.wrapping_sub(1)),
        };
        let origin = (
            centered.0.wrapping_add(offset.0),
            y,
            centered.1.wrapping_add(offset.1),
        );
        let bounds = crate::structure_transform::bounds(
            (sx, sy, sz),
            origin,
            (0, 0, 0),
            Mirror::None,
            rotation,
        );
        let Some(reg) = breg() else { return false };
        let mut empty = 0;
        for x in [bounds[0], bounds[3]] {
            for y in [bounds[1], bounds[4]] {
                for z in [bounds[2], bounds[5]] {
                    let state = ctx.get(x, y, z);
                    if reg.is_air(state)
                        || matches!(reg.name_of(state), "minecraft:water" | "minecraft:lava")
                    {
                        empty += 1;
                    }
                }
            }
        }
        if empty > self.max_empty_corners {
            return false;
        }
        let clip = [
            (pos.0 >> 4) * 16 - 16,
            ctx.g.world_min_y,
            (pos.2 >> 4) * 16 - 16,
            (pos.0 >> 4) * 16 + 31,
            ctx.g.world_min_y + ctx.g.world_height,
            (pos.2 >> 4) * 16 + 31,
        ];
        let placement = Placement {
            origin,
            pivot: (0, 0, 0),
            mirror: Mirror::None,
            rotation,
        };
        base.place_solid_blocks(ctx, rng, placement, &self.base_processors, clip)
            .expect("prevalidated fossil base");
        self.overlay[index]
            .place_solid_blocks(ctx, rng, placement, &self.overlay_processors, clip)
            .expect("prevalidated fossil overlay");
        // Native ignores both placeInWorld return values, including empty/rotted templates.
        true
    }
}

fn p_cf(j: &J) -> Option<Cf> {
    let ty = jstr(j, "type")?;
    // 1.21.1 wraps a configured feature's parameters in a "config" object;
    // 1.21.2+ (and this repository's older copy) inlines them beside "type".
    let j = js(j, "config").unwrap_or(j);
    let ty = ty.strip_prefix("minecraft:").unwrap_or(&ty);
    Some(match ty {
        "fossil" => Cf::Fossil(Box::new(Fossil::parse(j)?)),
        "ore" | "scattered_ore" => {
            let reg = breg()?;
            let mut targets = vec![];
            for t in jarr(j, "targets").map(|v| v.as_slice()).unwrap_or(&[]) {
                let rt = js(t, "target").and_then(p_rt)?;
                let (n, p) = js(t, "state").and_then(bs_parts)?;
                targets.push((FastRT::compile(rt, reg), reg.state_of(&n, p)));
            }
            Cf::OreFast(Box::new(FastOre {
                size: jint(j, "size", 0),
                discard: jf32(j, "discard_chance_on_air_exposure", 0.0),
                targets,
                scattered: ty == "scattered_ore",
            }))
        }
        "disk" => Cf::Disk {
            half: jint(j, "half_height", 0),
            radius: p_ip(js(j, "radius")?)?,
            bsp: p_bsp(js(j, "state_provider")?)?,
            target: p_pred(js(j, "target")?)?,
        },
        "geode" => {
            let b = js(j, "blocks")?;
            let lay = js(j, "layers")?;
            let crack = js(j, "crack")?;
            let tag = |k: &str| -> Option<String> {
                Some(
                    ns_name(js(b, k)?.str().trim_start_matches('#'))
                        .trim_start_matches('#')
                        .to_string(),
                )
            };
            let mut placements = vec![];
            for p in jarr(b, "inner_placements")
                .map(|v| v.as_slice())
                .unwrap_or(&[])
            {
                let (n, pr) = bs_parts(p)?;
                placements.push(breg()?.state_of(&n, pr));
            }
            Cf::Geode(Box::new(GeodeCfg {
                filling: p_bsp(js(b, "filling_provider")?)?,
                inner: p_bsp(js(b, "inner_layer_provider")?)?,
                alt_inner: p_bsp(js(b, "alternate_inner_layer_provider")?)?,
                middle: p_bsp(js(b, "middle_layer_provider")?)?,
                outer: p_bsp(js(b, "outer_layer_provider")?)?,
                placements,
                cannot_replace: tag("cannot_replace")?,
                invalid: tag("invalid_blocks")?,
                layers: [
                    jnum(lay, "filling")?,
                    jnum(lay, "inner_layer")?,
                    jnum(lay, "middle_layer")?,
                    jnum(lay, "outer_layer")?,
                ],
                crack_chance: jnum(crack, "generate_crack_chance")?,
                crack_size: jnum(crack, "base_crack_size")?,
                crack_offset: jint(crack, "crack_point_offset", 0),
                use_alt_layer0: jnum(j, "use_alternate_layer0_chance")?,
                use_potential_placements: jnum(j, "use_potential_placements_chance")?,
                placements_require_alt: jbool(j, "placements_require_layer0_alternate", true),
                outer_wall_distance: p_ip(js(j, "outer_wall_distance")?)?,
                distribution_points: p_ip(js(j, "distribution_points")?)?,
                point_offset: p_ip(js(j, "point_offset")?)?,
                min_gen_offset: jint(j, "min_gen_offset", 0),
                max_gen_offset: jint(j, "max_gen_offset", 0),
                noise_multiplier: jnum(j, "noise_multiplier")?,
                invalid_blocks_threshold: jint(j, "invalid_blocks_threshold", 0),
            }))
        }
        "root_system" => Cf::RootSystem(Box::new(RootSystemCfg {
            feature: pf_any(js(j, "feature")?)?,
            required_vspace: jint(j, "required_vertical_space_for_tree", 0),
            root_radius: jint(j, "root_radius", 0),
            root_replaceable: ns_name(js(j, "root_replaceable")?.str().trim_start_matches('#')),
            root_state: p_bsp(js(j, "root_state_provider")?)?,
            root_attempts: jint(j, "root_placement_attempts", 0),
            root_column_max_height: jint(j, "root_column_max_height", 0),
            hanging_radius: jint(j, "hanging_root_radius", 0),
            hanging_span: jint(j, "hanging_roots_vertical_span", 0),
            hanging_state: p_bsp(js(j, "hanging_root_state_provider")?)?,
            hanging_attempts: jint(j, "hanging_root_placement_attempts", 0),
            water_for_tree: jint(j, "allowed_vertical_water_for_tree", 0),
            allowed_tree_position: p_pred(js(j, "allowed_tree_position")?)?,
        })),
        "large_dripstone" => Cf::LargeDripstone(Box::new(LargeDripstoneCfg {
            range: jint(j, "floor_to_ceiling_search_range", 30),
            radius: p_ip(js(j, "column_radius")?)?,
            height_scale: p_fp(js(j, "height_scale")?)?,
            ratio: jf32(j, "max_column_radius_to_cave_height_ratio", 1.0),
            stalactite_bluntness: p_fp(js(j, "stalactite_bluntness")?)?,
            stalagmite_bluntness: p_fp(js(j, "stalagmite_bluntness")?)?,
            wind_speed: p_fp(js(j, "wind_speed")?)?,
            min_radius_for_wind: jint(j, "min_radius_for_wind", 0),
            min_bluntness_for_wind: jf32(j, "min_bluntness_for_wind", 0.0),
        })),
        "dripstone_cluster" => Cf::DripstoneCluster(Box::new(DripstoneClusterCfg {
            range: jint(j, "floor_to_ceiling_search_range", 12),
            height: p_ip(js(j, "height")?)?,
            radius: p_ip(js(j, "radius")?)?,
            max_diff: jint(j, "max_stalagmite_stalactite_height_diff", 1),
            height_deviation: jint(j, "height_deviation", 3),
            thickness: p_ip(js(j, "dripstone_block_layer_thickness")?)?,
            density: p_fp(js(j, "density")?)?,
            wetness: p_fp(js(j, "wetness")?)?,
            chance: jf32(
                j,
                "chance_of_dripstone_column_at_max_distance_from_center",
                0.1,
            ),
            max_dist_edge: jint(
                j,
                "max_distance_from_edge_affecting_chance_of_dripstone_column",
                3,
            ),
            max_dist_center: jint(j, "max_distance_from_center_affecting_height_bias", 8),
        })),
        "iceberg" => Cf::Iceberg {
            state: {
                let (n, p) = bs_parts(js(j, "state")?)?;
                breg()?.state_of(&n, p)
            },
        },
        "desert_well" => Cf::DesertWell,
        "ice_spike" => Cf::IceSpike,
        "sea_pickle" => Cf::SeaPickle {
            count: p_ip(js(j, "count")?)?,
        },
        "simple_block" => Cf::SimpleBlock {
            to: p_bsp(js(j, "to_place")?)?,
            on: name_list(j, "can_place_on"),
            in_: name_list(j, "can_place_in"),
            under: name_list(j, "can_place_under"),
        },
        "spring_feature" => {
            let reg = breg()?;
            // the config holds a *fluid* state and vanilla places its legacy block
            // (`createLegacyBlock`): falling water is level 8, a source is level 0
            let fluid = {
                let st = js(j, "state")?;
                let (n, p) = bs_parts(st)?;
                let falling = J::Obj(vec![(
                    "level".to_string(),
                    J::Str(
                        if jbool(st, "falling", false) {
                            "8"
                        } else {
                            "0"
                        }
                        .to_string(),
                    ),
                )]);
                let props = if n.ends_with("water") || n.ends_with("lava") {
                    Some(&falling)
                } else {
                    p
                };
                reg.state_of(&n, props)
            };
            let fluid_id = match reg.fluid_name(fluid) {
                "minecraft:water" => 0,
                "minecraft:flowing_water" => 1,
                "minecraft:lava" => 2,
                "minecraft:flowing_lava" => 3,
                _ => -1,
            };
            let valid: HashSet<String> = name_list(j, "valid_blocks").into_iter().collect();
            let valid_bits = Box::new(FeatBlockBitset::from_names(
                reg,
                valid.iter().map(String::as_str),
            ));
            Cf::Spring {
                fluid,
                fluid_id,
                req_below: jbool(j, "requires_block_below", true),
                rock: jint(j, "rock_count", 4),
                hole: jint(j, "hole_count", 1),
                valid,
                valid_bits,
            }
        }
        "seagrass" => Cf::Seagrass {
            prob: jf32(j, "probability", 0.0),
        },
        "kelp" => Cf::Kelp,
        // `minecraft:flower` is the same config shape (a patch of simple blocks), just a
        // different type name - it was one of the 34 types with no arm, so every flower
        // patch in the pack did nothing.
        // `flower`, `no_bonemeal_flower` and `random_patch` are three registered
        // names for the same `edc` RandomPatchFeature (tests/data/feature_classes_1211.tsv),
        // so one arm serves all three.
        "flower" | "no_bonemeal_flower" | "random_patch" => Cf::RandomPatch {
            feature: Box::new(pf_any(js(j, "feature")?)?),
            tries: jint(j, "tries", 0),
            xz: jint(j, "xz_spread", 0),
            y: jint(j, "y_spread", 0),
        },
        "random_selector" => {
            let mut v = vec![];
            for e in jarr(j, "features")? {
                let pf = js(e, "feature").and_then(pf_any)?;
                let c = jf32(e, "chance", 0.0);
                v.push((pf, c));
            }
            Cf::RandSel(v, js(j, "default").and_then(pf_any).map(Box::new))
        }
        "simple_random_selector" => {
            let mut v = vec![];
            for e in jarr(j, "features")? {
                v.push(pf_any(e)?);
            }
            Cf::SimpleRand(v)
        }
        "weighted_random_selector" => {
            let mut v = vec![];
            for e in jarr(j, "features")? {
                let pf = js(e, "data").and_then(pf_any)?;
                v.push((pf, jint(e, "weight", 1)));
            }
            Cf::WeightedSel(v)
        }
        "random_boolean_selector" => Cf::BoolSel(
            p_pf(js(j, "feature_true")?)?,
            p_pf(js(j, "feature_false")?)?,
        ),
        "sequence" => {
            let mut v = vec![];
            for e in jarr(j, "features")? {
                v.push(pf_any(e)?);
            }
            Cf::Seq(v)
        }
        "vegetation_patch" | "waterlogged_vegetation_patch" => Cf::VegPatch {
            depth: p_ip(js(j, "depth")?)?,
            extra_bottom: jf32(j, "extra_bottom_block_chance", 0.0),
            extra_edge: jf32(j, "extra_edge_column_chance", 0.0),
            replaceable: jstr(j, "replaceable")
                .unwrap_or_default()
                .trim_start_matches('#')
                .to_string(),
            ground: p_bsp(js(j, "ground_state")?)?,
            floor: jstr(j, "surface").as_deref() != Some("ceiling"),
            chance: jf32(j, "vegetation_chance", 0.0),
            vfeat: Box::new(p_pf(js(j, "vegetation_feature")?)?),
            xz: p_ip(js(j, "xz_radius")?)?,
            vrange: jint(j, "vertical_range", 1),
            waterlog: ty == "waterlogged_vegetation_patch",
        },
        "block_column" => {
            let mut layers = vec![];
            for e in jarr(j, "layers")? {
                layers.push((
                    p_ip(js(e, "height")?)?,
                    p_bsp(js(e, "provider").or_else(|| js(e, "contents"))?)?,
                ));
            }
            Cf::BlockColumn {
                layers,
                down: jstr(j, "direction").as_deref() != Some("up"),
                allowed: js(j, "allowed_placement").and_then(p_pred),
                prio: jbool(j, "prioritize_tip", false),
            }
        }
        "block_pile" => Cf::BlockPile(p_bsp(js(j, "state_provider")?)?),
        "lake" => Cf::Lake {
            fluid: p_bsp(js(j, "fluid")?)?,
            barrier: p_bsp(js(j, "barrier")?)?,
        },
        "blue_ice" => Cf::BlueIce,
        "block_blob" | "forest_rock" => {
            let (n, p) = js(j, "state").and_then(bs_parts)?;
            Cf::BlockBlob {
                state: breg()?.state_of(&n, p),
            }
        }
        "underwater_magma" => Cf::UnderMagma {
            floor: jint(j, "floor_search_range", 5),
            prob: jf32(j, "placement_probability_per_valid_position", 0.5),
            radius: jint(j, "placement_radius_around_floor", 1),
        },
        "freeze_top_layer" => Cf::FreezeTop,
        "multiface_growth" => {
            let reg = breg()?;
            let mut dirs = Vec::new();
            if jbool(j, "can_place_on_ceiling", false) {
                dirs.push(1);
            }
            if jbool(j, "can_place_on_floor", false) {
                dirs.push(0);
            }
            if jbool(j, "can_place_on_wall", false) {
                dirs.extend([2, 5, 3, 4]);
            }
            let name = ns_name(&jstr(j, "block")?);
            let base_state = reg.state_of(&name, None);
            let waterlogged_base = reg.with_prop(base_state, "waterlogged", "true");
            let block_id = reg.block_id_of(base_state).unwrap_or(-1);
            let on: HashSet<String> = name_list(j, "can_be_placed_on").into_iter().collect();
            let on_bits = FeatBlockBitset::from_names(reg, on.iter().map(String::as_str));
            Cf::Multiface(Box::new(MultifaceCfg {
                block_id,
                base_state,
                waterlogged_base,
                on,
                on_bits,
                dirs,
                search: jint(j, "search_range", 10),
                spread: jf32(j, "chance_of_spreading", 0.5),
            }))
        }
        "bamboo" => Cf::Bamboo(jf32(j, "probability", 0.0)),
        "vines" => Cf::Vines,
        "end_island" => Cf::EndIsland,
        "chorus_plant" => Cf::ChorusPlant,
        "glowstone_blob" => Cf::Glowstone,
        "basalt_pillar" => Cf::BasaltPillar,
        "void_start_platform" => Cf::VoidStartPlatform,
        "end_platform" => Cf::EndPlatform,
        "end_gateway" => Cf::EndGateway,
        "end_spike" => {
            let mut spikes = Vec::new();
            for s in jarr(j, "spikes")? {
                if !matches!(s, J::Obj(_)) {
                    return None;
                }
                spikes.push(EndSpike {
                    center_x: jint(s, "centerX", 0),
                    center_z: jint(s, "centerZ", 0),
                    radius: jint(s, "radius", 0),
                    height: jint(s, "height", 0),
                    guarded: jbool(s, "guarded", false),
                });
            }
            let crystal_beam_target = match js(j, "crystal_beam_target") {
                None => None,
                Some(v) => {
                    let [J::Num(x), J::Num(y), J::Num(z)] = v.as_arr()?.as_slice() else {
                        return None;
                    };
                    Some((*x as i32, *y as i32, *z as i32))
                }
            };
            Cf::EndSpike(EndSpikeCfg {
                crystal_invulnerable: jbool(j, "crystal_invulnerable", false),
                spikes,
                crystal_beam_target,
            })
        }
        "huge_brown_mushroom" => Cf::HugeBrownMushroom {
            cap: p_bsp(js(j, "cap_provider")?)?,
            stem: p_bsp(js(j, "stem_provider")?)?,
            radius: jint(j, "foliage_radius", 0),
        },
        "huge_red_mushroom" => Cf::HugeRedMushroom {
            cap: p_bsp(js(j, "cap_provider")?)?,
            stem: p_bsp(js(j, "stem_provider")?)?,
            radius: jint(j, "foliage_radius", 0),
        },
        "sculk_patch" => Cf::SculkPatch(SculkPatchCfg {
            charge_count: jint(j, "charge_count", 0),
            amount_per_charge: jint(j, "amount_per_charge", 0),
            spread_attempts: jint(j, "spread_attempts", 0),
            growth_rounds: jint(j, "growth_rounds", 0),
            spread_rounds: jint(j, "spread_rounds", 0),
            extra_rare_growths: p_ip(js(j, "extra_rare_growths")?)?,
            catalyst_chance: jf32(j, "catalyst_chance", 0.0),
        }),
        "basalt_columns" => Cf::BasaltColumns {
            height: p_ip(js(j, "height")?)?,
            reach: p_ip(js(j, "reach")?)?,
        },
        "huge_fungus" => Cf::HugeFungus(HugeFungusCfg {
            base: bs_state(js(j, "valid_base_block")?)?,
            stem: bs_state(js(j, "stem_state")?)?,
            hat: bs_state(js(j, "hat_state")?)?,
            decor: bs_state(js(j, "decor_state")?)?,
            replaceable: p_pred(js(j, "replaceable_blocks")?)?,
            planted: jbool(j, "planted", false),
        }),
        "coral_tree" => Cf::Coral(CoralShape::Tree),
        "coral_claw" => Cf::Coral(CoralShape::Claw),
        "coral_mushroom" => Cf::Coral(CoralShape::Mushroom),
        "pointed_dripstone" => Cf::PointedDripstone(PointedDripstoneCfg {
            taller: jf32(j, "chance_of_taller_dripstone", 0.2),
            directional: jf32(j, "chance_of_directional_spread", 0.7),
            radius2: jf32(j, "chance_of_spread_radius2", 0.5),
            radius3: jf32(j, "chance_of_spread_radius3", 0.5),
        }),
        "twisting_vines" => Cf::TwistingVines {
            width: jint(j, "spread_width", 0),
            height: jint(j, "spread_height", 0),
            max_height: jint(j, "max_height", 0),
        },
        "weeping_vines" => Cf::WeepingVines,
        "netherrack_replace_blobs" => Cf::NetherrackReplaceBlobs {
            target: {
                let (n, p) = bs_parts(js(j, "target")?)?;
                breg()?.state_of(&n, p)
            },
            state: {
                let (n, p) = bs_parts(js(j, "state")?)?;
                breg()?.state_of(&n, p)
            },
            radius: p_ip(js(j, "radius")?)?,
        },
        "delta_feature" => Cf::DeltaFeature {
            contents: {
                let (n, p) = bs_parts(js(j, "contents")?)?;
                breg()?.state_of(&n, p)
            },
            rim: {
                let (n, p) = bs_parts(js(j, "rim")?)?;
                breg()?.state_of(&n, p)
            },
            size: p_ip(js(j, "size")?)?,
            rim_size: p_ip(js(j, "rim_size")?)?,
        },
        "nether_forest_vegetation" => Cf::NetherForestVegetation {
            bsp: p_bsp(js(j, "state_provider")?)?,
            width: jint(j, "spread_width", 0),
            height: jint(j, "spread_height", 0),
        },
        "monster_room" => Cf::MonsterRoom,
        "bonus_chest" => Cf::BonusChest,
        "tree" => Cf::Tree(Box::new(crate::tree::p_tree(j)?)),
        "fallen_tree" => Cf::Fallen {
            trunk: p_bsp(js(j, "trunk_provider")?)?,
        },
        _ => Cf::Unknown(ty.to_string()),
    })
}

// ---------- feature registry + ordering ----------

type FeatureOrder = Vec<Vec<(u32, String)>>;

/// One pre-resolved entry of a preset's per-step feature order.
///
/// Built once in `load_freg` so `populate_body` never re-runs the
/// `biomes.iter().any(|b| bsteps[b][step].contains(name))` scan nor the
/// `placed.get(name)` hash lookup on a per-chunk basis. `gi`/`name` are the
/// values `sort_features` produced; `step_biome_mask` is the union of the bit
/// indices of every biome whose `bsteps[b][step]` lists `name`, so the
/// `BiomeFilter`-equivalent `present` test is a single integer AND with the
/// chunk's `present_mask`. `placed_idx` indexes `placed_vec` directly.
struct StepEntry {
    #[cfg(test)]
    #[cfg_attr(not(megu_vanilla_data), allow(dead_code))]
    gi: u32,
    #[cfg(test)]
    #[cfg_attr(not(megu_vanilla_data), allow(dead_code))]
    name: String,
    step_biome_mask: u128,
    ball_mask: u128,
    placed_idx: usize,
}

pub(crate) struct FReg {
    cfg: HashMap<String, usize>,
    cfg_vec: Vec<Cf>,
    /// Kept for legacy/test callers (and child placed features) that look a
    /// placed feature up by name. Shares its `Pf` values with `placed_vec`
    /// through `Arc`, so both views are the same object.
    placed: HashMap<String, Arc<Pf>>,
    /// Direct-index view over the same `Pf` values, used by `populate_body`'s
    /// hot loop. A `StepEntry.placed_idx` points straight into this vector.
    placed_vec: Vec<Arc<Pf>>,
    /// Compact bit index (`0..N`) for every biome name appearing in `bsteps`
    /// or `ball`. Vanilla 1.21.1 has ~64 biomes, comfortably within `u128`.
    biome_bits: HashMap<String, u8>,
    /// Precomputed `ball_biome_mask` per placed-feature name: the union of
    /// bit(biome) over all biomes whose flattened feature list contains it.
    ball_masks: HashMap<String, u128>,
    /// Biomes that can ever satisfy `cold_enough_to_snow` at `y <= 320`
    /// (`temperature_modifier == "frozen"` or `temperature < 0.50`).
    possibly_cold_mask: u128,
    /// False only if there were more than 128 distinct feature biomes; in
    /// that defensive case `populate_body` uses the original string path.
    bit_ok: bool,
    bsteps: HashMap<String, Vec<Vec<String>>>,
    ball: HashMap<String, HashSet<String>>,
    btemp: HashMap<String, f32>,
    // 1.21.1 `spawners`: biome -> MobCategory serialized name -> weighted entries.
    spawners: HashMap<String, HashMap<String, Vec<SpawnerData>>>,
    // 1.21.1 `spawn_costs`: biome -> entity id -> (energy_budget, charge).
    spawn_costs: HashMap<String, HashMap<String, SpawnCost>>,
    spawn_probability: HashMap<String, f32>,
    order: HashMap<NoisePreset, FeatureOrder>,
    /// Pre-resolved hot-path form of `order`, parallel to it element by element.
    step_entries: HashMap<NoisePreset, Vec<Vec<StepEntry>>>,
}

/// `MobSpawnSettings.SpawnerData`: type, min/max group size, Weight value.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct SpawnerData {
    pub kind: String,
    pub min_count: i32,
    pub max_count: i32,
    pub weight: i32,
}

/// `MobSpawnSettings.MobSpawnCost` (both values are f64 in the codec).
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct SpawnCost {
    pub energy_budget: f64,
    pub charge: f64,
}

/// Vanilla's DFS (obf `ayd.a`): post-order visit over the feature edges, neighbours in
/// (step, featureIndex) order. A cycle is a data error, so it stops rather than recursing forever.
fn dfs_step(
    v: (usize, u32),
    edges: &HashMap<(usize, u32), Vec<(usize, u32)>>,
    visited: &mut std::collections::HashSet<(usize, u32)>,
    in_progress: &mut std::collections::HashSet<(usize, u32)>,
    order: &mut Vec<(usize, u32)>,
) {
    if visited.contains(&v) || in_progress.contains(&v) {
        return;
    }
    in_progress.insert(v);
    if let Some(neigh) = edges.get(&v) {
        let mut sorted = neigh.clone();
        sorted.sort_by_key(|a| (a.0, a.1));
        for m in sorted {
            dfs_step(m, edges, visited, in_progress, order);
        }
    }
    in_progress.remove(&v);
    visited.insert(v);
    order.push(v);
}

fn spawners_obj(j: &J) -> Vec<(String, &Vec<J>)> {
    match j {
        J::Obj(m) => m
            .iter()
            .filter_map(|(k, v)| v.as_arr().map(|a| (k.clone(), a)))
            .collect(),
        _ => Vec::new(),
    }
}

fn spawn_costs_obj(j: &J) -> Vec<(&str, &J)> {
    match j {
        J::Obj(m) => m.iter().map(|(k, v)| (k.as_str(), v)).collect(),
        _ => Vec::new(),
    }
}

fn p_spawner_data(j: &J) -> Option<SpawnerData> {
    Some(SpawnerData {
        kind: ns_name(&jstr(j, "type")?),
        min_count: jint(j, "minCount", 0),
        max_count: jint(j, "maxCount", 0),
        weight: jint(j, "weight", 1),
    })
}

fn p_spawn_cost(j: &J) -> Option<SpawnCost> {
    Some(SpawnCost {
        energy_budget: jnum(j, "energy_budget")?,
        charge: jnum(j, "charge")?,
    })
}

fn resolve_fref(
    fref: &mut FRef,
    cfg_idx: &HashMap<String, usize>,
    placed_idx: &HashMap<String, usize>,
    ball_masks: &HashMap<String, u128>,
) {
    match fref {
        FRef::Name(n) => {
            if let Some(&idx) = cfg_idx.get(n.as_str()) {
                *fref = FRef::CfgIdx(idx);
            }
        }
        FRef::Placed(n) => {
            if let Some(&idx) = placed_idx.get(n.as_str()) {
                let mask = ball_masks.get(n.as_str()).copied().unwrap_or(0);
                *fref = FRef::PlacedIdx(idx, mask);
            }
        }
        FRef::Inline(cf) => resolve_cf(cf, cfg_idx, placed_idx, ball_masks),
        FRef::CfgIdx(_) | FRef::PlacedIdx(_, _) => {}
    }
}

fn resolve_pf(
    pf: &mut Pf,
    cfg_idx: &HashMap<String, usize>,
    placed_idx: &HashMap<String, usize>,
    ball_masks: &HashMap<String, u128>,
) {
    resolve_fref(&mut pf.feat, cfg_idx, placed_idx, ball_masks);
}

fn resolve_cf(
    cf: &mut Cf,
    cfg_idx: &HashMap<String, usize>,
    placed_idx: &HashMap<String, usize>,
    ball_masks: &HashMap<String, u128>,
) {
    match cf {
        Cf::RandomPatch { feature, .. } => resolve_pf(feature, cfg_idx, placed_idx, ball_masks),
        Cf::RandSel(list, default) => {
            for (pf, _) in list {
                resolve_pf(pf, cfg_idx, placed_idx, ball_masks);
            }
            if let Some(d) = default {
                resolve_pf(d, cfg_idx, placed_idx, ball_masks);
            }
        }
        Cf::SimpleRand(list) | Cf::Seq(list) => {
            for pf in list {
                resolve_pf(pf, cfg_idx, placed_idx, ball_masks);
            }
        }
        Cf::WeightedSel(list) => {
            for (pf, _) in list {
                resolve_pf(pf, cfg_idx, placed_idx, ball_masks);
            }
        }
        Cf::BoolSel(t, f) => {
            resolve_pf(t, cfg_idx, placed_idx, ball_masks);
            resolve_pf(f, cfg_idx, placed_idx, ball_masks);
        }
        Cf::VegPatch { vfeat, .. } => resolve_pf(vfeat, cfg_idx, placed_idx, ball_masks),
        Cf::RootSystem(rc) => resolve_pf(&mut rc.feature, cfg_idx, placed_idx, ball_masks),
        _ => {}
    }
}

/// Best-effort reason a `configured_feature` body refused to parse: an absent
/// `type` is a missing-field failure, anything else that reaches this point is
/// a body `p_cf` rejected (an unimplemented type resolves to `Cf::Unknown`,
/// never `None`).
fn cf_fail_reason(j: &J) -> String {
    match jstr(j, "type") {
        Some(ty) => format!("malformed configured_feature body (type {ty:?})"),
        None => "missing required field \"type\"".to_string(),
    }
}

/// Parse one `configured_feature` file into `reg`. A failure is recorded in
/// `rep` (one stderr line, queryable after the load) and returned so a strict
/// load can refuse instead of dropping the entry.
fn load_cf_entry(
    reg: &mut FReg,
    name: &str,
    b: &[u8],
    rep: &mut WorldgenParseReport,
) -> Result<(), WorldgenParseError> {
    let Some(j) = jparse(b) else {
        return Err(rep.fail("configured_feature", name, "malformed JSON"));
    };
    match p_cf(&j) {
        Some(cf) => {
            let idx = reg.cfg_vec.len();
            reg.cfg_vec.push(cf);
            reg.cfg.insert(name.to_string(), idx);
            Ok(())
        }
        None => {
            if env_flag!("CF_UNPARSED") {
                eprintln!("CFUNPARSED {name}");
            }
            Err(rep.fail("configured_feature", name, cf_fail_reason(&j)))
        }
    }
}

/// Best-effort reason a `placed_feature` body refused to parse: mirrors the
/// `p_pf` contract (a `feature` name or inline body, then the modifier chain).
fn pf_fail_reason(j: &J) -> String {
    let f = match js(j, "feature") {
        Some(f) => f,
        None => return "missing required field \"feature\"".to_string(),
    };
    match f {
        J::Str(_) => {}
        o @ J::Obj(_) => {
            if js(o, "feature").is_some() {
                return pf_fail_reason(o);
            }
            return cf_fail_reason(o);
        }
        _ => return "field \"feature\" is not a name or object".to_string(),
    }
    if let Some(ms) = jarr(j, "placement") {
        for m in ms {
            if p_mod(m).is_none() {
                return match jstr(m, "type") {
                    Some(ty) => format!("unsupported placement modifier {ty:?}"),
                    None => "placement modifier missing required field \"type\"".to_string(),
                };
            }
        }
    }
    "placed_feature did not parse".to_string()
}

/// Parse one `placed_feature` file into the staging list. Same contract as
/// [`load_cf_entry`]: failures are recorded and returned, never dropped.
fn load_pf_entry(
    raw_placed: &mut Vec<(String, Pf)>,
    placed_idx_map: &mut HashMap<String, usize>,
    name: String,
    b: &[u8],
    rep: &mut WorldgenParseReport,
) -> Result<(), WorldgenParseError> {
    let Some(j) = jparse(b) else {
        return Err(rep.fail("placed_feature", name, "malformed JSON"));
    };
    match p_pf(&j) {
        Some(mut pf) => {
            pf.name = name.clone();
            let idx = raw_placed.len();
            placed_idx_map.insert(name.clone(), idx);
            raw_placed.push((name, pf));
            Ok(())
        }
        None => Err(rep.fail("placed_feature", name, pf_fail_reason(&j))),
    }
}

fn load_freg() -> FReg {
    let mut rep = WorldgenParseReport::default();
    let reg = load_freg_report(
        &mut rep,
        strict_worldgen_parse(),
        &Gen::dp_files_any(&["configured_feature", "feature"]),
        &Gen::dp_files("placed_feature"),
        &Gen::dp_files("biome"),
    );
    publish_worldgen_parse_report(&rep);
    // Strict mode fails closed: a registry that dropped an entry reports the
    // refusal instead of loading a partial feature set.
    reg.unwrap_or_else(|e| panic!("worldgen parse error: {e}"))
}

fn load_freg_report(
    rep: &mut WorldgenParseReport,
    strict: bool,
    cf_files: &HashMap<String, Vec<u8>>,
    pf_files: &HashMap<String, Vec<u8>>,
    biome_files: &HashMap<String, Vec<u8>>,
) -> Result<FReg, WorldgenParseError> {
    let mut reg = FReg {
        cfg: HashMap::new(),
        cfg_vec: Vec::new(),
        placed: HashMap::new(),
        placed_vec: Vec::new(),
        biome_bits: HashMap::new(),
        ball_masks: HashMap::new(),
        possibly_cold_mask: 0,
        bit_ok: true,
        bsteps: HashMap::new(),
        ball: HashMap::new(),
        btemp: HashMap::new(),
        spawners: HashMap::new(),
        spawn_costs: HashMap::new(),
        spawn_probability: HashMap::new(),
        order: HashMap::new(),
        step_entries: HashMap::new(),
    };
    for (name, b) in cf_files {
        if let Err(e) = load_cf_entry(&mut reg, name, b, rep) {
            if strict {
                return Err(e);
            }
        }
    }
    let mut raw_placed: Vec<(String, Pf)> = Vec::new();
    let mut placed_idx_map: HashMap<String, usize> = HashMap::new();
    for (name, b) in pf_files {
        if let Err(e) = load_pf_entry(&mut raw_placed, &mut placed_idx_map, name.clone(), b, rep) {
            if strict {
                return Err(e);
            }
        }
    }
    let mut frozen_biomes: HashSet<String> = HashSet::new();
    for (name, b) in biome_files {
        if let Some(j) = jparse(b) {
            if let Some(t) = jnum(&j, "temperature") {
                reg.btemp.insert(name.clone(), t as f32);
            }
            if jstr(&j, "temperature_modifier").as_deref() == Some("frozen") {
                frozen_biomes.insert(name.clone());
            }
            let mut steps: Vec<Vec<String>> = vec![];
            if let Some(fs) = jarr(&j, "features") {
                for s in fs {
                    let mut step = vec![];
                    if let Some(arr) = s.as_arr() {
                        for v in arr {
                            step.push(ns_name(v.str()));
                        }
                    }
                    steps.push(step);
                }
            }
            // The JSON field is `creature_spawn_probability`; MobSpawnSettings
            // defaults it to 0.1 when omitted.
            let prob = jnum(&j, "creature_spawn_probability").unwrap_or(0.1);
            reg.spawn_probability.insert(name.clone(), prob as f32);
            if let Some(spawners) = js(&j, "spawners").and_then(|v| match v {
                J::Obj(_) => Some(v),
                _ => None,
            }) {
                let mut by_category = HashMap::new();
                for (cat, list) in spawners_obj(spawners) {
                    let mut entries = Vec::new();
                    for e in list {
                        if let Some(data) = p_spawner_data(e) {
                            entries.push(data);
                        }
                    }
                    by_category.insert(cat, entries);
                }
                reg.spawners.insert(name.clone(), by_category);
            }
            if let Some(costs) = js(&j, "spawn_costs").and_then(|v| match v {
                J::Obj(_) => Some(v),
                _ => None,
            }) {
                let mut by_entity = HashMap::new();
                for (kind, cost) in spawn_costs_obj(costs) {
                    if let Some(c) = p_spawn_cost(cost) {
                        by_entity.insert(ns_name(kind), c);
                    }
                }
                reg.spawn_costs.insert(name.clone(), by_entity);
            }
            reg.ball
                .insert(name.clone(), steps.iter().flatten().cloned().collect());
            reg.bsteps.insert(name.clone(), steps);
        }
    }
    // Compact biome -> bit index over every biome that declares features.
    // Sorted for a deterministic assignment; the only consumer is the integer
    // mask arithmetic, so the particular bit numbers carry no semantics.
    let mut names: Vec<&String> = reg.bsteps.keys().chain(reg.ball.keys()).collect();
    names.sort();
    names.dedup();
    let bit_ok = names.len() <= 128;
    let mut biome_bits = HashMap::with_capacity(names.len());
    if bit_ok {
        for (i, name) in names.into_iter().enumerate() {
            biome_bits.insert(name.clone(), i as u8);
        }
    }
    reg.biome_bits = biome_bits;
    reg.bit_ok = bit_ok;

    if bit_ok {
        // `ball_biome_mask` per placed-feature name: the union of bit(biome)
        // over every biome whose flattened `features` list contains it.
        // Kept as a registry-level map because `Pf` literals also live in
        // test modules that this change may not touch.
        let mut ball_masks: HashMap<String, u128> = HashMap::new();
        for (biome, feats) in reg.ball.iter() {
            let Some(&bi) = reg.biome_bits.get(biome) else {
                continue;
            };
            let bit = 1u128 << bi;
            for f in feats.iter() {
                *ball_masks.entry(f.clone()).or_insert(0) |= bit;
            }
        }
        reg.ball_masks = ball_masks;
        let mut cold_mask = 0u128;
        for (biome, &bi) in reg.biome_bits.iter() {
            let temp = reg.btemp.get(biome).copied().unwrap_or(0.0);
            if frozen_biomes.contains(biome) || temp < 0.50 {
                cold_mask |= 1u128 << bi;
            }
        }
        reg.possibly_cold_mask = cold_mask;
    }

    // Pre-resolve every `FRef::Name`/`FRef::Placed` into direct slice indices so
    // hot feature execution (RandomPatch, RandomSelector, etc.) avoids HashMap
    // lookups on every placement attempt.
    for cf in &mut reg.cfg_vec {
        resolve_cf(cf, &reg.cfg, &placed_idx_map, &reg.ball_masks);
    }
    for (_, pf) in &mut raw_placed {
        resolve_pf(pf, &reg.cfg, &placed_idx_map, &reg.ball_masks);
    }
    let mut placed_vec: Vec<Arc<Pf>> = Vec::with_capacity(raw_placed.len());
    for (name, pf) in raw_placed {
        let arc = Arc::new(pf);
        placed_vec.push(Arc::clone(&arc));
        reg.placed.insert(name, arc);
    }
    reg.placed_vec = placed_vec;
    let placed_idx: HashMap<&str, usize> = reg
        .placed_vec
        .iter()
        .enumerate()
        .map(|(i, pf)| (pf.name.as_str(), i))
        .collect();

    // A server rooted at a directory with no `datapacks/` tree at all — the
    // wire-test layout, where the working directory carries only `assets` and
    // `config_packets.bin` — loads zero biome files, so `bsteps` is empty and
    // every preset entry would trip the `expect` below on the tick thread.
    // That is an empty-registry bootstrap, not corrupt data: skip the order
    // build and leave `order`/`step_entries` empty. The fail-closed contract
    // is unchanged: once *any* generation settings loaded, a possible biome
    // without them still panics instead of silently ordering nothing.
    if reg.bsteps.is_empty() {
        return Ok(reg);
    }
    // The order source is a runtime read now, so a stripped checkout still
    // compiles: with a populated registry the pinned table is required, and a
    // missing or corrupt one is the same fail-closed panic as before.
    let order_json =
        crate::structure_transform::pinned_data_bytes("assets/feature_order_1211.json");
    if order_json.is_none() {
        eprintln!(
            "[spine] feature order table absent; deriving biome iteration from loaded biome feature lists"
        );
    }
    let sources = order_json.as_deref().and_then(jparse);
    for preset in [
        NoisePreset::Overworld,
        NoisePreset::Nether,
        NoisePreset::End,
    ] {
        let biomes: Vec<&str> = if let Some(source) = sources.as_ref() {
            source
                .get(preset.name())
                .and_then(|v| jarr(v, "biomes"))
                .expect("native possible biomes")
                .iter()
                .map(J::str)
                .collect()
        } else {
            // The exported table is an optimization/reproducibility fixture, not extra
            // information: vanilla computes the possible-biome list from the biome
            // source and then sorts those biomes' feature lists. Deriving the same input
            // from the loaded biome JSON keeps a stripped/generated asset checkout on
            // the real sorter instead of panicking or silently dropping features.
            let mut names: Vec<&str> = reg
                .bsteps
                .keys()
                .filter(|name| match preset {
                    NoisePreset::Nether => matches!(
                        name.as_str(),
                        "minecraft:nether_wastes"
                            | "minecraft:crimson_forest"
                            | "minecraft:warped_forest"
                            | "minecraft:soul_sand_valley"
                            | "minecraft:basalt_deltas"
                    ),
                    NoisePreset::End => matches!(
                        name.as_str(),
                        "minecraft:the_end"
                            | "minecraft:end_highlands"
                            | "minecraft:end_midlands"
                            | "minecraft:small_end_islands"
                            | "minecraft:end_barrens"
                    ),
                    NoisePreset::Overworld => !matches!(
                        name.as_str(),
                        "minecraft:nether_wastes"
                            | "minecraft:crimson_forest"
                            | "minecraft:warped_forest"
                            | "minecraft:soul_sand_valley"
                            | "minecraft:basalt_deltas"
                            | "minecraft:the_end"
                            | "minecraft:end_highlands"
                            | "minecraft:end_midlands"
                            | "minecraft:small_end_islands"
                            | "minecraft:end_barrens"
                    ),
                })
                .map(String::as_str)
                .collect();
            names.sort();
            names
        };
        let max_steps = biomes
            .iter()
            .map(|name| {
                reg.bsteps
                    .get(*name)
                    .expect("possible biome generation settings")
                    .len()
            })
            .max()
            .unwrap_or(0);
        let order = sort_features(&reg, &biomes, max_steps);
        // Pre-resolve each order slot's biome mask and placed-feature index so
        // `populate_body` does no hash lookup / string scan per chunk.
        let mut step_entries = Vec::with_capacity(order.len());
        for (step, list) in order.iter().enumerate() {
            let mut entries = Vec::with_capacity(list.len());
            for (gi, name) in list {
                let mut mask = 0u128;
                if bit_ok {
                    for (biome, steps) in reg.bsteps.iter() {
                        if steps.get(step).is_some_and(|l| l.iter().any(|f| f == name)) {
                            if let Some(&bi) = reg.biome_bits.get(biome) {
                                mask |= 1u128 << bi;
                            }
                        }
                    }
                }
                let _ = gi;
                entries.push(StepEntry {
                    #[cfg(test)]
                    gi: *gi,
                    #[cfg(test)]
                    name: name.clone(),
                    step_biome_mask: mask,
                    ball_mask: reg.ball_masks.get(name).copied().unwrap_or(0),
                    placed_idx: placed_idx.get(name.as_str()).copied().unwrap_or(usize::MAX),
                });
            }
            step_entries.push(entries);
        }
        reg.step_entries.insert(preset, step_entries);
        reg.order.insert(preset, order);
    }
    Ok(reg)
}

fn sort_features(reg: &FReg, biomes: &[&str], max_steps: usize) -> FeatureOrder {
    // Vanilla FeatureSorter.buildFeaturesPerStep (obf `deg.a`): a global first-seen index per
    // feature, cross-step edges over each biome's flattened feature list, a DFS post-order
    // reversed under a step-then-index comparator, then grouped by step. The old Kahn FIFO
    // order differed, which moved every placed feature's RNG offset (one extra tree at
    // chunk (0,0)) and every sky-spill column it touched.
    let mut fidx: HashMap<String, u32> = HashMap::new();
    let mut counter = 0u32;
    for b in biomes {
        let Some(steps) = reg.bsteps.get(*b) else {
            continue;
        };
        for list in steps {
            for f in list {
                if !fidx.contains_key(f) {
                    fidx.insert(f.clone(), counter);
                    counter += 1;
                }
            }
        }
    }
    // A node is (step, featureIndex); the name is kept alongside for the output.
    let mut name_of: HashMap<(usize, u32), String> = HashMap::new();
    let mut edges: HashMap<(usize, u32), Vec<(usize, u32)>> = HashMap::new();
    for b in biomes {
        let Some(steps) = reg.bsteps.get(*b) else {
            continue;
        };
        let mut flat: Vec<(usize, u32)> = vec![];
        for (i, list) in steps.iter().enumerate() {
            for f in list {
                if let Some(&fi) = fidx.get(f) {
                    name_of.entry((i, fi)).or_insert_with(|| f.clone());
                    flat.push((i, fi));
                }
            }
        }
        for w in flat.windows(2) {
            if w[0] != w[1] {
                edges.entry(w[0]).or_default().push(w[1]);
            }
        }
    }
    let mut order: Vec<(usize, u32)> = vec![];
    let mut visited: std::collections::HashSet<(usize, u32)> = std::collections::HashSet::new();
    let mut in_progress: std::collections::HashSet<(usize, u32)> = std::collections::HashSet::new();
    // Every feature is a node. The edge map alone omits a feature that is the
    // only entry in its step (the one-feature fixtures exposed this: a valid
    // `trees_birch_and_oak` step vanished before placement). Sort all nodes by
    // (step, first-seen index) and let the DFS visit both connected and
    // dependency-free nodes.
    let mut keys: Vec<(usize, u32)> = name_of.keys().copied().collect();
    keys.sort_by_key(|a| (a.0, a.1));
    for k in keys {
        dfs_step(k, &edges, &mut visited, &mut in_progress, &mut order);
    }
    order.reverse();
    let mut by_step: Vec<Vec<(u32, String)>> = vec![vec![]; max_steps];
    for (step, slot) in by_step.iter_mut().enumerate() {
        for (gi, k) in order.iter().filter(|k| k.0 == step).enumerate() {
            if let Some(name) = name_of.get(k) {
                slot.push((gi as u32, name.clone()));
            }
        }
    }
    by_step
}

static FREG: std::sync::OnceLock<FReg> = std::sync::OnceLock::new();
fn freg() -> &'static FReg {
    FREG.get_or_init(load_freg)
}

/// The pinned biome's weighted spawner list for one MobCategory (serialized name).
pub(crate) fn spawners_for(biome: &str, category: &str) -> Option<&'static [SpawnerData]> {
    freg().spawners.get(biome)?.get(category).map(Vec::as_slice)
}

/// The pinned biome's spawn cost for an entity id, if the data declares one.
pub(crate) fn spawn_cost_for(biome: &str, entity: &str) -> Option<SpawnCost> {
    freg().spawn_costs.get(biome)?.get(entity).copied()
}

/// Pick a weighted `monster` entry that this server can actually simulate.
///
/// This is deliberately narrower than `NaturalSpawner`: only the four hostile
/// types the runtime owns are candidates. The weights and the entry order are
/// the pinned data's, so the nightly spawn path no longer rotates through a
/// hard-coded type list irrespective of biome. Mob-specific spawn placement,
/// group size, difficulty and per-category caps remain the caller's contract.
#[cfg(test)]
#[cfg_attr(not(megu_vanilla_data), allow(dead_code))]
pub(crate) fn choose_supported_hostile(biome: &str, roll: u64) -> Option<&'static str> {
    const SUPPORTED: [&str; 4] = [
        "minecraft:zombie",
        "minecraft:skeleton",
        "minecraft:spider",
        "minecraft:creeper",
    ];
    let list = spawners_for(biome, "monster")?;
    let total: i64 = list
        .iter()
        .filter(|s| SUPPORTED.contains(&s.kind.as_str()) && s.weight > 0)
        .map(|s| s.weight as i64)
        .sum();
    if total <= 0 {
        return None;
    }
    let mut point = (roll % total as u64) as i64;
    for s in list {
        if !SUPPORTED.contains(&s.kind.as_str()) || s.weight <= 0 {
            continue;
        }
        if point < s.weight as i64 {
            return Some(s.kind.as_str());
        }
        point -= s.weight as i64;
    }
    None
}

#[cfg(test)]
mod empty_registry_tests {
    /// The wire-test harness runs `megu` from a scratch working directory
    /// that carries only `assets` and `config_packets.bin`, so `Gen::packs()`
    /// sees no `datapacks/` tree and every datapack inventory — biome files
    /// included — comes back empty. `load_freg` must still build an (empty)
    /// registry there: before the guard it panicked on the first
    /// `feature_order_1211.json` biome that `bsteps` could never contain, which
    /// is how the live server's tick thread died on the first natural-spawn
    /// pass (`spawners_for` → `freg()` → `expect("possible biome generation
    /// settings")`). The fail-closed arm is the opposite case — a registry
    /// that loaded *some* generation settings but not a listed possible biome
    /// — and stays pinned by the unchanged `expect` inside the loop.
    #[test]
    fn load_freg_without_datapacks_builds_empty_registry() {
        let _gen = crate::worldgen::lock_gen_for_tests();
        // Restore cwd even if `load_freg` panics, so the pre-fix failure does
        // not corrupt the next test's relative paths.
        struct Cwd(std::path::PathBuf);
        impl Drop for Cwd {
            fn drop(&mut self) {
                let _ = std::env::set_current_dir(&self.0);
            }
        }
        let scratch = std::env::temp_dir().join(format!("spine-freg-empty-{}", std::process::id()));
        let _ = std::fs::remove_dir(&scratch);
        std::fs::create_dir(&scratch).unwrap();
        let _cwd = Cwd(std::env::current_dir().unwrap());
        std::env::set_current_dir(&scratch).unwrap();
        let reg = super::load_freg();
        drop(_cwd);
        let _ = std::fs::remove_dir(&scratch);
        assert!(reg.bsteps.is_empty(), "fixture must hide every datapack");
        assert!(reg.order.is_empty());
        assert!(reg.step_entries.is_empty());
    }
}

fn foliage_noise() -> &'static OSimp {
    static N: std::sync::OnceLock<OSimp> = std::sync::OnceLock::new();
    N.get_or_init(|| OSimp::new_l(&mut Lcg::new(2345), &[0]))
}

// ---------- placed feature execution ----------

fn gen_placed_feature_by_name(
    name: &str,
    ctx: &mut FC,
    rng: &mut Xor,
    pos: (i32, i32, i32),
) -> bool {
    let fr = freg();
    let Some(pf) = fr.placed.get(name) else {
        return false;
    };
    gen_pf(pf, fr, ctx, rng, pos);
    true
}

/// `java.util.HashSet<BlockPos>` iteration order.
///
/// `VegetationPatchFeature` collects its ground-patch surface points into a `HashSet` and then
/// walks that set for the vegetation pass, so the per-point `vegetation_chance` draw is bound to
/// points in *hash-bucket* order, not insertion order. Getting this wrong keeps every moss
/// block in place but swaps which point receives which vegetation (`short_grass` vs
/// `moss_carpet` land on each other's cells).
///
/// A bucket is `spread(hashCode) & (capacity - 1)`, where `spread(h) = h ^ (h >>> 16)` and
/// `Vec3i.hashCode() = (y + z * 31) * 31 + x` (pinned from `kh.hashCode` bytecode). The table
/// starts at 16 and doubles while `size > capacity * 0.75`; `HashMap` appends a new node to the
/// end of its bin, so a bin keeps insertion order.
fn java_hashset_order(pts: &[(i32, i32, i32)]) -> Vec<(i32, i32, i32)> {
    let mut cap = 16usize;
    while pts.len() > cap - cap / 4 {
        cap *= 2;
    }
    let mut keyed: Vec<(usize, usize, (i32, i32, i32))> = pts
        .iter()
        .enumerate()
        .map(|(i, &p)| {
            let h =
                p.1.wrapping_add(p.2.wrapping_mul(31))
                    .wrapping_mul(31)
                    .wrapping_add(p.0);
            let spread = (h ^ ((h as u32) >> 16) as i32) as u32;
            ((spread as usize) & (cap - 1), i, p)
        })
        .collect();
    keyed.sort_unstable_by_key(|&(bucket, ins, _)| (bucket, ins));
    keyed.into_iter().map(|(_, _, p)| p).collect()
}

/// `VEG_COL_TRACE=cx,cz`: every candidate column of a vegetation patch whose origin is in that
/// chunk, with the edge draw it consumed and why it was refused. Column order and the edge draw
/// are what tie a patch's depth draws to positions; the native side has the same draws in
/// `sculk-trace.txt` windows (`SDRAW ...@dyn`), so the two sequences align draw by draw.
fn veg_col_trace(pos: (i32, i32, i32)) -> bool {
    static CACHE: std::sync::OnceLock<Option<(i32, i32)>> = std::sync::OnceLock::new();
    let want = *CACHE.get_or_init(|| {
        let raw = std::env::var("VEG_COL_TRACE").ok()?;
        let (x, z) = raw.split_once(',')?;
        Some((x.trim().parse().ok()?, z.trim().parse().ok()?))
    });
    want == Some((pos.0 >> 4, pos.2 >> 4))
}

// The `ball_biome_mask` of the `Pf` whose modifier chain is currently running.
// `gen_pf` resolves it once per feature (a single hash lookup) and pushes it;
// `PM::Biome` then reads the top of this stack, so each placement candidate
// needs only one integer AND. Nested placed features push their own mask.
fn gen_pf(pf: &Pf, fr: &'static FReg, ctx: &mut FC, rng: &mut Xor, pos: (i32, i32, i32)) {
    let mask = if pf.name.is_empty() {
        0
    } else {
        fr.ball_masks.get(&pf.name).copied().unwrap_or(0)
    };
    gen_pf_with_mask(pf, mask, fr, ctx, rng, pos);
}

#[inline]
fn gen_pf_with_mask(
    pf: &Pf,
    pf_mask: u128,
    fr: &'static FReg,
    ctx: &mut FC,
    rng: &mut Xor,
    pos: (i32, i32, i32),
) {
    // Modifiers are streams of positions, not passes over a list. Vanilla builds
    // `Stream.of(origin).flatMap(modifier::getPositions)` for each modifier in turn, and the
    // terminal operation places the feature as each position is pulled. So a position produced by
    // `count` goes through `in_square`, `height_range` and the feature before the next position is
    // produced, and every draw lands in that order on the shared RandomSource.
    //
    // Applying one modifier to all positions before the next modifier is not equivalent: with
    // `count: 2` this server drew x1,z1,x2,z2,y1,y2 while vanilla draws x1,z1,y1,x2,z2,y2. The
    // vein origins then differ for every multi-position feature - `ore_andesite_lower` in chunk
    // (0,0) put its veins at y=15 and y=18 instead of y=49 and y=18 - which is why no step index
    // reproduced vanilla's ore cells.
    apply_modifiers(pf.mods.as_slice(), pf, pf_mask, fr, ctx, rng, pos);
}

// Reusable scratch buffers for `PM::CountEveryLayer`, the only modifier that
// must materialize all positions before recursing into downstream stages.
thread_local! {
    static MOD_SCRATCH: std::cell::RefCell<Vec<Vec<(i32, i32, i32)>>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

#[inline]
fn fast_test_pred(
    pr: &Pr,
    reg: &BReg,
    ft: &FeatTables,
    ctx: &mut FC,
    x: i32,
    y: i32,
    z: i32,
) -> bool {
    match pr {
        Pr::True => true,
        Pr::InsideWB => !ctx.out_of_h(y),
        Pr::Blocks(names) if names.len() == 1 && names[0] == "minecraft:air" => {
            reg.block_id_of(ctx.get(x, y, z)) == Some(ft.air_bid)
        }
        Pr::Solid => reg.is_solid(ctx.get(x, y, z)),
        Pr::Replaceable => reg.is_replaceable(ctx.get(x, y, z)),
        Pr::All(v) => v.iter().all(|p| fast_test_pred(p, reg, ft, ctx, x, y, z)),
        Pr::Any(v) => v.iter().any(|p| fast_test_pred(p, reg, ft, ctx, x, y, z)),
        Pr::Not(p) => !fast_test_pred(p, reg, ft, ctx, x, y, z),
        Pr::Offset(p, off) => fast_test_pred(p, reg, ft, ctx, x + off[0], y + off[1], z + off[2]),
        _ => test_pred(pr, reg, ctx, x, y, z),
    }
}

fn apply_modifiers(
    mods: &[PM],
    pf: &Pf,
    pf_mask: u128,
    fr: &'static FReg,
    ctx: &mut FC,
    rng: &mut Xor,
    pos: (i32, i32, i32),
) {
    let Some((m, rest)) = mods.split_first() else {
        gen_cf_ref(&pf.feat, fr, ctx, rng, pos);
        return;
    };
    match m {
        PM::Count(ip) => {
            let n = ip.get(rng).max(0);
            for _ in 0..n {
                apply_modifiers(rest, pf, pf_mask, fr, ctx, rng, pos);
            }
        }
        PM::CountEveryLayer(ip) => {
            let mut out = MOD_SCRATCH
                .with(|p| p.borrow_mut().pop())
                .unwrap_or_default();
            out.clear();
            count_every_layer_positions(ip, ctx, rng, pos, &mut out);
            for &p in &out {
                apply_modifiers(rest, pf, pf_mask, fr, ctx, rng, p);
            }
            out.clear();
            MOD_SCRATCH.with(|p| {
                let mut pool = p.borrow_mut();
                if pool.len() < 16 {
                    pool.push(out);
                }
            });
        }
        PM::InSquare => {
            let (x, z) = (pos.0 + rng.next_bound(16), pos.2 + rng.next_bound(16));
            if let Some(spec) = env_str!("INSQ_TRACE") {
                if let Some((a, c)) = spec.split_once(',') {
                    if let (Ok(cx), Ok(cz)) = (a.parse::<i32>(), c.parse::<i32>()) {
                        if (x >> 4) == cx && (z >> 4) == cz {
                            let cf = CUR_FEAT.load(std::sync::atomic::Ordering::Relaxed);
                            eprintln!("INSQ {x} {z} feature={},{}", cf >> 32, cf & 0xFFFF);
                        }
                    }
                }
            }
            apply_modifiers(rest, pf, pf_mask, fr, ctx, rng, (x, pos.1, z));
        }
        PM::Offset(xz, ysp) => {
            let next = (
                pos.0 + xz.get(rng),
                pos.1 + ysp.get(rng),
                pos.2 + xz.get(rng),
            );
            apply_modifiers(rest, pf, pf_mask, fr, ctx, rng, next);
        }
        PM::HeightRange(hp) => {
            let next = (pos.0, hp.get(rng, ctx.g.min_y, ctx.g.height), pos.2);
            apply_modifiers(rest, pf, pf_mask, fr, ctx, rng, next);
        }
        PM::Heightmap(kind) => {
            let top = ctx.heightmap(*kind, pos.0, pos.2);
            if top > ctx.g.min_y {
                apply_modifiers(rest, pf, pf_mask, fr, ctx, rng, (pos.0, top, pos.2));
            }
        }
        PM::Fixed => apply_modifiers(rest, pf, pf_mask, fr, ctx, rng, pos),
        PM::Biome => {
            let trace_spec = env_str!("BIOMEF_TRACE");
            let fast = fr.bit_ok
                && test_biome_leaf(ctx).is_none()
                && LEAF_BITS.with(|c| c.borrow().active);
            let leaf = if fast {
                ctx.g
                    .biome_blended(pos.0, pos.1, pos.2)
                    .map(|(_, l)| l)
                    .or_else(|| {
                        ctx.g
                            .biome_quart(pos.0 >> 2, pos.1 >> 2, pos.2 >> 2, None)
                            .map(|(_, l)| l)
                    })
                    .filter(|l| usize::from(*l) < 128)
            } else {
                None
            };
            let mut ok = false;
            let mut bname: Option<String> = None;
            if let Some(leaf) = leaf {
                let bit = leaf_bit(ctx, fr, leaf);
                ok = bit.is_some_and(|b| (pf_mask & b) != 0);
                if trace_spec.is_some() {
                    bname = ctx.g.biome_name_of_leaf(leaf);
                }
            } else if let Some(b) = ctx.biome(pos.0, pos.1, pos.2) {
                ok = fr
                    .ball
                    .get(&b)
                    .map(|s| s.contains(&pf.name))
                    .unwrap_or(false);
                bname = Some(b);
            }
            if let Some(spec) = trace_spec {
                if let Some((a, c)) = spec.split_once(',') {
                    if let (Ok(cx), Ok(cz)) = (a.parse::<i32>(), c.parse::<i32>()) {
                        if (pos.0 >> 4) == cx && (pos.2 >> 4) == cz {
                            let cf = CUR_FEAT.load(std::sync::atomic::Ordering::Relaxed);
                            eprintln!(
                                "BIOMEF {} {} {} {} biome={} feature={},{}",
                                pos.0,
                                pos.1,
                                pos.2,
                                ok,
                                bname.as_deref().unwrap_or(""),
                                cf >> 32,
                                cf & 0xFFFF
                            );
                        }
                    }
                }
            }
            if ok {
                apply_modifiers(rest, pf, pf_mask, fr, ctx, rng, pos);
            }
        }
        PM::BlockPred(p) => {
            if let Some(reg) = breg() {
                let ft = feat_tables(reg);
                let ok = fast_test_pred(p, reg, ft, ctx, pos.0, pos.1, pos.2);
                if let Some(spec) = env_str!("BLOCKPRED_TRACE") {
                    if spec == "all" || spec == format!("{},{}", pos.0 >> 4, pos.2 >> 4) {
                        eprintln!(
                            "BLOCKPRED {} {} {} ok={} below={} at={} feature={},{}",
                            pos.0,
                            pos.1,
                            pos.2,
                            ok,
                            reg.name_of(ctx.get(pos.0, pos.1 - 1, pos.2)),
                            reg.name_of(ctx.get(pos.0, pos.1, pos.2)),
                            CUR_FEAT.load(std::sync::atomic::Ordering::Relaxed) >> 32,
                            CUR_FEAT.load(std::sync::atomic::Ordering::Relaxed) & 0xFFFF
                        );
                    }
                }
                if ok {
                    apply_modifiers(rest, pf, pf_mask, fr, ctx, rng, pos);
                }
            }
        }
        PM::Rarity(ch) => {
            if *ch > 0 && rng.next_f32() < 1.0 / *ch as f32 {
                apply_modifiers(rest, pf, pf_mask, fr, ctx, rng, pos);
            }
        }
        PM::SurfaceRel(kind, min_i, max_i) => {
            let y = i64::from(ctx.heightmap(*kind, pos.0, pos.2));
            let lo = y + i64::from(*min_i);
            let hi = y + i64::from(*max_i);
            if lo <= i64::from(pos.1) && i64::from(pos.1) <= hi {
                apply_modifiers(rest, pf, pf_mask, fr, ctx, rng, pos);
            }
        }
        PM::SurfWaterDepth(max) => {
            let wt = ctx.hm_ws(pos.0, pos.2);
            let oc = ctx.hm_oc(pos.0, pos.2);
            if let Some(spec) = env_str!("SURF_TRACE") {
                if spec == "all" || spec == format!("{},{}", pos.0 >> 4, pos.2 >> 4) {
                    eprintln!(
                        "SURF {} {} ws={} oc={} depth={} ok={}",
                        pos.0,
                        pos.2,
                        wt,
                        oc,
                        wt - oc,
                        wt - oc <= *max
                    );
                }
            }
            if wt - oc <= *max {
                apply_modifiers(rest, pf, pf_mask, fr, ctx, rng, pos);
            }
        }
        PM::NoiseCount(ratio, factor, offset) => {
            let n = foliage_noise().sample(pos.0 as f64 / *factor, pos.2 as f64 / *factor);
            let cnt = ((n + *offset) * *ratio as f64).ceil().max(0.0) as i32;
            for _ in 0..cnt {
                apply_modifiers(rest, pf, pf_mask, fr, ctx, rng, pos);
            }
        }
        PM::NoiseThrCount(level, below, above) => {
            let n = foliage_noise().sample(pos.0 as f64 / 200.0, pos.2 as f64 / 200.0);
            let cnt = if n < *level { *below } else { *above };
            for _ in 0..cnt.max(0) {
                apply_modifiers(rest, pf, pf_mask, fr, ctx, rng, pos);
            }
        }
        PM::RandomChance(c) => {
            if rng.next_f32() < *c {
                apply_modifiers(rest, pf, pf_mask, fr, ctx, rng, pos);
            }
        }
        PM::EnvScan(up, allowed, target, max_steps) => {
            if let Some(reg) = breg() {
                let ft = feat_tables(reg);
                let allowed_pred: &Pr = allowed.as_ref().unwrap_or(&Pr::True);
                if !fast_test_pred(allowed_pred, reg, ft, ctx, pos.0, pos.1, pos.2) {
                    return;
                }
                let (dx, dy, dz) = dir_off(*up);
                let mut p = pos;
                for _ in 0..*max_steps {
                    if fast_test_pred(target, reg, ft, ctx, p.0, p.1, p.2) {
                        apply_modifiers(rest, pf, pf_mask, fr, ctx, rng, p);
                        return;
                    }
                    p = (p.0 + dx, p.1 + dy, p.2 + dz);
                    if ctx.out_of_h(p.1) {
                        return;
                    }
                    if !fast_test_pred(allowed_pred, reg, ft, ctx, p.0, p.1, p.2) {
                        break;
                    }
                }
                if fast_test_pred(target, reg, ft, ctx, p.0, p.1, p.2) {
                    apply_modifiers(rest, pf, pf_mask, fr, ctx, rng, p);
                }
            }
        }
        PM::Carving => {}
    }
}

fn gen_cf_ref(fref: &FRef, fr: &'static FReg, ctx: &mut FC, rng: &mut Xor, pos: (i32, i32, i32)) {
    match fref {
        FRef::CfgIdx(idx) => gen_cf(&fr.cfg_vec[*idx], fr, ctx, rng, pos),
        FRef::PlacedIdx(idx, mask) => {
            gen_pf_with_mask(&fr.placed_vec[*idx], *mask, fr, ctx, rng, pos);
        }
        FRef::Name(n) => {
            match fr.cfg.get(n) {
                Some(&idx) => gen_cf(&fr.cfg_vec[idx], fr, ctx, rng, pos),
                None => {
                    // A configured feature that failed to parse is skipped
                    // silently, which looks exactly like "the feature chose not
                    // to place anything".
                    if env_flag!("CF_MISSING") {
                        eprintln!("CFMISSING {n}");
                    }
                }
            }
        }
        FRef::Placed(n) => match fr.placed.get(n) {
            // the child runs its own modifier chain on the same RNG stream
            Some(pf) => gen_pf(pf, fr, ctx, rng, pos),
            None => {
                if env_flag!("CF_MISSING") {
                    eprintln!("CFMISSING placed {n}");
                }
            }
        },
        FRef::Inline(cf) => gen_cf(cf, fr, ctx, rng, pos),
    }
}

/// Thread-local `leaf -> biome-bit` cache used by `PM::Biome` during the
/// dynamic extent of `populate_body`, where all feature placement on this
/// thread happens. Each distinct biome leaf is resolved at most once per
/// chunk (a handful of values in practice), after which the hot check is a
/// single array index plus an integer AND — no `String` allocation and no
/// hash lookup per placement candidate. Leaves >= 128 fall back to the
/// original string path. `active` is false outside `populate_body`, which
/// keeps direct callers (unit tests, structure-pipeline tests) on the exact
/// pre-change path.
struct LeafBitCache {
    active: bool,
    present_mask: u128,
    bits: [u128; 128],
    known: u128,
}
thread_local! {
    static LEAF_BITS: std::cell::RefCell<LeafBitCache> = const { std::cell::RefCell::new(LeafBitCache {
        active: false,
        present_mask: 0,
        bits: [0; 128],
        known: 0,
    }) };
}

/// Test-only override that makes `FC::biome` return a pinned leaf regardless
/// of position; the fast path must defer to the original path when it is set.
#[cfg(test)]
fn test_biome_leaf(ctx: &FC) -> Option<u16> {
    ctx.test_biome_leaf
}
#[cfg(not(test))]
fn test_biome_leaf(_ctx: &FC) -> Option<u16> {
    None
}

/// Resolve the bit for a biome leaf, caching it in `LEAF_BITS`. Returns
/// `None` only when the leaf cannot be resolved by the fast path (leaf index
/// out of range), in which case the caller uses the original string path.
fn leaf_bit(ctx: &mut FC, fr: &FReg, leaf: u16) -> Option<u128> {
    if usize::from(leaf) >= 128 {
        return None;
    }
    let l = usize::from(leaf);
    let flag = 1u128 << l;
    let (known, cached) = LEAF_BITS.with(|c| {
        let c = c.borrow();
        (c.known & flag != 0, c.bits[l])
    });
    if known {
        return Some(cached);
    }
    // At most one allocation / name lookup per distinct leaf per chunk.
    let bit = ctx
        .g
        .biome_name_of_leaf(leaf)
        .and_then(|name| fr.biome_bits.get(&name).copied())
        .map_or(0, |i| 1u128 << i);
    LEAF_BITS.with(|c| {
        let mut c = c.borrow_mut();
        c.bits[l] = bit;
        c.known |= flag;
    });
    Some(bit)
}

#[cfg(test)]
#[cfg_attr(not(megu_vanilla_data), allow(dead_code))]
fn mod_positions(
    m: &PM,
    fr: &'static FReg,
    ctx: &mut FC,
    rng: &mut Xor,
    pos: (i32, i32, i32),
    pf: &Pf,
    out: &mut Vec<(i32, i32, i32)>,
) {
    match m {
        PM::Count(ip) => {
            for _ in 0..ip.get(rng).max(0) {
                out.push(pos);
            }
        }
        PM::CountEveryLayer(ip) => count_every_layer_positions(ip, ctx, rng, pos, out),
        PM::InSquare => out.push((
            pos.0 + rng.next_bound(16),
            pos.1,
            pos.2 + rng.next_bound(16),
        )),
        PM::Offset(xz, ysp) => out.push((
            pos.0 + xz.get(rng),
            pos.1 + ysp.get(rng),
            pos.2 + xz.get(rng),
        )),
        PM::HeightRange(hp) => out.push((pos.0, hp.get(rng, ctx.g.min_y, ctx.g.height), pos.2)),
        PM::Heightmap(kind) => {
            let top = ctx.heightmap(*kind, pos.0, pos.2);
            if top > ctx.g.min_y {
                out.push((pos.0, top, pos.2));
            }
        }
        PM::Fixed => out.push(pos),
        PM::Biome => {
            if let Some(b) = ctx.biome(pos.0, pos.1, pos.2) {
                if fr
                    .ball
                    .get(&b)
                    .map(|s| s.contains(&pf.name))
                    .unwrap_or(false)
                {
                    out.push(pos);
                }
            }
        }
        PM::BlockPred(p) => {
            if let Some(reg) = breg() {
                if test_pred(p, reg, ctx, pos.0, pos.1, pos.2) {
                    out.push(pos);
                }
            }
        }
        PM::Rarity(ch) => {
            if *ch > 0 && rng.next_f32() < 1.0 / *ch as f32 {
                out.push(pos);
            }
        }
        PM::SurfaceRel(kind, min_i, max_i) => {
            let y = i64::from(ctx.heightmap(*kind, pos.0, pos.2));
            let lo = y + i64::from(*min_i);
            let hi = y + i64::from(*max_i);
            if lo <= i64::from(pos.1) && i64::from(pos.1) <= hi {
                out.push(pos);
            }
        }
        PM::SurfWaterDepth(max) => {
            let wt = ctx.hm_ws(pos.0, pos.2);
            let oc = ctx.hm_oc(pos.0, pos.2);
            if wt - oc <= *max {
                out.push(pos);
            }
        }
        PM::NoiseCount(ratio, factor, offset) => {
            let n = foliage_noise().sample(pos.0 as f64 / *factor, pos.2 as f64 / *factor);
            let cnt = ((n + *offset) * *ratio as f64).ceil().max(0.0) as i32;
            for _ in 0..cnt {
                out.push(pos);
            }
        }
        PM::NoiseThrCount(level, below, above) => {
            let n = foliage_noise().sample(pos.0 as f64 / 200.0, pos.2 as f64 / 200.0);
            let cnt = if n < *level { *below } else { *above };
            for _ in 0..cnt.max(0) {
                out.push(pos);
            }
        }
        PM::RandomChance(c) => {
            if rng.next_f32() < *c {
                out.push(pos);
            }
        }
        PM::EnvScan(up, allowed, target, max_steps) => {
            if let Some(reg) = breg() {
                let allowed_pred: &Pr = allowed.as_ref().unwrap_or(&Pr::True);
                if !test_pred(allowed_pred, reg, ctx, pos.0, pos.1, pos.2) {
                    return;
                }
                let (dx, dy, dz) = dir_off(*up);
                let mut p = pos;
                for _ in 0..*max_steps {
                    if test_pred(target, reg, ctx, p.0, p.1, p.2) {
                        out.push(p);
                        return;
                    }
                    p = (p.0 + dx, p.1 + dy, p.2 + dz);
                    if ctx.out_of_h(p.1) {
                        return;
                    }
                    if !test_pred(allowed_pred, reg, ctx, p.0, p.1, p.2) {
                        break;
                    }
                }
                if test_pred(target, reg, ctx, p.0, p.1, p.2) {
                    out.push(p);
                }
            }
        }
        PM::Carving => {}
    }
}

fn count_every_layer_positions(
    ip: &IP,
    ctx: &mut FC,
    rng: &mut Xor,
    pos: (i32, i32, i32),
    out: &mut Vec<(i32, i32, i32)>,
) {
    // `CountOnEveryLayerPlacement.getPositions` (`eim.a_(eiw,ayw,jd)`):
    //
    // ```java
    // int layer = 0; boolean found;
    // do {
    //    found = false;
    //    for (int j = 0; j < this.count.sample(random); j++) {   // per layer
    //       int x = random.nextInt(16) + pos.getX();
    //       int z = random.nextInt(16) + pos.getZ();
    //       int top = context.getHeight(MOTION_BLOCKING, x, z);
    //       int y = getY(context, x, top, z, layer);
    //       if (y != Integer.MAX_VALUE) { builder.add(...); found = true; }
    //    }
    //    layer++;
    // } while (found);
    // ```
    //
    // Three details each move every later draw (and every later layer):
    // the count is re-sampled *inside* the layer loop, the column height is
    // `MOTION_BLOCKING` (not the OCEAN_FLOOR every other modifier here reads),
    // and the layer scan uses `isEmpty() = isAir() || WATER || LAVA`.
    let Some(reg) = breg() else { return };
    let trace = cel_trace(pos);
    let mut layer = 0i32;
    loop {
        let n = ip.get(rng).max(0);
        let mut any = false;
        for _ in 0..n {
            let x = pos.0 + rng.next_bound(16);
            let z = pos.2 + rng.next_bound(16);
            let top = ctx.heightmap(HeightmapKind::MotionBlocking, x, z);
            let y = cel_find_y(ctx, reg, x, top, z, layer);
            if trace.is_some() {
                // Every layer the column offers, so a layer-count difference says which
                // boundary one side has and the other does not instead of only "one more".
                let all = cel_layer_ys(ctx, reg, x, top, z);
                eprintln!(
                    "CEL pass={},{} layer={layer} x={x} z={z} top={top} y={} layers={:?}",
                    pos.0 >> 4,
                    pos.2 >> 4,
                    y.map(|v| v.to_string()).unwrap_or_else(|| "-".into()),
                    all
                );
            }
            if cel_column_wanted(x, z) {
                cel_column_dump(ctx, x, z, top);
            }
            if let Some(y) = y {
                out.push((x, y, z));
                any = true;
            }
        }
        layer += 1;
        if !any {
            break;
        }
    }
}

/// `CEL_TRACE=cx,cz`: every `count_on_every_layer` sample whose *placement* origin is in that
/// chunk, with the layer it was asked for, the `MOTION_BLOCKING` top it read, the y it answered
/// and every layer the column offers. `eim.a(eiw,int,int,int,int)` walks a column from its
/// `MOTION_BLOCKING` top down, so "one more layer" is a difference in the *column*, not in the
/// sampling - the layer list says which boundary one side has and the other does not.
fn cel_trace(pos: (i32, i32, i32)) -> Option<()> {
    static CACHE: std::sync::OnceLock<Option<(i32, i32)>> = std::sync::OnceLock::new();
    let want = *CACHE.get_or_init(|| {
        let raw = std::env::var("CEL_TRACE").ok()?;
        let (x, z) = raw.split_once(',')?;
        Some((x.trim().parse().ok()?, z.trim().parse().ok()?))
    });
    if want == Some((pos.0 >> 4, pos.2 >> 4)) {
        Some(())
    } else {
        None
    }
}

/// `CEL_COLUMN_TRACE=x,z`: the live feature-facing state of one column, from the
/// `MOTION_BLOCKING` top down, one `CCELL` line per cell.
///
/// Why: `cel_layer_ys` says which boundaries the walk found, which localises a layer-count
/// difference to a column but not to a cell. The comparison that settles it is against a saved
/// column (or a `--mode carvers` dump), and a boundary list cannot say whether the walk saw a
/// different block or the same block one y lower. This prints the states the walk read, through the
/// same `FC::get`, so the answer is the read itself rather than a rederivation of it.
fn cel_column_wanted(x: i32, z: i32) -> bool {
    static CACHE: std::sync::OnceLock<Option<(i32, i32)>> = std::sync::OnceLock::new();
    let want = *CACHE.get_or_init(|| {
        let raw = std::env::var("CEL_COLUMN_TRACE").ok()?;
        let (a, b) = raw.split_once(',')?;
        Some((a.trim().parse().ok()?, b.trim().parse().ok()?))
    });
    want == Some((x, z))
}

fn cel_column_dump(ctx: &mut FC<'_>, x: i32, z: i32, top: i32) {
    let mut y = top;
    while y >= ctx.g.min_y {
        let (st, src) = ctx.get_src(x, y, z);
        eprintln!("CCELL {x} {y} {z} {st} {src}");
        y -= 1;
    }
}

/// `CountOnEveryLayerPlacement`'s `isEmpty()`: `isAir() || WATER || LAVA`. A fluid cell is an
/// *empty* layer, so the layer sits in the fluid rather than above it.
fn cel_empty(reg: &BReg, st: u32) -> bool {
    reg.is_air(st) || matches!(reg.name_of(st), "minecraft:water" | "minecraft:lava")
}

/// Every layer of a column in `CountOnEveryLayerPlacement.getY`'s sense: the empty cells whose
/// cell below is non-empty and not `BEDROCK`, from the `MOTION_BLOCKING` top down. `cel_find_y`
/// returns element `layer` of this list.
fn cel_layer_ys(ctx: &mut FC<'_>, reg: &BReg, x: i32, top: i32, z: i32) -> Vec<i32> {
    let min_y = ctx.g.min_y;
    let mut out = Vec::new();
    let mut cur = ctx.get(x, top, z);
    let mut y = top;
    while y > min_y {
        let below = ctx.get(x, y - 1, z);
        let boundary = !cel_empty(reg, below) && cel_empty(reg, cur);
        if boundary && reg.name_of(below) != "minecraft:bedrock" {
            out.push(y);
        }
        cur = below;
        y -= 1;
    }
    out
}

/// `CountOnEveryLayerPlacement.getY` (`eim.a(eiw,int,int,int,int)`): the
/// `layer`-th y from the column's `top` downward whose state is empty
/// (`isAir() || WATER || LAVA`) while the cell below it is not, skipping a
/// `BEDROCK` support. Vanilla returns `mutable.getY() + 1` after reading
/// `j - 1`, i.e. the empty cell itself, and `Integer.MAX_VALUE` (here `None`)
/// when the column has fewer than `layer + 1` layers.
fn cel_find_y(ctx: &mut FC<'_>, reg: &BReg, x: i32, top: i32, z: i32, layer: i32) -> Option<i32> {
    let min_y = ctx.g.min_y;
    let mut found = 0;
    // `getHeight` can return `minY + height` (a blocking block in the top layer);
    // `ctx.get` answers air there, exactly like `ChunkAccess.getBlockState` outside
    // the build height, so the first cell the loop reads is `top - 1`.
    let mut cur = ctx.get(x, top, z);
    let mut y = top;
    while y > min_y {
        let below = ctx.get(x, y - 1, z);
        if !cel_empty(reg, below)
            && cel_empty(reg, cur)
            && reg.name_of(below) != "minecraft:bedrock"
        {
            if found == layer {
                return Some(y);
            }
            found += 1;
        }
        cur = below;
        y -= 1;
    }
    None
}

// ---------- configured feature generation ----------

struct OreScratch {
    ds: Vec<f64>,
    bits: Vec<u64>,
}

thread_local! {
    static ORE_SCRATCH: std::cell::RefCell<OreScratch> = const { std::cell::RefCell::new(OreScratch {
        ds: Vec::new(),
        bits: Vec::new(),
    }) };
}

#[cfg(test)]
#[cfg_attr(not(megu_vanilla_data), allow(dead_code))]
fn ore_should_place(
    discard: f32,
    ctx: &mut FC,
    rt: &RT,
    pos: (i32, i32, i32),
    r: &mut Xor,
) -> bool {
    let st = ctx.get(pos.0, pos.1, pos.2);
    let Some(reg) = breg() else { return false };
    test_rt(rt, reg, st, pos.1, r) && ore_air_discard_ok(discard, reg, ctx, pos, r)
}

#[inline]
fn ore_air_discard_ok(
    discard: f32,
    reg: &BReg,
    ctx: &mut FC,
    pos: (i32, i32, i32),
    r: &mut Xor,
) -> bool {
    if discard <= 0.0 {
        return true;
    }
    // OreFeature.shouldSkipAirCheck returns false at chance=1: that means
    // inspect the neighbours, not reject the cell. Endpoints consume no RNG.
    if discard < 1.0 && r.next_f32() >= discard {
        return true;
    }
    for d in 0..6u8 {
        let (dx, dy, dz) = dir_off(d);
        if reg.is_air(ctx.get(pos.0 + dx, pos.1 + dy, pos.2 + dz)) {
            return false;
        }
    }
    true
}

#[expect(clippy::too_many_arguments)]
fn gen_vein_impl<T>(
    ctx: &mut FC,
    rng: &mut Xor,
    sx: f64,
    ex: f64,
    sz: f64,
    ez: f64,
    sy: f64,
    ey: f64,
    xb: i32,
    yb: i32,
    zb: i32,
    hs: i32,
    vs: i32,
    size: i32,
    discard: f32,
    targets: &[(T, u32)],
    trace: bool,
    mut matches_rt: impl FnMut(&T, &BReg, u32, i32, &mut Xor) -> bool,
) {
    let Some(reg) = breg() else { return };
    let j = size.max(1);
    let ds_len = (j * 4) as usize;
    let bsz = (hs.max(1) * vs.max(1) * hs.max(1)) as usize;
    let words = bsz.div_ceil(64);
    let (mut ds, mut bits) = ORE_SCRATCH.with(|s| {
        let mut s = s.borrow_mut();
        (std::mem::take(&mut s.ds), std::mem::take(&mut s.bits))
    });
    ds.resize(ds_len, 0.0);
    bits.resize(words, 0u64);
    bits.fill(0);
    for k in 0..j {
        let f = k as f32 / j as f32;
        let ki = k as usize * 4;
        ds[ki] = sx + (ex - sx) * f as f64;
        ds[ki + 1] = sy + (ey - sy) * f as f64;
        ds[ki + 2] = sz + (ez - sz) * f as f64;
        // The radius is the one place vanilla uses the table: `Mth.sin(float)` over
        // the f32 fraction, then double arithmetic around it.
        let hh = rng.next_f64() * j as f64 / 16.0;
        let s1 = crate::generation_math::mth_sin(std::f32::consts::PI * f) + 1.0f32;
        ds[ki + 3] = (s1 as f64 * hh + 1.0) / 2.0;
    }
    for k in 0..(j - 1) {
        let ki = k as usize * 4;
        if ds[ki + 3] <= 0.0 {
            continue;
        }
        for m in (k + 1)..j {
            let mi = m as usize * 4;
            if ds[mi + 3] <= 0.0 {
                continue;
            }
            let hv = ds[ki + 3] - ds[mi + 3];
            let dv = ds[ki] - ds[mi];
            let ev = ds[ki + 1] - ds[mi + 1];
            let gv = ds[ki + 2] - ds[mi + 2];
            if hv * hv > dv * dv + ev * ev + gv * gv {
                if hv > 0.0 {
                    ds[mi + 3] = -1.0;
                } else {
                    ds[ki + 3] = -1.0;
                }
            }
        }
    }
    let hs_vs = hs * vs;
    for m in 0..j {
        let mi = m as usize * 4;
        let d = ds[mi + 3];
        if d <= 0.0 {
            continue;
        }
        let cx = ds[mi];
        let cy = ds[mi + 1];
        let cz = ds[mi + 2];
        let x0 = ((cx - d).floor() as i32).max(xb);
        let y0 = ((cy - d).floor() as i32).max(yb);
        let z0 = ((cz - d).floor() as i32).max(zb);
        let x1 = ((cx + d).floor() as i32).max(x0);
        let y1 = ((cy + d).floor() as i32).max(y0);
        let z1 = ((cz + d).floor() as i32).max(z0);
        // The world window is the placement boundary (which can exceed the
        // noise-generator window in the Nether and End). Reject whole Y slices
        // before evaluating their ellipsoid coordinates.
        let y0 = y0.max(ctx.g.world_min_y);
        let y1 = y1.min(ctx.g.world_min_y + ctx.g.world_height - 1);
        if y0 > y1 {
            continue;
        }
        for xx in x0..=x1 {
            let ux = (xx as f64 + 0.5 - cx) / d;
            let ux2 = ux * ux;
            if ux2 >= 1.0 {
                continue;
            }
            for yy in y0..=y1 {
                let uy = (yy as f64 + 0.5 - cy) / d;
                let uxy2 = ux2 + uy * uy;
                if uxy2 >= 1.0 {
                    continue;
                }
                if ctx.out_of_h(yy) {
                    continue;
                }
                let base = (xx - xb) + (yy - yb) * hs;
                for zz in z0..=z1 {
                    let uz = (zz as f64 + 0.5 - cz) / d;
                    if uxy2 + uz * uz >= 1.0 {
                        continue;
                    }
                    let bi = (base + (zz - zb) * hs_vs) as usize;
                    if bi >= bsz {
                        continue;
                    }
                    let mask = 1u64 << (bi & 63);
                    let word = &mut bits[bi >> 6];
                    if (*word & mask) != 0 {
                        continue;
                    }
                    *word |= mask;
                    let cur = ctx.get(xx, yy, zz);
                    for (rt, st) in targets {
                        let ok = matches_rt(rt, reg, cur, yy, rng)
                            && ore_air_discard_ok(discard, reg, ctx, (xx, yy, zz), rng);
                        if trace {
                            eprintln!("OTEST {xx} {yy} {zz} state={cur} ok={ok}");
                        }
                        if ok {
                            ctx.set(xx, yy, zz, *st);
                            break;
                        }
                    }
                }
            }
        }
    }
    ORE_SCRATCH.with(|s| {
        let mut s = s.borrow_mut();
        s.ds = ds;
        s.bits = bits;
    });
}

/// (feature index << 32) | step of the placed feature currently being generated.
pub(crate) static CUR_FEAT: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(-1);

#[expect(clippy::too_many_arguments)]
fn gen_ore_impl<T>(
    ctx: &mut FC,
    rng: &mut Xor,
    pos: (i32, i32, i32),
    size: i32,
    discard: f32,
    targets: &[(T, u32)],
    scattered: bool,
    mut matches_rt: impl FnMut(&T, &BReg, u32, i32, &mut Xor) -> bool,
) {
    if let Some(spec) = env_str!("ORE_TRACE") {
        if let Some((a, b)) = spec.split_once(',') {
            if a.parse::<i32>() == Ok(pos.0 >> 4) && b.parse::<i32>() == Ok(pos.2 >> 4) {
                let cf = CUR_FEAT.load(std::sync::atomic::Ordering::Relaxed);
                eprintln!(
                    "ORE {} {} {} size={} feature={},{}",
                    pos.0,
                    pos.1,
                    pos.2,
                    size,
                    cf >> 32,
                    cf & 0xFFFF
                );
            }
        }
    }
    if scattered {
        let Some(reg) = breg() else { return };
        let count = rng.next_bound(size + 1);
        for i in 0..count {
            let md = i.min(7);
            let off = |rng: &mut Xor| {
                let f1 = rng.next_f32();
                let f2 = rng.next_f32();
                ((f1 - f2) * md as f32).round() as i32
            };
            let tp = (pos.0 + off(rng), pos.1 + off(rng), pos.2 + off(rng));
            if ctx.out_of_h(tp.1) {
                continue;
            }
            let cur = ctx.get(tp.0, tp.1, tp.2);
            for (rt, st) in targets {
                if matches_rt(rt, reg, cur, tp.1, rng)
                    && ore_air_discard_ok(discard, reg, ctx, tp, rng)
                {
                    ctx.set(tp.0, tp.1, tp.2, *st);
                    break;
                }
            }
        }
        return;
    }
    let f = rng.next_f32() * std::f32::consts::PI;
    let g = size as f32 / 8.0;
    let i = ((size as f32 / 16.0 * 2.0 + 1.0) / 2.0).ceil() as i32;
    // The centre uses `Math.sin((double)f)`, not the table: vanilla widens the
    // float angle and calls double-precision sin/cos here.
    let sin_f = (f as f64).sin();
    let cos_f = (f as f64).cos();
    let d = pos.0 as f64 + sin_f * g as f64;
    let e = pos.0 as f64 - sin_f * g as f64;
    let h = pos.2 as f64 + cos_f * g as f64;
    let j = pos.2 as f64 - cos_f * g as f64;
    let l = pos.1 as f64 + rng.next_bound(3) as f64 - 2.0;
    let mm = pos.1 as f64 + rng.next_bound(3) as f64 - 2.0;
    let n = pos.0 - g.ceil() as i32 - i;
    let o = pos.1 - 2 - i;
    let p = pos.2 - g.ceil() as i32 - i;
    let q = 2 * (g.ceil() as i32 + i);
    let r2 = 2 * (2 + i);
    let before = ctx.wr.len();
    for xp in n..=(n + q) {
        for zp in p..=(p + q) {
            // OreFeature.place asks for OCEAN_FLOOR_WG, not the live
            // OCEAN_FLOOR used by placement modifiers after neighbour features.
            if o <= ctx.hm_oc_wg(xp, zp) {
                let trace = env_str!("ORE_CELL_TRACE")
                    .is_some_and(|spec| spec == format!("{},{},{}", pos.0, pos.1, pos.2));
                gen_vein_impl(
                    ctx, rng, d, e, h, j, l, mm, n, o, p, q, r2, size, discard, targets, trace,
                    matches_rt,
                );
                if trace {
                    eprintln!(
                        "ORECELL origin={pos:?} size={size} wrote={} radius={:?}",
                        ctx.wr.len() - before,
                        (d, e, h, j, l, mm)
                    );
                }
                return;
            }
        }
    }
}

fn gen_ore(
    ctx: &mut FC,
    rng: &mut Xor,
    pos: (i32, i32, i32),
    size: i32,
    discard: f32,
    targets: &[(RT, u32)],
    scattered: bool,
) {
    gen_ore_impl(ctx, rng, pos, size, discard, targets, scattered, test_rt);
}

fn gen_ore_fast(ctx: &mut FC, rng: &mut Xor, pos: (i32, i32, i32), ore: &FastOre) {
    gen_ore_impl(
        ctx,
        rng,
        pos,
        ore.size,
        ore.discard,
        &ore.targets,
        ore.scattered,
        |rt, reg, st, y, r| rt.matches(reg, st, y, r),
    );
}

/// Test helper: the cells one vein writes for a seed and position, with a rule test that
/// always matches, so the geometry can be compared cell for cell with vanilla's own output
/// (see evidence/gameplay/feature-parity/ore_andesite_666.txt).
#[cfg(test)]
#[cfg_attr(not(megu_vanilla_data), allow(dead_code))]
pub(crate) fn ore_cells_for_test(
    seed: u64,
    pos: (i32, i32, i32),
    size: i32,
) -> Vec<(i32, i32, i32)> {
    let mut g = crate::worldgen::Gen::load(0, crate::name_to_state).expect("datapack");
    let mut ctx = FC::new(&mut g);
    let mut rng = Xor::seed(seed);
    let targets = [(RT::True, 0u32)];
    gen_ore(&mut ctx, &mut rng, pos, size, 0.0, &targets, false);
    let mut v: Vec<(i32, i32, i32)> = ctx.wr.keys().copied().collect();
    v.sort();
    v
}

/// Pins the ore geometry and the full feature-population capture to SHA-256 while
/// reporting the elapsed time per populated chunk for this exact fixture.
#[cfg(all(test, megu_vanilla_data))]
#[test]
fn perf_ore_parity_and_timing() {
    use std::time::{Duration, Instant};

    crate::test_support::require_vanilla_data();
    let _guard = crate::worldgen::lock_gen_for_tests();

    let mut g = Gen::load(0, crate::name_to_state).expect("datapack");
    let mut bytes = b"GEN-OUTPUT-1\0".to_vec();
    let mut population_time = Duration::ZERO;
    for (x, y, z) in [(-1, 64, -1), (0, 64, 0), (-17, 64, -1), (0, 80, 0)] {
        let start = Instant::now();
        let result = g.feat_state(x, y, z, populate);
        population_time += start.elapsed();
        for n in [x, y, z] {
            bytes.extend_from_slice(&n.to_le_bytes());
        }
        bytes.push(u8::from(result.is_some()));
        bytes.extend_from_slice(&result.unwrap_or(0).to_le_bytes());
        // Every successful FEATURES write belongs to the active chunk's 3x3
        // neighborhood. Include each bucket exactly once, including edge writes.
        let mut buckets = std::collections::BTreeSet::new();
        for &(cx, cz) in &g.pop_done {
            for dx in -1..=1 {
                for dz in -1..=1 {
                    buckets.insert((cx + dx, cz + dz));
                }
            }
        }
        let mut rows = Vec::new();
        for (cx, cz) in buckets {
            if let Some(map) = g.feat_map(cx, cz) {
                rows.extend(map.iter().map(|(&index, &state)| (cx, cz, index, state)));
            }
        }
        rows.sort_unstable();
        assert!(!rows.is_empty(), "fixture must exercise feature writes");
        bytes.extend_from_slice(&(rows.len() as u64).to_le_bytes());
        for (cx, cz, index, state) in rows {
            bytes.extend_from_slice(&cx.to_le_bytes());
            bytes.extend_from_slice(&cz.to_le_bytes());
            bytes.extend_from_slice(&index.to_le_bytes());
            bytes.extend_from_slice(&state.to_le_bytes());
        }
        let mut done: Vec<_> = g.pop_done.iter().copied().collect();
        done.sort_unstable();
        bytes.extend_from_slice(&(done.len() as u64).to_le_bytes());
        for (cx, cz) in done {
            bytes.extend_from_slice(&cx.to_le_bytes());
            bytes.extend_from_slice(&cz.to_le_bytes());
        }
    }
    let feature_hash = crate::generation_math::sha256(&bytes);
    let feature_digest = feature_hash
        .iter()
        .map(|v| format!("{v:02x}"))
        .collect::<String>();
    assert_eq!(
        feature_digest, "d63f366bd84594170be88b2391c4f39bdffc921578cba65b4a2ef114da6c2cfb",
        "feature population changed from the pre-optimization SHA-256 capture"
    );
    let chunks = g.pop_done.len();
    assert!(chunks > 0, "fixture must populate chunks");
    eprintln!(
        "perf_ore feature_population sha256={} chunks={} us/chunk={:.1}",
        feature_digest,
        chunks,
        population_time.as_secs_f64() * 1_000_000.0 / chunks as f64,
    );

    let ore_cells = ore_cells_for_test(60007, (7, 49, 13), 64);
    let mut ore_bytes = Vec::with_capacity(ore_cells.len() * 12);
    for &(x, y, z) in &ore_cells {
        for n in [x, y, z] {
            ore_bytes.extend_from_slice(&n.to_le_bytes());
        }
    }
    assert_eq!(ore_cells.len(), 666, "native ore fixture cell count");
    let ore_hash = crate::generation_math::sha256(&ore_bytes);
    let ore_digest = ore_hash
        .iter()
        .map(|v| format!("{v:02x}"))
        .collect::<String>();
    assert_eq!(
        ore_digest, "683a9109b6089e98b73ba4eafb268cd4b8e79af21f672d7d0dfd04fd2cc50f55",
        "native ore geometry changed from the pre-optimization SHA-256 capture"
    );
    eprintln!(
        "perf_ore ore_geometry sha256={} cells={}",
        ore_digest,
        ore_cells.len(),
    );
}

/// StructurePiece.reorient: horizontal order is NORTH, EAST, SOUTH, WEST;
/// face away from one opaque wall, otherwise turn away from blocked fronts.
fn orient_chest(ctx: &mut FC, pos: (i32, i32, i32), state: u32) -> u32 {
    let Some(reg) = breg() else { return state };
    let directions = [
        (0, -1, "north"),
        (1, 0, "east"),
        (0, 1, "south"),
        (-1, 0, "west"),
    ];
    let mut wall = None;
    for (i, &(dx, dz, _)) in directions.iter().enumerate() {
        let neighbour = ctx.get(pos.0 + dx, pos.1, pos.2 + dz);
        if reg.name_of(neighbour) == "minecraft:chest" {
            return state;
        }
        if reg.is_solid_render(neighbour) {
            if wall.is_some() {
                wall = None;
                break;
            }
            wall = Some(i);
        }
    }
    let face = if let Some(i) = wall {
        (i + 2) % 4
    } else {
        let facing = reg.get_prop(state, "facing").unwrap_or_default();
        let mut i = directions.iter().position(|d| d.2 == facing).unwrap_or(0);
        for turn in [2, 1, 2] {
            let (dx, dz, _) = directions[i];
            if reg.is_solid_render(ctx.get(pos.0 + dx, pos.1, pos.2 + dz)) {
                i = (i + turn) % 4;
            }
        }
        i
    };
    reg.with_prop(state, "facing", directions[face].2)
}

fn multiface_relative(p: (i32, i32, i32), d: u8) -> (i32, i32, i32) {
    let (x, y, z) = dir_off(d);
    (p.0 + x, p.1 + y, p.2 + z)
}
fn multiface_property(d: u8) -> &'static str {
    ["down", "up", "north", "south", "west", "east"][d as usize]
}
fn multiface_state(ctx: &mut FC, reg: &BReg, p: (i32, i32, i32), name: &str, d: u8) -> Option<u32> {
    let cur = ctx.get(p.0, p.1, p.2);
    if reg.name_of(cur) == name
        && reg.get_prop(cur, multiface_property(d)).as_deref() == Some("true")
    {
        return None;
    }
    let support = multiface_relative(p, d);
    if !reg.supports_multiface(ctx.get(support.0, support.1, support.2), d) {
        return None;
    }
    let base = if reg.name_of(cur) == name {
        cur
    } else {
        let st = reg.state_of(name, None);
        if reg.fluid_name(cur) == "minecraft:water" {
            reg.with_prop(st, "waterlogged", "true")
        } else {
            st
        }
    };
    Some(reg.with_prop(base, multiface_property(d), "true"))
}
fn multiface_state_cfg(
    ctx: &mut FC,
    reg: &BReg,
    p: (i32, i32, i32),
    cfg: &MultifaceCfg,
    d: u8,
) -> Option<u32> {
    let cur = ctx.get(p.0, p.1, p.2);
    let is_self = reg.block_id_of(cur) == Some(cfg.block_id);
    if is_self && reg.get_prop(cur, multiface_property(d)).as_deref() == Some("true") {
        return None;
    }
    let support = multiface_relative(p, d);
    if !reg.supports_multiface(ctx.get(support.0, support.1, support.2), d) {
        return None;
    }
    let base = if is_self {
        cur
    } else if reg.fluid_name(cur) == "minecraft:water" {
        cfg.waterlogged_base
    } else {
        cfg.base_state
    };
    Some(reg.with_prop(base, multiface_property(d), "true"))
}
fn multiface_spread(
    ctx: &mut FC,
    reg: &BReg,
    rng: &mut Xor,
    p: (i32, i32, i32),
    cfg: &MultifaceCfg,
    state: u32,
    from: u8,
) {
    let mut dirs = [0, 1, 2, 3, 4, 5];
    feature_util_shuffle(&mut dirs, rng);
    for to in dirs {
        if to / 2 == from / 2
            || reg.get_prop(state, multiface_property(to)).as_deref() == Some("true")
        {
            continue;
        }
        let next = multiface_relative(p, to);
        // MultifaceSpreader's SAME_POSITION, SAME_PLANE, WRAP_AROUND order.
        for (target, face) in [
            (p, to),
            (next, from),
            (multiface_relative(next, from), to ^ 1),
        ] {
            let cur = ctx.get(target.0, target.1, target.2);
            if !(reg.is_air(cur)
                || reg.block_id_of(cur) == Some(cfg.block_id)
                || (is_water_st(reg, cur) && reg.fluid_name(cur) == "minecraft:water"))
            {
                continue;
            }
            if let Some(st) = multiface_state_cfg(ctx, reg, target, cfg, face) {
                if ctx.out_of_h(target.1) {
                    break;
                }
                ctx.set(target.0, target.1, target.2, st);
                // `MultifaceSpreader$SpreadConfig.spread(..., mark=true)`: the bonus spread the
                // growth feature runs marks the cell it spreads into (`dkz$b`, site 7). The
                // sculk vein spreader passes `!spreadOnly` (no mark) and does not come here.
                ctx.g.note_mark_source(
                    target.0,
                    target.1,
                    target.2,
                    crate::worldgen::MarkSrc::Multiface,
                );
                ctx.mark_shape(target.0, target.1, target.2);
                return;
            }
        }
    }
}
fn multiface_try(
    ctx: &mut FC,
    reg: &BReg,
    rng: &mut Xor,
    p: (i32, i32, i32),
    cfg: &MultifaceCfg,
    dirs: &[u8],
) -> bool {
    for &d in dirs {
        let support = multiface_relative(p, d);
        let s_st = ctx.get(support.0, support.1, support.2);
        let ok = if !cfg.on_bits.overflow {
            cfg.on_bits.contains_state(reg, s_st)
        } else {
            cfg.on.contains(reg.name_of(s_st))
        };
        if !ok {
            continue;
        }
        let Some(st) = multiface_state_cfg(ctx, reg, p, cfg, d) else {
            return false;
        };
        if !ctx.out_of_h(p.1) {
            ctx.set(p.0, p.1, p.2, st);
            // `MultifaceGrowthFeature.a(...)`: `level.setBlock(pos, state, 3)` then
            // `level.getChunk(pos).markPosForPostprocessing(pos)` — the mark is filed even when
            // `setBlock` changed nothing, and `ProtoChunk.e` drops it outside the build height
            // (the guard above).
            ctx.g
                .note_mark_source(p.0, p.1, p.2, crate::worldgen::MarkSrc::Multiface);
            ctx.mark_shape(p.0, p.1, p.2);
        }
        // The float draw happens even for zero/one spreading probabilities.
        if rng.next_f32() < cfg.spread {
            multiface_spread(ctx, reg, rng, p, cfg, st, d);
        }
        return true;
    }
    false
}

fn chorus_state(ctx: &mut FC, pos: (i32, i32, i32)) -> u32 {
    let reg = breg().unwrap();
    let mut state = reg.state_of("minecraft:chorus_plant", None);
    for (dx, dy, dz, property) in [
        (0, -1, 0, "down"),
        (0, 1, 0, "up"),
        (0, 0, -1, "north"),
        (0, 0, 1, "south"),
        (-1, 0, 0, "west"),
        (1, 0, 0, "east"),
    ] {
        let name = reg.name_of(ctx.get(pos.0 + dx, pos.1 + dy, pos.2 + dz));
        let connected = name == "minecraft:chorus_plant"
            || name == "minecraft:chorus_flower"
            || dy == -1 && name == "minecraft:end_stone";
        state = reg.with_prop(state, property, if connected { "true" } else { "false" });
    }
    state
}

fn chorus_clear(ctx: &mut FC, pos: (i32, i32, i32), except: Option<(i32, i32)>) -> bool {
    [(0, -1), (1, 0), (0, 1), (-1, 0)]
        .into_iter()
        .all(|(dx, dz)| except == Some((dx, dz)) || ctx.is_air(pos.0 + dx, pos.1, pos.2 + dz))
}

/// ChorusFlowerBlock.generatePlant's bounded recursive branch growth. The
/// adjacent stem is refreshed after each write, as flags=2 does not notify it.
fn grow_chorus(
    ctx: &mut FC,
    rng: &mut Xor,
    pos: (i32, i32, i32),
    origin: (i32, i32, i32),
    depth: i32,
) {
    let height = rng.next_bound(4) + 1 + i32::from(depth == 0);
    for dy in 1..=height {
        let next = (pos.0, pos.1 + dy, pos.2);
        if !chorus_clear(ctx, next, None) {
            return;
        }
        let state = chorus_state(ctx, next);
        ctx.set(next.0, next.1, next.2, state);
        let below = (next.0, next.1 - 1, next.2);
        let state = chorus_state(ctx, below);
        ctx.set(below.0, below.1, below.2, state);
    }
    let mut branched = false;
    if depth < 4 {
        let branches = rng.next_bound(4) + i32::from(depth == 0);
        for _ in 0..branches {
            let (dx, dz) = [(0, -1), (1, 0), (0, 1), (-1, 0)][rng.next_bound(4) as usize];
            let next = (pos.0 + dx, pos.1 + height, pos.2 + dz);
            if (next.0 - origin.0).abs() < 8
                && (next.2 - origin.2).abs() < 8
                && ctx.is_air(next.0, next.1, next.2)
                && ctx.is_air(next.0, next.1 - 1, next.2)
                && chorus_clear(ctx, next, Some((-dx, -dz)))
            {
                branched = true;
                let state = chorus_state(ctx, next);
                ctx.set(next.0, next.1, next.2, state);
                let parent = (next.0 - dx, next.1, next.2 - dz);
                let state = chorus_state(ctx, parent);
                ctx.set(parent.0, parent.1, parent.2, state);
                grow_chorus(ctx, rng, next, origin, depth + 1);
            }
        }
    }
    if !branched {
        let reg = breg().unwrap();
        let flower = reg.with_prop(reg.state_of("minecraft:chorus_flower", None), "age", "5");
        ctx.set(pos.0, pos.1 + height, pos.2, flower);
    }
}

fn snow_can_survive(reg: &BReg, below: u32) -> bool {
    let ft = feat_tables(reg);
    let cannot = if !ft.snow_cannot_survive.overflow {
        ft.snow_cannot_survive.contains_state(reg, below)
    } else {
        reg.in_tag(below, "minecraft:snow_layer_cannot_survive_on")
    };
    if cannot {
        return false;
    }
    let can = if !ft.snow_can_survive.overflow {
        ft.snow_can_survive.contains_state(reg, below)
    } else {
        reg.in_tag(below, "minecraft:snow_layer_can_survive_on")
    };
    if can {
        return true;
    }
    reg.supports_multiface(below, 0) || below == ft.snow_layer_8
}

fn gen_cf(cf: &Cf, fr: &'static FReg, ctx: &mut FC, rng: &mut Xor, pos: (i32, i32, i32)) {
    match cf {
        Cf::Unknown(_) => {}
        Cf::Fossil(config) => {
            config.place(ctx, rng, pos);
        }
        Cf::Glowstone => {
            let Some(reg) = breg() else { return };
            if !ctx.is_air(pos.0, pos.1, pos.2) {
                return;
            }
            let above = reg.name_of(ctx.get(pos.0, pos.1 + 1, pos.2));
            if !matches!(
                above,
                "minecraft:netherrack" | "minecraft:basalt" | "minecraft:blackstone"
            ) {
                return;
            }
            let state = reg.state_of("minecraft:glowstone", None);
            ctx.set(pos.0, pos.1, pos.2, state);
            for _ in 0..1500 {
                let x = pos.0 + rng.next_bound(8) - rng.next_bound(8);
                let y = pos.1 - rng.next_bound(12);
                let z = pos.2 + rng.next_bound(8) - rng.next_bound(8);
                if !ctx.is_air(x, y, z) {
                    continue;
                }
                let mut adjacent = 0;
                for (dx, dy, dz) in [
                    (0, -1, 0),
                    (0, 1, 0),
                    (0, 0, -1),
                    (0, 0, 1),
                    (-1, 0, 0),
                    (1, 0, 0),
                ] {
                    if ctx.get(x + dx, y + dy, z + dz) == state {
                        adjacent += 1;
                    }
                    if adjacent > 1 {
                        break;
                    }
                }
                if adjacent == 1 {
                    ctx.set(x, y, z, state);
                }
            }
        }
        Cf::BasaltPillar => gen_basalt_pillar(ctx, rng, pos),
        Cf::BonusChest => gen_bonus_chest(ctx, rng, pos),
        Cf::VoidStartPlatform => gen_void_start_platform(ctx, pos),
        Cf::EndPlatform => gen_end_platform(ctx, pos),
        Cf::EndGateway => gen_end_gateway(ctx, pos),
        Cf::EndSpike(c) => gen_end_spike(ctx, rng, pos, c),
        Cf::HugeBrownMushroom { cap, stem, radius } => {
            gen_huge_mushroom(ctx, rng, pos, cap, stem, *radius, HugeMushroom::Brown)
        }
        Cf::HugeRedMushroom { cap, stem, radius } => {
            gen_huge_mushroom(ctx, rng, pos, cap, stem, *radius, HugeMushroom::Red)
        }
        Cf::PointedDripstone(c) => gen_pointed_dripstone(ctx, rng, pos, c),
        Cf::Coral(shape) => gen_coral(ctx, rng, pos, *shape),
        Cf::HugeFungus(c) => gen_huge_fungus(ctx, rng, pos, c),
        Cf::BasaltColumns { height, reach } => gen_basalt_columns(ctx, rng, pos, height, reach),
        Cf::SculkPatch(c) => gen_sculk_patch(ctx, rng, pos, c),
        Cf::TwistingVines {
            width,
            height,
            max_height,
        } => gen_twisting_vines(ctx, rng, pos, *width, *height, *max_height),
        Cf::WeepingVines => gen_weeping_vines(ctx, rng, pos),
        Cf::NetherrackReplaceBlobs {
            target,
            state,
            radius,
        } => gen_netherrack_replace_blobs(ctx, rng, pos, *target, *state, radius),
        Cf::DeltaFeature {
            contents,
            rim,
            size,
            rim_size,
        } => gen_delta_feature(ctx, rng, pos, *contents, *rim, size, rim_size),
        Cf::NetherForestVegetation { bsp, width, height } => {
            gen_nether_forest_vegetation(ctx, rng, pos, bsp, *width, *height)
        }
        Cf::ChorusPlant => {
            let Some(reg) = breg() else { return };
            if ctx.is_air(pos.0, pos.1, pos.2)
                && reg.name_of(ctx.get(pos.0, pos.1 - 1, pos.2)) == "minecraft:end_stone"
            {
                let state = chorus_state(ctx, pos);
                ctx.set(pos.0, pos.1, pos.2, state);
                grow_chorus(ctx, rng, pos, pos, 0);
            }
        }
        Cf::EndIsland => {
            let Some(reg) = breg() else { return };
            let state = reg.state_of("minecraft:end_stone", None);
            let mut radius = rng.next_bound(3) as f32 + 4.0;
            let mut dy = 0;
            while radius > 0.5 {
                for dx in (-radius).floor() as i32..=radius.ceil() as i32 {
                    for dz in (-radius).floor() as i32..=radius.ceil() as i32 {
                        if (dx * dx + dz * dz) as f32 <= (radius + 1.0) * (radius + 1.0) {
                            ctx.set(pos.0 + dx, pos.1 + dy, pos.2 + dz, state);
                        }
                    }
                }
                radius -= rng.next_bound(2) as f32 + 0.5;
                dy -= 1;
            }
        }
        Cf::Iceberg { state } => gen_iceberg(ctx, rng, pos, *state),
        Cf::DripstoneCluster(cfg) => gen_dripstone_cluster(ctx, rng, pos, cfg),
        Cf::LargeDripstone(cfg) => gen_large_dripstone(ctx, rng, pos, cfg),
        Cf::DesertWell => gen_desert_well(ctx, rng, pos),
        Cf::RootSystem(c) => gen_root_system(ctx, rng, pos, c),
        Cf::IceSpike => gen_ice_spike(ctx, rng, pos),
        Cf::SeaPickle { count } => {
            let Some(reg) = breg() else { return };
            let water = reg.state_of("minecraft:water", None);
            let n = count.get(rng);
            for _ in 0..n.max(0) {
                let dx = rng.next_bound(8) - rng.next_bound(8);
                let dz = rng.next_bound(8) - rng.next_bound(8);
                let (x, z) = (pos.0 + dx, pos.2 + dz);
                let y = ctx.hm_oc(x, z);
                // the pickles draw comes before both survival tests, so a rejected
                // cell still moves the stream
                let pickles = rng.next_bound(4) + 1;
                if ctx.out_of_h(y) || y <= ctx.g.min_y {
                    continue;
                }
                if ctx.get(x, y, z) != water {
                    continue;
                }
                let below = ctx.get(x, y - 1, z);
                if !(reg.side_solid(below, 1) || reg.name_of(below) == "minecraft:magma_block") {
                    continue;
                }
                // `SeaPickleBlock`'s default state is pickles=1 waterlogged=true, and
                // the feature only sets pickles
                let props = J::Obj(vec![
                    ("pickles".to_string(), J::Str(pickles.to_string())),
                    ("waterlogged".to_string(), J::Str("true".to_string())),
                ]);
                let st = reg.state_of("minecraft:sea_pickle", Some(&props));
                ctx.set(x, y, z, st);
            }
        }
        Cf::Geode(g) => gen_geode(ctx, rng, pos, g),
        Cf::Tree(t) => {
            if let Some(spec) = env_str!("TREE_TRACE") {
                // same one-chunk radius as the agent's -Dtreechunk
                if let Some((a, b)) = spec.split_once(',') {
                    if let (Ok(cx), Ok(cz)) = (a.parse::<i32>(), b.parse::<i32>()) {
                        let (dx, dz) = (pos.0 >> 4, pos.2 >> 4);
                        if (cx - dx).abs() <= 1 && (cz - dz).abs() <= 1 {
                            let cf = CUR_FEAT.load(std::sync::atomic::Ordering::Relaxed);
                            eprintln!(
                                "TREE {} {} {} feature={},{}",
                                pos.0,
                                pos.1,
                                pos.2,
                                cf >> 32,
                                cf & 0xFFFF
                            );
                        }
                    }
                }
            }
            crate::tree::gen_tree(ctx, rng, pos, t);
        }
        Cf::Ore {
            size,
            discard,
            targets,
            scattered,
        } => {
            gen_ore(ctx, rng, pos, *size, *discard, targets, *scattered);
        }
        Cf::OreFast(ore) => {
            gen_ore_fast(ctx, rng, pos, ore);
        }
        Cf::Disk {
            half,
            radius,
            bsp,
            target,
        } => {
            let Some(reg) = breg() else { return };
            let ft = feat_tables(reg);
            let disk_trace = env_flag!("DISK_TRACE");
            let top = pos.1 + half;
            let bottom = pos.1 - half - 1;
            let rr = radius.get(rng);
            // `BlockPos.betweenClosed(pos.offset(-r, 0, -r), pos.offset(r, 0, r))`: x is the
            // fastest axis and z the slowest, so z is the **outer** loop. The column test
            // (`config.target().test`) runs before the state provider in `placeColumn`, and
            // the provider draws, so a column's draws belong to it in this order only - the
            // x-outer walk bound the wrong draw stream to every column of a sand/gravel/clay
            // disk.
            for cz in -rr..=rr {
                for cx in -rr..=rr {
                    if cx * cx + cz * cz > rr * rr {
                        continue;
                    }
                    let (x, z) = (pos.0 + cx, pos.2 + cz);
                    let mut y = top;
                    while y > bottom {
                        if disk_trace && cx == 0 && cz == 0 {
                            let cf = CUR_FEAT.load(std::sync::atomic::Ordering::Relaxed);
                            eprintln!(
                                "DISK {x} {y} {z} block={} target={} feature={},{}",
                                reg.name_of(ctx.get(x, y, z)),
                                test_pred(target, reg, ctx, x, y, z),
                                cf >> 32,
                                cf & 0xFFFF
                            );
                        }
                        if !ctx.out_of_h(y) && fast_test_pred(target, reg, ft, ctx, x, y, z) {
                            let st = bsp.get(ctx, rng, x, y, z);
                            ctx.set(x, y, z, st);
                            mark_above_for_post_processing(ctx, reg, x, y, z);
                        }
                        y -= 1;
                    }
                }
            }
        }
        Cf::SimpleBlock { to, on, in_, under } => {
            if let Some(reg) = breg() {
                if !on.is_empty()
                    && !on
                        .iter()
                        .any(|t| *t == reg.name_of(ctx.get(pos.0, pos.1 - 1, pos.2)))
                {
                    return;
                }
                if !in_.is_empty()
                    && !in_
                        .iter()
                        .any(|t| *t == reg.name_of(ctx.get(pos.0, pos.1, pos.2)))
                {
                    return;
                }
                if !under.is_empty()
                    && !under
                        .iter()
                        .any(|t| *t == reg.name_of(ctx.get(pos.0, pos.1 + 1, pos.2)))
                {
                    return;
                }
            }
            let st = to.get(ctx, rng, pos.0, pos.1, pos.2);
            // `SimpleBlockFeature` (edl) places only where the state `canSurvive`
            // (`edl.a`: `if ($$4.a($$2, $$3)) ... else return false`). Without it a
            // patch places its block on any air cell inside the box - measured:
            // short_grass 362 cells against vanilla's 102, with the draw streams
            // matching exactly, so the outer placement and the patch spreads were
            // already right and only this check was missing.
            if let Some(reg) = breg() {
                let ok = can_place(reg, ctx, reg.name_of(st), pos.0, pos.1, pos.2);
                if env_flag!("SIMPLE_TRACE") {
                    static N: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);
                    if N.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < 100_000 {
                        eprintln!(
                            "SIMPLE {} {} {} state={} below={} at={} ok={ok}",
                            pos.0,
                            pos.1,
                            pos.2,
                            reg.name_of(st),
                            reg.name_of(ctx.get(pos.0, pos.1 - 1, pos.2)),
                            reg.name_of(ctx.get(pos.0, pos.1, pos.2))
                        );
                    }
                }
                if !ok {
                    return;
                }
                // SimpleBlockFeature delegates DoublePlantBlock placement:
                // reject an occupied head before either write, force both
                // halves, and copy waterlogging at each destination.
                if reg.is_double_plant(st) {
                    if !ctx.is_air(pos.0, pos.1 + 1, pos.2) {
                        return;
                    }
                    for (dy, half) in [(0, "lower"), (1, "upper")] {
                        let y = pos.1 + dy;
                        let mut state = reg.with_prop(st, "half", half);
                        if reg.get_prop(state, "waterlogged").is_some() {
                            let wet = reg.has_water(ctx.get(pos.0, y, pos.2));
                            state = reg.with_prop(
                                state,
                                "waterlogged",
                                if wet { "true" } else { "false" },
                            );
                        }
                        if !ctx.out_of_h(y) {
                            ctx.set(pos.0, y, pos.2, state);
                        }
                    }
                    return;
                }
            }
            ctx.set(pos.0, pos.1, pos.2, st);
        }
        Cf::Spring {
            fluid,
            fluid_id,
            req_below,
            rock,
            hole,
            valid,
            valid_bits,
        } => {
            // `SpringFeature` (`edp`): `pos.above()` must be valid, and with
            // `requires_block_below` `pos.below()` too. The names are easy to
            // invert: `jd.d()` is above (Direction.UP) and `jd.e()` is below.
            let Some(reg) = breg() else { return };
            let is_valid = |st: u32| -> bool {
                if !valid_bits.overflow {
                    valid_bits.contains_state(reg, st)
                } else {
                    valid.contains(reg.name_of(st))
                }
            };
            if !is_valid(ctx.get(pos.0, pos.1 + 1, pos.2)) {
                return;
            }
            if *req_below && !is_valid(ctx.get(pos.0, pos.1 - 1, pos.2)) {
                return;
            }
            let at = ctx.get(pos.0, pos.1, pos.2);
            if !reg.is_air(at) && !is_valid(at) {
                return;
            }
            let mut rocks = 0;
            let mut air = 0;
            // vanilla counts the four horizontals and *below* (`jd.e()`)
            for d in [4u8, 5, 2, 3, 0] {
                let (dx, dy, dz) = dir_off(d);
                let s = ctx.get(pos.0 + dx, pos.1 + dy, pos.2 + dz);
                if is_valid(s) {
                    rocks += 1;
                }
                if reg.is_air(s) {
                    air += 1;
                }
            }
            if env_flag!("SPRING_TRACE") {
                static SHOWN: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);
                if SHOWN.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < 6 {
                    eprintln!(
                        "SPRING {} {} {} at={} below={} below2={} rocks={rocks} holes={air} want={rock},{hole}",
                        pos.0,
                        pos.1,
                        pos.2,
                        reg.name_of(at),
                        reg.name_of(ctx.get(pos.0, pos.1 - 1, pos.2)),
                        reg.name_of(ctx.get(pos.0, pos.1 - 2, pos.2)),
                    );
                }
            }
            if rocks == *rock && air == *hole {
                ctx.set(pos.0, pos.1, pos.2, *fluid);
                if *fluid_id < 0 {
                    eprintln!(
                        "[worldgen] unsupported spring fluid tick {} at {pos:?}",
                        reg.fluid_name(*fluid)
                    );
                    return;
                }
                ctx.g.schedule_generated_fluid(pos, *fluid_id as u32);
            }
        }
        Cf::Seagrass { prob } => {
            let Some(reg) = breg() else { return };
            let ft = feat_tables(reg);
            // dx, dz (4 draws), the heightmap read (none), the water test (none),
            // then the tall/short draw - that order is what the streams compare
            let (x, z) = (
                pos.0 + rng.next_bound(8) - rng.next_bound(8),
                pos.2 + rng.next_bound(8) - rng.next_bound(8),
            );
            let y = ctx.hm_oc(x, z);
            let trace_read = |y, st| {
                if env_flag!("SEAGRASS_TRACE")
                    && DTRACE.load(std::sync::atomic::Ordering::Relaxed) >= 0
                {
                    eprintln!("SEAREAD {x} {y} {z} state={st}");
                }
            };
            let at = ctx.get(x, y, z);
            trace_read(y, at);
            if reg.block_id_of(at) != Some(ft.water_bid) {
                return;
            }
            let tall = rng.next_f64() < f64::from(*prob);
            let below = ctx.get(x, y - 1, z);
            // `SeagrassBlock.mayPlaceOn`: sturdy top face, and not magma
            if !reg.side_solid(below, 1) || reg.block_id_of(below) == Some(ft.magma_bid) {
                return;
            }
            if tall {
                let above = ctx.get(x, y + 1, z);
                trace_read(y + 1, above);
                if reg.block_id_of(above) != Some(ft.water_bid) {
                    return;
                }
                ctx.set(x, y, z, ft.tall_seagrass_lo);
                ctx.set(x, y + 1, z, ft.tall_seagrass_hi);
            } else {
                ctx.set(x, y, z, ft.seagrass);
            }
        }
        Cf::Kelp => {
            let Some(reg) = breg() else { return };
            let ft = feat_tables(reg);
            let base_y = ctx.hm_oc(pos.0, pos.2);
            let mut p = (pos.0, base_y, pos.2);
            let trace = env_flag!("KELP_TRACE");
            if !is_water_st(reg, ctx.get(p.0, p.1, p.2)) {
                if trace {
                    eprintln!("KELP x={} z={} y0={base_y} no-water-at-base", pos.0, pos.2);
                }
                return;
            }
            let max_height = 1 + rng.next_bound(10);
            for i in 0..=max_height {
                let here_water = is_water_st(reg, ctx.get(p.0, p.1, p.2));
                let above_water = is_water_st(reg, ctx.get(p.0, p.1 + 1, p.2));
                let plant_survives = can_place(reg, ctx, "minecraft:kelp_plant", p.0, p.1, p.2);
                if here_water && above_water && plant_survives {
                    if i == max_height {
                        let age_off = rng.next_bound(4) as usize;
                        ctx.set(p.0, p.1, p.2, ft.kelp_ages[age_off]);
                    } else {
                        ctx.set(p.0, p.1, p.2, ft.kelp_plant);
                    }
                    if trace && i == max_height {
                        eprintln!(
                            "KELP x={} z={} y0={base_y} maxh={max_height} stem={max_height} full-height head_y={}",
                            p.0,
                            p.2,
                            base_y + max_height
                        );
                    }
                } else if i > 0 {
                    if trace {
                        eprintln!(
                            "KELP x={} z={} y0={base_y} maxh={max_height} stem={i} stopped_at={} here_water={here_water} above_water={above_water} survives={plant_survives}",
                            p.0, p.2, p.1
                        );
                    }
                    let base = (p.0, p.1 - 1, p.2);
                    let below_is_head =
                        reg.block_id_of(ctx.get(base.0, base.1 - 1, base.2)) == Some(ft.kelp_bid);
                    if can_place(reg, ctx, "minecraft:kelp", base.0, base.1, base.2)
                        && !below_is_head
                    {
                        let age_off = rng.next_bound(4) as usize;
                        ctx.set(base.0, base.1, base.2, ft.kelp_ages[age_off]);
                    }
                    break;
                }
                // `KelpFeature.place` moves up unconditionally: `pos.move(UP)` is
                // outside its if/else. Advancing only on a placement makes the
                // rejected-at-i=0 case write its head one cell lower than vanilla.
                p.1 += 1;
            }
        }
        Cf::RandomPatch {
            feature,
            tries,
            xz,
            y,
        } => {
            if env_str!("PATCH_TRACE")
                .is_some_and(|want| want == format!("{},{}", pos.0 >> 4, pos.2 >> 4))
            {
                let f = CUR_FEAT.load(std::sync::atomic::Ordering::Relaxed);
                eprintln!(
                    "PATCH {} {} {} feature={},{}",
                    pos.0,
                    pos.1,
                    pos.2,
                    f >> 32,
                    f & 0xFFFF
                );
            }
            let (xs, ys) = (xz + 1, y + 1);
            for _ in 0..*tries {
                // the arguments are evaluated left to right, so the draws go
                // xz, xz, y, y, xz, xz - a different grouping shifts every position
                let dx = rng.next_bound(xs) - rng.next_bound(xs);
                let dy = rng.next_bound(ys) - rng.next_bound(ys);
                let dz = rng.next_bound(xs) - rng.next_bound(xs);
                gen_pf(feature, fr, ctx, rng, (pos.0 + dx, pos.1 + dy, pos.2 + dz));
            }
        }
        Cf::RandSel(list, default) => {
            for (pf, chance) in list {
                if rng.next_f32() < *chance {
                    gen_pf(pf, fr, ctx, rng, pos);
                    return;
                }
            }
            if let Some(d) = default {
                gen_pf(d, fr, ctx, rng, pos);
            }
        }
        Cf::SimpleRand(list) => {
            if list.is_empty() {
                return;
            }
            let i = rng.next_bound(list.len() as i32) as usize;
            gen_pf(&list[i], fr, ctx, rng, pos);
        }
        Cf::WeightedSel(list) => {
            let total: i32 = list.iter().map(|e| e.1.max(0)).sum();
            if total <= 0 {
                return;
            }
            let mut roll = rng.next_bound(total);
            for (pf, w) in list {
                roll -= (*w).max(0);
                if roll < 0 {
                    gen_pf(pf, fr, ctx, rng, pos);
                    return;
                }
            }
        }
        Cf::BoolSel(t, f) => {
            if rng.next_bool() {
                gen_pf(t, fr, ctx, rng, pos);
            } else {
                gen_pf(f, fr, ctx, rng, pos);
            }
        }
        Cf::Seq(list) => {
            for pf in list {
                gen_pf(pf, fr, ctx, rng, pos);
            }
        }
        Cf::VegPatch {
            depth,
            extra_bottom,
            extra_edge,
            replaceable,
            ground,
            floor,
            chance,
            vfeat,
            xz,
            vrange,
            waterlog,
        } => {
            let Some(reg) = breg() else { return };
            let xr = xz.get(rng) + 1;
            let zr = xz.get(rng) + 1;
            if veg_col_trace(pos) {
                let gi = CUR_FEAT.load(std::sync::atomic::Ordering::Relaxed);
                eprintln!(
                    "VPATCH origin={} {} {} xr={xr} zr={zr} floor={floor} feature={},{}",
                    pos.0,
                    pos.1,
                    pos.2,
                    gi >> 32,
                    gi & 0xFFFF
                );
            }
            let inward: i32 = if *floor { -1 } else { 1 };
            let mut surface_pts: Vec<(i32, i32, i32)> = vec![];
            for dx in -xr..=xr {
                for dz in -zr..=zr {
                    let xe = dx == -xr || dx == xr;
                    let ze = dz == -zr || dz == zr;
                    if xe && ze {
                        continue;
                    }
                    // `if (!corner && (!sideEdge || (extra_edge != 0 && !(nextFloat() > extra_edge))))`
                    // enters; the draw exists only for a side edge with a non-zero chance.
                    let side_edge = xe || ze;
                    let edge_draw = if side_edge && *extra_edge != 0.0 {
                        Some(rng.next_f32())
                    } else {
                        None
                    };
                    if side_edge && edge_draw.is_none_or(|d| d > *extra_edge) {
                        if veg_col_trace(pos) {
                            eprintln!("VCOL dx={dx} dz={dz} edge={edge_draw:?} SKIP edge");
                        }
                        continue;
                    }
                    let mut p = (pos.0 + dx, pos.1, pos.2 + dz);
                    for _ in 0..*vrange {
                        if !ctx.is_air(p.0, p.1, p.2) {
                            break;
                        }
                        p.1 += inward;
                    }
                    for _ in 0..*vrange {
                        if ctx.is_air(p.0, p.1, p.2) {
                            break;
                        }
                        p.1 -= inward;
                    }
                    let below = (p.0, p.1 + inward, p.2);
                    let bs = ctx.get(below.0, below.1, below.2);
                    let sdir = if *floor { 1usize } else { 0 }; // face opposing inward walk
                    if !(ctx.is_air(p.0, p.1, p.2) && reg.side_solid(bs, sdir)) {
                        if veg_col_trace(pos) {
                            eprintln!(
                                "VCOL dx={dx} dz={dz} edge={edge_draw:?} SKIP surface air={} sturdy={} y={}",
                                ctx.is_air(p.0, p.1, p.2),
                                reg.side_solid(bs, sdir),
                                p.1
                            );
                        }
                        continue;
                    }
                    let mut dep = depth.get(rng);
                    if *extra_bottom > 0.0 && rng.next_f32() < *extra_bottom {
                        dep += 1;
                    }
                    if veg_col_trace(pos) {
                        eprintln!(
                            "VCOL dx={dx} dz={dz} edge={edge_draw:?} ACCEPT y={} dep={dep}",
                            p.1
                        );
                    }
                    let mut bp = below;
                    // `VegetationPatchFeature.placeGround` (`edt.a(dds,efh,Predicate,ayw,jd.a,int)`):
                    //
                    //    for (int i = 0; i < depth; i++) {
                    //       newState = groundState.getState(random, mut);
                    //       existing = level.getBlockState(mut);
                    //       if (newState.is(existing.getBlock())) continue;   // no move either
                    //       if (!replaceable.test(existing)) return i != 0;
                    //       level.setBlock(mut, newState, 2);
                    //       mut.move(inward);
                    //    }
                    //    return true;
                    //
                    // The returned flag decides whether the caller adds the column to the
                    // vegetation set, so a column whose ground is *already* the patch's block
                    // still contributes a surface point and still gets a vegetation roll. The
                    // port gated on "wrote at least one block" instead, which lost those
                    // columns: measured on chunk (5,-32), moss_patch stopped one roll short of
                    // native (draw 124: native `nextFloat`, port the next attempt's `nextInt(16)`) and its
                    // whole vegetation sequence shifted.
                    let mut add_point = true;
                    for i in 0..dep {
                        let cur = ctx.get(bp.0, bp.1, bp.2);
                        let mut st = ground.get(ctx, rng, bp.0, bp.1, bp.2);
                        if *waterlog && is_water_st(reg, cur) {
                            st = reg.with_prop(st, "waterlogged", "true");
                        }
                        if reg.name_of(st) == reg.name_of(cur) {
                            continue;
                        }
                        if !reg.in_tag(cur, replaceable) {
                            add_point = i != 0;
                            break;
                        }
                        ctx.set(bp.0, bp.1, bp.2, st);
                        bp.1 += inward;
                    }
                    if add_point {
                        surface_pts.push(below);
                    }
                }
            }
            if let Some(spec) = env_str!("VEG_TRACE") {
                if let Some((a, c)) = spec.split_once(',') {
                    if let (Ok(cx), Ok(cz)) = (a.parse::<i32>(), c.parse::<i32>()) {
                        if (pos.0 >> 4) == cx && (pos.2 >> 4) == cz {
                            let cf = CUR_FEAT.load(std::sync::atomic::Ordering::Relaxed);
                            eprintln!(
                                "VEG origin={} {} {} xr={xr} zr={zr} pts={} feature={},{}",
                                pos.0,
                                pos.1,
                                pos.2,
                                surface_pts.len(),
                                cf >> 32,
                                cf & 0xFFFF
                            );
                        }
                    }
                }
            }
            let water_st = reg.state_of("minecraft:water", None);
            // `WaterloggedVegetationPatchFeature.placeGroundPatch` splits the surface set
            // into *enclosed* points and drops the rest: a point survives only when none of
            // its NORTH/EAST/SOUTH/WEST/**DOWN** neighbours is face-sturdy towards it (the
            // decompiled `isExposed` tests `ji.c`, `ji.f`, `ji.d`, `ji.e`, `ji.a`, and `ji.a`
            // is DOWN - proven by `LakeFeature`'s `pos.c(4)`, which resolves to `below(4)`).
            // The surviving points are filled with water first, and only then does the
            // vegetation pass visit them, so a later pool sees the earlier water.
            let mut surface_pts = java_hashset_order(&surface_pts);
            if *waterlog {
                let mut enclosed = Vec::with_capacity(surface_pts.len());
                for sp in surface_pts.drain(..) {
                    // `isExposedDirection`: `!state.isFaceSturdy(level, pos.relative(dir),
                    // dir.getOpposite())`, so the face index is the neighbour's face
                    // pointing back at `sp`.
                    let mut exposed = false;
                    for (dx, dy, dz) in [
                        (0, 0, -1), // NORTH -> face SOUTH (3)
                        (1, 0, 0),  // EAST  -> face WEST  (4)
                        (0, 0, 1),  // SOUTH -> face NORTH (2)
                        (-1, 0, 0), // WEST  -> face EAST  (5)
                        (0, -1, 0), // DOWN  -> face UP    (1)
                    ] {
                        let s = ctx.get(sp.0 + dx, sp.1 + dy, sp.2 + dz);
                        let face = match (dx, dz) {
                            (0, -1) => 3usize,
                            (1, 0) => 4,
                            (0, 1) => 2,
                            (-1, 0) => 5,
                            _ => 1,
                        };
                        if !reg.side_solid(s, face) {
                            exposed = true;
                            break;
                        }
                    }
                    if !exposed {
                        enclosed.push(sp);
                    }
                }
                // vanilla fills every surviving point before the vegetation pass, and the
                // fill changes the neighbour test of the next pool, so the two loops stay
                // separate.
                for sp in &enclosed {
                    ctx.set(sp.0, sp.1, sp.2, water_st);
                }
                surface_pts = java_hashset_order(&enclosed);
            }
            for sp in surface_pts {
                if *chance > 0.0 && rng.next_f32() < *chance {
                    // `VegetationPatchFeature.placeVegetation` places at
                    // `pos.relative(surface.getDirection().getOpposite())`, and the
                    // waterlogged subclass hands it `pos.below()`, so the waterlogged
                    // position is `pos.below().relative(opposite)` = `pos - inward` for a
                    // floor patch and two blocks below the ceiling for a ceiling patch.
                    let pp = if *waterlog {
                        (sp.0, sp.1 - 1 - inward, sp.2)
                    } else {
                        (sp.0, sp.1 - inward, sp.2)
                    };
                    gen_pf(vfeat, fr, ctx, rng, pp);
                    if *waterlog {
                        // `WaterloggedVegetationPatchFeature.placeVegetation` re-reads the
                        // *surface* point after the feature and sets `waterlogged=true` when
                        // the state carries the property and has it false.
                        let placed = ctx.get(sp.0, sp.1, sp.2);
                        let wst = reg.with_prop(placed, "waterlogged", "true");
                        if wst != placed {
                            ctx.set(sp.0, sp.1, sp.2, wst);
                        }
                    }
                }
            }
        }
        Cf::BlockColumn {
            layers,
            down,
            allowed,
            prio,
        } => {
            let Some(reg) = breg() else { return };
            let n = layers.len();
            let mut heights: Vec<i32> = layers.iter().map(|(h, _)| h.get(rng)).collect();
            let total: i32 = heights.iter().sum();
            if total == 0 {
                return;
            }
            let d = if *down { -1 } else { 1 };
            // measure allowed run length
            let mut allowed_len = 0;
            {
                let mut mp = (pos.0, pos.1 + d, pos.2);
                while allowed_len < total {
                    let ok = match allowed {
                        Some(pr) => test_pred(pr, reg, ctx, mp.0, mp.1, mp.2),
                        None => {
                            ctx.is_air(mp.0, mp.1, mp.2)
                                || reg.name_of(ctx.get(mp.0, mp.1, mp.2)) == "minecraft:water"
                        }
                    };
                    if !ok {
                        // vanilla adjust_layer_heights: trim layers from tip (prio)
                        // or base
                        let mut trim = total - allowed_len;
                        let idxs: Vec<usize> = if *prio {
                            (0..n).collect()
                        } else {
                            (0..n).rev().collect()
                        };
                        for i in idxs {
                            let cut = heights[i].min(trim);
                            heights[i] -= cut;
                            trim -= cut;
                            if trim <= 0 {
                                break;
                            }
                        }
                        break;
                    }
                    mp.1 += d;
                    allowed_len += 1;
                }
            }
            let mut mp = pos;
            for (i, (_h, b)) in layers.iter().enumerate() {
                for _ in 0..heights[i] {
                    let st = b.get(ctx, rng, mp.0, mp.1, mp.2);
                    ctx.set(mp.0, mp.1, mp.2, st);
                    mp.1 += d;
                }
            }
        }
        Cf::BlockPile(bsp) => {
            // `BlockPileFeature` (`ebm.a(ecg)`), read from the pinned 1.21.1 bytecode:
            //   if (pos.y < minBuildHeight + 5) return false;
            //   int dx = 2 + random.nextInt(2); int dz = 2 + random.nextInt(2);
            //   for (BlockPos p : BlockPos.betweenClosed(pos.offset(-dx, 0, -dz),
            //                                              pos.offset(dx, 1, dz))) {
            //      int ox = pos.x - p.x; int oz = pos.z - p.z;
            //      if ((float)(ox*ox + oz*oz) <= random.nextFloat() * 10.0F - random.nextFloat() * 6.0F)
            //         tryPlaceBlock(p);
            //      else if ((double)random.nextFloat() < 0.031) tryPlaceBlock(p);
            //   }
            // `betweenClosed` varies X fastest, then Y, then Z — the draw order is the fixture.
            let Some(reg) = breg() else { return };
            if pos.1 < ctx.g.min_y + 5 {
                return;
            }
            let dx = 2 + rng.next_bound(2);
            let dz = 2 + rng.next_bound(2);
            for z in (pos.2 - dz)..=(pos.2 + dz) {
                for y in pos.1..=(pos.1 + 1) {
                    for x in (pos.0 - dx)..=(pos.0 + dx) {
                        let ox = pos.0 - x;
                        let oz = pos.2 - z;
                        let d2 = (ox * ox + oz * oz) as f32;
                        let placed = if d2 <= rng.next_f32() * 10.0 - rng.next_f32() * 6.0 {
                            true
                        } else {
                            (rng.next_f32() as f64) < 0.031
                        };
                        if !placed {
                            continue;
                        }
                        // `tryPlaceBlock`: empty cell and a support that can hold the pile.
                        if !ctx.is_air(x, y, z) {
                            continue;
                        }
                        let below = ctx.get(x, y - 1, z);
                        let may_place_on = if reg.name_of(below) == "minecraft:dirt_path" {
                            rng.next_bool()
                        } else {
                            // `BlockStateBase.isFaceSturdy(level, below, Direction.UP)`
                            face_sturdy(reg, below, 1, Support::Full)
                        };
                        if may_place_on {
                            let st = bsp.get(ctx, rng, x, y, z);
                            ctx.set(x, y, z, st);
                        }
                    }
                }
            }
        }
        Cf::Lake { fluid, barrier } => {
            let Some(reg) = breg() else { return };
            // `LakeFeature.place`: the origin must clear the bottom by more than four
            // blocks (`pos.getY() <= level.getMinBuildHeight() + 4` -> fail), then the
            // 16x8x16 grid is anchored with `pos.relative(Direction.DOWN, 4)` -- the
            // x and z of the origin are *not* shifted, so grid cell (x, y, z) is the
            // world position `pos.offset(x, y, z)` of `(pos.x, pos.y - 4, pos.z)`.
            // Reading that as a `pos.offset(-8, -4, -8)` anchor moved every lava lake
            // eight blocks diagonally: seed 4242 chunk (5,-32) put its lake at
            // (73..88, -516..-499) where the native world has it at (81..96, -508..-493).
            if pos.1 <= ctx.g.min_y + 4 {
                return;
            }
            let anchor = (pos.0, pos.1 - 4, pos.2);
            // vanilla LakeFeature: 4-7 ellipsoid blobs in a 16x8x16 grid
            let mut grid = [false; 16 * 16 * 8];
            let gidx = |x: usize, z: usize, y: usize| (x * 16 + z) * 8 + y;
            let spots = rng.next_bound(4) + 4;
            for _ in 0..spots {
                let xr = rng.next_f64() * 6.0 + 3.0;
                let yr = rng.next_f64() * 4.0 + 2.0;
                let zr = rng.next_f64() * 6.0 + 3.0;
                let xp = rng.next_f64() * (16.0 - xr - 2.0) + 1.0 + xr / 2.0;
                let yp = rng.next_f64() * (8.0 - yr - 4.0) + 2.0 + yr / 2.0;
                let zp = rng.next_f64() * (16.0 - zr - 2.0) + 1.0 + zr / 2.0;
                for xx in 1..15usize {
                    for zz in 1..15usize {
                        for yy in 1..7usize {
                            let xd = (xx as f64 - xp) / (xr / 2.0);
                            let yd = (yy as f64 - yp) / (yr / 2.0);
                            let zd = (zz as f64 - zp) / (zr / 2.0);
                            if xd * xd + yd * yd + zd * zd < 1.0 {
                                grid[gidx(xx, zz, yy)] = true;
                            }
                        }
                    }
                }
            }
            let shell = |x: usize, z: usize, y: usize| -> bool {
                !grid[gidx(x, z, y)]
                    && (x < 15 && grid[gidx(x + 1, z, y)]
                        || x > 0 && grid[gidx(x - 1, z, y)]
                        || z < 15 && grid[gidx(x, z + 1, y)]
                        || z > 0 && grid[gidx(x, z - 1, y)]
                        || y < 7 && grid[gidx(x, z, y + 1)]
                        || y > 0 && grid[gidx(x, z, y - 1)])
            };
            let fluid_st = fluid.get(ctx, rng, anchor.0, anchor.1, anchor.2);
            let barrier_st = barrier.get(ctx, rng, anchor.0, anchor.1, anchor.2);
            let air_st = reg.state_of("minecraft:cave_air", None);
            for x in 0..16usize {
                for z in 0..16usize {
                    for y in 0..8usize {
                        if !shell(x, z, y) {
                            continue;
                        }
                        let bp = (
                            anchor.0 + x as i32,
                            anchor.1 + y as i32,
                            anchor.2 + z as i32,
                        );
                        let cur = ctx.get(bp.0, bp.1, bp.2);
                        if y >= 4 && reg.is_liquid(cur) {
                            return;
                        }
                        if y < 4 && !reg.is_solid(cur) && cur != fluid_st {
                            return;
                        }
                    }
                }
            }
            for x in 0..16usize {
                for z in 0..16usize {
                    for y in 0..8usize {
                        if grid[gidx(x, z, y)] {
                            let bp = (
                                anchor.0 + x as i32,
                                anchor.1 + y as i32,
                                anchor.2 + z as i32,
                            );
                            let cur = ctx.get(bp.0, bp.1, bp.2);
                            if !reg.in_tag(cur, "minecraft:features_cannot_replace") {
                                ctx.set(bp.0, bp.1, bp.2, if y >= 4 { air_st } else { fluid_st });
                                if y >= 4 {
                                    mark_above_for_post_processing(ctx, reg, bp.0, bp.1, bp.2);
                                }
                            }
                        }
                    }
                }
            }
            if !reg.is_air(barrier_st) {
                for x in 0..16usize {
                    for z in 0..16usize {
                        for y in 0..8usize {
                            if shell(x, z, y) && (y < 4 || rng.next_bound(2) != 0) {
                                let bp = (
                                    anchor.0 + x as i32,
                                    anchor.1 + y as i32,
                                    anchor.2 + z as i32,
                                );
                                let cur = ctx.get(bp.0, bp.1, bp.2);
                                if reg.is_solid(cur)
                                    && !reg.in_tag(cur, "minecraft:lava_pool_stone_cannot_replace")
                                {
                                    ctx.set(bp.0, bp.1, bp.2, barrier_st);
                                    mark_above_for_post_processing(ctx, reg, bp.0, bp.1, bp.2);
                                }
                            }
                        }
                    }
                }
            }
            // freeze the water surface row in cold biomes
            if reg.name_of(fluid_st) == "minecraft:water" {
                let ice = reg.state_of("minecraft:ice", None);
                for x in 0..16usize {
                    for z in 0..16usize {
                        let bp = (anchor.0 + x as i32, anchor.1 + 4, anchor.2 + z as i32);
                        if let Some(biome) = ctx.biome_leaf(bp.0, bp.1, bp.2) {
                            // `Biome.shouldFreeze`: positional temperature (including the
                            // height adjustment/frozen noise) and the block-light check,
                            // not the biome JSON's unadjusted base temperature.
                            if ctx.cold_enough_to_snow(biome, bp.0, bp.1, bp.2)
                                && ctx.block_light(bp.0, bp.1, bp.2) < 10
                            {
                                let cur = ctx.get(bp.0, bp.1, bp.2);
                                if !reg.in_tag(cur, "minecraft:features_cannot_replace")
                                    && is_water_st(reg, cur)
                                {
                                    ctx.set(bp.0, bp.1, bp.2, ice);
                                }
                            }
                        }
                    }
                }
            }
        }
        Cf::BlueIce => {
            // `BlueIceFeature` (`ebn.a(ecg)`), read from the pinned 1.21.1 bytecode. The
            // previous body was a hand-written "drop to the floor and scatter a 7x5x7
            // ball" sketch; it was not this algorithm and its draws were not this
            // stream.
            //
            //   0: if (origin.getY() > level.getSeaLevel() - 1) return false
            //   33: if (!get(origin).is(WATER) && !get(origin.below()).is(WATER)) return false
            //   70: for (Direction d : Direction.values())
            //   100:   if (d == DOWN) continue
            //   111:   if (get(origin.relative(d)).is(PACKED_ICE)) { flag = true; break }
            //   144: if (!flag) return false
            //   151: setBlock(origin, BLUE_ICE, 2)
            //   166: for (i = 0; i < 200; ++i) {
            //   177:   int j = nextInt(5) - nextInt(6)
            //   197:   int k = 3; if (j < 2) k += j / 2
            //   215:   if (k < 1) continue
            //   224:   p = origin.offset(nextInt(k) - nextInt(k), j,
            //                        nextInt(k) - nextInt(k))
            //   270:   st = get(p)
            //   280:   if (st.isAir() || st.is(WATER) || st.is(ICE) || st.is(PACKED_ICE))
            //   324:     for (Direction d : Direction.values())
            //   366:       if (get(p.relative(d)).is(BLUE_ICE)) {
            //   377:         setBlock(p, BLUE_ICE, 2); break }
            //
            // `Direction.values()` is DOWN, UP, NORTH, SOUTH, WEST, EAST. Both direction
            // walks only ask "does any neighbour match", so their order is not a draw;
            // the 200 draws are, and `nextInt(1)`-shaped draws are not elided here.
            let Some(reg) = breg() else { return };
            const DIRS: [(i32, i32, i32); 6] = [
                (0, -1, 0),
                (0, 1, 0),
                (0, 0, -1),
                (0, 0, 1),
                (-1, 0, 0),
                (1, 0, 0),
            ];
            let sea = ctx.g.sea;
            if pos.1 > sea - 1 {
                return;
            }
            let water = reg.state_of("minecraft:water", None);
            if ctx.get(pos.0, pos.1, pos.2) != water && ctx.get(pos.0, pos.1 - 1, pos.2) != water {
                return;
            }
            let packed = reg.state_of("minecraft:packed_ice", None);
            let blue = reg.state_of("minecraft:blue_ice", None);
            let ice = reg.state_of("minecraft:ice", None);
            let mut supported = false;
            for &(dx, dy, dz) in &DIRS[1..] {
                if ctx.get(pos.0 + dx, pos.1 + dy, pos.2 + dz) == packed {
                    supported = true;
                    break;
                }
            }
            if !supported {
                return;
            }
            if !ctx.out_of_h(pos.1) {
                ctx.set(pos.0, pos.1, pos.2, blue);
            }
            for _ in 0..200 {
                let j = rng.next_bound(5) - rng.next_bound(6);
                let mut k = 3;
                if j < 2 {
                    k += j / 2;
                }
                if k < 1 {
                    continue;
                }
                let p = (
                    pos.0 + rng.next_bound(k) - rng.next_bound(k),
                    pos.1 + j,
                    pos.2 + rng.next_bound(k) - rng.next_bound(k),
                );
                let cur = ctx.get(p.0, p.1, p.2);
                if reg.is_air(cur) || cur == water || cur == ice || cur == packed {
                    for &(dx, dy, dz) in &DIRS {
                        if ctx.get(p.0 + dx, p.1 + dy, p.2 + dz) == blue {
                            if !ctx.out_of_h(p.1) {
                                ctx.set(p.0, p.1, p.2, blue);
                            }
                            break;
                        }
                    }
                }
            }
        }
        Cf::BlockBlob { state } => {
            // vanilla BlockBlobFeature: descend to first solid ground, place blob
            let Some(reg) = breg() else { return };
            let mut p = pos;
            for _ in 0..64 {
                if ctx.is_air(p.0, p.1, p.2) {
                    let below = ctx.get(p.0, p.1 - 1, p.2);
                    if reg.in_tag(below, "minecraft:dirt")
                        || reg.in_tag(below, "minecraft:base_stone_overworld")
                        || reg.is_solid(below)
                    {
                        break;
                    }
                    p.1 -= 1;
                } else {
                    p.1 += 1;
                }
                if p.1 < ctx.g.world_min_y + 3 || p.1 > ctx.g.world_min_y + ctx.g.world_height - 4 {
                    return;
                }
            }
            // three-block blob: base + two above, radius falloff by chance
            for dy in 0..3i32 {
                let r = if dy == 0 { 2 } else { 1 };
                for dx in -r..=r {
                    for dz in -r..=r {
                        let bp = (p.0 + dx, p.1 + dy, p.2 + dz);
                        if dx * dx + dz * dz > r * r + 1 {
                            continue;
                        }
                        let cur = ctx.get(bp.0, bp.1, bp.2);
                        if reg.is_air(cur)
                            || reg.is_replaceable(cur)
                            || reg.name_of(cur).contains("grass")
                            || reg.name_of(cur).contains("snow")
                        {
                            ctx.set(bp.0, bp.1, bp.2, *state);
                        }
                    }
                }
            }
        }
        Cf::UnderMagma {
            floor,
            prob,
            radius,
        } => {
            // `UnderwaterMagmaFeature.place` (`eds.a(ecg)`), read from the pinned 1.21.1
            // bytecode:
            //
            //    OptionalInt floorY = Column.scan(level, pos, config.floor_search_range,
            //                                     s -> s.is(WATER), s -> !s.is(WATER))
            //                                 .map(Column::getFloorY);
            //    if (floorY.isEmpty()) return false;
            //    BoundingBox bb = fromCorners(floorPos.offset(-r,-r,-r), floorPos.offset(r,r,r));
            //    return BlockPos.betweenClosedStream(bb)
            //        .filter(p -> random.nextFloat() < probability)      // one draw per cell
            //        .filter(p -> isValidPosition(level, p))
            //        .mapToInt(p -> { setBlock(p, MAGMA, 2); return 1; }).sum() > 0;
            //
            // Three things the port had wrong, all of them visible on the taiga 3x3 (11
            // magma cells at (69..71,19..20,-503..-501), (79..81,45..46,-485..-483) and
            // (95,-1..-2,-504)):
            //
            //  * the draw order. `betweenClosed` walks x fastest, then y, then z; the port
            //    ran dx/dy/dz with dz fastest, so the probability draws landed on a
            //    permutation of the cells and magma appeared two blocks diagonally away.
            //  * `Column.scan` inspects five positions (it moves `i < maxSteps` times and
            //    then tests the *moved* position), so a floor four blocks under the origin
            //    is still a floor; the port stopped one short.
            //  * `isValidPosition` tests the cell, the cell below and the four horizontal
            //    neighbours - not the cell above.
            let Some(reg) = breg() else { return };
            let magma = reg.state_of("minecraft:magma_block", None);
            // `s.is(Blocks.WATER)`: the block, not the fluid, so a waterlogged state is not
            // water here.
            let is_water_block = |ctx: &mut FC, p: (i32, i32, i32)| {
                reg.name_of(ctx.get(p.0, p.1, p.2)) == "minecraft:water"
            };
            if !is_water_block(ctx, pos) {
                return;
            }
            // `Column.scan`: `for (i = 1; i < maxSteps && isStateAtPosition(mut, allowed); i++)
            // mut.move(down);` then `isStateAtPosition(mut, target) ? of(mut.getY()) : empty`.
            let mut y = pos.1;
            let mut steps = 1;
            while steps < *floor && is_water_block(ctx, (pos.0, y, pos.2)) {
                y -= 1;
                steps += 1;
            }
            if is_water_block(ctx, (pos.0, y, pos.2)) {
                return;
            }
            let r = *radius;
            let wet_air = |ctx: &mut FC, p: (i32, i32, i32)| {
                let st = ctx.get(p.0, p.1, p.2);
                reg.is_air(st) || reg.name_of(st) == "minecraft:water"
            };
            // `isValidPosition` (`eds.b`).
            let valid = |ctx: &mut FC, p: (i32, i32, i32)| {
                if wet_air(ctx, p) || wet_air(ctx, (p.0, p.1 - 1, p.2)) {
                    return false;
                }
                for (dx, dz) in [(1, 0), (-1, 0), (0, 1), (0, -1)] {
                    if wet_air(ctx, (p.0 + dx, p.1, p.2 + dz)) {
                        return false;
                    }
                }
                true
            };
            // `betweenClosed` order: x fastest, then y, then z.
            for dz in -r..=r {
                for dy in -r..=r {
                    for dx in -r..=r {
                        let bp = (pos.0 + dx, y + dy, pos.2 + dz);
                        let draw = rng.next_f32();
                        if draw >= *prob {
                            continue;
                        }
                        if !valid(ctx, bp) {
                            continue;
                        }
                        ctx.set(bp.0, bp.1, bp.2, magma);
                    }
                }
            }
        }
        Cf::FreezeTop => {
            let Some(reg) = breg() else { return };
            let min_y = ctx.g.world_min_y;
            let max_y = min_y + ctx.g.world_height;
            // Fast skip: if the 3x3 chunk neighborhood carries no biome that can
            // reach < 0.15 temperature anywhere below y=320, no column can freeze
            // or accumulate snow and FreezeTop consumes zero RNG.
            if fr.bit_ok && test_biome_leaf(ctx).is_none() && max_y <= 320 {
                let skip = LEAF_BITS.with(|c| {
                    let c = c.borrow();
                    c.active && (c.present_mask & fr.possibly_cold_mask) == 0
                });
                if skip {
                    return;
                }
            }
            let ft = feat_tables(reg);
            let ice = ft.ice;
            let snow = ft.snow;
            for lx in 0..16i32 {
                for lz in 0..16i32 {
                    let (x, z) = (pos.0 + lx, pos.2 + lz);
                    let top = ctx.heightmap(HeightmapKind::MotionBlocking, x, z);
                    let Some(biome) = ctx.biome_leaf(x, top, z) else {
                        continue;
                    };
                    let below_y = top - 1;
                    if below_y >= min_y
                        && below_y < max_y
                        && ctx.cold_enough_to_snow(biome, x, below_y, z)
                        && ctx.block_light(x, below_y, z) < 10
                    {
                        let below = ctx.get(x, below_y, z);
                        if reg.block_id_of(below) == Some(ft.water_bid)
                            && reg.fluid_name(below) == "minecraft:water"
                        {
                            ctx.set(x, below_y, z, ice);
                        }
                    }
                    if top >= min_y
                        && top < max_y
                        && ctx.cold_enough_to_snow(biome, x, top, z)
                        && ctx.block_light(x, top, z) < 10
                    {
                        let at = ctx.get(x, top, z);
                        let below = if below_y >= min_y && below_y < max_y {
                            ctx.get(x, below_y, z)
                        } else {
                            0
                        };
                        if (reg.is_air(at) || reg.block_id_of(at) == Some(ft.snow_bid))
                            && snow_can_survive(reg, below)
                        {
                            ctx.set(x, top, z, snow);
                            let has_snowy = if !ft.snowy_blocks.overflow {
                                ft.snowy_blocks.contains_state(reg, below)
                            } else {
                                reg.has_prop(reg.name_of(below), "snowy")
                            };
                            if has_snowy {
                                ctx.set(x, below_y, z, reg.with_prop(below, "snowy", "true"));
                            }
                        }
                    }
                }
            }
        }
        Cf::Multiface(cfg) => {
            let Some(reg) = breg() else { return };
            let cur = ctx.get(pos.0, pos.1, pos.2);
            if !(reg.is_air(cur) || is_water_st(reg, cur)) {
                return;
            }
            let dlen = cfg.dirs.len();
            if dlen <= 6 {
                let mut dirs_arr = [0u8; 6];
                dirs_arr[..dlen].copy_from_slice(&cfg.dirs);
                let dirs = &mut dirs_arr[..dlen];
                feature_util_shuffle(dirs, rng);
                if multiface_try(ctx, reg, rng, pos, cfg, dirs) {
                    return;
                }
                for &d in dirs.iter() {
                    let mut faces_arr = [0u8; 6];
                    let mut flen = 0usize;
                    for &v in &cfg.dirs {
                        if v != (d ^ 1) {
                            faces_arr[flen] = v;
                            flen += 1;
                        }
                    }
                    let faces = &mut faces_arr[..flen];
                    feature_util_shuffle(faces, rng);
                    // ecw resets to origin + direction on EVERY iteration; it does
                    // not walk search_range blocks down the corridor.
                    let p = multiface_relative(pos, d);
                    for _ in 0..cfg.search {
                        let cur = ctx.get(p.0, p.1, p.2);
                        if !(reg.is_air(cur)
                            || is_water_st(reg, cur)
                            || reg.block_id_of(cur) == Some(cfg.block_id))
                        {
                            break;
                        }
                        if multiface_try(ctx, reg, rng, p, cfg, faces) {
                            return;
                        }
                    }
                }
            } else {
                let mut dirs = cfg.dirs.clone();
                feature_util_shuffle(&mut dirs, rng);
                if multiface_try(ctx, reg, rng, pos, cfg, &dirs) {
                    return;
                }
                for &d in &dirs {
                    let mut faces: Vec<_> =
                        cfg.dirs.iter().copied().filter(|v| *v != (d ^ 1)).collect();
                    feature_util_shuffle(&mut faces, rng);
                    let p = multiface_relative(pos, d);
                    for _ in 0..cfg.search {
                        let cur = ctx.get(p.0, p.1, p.2);
                        if !(reg.is_air(cur)
                            || is_water_st(reg, cur)
                            || reg.block_id_of(cur) == Some(cfg.block_id))
                        {
                            break;
                        }
                        if multiface_try(ctx, reg, rng, p, cfg, &faces) {
                            return;
                        }
                    }
                }
            }
        }
        Cf::Bamboo(prob) => {
            let Some(reg) = breg() else { return };
            if !ctx.is_air(pos.0, pos.1, pos.2)
                || !can_place(reg, ctx, "minecraft:bamboo", pos.0, pos.1, pos.2)
            {
                return;
            }
            let height = rng.next_bound(12) + 5;
            if rng.next_f32() < *prob {
                let r = rng.next_bound(4) + 1;
                let podzol = reg.state_of("minecraft:podzol", None);
                for xx in pos.0 - r..=pos.0 + r {
                    for zz in pos.2 - r..=pos.2 + r {
                        let xd = xx - pos.0;
                        let zd = zz - pos.2;
                        if xd * xd + zd * zd <= r * r {
                            let py = ctx.hm_ws(xx, zz) - 1;
                            let cur = ctx.get(xx, py, zz);
                            if reg.in_tag(cur, "minecraft:bamboo_plantable_on")
                                || is_dirtish(reg, cur)
                            {
                                ctx.set(xx, py, zz, podzol);
                            }
                        }
                    }
                }
            }
            let mk = |leaves: &str, stage: &str| {
                reg.state_of(
                    "minecraft:bamboo",
                    Some(&mkprops(&[
                        ("age", "1"),
                        ("leaves", leaves),
                        ("stage", stage),
                    ])),
                )
            };
            let trunk = mk("none", "0");
            let mut by = pos.1;
            for _ in 0..height {
                if ctx.is_air(pos.0, by, pos.2) {
                    ctx.set(pos.0, by, pos.2, trunk);
                    by += 1;
                } else {
                    break;
                }
            }
            if by - pos.1 >= 3 {
                ctx.set(pos.0, by - 1, pos.2, mk("large", "1"));
                ctx.set(pos.0, by - 2, pos.2, mk("large", "0"));
                ctx.set(pos.0, by - 3, pos.2, mk("small", "0"));
            }
        }
        Cf::Vines => {
            // `VinesFeature.place` (`edu.a(ecg)`), read from the pinned 1.21.1 bytecode:
            //
            //    if (!level.isEmptyBlock(pos)) return false;
            //    for (Direction dir : Direction.values())
            //       if (dir != DOWN && VineBlock.canSupportAtFace(level, pos.relative(dir), dir)) {
            //          level.setBlock(pos, VINE.setValue(VINE_PROPS.get(dir), true), 2);
            //          return true;
            //       }
            //    return false;
            //
            // One block, one supporting face, `Direction.values()` order (DOWN, UP, NORTH, SOUTH,
            // WEST, EAST) with DOWN skipped - so UP is tried first. The port hung a four-block
            // column and only from horizontal neighbours, which is not this version's feature:
            // the lush caves' `classic_vines_cave_feature` places ceiling-anchored
            // `vine[up=true]`, and the port wrote a wall vine or nothing (measured: chunk
            // (5,-32) native `vine[up=true]` at (78,-2,-497) where the port had air, which let a
            // later ceiling patch's air test pass and pick up 126 columns vanilla refuses).
            //
            // `VineBlock.canSupportAtFace` (`dov.a(dcc,jd,ji)`) is `MultifaceBlock.canAttachTo`
            // (`dky.a(dcc,ji,jd,dtc)`): the neighbour's block-support shape *or* its collision
            // shape fully covers the face that points back at the vine.
            let Some(reg) = breg() else { return };
            let base = reg.state_of("minecraft:vine", None);
            if !ctx.is_air(pos.0, pos.1, pos.2) {
                return;
            }
            for d in [1u8, 2, 3, 4, 5] {
                let (dx, dy, dz) = dir_off(d);
                let nb = (pos.0 + dx, pos.1 + dy, pos.2 + dz);
                let s = ctx.get(nb.0, nb.1, nb.2);
                let back = (d ^ 1) as usize;
                if !(reg.side_solid(s, back) || reg.is_collision_shape_full_block(s)) {
                    continue;
                }
                let prop = match d {
                    1 => "up",
                    2 => "north",
                    3 => "south",
                    4 => "west",
                    _ => "east",
                };
                let st = reg.with_prop(base, prop, "true");
                ctx.set(pos.0, pos.1, pos.2, st);
                return;
            }
        }
        Cf::MonsterRoom => {
            let Some(reg) = breg() else { return };
            let xr = rng.next_bound(2) + 2;
            let zr = rng.next_bound(2) + 2;
            let (min_x, max_x) = (-xr - 1, xr + 1);
            let (min_z, max_z) = (-zr - 1, zr + 1);
            // The validation scan consumes zero RNG and makes zero writes. Count
            // perimeter openings first (24-32 columns) and only verify floor/ceiling
            // solidity when 1..=5 openings exist, skipping unused interior cells.
            let mut holes = 0;
            for dx in min_x..=max_x {
                for dz in min_z..=max_z {
                    let on_wall = dx == min_x || dx == max_x || dz == min_z || dz == max_z;
                    if !on_wall {
                        continue;
                    }
                    let (wx, wz) = (pos.0 + dx, pos.2 + dz);
                    if ctx.is_air(wx, pos.1, wz) && ctx.is_air(wx, pos.1 + 1, wz) {
                        holes += 1;
                        if holes > 5 {
                            return;
                        }
                    }
                }
            }
            if holes == 0 {
                return;
            }
            for dx in min_x..=max_x {
                let wx = pos.0 + dx;
                for dz in min_z..=max_z {
                    let wz = pos.2 + dz;
                    if !reg.is_solid(ctx.get(wx, pos.1 - 1, wz))
                        || !reg.is_solid(ctx.get(wx, pos.1 + 4, wz))
                    {
                        return;
                    }
                }
            }
            let ft = feat_tables(reg);
            let cobble = ft.cobble;
            let mossy = ft.mossy;
            let cave_air = ft.cave_air;
            for dx in min_x..=max_x {
                for dy in (-1..=3i32).rev() {
                    for dz in min_z..=max_z {
                        let wp = (pos.0 + dx, pos.1 + dy, pos.2 + dz);
                        let boundary = dx == min_x
                            || dy == -1
                            || dz == min_z
                            || dx == max_x
                            || dy == 4
                            || dz == max_z;
                        let cur = ctx.get(wp.0, wp.1, wp.2);
                        if boundary {
                            let below = ctx.get(wp.0, wp.1 - 1, wp.2);
                            if wp.1 >= ctx.g.min_y && !reg.is_solid(below) {
                                ctx.set(wp.0, wp.1, wp.2, cave_air);
                            } else if reg.is_solid(cur)
                                && reg.block_id_of(cur) != Some(ft.chest_bid)
                            {
                                let state = if dy == -1 && rng.next_bound(4) != 0 {
                                    mossy
                                } else {
                                    cobble
                                };
                                safe_feature_set(
                                    ctx,
                                    wp,
                                    state,
                                    "minecraft:features_cannot_replace",
                                );
                            }
                        } else {
                            let bid = reg.block_id_of(cur);
                            if bid != Some(ft.chest_bid) && bid != Some(ft.spawner_bid) {
                                safe_feature_set(
                                    ctx,
                                    wp,
                                    cave_air,
                                    "minecraft:features_cannot_replace",
                                );
                            }
                        }
                    }
                }
            }
            // Chest placement precedes the spawner and its mob-selection draw.
            // Block-entity loot contents are not yet persisted by the feature layer.
            let chest = ft.chest;
            for _ in 0..2 {
                for _ in 0..3 {
                    let cx = pos.0 + rng.next_bound(xr * 2 + 1) - xr;
                    let cz = pos.2 + rng.next_bound(zr * 2 + 1) - zr;
                    if !ctx.is_air(cx, pos.1, cz) {
                        continue;
                    }
                    let walls = [(-1i32, 0i32), (1, 0), (0, -1), (0, 1)]
                        .iter()
                        .filter(|(dx, dz)| reg.is_solid(ctx.get(cx + dx, pos.1, cz + dz)))
                        .count();
                    if walls == 1 {
                        let oriented = orient_chest(ctx, (cx, pos.1, cz), chest);
                        safe_feature_set(
                            ctx,
                            (cx, pos.1, cz),
                            oriented,
                            "minecraft:features_cannot_replace",
                        );
                        // RandomizableContainer.setBlockEntityLootTable consumes
                        // nextLong only if the chest block entity exists.
                        if reg.block_id_of(ctx.get(cx, pos.1, cz)) == Some(ft.chest_bid) {
                            let loot_seed = rng.next_i64();
                            ctx.push_loot_container(
                                (cx, pos.1, cz),
                                crate::generation_payload::ContainerKind::Chest,
                                "minecraft:chests/simple_dungeon".to_owned(),
                                Some(loot_seed),
                            );
                        }
                        break;
                    }
                }
            }
            let spawner = ft.spawner;
            safe_feature_set(ctx, pos, spawner, "minecraft:features_cannot_replace");
            // MonsterRoomFeature.randomEntityId is called only with a spawner
            // entity present. A protected centre must not consume this draw.
            if reg.block_id_of(ctx.get(pos.0, pos.1, pos.2)) == Some(ft.spawner_bid) {
                let mob = rng.next_bound(4) as usize;
                let mob_id = [
                    "minecraft:skeleton",
                    "minecraft:zombie",
                    "minecraft:zombie",
                    "minecraft:spider",
                ][mob];
                ctx.push_spawner(pos, mob_id);
            }
        }
        Cf::Fallen { trunk } => {
            let Some(reg) = breg() else { return };
            let len = 2 + rng.next_bound(3) + rng.next_bound(3); // vanilla-ish log len
            let horiz = rng.next_bool();
            // find ground: scan down from pos to first solid
            let mut p = pos;
            for _ in 0..8 {
                let below = ctx.get(p.0, p.1 - 1, p.2);
                if ctx.is_air(p.0, p.1, p.2) && reg.is_solid(below) {
                    break;
                }
                if ctx.is_air(p.0, p.1, p.2) {
                    p.1 -= 1;
                } else {
                    p.1 += 1;
                }
                if p.1 < ctx.g.world_min_y + 2 || p.1 > ctx.g.world_min_y + ctx.g.world_height - 2 {
                    return;
                }
            }
            for i in 0..len {
                let (x, z) = if horiz {
                    (p.0 + i, p.2)
                } else {
                    (p.0, p.2 + i)
                };
                let below = ctx.get(x, p.1 - 1, z);
                if !(is_dirtish(reg, below) || reg.is_solid(below)) {
                    break;
                }
                let cur = ctx.get(x, p.1, z);
                if reg.is_air(cur) || reg.is_replaceable(cur) || reg.name_of(cur).contains("snow") {
                    let st = trunk.get(ctx, rng, x, p.1, z);
                    let st = reg.with_prop(st, "axis", if horiz { "x" } else { "z" });
                    ctx.set(x, p.1, z, st);
                }
            }
        }
    }
}

// ---------- carvers (GenerationStep.Carving.AIR) ----------
//
// Vanilla `NoiseBasedChunkGenerator.applyCarvers` walks the -8..8 chunk
// neighbourhood, asks each neighbour's biome (sampled at that chunk's minimum
// corner, y=0) for its carver list, reseeds a `LegacyRandomSource` per carver
// index and lets every carver that "starts" carve tunnels into THIS chunk
// through the chunk's shared `CarvingMask`. The port keeps that shape: one pass
// per target chunk, run before its features, writing into the same override map
// the feature layer uses so everything downstream sees the caves.
//
// Every constant below is from the 1.21.1 bytecode (`ebb` CaveWorldCarver,
// `ebe` WorldCarver, `eaw` CanyonWorldCarver, `dux` CarvingMask,
// `dzb.a(arb,long,...)` applyCarvers), not from a decompiler's guess.

/// `2 * getRange() - 1` as a block distance: WorldCarver.getRange() is 8 and the
/// cave tunnel count is `112 - nextInt(112 / 4)` steps; the canyon multiplies the
/// same 112 by its `distance_factor`.
const CARVE_STEPS: i32 = 112;

/// `2 * PI` as the 1.21.1 bytecode spells it (`ldc 6.2831855f`).
const TAU: f32 = 6.283_185_5;

#[derive(Clone, Copy, PartialEq, Debug)]
enum CKind {
    Cave,
    NetherCave,
    Canyon,
}

/// A `VerticalAnchor`: absolute, above the bottom of the world, or below its top.
#[derive(Clone, Copy)]
enum Anchor {
    Abs(i32),
    AboveBottom(i32),
    BelowTop(i32),
}

impl Anchor {
    fn resolve(self, min_y: i32, height: i32) -> i32 {
        match self {
            Anchor::Abs(v) => v,
            Anchor::AboveBottom(v) => min_y + v,
            Anchor::BelowTop(v) => min_y + height - 1 - v,
        }
    }
}

/// A `FloatProvider` sample. `uniform` is min-inclusive/max-exclusive; the
/// `trapezoid` form draws twice and lands in the middle more often.
#[derive(Clone, Copy)]
enum FProv {
    Const(f32),
    Uniform(f32, f32),
    Trapezoid(f32, f32, f32),
}

impl FProv {
    fn sample(self, r: &mut Lcg) -> f32 {
        match self {
            FProv::Const(v) => v,
            FProv::Uniform(lo, hi) => r.next_f32() * (hi - lo) + lo,
            FProv::Trapezoid(lo, hi, plateau) => {
                let range = hi - lo;
                let half = (range - plateau) / 2.0;
                let flat = range - half;
                lo + r.next_f32() * flat + r.next_f32() * half
            }
        }
    }
}

/// A `HeightProvider`. `minecraft:uniform` is `eia` in 1.21.1, which resolves
/// both anchors and calls `Mth.randomBetweenInclusive` - a *single*
/// `nextInt(max - min + 1)`. `ehv` (trapezoid, `nextInt` twice) is a different
/// provider; copying its shape shifts every later draw in the tunnel.
#[derive(Clone, Copy)]
enum HProv {
    Const(Anchor),
    Uniform(Anchor, Anchor),
}

impl HProv {
    fn sample(self, r: &mut Lcg, min_y: i32, height: i32) -> i32 {
        match self {
            HProv::Const(a) => a.resolve(min_y, height),
            HProv::Uniform(a, b) => {
                let lo = a.resolve(min_y, height);
                let hi = b.resolve(min_y, height);
                if lo > hi {
                    // `eia` warns once and returns the minimum.
                    return lo;
                }
                r.next_bound(hi - lo + 1) + lo
            }
        }
    }
}

struct CarverDef {
    kind: CKind,
    probability: f32,
    y: HProv,
    y_scale: FProv,
    lava: Anchor,
    replaceable: String,
    // CaveCarverConfiguration
    floor: FProv,
    horiz: FProv,
    vert: FProv,
    // CanyonCarverConfiguration
    rotation: FProv,
    canyon: Option<CanyonShape>,
    /// Flat bitmap of the state ids the `#replaceable` tag accepts. Resolved once, lazily,
    /// from `breg()` on the first carve that reaches this carver: asking `in_tag` for every
    /// candidate cell was one of the carver hot loop's costs. `OnceLock` keeps `creg()`
    /// independent of `breg()`'s initialisation order.
    replaceable_bits: std::sync::OnceLock<Box<[u64]>>,
}

/// One carver name in a biome's list, resolved to its index in [`CarverReg::defs`]. The index is
/// the answer the carve loop used to get from a `cfg.get(name)` hash lookup on every origin.
struct ResolvedCarver {
    name: String,
    /// `usize::MAX` when the name has no configured carver — the original `cfg.get` miss was
    /// skipped at the same list index, which is the carver's seed offset.
    def_idx: usize,
}

/// `CanyonShapeConfiguration`, flattened. Field order matches its codec:
/// distance_factor, thickness, width_smoothness, horizontal_radius_factor,
/// vertical_radius_default_factor, vertical_radius_center_factor.
#[derive(Clone, Copy)]
struct CanyonShape {
    distance: FProv,
    thickness: FProv,
    width_smoothness: i32,
    horizontal_radius: FProv,
    vertical_default: f32,
    vertical_center: f32,
}

pub(crate) struct CarverReg {
    /// name -> carver definition, kept for the diagnostics that name a carver. The carve hot
    /// loop never consults it: [`Self::biome_resolved`] carries the direct index instead.
    cfg: HashMap<String, &'static CarverDef>,
    /// Every configured carver in insertion order. Indexed directly by the resolved `def_idx`
    /// in the `17x17` origin loop instead of hash-looking up a name per candidate.
    defs: Vec<&'static CarverDef>,
    /// biome -> carver names, in the order the biome lists them (the index is the seed offset,
    /// so the order is load-bearing). Kept as the canonical name list.
    biome: HashMap<String, Vec<String>>,
    /// biome -> the same lists with each name resolved to its index in `defs`, so the hot loop
    /// needs no name lookup at all.
    biome_resolved: HashMap<String, Vec<ResolvedCarver>>,
}

impl CarverReg {
    /// A configured carver by its resource name, for diagnostics and tests only.
    #[cfg(test)]
    #[cfg_attr(not(megu_vanilla_data), allow(dead_code))]
    fn def(&self, name: &str) -> Option<&'static CarverDef> {
        self.cfg.get(name).copied()
    }
}

pub(crate) fn creg() -> &'static CarverReg {
    static R: std::sync::OnceLock<CarverReg> = std::sync::OnceLock::new();
    R.get_or_init(load_creg)
}

/// Best-effort reason a `configured_carver` body refused to parse: an absent
/// `type` is a missing-field failure, an unlisted `type` is unsupported, and a
/// listed one that still failed has a malformed `config`.
fn carver_fail_reason(j: &J) -> String {
    let Some(ty) = jstr(j, "type") else {
        return "missing required field \"type\"".to_string();
    };
    match ty.rsplit(':').next().unwrap_or(&ty) {
        "cave" | "nether_cave" | "canyon" => match js(j, "config") {
            Some(_) => format!("malformed {ty} carver config (missing or invalid field)"),
            None => "missing required field \"config\"".to_string(),
        },
        _ => format!("unsupported carver type {ty:?}"),
    }
}

/// Parse one `configured_carver` file into `reg`. A failure is recorded in
/// `rep` (one stderr line, queryable after the load) and returned so a strict
/// load can refuse instead of dropping the entry.
fn load_carver_entry(
    reg: &mut CarverReg,
    by_name: &mut HashMap<String, usize>,
    name: &str,
    b: &[u8],
    rep: &mut WorldgenParseReport,
) -> Result<(), WorldgenParseError> {
    let Some(j) = jparse(b) else {
        return Err(rep.fail("configured_carver", name, "malformed JSON"));
    };
    match p_carver(&j) {
        Some(d) => {
            // The registry is `&'static` and each carver is immutable, so leaking the
            // definition lets both `cfg` and `defs` own a stable `&'static` reference.
            let d: &'static CarverDef = Box::leak(Box::new(d));
            let idx = reg.defs.len();
            reg.cfg.insert(name.to_string(), d);
            reg.defs.push(d);
            by_name.insert(name.to_string(), idx);
            Ok(())
        }
        // A carver type this port does not implement must not silently
        // become a different algorithm: it stays out of the registry and
        // the miss is reported once.
        None => Err(rep.fail("configured_carver", name, carver_fail_reason(&j))),
    }
}

fn load_creg() -> CarverReg {
    let mut rep = WorldgenParseReport::default();
    let reg = load_creg_report(
        &mut rep,
        strict_worldgen_parse(),
        &Gen::dp_files("configured_carver"),
        &Gen::dp_files("biome"),
    );
    publish_worldgen_parse_report(&rep);
    reg.unwrap_or_else(|e| panic!("worldgen parse error: {e}"))
}

fn load_creg_report(
    rep: &mut WorldgenParseReport,
    strict: bool,
    carver_files: &HashMap<String, Vec<u8>>,
    biome_files: &HashMap<String, Vec<u8>>,
) -> Result<CarverReg, WorldgenParseError> {
    let mut reg = CarverReg {
        cfg: HashMap::new(),
        defs: Vec::new(),
        biome: HashMap::new(),
        biome_resolved: HashMap::new(),
    };
    let mut by_name: HashMap<String, usize> = HashMap::new();
    for (name, b) in carver_files {
        if let Err(e) = load_carver_entry(&mut reg, &mut by_name, name, b, rep) {
            if strict {
                return Err(e);
            }
        }
    }
    for (name, b) in biome_files {
        if let Some(list) = jparse(b).and_then(|j| carver_list(&j)) {
            // Resolve every name once. Keep unknown names as a sentinel entry so the
            // seed offset (`k`, the list position) stays exactly as before.
            let resolved = list
                .iter()
                .map(|name| ResolvedCarver {
                    def_idx: by_name.get(name).copied().unwrap_or(usize::MAX),
                    name: name.clone(),
                })
                .collect();
            reg.biome_resolved.insert(name.clone(), resolved);
            reg.biome.insert(name.clone(), list);
        }
    }
    Ok(reg)
}

/// 1.21.1 writes `carvers` as `{"air": [...]}` and uses a bare string when the
/// step has one carver; converted packs wrote a bare list (implicitly AIR).
/// The order is the carver seed index.
fn carver_list(j: &J) -> Option<Vec<String>> {
    let carvers = js(j, "carvers")?;
    let air = match carvers {
        // `"carvers": {}` is a legitimate empty AIR step (the End biomes).
        J::Obj(_) => match carvers.get("air") {
            Some(air) => air,
            None => return Some(Vec::new()),
        },
        _ => carvers,
    };
    match air {
        J::Str(s) => Some(vec![ns_name(s)]),
        J::Arr(a) => Some(a.iter().map(|v| ns_name(v.str())).collect()),
        _ => None,
    }
}

fn p_anchor(j: &J) -> Option<Anchor> {
    if let Some(v) = jnum(j, "absolute") {
        return Some(Anchor::Abs(v as i32));
    }
    if let Some(v) = jnum(j, "above_bottom") {
        return Some(Anchor::AboveBottom(v as i32));
    }
    if let Some(v) = jnum(j, "below_top") {
        return Some(Anchor::BelowTop(v as i32));
    }
    None
}

fn p_fprov(j: &J) -> FProv {
    // the codec is `either(float, FloatProvider)`: a bare number is a constant
    if let J::Num(v) = j {
        return FProv::Const(*v as f32);
    }
    let ty = jstr(j, "type").unwrap_or_default();
    match ty.rsplit(':').next().unwrap_or(&ty) {
        "constant" => FProv::Const(jf32(j, "value", 0.0)),
        "trapezoid" => FProv::Trapezoid(
            jf32(j, "min", 0.0),
            jf32(j, "max", 0.0),
            jf32(j, "plateau", 0.0),
        ),
        _ => {
            let lo = jf32(j, "min_inclusive", jf32(j, "min_exclusive", 0.0));
            let hi = jf32(j, "max_exclusive", jf32(j, "max_inclusive", 0.0));
            FProv::Uniform(lo, hi)
        }
    }
}

fn fprov(j: &J, key: &str) -> FProv {
    js(j, key).map(p_fprov).unwrap_or(FProv::Const(0.0))
}

fn p_hprov(j: &J) -> Option<HProv> {
    let ty = jstr(j, "type")?;
    match ty.rsplit(':').next().unwrap_or(&ty) {
        "constant" => Some(HProv::Const(p_anchor(js(j, "value")?)?)),
        "uniform" => Some(HProv::Uniform(
            p_anchor(js(j, "min_inclusive")?)?,
            p_anchor(js(j, "max_inclusive")?)?,
        )),
        _ => None,
    }
}

fn p_carver(j: &J) -> Option<CarverDef> {
    let ty = jstr(j, "type")?;
    let kind = match ty.rsplit(':').next().unwrap_or(&ty) {
        "cave" => CKind::Cave,
        "nether_cave" => CKind::NetherCave,
        "canyon" => CKind::Canyon,
        _ => return None,
    };
    let cfg = js(j, "config")?;
    let canyon = js(cfg, "shape").map(|s| CanyonShape {
        distance: fprov(s, "distance_factor"),
        thickness: fprov(s, "thickness"),
        width_smoothness: jint(s, "width_smoothness", 3),
        horizontal_radius: fprov(s, "horizontal_radius_factor"),
        vertical_default: jf32(s, "vertical_radius_default_factor", 1.0),
        vertical_center: jf32(s, "vertical_radius_center_factor", 0.0),
    });
    if kind == CKind::Canyon && canyon.is_none() {
        return None;
    }
    Some(CarverDef {
        kind,
        probability: jf32(cfg, "probability", 0.0),
        y: p_hprov(js(cfg, "y")?)?,
        y_scale: p_fprov(js(cfg, "yScale")?),
        lava: p_anchor(js(cfg, "lava_level")?)?,
        // the JSON keeps the `#` tag marker; `in_tag` is keyed without it
        replaceable: ns_name(jstr(cfg, "replaceable")?.trim_start_matches('#')),
        floor: fprov(cfg, "floor_level"),
        horiz: fprov(cfg, "horizontal_radius_multiplier"),
        vert: fprov(cfg, "vertical_radius_multiplier"),
        canyon,
        rotation: fprov(cfg, "vertical_rotation"),
        replaceable_bits: std::sync::OnceLock::new(),
    })
}

/// `WorldgenRandom.setLargeFeatureSeed`: two raw longs and a XOR, no `|1` — the
/// population seed used by the feature layer is a different function.
fn set_large_feature_seed(r: &mut Lcg, seed: u64, cx: i32, cz: i32) {
    if let Some(spec) = env_str!("CARVE_RAW") {
        if let Some((a, b)) = spec.split_once(',') {
            if a.parse::<i32>() == Ok(cx) && b.parse::<i32>() == Ok(cz) {
                crate::generation_math::RAW_ARM.store(0, std::sync::atomic::Ordering::Relaxed);
            }
        }
    }
    *r = Lcg::for_large_feature(seed, cx, cz);
}

/// `CaveWorldCarver.getYScale`: two floats plus an occasional 1-in-10 room bulge.
fn cave_rng_y_scale(r: &mut Lcg) -> f32 {
    let mut f = r.next_f32() * 2.0 + r.next_f32();
    if r.next_bound(10) == 0 {
        f *= r.next_f32() * r.next_f32() * 3.0 + 1.0;
    }
    f
}

thread_local! {
    /// The `canyon_widths` profile buffer, parked between tunnels: a canyon run
    /// resizes the same `height`-sized allocation every branch instead of
    /// allocating it anew. Bounded by the 256 KiB idle-scratch rule.
    static CANYON_WIDTHS: std::cell::RefCell<Vec<f32>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// `CanyonWorldCarver.getWidths`: one squared width per block of world height,
/// re-drawn every `width_smoothness` blocks. The height index is absolute world
/// height, so the array is `getGenDepth()` long from the bottom.
fn canyon_widths(depth: usize, smoothness: i32, r: &mut Lcg) -> Vec<f32> {
    let mut out = CANYON_WIDTHS.with(|b| std::mem::take(&mut *b.borrow_mut()));
    out.clear();
    out.resize(depth, 0.0);
    let mut f = 1.0f32;
    for (i, slot) in out.iter_mut().enumerate() {
        // `i == 0 ||` short-circuits, so the first entry never draws nextInt
        if i == 0 || r.next_bound(smoothness) == 0 {
            f = 1.0 + r.next_f32() * r.next_f32();
        }
        *slot = f * f;
    }
    out
}

/// Return a `canyon_widths` buffer to the parked slot. `v` keeps its allocation;
/// the caller swaps it out with `mem::take` when it still owns the vector, so an
/// over-sized buffer is replaced rather than parked past the idle bound.
fn park_canyon_widths(v: &mut Vec<f32>) {
    v.clear();
    if v.capacity() > (256 * 1024) / std::mem::size_of::<f32>() {
        *v = Vec::new();
        return;
    }
    CANYON_WIDTHS.with(|b| {
        let mut b = b.borrow_mut();
        if v.capacity() > b.capacity() {
            std::mem::swap(&mut *b, v);
        }
    });
}

/// `CanyonWorldCarver.getY`: a triangular vertical-radius window over the
/// branch index, times `yScale`, times a 0.75..1.0 jitter.
fn canyon_y(shape: CanyonShape, r: &mut Lcg, y_scale: f64, branch_count: f32, i: f32) -> f64 {
    let w = 1.0 - ((0.5 - i / branch_count).abs() * 2.0);
    let f = shape.vertical_default + shape.vertical_center * w;
    f as f64 * y_scale * (r.next_f32() * 0.25 + 0.75) as f64
}

/// The carver list of one origin chunk, resolved from the biome at its minimum corner.
///
/// `creg()` is `&'static`, so an entry is two static borrows and copying it is free. The
/// lookup behind it (`biome_name_at`: a quart sample plus one `String` clone) is a pure
/// function of the generator's data and seed, and the `17x17` origin scan repeats it across
/// adjacent chunks, so it is resolved once per origin and kept.
type OriginCarvers = Option<(&'static str, &'static [ResolvedCarver])>;

/// How many origin chunks one thread keeps resolved. The scan is 289 entries per chunk and a
/// run walks chunks in raster order, so a few rows of the window is all any thread re-reads;
/// the cap only keeps a long run from growing without bound.
const ORIGIN_CARVERS_CAP: usize = 4096;

struct OriginCarversCache {
    /// `(generation_seq, world seed, preset)`. `generation_seq` is bumped by every `init`, so
    /// a replacement generator, a reseed or another dimension cannot answer from the previous
    /// one's entries: the biome source is a function of exactly these three.
    ctx: (u64, u64, NoisePreset),
    map: CoordMap<(i32, i32), OriginCarvers>,
    order: std::collections::VecDeque<(i32, i32)>,
}

impl Default for OriginCarversCache {
    fn default() -> Self {
        Self {
            // `GEN_SEQ` starts at 1, so the first lookup always takes the miss path and a
            // cache can never answer before it knows which generator it belongs to.
            ctx: (0, 0, NoisePreset::Overworld),
            map: CoordMap::default(),
            order: std::collections::VecDeque::new(),
        }
    }
}

thread_local! {
    /// Per thread, not shared: the answer is pure, so two threads resolving the same origin
    /// is duplicated work and never a disagreement, while a process-wide map would serialise
    /// the 289 lookups every chunk makes (including every carver worker's).
    static ORIGIN_CARVERS: std::cell::RefCell<OriginCarversCache> =
        std::cell::RefCell::new(OriginCarversCache::default());
}

/// Number of block state ids the replaceable bitset covers. The loaded registry has 26_684
/// states; this power-of-two bound leaves headroom. A state at or above it takes the
/// (unreachable in practice) `in_tag` fallback in [`is_replaceable`].
const CARVE_STATE_CAP: usize = 32 * 1024;

/// The lazily-built bitset for one carver's `replaceable` tag.
fn replaceable_bits<'a>(c: &'a CarverDef, br: &BReg) -> &'a [u64] {
    c.replaceable_bits.get_or_init(|| {
        let mut bits = vec![0u64; CARVE_STATE_CAP.div_ceil(64)];
        for st in 0..CARVE_STATE_CAP as u32 {
            if br.in_tag(st, &c.replaceable) {
                bits[st as usize >> 6] |= 1u64 << (st & 63);
            }
        }
        bits.into_boxed_slice()
    })
}

/// Whether state `st` is in `c.replaceable`, answered from a flat bitmap instead of two hash
/// lookups per carved cell. The bounds branch keeps the answer exact if the registry ever grows
/// past [`CARVE_STATE_CAP`].
#[inline]
fn is_replaceable(c: &CarverDef, br: &BReg, st: u32) -> bool {
    let bits = replaceable_bits(c, br);
    if (st as usize) < CARVE_STATE_CAP {
        bits[st as usize >> 6] >> (st & 63) & 1 != 0
    } else {
        br.in_tag(st, &c.replaceable)
    }
}

/// Resolve the carver list of origin `(ox, oz)` from the biome at its minimum corner. Pure in
/// `(data, seed, preset)`: a worker's scratch generator and the owner give the same answer.
fn resolve_origin(g: &mut Gen, ox: i32, oz: i32) -> OriginCarvers {
    g.biome_name_at(ox * 16, 0, oz * 16).and_then(|biome| {
        creg()
            .biome_resolved
            .get_key_value(&biome)
            .map(|(name, list)| (name.as_str(), list.as_slice()))
    })
}

/// The carver list of origin chunk `(ox, oz)`, resolved once per (generator, seed, preset,
/// origin) and kept in the owner's thread-local cache.
fn origin_carvers(g: &mut Gen, ox: i32, oz: i32) -> OriginCarvers {
    let ctx = (crate::worldgen::generation_seq(), g.wseed(), g.preset);
    ORIGIN_CARVERS.with(|cache| {
        let mut cache = cache.borrow_mut();
        if cache.ctx != ctx {
            cache.ctx = ctx;
            cache.map.clear();
            cache.order.clear();
        }
        if let Some(hit) = cache.map.get(&(ox, oz)) {
            return *hit;
        }
        let resolved = resolve_origin(g, ox, oz);
        cache.map.insert((ox, oz), resolved);
        cache.order.push_back((ox, oz));
        while cache.order.len() > ORIGIN_CARVERS_CAP {
            if let Some(old) = cache.order.pop_front() {
                cache.map.remove(&old);
            }
        }
        resolved
    })
}

/// Resolved origin-carver lists shared read-only by every carve worker. Contiguous batches
/// use a flat rectangle; distant batches keep only origins within reach of a claim.
/// All values borrow from `creg()` and are `Send + Sync`.
struct OriginGrid {
    ox0: i32,
    oz0: i32,
    w: usize,
    h: usize,
    cells: Vec<OriginCarvers>,
    sparse: Option<CoordMap<(i32, i32), OriginCarvers>>,
}

impl OriginGrid {
    /// The union bounding box of the claims plus each carver's `+/-8` origin reach.
    fn new(order: &[(i32, i32)]) -> Self {
        if order.is_empty() {
            return Self {
                ox0: 0,
                oz0: 0,
                w: 0,
                h: 0,
                cells: Vec::new(),
                sparse: None,
            };
        }
        let min_tx = order.iter().map(|p| p.0).min().unwrap_or(0);
        let max_tx = order.iter().map(|p| p.0).max().unwrap_or(0);
        let min_tz = order.iter().map(|p| p.1).min().unwrap_or(0);
        let max_tz = order.iter().map(|p| p.1).max().unwrap_or(0);
        let (ox0, oz0) = (min_tx - 8, min_tz - 8);
        let w = (i64::from(max_tx) - i64::from(min_tx) + 17) as usize;
        let h = (i64::from(max_tz) - i64::from(min_tz) + 17) as usize;
        let sparse = order.len() > 1
            && (w as u64).saturating_mul(h as u64) > (order.len() as u64).saturating_mul(289 * 2);
        Self {
            ox0,
            oz0,
            w,
            h,
            cells: if sparse {
                Vec::new()
            } else {
                vec![None; w * h]
            },
            sparse: sparse.then(CoordMap::default),
        }
    }
    #[inline]
    fn idx(&self, ox: i32, oz: i32) -> Option<usize> {
        let ix = (ox - self.ox0) as usize;
        let iz = (oz - self.oz0) as usize;
        (ix < self.w && iz < self.h).then_some(ix * self.h + iz)
    }
    #[inline]
    fn get(&self, ox: i32, oz: i32) -> OriginCarvers {
        if let Some(sparse) = &self.sparse {
            return sparse.get(&(ox, oz)).copied().flatten();
        }
        self.idx(ox, oz).and_then(|i| self.cells[i])
    }
    #[inline]
    fn set(&mut self, ox: i32, oz: i32, v: OriginCarvers) {
        if let Some(sparse) = &mut self.sparse {
            sparse.insert((ox, oz), v);
            return;
        }
        if let Some(i) = self.idx(ox, oz) {
            self.cells[i] = v;
        }
    }
}

/// Fill an [`OriginGrid`] for `claims`: reuse the owner's cache first, resolve the misses once
/// (in parallel when there are enough of them), then record the new answers back in the cache.
fn build_origin_grid(g: &mut Gen, claims: &[(i32, i32)], jobs: usize) -> OriginGrid {
    let mut grid = OriginGrid::new(claims);
    if claims.is_empty() {
        return grid;
    }
    let ctx = (crate::worldgen::generation_seq(), g.wseed(), g.preset);
    let mut missing: Vec<(i32, i32)> = Vec::new();
    ORIGIN_CARVERS.with(|cache| {
        let mut cache = cache.borrow_mut();
        if cache.ctx != ctx {
            cache.ctx = ctx;
            cache.map.clear();
            cache.order.clear();
        }
        let is_sparse = grid.sparse.is_some();
        let (ox0, oz0, w, h) = (grid.ox0, grid.oz0, grid.w, grid.h);
        let mut visit = |ox, oz| match cache.map.get(&(ox, oz)) {
            Some(&v) => grid.set(ox, oz, v),
            None => missing.push((ox, oz)),
        };
        if is_sparse {
            let mut seen = HashSet::new();
            for &(tx, tz) in claims {
                for ox in tx - 8..=tx + 8 {
                    for oz in tz - 8..=tz + 8 {
                        if seen.insert((ox, oz)) {
                            visit(ox, oz);
                        }
                    }
                }
            }
        } else {
            for ix in 0..w {
                for iz in 0..h {
                    visit(ox0 + ix as i32, oz0 + iz as i32);
                }
            }
        }
    });
    if jobs > 1 && missing.len() >= 8 {
        let owner: &Gen = &*g;
        let per = missing.len().div_ceil(jobs).max(1);
        let mut resolved: Vec<OriginCarvers> = vec![None; missing.len()];
        bg_scope!(|s| {
            for (chunk, out) in missing.chunks(per).zip(resolved.chunks_mut(per)) {
                s.spawn(move || {
                    let mut scratch = Gen::fresh_scratch(owner.data.clone());
                    scratch.starts_round = owner.starts_round;
                    if let Some(resolver) = owner.beard_resolver() {
                        scratch.set_beard_resolver(resolver);
                    }
                    for (&(ox, oz), slot) in chunk.iter().zip(out.iter_mut()) {
                        *slot = resolve_origin(&mut scratch, ox, oz);
                    }
                });
            }
        });
        for (&(ox, oz), &v) in missing.iter().zip(&resolved) {
            grid.set(ox, oz, v);
        }
    } else {
        for &(ox, oz) in &missing {
            let v = resolve_origin(g, ox, oz);
            grid.set(ox, oz, v);
        }
    }
    ORIGIN_CARVERS.with(|cache| {
        let mut cache = cache.borrow_mut();
        for &(ox, oz) in &missing {
            cache.map.insert((ox, oz), grid.get(ox, oz));
            cache.order.push_back((ox, oz));
            while cache.order.len() > ORIGIN_CARVERS_CAP {
                if let Some(old) = cache.order.pop_front() {
                    cache.map.remove(&old);
                }
            }
        }
    });
    grid
}

/// The prefetched base state of one cell: the owner generator a worker carves for, else the
/// generator being carved (`None`).
#[inline]
fn prefetched_cell(g: &Gen, pref: Option<&Gen>, x: i32, y: i32, z: i32) -> Option<u32> {
    match pref {
        Some(pref) => pref.prefetched_cell(x, y, z),
        None => g.prefetched_cell(x, y, z),
    }
}

#[cfg(test)]
thread_local! {
    /// Test-side proof that the prefetch fast path in [`CarveRun::read`] answered at least one
    /// cell: a bit-identical result is produced by the `terrain_cell` fallback too, so identity
    /// alone does not show the new path ran (rules CV1).
    static CARVE_PREFETCH_READS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// Test-side proof that [`carve_batch`] really spawned workers: the serial and the
/// parallel arm produce the same bytes, so the comparison alone cannot say which one ran.
#[cfg(test)]
static CARVE_RING_WORKERS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Reusable per-thread buffers for one target chunk's carve. A worker creates one and keeps it
/// across every chunk in its slice; the owner has a thread-local one for serial carves. Reusing
/// them removed a 396 KB allocate-and-zero (384 KB `out` + 12 KB `mask`) on every chunk.
struct CarveScratch {
    height: usize,
    mask: Vec<u64>,
    out: Vec<u32>,
    out_order: Vec<u32>,
    shape_marks: Vec<(i32, i32, i32)>,
    /// The write list a finished carve hands to the owner; the commit returns the
    /// emptied buffer here so the next chunk's `out_writes` needs no allocation.
    out_writes: Vec<(i32, i32, i32, u32)>,
}

impl CarveScratch {
    fn new() -> Self {
        Self {
            height: 0,
            mask: Vec::new(),
            out: Vec::new(),
            out_order: Vec::new(),
            shape_marks: Vec::new(),
            out_writes: Vec::new(),
        }
    }
    /// Make the buffers ready for a chunk of `height` blocks. On the common same-height path
    /// only the slots actually written last time are restored, both small vectors are cleared
    /// and the mask is zeroed in place, so no allocation happens per chunk.
    fn reset_for(&mut self, height: usize) {
        if self.height != height {
            self.mask = vec![0u64; (height * 256).div_ceil(64)];
            self.out = vec![u32::MAX; height * 256];
            self.height = height;
        } else {
            for &i in &self.out_order {
                self.out[i as usize] = u32::MAX;
            }
            self.mask.fill(0);
        }
        self.out_order.clear();
        self.shape_marks.clear();
        self.out_writes.clear();
        // The write list is committed one bucket at a time; a bucket that ever
        // grew past the idle bound is shrunk so a parked scratch stays tiny.
        if self.out_writes.capacity() > (256 * 1024) / std::mem::size_of::<(i32, i32, i32, u32)>() {
            self.out_writes = Vec::new();
        }
    }
}

thread_local! {
    /// The owner's scratch buffer for the serial carve paths (`apply_carvers`, `carve_claimed`).
    static CARVE_SCRATCH: std::cell::RefCell<CarveScratch> =
        std::cell::RefCell::new(CarveScratch::new());
}

/// One target chunk's carving state: a borrowed [`CarveScratch`] holding the shared mask and the
/// cells written so far, plus the `CarvingContext` facts the carve reads (world bounds, floor
/// level) and the block states resolved once per run instead of once per candidate cell.
struct CarveRun<'a, 's> {
    tx: i32,
    tz: i32,
    min_y: i32,
    height: i32,
    /// `floor_level` of the carver being applied: `skip` rejects cells at or
    /// below it, and vanilla builds that predicate once per carve call.
    floor: f64,
    /// `lava_level` of the carver being applied, set before its first read: see
    /// [`CarveRun::read`] for why the prefetch fast path refuses a cell at or below it.
    lava: i32,
    scratch: &'s mut CarveScratch,
    /// The generator whose prefetched base rows answer a cell this run has not written
    /// (`None`: the generator being carved). A worker's scratch generator has no prefetch of
    /// its own, so its owner supplies the answers (see [`prefetched_cell`]).
    pref: Option<&'a Gen>,
    /// Probe mode: pretend every cell is stone and every aquifer sample is cave
    /// air, and print the writes. That is the world `CarverProbe.java` gives
    /// vanilla, so the two cell sets can be compared without a worldgen behind
    /// either side.
    probe: bool,
    probe_air: u32,
    probe_lava: u32,
    /// `minecraft:stone`, for the probe read; resolved once per run.
    stone_state: u32,
    /// `grass_block` / `podzol` / `dirt`, for the surface-restore check in `carve_block`.
    grass_state: u32,
    podzol_state: u32,
    dirt_state: u32,
    /// `MEGU_NOFOLD`, hoisted out of the per-cell `carve_block`.
    nofold: bool,
    /// First cell the current source chunk wrote, for the `SRC` trace that is
    /// compared against the real server's carve agent.
    mark: Option<(i32, i32, i32)>,
}

impl<'a, 's> CarveRun<'a, 's> {
    /// An empty run for `(tx, tz)`: no writes, no mask bits, probe off.
    fn empty(
        g: &Gen,
        pref: Option<&'a Gen>,
        tx: i32,
        tz: i32,
        scratch: &'s mut CarveScratch,
    ) -> Self {
        let (min_y, height) = (g.min_y, g.height);
        scratch.reset_for(height as usize);
        let br = breg();
        Self {
            tx,
            tz,
            min_y,
            height,
            floor: 0.0,
            // No carver has run yet, so no cell is at or below a lava level this run knows:
            // the first `carve_block` sets it before its first read.
            lava: min_y - 1,
            scratch,
            pref,
            probe: false,
            probe_air: br.map_or(12959, |b| b.state_of("minecraft:cave_air", None)),
            probe_lava: br.map_or(96, |b| b.state_of("minecraft:lava", None)),
            stone_state: br.map_or(1, |b| b.state_of("minecraft:stone", None)),
            grass_state: br.map_or(0, |b| b.state_of("minecraft:grass_block", None)),
            podzol_state: br.map_or(0, |b| b.state_of("minecraft:podzol", None)),
            dirt_state: br.map_or(0, |b| b.state_of("minecraft:dirt", None)),
            nofold: env_flag!("MEGU_NOFOLD"),
            mark: None,
        }
    }
    #[inline]
    fn idx(&self, lx: i32, y: i32, lz: i32) -> usize {
        (((y - self.min_y) << 8) | ((lz & 15) << 4) | (lx & 15)) as usize
    }
    /// [`CarveRun::idx`] when `y` is inside the tracked window, which is the mask's window.
    ///
    /// Every `carve_block` call site is clamped inside it (`y1` stops at `min_y + height - 8`
    /// and `y0` at `min_y + 1`, and the second write is one below), so this never rejects a
    /// cell the run would have written: it turns a position that has no write to see into the
    /// same answer the old map's missing key gave, instead of an index past the array.
    #[inline]
    fn slot(&self, lx: i32, y: i32, lz: i32) -> Option<usize> {
        if y < self.min_y || y >= self.min_y + self.height {
            return None;
        }
        Some(self.idx(lx, y, lz))
    }
    #[inline]
    fn mask_get(&self, lx: i32, y: i32, lz: i32) -> bool {
        let i = self.idx(lx, y, lz);
        self.scratch.mask[i >> 6] >> (i & 63) & 1 != 0
    }
    #[inline]
    fn mask_set(&mut self, lx: i32, y: i32, lz: i32) {
        let i = self.idx(lx, y, lz);
        self.scratch.mask[i >> 6] |= 1 << (i & 63);
    }
    /// The cell this run has written, if it has: the fold (`& 15`) is inside [`CarveRun::idx`].
    #[inline]
    fn written(&self, lx: i32, y: i32, lz: i32) -> Option<u32> {
        let i = self.slot(lx, y, lz)?;
        let st = self.scratch.out[i];
        (st != u32::MAX).then_some(st)
    }
    /// Record a carve write: the first write to a cell appends its index, later ones only
    /// replace the state (the commit order is the order of first writes).
    #[inline]
    fn put(&mut self, lx: i32, y: i32, lz: i32, st: u32) {
        if let Some(i) = self.slot(lx, y, lz) {
            if self.scratch.out[i] == u32::MAX {
                self.scratch.out_order.push(i as u32);
            }
            self.scratch.out[i] = st;
        }
    }
    /// The local position a flat index stands for, inverting [`CarveRun::idx`].
    #[inline]
    fn pos_of(&self, i: u32) -> (i32, i32, i32) {
        let i = i as usize;
        (
            (i & 15) as i32,
            self.min_y + (i >> 8) as i32,
            ((i >> 4) & 15) as i32,
        )
    }
    /// The chunk's block state as the carver sees it: cells carved earlier in
    /// this pass, else the surface-rule terrain.
    fn read(&self, g: &mut Gen, cc: &ChunkCols, x: i32, y: i32, z: i32) -> u32 {
        if self.probe {
            return self.stone_state;
        }
        // Vanilla hands `carveBlock` the *target* chunk and both the read and the
        // write go through `pos` folded with `& 15`, so out-of-chunk candidates
        // are judged against this chunk's block at the folded position. Reading
        // the neighbour's terrain instead (tried 2026-10-22) changes nothing and
        // is not what the bytecode does.
        if let Some(st) = self.written(x, y, z) {
            return st;
        }
        // A base row a worker already built is the state `terrain_cell` would compute, for
        // one map lookup instead of a cold material/aquifer/deep-rule evaluation (see
        // `Gen::prefetched_cell`). Two gates, both about identity:
        //
        // * the cell must be *of this chunk*. The fold above applies to the write and to the
        //   write's read, but the terrain read below hands `terrain_cell` the unfolded
        //   position (`aq_state`/`block_density` see it), so answering a folded out-of-chunk
        //   position from this chunk's rows would substitute another cell's state.
        // * the cell must be above `lava`. A carve-time read is a lookup of what the fill
        //   wrote — vanilla's `carveBlock` reads the chunk's blocks and never asks the
        //   aquifer, so `shouldScheduleFluidUpdate` is only ever written by
        //   `getCarveState` (and by the fill before carving started). `terrain_cell`
        //   recomputes the aquifer's answer lazily, so at or below the level its flag
        //   write *would* be the one the mark gate sees; the prefetch fast path simply
        //   cannot answer there, because it has no flag of its own to write.
        if y > self.lava && (x >> 4) == self.tx && (z >> 4) == self.tz {
            if let Some(st) = prefetched_cell(g, self.pref, x, y, z) {
                #[cfg(test)]
                CARVE_PREFETCH_READS.with(|c| c.set(c.get() + 1));
                return st;
            }
        }
        // Keep the flag across the fallback too: a `terrain_cell` call here stands in for
        // the same block lookup the prefetch answered, and its aquifer work — including a
        // lazy surface-material build under `ChunkCols` — is this generator's own history,
        // not part of the carve's `getCarveState` flag chain the mark gate reads.
        let sched = g.aq_schedule_fluid_update();
        let st = g.terrain_cell(cc, x, y, z);
        g.set_aq_schedule_fluid_update(sched);
        st
    }
}

/// `NoiseBasedChunkGenerator.applyCarvers` for one target chunk.
///
/// Diagnostics are env-gated and deliberately small, because each one is a way to
/// compare against vanilla without a second implementation standing in for it:
///
/// * `MEGU_NOCARVE` — skip carvers (the "no carvers" world fixture).
/// * `CARVE_TRACE` — one line per chunk: configs loaded, starts, cells written.
/// * `CARVE_PROBE=<tx,tz>|all` — carve an all-stone world (every ellipsoid cell is
///   replaceable and the aquifer answers cave_air) and print the writes. That is
///   the world `tools/vanilla-probe/CarverProbe.java` gives vanilla, so the two
///   cell sets compare directly.
/// * `CARVE_ONLY=<carver id>` — run a single carver (cave / canyon / …).
/// * `CARVE_DUMP_WRITES=<tx,tz>` — print the real (terrain-filtered) writes.
pub(crate) fn apply_carvers(g: &mut Gen, tx: i32, tz: i32) {
    // M3 P1a M5: the carver stage is the tail the rest of the batch waits on. The body has
    // several early exits; the wrapper is how every one of them is timed without moving them.
    let t0 = crate::worldgen::m3_start();
    apply_carvers_body(g, tx, tz, None);
    crate::worldgen::m3_elapsed(
        crate::worldgen::M3_CARVER_NS,
        crate::worldgen::M3_CARVER_N,
        t0,
    );
}

/// `grid`, when a batch has already resolved the origins, is passed straight through; a serial
/// single-chunk call falls back to [`origin_carvers`] and its owner-thread cache.
fn apply_carvers_body(g: &mut Gen, tx: i32, tz: i32, grid: Option<&OriginGrid>) {
    if env_flag!("MEGU_NOCARVE") {
        return;
    }
    let reg = creg();
    if reg.defs.is_empty() || reg.biome.is_empty() || breg().is_none() {
        return;
    }
    let Some(cc) = g.cols(tx, tz) else { return };
    if !g.begin_carve(tx, tz) {
        return;
    }
    let out = CARVE_SCRATCH.with(|s| {
        let mut scratch = s.borrow_mut();
        compute_carve_chunk(g, None, grid, &cc, tx, tz, &mut scratch)
    });
    commit_carve(g, out);
}

/// The carver stage of one populate: the chunk, then the `3x3` around it.
///
/// Vanilla carves the whole decoration area before decorating the centre, so a feature may
/// read a neighbour's carver air. `begin_carve` makes the later per-chunk populate a no-op for
/// its own carver stage, so the ring is carved once and the claims are made in the order the
/// serial loop made them — which is also the order the outputs are committed in.
fn carve_ring(g: &mut Gen, sx: i32, sz: i32) {
    let jobs = if g.starts_round.is_some() {
        crate::worldgen::terrain_jobs()
    } else {
        1
    };
    carve_ring_with(g, sx, sz, jobs);
}

/// [`carve_ring`] with the worker count passed in, so the serial and parallel arms are the same
/// function with one argument different (a test can compare them in one process: the
/// environment is read once per call site and cannot be changed after that).
fn carve_ring_with(g: &mut Gen, sx: i32, sz: i32, jobs: usize) {
    // The chunk itself first, then the ring in the `(dx, dz)` scan order of the
    // double loop it replaces — a fixed 9-cell walk needs no allocation.
    let order = [
        (sx, sz),
        (sx - 1, sz - 1),
        (sx - 1, sz),
        (sx - 1, sz + 1),
        (sx, sz - 1),
        (sx, sz + 1),
        (sx + 1, sz - 1),
        (sx + 1, sz),
        (sx + 1, sz + 1),
    ];
    // A diagnostic that prints per-cell lines is ordered by the carver call it came from, so
    // it runs on one thread: interleaved lines from eight workers would not be the same trace.
    if jobs <= 1 || carver_diag_env() {
        for (tx, tz) in order {
            apply_carvers(g, tx, tz);
        }
        return;
    }
    carve_batch(g, &order, jobs);
}

/// `true` when a carver diagnostic that prints per-carver or per-cell lines is on.
fn carver_diag_env() -> bool {
    env_flag!("MEGU_NOCARVE")
        || env_flag!("CARVE_TRACE")
        || env_str!("CARVE_PROBE").is_some()
        || env_str!("CARVE_DUMP_WRITES").is_some()
        || env_str!("CARVE_RNG").is_some()
        || env_str!("CARVE_SRC").is_some()
        || env_flag!("CV_TRACE")
        || env_str!("CV_TUN").is_some()
        || env_flag!("MEGU_NOFOLD")
}

/// Carve a batch of chunks on `jobs` worker threads, the same work the serial arm does on one.
///
/// The claims are made on the owner first (they are the owner's state, and the order is what the
/// commit order follows); the origins the carvers read are resolved once into a shared
/// [`OriginGrid`]; each worker then carves its share on its own scratch generator and reusable
/// [`CarveScratch`], reading the owner's prefetched base terrain, and the results are committed
/// on the owner in claim order. Nothing a carve reads is another chunk's carve output —
/// `carve_state`, `top_material_at` and `terrain_cell` read `data`, `starts_round` and the beard
/// resolver, never `feat_c` — so the split cannot change the answer, only its schedule.
pub(crate) fn carve_batch(g: &mut Gen, order: &[(i32, i32)], jobs: usize) {
    // The same gates `apply_carvers_body` applies, once for the whole batch: a run that skips
    // carvers or has no carver registry must not claim anything.
    if env_flag!("MEGU_NOCARVE") {
        return;
    }
    let reg = creg();
    if reg.defs.is_empty() || reg.biome.is_empty() || breg().is_none() {
        return;
    }
    // Claim first, and claim *only*: the columns a carve needs are most of the stage's cost,
    // so building them here would leave the arm serial. Each worker builds its own chunk's
    // columns and hands them back through the commit (`Gen::install_cols`), which keeps the
    // owner's `col_c` as warm as the serial ring left it.
    let mut claims: Vec<(i32, i32)> = Vec::with_capacity(order.len());
    for &(tx, tz) in order {
        if g.begin_carve(tx, tz) {
            claims.push((tx, tz));
        }
    }
    if claims.len() < 2 {
        // The claim is already made, so the chunks cannot go through `apply_carvers` again
        // (it would see its own claim and return): carve them here instead.
        for (tx, tz) in claims {
            carve_claimed(g, tx, tz);
        }
        return;
    }
    // Resolve every origin the batch will read once, up front, then share the answer. Workers no
    // longer recompute 289 `biome_name_at` calls each on a cold thread-local cache.
    let grid = build_origin_grid(g, &claims, jobs);
    let jobs = jobs.min(claims.len());
    let mut results: Vec<Option<CarveSlot>> = claims.iter().map(|_| None).collect();
    {
        // `Gen` is `Sync` and a worker only *reads* the owner's prefetched base rows: the
        // landed row is the state `terrain_cell` would compute, and nothing a worker writes
        // reaches the owner until the commit below.
        let owner: &Gen = &*g;
        // Take a shared reference outside the loop: a `move` worker closure captures it by copy,
        // while capturing `grid` itself would try to move the same value into every worker.
        let grid_ref: &OriginGrid = &grid;
        let per = claims.len().div_ceil(jobs).max(1);
        let mut start = 0usize;
        bg_scope!(|s| {
            for slice in results.chunks_mut(per) {
                let cslice = &claims[start..start + slice.len()];
                start += slice.len();
                s.spawn(move || {
                    #[cfg(test)]
                    CARVE_RING_WORKERS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    // M3 P1a M1/M4: everything this thread does is off-owner work.
                    let _m3w = crate::worldgen::M3WorkerScope::enter();
                    let mut gen = Gen::fresh_scratch(owner.data.clone());
                    gen.starts_round = owner.starts_round;
                    if let Some(resolver) = owner.beard_resolver() {
                        gen.set_beard_resolver(resolver);
                    }
                    let mut carve_scratch = CarveScratch::new();
                    for (slot, &(tx, tz)) in slice.iter_mut().zip(cslice) {
                        let t0 = crate::worldgen::m3_start();
                        let got = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                            match gen.cols(tx, tz) {
                                Some(cc) => {
                                    let out = compute_carve_chunk(
                                        &mut gen,
                                        Some(owner),
                                        Some(grid_ref),
                                        &cc,
                                        tx,
                                        tz,
                                        &mut carve_scratch,
                                    );
                                    Some((out, cc))
                                }
                                // No columns for this chunk: the serial body returns before
                                // claiming it, and a carve of it writes nothing either way.
                                None => None,
                            }
                        }));
                        crate::worldgen::m3_elapsed(
                            crate::worldgen::M3_CARVER_NS,
                            crate::worldgen::M3_CARVER_N,
                            t0,
                        );
                        match got {
                            Ok(Some((out, cc))) => *slot = Some(CarveSlot::Carved(out, cc)),
                            Ok(None) => *slot = Some(CarveSlot::NoColumns),
                            // The claim is already made, so a panicking worker leaves its
                            // chunk to the owner (re-run after the scope). Both the generator
                            // and the scratch buffer can hold half-updated state after a panic,
                            // so both are replaced rather than handed the next chunk.
                            Err(_) => {
                                gen = Gen::fresh_scratch(owner.data.clone());
                                gen.starts_round = owner.starts_round;
                                if let Some(resolver) = owner.beard_resolver() {
                                    gen.set_beard_resolver(resolver);
                                }
                                carve_scratch = CarveScratch::new();
                            }
                        }
                    }
                    // The meter is per thread: without this the workers' counts die with
                    // them and the report under-counts exactly the stage being measured.
                    crate::worldgen::m3_flush();
                });
            }
        });
    }
    // The commit order is the claim order, so the writes, the marks and the WG heightmaps
    // land exactly where the serial ring put them.
    for (i, &(tx, tz)) in claims.iter().enumerate() {
        match results[i].take() {
            Some(CarveSlot::Carved(out, cc)) => {
                // The worker built this chunk's columns; land them so the owner does not
                // rebuild what the ring already has (a no-op if it holds them).
                g.install_cols(vec![((tx, tz), cc)]);
                commit_carve(g, out)
            }
            Some(CarveSlot::NoColumns) => {}
            // A worker panicked: the claim is ours, so the chunk is carved here instead.
            None => carve_claimed(g, tx, tz),
        }
    }
}

/// What a worker left in its slot: the carve, or the chunk it could not build columns for.
enum CarveSlot {
    Carved(CarveOutput, std::sync::Arc<ChunkCols>),
    NoColumns,
}

/// Carve one already-claimed chunk on the owner and commit it, timed as one carver call.
///
/// This is the arm a single-claim ring takes, and the fallback a panicking worker leaves
/// behind; the timing covers the column build too, which is what the worker side times.
fn carve_claimed(g: &mut Gen, tx: i32, tz: i32) {
    let t0 = crate::worldgen::m3_start();
    let Some(cc) = g.cols(tx, tz) else {
        return;
    };
    let out = CARVE_SCRATCH.with(|s| {
        let mut scratch = s.borrow_mut();
        compute_carve_chunk(g, None, None, &cc, tx, tz, &mut scratch)
    });
    crate::worldgen::m3_elapsed(
        crate::worldgen::M3_CARVER_NS,
        crate::worldgen::M3_CARVER_N,
        t0,
    );
    commit_carve(g, out);
}

// ---------- Layer A3-a: whole-order pre-carve + dependency-DAG feature waves ----------

/// True when a feature diagnostic depends on serial per-chunk scheduling (interleaved stdout,
/// a thread-local RNG trace, or a height dump taken mid-population). The bytes are the same on
/// either arm; the *trace* is not, so these modes keep the original serial loop.
fn populate_diag_env() -> bool {
    carver_diag_env()
        || env_str!("FEAT_LIST").is_some()
        || env_flag!("FEAT_TRACE")
        || env_flag!("FEAT_PROFILE")
        || env_str!("FEATURE_ATTEMPT_TRACE").is_some()
        || env_str!("FSEED_ARM").is_some()
        || env_str!("FSEED_TRACE").is_some()
        || env_str!("FEATURE_CELL_TRACE").is_some()
        || env_str!("FEATURE_REFUSE_TRACE").is_some()
        || env_str!("FEATURE_WRITE_TRACE").is_some()
        || env_str!("MARK_TRACE").is_some()
        || env_flag!("MEGU_NOFEAT")
        || env_str!("MEGU_HM_DUMP").is_some()
        || env_str!("CEL_COLUMN_TRACE").is_some()
        || env_flag!("MINESHAFT_TRACE")
        // The sculk-cursor sink is thread-local and its stderr arm is an ordered
        // stream: both can only observe the calling thread's work, in order, so a
        // population armed for one stays serial too.
        || env_flag!("SPC_CURSOR_TRACE")
        || SPC_TRACE_SINK.with(|s| s.borrow().is_some())
}

/// Pre-carve the union of every pending chunk's 3x3 before any feature runs.
///
/// Serial `populate_body` carves `(sx,sz)` and its ring before `(sx,sz)`'s features, so a
/// feature may read a neighbour's carved air. The batch leaves those claims and writes on the
/// owner first, which makes every later `carve_ring` a no-op (`begin_carve` returns false) and
/// removes the 49+ tiny per-chunk scopes the old loop spawned.
pub(crate) fn precarve_order(g: &mut Gen, pending: &[(i32, i32)], jobs: usize) {
    // The order list and dedupe set are rebuilt identically for every populate;
    // the parked buffers carry the allocation across calls (cleared before use).
    let (mut order, mut seen) = PRECARVE_SCRATCH.with(|s| {
        let mut s = s.borrow_mut();
        (std::mem::take(&mut s.0), std::mem::take(&mut s.1))
    });
    order.clear();
    seen.clear();
    order.reserve(
        pending
            .len()
            .saturating_mul(9)
            .saturating_sub(order.capacity()),
    );
    seen.reserve(
        pending
            .len()
            .saturating_mul(9)
            .saturating_sub(seen.capacity()),
    );
    for &(sx, sz) in pending {
        // The chunk itself comes first, then the ring in the same `(dx, dz)` order as
        // `carve_ring`, so the claims (and therefore the commit order) stay identical.
        if seen.insert((sx, sz)) {
            order.push((sx, sz));
        }
        for dx in -1..=1 {
            for dz in -1..=1 {
                if dx == 0 && dz == 0 {
                    continue;
                }
                let c = (sx + dx, sz + dz);
                if seen.insert(c) {
                    order.push(c);
                }
            }
        }
    }
    // Carve writes live in the same `feat_c` buckets as feature writes, bounded by
    // `FEATURE_BUCKET_BOUND` (384). A recorded plan can cover far more than 42 source chunks;
    // keep each batch under 9*32 = 288 buckets so no carve output is evicted before the
    // feature pass that reads it.
    const PER_CALL: usize = 32;
    for part in order.chunks(PER_CALL) {
        carve_batch(g, part, jobs);
    }
    PRECARVE_SCRATCH.with(|s| {
        let mut s = s.borrow_mut();
        order.clear();
        seen.clear();
        if order.capacity() > (256 * 1024) / std::mem::size_of::<(i32, i32)>() {
            order = Vec::new();
        }
        if seen.capacity() > (256 * 1024) / std::mem::size_of::<(i32, i32)>() {
            seen = CoordSet::default();
        }
        s.0 = order;
        s.1 = seen;
    });
}

type PrecarveScratch = (Vec<(i32, i32)>, CoordSet<(i32, i32)>);

thread_local! {
    /// `precarve_order`'s dedupe buffers, parked between populates. The pair is a
    /// few KB at the 32-chunk call bound, well under the 256 KiB idle-scratch cap.
    static PRECARVE_SCRATCH: std::cell::RefCell<PrecarveScratch> =
        std::cell::RefCell::new((Vec::new(), CoordSet::default()));
}

/// The minimum Chebyshev-independent spacing of two chunks in one wave. `POP_WRITE_RADIUS`
/// is one chunk, so `|dx| >= 3 || |dz| >= 3` keeps their write windows disjoint. The value is
/// adjustable under `cfg(test)` so the parity test can try the conservative `4` arm too.
fn pop_wave_spacing() -> i32 {
    #[cfg(test)]
    {
        let override_value = POP_WAVE_SPACING.load(Ordering::Relaxed);
        if override_value > 0 {
            return override_value as i32;
        }
    }
    3
}

#[cfg(test)]
static POP_WAVE_SPACING: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
#[cfg(test)]
#[cfg_attr(not(megu_vanilla_data), allow(dead_code))]
pub(crate) fn set_pop_wave_spacing(spacing: usize) {
    POP_WAVE_SPACING.store(spacing, Ordering::Relaxed);
}

#[cfg(test)]
pub(crate) static POPULATE_WORKERS: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);
#[cfg(test)]
#[cfg_attr(not(megu_vanilla_data), allow(dead_code))]
pub(crate) fn populate_workers() -> usize {
    POPULATE_WORKERS.load(Ordering::Relaxed)
}

#[inline]
fn wave_conflict(a: (i32, i32), b: (i32, i32), spacing: i32) -> bool {
    (a.0 - b.0).abs() < spacing && (a.1 - b.1).abs() < spacing
}

/// Topological wave levels. If `j` precedes `i` in `pending` and their write windows overlap,
/// `i` must be committed after `j`: `level[i] > level[j]`. Two chunks at one level therefore
/// neither overlap nor violate the caller's dependency order, which is the checkerboard
/// colouring generalised to an arbitrary `POP_ORDER`.
fn wave_levels(pending: &[(i32, i32)], spacing: i32) -> Vec<usize> {
    let mut levels = vec![0usize; pending.len()];
    for i in 0..pending.len() {
        for j in 0..i {
            if wave_conflict(pending[i], pending[j], spacing) {
                levels[i] = levels[i].max(levels[j] + 1);
            }
        }
    }
    levels
}

type WorkerCols = Vec<((i32, i32), std::sync::Arc<crate::worldgen::ChunkCols>)>;
type WorkerChunkBiomes = Vec<((i32, i32), std::sync::Arc<Vec<String>>)>;
type WorkerDirectMarks = Vec<((i32, i32), Vec<(i32, i32, i32)>)>;

/// What one worker hands back: every write and mark the placement loop produced, plus the
/// ticks and direct marks it filed on its scratch `Gen`. The owner replays them in `pending`
/// order at the wave commit.
struct PopulateWorkerResult {
    wr: crate::generation_context::FlatChunkWriteMap,
    shape_marks: Vec<(i32, i32, i32)>,
    direct_marks: WorkerDirectMarks,
    payloads: crate::generation_context::StagedPayloads,
    mineshaft_spawners: HashSet<(i32, i32, usize)>,
    feat_hm: CoordMap<(i32, i32), (i32, i32)>,
    leaves: Vec<GeneratedTick>,
    fluids: Vec<GeneratedTick>,
    /// The worker's resident terrain answers: the columns it built (`Gen::col_c`) and the
    /// biome unions it computed (`Gen::chunk_biomes`). Pure functions of the round's chunks,
    /// so the owner installs them (`Gen::install_cols` / `Gen::merge_chunk_biomes`) and
    /// later waves plus `promote_full_chunks` answer from the owner instead of rebuilding.
    cols: WorkerCols,
    chunk_biomes: WorkerChunkBiomes,
}

/// The worker half of a feature pass: biome union, `References::collect` and the placement
/// loop. It deliberately omits `carve_ring` (already claimed by `precarve_order`) and the
/// promotion boundary (an owner-only operation, because it reads `pop_done`).
fn populate_worker(
    scratch: &mut Gen,
    owner: &Gen,
    (sx, sz): (i32, i32),
    population_order: &HashMap<(i32, i32), usize>,
    index: usize,
) -> PopulateWorkerResult {
    #[cfg(test)]
    POPULATE_WORKERS.fetch_add(1, Ordering::Relaxed);

    let empty = || PopulateWorkerResult {
        wr: crate::generation_context::FlatChunkWriteMap::new(),
        shape_marks: Vec::new(),
        direct_marks: Vec::new(),
        payloads: Vec::new(),
        mineshaft_spawners: HashSet::new(),
        feat_hm: CoordMap::default(),
        leaves: Vec::new(),
        fluids: Vec::new(),
        cols: Vec::new(),
        chunk_biomes: Vec::new(),
    };
    if env_flag!("MEGU_NOFEAT") {
        return empty();
    }
    let fr = freg();
    let Some(order) = fr.order.get(&owner.preset) else {
        return empty();
    };
    if breg().is_none() {
        return empty();
    }

    let gen: &mut Gen = scratch;
    gen.pop_active = Some((sx, sz));
    gen.suppress_generated_ticks = owner.restores_ticks_for((sx, sz));

    let use_bits = fr.bit_ok;
    let mut present_mask: u128 = 0;
    let mut biomes: Vec<String> = Vec::new();
    for dx in -1..=1 {
        for dz in -1..=1 {
            // A3-a: the union of a chunk the owner already built is the owner's answer
            // (the serial arm populates the same `chunk_biomes` on `g`; the parallel arm
            // merges each wave's unions back into the owner at the wave commit). Only a
            // genuinely new union is built, on the scratch.
            let set = owner
                .chunk_biome_set_readonly(sx + dx, sz + dz)
                .unwrap_or_else(|| gen.chunk_biome_set(sx + dx, sz + dz));
            for b in set.iter() {
                if use_bits {
                    if let Some(&bit) = fr.biome_bits.get(b) {
                        present_mask |= 1u128 << bit;
                    }
                } else if !biomes.iter().any(|v| v == b) {
                    biomes.push(b.clone());
                }
            }
        }
    }
    let pop = pop_seed(owner.wseed(), sx * 16, sz * 16);
    let mut structures = References::collect(gen, sx, sz);
    let mut ctx = FC::new_worker(gen, owner, population_order, index);
    LEAF_BITS.with(|c| {
        let mut c = c.borrow_mut();
        c.active = true;
        c.present_mask = present_mask;
        c.known = 0;
    });
    let preset = owner.preset;
    let t0 = crate::worldgen::m3_start();
    for step in 0..order.len().max(structures.step_count()) {
        structures.place_step(&mut ctx, step, pop, sx, sz);
        let Some(entries) = order.get(step) else {
            continue;
        };
        let bit_entries = fr.step_entries.get(&preset).and_then(|v| v.get(step));
        for (ei, (gi, name)) in entries.iter().enumerate() {
            let slot = bit_entries.map(|v| &v[ei]);
            let present = if use_bits {
                slot.is_some_and(|s| (present_mask & s.step_biome_mask) != 0)
            } else {
                biomes.iter().any(|b| {
                    fr.bsteps
                        .get(b)
                        .and_then(|s| s.get(step))
                        .map(|l| l.iter().any(|f| f == name))
                        .unwrap_or(false)
                })
            };
            if !present {
                continue;
            }
            let pf: &Pf = if use_bits {
                let idx = slot.expect("step entry parallel to order").placed_idx;
                if idx == usize::MAX {
                    continue;
                }
                &fr.placed_vec[idx]
            } else {
                let Some(p) = fr.placed.get(name) else {
                    continue;
                };
                p
            };
            let deco = pop
                .wrapping_add(*gi as u64)
                .wrapping_add(10_000u64.wrapping_mul(step as u64));
            let mut rng = Xor::seed_worldgen(deco);
            if use_bits {
                let mask = slot.expect("step entry parallel to order").ball_mask;
                gen_pf_with_mask(pf, mask, fr, &mut ctx, &mut rng, (sx * 16, 0, sz * 16));
            } else {
                gen_pf(pf, fr, &mut ctx, &mut rng, (sx * 16, 0, sz * 16));
            }
        }
    }
    crate::worldgen::m3_elapsed(
        crate::worldgen::M3_FEATURE_NS,
        crate::worldgen::M3_FEATURE_N,
        t0,
    );
    LEAF_BITS.with(|c| c.borrow_mut().active = false);

    let wr = std::mem::take(&mut ctx.wr);
    let shape_marks = std::mem::take(&mut ctx.shape_marks);
    let payloads = std::mem::take(&mut ctx.payloads);
    let direct_marks = gen.take_all_shape_marks();
    let mineshaft_spawners = std::mem::take(&mut gen.mineshaft_spawners);
    let feat_hm = std::mem::take(&mut gen.feat_hm_for_worker());
    let (leaves, fluids) = gen.take_generated_ticks();
    // A3-a: snapshot the worker's resident terrain answers (its built columns and the
    // biome unions it computed) so the owner can install them after the wave.
    let cols = gen.col_c_snapshot();
    let chunk_biomes = gen.chunk_biomes_snapshot();
    PopulateWorkerResult {
        wr,
        shape_marks,
        direct_marks,
        payloads,
        mineshaft_spawners,
        feat_hm,
        leaves,
        fluids,
        cols,
        chunk_biomes,
    }
}

/// Generated ticks this chunk contributed, in the serial order: the placement loop's first,
/// then the promotion pass'. They are buffered and replayed in caller order at the end of the
/// whole population, because waves commit in dependency order rather than caller order.
struct TickBatch {
    leaves: Vec<GeneratedTick>,
    fluids: Vec<GeneratedTick>,
}

/// The owner continuation of one worker result: land the worker's state, run the same
/// promotion boundary as the serial arm, and flush the writes in the same order.
fn commit_populate(g: &mut Gen, sx: i32, sz: i32, res: PopulateWorkerResult) -> TickBatch {
    let PopulateWorkerResult {
        wr,
        shape_marks,
        direct_marks,
        payloads,
        mineshaft_spawners,
        feat_hm,
        leaves,
        fluids,
        cols,
        chunk_biomes,
    } = res;
    // A3-a: install the worker's resident terrain answers (built columns, biome unions)
    // before the promotion pass, so `FC::cols_for` and any later-wave read answer from
    // the owner instead of rebuilding them.
    g.install_cols(cols);
    g.merge_chunk_biomes(chunk_biomes);
    g.pop_done.insert((sx, sz));
    g.suppress_generated_ticks = g.restores_ticks_for((sx, sz));
    g.merge_shape_marks(direct_marks);
    g.merge_feat_hm(feat_hm);
    g.mineshaft_spawners.extend(mineshaft_spawners);

    let mut ctx = FC::new_commit(g, wr, shape_marks, payloads);
    promote_full_chunks(&mut ctx, sx, sz);
    let mut staged_payloads = std::mem::take(&mut ctx.payloads);
    for ((x, y, z), st) in std::mem::take(&mut ctx.wr) {
        ctx.g.feat_write(x, y, z, st);
    }
    ctx.g.stage_payloads((sx, sz), &mut staged_payloads);
    ctx.g.suppress_generated_ticks = false;
    // The promotion pass scheduled on the owner; keep it with this chunk's placement ticks
    // instead of leaving it in the global queue, which is replayed later in caller order.
    let (promo_leaves, promo_fluids) = ctx.g.take_generated_ticks();
    TickBatch {
        leaves: [leaves, promo_leaves].concat(),
        fluids: [fluids, promo_fluids].concat(),
    }
}

/// Run the feature placement of a multi-chunk population as a dependency-DAG
/// ready-queue instead of barrier-per-wave.
///
/// A chunk's only predecessors are the *earlier* `pending` entries whose write
/// windows overlap its own (`wave_conflict` under `pop_wave_spacing()`): every
/// owner mutation a commit lands stays within two chunks of its own chunk, so
/// a worker's reads — bounded by its 3x3 placement window — can only observe
/// its conflicting predecessors' commits. It is therefore runnable the moment
/// those predecessors are *committed*, not when the slowest chunk of the
/// previous wave finished, which is what the wave barrier waited for.
///
/// Commits still land on the owner in exactly the (level, index) order the
/// per-wave schedule used — wave `k`'s chunks in index order, then wave
/// `k+1`'s — and a chunk is only dispatched after all its conflicting
/// predecessors are committed. Both rules together make the output
/// bit-identical to the serial arm; only the idle time between waves is gone.
pub(crate) fn populate_parallel(g: &mut Gen, pending: &[(i32, i32)], jobs: usize) -> usize {
    // Feature-time lighting sees the caller's order even when distant chunks
    // run ahead of one another.
    let order: HashMap<_, _> = pending.iter().copied().zip(0..).collect();
    let n = pending.len();
    let spacing = pop_wave_spacing();
    let levels = wave_levels(pending, spacing);
    // The commit order the per-wave schedule produced: stable-sort by level
    // keeps each wave's index order and orders whole waves ascending.
    let mut commit_seq: Vec<usize> = (0..n).collect();
    commit_seq.sort_by_key(|&i| levels[i]);
    // The DAG edges: `j -> i` when `j` is an earlier chunk whose write window
    // overlaps `i`'s. `dep_left[i]` counts the predecessors not yet committed.
    let mut dep_left = vec![0usize; n];
    let mut dependents: Vec<Vec<usize>> = (0..n).map(|_| Vec::new()).collect();
    for i in 0..n {
        for j in 0..i {
            if wave_conflict(pending[i], pending[j], spacing) {
                dep_left[i] += 1;
                dependents[j].push(i);
            }
        }
    }
    let mut results: Vec<Option<PopulateWorkerResult>> = (0..n).map(|_| None).collect();
    let mut arrived = vec![false; n];
    let mut ticks: Vec<Option<TickBatch>> = (0..n).map(|_| None).collect();
    // Ticks filed by the pre-carve stay in front of the feature ticks, as they do in the
    // serial arm (CARVERS precede FEATURES for every chunk).
    let (pre_leaves, pre_fluids) = g.take_generated_ticks();

    let data = g.data.clone();
    let starts_round = g.starts_round;
    let resolver = g.beard_resolver();
    let workers = n.min(jobs.max(1));
    // `shared` is how a worker reads the owner and how the owner still commits:
    // a worker holds the read side for one chunk's whole placement — its
    // predecessors are already committed, and a non-conflicting commit can
    // never reach into its read window — while the owner takes the write side
    // for one commit at a time. A queued writer could starve under a steady
    // stream of new readers, so `commit_pending` keeps freshly queued chunks
    // from joining the reader set until the write lands.
    let shared = std::sync::RwLock::new(&mut *g);
    let commit_pending = std::sync::atomic::AtomicBool::new(false);
    let (rtx, rrx) = std::sync::mpsc::channel::<(usize, Option<PopulateWorkerResult>)>();
    // The ready queue: the owner pushes chunks whose predecessors are all
    // committed; workers drain it on their own scratch generators.
    let queue = std::sync::Mutex::new(std::collections::VecDeque::<usize>::new());
    let queue_cv = std::sync::Condvar::new();
    let queue_closed = std::sync::atomic::AtomicBool::new(false);
    let mut ready: std::collections::VecDeque<usize> =
        (0..n).filter(|&i| dep_left[i] == 0).collect();

    bg_scope!(|s| {
        for _ in 0..workers {
            let rtx = rtx.clone();
            let data = data.clone();
            let queue = &queue;
            let queue_cv = &queue_cv;
            let queue_closed = &queue_closed;
            let commit_pending = &commit_pending;
            let shared = &shared;
            let order = &order;
            s.spawn(move || {
                let _m3w = crate::worldgen::M3WorkerScope::enter();
                // One scratch generator per worker (not per chunk): the structure
                // starts, columns and memos it warms are reused by the next chunk on
                // this thread, which is where the per-chunk arm paid most of its cost.
                let mut scratch = Gen::fresh_scratch(data);
                scratch.starts_round = starts_round;
                if let Some(resolver) = resolver {
                    scratch.set_beard_resolver(resolver);
                }
                loop {
                    let i = {
                        let mut q = queue.lock().unwrap_or_else(|e| e.into_inner());
                        loop {
                            if let Some(i) = q.pop_front() {
                                break Some(i);
                            }
                            if queue_closed.load(std::sync::atomic::Ordering::Relaxed) {
                                break None;
                            }
                            q = queue_cv.wait(q).unwrap_or_else(|e| e.into_inner());
                        }
                    };
                    let Some(i) = i else { break };
                    while commit_pending.load(std::sync::atomic::Ordering::Acquire) {
                        std::thread::yield_now();
                    }
                    let guard = shared.read().unwrap_or_else(|e| e.into_inner());
                    let owner: &Gen = &guard;
                    let out = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        // Results are indexed by the chunk's place in `pending`, so a
                        // worker finishing early cannot change the commit order.
                        populate_worker(&mut scratch, owner, pending[i], order, i)
                    }));
                    drop(guard);
                    let res = match out {
                        Ok(r) => Some(r),
                        Err(payload) => {
                            // Unblock the owner before the panic reaches the scope's
                            // latch; an uncommitted chunk is skipped exactly as a
                            // missing wave result was.
                            let _ = rtx.send((i, None));
                            std::panic::resume_unwind(payload);
                        }
                    };
                    let _ = rtx.send((i, res));
                }
                crate::worldgen::m3_flush();
            });
        }
        // The owner event loop: feed the ready queue, land results, and commit
        // in the fixed (level, index) order as soon as each result is in.
        let mut next_commit = 0usize;
        loop {
            if next_commit >= n {
                break;
            }
            while let Some(&i) = ready.front() {
                ready.pop_front();
                queue.lock().unwrap_or_else(|e| e.into_inner()).push_back(i);
                queue_cv.notify_one();
            }
            let Ok((i, res)) = rrx.recv() else { break };
            arrived[i] = true;
            results[i] = res;
            while next_commit < n {
                let ci = commit_seq[next_commit];
                if !arrived[ci] {
                    break;
                }
                next_commit += 1;
                if let Some(res) = results[ci].take() {
                    commit_pending.store(true, std::sync::atomic::Ordering::Release);
                    {
                        let mut guard = shared.write().unwrap_or_else(|e| e.into_inner());
                        let (sx, sz) = pending[ci];
                        ticks[ci] = Some(commit_populate(&mut guard, sx, sz, res));
                    }
                    commit_pending.store(false, std::sync::atomic::Ordering::Release);
                }
                for &d in &dependents[ci] {
                    dep_left[d] -= 1;
                    if dep_left[d] == 0 {
                        ready.push_back(d);
                    }
                }
            }
        }
        queue_closed.store(true, std::sync::atomic::Ordering::Relaxed);
        queue_cv.notify_all();
    });
    // Replay every tick queue in the caller's order. The commit order can
    // differ from `order` for independent chunks; the tick queue is the one
    // global observable where that difference would be visible.
    g.append_generated_ticks(pre_leaves, pre_fluids);
    for batch in ticks.into_iter().flatten() {
        g.append_generated_ticks(batch.leaves, batch.fluids);
    }
    n
}

/// Everything one chunk's carver pass produces, before it is committed onto an owner.
struct CarveOutput {
    tx: i32,
    tz: i32,
    wg: Box<[i32; 512]>,
    shape_marks: Vec<(i32, i32, i32)>,
    /// The stage's block writes as world positions, in first-write order: `feat_c`'s buckets
    /// are keyed by local position, so the order they are inserted in is the order the
    /// carvers made the writes.
    out_writes: Vec<(i32, i32, i32, u32)>,
}

/// Hand one chunk's carve output to the owner that owns its `carve_c` and `feat_c`.
fn commit_carve(g: &mut Gen, out: CarveOutput) {
    let CarveOutput {
        tx,
        tz,
        wg,
        shape_marks,
        out_writes,
    } = out;
    g.set_wg_after_carve(tx, tz, wg);
    // The marks are the positions the carver *aimed* at, filed by
    // `ChunkAccess.markPosForPostprocessing` on the chunk being carved. A write more than a
    // chunk away folds (`x & 15`) into this chunk's array, and the mark folds with it, because
    // vanilla stores the mark as the same packed local offset — so the folded absolute
    // position is the faithful one to file.
    for (x, y, z) in shape_marks {
        g.store_shape_mark_from(x, y, z, crate::worldgen::MarkSrc::Carver);
    }
    let mut out_writes = out_writes;
    for (x, y, z, st) in out_writes.drain(..) {
        g.feat_write(x, y, z, st);
    }
    // The emptied buffer goes back to the owner's scratch so the next committed
    // carve writes into the same allocation.
    CARVE_SCRATCH.with(|s| {
        let mut s = s.borrow_mut();
        if out_writes.capacity() > s.out_writes.capacity() {
            s.out_writes = out_writes;
        }
    });
}

/// Carve one claimed chunk and return what the owner has to record.
///
/// `g` does the carving (a worker's scratch generator when the ring runs in parallel); `pref`
/// is the generator whose prefetched base rows answer its reads, `None` meaning `g` itself.
fn compute_carve_chunk(
    g: &mut Gen,
    pref: Option<&Gen>,
    grid: Option<&OriginGrid>,
    cc: &ChunkCols,
    tx: i32,
    tz: i32,
    scratch: &mut CarveScratch,
) -> CarveOutput {
    let reg = creg();
    // All diagnostics are read once per chunk and then reused; in particular `here` is only
    // formatted when a diagnostic that compares against it is actually on.
    let trace = env_flag!("CARVE_TRACE");
    let probe_env = env_str!("CARVE_PROBE");
    let dump_env = env_str!("CARVE_DUMP_WRITES");
    let only = env_str!("CARVE_ONLY");
    let carve_rng = env_str!("CARVE_RNG");
    let carve_src = env_str!("CARVE_SRC");
    let here = if probe_env.is_some() || dump_env.is_some() || carve_src.is_some() {
        Some(format!("{tx},{tz}"))
    } else {
        None
    };
    let here_str = here.as_deref();
    let probe_here = probe_env == Some("all") || (here_str.is_some() && probe_env == here_str);
    let dump_here = dump_env.is_some() && dump_env == here_str;
    let wseed = g.wseed();
    // Vanilla's aquifer lives on the chunk's `NoiseChunk`: `shouldScheduleFluidUpdate` is
    // whatever `fillFromNoise` left when `applyCarvers` runs on the same object. A worker
    // served its columns from the round's share did not run that build, so the flag enters
    // the carve from the columns, not from this generator's unrelated last aquifer call.
    if let Some(sched) = cc.carve_sched {
        g.set_aq_schedule_fluid_update(sched);
    }
    let mut run = CarveRun::empty(g, pref, tx, tz, scratch);
    run.probe = probe_here;
    let mut starts = 0u32;
    for ox in tx - 8..=tx + 8 {
        for oz in tz - 8..=tz + 8 {
            // the carver list comes from the biome at the neighbour chunk's
            // minimum corner at y=0, not from its centre. With a shared grid this is one
            // bounds-checked slice read per origin, with no lock and no duplicate sampling.
            let origin = match grid {
                Some(grid) => grid.get(ox, oz),
                None => origin_carvers(g, ox, oz),
            };
            let Some((biome, list)) = origin else {
                continue;
            };
            for (k, rc) in list.iter().enumerate() {
                let name = &rc.name;
                if only.is_some_and(|only| only != name) {
                    continue;
                }
                // An unconfigured name was skipped at this same `k` before any RNG draw; keep
                // the sentinel so the carver's seed offset is unchanged.
                if rc.def_idx == usize::MAX {
                    continue;
                }
                let c = reg.defs[rc.def_idx];
                let mut r = Lcg::new(0);
                set_large_feature_seed(&mut r, wseed.wrapping_add(k as u64), ox, oz);
                if r.next_f32() > c.probability {
                    continue;
                }
                starts += 1;
                // The `format!` here only runs when CARVE_RNG is actually set.
                if let Some(spec) = carve_rng {
                    if spec == format!("{ox},{oz}") || spec == "all" {
                        crate::generation_math::RNG_TRACE
                            .store(400, std::sync::atomic::Ordering::Relaxed);
                    }
                }
                if trace {
                    eprintln!("START target={tx},{tz} origin={ox},{oz} k={k} {name} ({biome})");
                }
                run.mark = None;
                match c.kind {
                    CKind::Cave | CKind::NetherCave => run.cave(g, cc, c, &mut r, ox, oz),
                    CKind::Canyon => run.canyon(g, cc, c, &mut r, ox, oz),
                }
                if let Some(spec) = carve_src {
                    if spec == "all" || here_str.is_some_and(|h| spec == h) {
                        if let Some((x, y, z)) = run.mark {
                            eprintln!("SRC ox={ox} oz={oz} k={k} first={x},{y},{z}");
                        }
                    }
                }
            }
        }
    }
    if trace {
        eprintln!(
            "[carve] chunk {tx},{tz}: {} starts, {} mask bits, {} cells written",
            starts,
            run.scratch
                .mask
                .iter()
                .map(|w| w.count_ones() as u64)
                .sum::<u64>(),
            run.scratch.out_order.len()
        );
    }
    if probe_here || dump_here {
        let tag = if probe_here { "CELL" } else { "WCELL" };
        let mut cells: Vec<((u8, i32, u8), u32)> = run
            .scratch
            .out_order
            .iter()
            .map(|&i| {
                let (lx, y, lz) = run.pos_of(i);
                (
                    ((lx & 15) as u8, y, (lz & 15) as u8),
                    run.scratch.out[i as usize],
                )
            })
            .collect();
        cells.sort();
        for ((lx, y, lz), st) in cells {
            eprintln!(
                "{tag} {} {} {} {}",
                tx * 16 + lx as i32,
                y,
                tz * 16 + lz as i32,
                st
            );
        }
        eprintln!(
            "PROBE {} cells={}",
            if probe_here { "stone" } else { "world" },
            run.scratch.out_order.len()
        );
    }
    let wg = wg_heightmaps_after_carve_run(g, cc, tx, tz, &run);
    // The write list rides out on the scratch the run already used and comes back
    // emptied at commit time, so a run of chunks allocates it once.
    let mut out_writes = std::mem::take(&mut run.scratch.out_writes);
    out_writes.reserve(run.scratch.out_order.len());
    for &i in &run.scratch.out_order {
        let (lx, y, lz) = run.pos_of(i);
        out_writes.push((tx * 16 + lx, y, tz * 16 + lz, run.scratch.out[i as usize]));
    }
    CarveOutput {
        tx,
        tz,
        wg,
        shape_marks: std::mem::take(&mut run.scratch.shape_marks),
        out_writes,
    }
}

/// [`wg_heightmaps_after_carve_run`] over an explicit write map, for the fixtures that hand the
/// function a carved cell set instead of running a carve (a `CarveRun` is the carver's own
/// storage and carries a mask and an order a fixture has no use for).
#[cfg(test)]
#[cfg_attr(not(megu_vanilla_data), allow(dead_code))]
fn wg_heightmaps_after_carve(
    g: &mut Gen,
    cc: &ChunkCols,
    tx: i32,
    tz: i32,
    writes: &HashMap<(u8, i32, u8), u32>,
) -> Box<[i32; 512]> {
    let mut scratch = CarveScratch::new();
    let mut run = CarveRun::empty(g, None, tx, tz, &mut scratch);
    for (&(lx, y, lz), &st) in writes {
        run.put(lx as i32, y, lz as i32, st);
    }
    wg_heightmaps_after_carve_run(g, cc, tx, tz, &run)
}

/// `Heightmap.Types.WORLD_SURFACE_WG` and `OCEAN_FLOOR_WG` for one chunk, as
/// `ProtoChunk.setBlockState` leaves them at the end of the carver pass.
///
/// The carver writes go through the chunk, whose status is still SURFACE (vanilla sets
/// the new status after the stage body), and the SURFACE status tracks exactly these two
/// maps — so carve writes lower them, and the FEATURES status afterwards does not touch
/// them at all. Reading them off the pre-carve terrain instead leaves them far too high
/// wherever a cave cut the surface (the port measured 110 against vanilla's 100 at
/// (107,122) on seed 4242).
fn wg_heightmaps_after_carve_run(
    g: &mut Gen,
    cc: &ChunkCols,
    tx: i32,
    tz: i32,
    run: &CarveRun<'_, '_>,
) -> Box<[i32; 512]> {
    let reg = breg();
    let Some(reg) = reg else {
        return Box::new([g.min_y; 512]);
    };
    let (min_y, height) = (g.min_y, g.height);
    let mut tops = Box::new([min_y; 512]);
    // A carve write cannot sit above the terrain top (`#overworld_carver_replaceables`
    // excludes air), but take the written maximum anyway so the scan bound is never the
    // reason a value is missed.
    let mut carve_top = [[i32::MIN; 16]; 16];
    for &i in &run.scratch.out_order {
        let (lx, y, lz) = run.pos_of(i);
        let slot = &mut carve_top[(lz & 15) as usize][(lx & 15) as usize];
        *slot = (*slot).max(y);
    }
    for (lz, carve_row) in carve_top.iter().enumerate() {
        for (lx, &ct) in carve_row.iter().enumerate() {
            let (wx, wz) = (tx * 16 + lx as i32, tz * 16 + lz as i32);
            let mut y = cc.wtop(lx, lz).max(ct).min(min_y + height - 1);
            let mut surface = None;
            let mut floor = None;
            while y >= min_y {
                // Every `(wx, wz)` here is inside `(tx, tz)`, so the prefetched row is this
                // chunk's own cell (see `CarveRun::read` for the two gates a carver read
                // needs; the fold one cannot apply here).
                // A column with no carve writes has nothing for `written` to find, so skip
                // that lookup entirely there — the base/terrain answer is identical.
                let st = if ct == i32::MIN {
                    match prefetched_cell(g, run.pref, wx, y, wz) {
                        Some(st) => st,
                        None => g.terrain_cell(cc, wx, y, wz),
                    }
                } else {
                    match run.written(lx as i32, y, lz as i32) {
                        Some(st) => st,
                        None => match prefetched_cell(g, run.pref, wx, y, wz) {
                            Some(st) => st,
                            None => g.terrain_cell(cc, wx, y, wz),
                        },
                    }
                };
                if surface.is_none() && reg.heightmap_counts(st, HeightmapKind::WorldSurfaceWg) {
                    surface = Some(y + 1);
                }
                if floor.is_none() && reg.heightmap_counts(st, HeightmapKind::OceanFloorWg) {
                    floor = Some(y + 1);
                }
                if surface.is_some() && floor.is_some() {
                    break;
                }
                y -= 1;
            }
            let index = lz * 16 + lx;
            tops[index] = surface.unwrap_or(min_y);
            tops[256 + index] = floor.unwrap_or(min_y);
        }
    }
    tops
}

impl CarveRun<'_, '_> {
    /// `CaveWorldCarver.carve`.
    fn cave(&mut self, g: &mut Gen, cc: &ChunkCols, c: &CarverDef, r: &mut Lcg, ox: i32, oz: i32) {
        // nextInt(nextInt(nextInt(getRange()) + 1) + 1); getRange() is 15 here
        let inner = r.next_bound(if c.kind == CKind::NetherCave { 10 } else { 15 });
        let mid = r.next_bound(inner + 1);
        let tunnels = r.next_bound(mid + 1);
        if env_flag!("CV_TRACE") {
            eprintln!("CVSTART ox={ox} oz={oz} inner={inner} mid={mid} tunnels={tunnels}");
        }
        for _ in 0..tunnels {
            let x = (ox * 16 + r.next_bound(16)) as f64;
            let y = c.y.sample(r, self.min_y, self.height) as f64;
            let z = (oz * 16 + r.next_bound(16)) as f64;
            let hrm = c.horiz.sample(r) as f64;
            let vrm = c.vert.sample(r) as f64;
            self.floor = c.floor.sample(r) as f64;
            let mut branches = 1i32;
            if r.next_bound(4) == 0 {
                let room_scale = c.y_scale.sample(r) as f64;
                let thick = 1.0f32 + r.next_f32() * 6.0;
                // carveRoom: an ellipsoid 1.5 blocks fatter than the tunnel,
                // squashed vertically by yScale, centred one block +x
                let hr = 1.5 + (mth_sin(std::f32::consts::FRAC_PI_2) * thick) as f64;
                self.ellipsoid(g, cc, c, x + 1.0, y, z, hr, hr * room_scale);
                branches += r.next_bound(4);
            }
            if env_flag!("CV_TRACE") {
                eprintln!(
                    "CVTUN ox={ox} oz={oz} x={x} y={y} z={z} hrm={hrm} vrm={vrm} floor={} branches={branches}",
                    self.floor
                );
            }
            for _ in 0..branches {
                let yaw = r.next_f32() * TAU;
                let pitch = (r.next_f32() - 0.5) / 4.0;
                let y_scale = if c.kind == CKind::NetherCave {
                    (r.next_f32() * 2.0 + r.next_f32()) * 2.0
                } else {
                    cave_rng_y_scale(r)
                };
                // a tunnel is up to CARVE_STEPS long, minus up to a quarter
                let count = CARVE_STEPS - r.next_bound(CARVE_STEPS / 4);
                let seed = r.next_i64() as u64;
                if let Some(spec) = env_str!("CV_TUN") {
                    if spec == "all" || spec == format!("{ox},{oz}") {
                        eprintln!(
                            "TUN ox={ox} oz={oz} seed={seed} x={} y={} z={} yaw={yaw} pitch={pitch} count={count}",
                            x.floor(),
                            y.floor(),
                            z.floor()
                        );
                    }
                }
                self.tunnel(
                    g,
                    cc,
                    c,
                    seed,
                    x,
                    y,
                    z,
                    hrm,
                    vrm,
                    y_scale,
                    yaw,
                    pitch,
                    0,
                    count,
                    if c.kind == CKind::NetherCave {
                        5.0
                    } else {
                        1.0
                    },
                );
            }
        }
    }

    /// `CanyonWorldCarver.carve`.
    fn canyon(
        &mut self,
        g: &mut Gen,
        cc: &ChunkCols,
        c: &CarverDef,
        r: &mut Lcg,
        ox: i32,
        oz: i32,
    ) {
        // `p_carver` refuses a canyon config without a shape, so this cannot be
        // reached with one; the guard keeps the borrow honest.
        let Some(shape) = c.canyon else { return };
        let x = (ox * 16 + r.next_bound(16)) as f64;
        let y = c.y.sample(r, self.min_y, self.height) as f64;
        let z = (oz * 16 + r.next_bound(16)) as f64;
        let yaw = r.next_f32() * TAU;
        let rotation = c.rotation.sample(r);
        let y_scale = c.y_scale.sample(r) as f64;
        let thickness = shape.thickness.sample(r);
        let count = (CARVE_STEPS as f32 * shape.distance.sample(r)) as i32;
        let seed = r.next_i64() as u64;
        self.canyon_tunnel(
            g, cc, c, seed, x, y, z, thickness, yaw, rotation, 0, count, y_scale,
        );
    }

    /// `CanyonWorldCarver.carveTunnel`: the cave walk with a per-height vertical
    /// width profile (`getWidths`) instead of an ellipsoid, and its own `skip`.
    #[expect(clippy::too_many_arguments)]
    fn canyon_tunnel(
        &mut self,
        g: &mut Gen,
        cc: &ChunkCols,
        c: &CarverDef,
        seed: u64,
        mut x: f64,
        mut y: f64,
        mut z: f64,
        thickness: f32,
        mut yaw: f32,
        mut pitch: f32,
        branch_index: i32,
        branch_count: i32,
        y_scale: f64,
    ) {
        let Some(shape) = c.canyon else { return };
        let mut r = Lcg::new(seed);
        let mut widths = canyon_widths(self.height as usize, shape.width_smoothness, &mut r);
        let mut yaw_drift = 0.0f32;
        let mut pitch_drift = 0.0f32;
        for i in branch_index..branch_count {
            let mut d0 = 1.5
                + (mth_sin(std::f32::consts::PI * i as f32 / branch_count as f32) * thickness)
                    as f64;
            let d1 = d0 * y_scale;
            d0 *= shape.horizontal_radius.sample(&mut r) as f64;
            let d1 = canyon_y(shape, &mut r, d1, branch_count as f32, i as f32);
            let cp = mth_cos(pitch);
            let sp = mth_sin(pitch);
            x += (mth_cos(yaw) * cp) as f64;
            y += sp as f64;
            z += (mth_sin(yaw) * cp) as f64;
            pitch *= 0.7;
            pitch += pitch_drift * 0.05;
            yaw += yaw_drift * 0.05;
            pitch_drift *= 0.8;
            yaw_drift *= 0.5;
            pitch_drift += (r.next_f32() - r.next_f32()) * r.next_f32() * 2.0;
            yaw_drift += (r.next_f32() - r.next_f32()) * r.next_f32() * 4.0;
            if r.next_bound(4) == 0 {
                continue;
            }
            if !can_reach(self.tx, self.tz, x, z, i, branch_count, thickness) {
                park_canyon_widths(&mut widths);
                return;
            }
            self.canyon_ellipsoid(g, cc, c, x, y, z, d0, d1, &widths);
        }
        park_canyon_widths(&mut widths);
    }

    /// `CanyonWorldCarver.carveEllipsoid`: the canyon profile is a vertical
    /// stack of widths, so the `skip` test is `(x^2 + z^2) * width + y^2 / 6`.
    #[expect(clippy::too_many_arguments)]
    fn canyon_ellipsoid(
        &mut self,
        g: &mut Gen,
        cc: &ChunkCols,
        c: &CarverDef,
        x: f64,
        y: f64,
        z: f64,
        hr: f64,
        vr: f64,
        widths: &[f32],
    ) -> bool {
        // The iteration range below is clipped to this chunk, so an ellipsoid
        // centred far away contributes nothing; the distance test is only a
        // cheap way to skip those. (Removing it changes no cell: measured
        // 2026-10-22.)
        let mid_x = (self.tx * 16 + 8) as f64;
        let mid_z = (self.tz * 16 + 8) as f64;
        let reach = 16.0 + hr * 2.0;
        if (x - mid_x).abs() > reach || (z - mid_z).abs() > reach {
            return false;
        }
        let (min_x, min_z) = (self.tx * 16, self.tz * 16);
        let x0 = ((x - hr).floor() as i32 - min_x - 1).max(0);
        let x1 = ((x + hr).floor() as i32 - min_x + 15).min(15);
        let y0 = ((y - vr).floor() as i32 - 1).max(self.min_y + 1);
        let y1 = ((y + vr).floor() as i32 + 1).min(self.min_y + self.height - 1 - 7);
        let z0 = ((z - hr).floor() as i32 - min_z - 1).max(0);
        let z1 = ((z + hr).floor() as i32 - min_z + 15).min(15);
        let mut carved = false;
        for lx in x0..=x1 {
            let bx = min_x + lx;
            let nx = (bx as f64 + 0.5 - x) / hr;
            for lz in z0..=z1 {
                let bz = min_z + lz;
                let nz = (bz as f64 + 0.5 - z) / hr;
                if nx * nx + nz * nz >= 1.0 {
                    continue;
                }
                let mut yy = y1;
                while yy > y0 {
                    // vanilla normalizes the y cell centre with `- 0.5`, not the
                    // `+ 0.5` the x and z coordinates use (carveEllipsoid's
                    // `i2d; ldc 0.5; dsub`). Using +0.5 lifts every ellipsoid by
                    // one cell at its top edge.
                    let ny = (yy as f64 - 0.5 - y) / vr;
                    let w = widths
                        .get((yy - self.min_y - 1) as usize)
                        .copied()
                        .unwrap_or(1.0) as f64;
                    if (nx * nx + nz * nz) * w + ny * ny / 6.0 < 1.0 && !self.mask_get(lx, yy, lz) {
                        self.mask_set(lx, yy, lz);
                        carved |= self.carve_block(g, cc, c, bx, yy, bz);
                    }
                    yy -= 1;
                }
            }
        }
        carved
    }

    /// `CaveWorldCarver.carveTunnel`: a walk along (yaw, pitch) that lays an
    /// ellipsoid every step it does not reject, and forks once when `yScale` is
    /// large. The RNG is `RandomSource.create(seed)` — a `LegacyRandomSource`.
    #[expect(clippy::too_many_arguments)]
    fn tunnel(
        &mut self,
        g: &mut Gen,
        cc: &ChunkCols,
        c: &CarverDef,
        seed: u64,
        mut x: f64,
        mut y: f64,
        mut z: f64,
        hrm: f64,
        vrm: f64,
        y_scale: f32,
        mut yaw: f32,
        mut pitch: f32,
        branch_index: i32,
        branch_count: i32,
        ratio: f64,
    ) {
        let mut r = Lcg::new(seed);
        let split = r.next_bound(branch_count / 2) + branch_count / 4;
        let flat = r.next_bound(6) == 0;
        let mut pitch_drift = 0.0f32;
        let mut yaw_drift = 0.0f32;
        for i in branch_index..branch_count {
            let d0 = 1.5
                + (mth_sin(std::f32::consts::PI * i as f32 / branch_count as f32) * y_scale) as f64;
            let d1 = d0 * ratio;
            let cp = mth_cos(pitch);
            x += (mth_cos(yaw) * cp) as f64;
            y += mth_sin(pitch) as f64;
            z += (mth_sin(yaw) * cp) as f64;
            pitch *= if flat { 0.92 } else { 0.7 };
            pitch += pitch_drift * 0.1;
            yaw += yaw_drift * 0.1;
            pitch_drift *= 0.9;
            yaw_drift *= 0.75;
            pitch_drift += (r.next_f32() - r.next_f32()) * r.next_f32() * 2.0;
            yaw_drift += (r.next_f32() - r.next_f32()) * r.next_f32() * 4.0;
            if i == split && y_scale > 1.0 {
                let s1 = r.next_i64() as u64;
                let sc1 = r.next_f32() * 0.5 + 0.5;
                self.tunnel(
                    g,
                    cc,
                    c,
                    s1,
                    x,
                    y,
                    z,
                    hrm,
                    vrm,
                    sc1,
                    yaw - std::f32::consts::FRAC_PI_2,
                    pitch / 3.0,
                    i,
                    branch_count,
                    1.0,
                );
                let s2 = r.next_i64() as u64;
                let sc2 = r.next_f32() * 0.5 + 0.5;
                self.tunnel(
                    g,
                    cc,
                    c,
                    s2,
                    x,
                    y,
                    z,
                    hrm,
                    vrm,
                    sc2,
                    yaw + std::f32::consts::FRAC_PI_2,
                    pitch / 3.0,
                    i,
                    branch_count,
                    1.0,
                );
                return;
            } else if r.next_bound(4) != 0 {
                if !can_reach(self.tx, self.tz, x, z, i, branch_count, y_scale) {
                    return;
                }
                self.ellipsoid(g, cc, c, x, y, z, d0 * hrm, d1 * vrm);
            }
        }
    }

    /// `WorldCarver.carveEllipsoid`: clips the ellipsoid to this chunk and carves
    /// every cell inside it that the mask has not claimed and `skip` accepts.
    // the parameters are `WorldCarver.carveEllipsoid`'s own inputs
    #[expect(clippy::too_many_arguments)]
    fn ellipsoid(
        &mut self,
        g: &mut Gen,
        cc: &ChunkCols,
        c: &CarverDef,
        x: f64,
        y: f64,
        z: f64,
        hr: f64,
        vr: f64,
    ) -> bool {
        // The iteration range below is clipped to this chunk, so an ellipsoid
        // centred far away contributes nothing; the distance test is only a
        // cheap way to skip those. (Removing it changes no cell: measured
        // 2026-10-22.)
        let mid_x = (self.tx * 16 + 8) as f64;
        let mid_z = (self.tz * 16 + 8) as f64;
        let reach = 16.0 + hr * 2.0;
        if (x - mid_x).abs() > reach || (z - mid_z).abs() > reach {
            return false;
        }
        let (min_x, min_z) = (self.tx * 16, self.tz * 16);
        let x0 = ((x - hr).floor() as i32 - min_x - 1).max(0);
        let x1 = ((x + hr).floor() as i32 - min_x + 15).min(15);
        let y0 = ((y - vr).floor() as i32 - 1).max(self.min_y + 1);
        // fresh worlds have no below-zero retrogen, so the top stops 7 short
        let y1 = ((y + vr).floor() as i32 + 1).min(self.min_y + self.height - 1 - 7);
        let z0 = ((z - hr).floor() as i32 - min_z - 1).max(0);
        let z1 = ((z + hr).floor() as i32 - min_z + 15).min(15);
        let mut carved = false;
        for lx in x0..=x1 {
            let bx = min_x + lx;
            let nx = (bx as f64 + 0.5 - x) / hr;
            for lz in z0..=z1 {
                let bz = min_z + lz;
                let nz = (bz as f64 + 0.5 - z) / hr;
                if nx * nx + nz * nz >= 1.0 {
                    continue;
                }
                let mut yy = y1;
                while yy > y0 {
                    // vanilla normalizes the y cell centre with `- 0.5`, not the
                    // `+ 0.5` the x and z coordinates use (carveEllipsoid's
                    // `i2d; ldc 0.5; dsub`). Using +0.5 lifts every ellipsoid by
                    // one cell at its top edge.
                    let ny = (yy as f64 - 0.5 - y) / vr;
                    // `skip`: at or below the floor level, or outside the sphere
                    if ny > self.floor
                        && nx * nx + ny * ny + nz * nz < 1.0
                        && !self.mask_get(lx, yy, lz)
                    {
                        self.mask_set(lx, yy, lz);
                        carved |= self.carve_block(g, cc, c, bx, yy, bz);
                    }
                    yy -= 1;
                }
            }
        }
        carved
    }

    /// `WorldCarver.carveBlock`: replace a `#overworld_carver_replaceables` cell
    /// with the aquifer's substance (lava below the lava level), and restore the
    /// top material under a cave mouth that was opened through the surface.
    fn carve_block(
        &mut self,
        g: &mut Gen,
        cc: &ChunkCols,
        c: &CarverDef,
        x: i32,
        y: i32,
        z: i32,
    ) -> bool {
        // The ``read`` below needs the level up front for the prefetch gate (see
        // `CarveRun::read`): the prefetched rows carry the state but not the aquifer's flag,
        // so they can only answer above it.
        self.lava = c.lava.resolve(self.min_y, self.height);
        let cur = self.read(g, cc, x, y, z);
        let br = match breg() {
            Some(br) => br,
            None => return false,
        };
        let reached_surface = cur == self.grass_state || cur == self.podzol_state;
        if !is_replaceable(c, br, cur) {
            return false;
        }
        let lava_level = self.lava;
        let probed = if c.kind == CKind::NetherCave {
            Some(if y <= self.min_y + 31 {
                self.probe_lava
            } else {
                self.probe_air
            })
        } else if self.probe {
            let lava = self.probe_lava;
            Some(if y <= lava_level {
                lava
            } else {
                self.probe_air
            })
        } else {
            g.carve_state(x, y, z, lava_level)
        };
        let Some(st) = probed else {
            return false;
        };
        // vanilla writes with the *target* chunk and lets the position fold
        // (`x & 15`), so a tunnel from a source chunk up to 8 chunks away paints
        // its cells into the target chunk at folded coordinates. Keep that as
        // the default; MEGU_NOFOLD=1 clips instead, for experiments.
        if self.nofold && ((x >> 4) != self.tx || (z >> 4) != self.tz) {
            return false;
        }
        if self.mark.is_none() {
            self.mark = Some((x, y, z));
        }
        self.put(x, y, z, st);
        // `WorldCarver.apply`'s first mark site: `if (aquifer.shouldScheduleFluidUpdate() &&
        // !state.getFluidState().isEmpty()) chunk.markPosForPostprocessing(pos)`. The gate is
        // the aquifer's flag from the `getCarveState` call above and the state *it* returned,
        // not the state the write folded onto.
        if !self.probe && g.aq_schedule_fluid_update() && has_fluid_state(br, st) {
            // Folded onto the chunk being carved: vanilla stores the mark as the packed local
            // offset on `chunk` (the target), so a far-chunk carve marks the cell it folds to.
            self.scratch
                .shape_marks
                .push((self.tx * 16 + (x & 15), y, self.tz * 16 + (z & 15)));
        }
        if reached_surface && self.read(g, cc, x, y - 1, z) == self.dirt_state {
            let fluid = g.is_fluid_state(st);
            if let Some(top) = g.top_material_at(x, y - 1, z, fluid) {
                self.put(x, y - 1, z, top);
                // Second site: the restored top material is marked when its own fluid state is
                // non-empty (no aquifer gate — vanilla asks the restored state only).
                if !self.probe && has_fluid_state(br, top) {
                    self.scratch.shape_marks.push((
                        self.tx * 16 + (x & 15),
                        y - 1,
                        self.tz * 16 + (z & 15),
                    ));
                }
            }
        }
        true
    }
}

/// `WorldCarver.canReach`: stop walking a tunnel once it is so far from the
/// chunk and so late in its run that it cannot come back.
fn can_reach(
    tx: i32,
    tz: i32,
    x: f64,
    z: f64,
    branch_index: i32,
    branch_count: i32,
    y_scale: f32,
) -> bool {
    let dx = x - (tx * 16 + 8) as f64;
    let dz = z - (tz * 16 + 8) as f64;
    let left = (branch_count - branch_index) as f64;
    let reach = y_scale as f64 + 2.0 + 16.0;
    dx * dx + dz * dz - left * left <= reach * reach
}

// ---------- entry point ----------

fn pop_seed(seed: u64, bx: i32, bz: i32) -> u64 {
    // `WorldgenRandom.setDecorationSeed` (dzx.a(JII)J) runs on the *Xoroshiro*
    // source that `applyBiomeDecoration` wraps, not on an LCG, and combines the
    // two products with XOR:
    //   setSeed(seed); l = nextLong() | 1; m = nextLong() | 1;
    //   n = (x * l + z * m) ^ seed;      // products *added*, then XOR the seed
    let mut r = Xor::seed_worldgen(seed);
    let l = r.next_i64_legacy() | 1;
    let m = r.next_i64_legacy() | 1;
    let n = ((bx as i64)
        .wrapping_mul(l)
        .wrapping_add((bz as i64).wrapping_mul(m))
        ^ seed as i64) as u64;
    if env_flag!("POP_DBG") {
        eprintln!("POPDBG seed={seed} bx={bx} bz={bz} l={l} m={m} -> {n}");
    }
    n
}

/// populate chunk (sx,sz): evaluate the vanilla feature pipeline and merge
/// block-state overrides into Gen::feat_c.
/// Env-gated per-placed-feature timing, so the feature-population hotspot can be attributed to a
/// named feature instead of guessed. No behaviour change; the map is only written when
/// `FEAT_PROFILE` is set.
pub(crate) static FEAT_PROF: std::sync::OnceLock<std::sync::Mutex<HashMap<String, (u128, u64)>>> =
    std::sync::OnceLock::new();
pub(crate) fn feat_prof_record(name: &str, ns: u128) {
    let m = FEAT_PROF.get_or_init(|| std::sync::Mutex::new(HashMap::new()));
    let mut g = m.lock().unwrap_or_else(|e| e.into_inner());
    let e = g.entry(name.to_string()).or_insert((0, 0));
    e.0 += ns;
    e.1 += 1;
}
/// Dumps the feature-profile counters the test-gated probes fill.
#[cfg(test)]
#[cfg_attr(not(megu_vanilla_data), allow(dead_code))]
pub(crate) fn feat_prof_dump() {
    let Some(m) = FEAT_PROF.get() else { return };
    let m = m.lock().unwrap_or_else(|e| e.into_inner());
    let mut v: Vec<(String, u128, u64)> = m.iter().map(|(k, (t, c))| (k.clone(), *t, *c)).collect();
    v.sort_by_key(|x| std::cmp::Reverse(x.1));
    let total: u128 = v.iter().map(|x| x.1).sum();
    eprintln!("FEATPROF total={:.1}ms", total as f64 / 1e6);
    let scans = crate::generation_context::HM_SCAN_N.load(std::sync::atomic::Ordering::Relaxed);
    let cells = crate::generation_context::HM_SCAN_CELLS.load(std::sync::atomic::Ordering::Relaxed);
    eprintln!("FEATPROF wg-heightmap scans={scans} cells={cells}");
    eprintln!(
        "FEATPROF terrain reads={:.1}ms n={} ({:.0} ns/call)",
        crate::generation_context::READ_NS.load(std::sync::atomic::Ordering::Relaxed) as f64 / 1e6,
        crate::generation_context::READ_N.load(std::sync::atomic::Ordering::Relaxed),
        crate::generation_context::READ_NS.load(std::sync::atomic::Ordering::Relaxed) as f64
            / crate::generation_context::READ_N
                .load(std::sync::atomic::Ordering::Relaxed)
                .max(1) as f64
    );
    for (name, ns, cnt) in v.iter().take(25) {
        eprintln!("FEATPROF {:8.2}ms n={cnt:5}  {name}", *ns as f64 / 1e6);
    }
}

/// Populate one chunk (and the 3x3 around it, as a feature read would) so that a
/// diagnostic sees the same feature-layer state a generation loop would.
pub(crate) fn populate(g: &mut Gen, sx: i32, sz: i32) {
    // A3-a: `Gen::populate_in_order` hands a multi-chunk feature run to us through `Gen`
    // (it cannot call this layer directly). The first call drives the whole batch, so the
    // remaining calls in its serial loop are no-ops on `pop_done`.
    if let Some(pending) = g.a3a_dispatch.take() {
        if !populate_diag_env() {
            let jobs = crate::worldgen::terrain_jobs();
            precarve_order(g, &pending, jobs);
            if pending.len() >= 4 {
                let _ = populate_parallel(g, &pending, jobs);
                return;
            }
        }
    }
    populate_body(g, sx, sz);
}

fn populate_body(g: &mut Gen, sx: i32, sz: i32) {
    // Vanilla's chunk status order is CARVERS then FEATURES, and both write into
    // the chunk's block state, so the carver pass runs here — before the feature
    // gate, because "no features" is a comparison fixture, not a carver switch
    // (`MEGU_NOCARVE` is). Vanilla carves the whole decoration area before
    // decorating the centre, so a feature may read a neighbour's carver air; the
    // `begin_carve` claim makes the later per-chunk populate a no-op for its
    // carver stage. `carve_ring` carves that 3x3, on one thread or across the
    // terrain workers.
    carve_ring(g, sx, sz);
    // Diagnostic: MEGU_NOFEAT=1 skips the whole feature layer so a block diff can
    // attribute the remainder to terrain or to features.
    if env_flag!("MEGU_NOFEAT") {
        return;
    }
    let fr = freg();
    let order = fr.order.get(&g.preset).expect("dimension feature order");
    if breg().is_none() {
        return;
    }
    // biomes of the 3x3 neighborhood (chunk center at y=0, quart sample)
    // Vanilla unions the *biome palettes of this chunk's sections*
    // (`applyBiomeDecoration` collects `section.getBiomes()` into a set), so a
    // feature runs if any 4x4x4 cell of the chunk carries its biome. Sampling a few
    // columns at y=0 instead reads the cave biomes and misses a surface biome such as
    // `river` - which silently skipped `trees_water` (step 9 index 48) in chunk
    // (-2,1) and left the canopies of its trees missing here (measured: the draw
    // streams for that chunk's trees_birch_and_oak then diverged at draw 120).
    // Zero-allocation 3x3 biome union: map each biome name straight to its
    // precomputed bit. The legacy `Vec<String>` is built only under the
    // defensive >128-biome fallback, so the normal path clones no names.
    let use_bits = fr.bit_ok;
    let mut present_mask: u128 = 0;
    let mut biomes: Vec<String> = Vec::new();
    for dx in -1..=1 {
        for dz in -1..=1 {
            for b in g.chunk_biome_set(sx + dx, sz + dz).iter() {
                if use_bits {
                    if let Some(&bit) = fr.biome_bits.get(b) {
                        present_mask |= 1u128 << bit;
                    }
                } else if !biomes.iter().any(|v| v == b) {
                    biomes.push(b.clone());
                }
            }
        }
    }
    if let Some(spec) = env_str!("FEAT_LIST") {
        if let Ok(st) = spec.parse::<usize>() {
            if let Some(l) = order.get(st) {
                for (gi, name) in l.iter() {
                    eprintln!("FEATLIST {st} {gi} {name}");
                }
            }
            return;
        }
    }
    let pop = pop_seed(g.wseed(), sx * 16, sz * 16);
    // Pass marker, matching the agent's `SPASS` (a hook on `ChunkGenerator.applyBiomeDecoration`):
    // the native side logs a chunk's placements under the nine base positions of its 3x3 biome
    // sources, so only the pass marker makes the two sides' attempt lists comparable.
    if env_str!("FEATURE_ATTEMPT_TRACE").is_some() {
        eprintln!("SPASS {sx} {sz}");
    }
    crate::generation_math::CUR_POP_CHUNK.store(
        ((sx as i64) << 32) | (sz as i64 & 0xFFFF_FFFF),
        std::sync::atomic::Ordering::Relaxed,
    );
    // The write window this pass owns: `WorldGenRegion`'s `blockStateWriteRadius` is 1
    // chunk at the FEATURES step. See `Gen::feature_write_allowed` for the refusal and
    // for the chunk-level half of vanilla's rule that is deliberately not modelled.
    g.pop_active = Some((sx, sz));
    let preset = g.preset;
    let mut structures = References::collect(g, sx, sz);
    let mut ctx = FC::new(g);
    // Activate the thread-local leaf -> bit cache for this pass. Every
    // `mod_positions` call below runs on this same thread, so at most a
    // handful of leaf names are resolved per chunk and the rest of the
    // `PM::Biome` checks are a single array index plus an integer AND.
    LEAF_BITS.with(|c| {
        let mut c = c.borrow_mut();
        c.active = true;
        c.present_mask = present_mask;
        c.known = 0;
    });
    let trace = env_flag!("FEAT_TRACE");
    let profile = env_flag!("FEAT_PROFILE");
    // Hoisted out of the per-feature loop: the env macros cache their answer,
    // but a hoisted `Option<&str>` avoids even the static load on every slot.
    let attempt_trace = env_str!("FEATURE_ATTEMPT_TRACE");
    let fseed_arm = env_str!("FSEED_ARM");
    let fseed_trace = env_str!("FSEED_TRACE");
    let cell_trace = env_str!("FEATURE_CELL_TRACE").is_some();
    // M3 P1a M5: the decoration steps only, without the carver stage above or the promotion
    // and write-out below. Load-dependent by construction (it is a stopwatch), which is why
    // the P1a conclusion rests on the M1-M4 counts and this only splits the remainder.
    let feat_t0 = crate::worldgen::m3_start();
    for step in 0..order.len().max(structures.step_count()) {
        // Structures have steps even when the biome has no placed features.
        // Their registry slots and RNG streams are independent.
        structures.place_step(&mut ctx, step, pop, sx, sz);
        let Some(entries) = order.get(step) else {
            continue;
        };
        let bit_entries = fr.step_entries.get(&preset).and_then(|v| v.get(step));
        for (ei, (gi, name)) in entries.iter().enumerate() {
            let slot = bit_entries.map(|v| &v[ei]);
            let present = if use_bits {
                slot.is_some_and(|s| (present_mask & s.step_biome_mask) != 0)
            } else {
                biomes.iter().any(|b| {
                    fr.bsteps
                        .get(b)
                        .and_then(|s| s.get(step))
                        .map(|l| l.iter().any(|f| f == name))
                        .unwrap_or(false)
                })
            };
            if !present {
                continue;
            }
            let pf: &Pf = if use_bits {
                let idx = slot.expect("step entry parallel to order").placed_idx;
                if idx == usize::MAX {
                    continue;
                }
                &fr.placed_vec[idx]
            } else {
                let Some(p) = fr.placed.get(name) else {
                    continue;
                };
                p
            };
            // Feature-attempt trace, for comparing the *placement pipeline* with the native side
            // (`SPLACE` from the agent's `ebq.a` hook): one line per (chunk, placed feature) in
            // the order they are attempted, which is what localises an RNG-stream divergence to
            // the first feature that differs. `FEATURE_ATTEMPT_TRACE=1`.
            if attempt_trace.is_some() {
                eprintln!("SATTEMPT {name} {sx} {sz} {gi} {step}");
            }
            let deco = pop
                .wrapping_add(*gi as u64)
                .wrapping_add(10_000u64.wrapping_mul(step as u64));
            CUR_FEAT.store(
                ((*gi as i64) << 32) | step as i64,
                std::sync::atomic::Ordering::Relaxed,
            );
            crate::generation_math::XTRACE_FEAT.store(
                ((*gi as i64) << 32) | step as i64,
                std::sync::atomic::Ordering::Relaxed,
            );
            // feature placement sees a `WorldgenRandom` (legacy draw semantics)
            let mut rng = Xor::seed_worldgen(deco);
            if let Some(spec) = fseed_arm {
                let arm = if spec
                    .split(';')
                    .any(|want| want == format!("{sx},{sz},{gi},{step}"))
                {
                    0
                } else {
                    -1
                };
                crate::generation_math::XTRACE.store(arm, std::sync::atomic::Ordering::Relaxed);
                crate::generation_math::NTRACE.store(arm, std::sync::atomic::Ordering::Relaxed);
                crate::generation_math::FTRACE.store(arm, std::sync::atomic::Ordering::Relaxed);
                crate::generation_math::DTRACE.store(arm, std::sync::atomic::Ordering::Relaxed);
            }
            if let Some(spec) = fseed_trace {
                if let Some((a, b)) = spec.split_once(',') {
                    if a.parse::<i32>() == Ok(sx) && b.parse::<i32>() == Ok(sz) {
                        eprintln!("FSEED {pop} {gi} {step}");
                        eprintln!("FNAME {gi} {step} {name}");
                    }
                }
            }
            let before = ctx.wr.len();
            let t0 = if profile {
                Some(std::time::Instant::now())
            } else {
                None
            };
            if cell_trace {
                eprintln!("FCTX {sx} {sz} {gi} {step} {name}");
            }
            if use_bits {
                let mask = slot.expect("step entry parallel to order").ball_mask;
                gen_pf_with_mask(pf, mask, fr, &mut ctx, &mut rng, (sx * 16, 0, sz * 16));
            } else {
                gen_pf(pf, fr, &mut ctx, &mut rng, (sx * 16, 0, sz * 16));
            }
            if let Some(t0) = t0 {
                feat_prof_record(name, t0.elapsed().as_nanos());
            }
            if trace {
                let mut h: u64 = 0;
                for (k, v) in ctx.wr.iter() {
                    h ^= (k.0 as u64)
                        .wrapping_mul(31)
                        .wrapping_add(k.1 as u64)
                        .rotate_left(7)
                        ^ (k.2 as u64)
                        ^ (*v as u64).wrapping_mul(131);
                }
                eprintln!(
                    "TRACE step={} feat={} +{} writes h={:016x}",
                    step,
                    name,
                    ctx.wr.len() - before,
                    h
                );
            }
        }
    }
    // `ChunkMap`'s load path promotes the chunks of this pass' 3x3 once their own 3x3 has
    // reached `FULL` and `LevelChunk.postProcessGeneration` then consumes the marks a
    // `WorldGenRegion` write left on them: a `StructurePiece.placeBlock` cell in
    // `SHAPE_CHECK_BLOCKS` and a cell whose written state passed `hasPostProcess` (mushrooms,
    // magma block). That is why a mineshaft fence picks up `east`/`west` from the pillars
    // around it while the single bit it was written with is dropped, and why a mushroom a later
    // fire lit becomes air. See `promote_full_chunks` for the boundary itself and the timing.
    crate::worldgen::m3_elapsed(
        crate::worldgen::M3_FEATURE_NS,
        crate::worldgen::M3_FEATURE_N,
        feat_t0,
    );
    promote_full_chunks(&mut ctx, sx, sz);
    // Taken before the writes because `ctx` borrows the generator; the containers ride
    // the same population and move into the runtime store as soon as the lock is released.
    let mut staged_payloads = std::mem::take(&mut ctx.payloads);
    for ((x, y, z), st) in std::mem::take(&mut ctx.wr) {
        g.feat_write(x, y, z, st);
    }
    g.stage_payloads((sx, sz), &mut staged_payloads);
    LEAF_BITS.with(|c| c.borrow_mut().active = false);
}

/// The chunk-promotion boundary: `ChunkMap`'s load path (`getChunkRangeFuture(pos, 1, FULL)`
/// then `LevelChunk.postProcessGeneration`) for every chunk of the demand region whose 3x3 has
/// reached `FULL`.
///
/// `pop_done` is the port's status table and `promotion_window_ready` is "the whole 3x3 is
/// `FULL`", which is the condition `ChunkMap` asks for over radius one, so a chunk's marks are
/// complete the moment this loop sees it. The marks are flushed to their owning chunks first
/// (`markPosForPostprocessing` files on `getChunk(pos)`, not on the chunk being generated),
/// then consumed once, in section order then mark order.
///
/// The demand region itself is `promotion_region_contains` (`(n+4)²` for an `(n+2)²` plan
/// window): native's `FULL` region is the request plus two rings, one further out than the
/// plan keeps, so the outer ring of this loop is exactly the chunks native promoted outside the
/// recorded input. A chunk in that ring with no data is skipped by its empty mark bucket.
///
/// `pop_active` is dropped here, before anything is rewritten, because `postProcessGeneration`
/// runs on a promoted `LevelChunk`, not inside a `WorldGenRegion`: a write from the pass must
/// not file a mark (`FC::set`'s `hasPostProcess` gate reads this window) and must not be bounded
/// by the FEATURES step's write radius. That is the whole reason this is a function of its own
/// and not a tail inside the feature loop.
/// `Feature.markAboveForPostProcessing` (`ece.a(Ldds;Ljd;)V`): steps `UP` twice
/// (`y + 1`, `y + 2`), stopping on the first air cell (or outside build height, where
/// `WorldGenRegion.getBlockState` returns `VOID_AIR`), and marks each visited non-air cell on its
/// owning chunk with `MarkSrc::FeatureAbove`.
fn mark_above_for_post_processing(ctx: &mut FC<'_>, reg: &BReg, x: i32, y: i32, z: i32) {
    for dy in 1..=2 {
        let cy = y + dy;
        if ctx.out_of_h(cy) {
            return;
        }
        let st = ctx.get(x, cy, z);
        if reg.is_air(st) {
            return;
        }
        ctx.g
            .store_shape_mark_from(x, cy, z, crate::worldgen::MarkSrc::FeatureAbove);
    }
}

fn promote_full_chunks(ctx: &mut FC<'_>, sx: i32, sz: i32) {
    for (x, y, z) in std::mem::take(&mut ctx.shape_marks) {
        ctx.g
            .note_mark_source(x, y, z, crate::worldgen::MarkSrc::Write);
        ctx.g.store_shape_mark(x, y, z);
    }
    ctx.g.pop_active = None;
    let Some(reg) = breg() else { return };
    for dx in -1..=1 {
        for dz in -1..=1 {
            let (cx, cz) = (sx + dx, sz + dz);
            if !ctx.g.promotion_region_contains(cx, cz) {
                continue;
            }
            if !ctx.g.promotion_window_ready(cx, cz) {
                continue;
            }
            // Promotion is a chunk-status transition even when this chunk carries no
            // post-processing marks. Record it independently of feature completion so a
            // later FEATURES pass cannot write through vanilla's imposter proto chunk.
            ctx.g.mark_chunk_promoted(cx, cz);
            let mut marks = ctx.g.take_shape_marks(cx, cz);
            if marks.is_empty() {
                continue;
            }
            // `MARK_TRACE=<cx>,<cz>|all`: the pass' input, one line per marked cell with the
            // writer that filed it. This is the port-side counterpart of the reference's
            // RUNTIME lines: the same window files 106 marks there, and the breakdown by
            // source is what says whether a missing mark is a missing *site* or a cell the
            // site is not reached for.
            let mark_trace = crate::trace_env::env_str!("MARK_TRACE")
                .is_some_and(|want| want == "all" || want == format!("{cx},{cz}"));
            if mark_trace {
                eprintln!("MARKPASS {cx} {cz} marks={}", marks.len());
                for &(x, y, z) in &marks {
                    eprintln!(
                        "MARK {x} {y} {z} chunk={cx},{cz} src={}",
                        ctx.g.mark_source_of(x, y, z)
                    );
                }
            }
            // `ProtoChunk.markPosForPostprocessing` buckets by section and native walks the
            // buckets in ascending order; `sort_by_key` is stable, so a bucket keeps mark order.
            let min_y = ctx.g.min_y;
            marks.sort_by_key(|&(_, y, _)| (y - min_y) >> 4);
            // The chunk is FULL here, i.e. `INITIALIZE_LIGHT`/`LIGHT` have run in vanilla: sample
            // the light the shape rules read before the pass rewrites anything.
            ctx.begin_promotion_pass(cx, cz, &marks);
            crate::structure_template::postprocess_shape_marks(ctx, reg, &marks);
            // The consumed bucket goes back to the generator's pool so the next
            // chunk that files marks reuses the allocation.
            ctx.g.recycle_mark_bucket(marks);
        }
    }
    ctx.end_promotion_pass();
}

#[cfg(test)]
mod mark_above_tests {
    #[cfg(megu_vanilla_data)]
    use super::{freg, gen_cf, Cf};
    #[cfg(megu_vanilla_data)]
    use crate::block_registry::breg;
    #[cfg(megu_vanilla_data)]
    use crate::generation_context::FC;
    #[cfg(megu_vanilla_data)]
    use crate::generation_math::Xor;
    #[cfg(megu_vanilla_data)]
    use crate::json::jparse;
    #[cfg(megu_vanilla_data)]
    use crate::worldgen::Gen;

    /// `DiskFeature` calls `Feature.markAboveForPostProcessing` (`ece.a`), which walks `y+1`,
    /// `y+2`, stops at the first air cell (or outside build height), and files marks on the
    /// owning chunk.
    #[test]
    #[cfg(megu_vanilla_data)]
    fn disk_feature_marks_above_for_post_processing_stopping_on_air() {
        let reg = breg().expect("block registry");
        let fr = freg();
        let mut g = Gen::load(0, crate::name_to_state).expect("gen");
        let mut ctx = FC::new(&mut g);
        ctx.test_default_air = true;

        let sand = reg.state_of("minecraft:sand", None);
        let stone = reg.state_of("minecraft:stone", None);
        let fern = reg.state_of("minecraft:fern", None);

        // Column (0, _, 0): stone at y=69 (above disk target y=68 -> both 69 and 70 marked),
        // fern at y=70. Disk will replace y=68 with gravel and mark y=69 and y=70.
        ctx.set(0, 68, 0, sand);
        ctx.set(0, 69, 0, stone);
        ctx.set(0, 70, 0, fern);

        // Column (1, _, 0): disk writes y=68, y=69 is air, y=70 is fern — stopping on air at y=69
        // must prevent marking y=70.
        ctx.set(1, 68, 0, sand);
        ctx.set(1, 70, 0, fern);

        let disk_json = jparse(
            br#"{
                "type": "minecraft:disk",
                "config": {
                    "half_height": 0,
                    "radius": 1,
                    "state_provider": {
                        "fallback": {
                            "type": "minecraft:simple_state_provider",
                            "state": { "Name": "minecraft:gravel" }
                        },
                        "rules": []
                    },
                    "target": {
                        "type": "minecraft:matching_blocks",
                        "blocks": ["minecraft:sand"]
                    }
                }
            }"#,
        )
        .expect("parse disk config");
        let cf: Cf = super::p_cf(&disk_json).expect("disk Cf");
        let mut rng = Xor::seed_worldgen(1);
        gen_cf(&cf, fr, &mut ctx, &mut rng, (0, 68, 0));

        let marks = ctx.g.take_shape_marks(0, 0);
        assert!(
            marks.contains(&(0, 69, 0)) && marks.contains(&(0, 70, 0)),
            "disk at (0,68,0) must mark both (0,69,0) and (0,70,0), got {marks:?}"
        );
        assert!(
            !marks.contains(&(1, 70, 0)),
            "air at (1,69,0) must stop markAboveForPostProcessing before (1,70,0), got {marks:?}"
        );
    }

    /// `LakeFeature` (`ect.a`) calls `markAboveForPostProcessing` at two sites: after placing
    /// `CAVE_AIR` (`y >= 4`, NOT for `y < 4` fluid) and after placing the `barrier` block.
    #[test]
    #[cfg(megu_vanilla_data)]
    fn lake_feature_marks_above_at_cave_air_and_barrier_sites() {
        let reg = breg().expect("block registry");
        let fr = freg();
        let mut g = Gen::load(0, crate::name_to_state).expect("gen");
        let mut ctx = FC::new(&mut g);
        ctx.test_default_air = true;

        let stone = reg.state_of("minecraft:stone", None);
        let dirt_path = reg.state_of("minecraft:dirt_path", None);
        let fern = reg.state_of("minecraft:fern", None);
        let lava = reg.state_of("minecraft:lava", None);
        let cave_air = reg.state_of("minecraft:cave_air", None);

        // Anchor is (pos.0, pos.1 - 4, pos.2) = (0, 60, 0) for pos = (0, 64, 0).
        // Fill 16x10x16 stone volume around (0..16, 60..69, 0..16) with dirt_path at y=68 and
        // ferns at y=69 so both cave_air carves (anchor.y+4..7) and barrier writes mark ferns above.
        for x in 0..16 {
            for z in 0..16 {
                for y in 60..=67 {
                    ctx.set(x, y, z, stone);
                }
                ctx.set(x, 68, z, dirt_path);
                ctx.set(x, 69, z, fern);
            }
        }

        let lake_json = jparse(
            br#"{
                "type": "minecraft:lake",
                "config": {
                    "fluid": {
                        "type": "minecraft:simple_state_provider",
                        "state": { "Name": "minecraft:lava", "Properties": { "level": "0" } }
                    },
                    "barrier": {
                        "type": "minecraft:simple_state_provider",
                        "state": { "Name": "minecraft:mossy_cobblestone" }
                    }
                }
            }"#,
        )
        .expect("parse lake config");
        let cf: Cf = super::p_cf(&lake_json).expect("lake Cf");
        let mut rng = Xor::seed_worldgen(4242);
        gen_cf(&cf, fr, &mut ctx, &mut rng, (0, 64, 0));

        let mossy = reg.state_of("minecraft:mossy_cobblestone", None);
        let marks = ctx.g.take_shape_marks(0, 0);

        // Find at least one barrier write (mossy_cobblestone) that has non-air above at y+1/y+2,
        // and verify those cells were marked by the barrier site.
        let mut checked_barrier = false;
        let mut checked_cave_air = false;
        let mut checked_fluid = false;
        for x in 0..16 {
            for z in 0..16 {
                for y in 60..=68 {
                    let st = ctx.get(x, y, z);
                    if st == mossy && !reg.is_air(ctx.get(x, y + 1, z)) {
                        assert!(
                            marks.contains(&(x, y + 1, z)),
                            "lake barrier at ({x},{y},{z}) must mark ({x},{},{z})",
                            y + 1
                        );
                        checked_barrier = true;
                    }
                    if st == cave_air && y >= 64 && !reg.is_air(ctx.get(x, y + 1, z)) {
                        assert!(
                            marks.contains(&(x, y + 1, z)),
                            "lake cave_air (y>=4) at ({x},{y},{z}) must mark ({x},{},{z})",
                            y + 1
                        );
                        checked_cave_air = true;
                    }
                    // Fluid cell at y=63 (grid y=3 < 4) whose cell above (y=64) stayed stone (not
                    // cave_air and not barrier) must NOT have been marked by the fluid write.
                    if st == lava && y == 63 && ctx.get(x, 64, z) == stone {
                        checked_fluid = true;
                        assert!(
                            !marks.contains(&(x, 64, z)),
                            "lake fluid (y<4) at ({x},{y},{z}) must not mark ({x},64,{z})"
                        );
                    }
                }
            }
        }
        assert!(
            checked_barrier,
            "expected a lake barrier cell with non-air above"
        );
        assert!(
            checked_cave_air,
            "expected a lake cave_air cell with non-air above"
        );
        let _ = checked_fluid;
    }
}

#[cfg(all(test, megu_vanilla_data))]
mod carver_parity_tests {
    use super::{apply_carvers, build_origin_grid, carve_ring_with, resolve_origin, OriginGrid};
    use crate::worldgen::{lock_gen_for_tests, ChunkCols, Gen};

    /// Everything the carver stage hands an owner for one chunk: the block writes, both WG
    /// heightmaps over all 256 columns, and the marks in the order they were filed.
    type Snapshot = (Vec<((i32, i32, i32), u32)>, Vec<i32>, Vec<(i32, i32, i32)>);
    type RingSnapshot = (
        i32,
        i32,
        Vec<((i32, i32, i32), u32)>,
        Vec<i32>,
        Vec<(i32, i32, i32)>,
    );

    fn snapshot(g: &mut Gen, cx: i32, cz: i32) -> Snapshot {
        let mut writes: Vec<((i32, i32, i32), u32)> = g.feature_bucket_writes(cx, cz).collect();
        writes.sort();
        let mut wg = Vec::with_capacity(512);
        for ocean_floor in [false, true] {
            for lz in 0..16 {
                for lx in 0..16 {
                    let x = cx * 16 + lx;
                    let z = cz * 16 + lz;
                    wg.push(
                        g.wg_after_carve_height(ocean_floor, x, z)
                            .unwrap_or(i32::MIN),
                    );
                }
            }
        }
        let marks = g.take_shape_marks(cx, cz);
        (writes, wg, marks)
    }

    fn ring_snapshots(g: &mut Gen, sx: i32, sz: i32) -> Vec<RingSnapshot> {
        let mut out = Vec::with_capacity(9);
        for dz in -1..=1 {
            for dx in -1..=1 {
                let (cx, cz) = (sx + dx, sz + dz);
                let (writes, wg, marks) = snapshot(g, cx, cz);
                out.push((cx, cz, writes, wg, marks));
            }
        }
        out
    }

    /// The `3x3` ring carved across workers must leave the owner in exactly the state the
    /// serial ring leaves it in: same writes, same WG heightmaps, same mark order.
    ///
    /// The two arms are the same function with the worker count changed, so this is the
    /// schedule being compared and nothing else (the environment is read once per call site and
    /// cannot be re-pointed inside a process, which is why the count is a parameter).
    #[test]
    fn m3_p3a_carver_serial_vs_parallel_bit_identical() {
        crate::test_support::require_vanilla_data();
        let _gen = lock_gen_for_tests();
        let (sx, sz) = (0, 0);

        let workers_before = super::CARVE_RING_WORKERS.load(std::sync::atomic::Ordering::Relaxed);
        let mut serial = Gen::load(0, crate::name_to_state).expect("datapack");
        carve_ring_with(&mut serial, sx, sz, 1);
        let serial_workers =
            super::CARVE_RING_WORKERS.load(std::sync::atomic::Ordering::Relaxed) - workers_before;

        let mut parallel = Gen::load(0, crate::name_to_state).expect("datapack");
        carve_ring_with(&mut parallel, sx, sz, 4);
        let parallel_workers =
            super::CARVE_RING_WORKERS.load(std::sync::atomic::Ordering::Relaxed) - workers_before;

        assert_eq!(
            serial_workers, 0,
            "one worker is the serial arm: it must not spawn anything"
        );
        assert!(
            parallel_workers >= 2,
            "the parallel arm must actually spawn workers, or the comparison proves nothing \
             (it spawned {parallel_workers})"
        );

        let a = ring_snapshots(&mut serial, sx, sz);
        let b = ring_snapshots(&mut parallel, sx, sz);
        let writes: usize = a.iter().map(|s| s.2.len()).sum();
        assert!(
            writes > 0,
            "the fixture must carve something before its identity means anything"
        );
        for (x, y) in a.iter().zip(&b) {
            assert_eq!(x.0, y.0);
            assert_eq!(x.1, y.1);
            assert_eq!(
                x.2, y.2,
                "chunk {},{}: the carve writes differ between the serial and the parallel ring",
                x.0, x.1
            );
            assert_eq!(
                x.3, y.3,
                "chunk {},{}: the post-carve WG heightmaps differ",
                x.0, x.1
            );
            assert_eq!(x.4, y.4, "chunk {},{}: the mark order differs", x.0, x.1);
        }
    }

    /// The two gates on the prefetched-cell fast path, pinned one at a time: a position outside
    /// this chunk, and a cell at or below the carver's lava level.
    ///
    /// Neither is a cost decision. The write folds an out-of-chunk position with `& 15` but the
    /// terrain read does not (it hands `terrain_cell` the unfolded position), so answering one
    /// from this chunk's rows would substitute another cell's state; and `carve_state` returns
    /// at or below the lava level without asking the aquifer, so the flag the read writes there
    /// is the one the mark gate reads.
    #[test]
    fn m3_p3a_prefetch_refuses_out_of_chunk_and_lava_level_cells() {
        crate::test_support::require_vanilla_data();
        let _gen = lock_gen_for_tests();
        let (cx, cz) = (0, 0);
        let mut g = Gen::load(0, crate::name_to_state).expect("datapack");
        let data = g.data.clone();
        let (lo, hi) = (g.min_y, g.min_y + g.height - 16);
        // Both this chunk and its -x neighbour: the neighbour's row has to be installed for the
        // out-of-chunk check to mean anything.
        let keys = [(cx, cz), (cx - 1, cz)];
        let cols: [Option<std::sync::Arc<ChunkCols>>; 2] = [None, None];
        let rows = crate::worldgen::prefetch_terrain(&data, None, &keys, &cols, lo, hi, 1, None);
        assert!(
            rows.iter().all(|r| r.is_some()),
            "prefetched rows installed"
        );
        g.install_prefetched(rows);
        let cc = g.cols(cx, cz).expect("columns");
        // The level the data gives the overworld cave carver, not a literal: this test is about
        // the gate `carve_state` takes, which is that number.
        let lava = super::creg()
            .def("minecraft:cave")
            .expect("cave carver")
            .lava
            .resolve(g.min_y, g.height);
        let above_lava = lava + 6;
        let mut carve_scratch = super::CarveScratch::new();
        let mut run = super::CarveRun::empty(&g, None, cx, cz, &mut carve_scratch);
        run.lava = lava;
        let (x, z) = (cx * 16 + 1, cz * 16 + 1);
        let reads = || super::CARVE_PREFETCH_READS.with(|c| c.get());

        super::CARVE_PREFETCH_READS.with(|c| c.set(0));
        let above = run.read(&mut g, &cc, x, above_lava, z);
        assert_eq!(
            reads(),
            1,
            "above the lava level the base row answers the read"
        );
        assert_eq!(
            Some(above),
            g.prefetched_cell(x, above_lava, z),
            "the answer must be the base row's"
        );

        let below = run.read(&mut g, &cc, x, lava, z);
        assert_eq!(
            reads(),
            1,
            "at the lava level the read must keep going through terrain_cell, which writes \
             the aquifer flag the mark gate reads"
        );
        assert_eq!(below, g.terrain_cell(&cc, x, lava, z));

        assert!(
            g.prefetched_cell(x - 16, above_lava, z).is_some(),
            "the neighbour's base row must be installed, or the next check cannot fail"
        );
        let _ = run.read(&mut g, &cc, x - 16, above_lava, z);
        assert_eq!(
            reads(),
            1,
            "an out-of-chunk position must not be answered from this chunk's rows"
        );
    }

    /// The dense `out` array plus the prefetched-cell fast path must answer exactly what the
    /// sparse write map over `terrain_cell` answered.
    ///
    /// The reference arm has no prefetched rows, so every read the dense path can short-circuit
    /// falls through to `terrain_cell` — the pre-change behaviour. The fast arm installs the
    /// base rows a terrain worker would have built for the same chunk.
    #[test]
    fn m3_p3a_dense_out_matches_sparse_writes() {
        crate::test_support::require_vanilla_data();
        let _gen = lock_gen_for_tests();
        let (cx, cz) = (0, 0);

        super::CARVE_PREFETCH_READS.with(|c| c.set(0));
        let mut cold = Gen::load(0, crate::name_to_state).expect("datapack");
        apply_carvers(&mut cold, cx, cz);
        let cold_reads = super::CARVE_PREFETCH_READS.with(|c| c.get());
        let cold = snapshot(&mut cold, cx, cz);

        let mut warm = Gen::load(0, crate::name_to_state).expect("datapack");
        let data = warm.data.clone();
        let (lo, hi) = (warm.min_y, warm.min_y + warm.height - 16);
        let keys = [(cx, cz)];
        let cols: Vec<Option<std::sync::Arc<ChunkCols>>> = vec![None];
        let rows = crate::worldgen::prefetch_terrain(&data, None, &keys, &cols, lo, hi, 1, None);
        assert!(
            rows.iter().all(|r| r.is_some()),
            "the fixture needs the prefetched rows installed"
        );
        warm.install_prefetched(rows);
        // Installing a worker's base rows files the *terrain* stage's marks (they are decided by
        // the fill, not by the carver), so the bucket starts non-empty. This comparison is the
        // carver's: drop what the install filed.
        assert!(
            !warm.take_shape_marks(cx, cz).is_empty(),
            "the prefetched install must have filed the terrain stage's own marks"
        );
        apply_carvers(&mut warm, cx, cz);
        let warm_reads = super::CARVE_PREFETCH_READS.with(|c| c.get());
        let warm = snapshot(&mut warm, cx, cz);

        assert!(
            !cold.0.is_empty(),
            "the fixture must carve something before its identity means anything"
        );
        assert_eq!(
            cold_reads, 0,
            "the reference arm has no prefetched rows, so the fast path cannot have answered"
        );
        assert!(
            warm_reads > 0,
            "the fast arm must answer cells from the prefetched rows, or the comparison is \
             two copies of the same path"
        );
        assert_eq!(cold.0, warm.0, "the carve writes differ");
        assert_eq!(cold.1, warm.1, "the post-carve WG heightmaps differ");
        assert_eq!(cold.2, warm.2, "the mark order differs");
    }

    /// The precomputed replaceable bitmap must agree with `in_tag` for every state id in the
    /// tested range, including the ids past the real registry's 26_684 states (both answer false).
    #[test]
    fn carver_replaceable_bitset_matches_in_tag() {
        crate::test_support::require_vanilla_data();
        let _gen = lock_gen_for_tests();
        let br = crate::block_registry::breg().expect("block registry");
        let reg = super::creg();
        assert!(!reg.defs.is_empty(), "the carver registry must be loaded");
        for (def_idx, c) in reg.defs.iter().enumerate() {
            for st in 0..30_000u32 {
                assert_eq!(
                    super::is_replaceable(c, br, st),
                    br.in_tag(st, &c.replaceable),
                    "carver {def_idx} ({}) state {st}",
                    c.replaceable
                );
            }
        }
    }

    /// The shared `OriginGrid` plus reusable scratch batch must leave the owner in exactly the
    /// state the serial per-chunk arm leaves it over a 4x4 grid: same writes, WG heightmaps and
    /// mark order for every chunk.
    #[test]
    fn carve_batch_matches_serial_bit_for_bit() {
        crate::test_support::require_vanilla_data();
        let _gen = lock_gen_for_tests();
        let coords: Vec<(i32, i32)> = (0..4)
            .flat_map(|cx| (0..4).map(move |cz| (cx, cz)))
            .collect();

        let mut serial = Gen::load(0, crate::name_to_state).expect("datapack");
        for &(tx, tz) in &coords {
            apply_carvers(&mut serial, tx, tz);
        }
        let mut par = Gen::load(0, crate::name_to_state).expect("datapack");
        super::carve_batch(&mut par, &coords, 4);

        let a: Vec<_> = coords
            .iter()
            .map(|&(cx, cz)| (cx, cz, snapshot(&mut serial, cx, cz)))
            .collect();
        let b: Vec<_> = coords
            .iter()
            .map(|&(cx, cz)| (cx, cz, snapshot(&mut par, cx, cz)))
            .collect();
        let writes: usize = a.iter().map(|s| s.2 .0.len()).sum();
        assert!(
            writes > 0,
            "the fixture must carve something before its identity means anything"
        );
        for (x, y) in a.iter().zip(&b) {
            assert_eq!((x.0, x.1), (y.0, y.1));
            assert_eq!(
                x.2 .0, y.2 .0,
                "chunk {},{}: batch carve writes differ",
                x.0, x.1
            );
            assert_eq!(
                x.2 .1, y.2 .1,
                "chunk {},{}: post-carve WG heightmaps differ",
                x.0, x.1
            );
            assert_eq!(x.2 .2, y.2 .2, "chunk {},{}: mark order differs", x.0, x.1);
        }
    }

    #[test]
    fn origin_grid_empty_and_distant_claims_are_bounded() {
        let empty = OriginGrid::new(&[]);
        assert_eq!((empty.w, empty.h), (0, 0));
        assert!(empty.cells.is_empty());
        assert!(empty.get(0, 0).is_none());

        crate::test_support::require_vanilla_data();
        let _gen = lock_gen_for_tests();
        let mut g = Gen::load(0, crate::name_to_state).expect("datapack");
        let built_empty = build_origin_grid(&mut g, &[], 4);
        assert!(built_empty.cells.is_empty());
        assert!(built_empty.sparse.is_none());

        let claims = [(0, 0), (10_000, 10_000)];
        let grid = build_origin_grid(&mut g, &claims, 4);
        assert!(
            grid.cells.is_empty(),
            "distant batches must skip the rectangle"
        );
        let sparse = grid.sparse.as_ref().expect("sparse origin lookup");
        assert_eq!(sparse.len(), 2 * 17 * 17);
        for &(tx, tz) in &claims {
            for ox in tx - 8..=tx + 8 {
                for oz in tz - 8..=tz + 8 {
                    assert!(sparse.contains_key(&(ox, oz)));
                }
            }
            for (ox, oz) in [(tx - 8, tz - 8), (tx, tz), (tx + 8, tz + 8)] {
                let direct = resolve_origin(&mut g, ox, oz).map(|(name, list)| (name, list.len()));
                let prefetched = grid.get(ox, oz).map(|(name, list)| (name, list.len()));
                assert_eq!(prefetched, direct, "origin ({ox},{oz}) differs");
            }
        }
        assert!(grid.get(5_000, 5_000).is_none());
    }

    /// Reusing one `CarveScratch` over consecutive chunks must not leak a written cell, a mask
    /// bit, an order entry or a shape mark into the next chunk — on both the same-height fast
    /// path and the resize path.
    #[test]
    fn carve_scratch_reuse_leaves_no_stale_state() {
        let mut scratch = super::CarveScratch::new();
        scratch.reset_for(384);
        assert_eq!(scratch.out.len(), 384 * 256);
        assert!(scratch.out.iter().all(|&v| v == u32::MAX));
        assert!(scratch.mask.iter().all(|&v| v == 0));

        // Dirty every kind of state, then reuse at the same height (the common path).
        scratch.out[0] = 7;
        scratch.out[12345] = 9;
        scratch.mask[0] = u64::MAX;
        scratch.mask[1] = 1;
        scratch.out_order.extend([0, 12345]);
        scratch.shape_marks.push((1, 2, 3));
        scratch.reset_for(384);
        assert!(
            scratch.out.iter().all(|&v| v == u32::MAX),
            "stale cell state"
        );
        assert!(scratch.mask.iter().all(|&v| v == 0), "stale mask bit");
        assert!(scratch.out_order.is_empty(), "stale order entry");
        assert!(scratch.shape_marks.is_empty(), "stale shape mark");

        // The resize path must also start perfectly clean.
        scratch.reset_for(320);
        assert_eq!(scratch.out.len(), 320 * 256);
        assert!(scratch.out.iter().all(|&v| v == u32::MAX));
        assert!(scratch.mask.iter().all(|&v| v == 0));
        assert!(scratch.out_order.is_empty());
        assert!(scratch.shape_marks.is_empty());
    }
}

#[cfg(all(test, megu_vanilla_data))]
mod a3b_tests {
    use super::*;

    /// Every precomputed bitmask must agree exhaustively with the original
    /// string-set lookups it replaced: `ball_biome_mask` against `fr.ball`,
    /// and each `StepEntry.step_biome_mask` against `fr.bsteps[b][step]`,
    /// for all presets, steps and placed features.
    #[test]
    fn a3b_biome_bitmask_matches_bsteps_and_ball_exhaustively() {
        let fr = freg();
        assert!(
            fr.bit_ok,
            "the vanilla 1.21.1 feature biome set must fit in u128"
        );

        // `ball_masks`: exactly the biomes whose flattened feature list
        // contains the placed feature, no more and no less.
        for pf in fr.placed_vec.iter() {
            let mask = fr.ball_masks.get(&pf.name).copied().unwrap_or(0);
            for (biome, &b) in fr.biome_bits.iter() {
                let expected = fr.ball.get(biome).is_some_and(|s| s.contains(&pf.name));
                let fast = (mask & (1u128 << b)) != 0;
                assert_eq!(fast, expected, "ball mask mismatch for {biome}/{}", pf.name);
            }
        }
        // Every `ball` member also has a placed feature with that bit set.
        for (biome, feats) in fr.ball.iter() {
            let b = 1u128 << fr.biome_bits[biome];
            for name in feats {
                let mask = fr.ball_masks.get(name).copied().unwrap_or(0);
                assert_ne!(mask & b, 0, "ball membership lost for {biome}/{name}");
            }
        }

        // `step_biome_mask`: parallel to `order` and equal to the set of
        // biomes whose `bsteps[b][step]` lists that entry's feature.
        for (preset, order) in fr.order.iter() {
            let entries = &fr.step_entries[preset];
            assert_eq!(entries.len(), order.len(), "{preset:?} step count");
            for (step, (list, elist)) in order.iter().zip(entries.iter()).enumerate() {
                assert_eq!(list.len(), elist.len(), "{preset:?} step {step} count");
                for ((gi, name), e) in list.iter().zip(elist.iter()) {
                    assert_eq!(e.gi, *gi, "{preset:?} step {step} gi");
                    assert_eq!(&e.name, name, "{preset:?} step {step} name");
                    let pf = &fr.placed_vec[e.placed_idx];
                    assert_eq!(pf.name, *name, "{preset:?} step {step} placed_idx");
                    for (biome, &b) in fr.biome_bits.iter() {
                        let expected = fr
                            .bsteps
                            .get(biome)
                            .and_then(|s| s.get(step))
                            .is_some_and(|l| l.iter().any(|f| f == name));
                        let fast = (e.step_biome_mask & (1u128 << b)) != 0;
                        assert_eq!(
                            fast, expected,
                            "step mask mismatch {preset:?} step {step} {biome}/{name}"
                        );
                    }
                }
            }
        }
    }

    /// The fast path's leaf selection (`biome_blended`, falling back to the
    /// quart sample) and `leaf_bit_cache` must give the same biome name and
    /// bit as `FC::biome` for sample coordinates in every dimension.
    #[test]
    fn a3b_leaf_bit_cache_matches_biome_name_for_all_presets() {
        crate::test_support::require_vanilla_data();
        let _gen = crate::worldgen::lock_gen_for_tests();
        let fr = freg();
        let coords = [
            (0, 0, 0),
            (7, 64, 9),
            (-3, 120, -5),
            (16, 32, 48),
            (1, -8, 2),
        ];
        for preset in [
            NoisePreset::Overworld,
            NoisePreset::Nether,
            NoisePreset::End,
        ] {
            let mut g = Gen::load_preset(4242, crate::name_to_state, preset).expect("datapack");
            let mut ctx = crate::generation_context::FC::new(&mut g);
            for (x, y, z) in coords {
                let expected = ctx.biome(x, y, z);
                let leaf = ctx
                    .g
                    .biome_blended(x, y, z)
                    .map(|(_, l)| l)
                    .or_else(|| {
                        ctx.g
                            .biome_quart(x >> 2, y >> 2, z >> 2, None)
                            .map(|(_, l)| l)
                    })
                    .filter(|l| usize::from(*l) < 128);
                let actual = leaf.and_then(|l| ctx.g.biome_name_of_leaf(l));
                assert_eq!(expected, actual, "{preset:?} at ({x},{y},{z})");
                if let Some(l) = leaf {
                    LEAF_BITS.with(|c| {
                        let mut c = c.borrow_mut();
                        c.active = true;
                        c.known = 0;
                    });
                    let bit = leaf_bit(&mut ctx, fr, l);
                    let want = expected
                        .as_deref()
                        .and_then(|n| fr.biome_bits.get(n).copied())
                        .map_or(0, |i| 1u128 << i);
                    assert_eq!(bit, Some(want), "{preset:?} leaf bit at ({x},{y},{z})");
                    // A second lookup from the cache must return the same bit.
                    assert_eq!(
                        leaf_bit(&mut ctx, fr, l),
                        Some(want),
                        "{preset:?} cached leaf bit at ({x},{y},{z})"
                    );
                    LEAF_BITS.with(|c| c.borrow_mut().active = false);
                }
            }
        }
    }
}

#[cfg(all(test, megu_vanilla_data))]
mod a3a_tests {
    use super::*;
    use crate::worldgen::{lock_gen_for_tests, NoisePreset};

    type ChunkWrites = Vec<((i32, i32), Vec<((i32, i32, i32), u32)>)>;
    type WgTops = Vec<((i32, i32), Vec<i32>)>;
    type ChunkMarks = Vec<((i32, i32), Vec<(i32, i32, i32)>)>;

    /// Everything the feature + carver layers leave on the owner for a window: the write
    /// buckets, the heightmap maxima, the promotion marks, the staged payloads, the ticks,
    /// the post-carve WG tops and the `pop_done` set. Order is preserved where serial order
    /// is observable (writes, marks, ticks).
    #[derive(Debug)]
    struct State {
        writes: ChunkWrites,
        heights: Vec<((i32, i32), (i32, i32))>,
        marks: ChunkMarks,
        payloads: crate::generation_context::StagedPayloads,
        leaves: Vec<GeneratedTick>,
        fluids: Vec<GeneratedTick>,
        wg: WgTops,
        done: Vec<(i32, i32)>,
    }

    fn state(g: &mut Gen, x0: i32, z0: i32, n: i32) -> State {
        let mut writes = Vec::new();
        let mut heights = Vec::new();
        let mut marks = Vec::new();
        let mut wg = Vec::new();
        for cz in z0 - 1..=z0 + n {
            for cx in x0 - 1..=x0 + n {
                let mut w: Vec<_> = g.feature_bucket_writes(cx, cz).collect();
                w.sort();
                writes.push(((cx, cz), w));
                heights.push((
                    (cx, cz),
                    (
                        g.feature_height(cx * 16, cz * 16, 0),
                        g.feature_height(cx * 16, cz * 16, 1),
                    ),
                ));
                let mut m = g.take_shape_marks(cx, cz);
                m.sort();
                marks.push(((cx, cz), m));
                let mut tops = Vec::new();
                for ocean_floor in [false, true] {
                    for lz in 0..16 {
                        for lx in 0..16 {
                            let x = cx * 16 + lx;
                            let z = cz * 16 + lz;
                            tops.push(
                                g.wg_after_carve_height(ocean_floor, x, z)
                                    .unwrap_or(i32::MIN),
                            );
                        }
                    }
                }
                wg.push(((cx, cz), tops));
            }
        }
        let mut done: Vec<_> = g.pop_done.iter().copied().collect();
        done.sort();
        State {
            writes,
            heights,
            marks,
            payloads: g.take_staged_payloads(),
            leaves: g.generated_leaf_ticks(),
            fluids: g.generated_fluid_ticks(),
            wg,
            done,
        }
    }

    /// A deterministic window order with a deliberately non-lexicographic tail: the wave
    /// schedule must respect the caller's dependency order, not sort the chunks itself.
    fn window(n: i32) -> Vec<(i32, i32)> {
        let mut order: Vec<(i32, i32)> = (0..n).flat_map(|x| (0..n).map(move |z| (x, z))).collect();
        order.reverse();
        order
    }

    fn serial(g: &mut Gen, order: &[(i32, i32)]) {
        for &(x, z) in order {
            if g.pop_done.insert((x, z)) {
                g.suppress_generated_ticks = g.restores_ticks_for((x, z));
                super::populate(g, x, z);
                g.suppress_generated_ticks = false;
            }
        }
    }

    fn parallel(g: &mut Gen, order: &[(i32, i32)], jobs: usize) {
        super::precarve_order(g, order, jobs);
        let _ = super::populate_parallel(g, order, jobs);
    }

    fn assert_same(a: &State, b: &State, what: &str) {
        assert_eq!(a.wg, b.wg, "{what}: post-carve WG tops differ");
        assert_eq!(
            a.heights, b.heights,
            "{what}: feature heightmap maxima differ"
        );
        assert_eq!(a.marks, b.marks, "{what}: promotion marks differ");
        assert_eq!(a.payloads, b.payloads, "{what}: staged payloads differ");
        assert_eq!(
            a.writes, b.writes,
            "{what}: feature/carve write buckets differ"
        );
        assert_eq!(a.leaves, b.leaves, "{what}: leaf ticks differ");
        assert_eq!(a.fluids, b.fluids, "{what}: fluid ticks differ");
        assert_eq!(a.done, b.done, "{what}: pop_done differs");
    }

    /// The headline A3-a gate: three dimensions, three seeds, 5x5 windows at both ends of
    /// the job range. The serial arm is the original `populate_in_order` loop inlined (the
    /// process-wide `MEGU_GEN_JOBS` env cannot be changed mid-test).
    #[test]
    fn populate_order_parallel_matches_serial_bit_for_bit() {
        crate::test_support::require_vanilla_data();
        let _guard = lock_gen_for_tests();
        for preset in [
            NoisePreset::Overworld,
            NoisePreset::Nether,
            NoisePreset::End,
        ] {
            for seed in [0u64, 4242, 12345] {
                let order = window(5);
                let mut a =
                    Gen::load_preset(seed, crate::name_to_state, preset).expect("serial gen");
                serial(&mut a, &order);
                let a = state(&mut a, 0, 0, 5);
                for jobs in [5usize, 8] {
                    let mut b =
                        Gen::load_preset(seed, crate::name_to_state, preset).expect("parallel gen");
                    parallel(&mut b, &order, jobs);
                    let b = state(&mut b, 0, 0, 5);
                    assert_same(&a, &b, &format!("{preset:?} seed={seed} jobs={jobs}"));
                }
            }
        }
    }

    /// The batch pre-carve alone (2-3 pending chunks, below the wave threshold) must be a
    /// no-op on the bytes: it only moves carver work ahead of the first feature pass.
    #[test]
    fn precarve_only_matches_serial_bit_for_bit() {
        crate::test_support::require_vanilla_data();
        let _guard = lock_gen_for_tests();
        let order = vec![(3, 1), (4, 1), (3, 2)];
        for preset in [NoisePreset::Overworld, NoisePreset::Nether] {
            let mut a = Gen::load_preset(4242, crate::name_to_state, preset).expect("serial gen");
            serial(&mut a, &order);
            let a = state(&mut a, 3, 1, 2);
            let mut b = Gen::load_preset(4242, crate::name_to_state, preset).expect("parallel gen");
            super::precarve_order(&mut b, &order, 5);
            serial(&mut b, &order);
            let b = state(&mut b, 3, 1, 2);
            assert_same(&a, &b, &format!("{preset:?} pre-carve only"));
        }
    }

    /// The real `populate_in_order` dispatch (not just the private wave helper) must take
    /// the parallel arm and leave the owner in the serial state. This is the path the
    /// `--wgdump` plan uses; the env-cached `MEGU_GEN_JOBS` is whatever the harness set.
    #[test]
    fn populate_in_order_dispatches_to_waves_bit_for_bit() {
        crate::test_support::require_vanilla_data();
        let _guard = lock_gen_for_tests();
        let order = window(5);
        let mut a = Gen::load_preset(0, crate::name_to_state, NoisePreset::Overworld).unwrap();
        serial(&mut a, &order);
        let a = state(&mut a, 0, 0, 5);
        super::POPULATE_WORKERS.store(0, Ordering::Relaxed);
        let mut b = Gen::load_preset(0, crate::name_to_state, NoisePreset::Overworld).unwrap();
        let round = super::starts_round_begin();
        b.starts_round = Some(round);
        assert_eq!(
            b.populate_in_order(order.iter().copied(), super::populate),
            25
        );
        b.starts_round = None;
        super::starts_round_end(round);
        let workers = super::populate_workers();
        let b = state(&mut b, 0, 0, 5);
        assert_same(&a, &b, "populate_in_order dispatch");
        assert!(
            workers >= 2 || crate::worldgen::terrain_jobs() <= 1,
            "the real dispatch must use wave workers when jobs > 1, got {workers}"
        );
    }

    /// The round-less arm — the live-server path, where the caller opens no
    /// `starts_round` — must still dispatch to the workers (on the bounded
    /// fallback share and the thread-local start caches) and still leave the
    /// owner in the serial state, bit for bit.
    #[test]
    fn populate_in_order_without_round_dispatches_to_waves_bit_for_bit() {
        crate::test_support::require_vanilla_data();
        let _guard = lock_gen_for_tests();
        let order = window(5);
        let mut a = Gen::load_preset(0, crate::name_to_state, NoisePreset::Overworld).unwrap();
        serial(&mut a, &order);
        let a = state(&mut a, 0, 0, 5);
        super::POPULATE_WORKERS.store(0, Ordering::Relaxed);
        let mut b = Gen::load_preset(0, crate::name_to_state, NoisePreset::Overworld).unwrap();
        // No `starts_round` on the generator: the dispatch must not need one.
        assert_eq!(
            b.populate_in_order(order.iter().copied(), super::populate),
            25
        );
        assert!(b.starts_round.is_none());
        let workers = super::populate_workers();
        let b = state(&mut b, 0, 0, 5);
        assert_same(&a, &b, "populate_in_order round-less dispatch");
        assert!(
            workers >= 2 || crate::worldgen::terrain_jobs() <= 1,
            "the round-less dispatch must use wave workers when jobs > 1, got {workers}"
        );
    }

    /// The conservative spacing from the Layer A3 plan (`% 4`) must be bit-identical too.
    #[test]
    fn populate_wave_spacing_four_matches_serial_bit_for_bit() {
        crate::test_support::require_vanilla_data();
        let _guard = lock_gen_for_tests();
        super::set_pop_wave_spacing(4);
        let order = window(5);
        let mut a = Gen::load_preset(0, crate::name_to_state, NoisePreset::Overworld).unwrap();
        serial(&mut a, &order);
        let a = state(&mut a, 0, 0, 5);
        let mut b = Gen::load_preset(0, crate::name_to_state, NoisePreset::Overworld).unwrap();
        parallel(&mut b, &order, 5);
        let b = state(&mut b, 0, 0, 5);
        super::set_pop_wave_spacing(0);
        assert_same(&a, &b, "spacing 4");
    }

    /// The wave arm must actually use more than one thread: a serial loop accidentally left
    /// in place would still be bit-identical, so this probe is the guard against a false
    /// green.
    #[test]
    fn populate_waves_run_on_multiple_worker_threads() {
        crate::test_support::require_vanilla_data();
        let _guard = lock_gen_for_tests();
        let order = window(6);
        super::POPULATE_WORKERS.store(0, Ordering::Relaxed);
        let mut b = Gen::load_preset(0, crate::name_to_state, NoisePreset::Overworld).unwrap();
        parallel(&mut b, &order, 5);
        assert!(
            super::populate_workers() >= 2,
            "the 6x6 window must populate on at least two workers, got {}",
            super::populate_workers()
        );
        super::POPULATE_WORKERS.store(0, Ordering::Relaxed);
        let mut a = Gen::load_preset(0, crate::name_to_state, NoisePreset::Overworld).unwrap();
        serial(&mut a, &order);
        assert_eq!(
            super::populate_workers(),
            0,
            "the serial reference must not spawn wave workers"
        );
    }
}

#[cfg(all(test, megu_vanilla_data))]
mod p8b1_issue199_tests {
    // Issue #199: a configured_feature / placed_feature / configured_carver
    // file that fails to parse used to be dropped with at most an env-gated
    // trace. These tests pin the structured report (`WorldgenParseReport`),
    // the default-mode continuation, and the `MEGU_STRICT_WORLDGEN=1`
    // fail-closed mode.
    use super::*;

    fn empty_freg() -> FReg {
        FReg {
            cfg: HashMap::new(),
            cfg_vec: Vec::new(),
            placed: HashMap::new(),
            placed_vec: Vec::new(),
            biome_bits: HashMap::new(),
            ball_masks: HashMap::new(),
            possibly_cold_mask: 0,
            bit_ok: true,
            bsteps: HashMap::new(),
            ball: HashMap::new(),
            btemp: HashMap::new(),
            spawners: HashMap::new(),
            spawn_costs: HashMap::new(),
            spawn_probability: HashMap::new(),
            order: HashMap::new(),
            step_entries: HashMap::new(),
        }
    }

    fn empty_creg() -> CarverReg {
        CarverReg {
            cfg: HashMap::new(),
            defs: Vec::new(),
            biome: HashMap::new(),
            biome_resolved: HashMap::new(),
        }
    }

    #[test]
    fn p8b1_issue199_configured_feature_failures_are_recorded_not_dropped() {
        let mut rep = WorldgenParseReport::default();
        let mut reg = empty_freg();

        let e = load_cf_entry(&mut reg, "test:bad_json", b"{", &mut rep).unwrap_err();
        assert_eq!(e.kind, "configured_feature");
        assert_eq!(e.id, "test:bad_json");
        assert_eq!(e.reason, "malformed JSON");

        let e = load_cf_entry(&mut reg, "test:no_type", br#"{"config":{}}"#, &mut rep).unwrap_err();
        assert_eq!(e.id, "test:no_type");
        assert_eq!(e.reason, "missing required field \"type\"");

        let e = load_cf_entry(
            &mut reg,
            "test:bad_disk",
            br#"{"type":"minecraft:disk"}"#,
            &mut rep,
        )
        .unwrap_err();
        assert_eq!(e.id, "test:bad_disk");
        assert!(
            e.reason.contains("malformed configured_feature body")
                && e.reason.contains("minecraft:disk"),
            "reason should name the refused type: {e}"
        );

        assert_eq!(rep.of_kind("configured_feature").count(), 3);
        assert_eq!(rep.errors.len(), 3);
        assert!(
            reg.cfg.is_empty() && reg.cfg_vec.is_empty(),
            "a refused entry must not enter the registry"
        );
    }

    #[test]
    fn p8b1_issue199_placed_feature_failures_are_recorded_not_dropped() {
        let mut rep = WorldgenParseReport::default();
        let mut raw: Vec<(String, Pf)> = Vec::new();
        let mut idx: HashMap<String, usize> = HashMap::new();

        let e =
            load_pf_entry(&mut raw, &mut idx, "test:bad_json".into(), b"{", &mut rep).unwrap_err();
        assert_eq!(e.kind, "placed_feature");
        assert_eq!(e.id, "test:bad_json");
        assert_eq!(e.reason, "malformed JSON");

        let e = load_pf_entry(
            &mut raw,
            &mut idx,
            "test:no_feature".into(),
            br#"{"placement":[]}"#,
            &mut rep,
        )
        .unwrap_err();
        assert_eq!(e.reason, "missing required field \"feature\"");

        let e = load_pf_entry(
            &mut raw,
            &mut idx,
            "test:bad_mod".into(),
            br#"{"feature":"minecraft:foo","placement":[{"type":"minecraft:bogus_modifier"}]}"#,
            &mut rep,
        )
        .unwrap_err();
        assert_eq!(
            e.reason,
            "unsupported placement modifier \"minecraft:bogus_modifier\""
        );

        assert_eq!(rep.of_kind("placed_feature").count(), 3);
        assert!(raw.is_empty() && idx.is_empty());
    }

    #[test]
    fn p8b1_issue199_configured_carver_failures_are_recorded_not_dropped() {
        let mut rep = WorldgenParseReport::default();
        let mut reg = empty_creg();
        let mut by_name = HashMap::new();

        let e =
            load_carver_entry(&mut reg, &mut by_name, "test:bad_json", b"{", &mut rep).unwrap_err();
        assert_eq!(e.kind, "configured_carver");
        assert_eq!(e.id, "test:bad_json");
        assert_eq!(e.reason, "malformed JSON");

        let e = load_carver_entry(
            &mut reg,
            &mut by_name,
            "test:geyser",
            br#"{"type":"minecraft:geyser","config":{}}"#,
            &mut rep,
        )
        .unwrap_err();
        assert_eq!(e.reason, "unsupported carver type \"minecraft:geyser\"");

        let e = load_carver_entry(
            &mut reg,
            &mut by_name,
            "test:no_config",
            br#"{"type":"minecraft:cave"}"#,
            &mut rep,
        )
        .unwrap_err();
        assert_eq!(e.reason, "missing required field \"config\"");

        assert_eq!(rep.of_kind("configured_carver").count(), 3);
        assert!(reg.cfg.is_empty() && reg.defs.is_empty());
    }

    /// One report collects every family: feeding a malformed entry of each
    /// kind leaves exactly four `{kind, id, reason}` entries, none dropped.
    #[test]
    fn p8b1_issue199_one_report_collects_all_four_kinds() {
        let mut rep = WorldgenParseReport::default();
        let mut fr = empty_freg();
        let _ = load_cf_entry(&mut fr, "test:cf", b"{", &mut rep);
        let mut raw = Vec::new();
        let mut idx = HashMap::new();
        let _ = load_pf_entry(&mut raw, &mut idx, "test:pf".to_string(), b"{", &mut rep);
        let mut cr = empty_creg();
        let mut by_name = HashMap::new();
        let _ = load_carver_entry(&mut cr, &mut by_name, "test:cv", b"{", &mut rep);
        // The surface-rule arm records through the same channel.
        rep.fail("surface_rule", "test:sr", "malformed JSON");

        for (kind, id) in [
            ("configured_feature", "test:cf"),
            ("placed_feature", "test:pf"),
            ("configured_carver", "test:cv"),
            ("surface_rule", "test:sr"),
        ] {
            let found: Vec<_> = rep.of_kind(kind).collect();
            assert_eq!(found.len(), 1, "{kind} must be recorded exactly once");
            assert_eq!(found[0].id, id);
            assert_eq!(found[0].reason, "malformed JSON");
        }
        assert_eq!(rep.errors.len(), 4);
    }

    #[test]
    fn p8b1_issue199_default_mode_continues_strict_mode_refuses() {
        let bad_cf = HashMap::from([
            ("test:bad1".to_string(), b"{".to_vec()),
            ("test:bad2".to_string(), b"{".to_vec()),
        ]);
        let empty: HashMap<String, Vec<u8>> = HashMap::new();

        // Default: every failure recorded, the load still completes.
        let mut rep = WorldgenParseReport::default();
        let reg = load_freg_report(&mut rep, false, &bad_cf, &empty, &empty).unwrap();
        assert_eq!(rep.of_kind("configured_feature").count(), 2);
        assert!(reg.cfg.is_empty());

        // Strict (`MEGU_STRICT_WORLDGEN=1`): the first failure is the `Err`.
        let mut rep = WorldgenParseReport::default();
        let e = load_freg_report(&mut rep, true, &bad_cf, &empty, &empty)
            .err()
            .expect("strict load must refuse");
        assert_eq!(e.kind, "configured_feature");
        assert!(e.id.starts_with("test:bad"), "the refused file: {e}");
        assert_eq!(rep.errors.len(), 1, "strict aborts at the first failure");

        // Same contract for placed_feature entries.
        let bad_pf = HashMap::from([("test:pf".to_string(), b"{".to_vec())]);
        let mut rep = WorldgenParseReport::default();
        let e = load_freg_report(&mut rep, true, &empty, &bad_pf, &empty)
            .err()
            .expect("strict load must refuse");
        assert_eq!((e.kind, e.id.as_str()), ("placed_feature", "test:pf"));

        // And configured_carver.
        let mut rep = WorldgenParseReport::default();
        let e = load_creg_report(&mut rep, true, &bad_pf, &empty)
            .err()
            .expect("strict load must refuse");
        assert_eq!((e.kind, e.id.as_str()), ("configured_carver", "test:pf"));
        assert_eq!(rep.errors.len(), 1);
    }

    #[test]
    fn p8b1_issue199_strict_flag_is_read_per_load() {
        std::env::set_var("MEGU_STRICT_WORLDGEN", "1");
        assert!(strict_worldgen_parse());
        std::env::set_var("MEGU_STRICT_WORLDGEN", "0");
        assert!(!strict_worldgen_parse());
        std::env::remove_var("MEGU_STRICT_WORLDGEN");
        assert!(!strict_worldgen_parse());
    }

    /// The pinned datapack must load with an empty report: the diagnostic
    /// exists to name failures, and vanilla has none for these families.
    #[test]
    fn p8b1_issue199_vanilla_datapack_reports_zero_parse_failures() {
        crate::test_support::require_vanilla_data();
        let mut rep = WorldgenParseReport::default();
        let reg = load_freg_report(
            &mut rep,
            false,
            &Gen::dp_files_any(&["configured_feature", "feature"]),
            &Gen::dp_files("placed_feature"),
            &Gen::dp_files("biome"),
        )
        .unwrap();
        assert!(
            rep.is_clean(),
            "vanilla configured/placed features must parse clean: {:?}",
            rep.errors
        );
        assert!(!reg.cfg.is_empty() && !reg.placed_vec.is_empty());

        let mut rep = WorldgenParseReport::default();
        let reg = load_creg_report(
            &mut rep,
            false,
            &Gen::dp_files("configured_carver"),
            &Gen::dp_files("biome"),
        )
        .unwrap();
        assert!(
            rep.is_clean(),
            "vanilla carvers must parse clean: {:?}",
            rep.errors
        );
        assert!(!reg.defs.is_empty());
    }
}

#[cfg(all(test, megu_vanilla_data))]
mod biome_decoration_contract_tests {
    use super::*;

    fn flatten_strings(j: &J, out: &mut Vec<String>) {
        match j {
            J::Arr(v) => {
                for e in v {
                    flatten_strings(e, out);
                }
            }
            J::Str(s) => out.push(s.clone()),
            _ => {}
        }
    }

    fn biome_features(name: &str) -> Vec<String> {
        let files = Gen::dp_files("biome");
        let key = format!("minecraft:{name}");
        let bytes = files
            .get(&key)
            .unwrap_or_else(|| panic!("pinned vanilla biome {key} missing"));
        let j = crate::json::jparse(bytes)
            .unwrap_or_else(|| panic!("pinned vanilla biome {key} is malformed"));
        let mut out = Vec::new();
        flatten_strings(js(&j, "features").expect("biome feature steps"), &mut out);
        out
    }

    /// The biome half of decoration parity: the pinned 1.21.1 biome JSON must still
    /// wire every family to its marker placed features. The configured-feature bodies
    /// themselves are covered by the native cell/RNG oracle tests above; this matrix is
    /// what prevents a correct feature implementation from becoming unreachable in the
    /// biome that owns it.
    #[test]
    fn vanilla_1211_biome_families_wire_their_decoration_marker_features() {
        for (biome, markers) in [
            (
                "lush_caves",
                &[
                    "minecraft:lush_caves_ceiling_vegetation",
                    "minecraft:cave_vines",
                    "minecraft:lush_caves_clay",
                    "minecraft:lush_caves_vegetation",
                    "minecraft:rooted_azalea_tree",
                    "minecraft:spore_blossom",
                ][..],
            ),
            (
                "dripstone_caves",
                &[
                    "minecraft:large_dripstone",
                    "minecraft:dripstone_cluster",
                    "minecraft:pointed_dripstone",
                ][..],
            ),
            (
                "deep_dark",
                &["minecraft:sculk_vein", "minecraft:sculk_patch_deep_dark"][..],
            ),
            (
                "warm_ocean",
                &[
                    "minecraft:warm_ocean_vegetation",
                    "minecraft:seagrass_warm",
                    "minecraft:sea_pickle",
                ][..],
            ),
            (
                "lukewarm_ocean",
                &["minecraft:seagrass_warm", "minecraft:kelp_warm"][..],
            ),
            (
                "ice_spikes",
                &["minecraft:ice_spike", "minecraft:ice_patch"][..],
            ),
            (
                "frozen_ocean",
                &[
                    "minecraft:iceberg_packed",
                    "minecraft:iceberg_blue",
                    "minecraft:blue_ice",
                ][..],
            ),
            (
                "wooded_badlands",
                &[
                    "minecraft:trees_badlands",
                    "minecraft:patch_dead_bush_badlands",
                    "minecraft:patch_cactus_decorated",
                ][..],
            ),
            (
                "desert",
                &[
                    "minecraft:patch_dead_bush_2",
                    "minecraft:patch_cactus_desert",
                ][..],
            ),
            (
                "savanna",
                &["minecraft:trees_savanna", "minecraft:patch_grass_savanna"][..],
            ),
            (
                "meadow",
                &["minecraft:flower_meadow", "minecraft:trees_meadow"][..],
            ),
            (
                "flower_forest",
                &[
                    "minecraft:flower_forest_flowers",
                    "minecraft:trees_flower_forest",
                    "minecraft:flower_flower_forest",
                ][..],
            ),
            (
                "cherry_grove",
                &["minecraft:flower_cherry", "minecraft:trees_cherry"][..],
            ),
            (
                "bamboo_jungle",
                &["minecraft:bamboo", "minecraft:bamboo_vegetation"][..],
            ),
            (
                "basalt_deltas",
                &[
                    "minecraft:delta",
                    "minecraft:small_basalt_columns",
                    "minecraft:large_basalt_columns",
                    "minecraft:basalt_blobs",
                    "minecraft:blackstone_blobs",
                    "minecraft:ore_ancient_debris_large",
                ][..],
            ),
            (
                "crimson_forest",
                &[
                    "minecraft:weeping_vines",
                    "minecraft:crimson_fungi",
                    "minecraft:crimson_forest_vegetation",
                ][..],
            ),
            (
                "warped_forest",
                &[
                    "minecraft:warped_fungi",
                    "minecraft:warped_forest_vegetation",
                    "minecraft:nether_sprouts",
                    "minecraft:twisting_vines",
                ][..],
            ),
            (
                "the_end",
                &["minecraft:end_spike", "minecraft:end_platform"][..],
            ),
            (
                "end_highlands",
                &["minecraft:end_gateway_return", "minecraft:chorus_plant"][..],
            ),
            ("small_end_islands", &["minecraft:end_island_decorated"][..]),
        ] {
            let features = biome_features(biome);
            for marker in markers {
                assert!(
                    features.iter().any(|f| f == marker),
                    "{biome} does not wire {marker}; feature list: {features:?}"
                );
            }
        }
    }
}

// The unified StructureSet / StructureStart pipeline. Included rather than declared as a
// submodule: `feature` is the layer that may see `structure_template` and `mineshaft`
// together, and `tools/layers_check.sh` forbids new `mod` edges in production files.
include!("jigsaw_structures.rs");
include!("structure_pipeline.rs");

#[cfg(test)]
mod audit1_feature_tests {
    use super::*;
    #[test]
    fn audit1_end_crystal_queue_drains_once() {
        let _ = take_staged_end_crystals();
        let row = EndCrystalPlacement {
            x: 1,
            y: 70,
            z: 2,
            yaw: 90.0,
            guarded: true,
        };
        stage_end_crystal(row);
        stage_end_crystal(row);
        assert_eq!(take_staged_end_crystals(), vec![row]);
        assert!(drain_generated_end_crystals().is_empty());
    }
}
