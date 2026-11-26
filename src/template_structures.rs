// Native Minecraft 1.21.1 `TemplateStructurePiece` structures.
//
// This file is included at the end of `structure_template.rs`, so it shares that
// module's private template/processor vocabulary. It owns three things:
//
//   * the native `StructureTemplate.placeInWorld` contract the existing
//     `Template::place_blocks` path does not carry (waterlogging, the BARRIER
//     pre-clear, `RandomizableContainer` `LootTableSeed`, template entities,
//     both shape-update passes, and the per-position settings RNG),
//   * `BlockAgeProcessor`, the one native processor shape the crate's
//     `Processor` enum cannot express,
//   * the twelve 1.21.1 template structures: `nether_fossil`, `igloo`,
//     `shipwreck`/`shipwreck_beached`, `ocean_ruin_cold`/`ocean_ruin_warm` and
//     the seven `ruined_portal*` variants.
//
// Every contract below was read from the pinned 1.21.1 server jar
// (`tools/bytecode_probe.sh`) and is pinned by
// `tests/data/template_structures_1211.tsv` (71 native oracle vectors).

use crate::block_registry::{breg, HeightmapKind, Support};
use crate::generation_math::{hash_block_pos, Lcg, Xor};
use crate::structure_transform::{Mirror, Rotation};
use std::rc::Rc;

/// `[minX, minY, minZ, maxX, maxY, maxZ]`, native `BoundingBox` order.
pub(crate) type BB = [i32; 6];

// ---------------------------------------------------------------- bounding boxes

pub(crate) fn bb_inside(bb: &BB, p: Pos) -> bool {
    p.0 >= bb[0] && p.0 <= bb[3] && p.1 >= bb[1] && p.1 <= bb[4] && p.2 >= bb[2] && p.2 <= bb[5]
}

pub(crate) fn bb_intersects(a: &BB, b: &BB) -> bool {
    a[3] >= b[0] && a[0] <= b[3] && a[4] >= b[1] && a[1] <= b[4] && a[5] >= b[2] && a[2] <= b[5]
}

pub(crate) fn bb_encapsulate(a: &mut BB, b: &BB) {
    a[0] = a[0].min(b[0]);
    a[1] = a[1].min(b[1]);
    a[2] = a[2].min(b[2]);
    a[3] = a[3].max(b[3]);
    a[4] = a[4].max(b[4]);
    a[5] = a[5].max(b[5]);
}

pub(crate) fn bb_center(bb: &BB) -> Pos {
    (
        bb[0] + (bb[3] - bb[0] + 1) / 2,
        bb[1] + (bb[4] - bb[1] + 1) / 2,
        bb[2] + (bb[5] - bb[2] + 1) / 2,
    )
}

/// `StructureTemplate.transform(BlockPos, Mirror, Rotation, BlockPos)`.
pub(crate) fn transform_pos(p: Pos, mirror: Mirror, rotation: Rotation, pivot: Pos) -> Pos {
    crate::structure_transform::block(p, mirror, rotation, pivot)
}

/// `StructureTemplate.getBoundingBox(StructurePlaceSettings, BlockPos)`.
pub(crate) fn template_bb(
    template: &Template,
    pos: Pos,
    mirror: Mirror,
    rotation: Rotation,
    pivot: Pos,
) -> BB {
    crate::structure_transform::bounds(template.size, pos, pivot, mirror, rotation)
}

/// `BlockPos.betweenClosedStream(BoundingBox)` order: z outer, y middle, x inner
/// (`MutableBlockPosIterator` walks `x` fastest).
pub(crate) fn bb_positions(bb: &BB) -> impl Iterator<Item = Pos> + '_ {
    (bb[2]..=bb[5]).flat_map(move |z| {
        (bb[1]..=bb[4]).flat_map(move |y| (bb[0]..=bb[3]).map(move |x| (x, y, z)))
    })
}

// ---------------------------------------------------------------- rotation

pub(crate) const ROTATIONS: [Rotation; 4] = [
    Rotation::None,
    Rotation::Clockwise90,
    Rotation::Clockwise180,
    Rotation::Counterclockwise90,
];

/// `Rotation.getRandom(RandomSource)` = `values()[random.nextInt(4)]`.
pub(crate) fn random_rotation(rng: &mut impl FnMut(i32) -> i32) -> Rotation {
    ROTATIONS[rng(4) as usize]
}

// ---------------------------------------------------------------- world seam

/// The slice of `LevelAccessor` / `ChunkGenerator` these pieces read.
pub(crate) trait TemplateWorld {
    fn get(&mut self, x: i32, y: i32, z: i32) -> u32;
    /// `LevelAccessor.setBlock`: `false` when the cell is outside the build height.
    fn set(&mut self, x: i32, y: i32, z: i32, state: u32) -> bool;
    /// `LevelReader.getHeight` — `WORLD_SURFACE`/`OCEAN_FLOOR` scan the current world
    /// (writes included); their `*_WG` twins are the map a chunk freezes at the end of
    /// its carver pass, so a feature or structure write never moves them.
    fn heightmap(&mut self, kind: HeightmapKind, x: i32, z: i32) -> i32;
    /// `ChunkGenerator.getBaseHeight` — the generator's own column answer.
    fn base_height(&mut self, kind: HeightmapKind, x: i32, z: i32) -> i32;
    /// `ChunkGenerator.getBaseColumn(x, z, level, randomState)`: the noise-only column a
    /// structure's `findGenerationPoint` scans, before the surface rule, the carvers,
    /// the features and the chunk's own beardifier. `nether_fossil` is the only 1.21.1
    /// structure that reads it, and reading the live column instead makes its start
    /// depend on its own beard (measured: seed 4242 chunk (-22,20) places the fossil 23
    /// blocks too high). `None` when the world has no generator, where the live blocks
    /// are the only column a synthetic oracle scene can offer.
    fn base_column(&mut self, x: i32, y: i32, z: i32) -> Option<u32> {
        // A synthetic oracle scene has no generator; its live blocks are the column.
        let _ = (x, y, z);
        None
    }

    fn sea_level(&self) -> i32;
    /// `LevelReader.getRawBrightness(pos, 0)` (`dcz.b(jd,int)`) — the light query
    /// `MushroomBlock.canSurvive` makes. `None` means "this world has no light engine to ask":
    /// a synthetic oracle scene, or the port outside the one phase that has a light sample
    /// (`generation_context::FC::begin_promotion_pass`). A rule that reads it must then leave
    /// the state alone rather than guess, because the uninitialized engine and the lit engine
    /// answer differently at exactly the cells the rule exists for.
    fn raw_brightness(&mut self, pos: Pos) -> Option<u8> {
        let _ = pos;
        None
    }
    /// `LevelAccessor.getMinBuildHeight` — the **world** window (`dimension_type`).
    fn min_y(&self) -> i32;
    /// `LevelHeightAccessor.getHeight` — the **world** window (`dimension_type`).
    fn height(&self) -> i32;
    /// `WorldGenerationContext.getMinGenY()` = `ChunkGenerator.getMinY()` — the
    /// **generator** window (`noise_settings`).
    ///
    /// Vanilla has both and they are not the same object: `LevelHeightAccessor` answers
    /// build-height questions (256 in the Nether and the End), while a `HeightProvider`
    /// samples against the `WorldGenerationContext`, which is built from the chunk
    /// generator (128 there). `nether_fossil` is the one structure whose placement draws
    /// from a `HeightProvider`, and the measured answer is the generator's: the pinned
    /// native start is BB [-356, 55, 319, -350, 61, 324], and sampling the world window
    /// moves it to y 78 (a `next_bound(222)` draw instead of `next_bound(94)`), which the
    /// `nat-4242-nether-fossil` row rejects.
    fn gen_min_y(&self) -> i32 {
        self.min_y()
    }
    fn gen_height(&self) -> i32 {
        self.height()
    }
    /// `LevelAccessor.getRandom().nextInt(bound)` for non-placement level RNG
    /// (e.g. WoodlandMansion `Group of Allays` count), kept separate from the
    /// chunk decoration `RandomSource`.
    fn level_next_bound(&mut self, pos: Pos, bound: i32) -> i32 {
        Lcg::new(hash_block_pos(pos.0, pos.1, pos.2) as u64).next_bound(bound)
    }
    /// `ChunkAccess.markPosForPostprocessing` (`StructurePiece.placeBlock` for
    /// `SHAPE_CHECK_BLOCKS`). A synthetic oracle scene has no owning chunk, so the
    /// default is a no-op and the scripted oracles do not move.
    fn mark_shape(&mut self, _pos: Pos) {}
}

impl TemplateWorld for crate::generation_context::FC<'_> {
    fn get(&mut self, x: i32, y: i32, z: i32) -> u32 {
        crate::generation_context::FC::get(self, x, y, z)
    }
    fn set(&mut self, x: i32, y: i32, z: i32, state: u32) -> bool {
        // `WorldGenRegion.setBlock` refuses by the level's build height (`dimension_type`),
        // not by the generator's: a template piece may place above the terrain top.
        if y < self.g.world_min_y || y >= self.g.world_min_y + self.g.world_height {
            return false;
        }
        crate::generation_context::FC::set(self, x, y, z, state);
        true
    }
    fn heightmap(&mut self, kind: HeightmapKind, x: i32, z: i32) -> i32 {
        crate::generation_context::FC::heightmap(self, kind, x, z)
    }
    fn base_height(&mut self, kind: HeightmapKind, x: i32, z: i32) -> i32 {
        if matches!(kind, HeightmapKind::OceanFloor | HeightmapKind::OceanFloorWg) {
            self.g.top_oc(x, z)
        } else {
            self.g.top_ws(x, z)
        }
    }
    fn base_column(&mut self, x: i32, y: i32, z: i32) -> Option<u32> {
        self.g.base_column_state(x, y, z)
    }
    fn sea_level(&self) -> i32 {
        self.g.sea
    }
    #[expect(clippy::misnamed_getters, reason = "template placement reads the world build window")]
    fn min_y(&self) -> i32 {
        // `LevelAccessor.getMinBuildHeight` - the world window. `base_height`/
        // `base_column` above stay generator-side on purpose (`ChunkGenerator.getBaseHeight`
        // is the noise column, not the chunk).
        self.g.world_min_y
    }
    #[expect(clippy::misnamed_getters, reason = "template placement reads the world build window")]
    fn height(&self) -> i32 {
        // `LevelHeightAccessor.getHeight` - the world window. Every caller here is a
        // vanilla build-height question: `TemplatePlacement`'s `VerticalAnchor`
        // resolution, the shipwreck `getMaxBuildHeight` accumulator, the ocean ruin's
        // floor, `in_spawnable_bounds` and the mansion foundation scan.
        self.g.world_height
    }
    fn gen_min_y(&self) -> i32 {
        self.g.min_y
    }
    fn gen_height(&self) -> i32 {
        self.g.height
    }
    fn raw_brightness(&mut self, pos: Pos) -> Option<u8> {
        crate::generation_context::FC::raw_brightness(self, pos.0, pos.1, pos.2)
    }
    fn mark_shape(&mut self, pos: Pos) {
        self.shape_marks.push(pos);
    }
}

// ---------------------------------------------------------------- settings RNG

/// `StructurePlaceSettings.getRandom(pos)`: the shared stream when one was
/// installed, otherwise a fresh `LegacyRandomSource(Mth.getSeed(pos))`.
///
/// Every `TemplateStructurePiece` and every extended-placement oracle case
/// leaves the settings random unset, so processors are position-seeded; the
/// `block_age` fixture is the one place a shared stream is installed.
pub(crate) enum PosRng<'a> {
    Shared(&'a mut Xor),
    Seeded(Lcg),
}

impl PosRng<'_> {
    pub(crate) fn next_f32(&mut self) -> f32 {
        match self {
            Self::Shared(r) => r.next_f32(),
            Self::Seeded(r) => r.next_f32(),
        }
    }
    pub(crate) fn next_bound(&mut self, bound: i32) -> i32 {
        match self {
            Self::Shared(r) => r.next_bound(bound),
            Self::Seeded(r) => r.next_bound(bound),
        }
    }
}

pub(crate) fn settings_rng<'a>(shared: &'a mut Option<&mut Xor>, pos: Pos) -> PosRng<'a> {
    match shared {
        Some(r) => PosRng::Shared(r),
        None => PosRng::Seeded(Lcg::new(hash_block_pos(pos.0, pos.1, pos.2) as u64)),
    }
}

/// `(level.getRandom().nextLong()).forkPositional().at(pos)` — the stream a
/// `CappedProcessor` draws its limit and shuffle from.
pub(crate) fn positional_from_world_seed(world_seed: u64, pos: Pos) -> Lcg {
    let factory_seed = Lcg::new(world_seed).next_i64();
    Lcg::new((hash_block_pos(pos.0, pos.1, pos.2) ^ factory_seed) as u64)
}

// ---------------------------------------------------------------- settings types

/// The native processor shapes the structures use, plus `block_age`.
#[derive(Clone, Debug)]
pub(crate) enum ExtProc {
    Native(Processor),
    BlockAge { mossiness: f32 },
}

#[derive(Clone, Debug)]
pub(crate) struct Settings {
    pub(crate) mirror: Mirror,
    pub(crate) rotation: Rotation,
    /// `StructurePlaceSettings.rotationPivot`.
    pub(crate) pivot: Pos,
    /// `StructurePlaceSettings.boundingBox` — `None` writes the whole template.
    pub(crate) clip: Option<BB>,
    pub(crate) apply_waterlogging: bool,
    pub(crate) ignore_entities: bool,
    pub(crate) processors: Vec<ExtProc>,
}

impl Settings {
    pub(crate) fn new(mirror: Mirror, rotation: Rotation, pivot: Pos) -> Self {
        Settings {
            mirror,
            rotation,
            pivot,
            clip: None,
            // `StructurePlaceSettings.liquidSettings` defaults to APPLY.
            apply_waterlogging: true,
            ignore_entities: false,
            processors: Vec::new(),
        }
    }
}

/// One generated block entity, in the shape the native save produces.
#[derive(Clone, Debug)]
pub(crate) struct BlockEntityRecord {
    pub(crate) pos: Pos,
    /// Registry id of the block entity type (`minecraft:brushable_block`,
    /// `minecraft:sign`, ...), not the block's id.
    pub(crate) id: &'static str,
    pub(crate) loot_table: Option<String>,
    pub(crate) loot_seed: Option<i64>,
    /// The container `Items` list as `BlockEntity.saveWithoutMetadata` writes it.
    #[cfg_attr(any(not(test), not(megu_vanilla_data)), allow(dead_code))]
    pub(crate) items: Option<Tag>,
    /// Entity id inside `SpawnData.entity.id` for `minecraft:mob_spawner`.
    pub(crate) spawner_entity: Option<String>,
}

/// `blockEntities` is a map keyed by position; a write that creates no block
/// entity clears the entry. The event stream keeps that order observable.
#[derive(Clone, Debug)]
pub(crate) enum BeEvent {
    Set(BlockEntityRecord),
    Clear(Pos),
}

#[derive(Clone, Debug)]
pub(crate) struct EntitySpawn {
    pub(crate) pos: Pos,
    pub(crate) id: String,
    /// `Some` for the hanging entities a template or data marker places: they carry
    /// a facing and a framed stack, which no mob payload can express. `None` for a
    /// living entity or a hull entity (`minecraft:shulker`, `minecraft:boat`, …).
    pub(crate) hanging: Option<HangingSpawn>,
}

/// The two fields a hanging entity adds to a spawn.
///
/// `facing` is `Direction.get3DDataValue()`: 1.21.1 sorts `BY_3D_DATA` on the
/// declaration ordinal, so `north`/`south`/`west`/`east` are 2/3/4/5, and that
/// number is both the `Facing` save byte (`HangingEntity.addAdditionalSaveData`)
/// and the `spawn_entity` object data (`ItemFrame.getAddEntityPacket`).
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct HangingSpawn {
    pub(crate) facing: u8,
    pub(crate) item: Option<(String, u8)>,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct PlacementOut {
    pub(crate) wrote: bool,
    pub(crate) writes: usize,
    pub(crate) be_events: Vec<BeEvent>,
    pub(crate) entities: Vec<EntitySpawn>,
}

/// Route a placement's entity spawns and block-entity events to the generation payload store.
///
/// `SPIPE_PAYLOAD` names every dropped shape so a missing payload is observable
/// rather than assumed.
pub(crate) fn stage_payloads(
    ctx: &mut crate::generation_context::FC<'_>,
    out: PlacementOut,
) {
    for entity in out.entities {
        if let Some(hanging) = entity.hanging {
            // `minecraft:glow_item_frame` is the same entity with a glowing item; the
            // payload carries the flag so the runtime needs no second name table.
            let glow = entity.id == "minecraft:glow_item_frame";
            ctx.push_item_frame(entity.pos, glow, hanging.facing, hanging.item);
            continue;
        }
        ctx.push_entity(
            &entity.id,
            [
                entity.pos.0 as f64 + 0.5,
                entity.pos.1 as f64,
                entity.pos.2 as f64 + 0.5,
            ],
            0.0,
        );
    }
    for event in out.be_events {
        let BeEvent::Set(record) = event else {
            // `Clear` removes a block entity a previous write created; the write
            // itself already replaced the state.
            continue;
        };
        if record.id == "minecraft:mob_spawner" {
            if let Some(entity_id) = record.spawner_entity {
                ctx.push_spawner(record.pos, &entity_id);
                continue;
            }
        }
        let Some(table) = record.loot_table else {
            if crate::trace_env::env_flag!("SPIPE_PAYLOAD") {
                eprintln!(
                    "SPIPE_PAYLOAD unrouted {} at {:?}",
                    record.id, record.pos
                );
            }
            continue;
        };
        let kind = match crate::generation_payload::ContainerKind::for_block(record.id) {
            Some(kind) => kind,
            None => {
                if crate::trace_env::env_flag!("SPIPE_PAYLOAD") {
                    eprintln!("SPIPE_PAYLOAD no store for {} at {:?}", record.id, record.pos);
                }
                continue;
            }
        };
        ctx.payloads.push((
            record.pos,
            crate::generation_payload::GenerationPayload::LootContainer {
                kind,
                loot_table: table,
                loot_seed: record.loot_seed,
            },
        ));
    }
}

/// Registry id of the block entity a written state creates, when it creates one.
pub(crate) fn block_entity_id(name: &str) -> Option<&'static str> {
    let bare = name.strip_prefix("minecraft:").unwrap_or(name);
    Some(match bare {
        "chest" | "trapped_chest" => "minecraft:chest",
        "barrel" => "minecraft:barrel",
        "furnace" => "minecraft:furnace",
        "blast_furnace" => "minecraft:blast_furnace",
        "smoker" => "minecraft:smoker",
        "brewing_stand" => "minecraft:brewing_stand",
        "hopper" => "minecraft:hopper",
        "dispenser" => "minecraft:dispenser",
        "dropper" => "minecraft:dropper",
        "crafter" => "minecraft:crafter",
        "suspicious_sand" | "suspicious_gravel" => "minecraft:brushable_block",
        "structure_block" => "minecraft:structure_block",
        "jigsaw" => "minecraft:jigsaw",
        "beacon" => "minecraft:beacon",
        "conduit" => "minecraft:conduit",
        "ender_chest" => "minecraft:ender_chest",
        "lectern" => "minecraft:lectern",
        "campfire" => "minecraft:campfire",
        "soul_campfire" => "minecraft:soul_campfire",
        "decorated_pot" => "minecraft:decorated_pot",
        "flower_pot" => "minecraft:flower_pot",
        "enchanting_table" => "minecraft:enchanting_table",
        "shulker_box" => "minecraft:shulker_box",
        "spawner" => "minecraft:mob_spawner",
        _ if bare.ends_with("_bed") => "minecraft:bed",
        _ if bare.ends_with("_hanging_sign") => "minecraft:hanging_sign",
        _ if bare.ends_with("_sign") => "minecraft:sign",
        _ if bare.ends_with("_banner") => "minecraft:banner",
        _ if bare.ends_with("_shulker_box") => "minecraft:shulker_box",
        _ if bare.ends_with("_skull") || bare.ends_with("_head") => "minecraft:skull",
        _ => return None,
    })
}

/// `RandomizableContainer` implementors among the placed states.
fn is_randomizable_container(name: &str) -> bool {
    matches!(
        block_entity_id(name),
        Some(
            "minecraft:chest"
                | "minecraft:barrel"
                | "minecraft:shulker_box"
                | "minecraft:hopper"
                | "minecraft:dispenser"
                | "minecraft:dropper"
                | "minecraft:crafter"
        )
    )
}

/// `LiquidBlockContainer`: a block that can be waterlogged in place.
///
/// Keyed on the `waterlogged` property, which is what every
/// `SimpleWaterloggedBlock` uses. `KelpBlock`/`SeagrassBlock` implement the
/// interface without that property; no template in the pinned corpus places
/// them into fluid, so their (refusing) `placeLiquid` is not modelled.
fn is_liquid_container(reg: &BReg, state: u32) -> bool {
    reg.get_prop(state, "waterlogged").is_some()
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum FluidKind {
    Empty,
    Water,
    Lava,
}

#[derive(Clone, Copy)]
struct FluidAt {
    kind: FluidKind,
    source: bool,
}

/// Fluid type plus `isSource`: `placeLiquid` needs the type, the neighbour
/// repair needs the source bit.
fn fluid_at(reg: &BReg, state: u32) -> FluidAt {
    let (kind, source) = match reg.fluid_name(state) {
        "minecraft:water" => (FluidKind::Water, true),
        "minecraft:flowing_water" => (FluidKind::Water, false),
        "minecraft:lava" => (FluidKind::Lava, true),
        "minecraft:flowing_lava" => (FluidKind::Lava, false),
        _ => (FluidKind::Empty, false),
    };
    FluidAt { kind, source }
}

fn nbt_str(tag: &Tag, key: &str) -> Option<String> {
    match tag.get(key) {
        Some(Tag::String(value)) => value.to_utf8().ok(),
        _ => None,
    }
}

fn nbt_int(tag: &Tag, key: &str) -> Option<i32> {
    match tag.get(key) {
        Some(Tag::Int(value)) => Some(*value),
        Some(Tag::Byte(value)) => Some(i32::from(*value)),
        Some(Tag::Short(value)) => Some(i32::from(*value)),
        _ => None,
    }
}

fn nbt_long(tag: &Tag, key: &str) -> Option<i64> {
    match tag.get(key) {
        Some(Tag::Long(value)) => Some(*value),
        _ => None,
    }
}

/// `BlockStateParser.parseForBlock(..., raw, true)` for the `final_state`
/// strings templates carry.
pub(crate) fn parse_block_state(raw: &str, reg: &BReg) -> Option<u32> {
    let raw = crate::block_registry::ns_name(raw);
    let (name, rest) = match raw.split_once('[') {
        Some((name, rest)) => (name, Some(rest.trim_end_matches(']'))),
        None => (raw.as_str(), None),
    };
    let mut properties = Vec::new();
    if let Some(rest) = rest {
        for pair in rest.split(',') {
            let (key, value) = pair.split_once('=')?;
            properties.push((key.to_string(), value.to_string()));
        }
    }
    reg.checked_state(name, &properties)
}

// ---------------------------------------------------------------- BlockAgeProcessor

/// `BlockAgeProcessor.processBlock`. Every draw comes from
/// `settings.getRandom(current.pos())`; a `None` result keeps the original.
pub(crate) fn block_age(rng: &mut PosRng<'_>, current: &mut BlockInfo, mossiness: f32, reg: &BReg) {
    let next = match reg.name_of(current.state) {
        "minecraft:stone_bricks" | "minecraft:stone" | "minecraft:chiseled_stone_bricks" => {
            mossify_stone(rng, mossiness, reg)
        }
        _ if reg.in_tag(current.state, "minecraft:stairs") => {
            mossify_stairs(rng, mossiness, current.state, reg)
        }
        _ if reg.in_tag(current.state, "minecraft:slabs") => {
            mossy_if(rng, mossiness, "minecraft:mossy_stone_brick_slab", reg)
        }
        _ if reg.in_tag(current.state, "minecraft:walls") => {
            mossy_if(rng, mossiness, "minecraft:mossy_stone_brick_wall", reg)
        }
        "minecraft:obsidian" => {
            if rng.next_f32() < 0.15 {
                Some(reg.state_of("minecraft:crying_obsidian", None))
            } else {
                None
            }
        }
        _ => None,
    };
    if let Some(state) = next {
        current.state = state;
    }
}

fn mossy_if(rng: &mut PosRng<'_>, mossiness: f32, block: &str, reg: &BReg) -> Option<u32> {
    (rng.next_f32() < mossiness).then(|| reg.state_of(block, None))
}

/// `BlockAgeProcessor.mossifyStone`.
fn mossify_stone(rng: &mut PosRng<'_>, mossiness: f32, reg: &BReg) -> Option<u32> {
    if rng.next_f32() >= 0.5 {
        return None;
    }
    // Java builds both candidate arrays before choosing, so a losing staircase
    // still consumes its own gate (and face/half draws when that gate passes).
    let cracked_stairs = random_stairs(rng, "minecraft:stone_brick_stairs", reg);
    let mossy_stairs = random_stairs(rng, "minecraft:mossy_stone_brick_stairs", reg);
    let cracked = [
        Some(reg.state_of("minecraft:cracked_stone_bricks", None)),
        cracked_stairs,
    ];
    let mossy = [
        Some(reg.state_of("minecraft:mossy_stone_bricks", None)),
        mossy_stairs,
    ];
    let table: &[Option<u32>] = if rng.next_f32() < mossiness { &mossy } else { &cracked };
    table[rng.next_bound(table.len() as i32) as usize]
}

/// `BlockAgeProcessor.mossifyStairs`.
fn mossify_stairs(
    rng: &mut PosRng<'_>,
    mossiness: f32,
    state: u32,
    reg: &BReg,
) -> Option<u32> {
    let facing = reg.get_prop(state, "facing");
    let half = reg.get_prop(state, "half");
    if rng.next_f32() >= 0.5 {
        return None;
    }
    let stairs = match (&facing, &half) {
        (Some(facing), Some(half)) => reg.checked_state(
            "minecraft:mossy_stone_brick_stairs",
            &[("facing".to_string(), facing.clone()), ("half".to_string(), half.clone())],
        ),
        _ => None,
    };
    let static_candidates = [
        Some(reg.state_of("minecraft:stone_slab", None)),
        Some(reg.state_of("minecraft:stone_brick_slab", None)),
    ];
    let mossy_candidates = [
        stairs,
        Some(reg.state_of("minecraft:mossy_stone_brick_slab", None)),
    ];
    let table: &[Option<u32>] = if rng.next_f32() < mossiness {
        &mossy_candidates
    } else {
        &static_candidates
    };
    table[rng.next_bound(table.len() as i32) as usize]
}

/// `BlockAgeProcessor.randomStair`: `Direction.Plane.HORIZONTAL.getRandom`
/// over `[NORTH, EAST, SOUTH, WEST]`, then `Util.getRandom(Half.values())`
/// over `[TOP, BOTTOM]`. Partial `state_of` would start from the id lattice's
/// first value, not the block's default state, so use `checked_state`.
fn random_stairs(rng: &mut PosRng<'_>, block: &str, reg: &BReg) -> Option<u32> {
    // `BlockAgeProcessor.randomStair(Block)` has no gate: it draws the facing
    // and half directly. Only the *state*-based `mossifyStairs` gates at 0.5.
    const FACINGS: [&str; 4] = ["north", "east", "south", "west"];
    let facing = FACINGS[rng.next_bound(4) as usize];
    let half = if rng.next_bound(2) == 0 { "top" } else { "bottom" };
    Some(
        reg.checked_state(block, &[("facing".to_string(), facing.to_string()), ("half".to_string(), half.to_string())])
            .unwrap_or_else(|| panic!("unknown stairs state {block}")),
    )
}

// ---------------------------------------------------------------- processBlockInfos

/// Native `StructureTemplate.processBlockInfos` over the shared processor
/// vocabulary. `pivot` is the `placeInWorld` pivot argument, which position
/// predicates read; it is not the settings rotation pivot.
#[allow(clippy::too_many_arguments)]
pub(crate) fn process_block_infos<W: TemplateWorld>(
    world: &mut W,
    template: &Template,
    palette: usize,
    origin: Pos,
    pivot: Pos,
    settings: &Settings,
    shared: &mut Option<&mut Xor>,
    world_seed: u64,
    reg: &BReg,
) -> Result<Vec<BlockInfo>, &'static str> {
    let blocks = template
        .palettes
        .get(palette)
        .ok_or("palette outside template")?;
    let world_cell = std::cell::RefCell::new(&mut *world);
    let rng_cell = std::cell::RefCell::new(&mut *shared);
    let mut originals = Vec::new();
    let mut result = Vec::new();
    for original in blocks {
        let p = transform_pos(original.pos, settings.mirror, settings.rotation, settings.pivot);
        let mut current = original.clone();
        current.pos = (
            p.0.wrapping_add(origin.0),
            p.1.wrapping_add(origin.1),
            p.2.wrapping_add(origin.2),
        );
        let mut keep = true;
        for processor in &settings.processors {
            match processor {
                ExtProc::BlockAge { mossiness } => {
                    let mut guard = rng_cell.borrow_mut();
                    let mut rng = settings_rng(&mut guard, current.pos);
                    block_age(&mut rng, &mut current, *mossiness, reg);
                }
                ExtProc::Native(processor) => {
                    let mut world_fn = |p: Pos| {
                        let mut c = world_cell.borrow_mut();
                        c.get(p.0, p.1, p.2)
                    };
                    let mut height_fn = |kind: HeightmapKind, x: i32, z: i32| {
                        let mut c = world_cell.borrow_mut();
                        c.heightmap(kind, x, z)
                    };
                    let mut draw = |p: Pos| {
                        let mut guard = rng_cell.borrow_mut();
                        let mut rng = settings_rng(&mut guard, p);
                        rng.next_f32()
                    };
                    if !apply_single_processor(
                        processor,
                        original,
                        &mut current,
                        pivot,
                        reg,
                        &mut world_fn,
                        &mut draw,
                        &mut height_fn,
                    ) {
                        keep = false;
                        break;
                    }
                }
            }
        }
        if keep {
            originals.push(original.clone());
            result.push(current);
        }
    }
    // `CappedProcessor.finalizeProcessing`: the limit and the index shuffle draw
    // from `level.getRandom().forkPositional().at(pos)`, and only the delegate's
    // changed cells count against the cap.
    for processor in &settings.processors {
        let ExtProc::Native(Processor::Capped { delegate, limit }) = processor else {
            continue;
        };
        if limit.max_inclusive() == 0 || result.is_empty() {
            continue;
        }
        if result.len() != originals.len() {
            continue;
        }
        let mut cap_rng = positional_from_world_seed(world_seed, origin);
        let cap = sample_ip_lcg(limit, &mut cap_rng).min(result.len() as i32);
        if cap < 1 {
            continue;
        }
        let mut indices: Vec<usize> = (0..result.len()).collect();
        for i in (2..=indices.len()).rev() {
            let j = cap_rng.next_bound(i as i32) as usize;
            indices.swap(i - 1, j);
        }
        let world_cell = std::cell::RefCell::new(&mut *world);
        let rng_cell = std::cell::RefCell::new(&mut *shared);
        let mut replaced = 0;
        for idx in indices {
            if replaced >= cap {
                break;
            }
            let mut candidate = result[idx].clone();
            let mut world_fn = |p: Pos| {
                let mut c = world_cell.borrow_mut();
                c.get(p.0, p.1, p.2)
            };
            let mut height_fn = |kind: HeightmapKind, x: i32, z: i32| {
                let mut c = world_cell.borrow_mut();
                c.heightmap(kind, x, z)
            };
            let mut draw = |p: Pos| {
                let mut guard = rng_cell.borrow_mut();
                let mut rng = settings_rng(&mut guard, p);
                rng.next_f32()
            };
            let kept = apply_single_processor(
                delegate,
                &originals[idx],
                &mut candidate,
                pivot,
                reg,
                &mut world_fn,
                &mut draw,
                &mut height_fn,
            );
            if kept
                && (candidate.pos != result[idx].pos
                    || candidate.state != result[idx].state
                    || candidate.nbt != result[idx].nbt)
            {
                replaced += 1;
                result[idx] = candidate;
            }
        }
    }
    Ok(result)
}

// ---------------------------------------------------------------- placeInWorld

/// Native `StructureTemplate.placeInWorld` for one template.
///
/// `rng` is the `random` argument (used for `LootTableSeed`); processor and
/// palette streams come from `StructurePlaceSettings.getRandom(pos)`, which is
/// always position-seeded for the structures here.
#[allow(clippy::too_many_arguments)]
pub(crate) fn place_in_world<W: TemplateWorld>(
    world: &mut W,
    template: &Template,
    origin: Pos,
    pivot: Pos,
    settings: &Settings,
    rng: &mut Xor,
    world_seed: u64,
    reg: &BReg,
) -> Result<PlacementOut, &'static str> {
    let mut out = PlacementOut::default();
    if template.palettes.is_empty() {
        return Ok(out);
    }
    let palette = {
        let mut no_shared: Option<&mut Xor> = None;
        let mut pick = settings_rng(&mut no_shared, origin);
        pick.next_bound(template.palettes.len() as i32) as usize
    };
    if template.palettes[palette].is_empty()
        || template.size.0 < 1
        || template.size.1 < 1
        || template.size.2 < 1
    {
        return Ok(out);
    }
    let mut no_shared: Option<&mut Xor> = None;
    let processed = process_block_infos(
        world,
        template,
        palette,
        origin,
        pivot,
        settings,
        &mut no_shared,
        world_seed,
        reg,
    )?;
    if processed.is_empty() {
        return Ok(out);
    }
    let mut written: Vec<Pos> = Vec::new();
    let mut pending: Vec<Pos> = Vec::new();
    let mut no_source: Vec<Pos> = Vec::new();
    for block in processed {
        if let Some(clip) = &settings.clip {
            if !bb_inside(clip, block.pos) {
                continue;
            }
        }
        let state = crate::structure_transform::state(block.state, settings.mirror, settings.rotation)
            .ok_or("unknown template state transform")?;
        let state_name = reg.name_of(state).to_owned();
        // `getFluidState` is read before the write, and only when
        // waterlogging is enabled.
        let before = if settings.apply_waterlogging {
            fluid_at(reg, world.get(block.pos.0, block.pos.1, block.pos.2))
        } else {
            FluidAt {
                kind: FluidKind::Empty,
                source: false,
            }
        };
        let mut tag = block.nbt.clone();
        if tag.is_some() {
            // `Clearable.tryClear(existing)` then the BARRIER pre-clear. The
            // barrier state is overwritten by this same write, so it cannot be
            // observed afterwards; the block entity entry it clears does.
            out.be_events.push(BeEvent::Clear(block.pos));
        }
        if !world.set(block.pos.0, block.pos.1, block.pos.2, state) {
            continue;
        }
        if reg.needs_postprocess_shape(state) {
            world.mark_shape(block.pos);
        }
        out.writes += 1;
        written.push(block.pos);
        if let Some(tag) = tag.as_mut() {
            if is_randomizable_container(&state_name) {
                let seed = rng.next_i64();
                if let Tag::Compound(map) = tag {
                    map.insert(NbtString::text("LootTableSeed"), Tag::Long(seed));
                }
            }
            out.be_events
                .push(BeEvent::Set(block_entity_record(block.pos, &state_name, tag)));
        } else if let Some(id) = block_entity_id(&state_name) {
            out.be_events.push(BeEvent::Set(BlockEntityRecord {
                pos: block.pos,
                id,
                loot_table: None,
                loot_seed: None,
                items: None,
                spawner_entity: None,
            }));
        } else {
            // A successful write whose state creates no block entity removes the
            // entry the harness held for that cell.
            out.be_events.push(BeEvent::Clear(block.pos));
        }
        if settings.apply_waterlogging {
            // `state.getFluidState().isSource()` → this write is a fluid source
            // cell others must not take fluid from. Otherwise a liquid container
            // tries `placeLiquid` with whatever was already in the cell, and when
            // that fluid was not a source the cell is queued for the neighbour
            // repair below.
            if fluid_at(reg, state).source {
                no_source.push(block.pos);
            } else if is_liquid_container(reg, state) {
                if before.kind == FluidKind::Water {
                    waterlog(world, block.pos, state);
                }
                if !before.source {
                    pending.push(block.pos);
                }
            }
        }
    }
    // `PlaceInWorld`'s neighbour-source repair for a liquid container that was
    // placed into flowing fluid.
    if !pending.is_empty() {
        let mut progress = true;
        while progress && !pending.is_empty() {
            progress = false;
            let mut index = 0;
            while index < pending.len() {
                let p = pending[index];
                let here = fluid_at(reg, world.get(p.0, p.1, p.2));
                let mut found = here.source;
                // `Direction[] {UP, NORTH, SOUTH, WEST, EAST}`.
                for direction in [1u8, 2, 3, 4, 5] {
                    if found {
                        break;
                    }
                    let (dx, dy, dz) = direction_delta(direction);
                    let q = (p.0.wrapping_add(dx), p.1.wrapping_add(dy), p.2.wrapping_add(dz));
                    let other = fluid_at(reg, world.get(q.0, q.1, q.2));
                    if other.source && !no_source.contains(&q) {
                        found = true;
                    }
                }
                if found {
                    let current = world.get(p.0, p.1, p.2);
                    if is_liquid_container(reg, current) {
                        waterlog(world, p, current);
                        progress = true;
                        pending.remove(index);
                        continue;
                    }
                }
                index += 1;
            }
        }
    }
    // `updateShapeAtEdge` over the exposed faces of the written voxels, then the
    // per-cell `updateFromNeighbourShapes` pass (flag 18).
    for (p, state) in apply_shape_passes(&written, world, reg) {
        record_shape_write(&mut out, reg, p, state);
    }
    if !settings.ignore_entities {
        place_entities(world, template, origin, settings, &mut out);
    }
    out.wrote = true;
    Ok(out)
}

/// `StructureTemplate.placeInWorld`'s two shape passes, in native order:
/// `updateShapeAtEdge(level, flags, writtenShape, min)` over every exposed face
/// of the written voxels, then `BlockState.updateFromNeighbourShapes` on each
/// written cell.
///
/// Native runs these for *every* caller of `placeInWorld`, not only for the
/// template-structure pieces: a `TemplateStructurePiece` (jigsaw) placement does
/// the same two passes, which is how a fence, pane, wall or stair placed by an
/// earlier piece picks up a connection to a block a later piece writes beside
/// it. Both callers share this function so the order cannot drift apart.
///
/// Returns the cells the passes changed, in write order, so a caller can mirror
/// the `setBlock` side effect on its own bookkeeping (block entities above,
/// payload/behaviour maps for the jigsaw path).
pub(crate) fn apply_shape_passes<W: TemplateWorld>(
    written: &[Pos],
    world: &mut W,
    reg: &BReg,
) -> Vec<(Pos, u32)> {
    let mut changed: Vec<(Pos, u32)> = Vec::new();
    if written.is_empty() {
        return changed;
    }
    for (direction, p, q) in crate::voxel_faces::exposed_faces(written.iter().copied()) {
        let mut inside = world.get(p.0, p.1, p.2);
        let outside = world.get(q.0, q.1, q.2);
        let next_inside = update_shape(reg, inside, direction, outside, world, p);
        if next_inside != inside {
            world.set(p.0, p.1, p.2, next_inside);
            changed.push((p, next_inside));
            inside = next_inside;
        }
        let next_outside = update_shape(reg, outside, direction ^ 1, inside, world, q);
        if next_outside != outside {
            world.set(q.0, q.1, q.2, next_outside);
            changed.push((q, next_outside));
        }
    }
    for &p in written {
        let current = world.get(p.0, p.1, p.2);
        let updated = update_from_neighbour_shapes(reg, current, world, p);
        if updated != current {
            world.set(p.0, p.1, p.2, updated);
            changed.push((p, updated));
        }
    }
    changed
}

/// The block-entity side of a `setBlock` the shape passes perform.
///
/// Native routes every shape write through `LevelAccessor.setBlock`, which
/// creates or removes the cell's block entity; a wall banner whose support is
/// gone is written as air that way, so the banner's block entity has to leave
/// the map too. The update loops above bypass the main write path, so they
/// record the same event here.
fn record_shape_write(out: &mut PlacementOut, reg: &BReg, pos: Pos, state: u32) {
    match block_entity_id(reg.name_of(state)) {
        Some(id) => out.be_events.push(BeEvent::Set(BlockEntityRecord {
            pos,
            id,
            loot_table: None,
            loot_seed: None,
            items: None,
            spawner_entity: None,
        })),
        None => out.be_events.push(BeEvent::Clear(pos)),
    }
}

/// `SimpleWaterloggedBlock.placeLiquid`: set `waterlogged=true` and schedule.
fn waterlog<W: TemplateWorld>(world: &mut W, pos: Pos, state: u32) -> u32 {
    let reg = match breg() {
        Some(reg) => reg,
        None => return state,
    };
    if reg.get_prop(state, "waterlogged").as_deref() != Some("false") {
        return state;
    }
    // `SlabBlock.placeLiquid` refuses a DOUBLE slab; every other simple
    // waterlogged block accepts source water.
    if reg.name_of(state).ends_with("_slab")
        && reg.get_prop(state, "type").as_deref() == Some("double")
    {
        return state;
    }
    let next = reg.with_prop(state, "waterlogged", "true");
    world.set(pos.0, pos.1, pos.2, next);
    next
}

/// The saved-tag view of one block entity: loot table + seed, or the container
/// `Items` list re-encoded the way `ItemStack.save` writes it.
fn block_entity_record(pos: Pos, state_name: &str, tag: &Tag) -> BlockEntityRecord {
    let id = block_entity_id(state_name).unwrap_or("minecraft:chest");
    let loot_table = nbt_str(tag, "LootTable");
    let loot_seed = nbt_long(tag, "LootTableSeed");
    let items = if loot_table.is_some() {
        None
    } else {
        saved_items(tag)
    };
    let spawner_entity = if id == "minecraft:mob_spawner" {
        tag.get("SpawnData")
            .and_then(|d| d.get("entity"))
            .and_then(|e| nbt_str(e, "id"))
    } else {
        None
    };
    BlockEntityRecord {
        pos,
        id,
        loot_table,
        loot_seed,
        items,
        spawner_entity,
    }
}

/// `ContainerHelper.saveAllItems` + `ItemStack.save`: entries carry exactly
/// `Slot`, `id`, `count` and (when present) `components`.
fn saved_items(tag: &Tag) -> Option<Tag> {
    let Tag::List(_, entries) = tag.get("Items")? else {
        return None;
    };
    if entries.is_empty() {
        return None;
    }
    let mut out = Vec::with_capacity(entries.len());
    for entry in entries {
        let Tag::Compound(values) = entry else {
            continue;
        };
        let mut saved = BTreeMap::new();
        for key in values.keys() {
            let key_text = match key.to_utf8() {
                Ok(text) => text,
                Err(_) => continue,
            };
            match key_text.as_str() {
                "Slot" => {
                    if let Some(Tag::Byte(slot)) = entry.get("Slot") {
                        saved.insert(NbtString::text("Slot"), Tag::Byte(*slot));
                    }
                }
                "id" => {
                    if let Some(Tag::String(id)) = entry.get("id") {
                        saved.insert(NbtString::text("id"), Tag::String(id.clone()));
                    }
                }
                "count" => {
                    if let Some(count) = nbt_int(entry, "count") {
                        saved.insert(NbtString::text("count"), Tag::Int(count));
                    }
                }
                "components" => {
                    if let Some(Tag::Compound(_)) = entry.get("components") {
                        saved.insert(
                            NbtString::text("components"),
                            entry.get("components").unwrap().clone(),
                        );
                    }
                }
                _ => {}
            }
        }
        out.push(Tag::Compound(saved));
    }
    Some(Tag::List(10, out))
}

/// Native `placeEntities`: transform the stored doubles, filter by the clip
/// box and add each entity that this harness can construct.
fn place_entities<W: TemplateWorld>(
    _world: &mut W,
    template: &Template,
    origin: Pos,
    settings: &Settings,
    out: &mut PlacementOut,
) {
    for info in &template.entities {
        let block = transform_pos(
            info.block_pos,
            settings.mirror,
            settings.rotation,
            settings.pivot,
        );
        let block = (
            block.0.wrapping_add(origin.0),
            block.1.wrapping_add(origin.1),
            block.2.wrapping_add(origin.2),
        );
        if let Some(clip) = &settings.clip {
            if !bb_inside(clip, block) {
                continue;
            }
        }
        let Some(id) = nbt_str(&info.nbt, "id") else {
            continue;
        };
        // Measured limit of the native oracle harness: it builds entities
        // against `minimalWorldServerLevel`, whose registry access has no
        // dynamic registries. A villager's read path resolves the villager
        // type/profession holders and the entity is never constructed (the
        // igloo basement's two entities produce exactly one spawn), while
        // zombie_villager stores the record un-resolved.
        if id == "minecraft:villager" {
            continue;
        }
        let hanging = hanging_spawn(&info.nbt, settings.mirror, settings.rotation);
        out.entities.push(EntitySpawn {
            pos: block,
            id,
            hanging,
        });
    }
}

/// The hanging-entity fields a template's entity NBT carries.
///
/// `StructureTemplate.placeInWorld` loads the entity from its NBT — so `Facing`
/// becomes `HangingEntity`'s direction — and then applies `Entity.rotate(rotation)`
/// to it, which `HangingEntity` overrides by turning the direction with the very
/// same table `Rotation.rotate(Direction)` uses (1.21.1 `ciz.a(dmm)`:
/// `CLOCKWISE_90 -> getClockWise`). The facing therefore rotates with the blocks.
///
/// `Item` is the framed stack; `ItemRotation` is the item's turn inside the frame.
pub(crate) fn hanging_spawn(
    nbt: &Tag,
    mirror: Mirror,
    rotation: crate::structure_transform::Rotation,
) -> Option<HangingSpawn> {
    let Some(Tag::Byte(byte)) = nbt.get("Facing") else {
        return None;
    };
    let direction = direction_from_3d_data_value(*byte as u8)?;
    // `HangingEntity.setDirection` validates a horizontal direction, and the only
    // directions the 3D values can name here are the six face directions.
    if !matches!(direction, Dir::North | Dir::South | Dir::West | Dir::East) {
        return None;
    }
    let item = nbt.get("Item").and_then(|stack| {
        let id = nbt_str(stack, "id")?;
        let count = stack.get("count").and_then(Tag::int).unwrap_or(1);
        let count = u8::try_from(count.clamp(0, 255)).ok()?;
        (count > 0).then_some((id, count))
    });
    Some(HangingSpawn {
        facing: rotate_direction(rotation, mirrored_direction(mirror, direction)).data_3d(),
        item,
    })
}

/// `Mirror.getRotation(Direction)` resolved onto the direction it turns: 1.21.1
/// returns `CLOCKWISE_180` exactly for a direction that lies on the axis the mirror
/// flips (`dkv.a(ji)`), and `StructureTemplate.placeInWorld` applies it as a second
/// `Entity.rotate`, i.e. the opposite direction. This port's mirror names follow
/// `from_index` (native indices), so `LeftRight` flips z like native `LEFT_RIGHT`
/// and turns north/south, and `FrontBack` flips x and turns west/east.
fn mirrored_direction(mirror: Mirror, direction: Dir) -> Dir {
    match (mirror, direction) {
        (Mirror::LeftRight, Dir::North | Dir::South) => direction.opposite(),
        (Mirror::FrontBack, Dir::West | Dir::East) => direction.opposite(),
        _ => direction,
    }
}

// ---------------------------------------------------------------- updateShape

/// `BlockBehaviour.updateShape`, bounded to the families the templates and
/// their neighbours use. Direction indices are DOWN/UP/NORTH/SOUTH/WEST/EAST.
fn update_shape<W: TemplateWorld>(
    reg: &BReg,
    state: u32,
    direction: u8,
    neighbor: u32,
    world: &mut W,
    pos: Pos,
) -> u32 {
    let name = reg.name_of(state);
    let air = reg.state_of("minecraft:air", None);
    // `WallBannerBlock.updateShape`: a wall banner hangs on the block behind
    // it, so a change on that side with a block that fails `canSurvive` drops it
    // to air. `BannerBlock.canSurvive` asks the legacy `isSolid` heuristic of
    // the backing block, not `isSolidRender` or face sturdiness.
    if name
        .strip_prefix("minecraft:")
        .unwrap_or(name)
        .ends_with("_wall_banner")
    {
        if let Some(facing) = reg.get_prop(state, "facing") {
            let facing = match facing.as_str() {
                "down" => Some(0u8),
                "up" => Some(1),
                "north" => Some(2),
                "south" => Some(3),
                "west" => Some(4),
                "east" => Some(5),
                _ => None,
            };
            if facing == Some(direction ^ 1) && !reg.is_legacy_solid(neighbor) {
                return air;
            }
        }
    }
    // `SnowyDirtBlock.updateShape` (`dnj.a(dtc,ji,dtc,dcx,jd,jd)`):
    //
    // ```java
    // if (direction == Direction.UP)
    //     return state.setValue(SNOWY, isSnowySetting(neighborState));
    // return super.updateShape(...);
    // // isSnowySetting(state) = state.is(BlockTags.SNOW)   (`awe.bA`)
    // ```
    //
    // Only three blocks in the pinned 1.21.1 state table carry `snowy`
    // (`blocks_1211.json`): `grass_block`, `podzol` and `mycelium`, so the property
    // is the family predicate — a name list would have to be kept in step with the
    // table. `#minecraft:snow` is `snow`, `snow_block` and `powder_snow` (the
    // vanilla tag), which is why the igloo's bottom `snow_block` marks the
    // `grass_block` directly under it.
    //
    // Both directions of the change arrive here: `apply_shape_passes` asks each
    // side of an exposed face, so placing a `snow_block` above grass answers `UP`
    // on the grass (`false -> true`, the igloo floor) and removing it answers `UP`
    // with air or stone (`true -> false`). Every other direction is the vanilla
    // `super.updateShape(...)`, which this port models with the generic arms below.
    if let Some(snowy) = reg.get_prop(state, "snowy") {
        if direction == 1 {
            let want = if reg.in_tag(neighbor, "minecraft:snow") {
                "true"
            } else {
                "false"
            };
            if snowy != want {
                return reg.with_prop(state, "snowy", want);
            }
        }
    }
    if let Some(next) = update_connection_shapes(reg, state, direction, neighbor, world, pos) {
        return next;
    }
    let bare = name.strip_prefix("minecraft:").unwrap_or(name);
    // `TallSeagrassBlock` inherits `DoublePlantBlock.updateShape` (`dig.a`):
    // when the neighbouring half changes, a mismatched vertical partner is
    // replaced by air. A template's edge-shape pass can expose an upper half
    // over gravel before the water fluid tick restores the emptied cell.
    if bare == "tall_seagrass" {
        let half = reg.get_prop(state, "half");
        let partner_direction = match half.as_deref() {
            Some("upper") => Some(0),
            Some("lower") => Some(1),
            _ => None,
        };
        if partner_direction == Some(direction)
            && (reg.name_of(neighbor) != name || reg.get_prop(neighbor, "half") == half)
        {
            return air;
        }
        // `SeagrassBlock.canSurvive` (`dnc.a`) for the lower half: the block
        // below must offer a FULL sturdy UP face, with magma explicitly
        // excluded.
        if half.as_deref() == Some("lower")
            && direction == 0
            && (!is_face_sturdy(reg, neighbor, 1) || reg.name_of(neighbor) == "minecraft:magma_block")
        {
            return air;
        }
    }
    // `DoorBlock.updateShape`: the vertical partner copies the partner's state
    // (keeping this half); a lower half whose `canSurvive` fails becomes air -
    // `DoorBlock.canSurvive` is the block below's `isFaceSturdy(UP)` (FULL), so
    // a bottom slab or a fence also drops it, not only air or liquid.
    if bare.ends_with("_door") {
        let half = reg.get_prop(state, "half");
        let lower = half.as_deref() == Some("lower");
        let partner = (direction == 1 && lower) || (direction == 0 && !lower);
        if !partner {
            if lower && direction == 0 && !is_face_sturdy(reg, neighbor, 1) {
                return air;
            }
        } else if reg.name_of(neighbor).ends_with("_door")
            && reg.get_prop(neighbor, "half") != half
        {
            return match &half {
                Some(half) => reg.with_prop(neighbor, "half", half),
                None => air,
            };
        } else {
            return air;
        }
    }
    // `ChestBlock.updateShape`: horizontal neighbor updates connect/disconnect
    // `type` (`single` / `left` / `right`) for double chests.
    if (bare == "chest" || bare == "trapped_chest") && matches!(direction, 2..=5) {
        let chest_dir = |facing: &str, ctype: &str| -> Option<u8> {
            let f = match facing {
                "north" => 2u8,
                "south" => 3,
                "west" => 4,
                "east" => 5,
                _ => return None,
            };
            Some(match (f, ctype) {
                (2, "left") | (3, "right") => 5,  // east
                (2, "right") | (3, "left") => 4,  // west
                (4, "left") | (5, "right") => 2,  // north
                (4, "right") | (5, "left") => 3,  // south
                _ => return None,
            })
        };
        if let (Some(facing), Some(ctype)) = (reg.get_prop(state, "facing"), reg.get_prop(state, "type")) {
            if reg.name_of(neighbor) == name {
                if ctype == "single" {
                    if let (Some(nf), Some(nt)) = (reg.get_prop(neighbor, "facing"), reg.get_prop(neighbor, "type")) {
                        if nt != "single" && nf == facing && chest_dir(&nf, &nt) == Some(direction ^ 1) {
                            let opposite_type = if nt == "left" { "right" } else { "left" };
                            return reg.with_prop(state, "type", opposite_type);
                        }
                    }
                }
            } else if ctype != "single" && chest_dir(&facing, &ctype) == Some(direction) {
                return reg.with_prop(state, "type", "single");
            }
        }
    }
    // `CropBlock.canSurvive` / `BushBlock.updateShape`: during worldgen (unlit
    // chunk / `useUninitializedLightEngine`), `getRawBrightness < 8`, so crops
    // fail `canSurvive` and drop to air on shape update.
    if matches!(
        bare,
        "wheat" | "carrots" | "potatoes" | "beetroots" | "torchflower_crop" | "pitcher_crop"
    ) {
        return air;
    }
    // `MushroomBlock` inherits `BushBlock.updateShape` (`dgh.a(dtc,ji,dtc,dcx,jd,jd)`):
    // `!state.canSurvive(level, pos) ? AIR : super.updateShape(...)`, and its own
    // `canSurvive` (`dla.a(dtc,dcz,jd)`) is direction-independent:
    //
    // ```java
    // support.is(MUSHROOM_GROW_BLOCK)
    //    || (level.getRawBrightness(pos, 0) < 13 && isSolidRender(support, below))
    // ```
    //
    // At promotion the survival check reads the lit engine, so a nearby light
    // source can remove a mushroom that survived feature placement. A world
    // without a promotion light sample keeps the placed state.
    if matches!(bare, "brown_mushroom" | "red_mushroom") {
        let below = world.get(pos.0, pos.1 - 1, pos.2);
        if reg.in_tag(below, "minecraft:mushroom_grow_block") {
            return state;
        }
        let lit = match world.raw_brightness(pos) {
            Some(v) => v,
            None => return state,
        };
        return if lit < 13 && reg.is_solid_render(below) {
            state
        } else {
            air
        };
    }
    // `MultifaceBlock.updateShape` — `glow_lichen` and `sculk_vein` are the only
    // two subclasses in the pinned version. A face whose support block stops
    // being attachable is cleared, and the cell becomes air once its last face
    // is gone; a cell with no faces at all is already air. `canAttachTo` is the
    // neighbour's *support* shape **or** its *collision* shape covering that
    // face (`Block.isFaceFull`), which is the pinned `multiface_support` table,
    // indexed by the direction the face looks at.
    if matches!(bare, "glow_lichen" | "sculk_vein") {
        if !multiface_has_face(reg, state) {
            return air;
        }
        let key = match direction {
            0 => "down",
            1 => "up",
            2 => "north",
            3 => "south",
            4 => "west",
            _ => "east",
        };
        if reg.get_prop(state, key).as_deref() == Some("true")
            && !reg.supports_multiface(neighbor, direction)
        {
            let next = reg.with_prop(state, key, "false");
            return if multiface_has_face(reg, next) { next } else { air };
        }
        return state;
    }
    if let Some(next) = reg.connection_after_neighbor(state, neighbor, 0, direction) {
        return next;
    }
    // `TorchBlock.updateShape`/`LanternBlock.updateShape`/`WallTorchBlock.updateShape`
    // (`dhf.a`/`djm.a`/`dom.a`): `direction == attach && !canSurvive -> AIR`.
    // `canSurvive` re-asks the support face, so a support replaced by a
    // non-sturdy block - not only by air or liquid - also drops the block. The
    // registry owns each family's attachment direction and `Support` plane
    // (`attached_support`): CENTER below/above for floor torches and lanterns,
    // FULL for wall torches.
    if let Some((attach, plane)) = reg.attached_support(state) {
        if attach == direction && !reg.is_face_sturdy(neighbor, direction ^ 1, plane) {
            return air;
        }
    }
    // `LadderBlock.updateShape` (`dke.a`): the block on `facing.getOpposite()`
    // must offer the FULL sturdy face toward the ladder.
    if name == "minecraft:ladder" {
        if let Some(attach) = facing_dir(reg, state).map(|f| f ^ 1) {
            if direction == attach && !is_face_sturdy(reg, neighbor, direction ^ 1) {
                return air;
            }
        }
    }
    // `FaceAttachedHorizontalDirectionalBlock.updateShape` (`dgz.a`): buttons,
    // levers and grindstones drop when the cell on `getConnectedDirection()
    // .getOpposite()` changes and `canAttach` fails; `canAttach` is the
    // three-argument `isFaceSturdy` (FULL) on the support's facing side.
    if bare.ends_with("_button") || matches!(bare, "lever" | "grindstone") {
        if let Some(attach) = attach_face_direction(reg, state) {
            if direction == attach && !is_face_sturdy(reg, neighbor, direction ^ 1) {
                return air;
            }
        }
    }
    // `CarpetBlock.updateShape` (`cyn.a`): `!canSurvive -> AIR` on every
    // direction; `canSurvive` is `!level.isEmptyBlock(below)`, so a liquid floor
    // still counts as occupied.
    if bare.ends_with("_carpet")
        && reg.is_air(world.get(pos.0, pos.1.wrapping_sub(1), pos.2))
    {
        return air;
    }
    // `SnowLayerBlock.updateShape` (`dms.a`): `!canSurvive -> AIR` on every
    // direction; `canSurvive` is `#minecraft:snow_layer_can_survive_on`
    // (honey/soul_sand/mud) below or a FULL sturdy UP face that is not
    // `#minecraft:snow_layer_cannot_survive_on` (ice/packed_ice/barrier).
    if bare == "snow" {
        let below = world.get(pos.0, pos.1.wrapping_sub(1), pos.2);
        let full_snow_below = reg.name_of(below) == "minecraft:snow"
            && reg.get_prop(below, "layers").as_deref() == Some("8");
        if !reg.in_tag(below, "minecraft:snow_layer_can_survive_on")
            && !full_snow_below
            && (!is_face_sturdy(reg, below, 1)
                || reg.in_tag(below, "minecraft:snow_layer_cannot_survive_on"))
        {
            return air;
        }
    }
    // `BaseFireBlock.updateShape` (`dgc.a`): `!canSurvive -> AIR` on every
    // direction; `canSurvive` is a FULL sturdy UP face below (the portal-frame
    // limb `isPortal` is not modelled - portal frames write `nether_portal`,
    // never `fire`). `SoulFireBlock.canSurvive` (`dnz.a`) narrows the support
    // to `#minecraft:soul_fire_base_blocks`.
    if bare == "fire" {
        let below = world.get(pos.0, pos.1.wrapping_sub(1), pos.2);
        if !is_face_sturdy(reg, below, 1) {
            return air;
        }
    }
    if bare == "soul_fire" {
        let below = world.get(pos.0, pos.1.wrapping_sub(1), pos.2);
        if !reg.in_tag(below, "minecraft:soul_fire_base_blocks") {
            return air;
        }
    }
    // `RedStoneWireBlock.updateShape` (`dmb.a`): only DOWN carries the
    // survival check - a FULL sturdy UP face or a hopper. Repeaters and
    // comparators ask the RIGID support plane (`dia.b`); pressure plates ask
    // RIGID or CENTER (`dfm.a`).
    if bare == "redstone_wire"
        && direction == 0
        && !is_face_sturdy(reg, neighbor, 1)
        && reg.name_of(neighbor) != "minecraft:hopper"
    {
        return air;
    }
    if matches!(bare, "repeater" | "comparator")
        && direction == 0
        && !reg.is_face_sturdy(neighbor, 1, Support::Rigid)
    {
        return air;
    }
    if bare.ends_with("_pressure_plate")
        && direction == 0
        && !reg.is_face_sturdy(neighbor, 1, Support::Rigid)
        && !reg.is_face_sturdy(neighbor, 1, Support::Center)
    {
        return air;
    }
    // `SeagrassBlock.updateShape` (`dmx.a`): `direction == DOWN &&
    // !canSurvive -> AIR`; `canSurvive` = the block below's FULL sturdy UP
    // face, with magma explicitly excluded.
    if bare == "seagrass"
        && direction == 0
        && (!is_face_sturdy(reg, neighbor, 1) || reg.name_of(neighbor) == "minecraft:magma_block")
    {
        return air;
    }
    // `BaseCoralWallFanBlock.updateShape` (`czu.a`): the wall fan drops when the
    // block on `facing.getOpposite()` fails the FULL `isFaceSturdy` recheck.
    if bare.ends_with("_coral_wall_fan") {
        if let Some(attach) = facing_dir(reg, state).map(|f| f ^ 1) {
            if direction == attach && !is_face_sturdy(reg, neighbor, direction ^ 1) {
                return air;
            }
        }
    }
    // `BaseCoralPlantTypeBlock.updateShape`/`BaseCoralFanBlock.updateShape`
    // (`czs.a`/`czt.a`): `direction == DOWN && !canSurvive -> AIR`; the block
    // below must have a FULL sturdy UP face.
    if (bare.ends_with("_coral_fan") || bare.ends_with("_coral"))
        && direction == 0
        && !is_face_sturdy(reg, neighbor, 1)
    {
        return air;
    }
    // Cake and candle cake use the legacy solid predicate; candles use
    // `canSupportCenter`; a conduit also uses legacy solid. Sea pickles accept
    // a non-empty collision face as well as a FULL sturdy face.
    if (bare == "cake" || bare.ends_with("_candle_cake"))
        && direction == 0
        && !reg.is_legacy_solid(neighbor)
    {
        return air;
    }
    if (bare == "candle" || (bare.ends_with("_candle") && !bare.ends_with("_candle_cake")))
        && direction == 0
        && !reg.is_face_sturdy(neighbor, 1, Support::Center)
    {
        return air;
    }
    if bare == "sea_pickle"
        && direction == 0
        && !is_face_sturdy(reg, neighbor, 1)
        && !reg
            .collision_boxes(neighbor)
            .any(|(_, max)| max[1] >= 1.0)
    {
        return air;
    }
    // Flower pots, end rods, turtle eggs and conduits do not override
    // `canSurvive` in 1.21.1: the native matrix answers T even in the empty
    // context, so support loss must not remove them here.
    // `CocoaBlock.updateShape` (`dgh.a`-family): the pod only survives on
    // `#minecraft:jungle_logs` at `facing.getOpposite()`.
    if bare == "cocoa" {
        if let Some(attach) = facing_dir(reg, state).map(|f| f ^ 1) {
            if direction == attach && !reg.in_tag(neighbor, "minecraft:jungle_logs") {
                return air;
            }
        }
    }
    // `AmethystClusterBlock.updateShape` (the three bud sizes and the cluster):
    // `direction == facing.getOpposite() && !canSurvive -> AIR`; `canSurvive`
    // is the support's FULL sturdy face toward the cluster.
    if matches!(
        bare,
        "small_amethyst_bud" | "medium_amethyst_bud" | "large_amethyst_bud" | "amethyst_cluster"
    ) {
        if let Some(attach) = facing_dir(reg, state).map(|f| f ^ 1) {
            if direction == attach && !is_face_sturdy(reg, neighbor, direction ^ 1) {
                return air;
            }
        }
    }
    // `PointedDripstoneBlock.updateShape` (`dmf.a`): on the support side
    // (`tip_direction.getOpposite()`) `canSurvive` accepts a FULL sturdy face on
    // `tip_direction` or another pointed dripstone with the same tip direction.
    if bare == "pointed_dripstone" {
        let tip = match reg.get_prop(state, "tip_direction").as_deref() {
            Some("up") => 1u8,
            _ => 0u8,
        };
        if direction == (tip ^ 1) {
            let same_dripstone = reg.name_of(neighbor) == name
                && reg.get_prop(neighbor, "tip_direction")
                    == reg.get_prop(state, "tip_direction");
            if !reg.is_face_sturdy(neighbor, tip, Support::Full) && !same_dripstone {
                return air;
            }
        }
    }
    // `TripWireHookBlock.updateShape` (`doq.a`): the block on
    // `facing.getOpposite()` must offer a FULL sturdy face toward the hook.
    if bare == "tripwire_hook" {
        if let Some(attach) = facing_dir(reg, state).map(|f| f ^ 1) {
            if direction == attach && !is_face_sturdy(reg, neighbor, direction ^ 1) {
                return air;
            }
        }
    }
    // `TripWireBlock.updateShape` (`doa.a`): a horizontal change re-evaluates
    // `shouldConnectTo` - another tripwire, or a hook whose `facing` points back
    // at this side.
    if bare == "tripwire" && matches!(direction, 2..=5) {
        let key = match direction {
            2 => "north",
            3 => "south",
            4 => "west",
            _ => "east",
        };
        let neighbor_name = reg.name_of(neighbor);
        let connect = neighbor_name == name
            || (neighbor_name.ends_with("tripwire_hook")
                && facing_dir(reg, neighbor) == Some(direction ^ 1));
        let want = if connect { "true" } else { "false" };
        if reg.get_prop(state, key).as_deref() != Some(want) {
            return reg.with_prop(state, key, want);
        }
    }
    // `PistonHeadBlock.updateShape` (`dlg.a`): `direction ==
    // facing.getOpposite() && !canSurvive -> AIR`; the head survives while the
    // cell behind it is a piston body, moving piston, or another head.
    if bare == "piston_head" {
        if let Some(attach) = facing_dir(reg, state).map(|f| f ^ 1) {
            if direction == attach
                && !matches!(
                    reg.name_of(neighbor),
                    "minecraft:piston"
                        | "minecraft:sticky_piston"
                        | "minecraft:moving_piston"
                        | "minecraft:piston_head"
                )
            {
                return air;
            }
        }
    }
    // `BedBlock.updateShape` (`cyk.a`): on `getNeighbourDirection` (the foot
    // looks toward `facing` for its head, the head toward
    // `facing.getOpposite()`) a cell that is no longer the same bed breaks this
    // half.
    if bare.ends_with("_bed") {
        let part = reg.get_prop(state, "part");
        let pair = match (part.as_deref(), facing_dir(reg, state)) {
            (Some("foot"), Some(f)) => Some(f),
            (Some("head"), Some(f)) => Some(f ^ 1),
            _ => None,
        };
        if pair == Some(direction) && reg.name_of(neighbor) != name {
            return air;
        }
    }
    // `BellBlock.updateShape`: a change on an attached side re-asks
    // `canSurvive`; floor/ceiling bells ask CENTER, while wall bells ask the
    // FULL sturdy face toward the bell. A single-wall bell's `facing` points
    // toward its support in 1.21.1 (measured: facing=north survives a north
    // stone, not a south one).
    if bare == "bell" {
        let facing = facing_dir(reg, state).unwrap_or(2);
        let sides: [u8; 2] = match reg.get_prop(state, "attachment").as_deref() {
            Some("floor") => [0, 7],
            Some("ceiling") => [1, 7],
            Some("single_wall") => [facing, 7],
            Some("double_wall") => [facing ^ 1, facing],
            _ => [7, 7],
        };
        if sides.contains(&direction) {
            let survives = sides.iter().all(|&s| {
                if s > 5 {
                    return true;
                }
                let support = if s == direction {
                    neighbor
                } else {
                    let (dx, dz) = match s {
                        2 => (0, -1),
                        3 => (0, 1),
                        4 => (-1, 0),
                        _ => (1, 0),
                    };
                    world.get(pos.0 + dx, pos.1, pos.2 + dz)
                };
                let plane = if s <= 1 { Support::Center } else { Support::Full };
                reg.is_face_sturdy(support, s ^ 1, plane)
            });
            if !survives {
                return air;
            }
        }
    }
    // `ScaffoldingBlock.updateShape` (`dmo.a`) returns the state unchanged and
    // schedules the distance recompute tick; it does not rewrite `bottom` in
    // the shape callback itself.
    // `WallSignBlock`/`CeilingHangingSignBlock`/`StandingSignBlock`: the sign
    // drops when the side it hangs or stands on changes to a block that fails
    // `canSurvive` - legacy `isSolid` for the wooden sign family and CENTER
    // above for ceiling hanging signs.
    if bare.ends_with("_wall_sign") {
        if let Some(attach) = facing_dir(reg, state).map(|f| f ^ 1) {
            if direction == attach && !reg.is_legacy_solid(neighbor) {
                return air;
            }
        }
    } else if bare.ends_with("_hanging_sign") && !bare.ends_with("_wall_hanging_sign") {
        // CeilingHangingSignBlock.canSurvive always asks CENTER; `attached`
        // selects chains/planks visually and is not a support-plane switch.
        if direction == 1 && !reg.is_face_sturdy(neighbor, 0, Support::Center) {
            return air;
        }
    } else if bare.ends_with("_sign") && !bare.ends_with("_wall_hanging_sign") && direction == 0 && !reg.is_legacy_solid(neighbor) {
        return air;
    }
    // WallHangingSignBlock does not override `canSurvive` in 1.21.1, so its
    // `updateShape` guard is inert and the block survives even in air.
    // `BannerBlock.updateShape` (the standing form; the wall form is handled
    // above): the block below must pass legacy `isSolid`.
    if bare.ends_with("_banner")
        && !bare.ends_with("_wall_banner")
        && direction == 0
        && !reg.is_legacy_solid(neighbor)
    {
        return air;
    }
    // `CactusBlock.updateShape` (`czc.a`): `!canSurvive -> AIR` on every
    // direction - `#minecraft:sand` or another cactus below and no solid or
    // lava-bearing cell beside it.
    if bare == "cactus" {
        let below = world.get(pos.0, pos.1.wrapping_sub(1), pos.2);
        let base_ok =
            reg.name_of(below) == "minecraft:cactus" || reg.in_tag(below, "minecraft:sand");
        let blocked = [(-1i32, 0i32), (1, 0), (0, -1), (0, 1)]
            .iter()
            .any(|&(dx, dz)| {
                let s = world.get(pos.0 + dx, pos.1, pos.2 + dz);
                reg.is_legacy_solid(s) || reg.fluid_name(s).ends_with("lava")
            });
        let above_wet = reg.is_liquid_block(world.get(pos.0, pos.1.wrapping_add(1), pos.2));
        if !base_ok || blocked || above_wet {
            return air;
        }
    }
    // `SugarCaneBlock.updateShape` (`dob.a` family): `direction == DOWN &&
    // !canSurvive -> AIR`; `canSurvive` is cane on cane, or a `#dirt`/`#sand`
    // support with water (or `frosted_ice`) beside the support block.
    if bare == "sugar_cane" && direction == 0 {
        let survives = reg.name_of(neighbor) == "minecraft:sugar_cane" || {
            (reg.in_tag(neighbor, "minecraft:dirt") || reg.in_tag(neighbor, "minecraft:sand"))
                && [(-1i32, 0i32), (1, 0), (0, -1), (0, 1)]
                    .iter()
                    .any(|&(dx, dz)| {
                        let s = world.get(
                            pos.0 + dx,
                            pos.1.wrapping_sub(1),
                            pos.2 + dz,
                        );
                        reg.fluid_name(s) == "minecraft:water"
                            || reg.name_of(s) == "minecraft:frosted_ice"
                    })
        };
        if !survives {
            return air;
        }
    }
    // `BambooBlock.updateShape`/`BambooSaplingBlock.updateShape` (`cyc.a`/`cza.a`):
    // `!canSurvive -> AIR` on every direction; `canSurvive` is
    // `#minecraft:bamboo_plantable_on` below. The UP limb also copies the
    // neighbour bamboo's `leaves` property onto this stalk.
    if matches!(bare, "bamboo" | "bamboo_sapling") {
        if direction == 1 && matches!(reg.name_of(neighbor), "minecraft:bamboo" | "minecraft:bamboo_sapling") {
            if let Some(leaves) = reg.get_prop(neighbor, "leaves") {
                return reg.with_prop(state, "leaves", &leaves);
            }
        }
        let below = world.get(pos.0, pos.1.wrapping_sub(1), pos.2);
        if !reg.in_tag(below, "minecraft:bamboo_plantable_on") {
            return air;
        }
    }
    // `GrowingPlantHeadBlock`/`GrowingPlantBodyBlock.updateShape`: the
    // attach-direction cell (`growthDirection.getOpposite()`) must be
    // attachable - the head/body of the same plant or a FULL sturdy face
    // toward the plant - or the plant becomes AIR. A head whose
    // growth-direction neighbour stops being the same plant turns into the
    // body block, and a body whose growth-direction neighbour is neither
    // body nor head turns back into the head. Kelp's `canAttachTo` is the
    // only override: magma fails it. `direction` == attach direction means
    // `neighbor` is the support itself.
    let growing = match bare {
        "kelp" => Some((1u8, true, "kelp_plant", true)),
        "kelp_plant" => Some((1u8, false, "kelp", true)),
        "twisting_vines" => Some((1u8, true, "twisting_vines_plant", false)),
        "twisting_vines_plant" => Some((1u8, false, "twisting_vines", false)),
        "weeping_vines" => Some((0u8, true, "weeping_vines_plant", false)),
        "weeping_vines_plant" => Some((0u8, false, "weeping_vines", false)),
        "cave_vines" => Some((0u8, true, "cave_vines_plant", false)),
        "cave_vines_plant" => Some((0u8, false, "cave_vines", false)),
        _ => None,
    };
    if let Some((growth, head, partner, magma_fails)) = growing {
        let attach = growth ^ 1;
        let family = |st: u32| {
            matches!(reg.name_of(st), "minecraft:kelp" | "minecraft:kelp_plant") && growth == 1
                || matches!(
                    reg.name_of(st),
                    "minecraft:twisting_vines" | "minecraft:twisting_vines_plant"
                ) && bare.starts_with("twisting")
                || matches!(
                    reg.name_of(st),
                    "minecraft:weeping_vines" | "minecraft:weeping_vines_plant"
                ) && bare.starts_with("weeping")
                || matches!(
                    reg.name_of(st),
                    "minecraft:cave_vines" | "minecraft:cave_vines_plant"
                ) && bare.starts_with("cave")
        };
        if direction == growth && head {
            if !family(neighbor) && reg.name_of(neighbor) != name {
                let body = reg.state_of(&format!("minecraft:{partner}"), None);
                return body;
            }
        } else if direction == growth && !family(neighbor) {
            let head_st = reg.state_of(&format!("minecraft:{partner}"), None);
            return head_st;
        }
        if direction == attach {
            let can_attach = !(magma_fails && reg.name_of(neighbor) == "minecraft:magma_block");
            if !can_attach || !(family(neighbor) || is_face_sturdy(reg, neighbor, growth)) {
                return air;
            }
        }
    }
    // `VineBlock.updateShape` (`don.a`): a DOWN change defers to
    // `BlockBehaviour.updateShape` (state unchanged); any other direction
    // re-derives the claimed faces through `getUpdatedState` - a face survives
    // while its support is a FULL sturdy face or the vine above claims the
    // same face - and a vine with no face left becomes AIR.
    if bare == "vine" && direction != 0 {
        let mut next = state;
        let above = world.get(pos.0, pos.1.wrapping_add(1), pos.2);
        if reg.get_prop(next, "up").as_deref() == Some("true")
            && !is_face_sturdy(reg, above, 0)
        {
            next = reg.with_prop(next, "up", "false");
        }
        for d in [2u8, 3, 4, 5] {
            let key = match d {
                2 => "north",
                3 => "south",
                4 => "west",
                _ => "east",
            };
            if reg.get_prop(next, key).as_deref() == Some("true") {
                let side = if direction == d {
                    neighbor
                } else {
                    let (dx, dz) = match d {
                        2 => (0, -1),
                        3 => (0, 1),
                        4 => (-1, 0),
                        _ => (1, 0),
                    };
                    world.get(pos.0 + dx, pos.1, pos.2 + dz)
                };
                let hangs = reg.name_of(above) == name
                    && reg.get_prop(above, key).as_deref() == Some("true");
                if !is_face_sturdy(reg, side, d ^ 1) && !hangs {
                    next = reg.with_prop(next, key, "false");
                }
            }
        }
        let has_face = ["up", "north", "south", "west", "east"]
            .iter()
            .any(|k| reg.get_prop(next, k).as_deref() == Some("true"));
        return if has_face { next } else { air };
    }
    // `ChorusPlantBlock`/`ChorusFlowerBlock.updateShape` (`ddf.a`/`ddg.a`): a
    // failed `canSurvive` schedules the break tick rather than rewriting the
    // state; the tick's outcome is removal, so this pass emits AIR directly.
    // (The tick itself exists to let the whole plant collapse as one wave.)
    if bare == "chorus_plant" || bare == "chorus_flower" {
        let below = world.get(pos.0, pos.1.wrapping_sub(1), pos.2);
        let below_name = reg.name_of(below);
        let mut survives = matches!(
            below_name,
            "minecraft:chorus_plant" | "minecraft:end_stone" | "minecraft:chorus_flower"
        );
        if bare == "chorus_plant" && !survives {
            for (dx, dz) in [(-1i32, 0i32), (1, 0), (0, -1), (0, 1)] {
                let side = world.get(pos.0 + dx, pos.1, pos.2 + dz);
                if reg.name_of(side) == name {
                    let side_below = world.get(pos.0 + dx, pos.1.wrapping_sub(1), pos.2 + dz);
                    survives = matches!(
                        reg.name_of(side_below),
                        "minecraft:chorus_plant" | "minecraft:end_stone"
                    ) || matches!(below_name, "minecraft:chorus_plant" | "minecraft:end_stone");
                    if survives {
                        break;
                    }
                }
            }
        }
        if !survives {
            return air;
        }
    }
    state
}

fn update_connection_shapes<W: TemplateWorld>(
    reg: &BReg,
    state: u32,
    direction: u8,
    neighbor: u32,
    world: &mut W,
    pos: Pos,
) -> Option<u32> {
    let name = reg.name_of(state);
    if name.ends_with("_stairs") && matches!(direction, 2..=5) {
        if let Some(next) = reg.stair_after_neighbors(state, pos, |cell| {
            world.get(cell.0, cell.1, cell.2)
        }) {
            return Some(next);
        }
    }
    let bare = name.strip_prefix("minecraft:").unwrap_or(name);
    // `WallBlock.updateShape`: both `UP` (direction 1) and horizontal sides
    // recompute `low`/`tall` against `above.getCollisionShape().getFaceShape(DOWN)`
    // and recompute `up` (`shouldRaisePost`).
    if bare.ends_with("_wall") || reg.in_tag(state, "minecraft:walls") {
        return Some(wall_update_shape(reg, state, direction, neighbor, world, pos));
    }
    if bare.ends_with("_fence") && matches!(direction, 2..=5) {
        let key = match direction {
            2 => "north",
            3 => "south",
            4 => "west",
            _ => "east",
        };
        let connected = fence_connects(reg, state, neighbor, direction);
        let want = if connected { "true" } else { "false" };
        if reg.get_prop(state, key).as_deref() != Some(want) {
            return Some(reg.with_prop(state, key, want));
        }
    }
    // `IronBarsBlock.updateShape` (glass panes and iron bars).
    if (bare.ends_with("_pane") || bare == "iron_bars") && matches!(direction, 2..=5) {
        let key = match direction {
            2 => "north",
            3 => "south",
            4 => "west",
            _ => "east",
        };
        let sturdy = is_face_sturdy(reg, neighbor, direction ^ 1);
        let same = reg.name_of(neighbor).ends_with("_pane") || reg.name_of(neighbor) == "minecraft:iron_bars";
        let connected = (!is_exception_for_connection(reg, neighbor) && sturdy)
            || same
            || reg.in_tag(neighbor, "minecraft:walls");
        let want = if connected { "true" } else { "false" };
        if reg.get_prop(state, key).as_deref() != Some(want) {
            return Some(reg.with_prop(state, key, want));
        }
    }
    None
}

fn facing_dir(reg: &BReg, state: u32) -> Option<u8> {
    match reg.get_prop(state, "facing").as_deref() {
        Some("down") => Some(0),
        Some("up") => Some(1),
        Some("north") => Some(2),
        Some("south") => Some(3),
        Some("west") => Some(4),
        Some("east") => Some(5),
        _ => None,
    }
}

/// `FaceAttachedHorizontalDirectionalBlock.getConnectedDirection().getOpposite()`:
/// the direction from the block toward its support - DOWN for `face=floor`,
/// UP for `face=ceiling`, `facing.getOpposite()` for `face=wall`.
fn attach_face_direction(reg: &BReg, state: u32) -> Option<u8> {
    match reg.get_prop(state, "face").as_deref() {
        Some("floor") => Some(0),
        Some("ceiling") => Some(1),
        Some("wall") => facing_dir(reg, state).map(|f| f ^ 1),
        _ => None,
    }
}

/// `MultifaceBlock.hasAnyFace`: at least one of the six face properties is set.
fn multiface_has_face(reg: &BReg, state: u32) -> bool {
    ["down", "up", "north", "south", "west", "east"]
        .iter()
        .any(|key| reg.get_prop(state, key).as_deref() == Some("true"))
}

/// `Block.isExceptionForConnection`.
fn is_exception_for_connection(reg: &BReg, state: u32) -> bool {
    let name = reg.name_of(state);
    let bare = name.strip_prefix("minecraft:").unwrap_or(name);
    name.ends_with("_leaves")
        || matches!(
            bare,
            "barrier" | "carved_pumpkin" | "jack_o_lantern" | "melon" | "pumpkin"
        )
        || reg.in_tag(state, "minecraft:shulker_boxes")
}

/// `BlockStateBase.isFaceSturdy` for the plain FULL support type: the neighbour must cover
/// that whole side. Reads the pinned 1.21.1 fact table (`Support::Full`); the `side_flags`
/// this used to read agreed with it on all 26,684 states, and now has one owner.
fn is_face_sturdy(reg: &BReg, state: u32, direction: u8) -> bool {
    reg.is_face_sturdy(state, direction, Support::Full)
}

/// `FenceBlock.connectsTo`: a sturdy neighbour, the same fence family, or a
/// fence gate aligned across the connection direction.
fn fence_connects(reg: &BReg, state: u32, neighbor: u32, direction: u8) -> bool {
    let sturdy = is_face_sturdy(reg, neighbor, direction ^ 1);
    let same_fence = reg.in_tag(neighbor, "minecraft:fences")
        && reg.in_tag(state, "minecraft:wooden_fences")
            == reg.in_tag(neighbor, "minecraft:wooden_fences");
    let neighbor_name = reg.name_of(neighbor);
    let neighbor_bare = neighbor_name
        .strip_prefix("minecraft:")
        .unwrap_or(neighbor_name);
    let fence_gate = neighbor_bare.ends_with("_fence_gate")
        && match reg.get_prop(neighbor, "facing").as_deref() {
            Some("north" | "south") => matches!(direction, 4 | 5),
            Some("west" | "east") => matches!(direction, 2 | 3),
            _ => false,
        };
    (!is_exception_for_connection(reg, neighbor) && sturdy) || same_fence || fence_gate
}

/// `WallBlock.connectsTo`.
fn wall_connects(reg: &BReg, neighbor: u32, direction_toward_neighbor: u8) -> bool {
    let opposite = direction_toward_neighbor ^ 1;
    let sturdy = is_face_sturdy(reg, neighbor, opposite);
    let name = reg.name_of(neighbor);
    let bare = name.strip_prefix("minecraft:").unwrap_or(name);
    let fence_gate = bare.ends_with("_fence_gate")
        && match reg.get_prop(neighbor, "facing").as_deref() {
            Some("north" | "south") => matches!(opposite, 4 | 5),
            Some("west" | "east") => matches!(opposite, 2 | 3),
            _ => false,
        };
    reg.in_tag(neighbor, "minecraft:walls")
        || (!is_exception_for_connection(reg, neighbor) && sturdy)
        || bare.ends_with("_pane")
        || bare == "iron_bars"
        || fence_gate
}

/// XZ footprint of `above.getCollisionShape().getFaceShape(Direction.DOWN)`:
/// any collision box whose `minY <= 1e-7 && maxY > 1e-7`.
fn above_down_face_rects(reg: &BReg, above: u32) -> Vec<[f64; 4]> {
    reg.collision_boxes(above)
        .filter(|(min, max)| min[1] <= 1.0e-7 && max[1] > 1.0e-7)
        .map(|(min, max)| [min[0], min[2], max[0], max[2]])
        .collect()
}

/// `!Shapes.joinIsNotEmpty(testBox, aboveDownFace, BooleanOp.ONLY_FIRST)`:
/// every sub-cell of `test` (`[tx0, tz0, tx1, tz1]`) is inside at least one rect.
fn rects_cover(rects: &[[f64; 4]], test: [f64; 4]) -> bool {
    if rects.is_empty() {
        return false;
    }
    let [tx0, tz0, tx1, tz1] = test;
    let mut xs = Vec::with_capacity(rects.len() * 2 + 2);
    let mut zs = Vec::with_capacity(rects.len() * 2 + 2);
    xs.push(tx0);
    xs.push(tx1);
    zs.push(tz0);
    zs.push(tz1);
    for &[bx0, bz0, bx1, bz1] in rects {
        if bx1 > tx0 + 1.0e-9 && bx0 < tx1 - 1.0e-9 && bz1 > tz0 + 1.0e-9 && bz0 < tz1 - 1.0e-9 {
            xs.push(bx0.clamp(tx0, tx1));
            xs.push(bx1.clamp(tx0, tx1));
            zs.push(bz0.clamp(tz0, tz1));
            zs.push(bz1.clamp(tz0, tz1));
        }
    }
    xs.sort_by(|a, b| a.total_cmp(b));
    zs.sort_by(|a, b| a.total_cmp(b));
    for xw in xs.windows(2) {
        if xw[1] - xw[0] <= 1.0e-9 {
            continue;
        }
        let mx = 0.5 * (xw[0] + xw[1]);
        for zw in zs.windows(2) {
            if zw[1] - zw[0] <= 1.0e-9 {
                continue;
            }
            let mz = 0.5 * (zw[0] + zw[1]);
            if !rects
                .iter()
                .any(|&[bx0, bz0, bx1, bz1]| bx0 <= mx && mx <= bx1 && bz0 <= mz && mz <= bz1)
            {
                return false;
            }
        }
    }
    true
}

/// Full vanilla `WallBlock.updateShape`.
fn wall_update_shape<W: TemplateWorld>(
    reg: &BReg,
    state: u32,
    direction: u8,
    neighbor: u32,
    world: &mut W,
    pos: Pos,
) -> u32 {
    if direction == 0 {
        return state;
    }
    let connected = |key: &str| reg.get_prop(state, key).as_deref() != Some("none");
    let (north, south, west, east, above) = if direction == 1 {
        (
            connected("north"),
            connected("south"),
            connected("west"),
            connected("east"),
            neighbor,
        )
    } else {
        let north = if direction == 2 {
            wall_connects(reg, neighbor, 2)
        } else {
            connected("north")
        };
        let south = if direction == 3 {
            wall_connects(reg, neighbor, 3)
        } else {
            connected("south")
        };
        let west = if direction == 4 {
            wall_connects(reg, neighbor, 4)
        } else {
            connected("west")
        };
        let east = if direction == 5 {
            wall_connects(reg, neighbor, 5)
        } else {
            connected("east")
        };
        let above = world.get(pos.0, pos.1.wrapping_add(1), pos.2);
        (north, south, west, east, above)
    };
    let rects = above_down_face_rects(reg, above);
    let side_val = |want: bool, t: [f64; 4]| -> &'static str {
        if !want {
            "none"
        } else if rects_cover(&rects, t) {
            "tall"
        } else {
            "low"
        }
    };
    let n_val = side_val(north, [7.0 / 16.0, 0.0, 9.0 / 16.0, 9.0 / 16.0]);
    let s_val = side_val(south, [7.0 / 16.0, 7.0 / 16.0, 9.0 / 16.0, 1.0]);
    let w_val = side_val(west, [0.0, 7.0 / 16.0, 9.0 / 16.0, 9.0 / 16.0]);
    let e_val = side_val(east, [7.0 / 16.0, 7.0 / 16.0, 1.0, 9.0 / 16.0]);

    let above_wall_up = (reg.name_of(above).ends_with("_wall")
        || reg.in_tag(above, "minecraft:walls"))
        && reg.get_prop(above, "up").as_deref() == Some("true");
    let up_val = if above_wall_up {
        true
    } else {
        let n_none = n_val == "none";
        let s_none = s_val == "none";
        let w_none = w_val == "none";
        let e_none = e_val == "none";
        let raise = (n_none && s_none && w_none && e_none)
            || (n_none != s_none)
            || (w_none != e_none);
        if raise {
            true
        } else if (n_val == "tall" && s_val == "tall") || (w_val == "tall" && e_val == "tall") {
            false
        } else {
            reg.in_tag(above, "minecraft:wall_post_override")
                || rects_cover(&rects, [7.0 / 16.0, 7.0 / 16.0, 9.0 / 16.0, 9.0 / 16.0])
        }
    };
    let mut next = reg.with_prop(state, "north", n_val);
    next = reg.with_prop(next, "south", s_val);
    next = reg.with_prop(next, "west", w_val);
    next = reg.with_prop(next, "east", e_val);
    reg.with_prop(next, "up", if up_val { "true" } else { "false" })
}

// ---------------------------------------------------------------- fluid tick seam

/// A cell as `FlowingFluid` sees it. `flowing_fluid` is Layer 3 and this module is Layer 4,
/// so the vocabulary is restated here and the machine itself is registered into
/// [`postprocess_shape_marks`] from `main` (the same shape as the beardifier resolver).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FluidCell {
    Air,
    Solid,
    Fluid { water: bool, level: u8 },
    /// Non-air, no fluid, empty collision shape: `FlowingFluid.canHoldFluid` answers true and
    /// `canPassThrough` cannot fail, so a spread replaces the cell (`short_grass` in the
    /// measured cell `(100,19,-513)`, a lush-cave plant). It is never written by the machine.
    Passable,
}

/// The two verbs one `FluidState.tick` needs from the world. `None` from [`fluid_cell`] is
/// "this cell is not one the machine models" (a non-full collision shape, a cross-fluid
/// contact) — vanilla has no such answer, and the machine refuses instead of guessing
/// (see `flowing_fluid::Reader`).
pub(crate) trait FluidTickTarget {
    fn fluid_cell(&mut self, pos: Pos) -> Option<FluidCell>;
    fn set_fluid(&mut self, pos: Pos, cell: FluidCell);
}

/// One fluent tick at `pos`, in place. Registered by `main` for `flowing_fluid::tick`.
pub(crate) type FluidTicker = fn(&mut dyn FluidTickTarget, Pos);

static GLOBAL_FLUID_TICKER: std::sync::Mutex<Option<FluidTicker>> = std::sync::Mutex::new(None);

/// Register the process-wide fluid ticker (`main.rs`, and the structure tests that run the
/// pass without one).
pub(crate) fn register_fluid_ticker(ticker: FluidTicker) {
    *GLOBAL_FLUID_TICKER.lock().unwrap_or_else(|e| e.into_inner()) = Some(ticker);
}

fn global_fluid_ticker() -> Option<FluidTicker> {
    *GLOBAL_FLUID_TICKER.lock().unwrap_or_else(|e| e.into_inner())
}

/// `FluidState.getType().tick(level, pos, state)` reads and writes through the same world
/// the shape rules do, so every `TemplateWorld` is its own tick target.
impl<W: TemplateWorld> FluidTickTarget for W {
    fn fluid_cell(&mut self, pos: Pos) -> Option<FluidCell> {
        let reg = breg()?;
        let state = TemplateWorld::get(self, pos.0, pos.1, pos.2);
        match reg.fluid_name(state) {
            "minecraft:air" | "minecraft:empty" => {
                if reg.is_air(state) {
                    Some(FluidCell::Air)
                } else if reg.is_solid_render(state) {
                    Some(FluidCell::Solid)
                } else if holds_fluid(reg, state) {
                    Some(FluidCell::Passable)
                } else {
                    None
                }
            }
            "minecraft:water" | "minecraft:flowing_water" => Some(FluidCell::Fluid {
                water: true,
                level: fluid_level(reg, state),
            }),
            "minecraft:lava" | "minecraft:flowing_lava" => Some(FluidCell::Fluid {
                water: false,
                level: fluid_level(reg, state),
            }),
            _ => None,
        }
    }

    fn set_fluid(&mut self, pos: Pos, cell: FluidCell) {
        let Some(reg) = breg() else { return };
        let state = match cell {
            FluidCell::Air => reg.state_of("minecraft:air", None),
            FluidCell::Solid => return,
            // A `Passable` cell is only ever an *input*: the machine replaces it with the fluid
            // it spreads, so a write of one would be a plan bug, not a state.
            FluidCell::Passable => return,
            FluidCell::Fluid { water, level } => {
                let name = if water {
                    "minecraft:water"
                } else {
                    "minecraft:lava"
                };
                let base = reg.state_of(name, None);
                // `FluidState.createLegacyBlock`: a falling fluid (`level >= 8`) is written as
                // a *falling* fluid, which is the `falling` property on the block state.
                let base = if reg.get_prop(base, "falling").is_some() {
                    reg.with_prop(base, "falling", if level >= 8 { "true" } else { "false" })
                } else {
                    base
                };
                reg.with_prop(base, "level", &(level & 15).to_string())
            }
        };
        TemplateWorld::set(self, pos.0, pos.1, pos.2, state);
    }
}

/// The `level` property of a fluid state, 0 when the state does not carry one (a waterlogged
/// non-fluid keeps a source fluid).
pub(crate) fn fluid_level(reg: &BReg, state: u32) -> u8 {
    reg.get_prop(state, "level")
        .and_then(|v| v.parse::<u8>().ok())
        .unwrap_or(0)
}

/// `FlowingFluid.canHoldFluid` / `canPassThrough` for a cell that holds no fluid of its own,
/// restricted to the case this machine models: an **empty collision shape**.
///
/// From the pinned jar (`epc`): `canHoldFluid` delegates to `SimpleWaterloggedBlock` first
/// (a waterlogged-capable state is entered through `placeLiquid`, which this machine does not
/// model — so those states stay unsupported), refuses `DoorBlock`, `#minecraft:signs`,
/// `ladder`, `sugar_cane`, `bubble_column`, `nether_portal`, `end_portal`, `end_gateway` and
/// `structure_void` outright, and otherwise answers `!state.blocksMotion()`. `canPassThrough`
/// intersects the two collision shapes; against an empty shape it cannot fail. A cell with a
/// non-empty partial shape keeps its refusal: its answer depends on the two shapes' geometry.
fn holds_fluid(reg: &BReg, state: u32) -> bool {
    // `SimpleWaterloggedBlock.canPlaceLiquid` is the first branch of `canHoldFluid` and enters a
    // state through `placeLiquid`; every `DoorBlock` and sign state carries `waterlogged`, so this
    // guard answers both families as well as the arm this machine cannot write.
    if reg.get_prop(state, "waterlogged").is_some() {
        return false;
    }
    let refused = matches!(
        reg.name_of(state),
        "minecraft:ladder"
            | "minecraft:sugar_cane"
            | "minecraft:bubble_column"
            | "minecraft:nether_portal"
            | "minecraft:end_portal"
            | "minecraft:end_gateway"
            | "minecraft:structure_void"
    ) || reg.in_tag(state, "minecraft:signs");
    !refused && reg.is_collision_empty(state) && !reg.blocks_motion(state)
}

/// `LevelChunk.postProcessGeneration`'s per-cell rule, for the positions a `WorldGenRegion`
/// write marked with `markPosForPostprocessing`.
///
/// Native runs this once per chunk, when the `ProtoChunk` is promoted to a
/// `LevelChunk` (`ChunkMap.protoChunkToFullChunk`, after `INITIALIZE_LIGHT`/`LIGHT`): for every
/// marked position, in section order then mark order, it skips a `LiquidBlock` cell and
/// otherwise re-derives the state with `Block.updateFromNeighbourShapes`, writing the result
/// with `UPDATE_INVISIBLE | UPDATE_KNOWN_SHAPE` (`20`). The caller owns which marks belong to
/// the chunk being promoted and supplies them in that order.
///
/// The two mark sources of 1.21.1 are both here: `StructurePiece.placeBlock` for
/// `SHAPE_CHECK_BLOCKS` (fence/pipe/wall-torch connections, multiface faces, crops) and
/// `WorldGenRegion.setBlock`'s `hasPostProcess` gate for the three blocks whose shape depends
/// on the **lit** engine (`brown_mushroom`, `red_mushroom`, `magma_block`). The second is why
/// this pass reads light through [`TemplateWorld::raw_brightness`]; `magma_block` itself is the
/// one that does not move - `MagmaBlock.updateShape` only calls
/// `level.scheduleTick(pos, WATER, 20)` when water sits above it and returns the state
/// unchanged, and a scheduled tick is not a state change.
///
/// Returns the number of cells the pass rewrote.
pub(crate) fn postprocess_shape_marks<W: TemplateWorld>(
    world: &mut W,
    reg: &BReg,
    marks: &[Pos],
) -> usize {
    let mut changed = 0;
    // `POSTPROC_TRACE=<cx>,<cz>` (or `all`): every cell this pass rewrites, with the raw
    // brightness the shape rules read from it. That number has no other observable: the light
    // sample lives only for the duration of one promoted chunk (`FC::begin_promotion_pass`), and
    // a cell that is *not* rewritten by a light-driven rule looks exactly like a cell no rule
    // looked at.
    let trace = crate::trace_env::env_str!("POSTPROC_TRACE");
    // `POSTPROC_STATES=1`: every mark's state at pass time, not only the ones a rule rewrites.
    // This is the instrument that says whether a shape family is *reachable* from the mark set
    // before the rule is written for it: a family with no cell in this list is a rule no
    // measurement would exercise.
    let states = crate::trace_env::env_flag!("POSTPROC_STATES");
    let ticker = global_fluid_ticker();
    for &pos in marks {
        let state = world.get(pos.0, pos.1, pos.2);
        if states {
            eprintln!(
                "POSTPROCSTATE {} {} {} {} fluid={}",
                pos.0,
                pos.1,
                pos.2,
                reg.name_of(state),
                reg.fluid_name(state) != "minecraft:empty"
            );
        }
        // `FluidState fs = st.getFluidState(); if (!fs.isEmpty()) fs.tick(this.level, p);`
        //
        // This is the reason the pass runs at all on a marked water cell: a carver or the
        // fill left a source inside a settled aquifer body, and the tick is what gives its
        // neighbours a flowing level. `st` above is read *before* the tick and both branches
        // below use that stale state, exactly as the vanilla body does - a tick that rewrites
        // the cell is still followed by `updateFromNeighbourShapes` on the state that was read.
        if reg.fluid_name(state) != "minecraft:empty" {
            if let Some(tick) = ticker {
                tick(world, pos);
            }
        }
        // `!(state.getBlock() instanceof LiquidBlock)`: fluid blocks carry their own
        // level, and `updateShape` has nothing to say about them.
        if matches!(reg.name_of(state), "minecraft:water" | "minecraft:lava") {
            continue;
        }
        let updated = postprocess_bush_shape(reg, state, world, pos)
            .unwrap_or_else(|| update_from_neighbour_shapes(reg, state, world, pos));
        if updated != state {
            if trace
                .is_some_and(|want| want == "all" || want == format!("{},{}", pos.0 >> 4, pos.2 >> 4))
            {
                eprintln!(
                    "POSTPROC {} {} {} from={} to={} light={:?}",
                    pos.0,
                    pos.1,
                    pos.2,
                    reg.name_of(state),
                    reg.name_of(updated),
                    world.raw_brightness(pos)
                );
            }
            world.set(pos.0, pos.1, pos.2, updated);
            changed += 1;
        }
    }
    changed
}

/// `BushBlock.updateShape` (`dgh.a`) and `DoublePlantBlock.updateShape` (`dig.a`) for
/// `LevelChunk.postProcessGeneration` (`dvi.H`) only — NOT shared `update_shape`, because
/// `apply_shape_passes` runs at structure placement time while a later piece/layer may write the
/// substrate.
///
/// From the pinned 1.21.1 bytecode:
/// * `BushBlock` (`dgh`) checks `!state.canSurvive(level, pos) -> AIR`, with
///   `mayPlaceOn = state.is(BlockTags.DIRT) || state.is(Blocks.FARMLAND)`. Exactly the
///   subclasses inheriting `updateShape`, `canSurvive`, and `mayPlaceOn` unchanged are
///   `TallGrassBlock` (`short_grass`, `fern`), `FlowerBlock` (the 13 non-wither small flowers),
///   `SaplingBlock` (the 7 non-mangrove/azalea saplings), `PinkPetalsBlock`, and
///   `SweetBerryBushBlock`.
/// * `DoublePlantBlock` (`dig`) / `TallFlowerBlock` (`dod`) pair `lower`/`upper` halves on the
///   vertical axis and delegate the `lower` half's substrate check to `BushBlock.canSurvive`.
fn postprocess_bush_shape<W: TemplateWorld>(
    reg: &BReg,
    state: u32,
    world: &mut W,
    pos: Pos,
) -> Option<u32> {
    let name = reg.name_of(state);
    let bare = name.strip_prefix("minecraft:").unwrap_or(name);
    let air = reg.state_of("minecraft:air", None);
    let is_plain_bush = matches!(
        bare,
        "short_grass"
            | "fern"
            | "pink_petals"
            | "sweet_berry_bush"
            | "oak_sapling"
            | "spruce_sapling"
            | "birch_sapling"
            | "jungle_sapling"
            | "acacia_sapling"
            | "cherry_sapling"
            | "dark_oak_sapling"
            | "dandelion"
            | "torchflower"
            | "poppy"
            | "blue_orchid"
            | "allium"
            | "azure_bluet"
            | "red_tulip"
            | "orange_tulip"
            | "white_tulip"
            | "pink_tulip"
            | "oxeye_daisy"
            | "cornflower"
            | "lily_of_the_valley"
    );
    if is_plain_bush {
        let below = world.get(pos.0, pos.1 - 1, pos.2);
        let ok = reg.in_tag(below, "minecraft:dirt") || reg.name_of(below) == "minecraft:farmland";
        return Some(if ok { state } else { air });
    }
    // `BushBlock` subclasses that override `mayPlaceOn`/`canSurvive` but keep
    // the `updateShape -> !canSurvive -> AIR` contract. Each arm names the
    // subclass's own substrate rule; `mushroom`s are the one family whose
    // survival also reads the promotion light engine (the arm in
    // `update_shape`).
    let below = world.get(pos.0, pos.1.wrapping_sub(1), pos.2);
    let below_name = reg.name_of(below);
    let survives = match bare {
        // `NetherWartBlock.canSurvive` (`dlc.a`): SOUL_SAND only.
        "nether_wart" => Some(below_name == "minecraft:soul_sand"),
        // `StemBlock`/`AttachedStemBlock.canSurvive` (`dns.a`/`cxs.a`): the
        // `mayPlaceOn` set is FARMLAND only.
        "pumpkin_stem" | "melon_stem" | "attached_pumpkin_stem" | "attached_melon_stem" => {
            Some(below_name == "minecraft:farmland")
        }
        // `DeadBushBlock.mayPlaceOn` (`dhr.a`): sand/red_sand, the `#terracotta`
        // family, or plain dirt/podzol/mycelium - not grass or farmland.
        "dead_bush" => Some(
            matches!(
                below_name,
                "minecraft:sand"
                    | "minecraft:red_sand"
                    | "minecraft:dirt"
                    | "minecraft:coarse_dirt"
                    | "minecraft:podzol"
                    | "minecraft:mycelium"
            ) || reg.in_tag(below, "minecraft:terracotta"),
        ),
        // `WitherRoseBlock.mayPlaceOn` (`dom.a`): the bush default plus
        // netherrack and the soul family.
        "wither_rose" => Some(
            reg.in_tag(below, "minecraft:dirt")
                || matches!(
                    below_name,
                    "minecraft:farmland"
                        | "minecraft:netherrack"
                        | "minecraft:soul_sand"
                        | "minecraft:soul_soil"
                ),
        ),
        // `RootsBlock`/`NetherSproutsBlock.mayPlaceOn` (`dmi.a`/`dkz.a`): the
        // bush default plus `#nylium` and soul soil.
        "crimson_roots" | "warped_roots" | "nether_sprouts" => Some(
            reg.in_tag(below, "minecraft:nylium")
                || reg.in_tag(below, "minecraft:dirt")
                || matches!(below_name, "minecraft:soul_soil" | "minecraft:farmland"),
        ),
        // `FungusBlock.mayPlaceOn` (`diz.a`): the roots set plus mycelium.
        "crimson_fungus" | "warped_fungus" => Some(
            reg.in_tag(below, "minecraft:nylium")
                || reg.in_tag(below, "minecraft:dirt")
                || matches!(
                    below_name,
                    "minecraft:mycelium" | "minecraft:soul_soil" | "minecraft:farmland"
                ),
        ),
        // `AzaleaBlock.mayPlaceOn` (`cyg.a`): the bush default plus clay.
        "azalea" | "flowering_azalea" => Some(
            reg.in_tag(below, "minecraft:dirt")
                || matches!(below_name, "minecraft:farmland" | "minecraft:clay"),
        ),
        // `BigDripleafBlock.canSurvive` (`czk.a`): clay/moss or the bush
        // default; the leaf also sits on its stem. `BigDripleafStemBlock`
        // (`czl.a`) drops the stem limb.
        "big_dripleaf" => Some(
            reg.in_tag(below, "minecraft:dirt")
                || matches!(
                    below_name,
                    "minecraft:clay"
                        | "minecraft:moss_block"
                        | "minecraft:farmland"
                        | "minecraft:big_dripleaf_stem"
                ),
        ),
        "big_dripleaf_stem" => Some(
            reg.in_tag(below, "minecraft:dirt")
                || matches!(
                    below_name,
                    "minecraft:clay" | "minecraft:moss_block" | "minecraft:farmland"
                ),
        ),
        // `HangingRootsBlock.canSurvive` (`djx.a`): the support is the block
        // ABOVE - a FULL sturdy DOWN face.
        "hanging_roots" => {
            let above = world.get(pos.0, pos.1.wrapping_add(1), pos.2);
            Some(reg.is_face_sturdy(above, 0, Support::Full))
        }
        // `MangrovePropaguleBlock.canSurvive` (`dlh.a`): a hanging propagule
        // holds onto a sturdy DOWN face or mangrove leaves above; a standing
        // one takes the bush default plus clay.
        "mangrove_propagule" => {
            if reg.get_prop(state, "hanging").as_deref() == Some("true") {
                let above = world.get(pos.0, pos.1.wrapping_add(1), pos.2);
                Some(
                    reg.is_face_sturdy(above, 0, Support::Full)
                        || reg.name_of(above) == "minecraft:mangrove_leaves",
                )
            } else {
                Some(
                    reg.in_tag(below, "minecraft:dirt")
                        || matches!(below_name, "minecraft:farmland" | "minecraft:clay"),
                )
            }
        }
        // `SmallDripleafBlock` (`dnq.a`): a `DoublePlantBlock` - the lower half
        // wants `#minecraft:small_dripleaf_placeable` (clay/moss) under any
        // cell, or `#dirt`/farmland under a waterlogged one; the upper half
        // wants its lower half.
        "small_dripleaf" => {
            let half = reg.get_prop(state, "half");
            match half.as_deref() {
                Some("upper") => Some(
                    below_name == "minecraft:small_dripleaf"
                        && reg.get_prop(below, "half").as_deref() == Some("lower"),
                ),
                Some("lower") => {
                    let wet =
                        reg.get_prop(state, "waterlogged").as_deref() == Some("true");
                    Some(
                        reg.in_tag(below, "minecraft:small_dripleaf_placeable")
                            || (wet
                                && (reg.in_tag(below, "minecraft:dirt")
                                    || below_name == "minecraft:farmland")),
                    )
                }
                _ => Some(false),
            }
        }
        _ => None,
    };
    if let Some(ok) = survives {
        return Some(if ok { state } else { air });
    }
    if matches!(
        bare,
        "tall_grass" | "large_fern" | "sunflower" | "lilac" | "rose_bush" | "peony"
    ) {
        let half = reg.get_prop(state, "half");
        let ok = match half.as_deref() {
            Some("upper") => {
                let below = world.get(pos.0, pos.1 - 1, pos.2);
                reg.name_of(below) == name
                    && reg.get_prop(below, "half").as_deref() == Some("lower")
            }
            Some("lower") => {
                let below = world.get(pos.0, pos.1 - 1, pos.2);
                let above = world.get(pos.0, pos.1 + 1, pos.2);
                (reg.in_tag(below, "minecraft:dirt") || reg.name_of(below) == "minecraft:farmland")
                    && reg.name_of(above) == name
                    && reg.get_prop(above, "half").as_deref() == Some("upper")
            }
            _ => false,
        };
        return Some(if ok { state } else { air });
    }
    None
}

/// `Block.updateFromNeighbourShapes` — `UPDATE_SHAPE_ORDER` is
/// `[WEST, EAST, NORTH, SOUTH, DOWN, UP]`.
fn update_from_neighbour_shapes<W: TemplateWorld>(
    reg: &BReg,
    state: u32,
    world: &mut W,
    pos: Pos,
) -> u32 {
    let air = reg.state_of("minecraft:air", None);
    let mut current = state;
    for direction in [4u8, 5, 2, 3, 0, 1] {
        let (dx, dy, dz) = direction_delta(direction);
        let neighbor = world.get(pos.0 + dx, pos.1 + dy, pos.2 + dz);
        current = update_shape(reg, current, direction, neighbor, world, pos);
        if current == air {
            break;
        }
    }
    current
}

fn direction_delta(direction: u8) -> (i32, i32, i32) {
    match direction {
        0 => (0, -1, 0),
        1 => (0, 1, 0),
        2 => (0, 0, -1),
        3 => (0, 0, 1),
        4 => (-1, 0, 0),
        _ => (1, 0, 0),
    }
}

// ---------------------------------------------------------------- templates

/// Datapack template cache. Native keeps an `Optional<StructureTemplate>`
/// per resource id; loading on demand keeps the piece list small.
///
/// `StructureTemplateManager.getOrCreate` is a plain map on a plain object, so
/// native can reach it re-entrantly: a structure's `findGenerationPoint` may
/// query a heightmap, which resolves the beardifier, which generates the starts
/// of the neighbouring chunks, which reads this cache again. A `&mut self` cache
/// cannot be handed to that inner call while the outer one holds it (measured:
/// `--preset nether` panics in `with_templates` with "RefCell already borrowed"
/// on the first chunk), so the mutable parts live behind their own cells and the
/// accessor takes `&self` like the native manager.
type TemplateLoader = Box<dyn FnMut(&str) -> Option<Vec<u8>>>;

pub(crate) struct Templates {
    cache: std::cell::RefCell<std::collections::HashMap<String, Rc<Template>>>,
    /// `StructureTemplateManager.getOrCreate` behind a seam: this layer may not
    /// reach for the worldgen pack reader (the module graph forbids that edge),
    /// so the caller that owns the datapack supplies the bytes.
    loader: std::cell::RefCell<TemplateLoader>,
}

impl Templates {
    pub(crate) fn new(loader: impl FnMut(&str) -> Option<Vec<u8>> + 'static) -> Self {
        Templates {
            cache: std::cell::RefCell::new(std::collections::HashMap::new()),
            loader: std::cell::RefCell::new(Box::new(loader)),
        }
    }

    pub(crate) fn get(&self, id: &str, reg: &BReg) -> Result<Rc<Template>, &'static str> {
        if let Some(template) = self.cache.borrow().get(id) {
            return Ok(template.clone());
        }
        // Neither borrow is held across the load, so a nested `get` from inside
        // the loader cannot collide with this one.
        let bytes = (self.loader.borrow_mut())(id).ok_or("missing structure template")?;
        let (_, root) = crate::nbt::read_gzip(&bytes, 16 * 1024 * 1024)
            .map_err(|_| "structure template NBT")?;
        let template = Rc::new(Template::load(&root, reg)?);
        self.cache
            .borrow_mut()
            .insert(id.to_string(), template.clone());
        Ok(template)
    }
}

/// `StructureTemplate.filterBlocks(pos, settings, block)`: it re-picks the
/// palette (consuming the settings draw) and returns the *transformed*
/// positions of every matching info inside the settings box.
pub(crate) fn filter_blocks(
    template: &Template,
    origin: Pos,
    settings: &Settings,
    block: &str,
    reg: &BReg,
) -> Vec<BlockInfo> {
    let mut out = Vec::new();
    if template.palettes.is_empty() {
        return out;
    }
    let palette = {
        let mut no_shared: Option<&mut Xor> = None;
        let mut pick = settings_rng(&mut no_shared, origin);
        pick.next_bound(template.palettes.len() as i32) as usize
    };
    for info in &template.palettes[palette] {
        if reg.name_of(info.state) != block {
            continue;
        }
        let p = transform_pos(info.pos, settings.mirror, settings.rotation, settings.pivot);
        let pos = (
            p.0.wrapping_add(origin.0),
            p.1.wrapping_add(origin.1),
            p.2.wrapping_add(origin.2),
        );
        if let Some(clip) = &settings.clip {
            if !bb_inside(clip, pos) {
                continue;
            }
        }
        out.push(BlockInfo {
            pos,
            state: info.state,
            nbt: info.nbt.clone(),
        });
    }
    out
}
// ---------------------------------------------------------------- height providers

#[derive(Clone, Copy, Debug)]
pub(crate) enum VerticalAnchor {
    Absolute(i32),
    AboveBottom(i32),
    BelowTop(i32),
}

impl VerticalAnchor {
    /// `VerticalAnchor.resolveY(WorldGenerationContext)`. `below_top: n` is the
    /// n-th block *below* the top, so the offset subtracts: vanilla's
    /// `minGenY + genDepth - 1 - n`. Adding it instead asks for a y above the
    /// dimension, which silently widens the `HeightProvider` range.
    fn resolve(self, min_y: i32, depth: i32) -> i32 {
        match self {
            Self::Absolute(y) => y,
            Self::AboveBottom(offset) => min_y + offset,
            Self::BelowTop(offset) => min_y + depth - 1 - offset,
        }
    }

    fn parse(value: &crate::json::J) -> Result<Self, &'static str> {
        use crate::json::J;
        if let Some(J::Num(y)) = value.get("absolute") {
            return Ok(Self::Absolute(*y as i32));
        }
        if let Some(J::Num(offset)) = value.get("above_bottom") {
            return Ok(Self::AboveBottom(*offset as i32));
        }
        if let Some(J::Num(offset)) = value.get("below_top") {
            return Ok(Self::BelowTop(*offset as i32));
        }
        Err("unsupported vertical anchor")
    }
}

#[derive(Clone, Debug)]
pub(crate) enum HeightProvider {
    Constant(VerticalAnchor),
    Uniform(VerticalAnchor, VerticalAnchor),
}

impl HeightProvider {
    fn parse(value: &crate::json::J) -> Result<Self, &'static str> {
        use crate::json::J;
        match value.get("type").map(J::str) {
            Some("minecraft:constant") => Ok(Self::Constant(VerticalAnchor::parse(
                value.get("value").ok_or("constant height needs value")?,
            )?)),
            Some("minecraft:uniform") => Ok(Self::Uniform(
                VerticalAnchor::parse(
                    value.get("min_inclusive").ok_or("uniform height needs min")?,
                )?,
                VerticalAnchor::parse(
                    value.get("max_inclusive").ok_or("uniform height needs max")?,
                )?,
            )),
            _ => Err("unsupported height provider"),
        }
    }

    /// `HeightProvider.sample` over `WorldGenerationContext(minGenY, genDepth)`.
    pub(crate) fn sample(&self, rng: &mut Lcg, min_y: i32, depth: i32) -> i32 {
        match self {
            Self::Constant(anchor) => anchor.resolve(min_y, depth),
            Self::Uniform(min, max) => {
                let min = min.resolve(min_y, depth);
                let max = max.resolve(min_y, depth);
                if min > max {
                    min
                } else {
                    rng.next_bound(max - min + 1) + min
                }
            }
        }
    }
}

// ---------------------------------------------------------------- structure definitions

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum OceanTemp {
    Warm,
    Cold,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PortalPlacement {
    OnLandSurface,
    PartlyBuried,
    OnOceanFloor,
    InMountain,
    Underground,
    InNether,
}

impl PortalPlacement {
    fn parse(value: &str) -> Result<Self, &'static str> {
        Ok(match value {
            "on_land_surface" => Self::OnLandSurface,
            "partly_buried" => Self::PartlyBuried,
            "on_ocean_floor" => Self::OnOceanFloor,
            "in_mountain" => Self::InMountain,
            "underground" => Self::Underground,
            "in_nether" => Self::InNether,
            _ => return Err("unknown ruined portal placement"),
        })
    }

    /// `RuinedPortalPiece.getHeightmapType`.
    pub(crate) fn heightmap(self) -> HeightmapKind {
        if self == Self::OnOceanFloor {
            HeightmapKind::OceanFloorWg
        } else {
            HeightmapKind::WorldSurfaceWg
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct PortalSetup {
    pub(crate) placement: PortalPlacement,
    pub(crate) air_pocket_probability: f32,
    pub(crate) mossiness: f32,
    pub(crate) overgrown: bool,
    pub(crate) vines: bool,
    pub(crate) can_be_cold: bool,
    pub(crate) replace_with_blackstone: bool,
    pub(crate) weight: f32,
}

impl PortalSetup {
    fn parse(value: &crate::json::J) -> Result<Self, &'static str> {
        let number = |key: &str, default: f32| crate::json::jnum(value, key).unwrap_or(default as f64) as f32;
        let flag = |key: &str| crate::json::jbool(value, key, false);
        Ok(PortalSetup {
            placement: PortalPlacement::parse(
                &crate::json::jstr(value, "placement").ok_or("portal setup placement")?,
            )?,
            air_pocket_probability: number("air_pocket_probability", 0.0),
            mossiness: number("mossiness", 0.0),
            overgrown: flag("overgrown"),
            vines: flag("vines"),
            can_be_cold: flag("can_be_cold"),
            replace_with_blackstone: flag("replace_with_blackstone"),
            weight: number("weight", 1.0),
        })
    }
}

#[derive(Clone, Debug)]
pub(crate) enum StructureKind {
    Igloo,
    NetherFossil { height: HeightProvider },
    Shipwreck { beached: bool },
    OceanRuin {
        temp: OceanTemp,
        large_probability: f32,
        cluster_probability: f32,
    },
    RuinedPortal { setups: Vec<PortalSetup> },
}

pub(crate) fn parse_structure(value: &crate::json::J) -> Result<StructureKind, &'static str> {
    match crate::json::jstr(value, "type").as_deref() {
        Some("minecraft:igloo") => Ok(StructureKind::Igloo),
        Some("minecraft:nether_fossil") => Ok(StructureKind::NetherFossil {
            height: HeightProvider::parse(value.get("height").ok_or("fossil height")?)?,
        }),
        Some("minecraft:shipwreck") => Ok(StructureKind::Shipwreck {
            beached: crate::json::jbool(value, "is_beached", false),
        }),
        Some("minecraft:ocean_ruin") => Ok(StructureKind::OceanRuin {
            temp: match crate::json::jstr(value, "biome_temp").as_deref() {
                Some("cold") => OceanTemp::Cold,
                _ => OceanTemp::Warm,
            },
            large_probability: crate::json::jnum(value, "large_probability").unwrap_or(0.0) as f32,
            cluster_probability: crate::json::jnum(value, "cluster_probability")
                .unwrap_or(0.0) as f32,
        }),
        Some("minecraft:ruined_portal") => Ok(StructureKind::RuinedPortal {
            setups: crate::json::jarr(value, "setups")
                .ok_or("ruined portal setups")?
                .iter()
                .map(PortalSetup::parse)
                .collect::<Result<Vec<_>, _>>()?,
        }),
        _ => Err("unsupported template structure type"),
    }
}

// ---------------------------------------------------------------- pieces

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum IglooPart {
    Top,
    Middle,
    Bottom,
}

impl IglooPart {
    pub(crate) fn template(self) -> &'static str {
        match self {
            Self::Top => "minecraft:igloo/top",
            Self::Middle => "minecraft:igloo/middle",
            Self::Bottom => "minecraft:igloo/bottom",
        }
    }
    /// `IglooPieces.STRUCTURE_OFFSETS`.
    pub(crate) fn offset(self) -> Pos {
        match self {
            Self::Top => (0, 0, 0),
            Self::Middle => (2, -3, 4),
            Self::Bottom => (0, -3, -2),
        }
    }
    /// `IglooPieces.STRUCTURE_PIVOTS`.
    pub(crate) fn pivot(self) -> Pos {
        match self {
            Self::Top => (3, 5, 5),
            Self::Middle => (1, 3, 1),
            Self::Bottom => (3, 6, 7),
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct PortalProps {
    pub(crate) cold: bool,
    pub(crate) mossiness: f32,
    pub(crate) air_pocket: bool,
    pub(crate) overgrown: bool,
    pub(crate) vines: bool,
    pub(crate) replace_with_blackstone: bool,
}

#[derive(Clone, Debug)]
pub(crate) enum PieceKind {
    NetherFossil,
    Igloo(IglooPart),
    Shipwreck {
        beached: bool,
    },
    OceanRuin {
        large: bool,
    },
    RuinedPortal {
        placement: PortalPlacement,
        props: PortalProps,
    },
}

#[derive(Clone, Debug)]
pub(crate) struct Piece {
    pub(crate) template: String,
    pub(crate) pos: Pos,
    pub(crate) bb: BB,
    pub(crate) mirror: Mirror,
    pub(crate) rotation: Rotation,
    pub(crate) pivot: Pos,
    pub(crate) apply_waterlogging: bool,
    pub(crate) ignore_entities: bool,
    pub(crate) processors: Vec<ExtProc>,
    pub(crate) kind: PieceKind,
}

pub(crate) struct FindCtx<'a> {
    pub(crate) cx: i32,
    pub(crate) cz: i32,
    pub(crate) cold_biome: bool,
    pub(crate) rng: &'a mut Lcg,
}

#[expect(clippy::too_many_arguments, reason = "preserve the established placement and simulation call signature")]
fn piece(
    template: &str,
    pos: Pos,
    rotation: Rotation,
    pivot: Pos,
    apply_waterlogging: bool,
    processors: Vec<ExtProc>,
    kind: PieceKind,
    template_size: (i32, i32, i32),
    mirror: Mirror,
) -> Piece {
    let bb = crate::structure_transform::bounds(template_size, pos, pivot, mirror, rotation);
    Piece {
        template: template.to_string(),
        pos,
        bb,
        mirror,
        rotation,
        pivot,
        apply_waterlogging,
        ignore_entities: false,
        processors,
        kind,
    }
}

fn block_ignore(blocks: &[&str]) -> ExtProc {
    ExtProc::Native(Processor::BlockIgnore(
        blocks.iter().map(|b| b.to_string()).collect(),
    ))
}

fn rule(input: RuleTest, output: u32, modifier: BlockEntityModifier) -> BlockRule {
    BlockRule {
        input,
        location: RuleTest::Always,
        position: PosRuleTest::Always,
        output,
        modifier,
    }
}

// ---------------------------------------------------------------- findGenerationPoint

/// The lazy `Structure.GenerationStub` consumer: native holds the piece list
/// behind `Either.left(consumer)` for every one of these structures, so the
/// pieces are built only when `GenerationStub.getPiecesBuilder()` runs. The
/// caller's `rngState()` draw therefore lands *between* `findGenerationPoint`
/// and the piece construction, and the consumer observes that advanced stream.
pub(crate) type PieceBuilder = Box<
    dyn FnOnce(
        &mut dyn TemplateWorld,
        &Templates,
        &mut Lcg,
        &BReg,
    ) -> Result<Vec<Piece>, &'static str>,
>;

pub(crate) fn find_generation_point<W: TemplateWorld>(
    world: &mut W,
    templates: &Templates,
    kind: &StructureKind,
    ctx: &mut FindCtx<'_>,
    reg: &BReg,
) -> Result<Option<(Pos, PieceBuilder)>, &'static str> {
    match kind {
        StructureKind::Igloo => Ok(Some(igloo_find(world, ctx))),
        StructureKind::NetherFossil { height } => nether_fossil_find(world, height, ctx, reg),
        StructureKind::Shipwreck { beached } => Ok(Some(shipwreck_find(world, *beached, ctx))),
        StructureKind::OceanRuin {
            temp,
            large_probability,
            cluster_probability,
        } => Ok(Some(ocean_ruin_find(
            world,
            *temp,
            *large_probability,
            *cluster_probability,
            ctx,
        ))),
        StructureKind::RuinedPortal { setups } => {
            ruined_portal_find(world, templates, setups, ctx, reg)
        }
    }
}

fn chunk_min(cx: i32, cz: i32) -> (i32, i32) {
    (cx << 4, cz << 4)
}

/// `Structure.onTopOfChunkCenter`: `getFirstOccupiedHeight` is
/// `getBaseHeight - 1` on the stub generator and draws nothing.
fn on_top_of_chunk_center<W: TemplateWorld>(
    world: &mut W,
    cx: i32,
    cz: i32,
    kind: HeightmapKind,
) -> Pos {
    let (min_x, min_z) = chunk_min(cx, cz);
    let (x, z) = (min_x + 8, min_z + 8);
    (x, world.base_height(kind, x, z), z)
}

fn igloo_find<W: TemplateWorld>(world: &mut W, ctx: &mut FindCtx<'_>) -> (Pos, PieceBuilder) {
    let stub = on_top_of_chunk_center(world, ctx.cx, ctx.cz, HeightmapKind::WorldSurfaceWg);
    let (min_x, min_z) = chunk_min(ctx.cx, ctx.cz);
    let builder: PieceBuilder = Box::new(move |_world, templates, rng, reg| {
        let rotation = random_rotation(&mut |bound| rng.next_bound(bound));
        let base = (min_x, 90, min_z);
        let processor = vec![block_ignore(&["minecraft:structure_block"])];
        let mut pieces = Vec::new();
        // `$$4.j()` is `nextDouble`, not `nextFloat`: two LCG draws.
        if rng.next_f64() < 0.5 {
            let count = rng.next_bound(8) + 4;
            let part = IglooPart::Bottom;
            let template = templates.get(part.template(), reg)?;
            let offset = part.offset();
            let pos = (
                base.0 + offset.0,
                base.1 + offset.1 - count * 3,
                base.2 + offset.2,
            );
            pieces.push(piece(
                part.template(),
                pos,
                rotation,
                part.pivot(),
                false,
                processor.clone(),
                PieceKind::Igloo(part),
                template.size,
                Mirror::None,
            ));
            for index in 0..count - 1 {
                let part = IglooPart::Middle;
                let template = templates.get(part.template(), reg)?;
                let offset = part.offset();
                let pos = (
                    base.0 + offset.0,
                    base.1 + offset.1 - index * 3,
                    base.2 + offset.2,
                );
                pieces.push(piece(
                    part.template(),
                    pos,
                    rotation,
                    part.pivot(),
                    false,
                    processor.clone(),
                    PieceKind::Igloo(part),
                    template.size,
                    Mirror::None,
                ));
            }
        }
        let part = IglooPart::Top;
        let template = templates.get(part.template(), reg)?;
        pieces.push(piece(
            part.template(),
            base,
            rotation,
            part.pivot(),
            false,
            processor,
            PieceKind::Igloo(part),
            template.size,
            Mirror::None,
        ));
        Ok(pieces)
    });
    (stub, builder)
}

fn nether_fossil_find<W: TemplateWorld>(
    world: &mut W,
    height: &HeightProvider,
    ctx: &mut FindCtx<'_>,
    reg: &BReg,
) -> Result<Option<(Pos, PieceBuilder)>, &'static str> {
    const FOSSILS: [&str; 14] = [
        "minecraft:nether_fossils/fossil_1",
        "minecraft:nether_fossils/fossil_2",
        "minecraft:nether_fossils/fossil_3",
        "minecraft:nether_fossils/fossil_4",
        "minecraft:nether_fossils/fossil_5",
        "minecraft:nether_fossils/fossil_6",
        "minecraft:nether_fossils/fossil_7",
        "minecraft:nether_fossils/fossil_8",
        "minecraft:nether_fossils/fossil_9",
        "minecraft:nether_fossils/fossil_10",
        "minecraft:nether_fossils/fossil_11",
        "minecraft:nether_fossils/fossil_12",
        "minecraft:nether_fossils/fossil_13",
        "minecraft:nether_fossils/fossil_14",
    ];
    let (min_x, min_z) = chunk_min(ctx.cx, ctx.cz);
    let x = min_x + ctx.rng.next_bound(16);
    let z = min_z + ctx.rng.next_bound(16);
    let sea = world.sea_level();
    // The `HeightProvider` samples `WorldGenerationContext(minGenY, genDepth)`, i.e. the
    // generator window - see `TemplateWorld::gen_min_y`. Deliberately not the world
    // window: measured on `nat-4242-nether-fossil`, that draw lands at y 78 instead of
    // the native 55.
    let min_y = world.gen_min_y();
    let depth = world.gen_height();
    let mut y = height.sample(ctx.rng, min_y, depth);
    while y > sea {
        // `NoiseColumn` cells, not the live chunk. Vanilla's `getBaseColumn` carries no
        // beardifier, so the scan must not see the mound the fossil's own `beard_thin`
        // raises around it; and the start is computed at `structure_starts`, before the
        // noise, surface and carver stages write anything, so the live column would fold
        // those stages into the placement too.
        let above = world.base_column(x, y, z).unwrap_or_else(|| world.get(x, y, z));
        y -= 1;
        let below = world.base_column(x, y, z).unwrap_or_else(|| world.get(x, y, z));
        let sturdy = reg.is_template_solid(below) == Some(true);
        if reg.is_air(above) && (reg.name_of(below) == "minecraft:soul_sand" || sturdy) {
            break;
        }
    }
    if y <= sea {
        return Ok(None);
    }
    let pos = (x, y, z);
    let builder: PieceBuilder = Box::new(move |_world, templates, rng, reg| {
        let rotation = random_rotation(&mut |bound| rng.next_bound(bound));
        let template_id = FOSSILS[rng.next_bound(FOSSILS.len() as i32) as usize];
        let template = templates.get(template_id, reg)?;
        let processor = vec![block_ignore(&["minecraft:air", "minecraft:structure_block"])];
        Ok(vec![piece(
            template_id,
            pos,
            rotation,
            (0, 0, 0),
            true,
            processor,
            PieceKind::NetherFossil,
            template.size,
            Mirror::None,
        )])
    });
    Ok(Some((pos, builder)))
}

const SHIPWRECK_OCEAN: [&str; 20] = [
    "minecraft:shipwreck/with_mast",
    "minecraft:shipwreck/upsidedown_full",
    "minecraft:shipwreck/upsidedown_fronthalf",
    "minecraft:shipwreck/upsidedown_backhalf",
    "minecraft:shipwreck/sideways_full",
    "minecraft:shipwreck/sideways_fronthalf",
    "minecraft:shipwreck/sideways_backhalf",
    "minecraft:shipwreck/rightsideup_full",
    "minecraft:shipwreck/rightsideup_fronthalf",
    "minecraft:shipwreck/rightsideup_backhalf",
    "minecraft:shipwreck/with_mast_degraded",
    "minecraft:shipwreck/upsidedown_full_degraded",
    "minecraft:shipwreck/upsidedown_fronthalf_degraded",
    "minecraft:shipwreck/upsidedown_backhalf_degraded",
    "minecraft:shipwreck/sideways_full_degraded",
    "minecraft:shipwreck/sideways_fronthalf_degraded",
    "minecraft:shipwreck/sideways_backhalf_degraded",
    "minecraft:shipwreck/rightsideup_full_degraded",
    "minecraft:shipwreck/rightsideup_fronthalf_degraded",
    "minecraft:shipwreck/rightsideup_backhalf_degraded",
];

const SHIPWRECK_BEACHED: [&str; 11] = [
    "minecraft:shipwreck/with_mast",
    "minecraft:shipwreck/sideways_full",
    "minecraft:shipwreck/sideways_fronthalf",
    "minecraft:shipwreck/sideways_backhalf",
    "minecraft:shipwreck/rightsideup_full",
    "minecraft:shipwreck/rightsideup_fronthalf",
    "minecraft:shipwreck/rightsideup_backhalf",
    "minecraft:shipwreck/with_mast_degraded",
    "minecraft:shipwreck/rightsideup_full_degraded",
    "minecraft:shipwreck/rightsideup_fronthalf_degraded",
    "minecraft:shipwreck/rightsideup_backhalf_degraded",
];

const SHIPWRECK_PIVOT: Pos = (4, 0, 15);

/// `Structure.getMeanFirstFreeHeight` / `Structure.getLowestY` over the four
/// bounding-box corners, always read from `WORLD_SURFACE_WG`.
fn corner_heights<W: TemplateWorld + ?Sized>(world: &mut W, bb: &BB) -> [i32; 4] {
    let x0 = bb[0];
    let z0 = bb[2];
    let x1 = bb[0] + (bb[3] - bb[0] + 1);
    let z1 = bb[2] + (bb[5] - bb[2] + 1);
    [
        world.base_height(HeightmapKind::WorldSurfaceWg, x0, z0),
        world.base_height(HeightmapKind::WorldSurfaceWg, x0, z1),
        world.base_height(HeightmapKind::WorldSurfaceWg, x1, z0),
        world.base_height(HeightmapKind::WorldSurfaceWg, x1, z1),
    ]
}

fn mean_corner_height<W: TemplateWorld + ?Sized>(world: &mut W, bb: &BB) -> i32 {
    let h = corner_heights(world, bb);
    (h[0] + h[1] + h[2] + h[3]) / 4
}

fn lowest_corner_height<W: TemplateWorld + ?Sized>(world: &mut W, bb: &BB) -> i32 {
    let h = corner_heights(world, bb);
    h[0].min(h[1]).min(h[2]).min(h[3])
}

fn shipwreck_find<W: TemplateWorld>(
    world: &mut W,
    beached: bool,
    ctx: &mut FindCtx<'_>,
) -> (Pos, PieceBuilder) {
    let kind = if beached {
        HeightmapKind::WorldSurfaceWg
    } else {
        HeightmapKind::OceanFloorWg
    };
    let stub = on_top_of_chunk_center(world, ctx.cx, ctx.cz, kind);
    let (min_x, min_z) = chunk_min(ctx.cx, ctx.cz);
    let builder: PieceBuilder = Box::new(move |world, templates, rng, reg| {
        let rotation = random_rotation(&mut |bound| rng.next_bound(bound));
        let table: &[&str] = if beached {
            &SHIPWRECK_BEACHED
        } else {
            &SHIPWRECK_OCEAN
        };
        let template_id = table[rng.next_bound(table.len() as i32) as usize];
        let template = templates.get(template_id, reg)?;
        let mut piece = piece(
            template_id,
            (min_x, 90, min_z),
            rotation,
            SHIPWRECK_PIVOT,
            true,
            vec![block_ignore(&["minecraft:air", "minecraft:structure_block"])],
            PieceKind::Shipwreck { beached },
            template.size,
            Mirror::None,
        );
        // `ShipwreckPiece.isTooLarge`: only templates wider/taller than 32 get a
        // pre-fixup position. No vanilla shipwreck template is.
        if template.size.0 > 32 || template.size.1 > 32 {
            let bb = piece.bb;
            let y = if beached {
                lowest_corner_height(world, &bb)
            } else {
                mean_corner_height(world, &bb)
            };
            piece.pos.1 = if beached {
                y - template.size.1 / 2 - rng.next_bound(3)
            } else {
                y
            };
        }
        Ok(vec![piece])
    });
    (stub, builder)
}

const OCEAN_RUIN_WARM: [&str; 8] = [
    "minecraft:underwater_ruin/warm_1",
    "minecraft:underwater_ruin/warm_2",
    "minecraft:underwater_ruin/warm_3",
    "minecraft:underwater_ruin/warm_4",
    "minecraft:underwater_ruin/warm_5",
    "minecraft:underwater_ruin/warm_6",
    "minecraft:underwater_ruin/warm_7",
    "minecraft:underwater_ruin/warm_8",
];
const OCEAN_RUIN_WARM_BIG: [&str; 4] = [
    "minecraft:underwater_ruin/big_warm_4",
    "minecraft:underwater_ruin/big_warm_5",
    "minecraft:underwater_ruin/big_warm_6",
    "minecraft:underwater_ruin/big_warm_7",
];
const OCEAN_RUIN_BRICK: [&str; 8] = [
    "minecraft:underwater_ruin/brick_1",
    "minecraft:underwater_ruin/brick_2",
    "minecraft:underwater_ruin/brick_3",
    "minecraft:underwater_ruin/brick_4",
    "minecraft:underwater_ruin/brick_5",
    "minecraft:underwater_ruin/brick_6",
    "minecraft:underwater_ruin/brick_7",
    "minecraft:underwater_ruin/brick_8",
];
const OCEAN_RUIN_BRICK_BIG: [&str; 4] = [
    "minecraft:underwater_ruin/big_brick_1",
    "minecraft:underwater_ruin/big_brick_2",
    "minecraft:underwater_ruin/big_brick_3",
    "minecraft:underwater_ruin/big_brick_8",
];
const OCEAN_RUIN_CRACKED: [&str; 8] = [
    "minecraft:underwater_ruin/cracked_1",
    "minecraft:underwater_ruin/cracked_2",
    "minecraft:underwater_ruin/cracked_3",
    "minecraft:underwater_ruin/cracked_4",
    "minecraft:underwater_ruin/cracked_5",
    "minecraft:underwater_ruin/cracked_6",
    "minecraft:underwater_ruin/cracked_7",
    "minecraft:underwater_ruin/cracked_8",
];
const OCEAN_RUIN_CRACKED_BIG: [&str; 4] = [
    "minecraft:underwater_ruin/big_cracked_1",
    "minecraft:underwater_ruin/big_cracked_2",
    "minecraft:underwater_ruin/big_cracked_3",
    "minecraft:underwater_ruin/big_cracked_8",
];
const OCEAN_RUIN_MOSSY: [&str; 8] = [
    "minecraft:underwater_ruin/mossy_1",
    "minecraft:underwater_ruin/mossy_2",
    "minecraft:underwater_ruin/mossy_3",
    "minecraft:underwater_ruin/mossy_4",
    "minecraft:underwater_ruin/mossy_5",
    "minecraft:underwater_ruin/mossy_6",
    "minecraft:underwater_ruin/mossy_7",
    "minecraft:underwater_ruin/mossy_8",
];
const OCEAN_RUIN_MOSSY_BIG: [&str; 4] = [
    "minecraft:underwater_ruin/big_mossy_1",
    "minecraft:underwater_ruin/big_mossy_2",
    "minecraft:underwater_ruin/big_mossy_3",
    "minecraft:underwater_ruin/big_mossy_8",
];

fn ocean_ruin_find<W: TemplateWorld>(
    world: &mut W,
    temp: OceanTemp,
    large_probability: f32,
    cluster_probability: f32,
    ctx: &mut FindCtx<'_>,
) -> (Pos, PieceBuilder) {
    let stub = on_top_of_chunk_center(world, ctx.cx, ctx.cz, HeightmapKind::OceanFloorWg);
    let (min_x, min_z) = chunk_min(ctx.cx, ctx.cz);
    let builder: PieceBuilder = Box::new(move |_world, templates, rng, reg| {
        let rotation = random_rotation(&mut |bound| rng.next_bound(bound));
        let mut pieces = Vec::new();
        ocean_ruin_add_pieces(
            templates,
            (min_x, 90, min_z),
            rotation,
            temp,
            large_probability,
            cluster_probability,
            rng,
            reg,
            &mut pieces,
        )?;
        Ok(pieces)
    });
    (stub, builder)
}

/// `OceanRuinPieces.addPieces`.
#[allow(clippy::too_many_arguments)]
fn ocean_ruin_add_pieces(
    templates: &Templates,
    pos: Pos,
    rotation: Rotation,
    temp: OceanTemp,
    large_probability: f32,
    cluster_probability: f32,
    rng: &mut Lcg,
    reg: &BReg,
    out: &mut Vec<Piece>,
) -> Result<(), &'static str> {
    let large = rng.next_f32() <= large_probability;
    let integrity = if large { 0.9 } else { 0.8 };
    ocean_ruin_add_piece(
        templates, pos, rotation, temp, large, integrity, rng, reg, out,
    )?;
    if large && rng.next_f32() <= cluster_probability {
        ocean_ruin_add_cluster(templates, pos, rotation, temp, rng, reg, out)?;
    }
    Ok(())
}

/// `OceanRuinPieces.addPiece`.
#[allow(clippy::too_many_arguments)]
fn ocean_ruin_add_piece(
    templates: &Templates,
    pos: Pos,
    rotation: Rotation,
    temp: OceanTemp,
    large: bool,
    integrity: f32,
    rng: &mut Lcg,
    reg: &BReg,
    out: &mut Vec<Piece>,
) -> Result<(), &'static str> {
    let add = |template_id: &str,
               integrity: f32,
               out: &mut Vec<Piece>|
     -> Result<(), &'static str> {
        let template = templates.get(template_id, reg)?;
        let processors = ocean_ruin_processors(temp, integrity, reg);
        out.push(piece(
            template_id,
            pos,
            rotation,
            (0, 0, 0),
            true,
            processors,
            PieceKind::OceanRuin { large },
            template.size,
            Mirror::None,
        ));
        Ok(())
    };
    match temp {
        OceanTemp::Warm => {
            let table: &[&str] = if large {
                &OCEAN_RUIN_WARM_BIG
            } else {
                &OCEAN_RUIN_WARM
            };
            let template_id = table[rng.next_bound(table.len() as i32) as usize];
            add(template_id, integrity, out)?;
        }
        OceanTemp::Cold => {
            let (brick, cracked, mossy): (&[&str], &[&str], &[&str]) = if large {
                (
                    &OCEAN_RUIN_BRICK_BIG,
                    &OCEAN_RUIN_CRACKED_BIG,
                    &OCEAN_RUIN_MOSSY_BIG,
                )
            } else {
                (&OCEAN_RUIN_BRICK, &OCEAN_RUIN_CRACKED, &OCEAN_RUIN_MOSSY)
            };
            let index = rng.next_bound(brick.len() as i32) as usize;
            add(brick[index], integrity, out)?;
            add(cracked[index], 0.7, out)?;
            add(mossy[index], 0.5, out)?;
        }
    }
    Ok(())
}

fn ocean_ruin_processors(temp: OceanTemp, integrity: f32, reg: &BReg) -> Vec<ExtProc> {
    let (input, output, table) = match temp {
        OceanTemp::Warm => (
            "minecraft:sand",
            "minecraft:suspicious_sand",
            "minecraft:archaeology/ocean_ruin_warm",
        ),
        OceanTemp::Cold => (
            "minecraft:gravel",
            "minecraft:suspicious_gravel",
            "minecraft:archaeology/ocean_ruin_cold",
        ),
    };
    let output = reg.state_of(output, None);
    let rule_processor = Processor::Rules(vec![rule(
        RuleTest::Block(input.to_string()),
        output,
        BlockEntityModifier::AppendLoot(table.to_string()),
    )]);
    vec![
        ExtProc::Native(Processor::BlockRot {
            integrity,
            rottable: None,
        }),
        block_ignore(&["minecraft:air", "minecraft:structure_block"]),
        ExtProc::Native(Processor::Capped {
            delegate: Box::new(rule_processor),
            limit: CappedLimit::Const(5),
        }),
    ]
}

/// `OceanRuinPieces.addClusterPieces`.
#[allow(clippy::too_many_arguments)]
fn ocean_ruin_add_cluster(
    templates: &Templates,
    pos: Pos,
    rotation: Rotation,
    temp: OceanTemp,
    rng: &mut Lcg,
    reg: &BReg,
    out: &mut Vec<Piece>,
) -> Result<(), &'static str> {
    let center = (pos.0, 90, pos.2);
    let far = transform_pos((15, 0, 15), Mirror::None, rotation, (0, 0, 0));
    let far = (center.0 + far.0, center.1, center.2 + far.2);
    let main_box = [
        center.0.min(far.0),
        center.1,
        center.2.min(far.2),
        center.0.max(far.0),
        center.1,
        center.2.max(far.2),
    ];
    let base = (center.0.min(far.0), center.1, center.2.min(far.2));
    let mut candidates = ocean_ruin_cluster_positions(rng, base);
    let count = rng.next_bound(8 - 4 + 1) + 4;
    for _ in 0..count {
        if candidates.is_empty() {
            break;
        }
        let index = rng.next_bound(candidates.len() as i32) as usize;
        let spot = candidates.remove(index);
        let rotation = random_rotation(&mut |bound| rng.next_bound(bound));
        let far = transform_pos((5, 0, 6), Mirror::None, rotation, (0, 0, 0));
        let far = (spot.0 + far.0, spot.1, spot.2 + far.2);
        let candidate_box = [
            spot.0.min(far.0),
            spot.1,
            spot.2.min(far.2),
            spot.0.max(far.0),
            spot.1,
            spot.2.max(far.2),
        ];
        if !bb_intersects(&candidate_box, &main_box) {
            ocean_ruin_add_piece(
                templates, spot, rotation, temp, false, 0.8, rng, reg, out,
            )?;
        }
    }
    Ok(())
}

fn ocean_ruin_cluster_positions(rng: &mut Lcg, pos: Pos) -> Vec<Pos> {
    let between = |rng: &mut Lcg, low: i32, high: i32| rng.next_bound(high - low + 1) + low;
    let mut out = Vec::with_capacity(8);
    let candidate = |dx: i32, dz: i32| (pos.0 + dx, pos.1, pos.2 + dz);
    let next = |rng: &mut Lcg| between(rng, 1, 8);
    let (a, b) = (-16 + next(rng), 16 + between(rng, 1, 7));
    out.push(candidate(a, b));
    let (a, b) = (-16 + next(rng), between(rng, 1, 7));
    out.push(candidate(a, b));
    let (a, b) = (-16 + next(rng), -16 + between(rng, 4, 8));
    out.push(candidate(a, b));
    let (a, b) = (between(rng, 1, 7), 16 + between(rng, 1, 7));
    out.push(candidate(a, b));
    let (a, b) = (between(rng, 1, 7), -16 + between(rng, 4, 6));
    out.push(candidate(a, b));
    let (a, b) = (16 + between(rng, 1, 7), 16 + between(rng, 3, 8));
    out.push(candidate(a, b));
    let (a, b) = (16 + between(rng, 1, 7), between(rng, 1, 7));
    out.push(candidate(a, b));
    let (a, b) = (16 + between(rng, 1, 7), -16 + between(rng, 4, 8));
    out.push(candidate(a, b));
    out
}

const RUINED_PORTAL: [&str; 10] = [
    "minecraft:ruined_portal/portal_1",
    "minecraft:ruined_portal/portal_2",
    "minecraft:ruined_portal/portal_3",
    "minecraft:ruined_portal/portal_4",
    "minecraft:ruined_portal/portal_5",
    "minecraft:ruined_portal/portal_6",
    "minecraft:ruined_portal/portal_7",
    "minecraft:ruined_portal/portal_8",
    "minecraft:ruined_portal/portal_9",
    "minecraft:ruined_portal/portal_10",
];
const RUINED_PORTAL_GIANT: [&str; 3] = [
    "minecraft:ruined_portal/giant_portal_1",
    "minecraft:ruined_portal/giant_portal_2",
    "minecraft:ruined_portal/giant_portal_3",
];

fn ruined_portal_find<W: TemplateWorld>(
    world: &mut W,
    templates: &Templates,
    setups: &[PortalSetup],
    ctx: &mut FindCtx<'_>,
    reg: &BReg,
) -> Result<Option<(Pos, PieceBuilder)>, &'static str> {
    let setup = if setups.len() > 1 {
        let total: f32 = setups.iter().map(|setup| setup.weight).sum();
        let mut draw = ctx.rng.next_f32();
        let mut chosen = None;
        for setup in setups {
            draw -= setup.weight / total;
            if draw < 0.0 {
                chosen = Some(setup);
                break;
            }
        }
        chosen.ok_or("ruined portal setup selection")?
    } else {
        setups.first().ok_or("ruined portal has no setup")?
    };
    let air_pocket = if setup.air_pocket_probability == 0.0 {
        false
    } else if setup.air_pocket_probability == 1.0 {
        true
    } else {
        ctx.rng.next_f32() < setup.air_pocket_probability
    };
    let props = PortalProps {
        cold: false,
        mossiness: setup.mossiness,
        air_pocket,
        overgrown: setup.overgrown,
        vines: setup.vines,
        replace_with_blackstone: setup.replace_with_blackstone,
    };
    let template_id = if ctx.rng.next_f32() < 0.05 {
        let table = &RUINED_PORTAL_GIANT;
        table[ctx.rng.next_bound(table.len() as i32) as usize]
    } else {
        let table = &RUINED_PORTAL;
        table[ctx.rng.next_bound(table.len() as i32) as usize]
    };
    let rotation = random_rotation(&mut |bound| ctx.rng.next_bound(bound));
    let mirror = if ctx.rng.next_f32() < 0.5 {
        Mirror::None
    } else {
        Mirror::FrontBack
    };
    // `RuinedPortalStructure` reads the template inside findGenerationPoint for
    // its pivot and size; the piece itself is still lazy.
    let (size, pivot) = {
        let template = templates.get(template_id, reg)?;
        (template.size, (template.size.0 / 2, 0, template.size.2 / 2))
    };
    let (min_x, min_z) = chunk_min(ctx.cx, ctx.cz);
    let base = (min_x, 0, min_z);
    let bb = crate::structure_transform::bounds(size, base, pivot, mirror, rotation);
    let center = bb_center(&bb);
    let base_height = world.base_height(setup.placement.heightmap(), center.0, center.2) - 1;
    let y = ruined_portal_y(
        world,
        ctx.rng,
        setup.placement,
        props.air_pocket,
        base_height,
        &bb,
        setup.placement.heightmap(),
    );
    let pos = (base.0, y, base.2);
    let cold_biome = ctx.cold_biome;
    let can_be_cold = setup.can_be_cold;
    let placement = setup.placement;
    let builder: PieceBuilder = Box::new(move |_world, templates, _rng, reg| {
        let mut props = props;
        // `Properties.cold` is `Biome.coldEnoughToSnow` at the placement position;
        // the oracle's biome source is a single holder.
        if can_be_cold {
            props.cold = cold_biome;
        }
        let template = templates.get(template_id, reg)?;
        let processors = ruined_portal_processors(reg, placement, &props);
        Ok(vec![piece(
            template_id,
            pos,
            rotation,
            pivot,
            true,
            processors,
            PieceKind::RuinedPortal { placement, props },
            template.size,
            mirror,
        )])
    });
    Ok(Some(((min_x, y, min_z), builder)))
}

#[allow(clippy::too_many_arguments)]
fn ruined_portal_y<W: TemplateWorld>(
    world: &mut W,
    rng: &mut Lcg,
    placement: PortalPlacement,
    air_pocket: bool,
    base_height: i32,
    bb: &BB,
    heightmap: HeightmapKind,
) -> i32 {
    let min_y = world.min_y();
    let y_size = bb[4] - bb[1] + 1;
    let between = |rng: &mut Lcg, low: i32, high: i32| {
        if low < high {
            rng.next_bound(high - low + 1) + low
        } else {
            high
        }
    };
    let mut y = match placement {
        PortalPlacement::InNether => {
            if air_pocket {
                between(rng, 32, 100)
            } else if rng.next_f32() < 0.5 {
                between(rng, 27, 29)
            } else {
                between(rng, 29, 100)
            }
        }
        PortalPlacement::InMountain => between(rng, 70, base_height - y_size),
        PortalPlacement::Underground => between(rng, min_y + 15, base_height - y_size),
        PortalPlacement::PartlyBuried => base_height - y_size + between(rng, 2, 8),
        _ => base_height,
    };
    let corners = [
        (bb[0], bb[2]),
        (bb[3], bb[2]),
        (bb[0], bb[5]),
        (bb[3], bb[5]),
    ];
    let reg = breg().unwrap();
    let floor = min_y + 15;
    loop {
        if y <= floor {
            break;
        }
        let mut count = 0;
        for &(x, z) in &corners {
            if reg.heightmap_counts(world.get(x, y, z), heightmap) {
                count += 1;
                if count == 3 {
                    return y;
                }
            }
        }
        y -= 1;
    }
    y
}

fn ruined_portal_processors(
    reg: &BReg,
    placement: PortalPlacement,
    props: &PortalProps,
) -> Vec<ExtProc> {
    let air = reg.state_of("minecraft:air", None);
    let magma = reg.state_of("minecraft:magma_block", None);
    let netherrack = reg.state_of("minecraft:netherrack", None);
    let mut rules = Vec::new();
    rules.push(rule(
        RuleTest::RandomBlock {
            name: "minecraft:gold_block".to_string(),
            probability: 0.3,
        },
        air,
        BlockEntityModifier::Passthrough,
    ));
    match placement {
        PortalPlacement::OnOceanFloor => rules.push(rule(
            RuleTest::Block("minecraft:lava".to_string()),
            magma,
            BlockEntityModifier::Passthrough,
        )),
        _ => {
            if props.cold {
                rules.push(rule(
                    RuleTest::Block("minecraft:lava".to_string()),
                    netherrack,
                    BlockEntityModifier::Passthrough,
                ));
            } else {
                rules.push(rule(
                    RuleTest::RandomBlock {
                        name: "minecraft:lava".to_string(),
                        probability: 0.2,
                    },
                    magma,
                    BlockEntityModifier::Passthrough,
                ));
            }
        }
    }
    if !props.cold {
        rules.push(rule(
            RuleTest::RandomBlock {
                name: "minecraft:netherrack".to_string(),
                probability: 0.07,
            },
            magma,
            BlockEntityModifier::Passthrough,
        ));
    }
    let mut processors = vec![
        block_ignore(if props.air_pocket {
            &["minecraft:structure_block"]
        } else {
            &["minecraft:air", "minecraft:structure_block"]
        }),
        ExtProc::Native(Processor::Rules(rules)),
        ExtProc::BlockAge {
            mossiness: props.mossiness,
        },
        ExtProc::Native(Processor::Protected(
            "#minecraft:features_cannot_replace".to_string(),
        )),
        ExtProc::Native(Processor::LavaSubmergedBlock),
    ];
    if props.replace_with_blackstone {
        processors.push(ExtProc::Native(Processor::BlackstoneReplace));
    }
    processors
}

// ---------------------------------------------------------------- postProcess

#[expect(clippy::too_many_arguments, reason = "preserve the established placement and simulation call signature")]
pub(crate) fn place_structure<W: TemplateWorld>(
    world: &mut W,
    templates: &Templates,
    pieces: &mut [Piece],
    post_rng: &mut Xor,
    world_seed: u64,
    chunk_bb: &mut BB,
    stub: Pos,
    reg: &BReg,
) -> Result<PlacementOut, &'static str> {
    let mut out = PlacementOut::default();
    for piece in pieces.iter_mut() {
        if !bb_intersects(&piece.bb, chunk_bb) {
            continue;
        }
        let one = piece.post_process(
            world,
            templates,
            post_rng,
            world_seed,
            chunk_bb,
            stub,
            reg,
        )?;
        out.writes += one.writes;
        out.wrote |= one.wrote;
        out.be_events.extend(one.be_events);
        out.entities.extend(one.entities);
    }
    Ok(out)
}

impl Piece {
    #[expect(clippy::too_many_arguments, reason = "preserve the established placement and simulation call signature")]
    fn post_process<W: TemplateWorld>(
        &mut self,
        world: &mut W,
        templates: &Templates,
        post_rng: &mut Xor,
        world_seed: u64,
        chunk_bb: &mut BB,
        stub: Pos,
        reg: &BReg,
    ) -> Result<PlacementOut, &'static str> {
        match self.kind.clone() {
            PieceKind::NetherFossil => {
                let template = templates.get(&self.template, reg)?;
                let box_ = template_bb(&template, self.pos, self.mirror, self.rotation, self.pivot);
                bb_encapsulate(chunk_bb, &box_);
                self.template_pass(world, templates, post_rng, world_seed, chunk_bb, stub, reg)
            }
            PieceKind::Igloo(part) => {
                let offset = part.offset();
                let probe = transform_pos(
                    (3 - offset.0, 0, -offset.2),
                    self.mirror,
                    self.rotation,
                    self.pivot,
                );
                let probe = (self.pos.0 + probe.0, self.pos.1, self.pos.2 + probe.2);
                let height = world.heightmap(HeightmapKind::WorldSurfaceWg, probe.0, probe.2);
                let saved = self.pos;
                self.pos.1 += height - 90 - 1;
                let result =
                    self.template_pass(world, templates, post_rng, world_seed, chunk_bb, stub, reg);
                if part == IglooPart::Top {
                    let chest = transform_pos((3, 0, 5), self.mirror, self.rotation, self.pivot);
                    let chest = (
                        self.pos.0 + chest.0,
                        self.pos.1 + chest.1,
                        self.pos.2 + chest.2,
                    );
                    let below = (chest.0, chest.1 - 1, chest.2);
                    let state = world.get(below.0, below.1, below.2);
                    // `!below.is(Blocks.LADDER)` — the basement ladder marks the
                    // shaft where the surface must stay open.
                    if !reg.is_air(state) && reg.name_of(state) != "minecraft:ladder" {
                        let snow = reg.state_of("minecraft:snow_block", None);
                        world.set(chest.0, chest.1, chest.2, snow);
                    }
                }
                self.pos = saved;
                result
            }
            PieceKind::Shipwreck { beached } => {
                let template = templates.get(&self.template, reg)?;
                let size = template.size;
                let kind = if beached {
                    HeightmapKind::WorldSurfaceWg
                } else {
                    HeightmapKind::OceanFloorWg
                };
                // `int seaLevel = level.getMaxBuildHeight()` is a max accumulator
                // reduced by every footprint column.
                let mut min_height = world.min_y() + world.height();
                let mut mean = 0;
                let area = size.0 * size.2;
                if area == 0 {
                    mean = world.heightmap(kind, self.pos.0, self.pos.2);
                } else {
                    for x in self.pos.0..self.pos.0 + size.0 {
                        for z in self.pos.2..self.pos.2 + size.2 {
                            let height = world.heightmap(kind, x, z);
                            mean += height;
                            min_height = min_height.min(height);
                        }
                    }
                    mean /= area;
                }
                self.pos.1 = if beached {
                    min_height - size.1 / 2 - post_rng.next_bound(3)
                } else {
                    mean
                };
                self.template_pass(world, templates, post_rng, world_seed, chunk_bb, stub, reg)
            }
            PieceKind::OceanRuin { large: _ } => {
                let template = templates.get(&self.template, reg)?;
                let ocean_floor = world.heightmap(
                    HeightmapKind::OceanFloorWg,
                    self.pos.0,
                    self.pos.2,
                );
                self.pos.1 = ocean_floor;
                let end = transform_pos(
                    (template.size.0 - 1, 0, template.size.2 - 1),
                    Mirror::None,
                    self.rotation,
                    (0, 0, 0),
                );
                let end = (self.pos.0 + end.0, self.pos.1, self.pos.2 + end.2);
                self.pos.1 = ocean_ruin_height(world, self.pos, end, reg);
                self.template_pass(world, templates, post_rng, world_seed, chunk_bb, stub, reg)
            }
            PieceKind::RuinedPortal { placement, props } => {
                let template = templates.get(&self.template, reg)?;
                let box_ = template_bb(&template, self.pos, self.mirror, self.rotation, self.pivot);
                let center = bb_center(&box_);
                if !bb_inside(chunk_bb, center) {
                    return Ok(PlacementOut::default());
                }
                bb_encapsulate(chunk_bb, &box_);
                let result = self.template_pass(
                    world,
                    templates,
                    post_rng,
                    world_seed,
                    chunk_bb,
                    stub,
                    reg,
                )?;
                self.portal_base(world, post_rng, placement, props, reg);
                self.portal_columns(world, post_rng, placement, props, reg);
                if props.vines || props.overgrown {
                    let bb = self.bb;
                    for pos in bb_positions(&bb) {
                        if props.vines {
                            maybe_vine(world, post_rng, pos, reg);
                        }
                        if props.overgrown {
                            maybe_leaves(world, post_rng, pos, reg);
                        }
                    }
                }
                Ok(result)
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn template_pass<W: TemplateWorld>(
        &mut self,
        world: &mut W,
        templates: &Templates,
        post_rng: &mut Xor,
        world_seed: u64,
        clip: &BB,
        stub: Pos,
        reg: &BReg,
    ) -> Result<PlacementOut, &'static str> {
        let template = templates.get(&self.template, reg)?;
        let mut settings = Settings::new(self.mirror, self.rotation, self.pivot);
        settings.clip = Some(*clip);
        settings.apply_waterlogging = self.apply_waterlogging;
        settings.ignore_entities = self.ignore_entities;
        settings.processors = self.processors.clone();
        self.bb = template_bb(&template, self.pos, self.mirror, self.rotation, self.pivot);
        let mut out =
            place_in_world(world, &template, self.pos, stub, &settings, post_rng, world_seed, reg)?;
        if !out.wrote {
            return Ok(out);
        }
        let markers = filter_blocks(
            &template,
            self.pos,
            &settings,
            "minecraft:structure_block",
            reg,
        );
        for info in markers {
            let Some(nbt) = &info.nbt else {
                continue;
            };
            if nbt_str(nbt, "mode").as_deref() != Some("DATA") {
                continue;
            }
            let metadata = nbt_str(nbt, "metadata").unwrap_or_default();
            self.data_marker(world, &metadata, info.pos, post_rng, &mut out, reg)?;
        }
        let jigsaws = filter_blocks(&template, self.pos, &settings, "minecraft:jigsaw", reg);
        for info in jigsaws {
            let Some(nbt) = &info.nbt else {
                continue;
            };
            let Some(raw) = nbt_str(nbt, "final_state") else {
                continue;
            };
            let state = parse_block_state(&raw, reg)
                .unwrap_or_else(|| reg.state_of("minecraft:air", None));
            world.set(info.pos.0, info.pos.1, info.pos.2, state);
            // The jigsaw block (and its block entity) is replaced by the final
            // state; the harness clears the entry when the new state has none.
            match block_entity_id(reg.name_of(state)) {
                Some(id) => out.be_events.push(BeEvent::Set(BlockEntityRecord {
                    pos: info.pos,
                    id,
                    loot_table: None,
                    loot_seed: None,
                    items: None,
                    spawner_entity: None,
                })),
                None => out.be_events.push(BeEvent::Clear(info.pos)),
            }
        }
        Ok(out)
    }

    /// `TemplateStructurePiece.postProcess`'s `handleDataMarker` overrides.
    fn data_marker<W: TemplateWorld>(
        &self,
        world: &mut W,
        metadata: &str,
        pos: Pos,
        rng: &mut Xor,
        out: &mut PlacementOut,
        reg: &BReg,
    ) -> Result<(), &'static str> {
        let below = (pos.0, pos.1 - 1, pos.2);
        match &self.kind {
            PieceKind::Igloo(_) => {
                if metadata == "chest" {
                    let air = reg.state_of("minecraft:air", None);
                    world.set(pos.0, pos.1, pos.2, air);
                    out.be_events.push(BeEvent::Clear(pos));
                    let seed = rng.next_i64();
                    set_block_entity_loot(out, below, "minecraft:chests/igloo_chest", seed);
                }
            }
            PieceKind::Shipwreck { .. } => {
                let table = match metadata {
                    "map_chest" => "minecraft:chests/shipwreck_map",
                    "treasure_chest" => "minecraft:chests/shipwreck_treasure",
                    "supply_chest" => "minecraft:chests/shipwreck_supply",
                    _ => return Ok(()),
                };
                let seed = rng.next_i64();
                set_block_entity_loot(out, below, table, seed);
            }
            PieceKind::OceanRuin { large, .. } => {
                if metadata == "chest" {
                    let water =
                        reg.fluid_name(world.get(pos.0, pos.1, pos.2)) == "minecraft:water";
                    let chest = reg
                        .checked_state(
                            "minecraft:chest",
                            &[(
                                "waterlogged".to_string(),
                                if water { "true" } else { "false" }.to_string(),
                            )],
                        )
                        .unwrap_or(0);
                    world.set(pos.0, pos.1, pos.2, chest);
                    let table = if *large {
                        "minecraft:chests/underwater_ruin_big"
                    } else {
                        "minecraft:chests/underwater_ruin_small"
                    };
                    let seed = rng.next_i64();
                    out.be_events.push(BeEvent::Set(BlockEntityRecord {
                        pos,
                        id: "minecraft:chest",
                        loot_table: Some(table.to_string()),
                        loot_seed: Some(seed),
                        items: None,
                        spawner_entity: None,
                    }));
                } else if metadata == "drowned" {
                    out.entities.push(EntitySpawn {
                        pos,
                        id: "minecraft:drowned".to_string(),
                        hanging: None,
                    });
                    let state = if pos.1 > world.sea_level() {
                        reg.state_of("minecraft:air", None)
                    } else {
                        reg.state_of("minecraft:water", None)
                    };
                    world.set(pos.0, pos.1, pos.2, state);
                }
            }
            PieceKind::RuinedPortal { .. } | PieceKind::NetherFossil => {}
        }
        Ok(())
    }

    /// `RuinedPortalPiece.placeNetherrackBase` (`b(RandomSource, LevelAccessor)`).
    fn portal_base<W: TemplateWorld>(
        &self,
        world: &mut W,
        rng: &mut Xor,
        placement: PortalPlacement,
        props: PortalProps,
        reg: &BReg,
    ) {
        const WEIGHTS: [f32; 14] = [
            1.0, 1.0, 1.0, 1.0, 1.0, 1.0, 1.0, 0.9, 0.9, 0.8, 0.7, 0.6, 0.4, 0.2,
        ];
        let flat = matches!(
            placement,
            PortalPlacement::OnLandSurface | PortalPlacement::OnOceanFloor
        );
        let center = bb_center(&self.bb);
        let span = (self.bb[3] - self.bb[0] + 1 + (self.bb[5] - self.bb[2] + 1)) / 2;
        let offset = rng.next_bound((8 - span / 2).max(1));
        let n = WEIGHTS.len() as i32;
        for dx in center.0 - n..=center.0 + n {
            for dz in center.2 - n..=center.2 + n {
                let distance = (dx - center.0).abs() + (dz - center.2).abs();
                let index = (distance + offset).max(0);
                if index >= n {
                    continue;
                }
                if rng.next_f64() >= f64::from(WEIGHTS[index as usize]) {
                    continue;
                }
                // `getSurfaceY` = `level.getHeight(heightmap, x, z) - 1`, i.e. the top
                // solid block of the WORLD_SURFACE_WG / OCEAN_FLOOR_WG heightmap. That
                // map is frozen at the end of the carver pass, not at the surface.
                let height = world.heightmap(placement.heightmap(), dx, dz) - 1;
                let y = if flat { height } else { height.min(self.bb[1]) };
                if (y - self.bb[1]).abs() > 3 {
                    continue;
                }
                let pos = (dx, y, dz);
                if !portal_can_replace(world, pos, placement, reg) {
                    continue;
                }
                place_portal_block(world, rng, pos, props, reg);
                if props.overgrown {
                    maybe_leaves(world, rng, pos, reg);
                }
                portal_column(world, rng, (pos.0, pos.1 - 1, pos.2), props);
            }
        }
    }

    /// `RuinedPortalPiece.placeNetherrackColumns` (`a(RandomSource, LevelAccessor)`).
    fn portal_columns<W: TemplateWorld>(
        &self,
        world: &mut W,
        rng: &mut Xor,
        _placement: PortalPlacement,
        props: PortalProps,
        reg: &BReg,
    ) {
        let bb = self.bb;
        // `for (x = minX + 1; x < maxX; x++) for (z = minZ + 1; z < maxZ; z++)`:
        // the interior of the piece bounding box only.
        for x in bb[0] + 1..bb[3] {
            for z in bb[2] + 1..bb[5] {
                if reg.name_of(world.get(x, bb[1], z)) == "minecraft:netherrack" {
                    portal_column(world, rng, (x, bb[1] - 1, z), props);
                }
            }
        }
    }
}

/// `RuinedPortalPiece.placeNetherrackIfNotCold` (`d`): one draw only when cold.
fn place_portal_block<W: TemplateWorld>(
    world: &mut W,
    rng: &mut Xor,
    pos: Pos,
    props: PortalProps,
    reg: &BReg,
) {
    let state = if !props.cold && rng.next_f32() < 0.07 {
        reg.state_of("minecraft:magma_block", None)
    } else {
        reg.state_of("minecraft:netherrack", None)
    };
    world.set(pos.0, pos.1, pos.2, state);
}

/// `RuinedPortalPiece.placeNetherrackColumn` (`c`).
fn portal_column<W: TemplateWorld>(
    world: &mut W,
    rng: &mut Xor,
    pos: Pos,
    props: PortalProps,
) {
    let reg = breg().unwrap();
    place_portal_block(world, rng, pos, props, reg);
    let mut y = pos.1;
    let mut remaining = 8;
    while remaining > 0 && rng.next_f32() < 0.5 {
        y -= 1;
        remaining -= 1;
        place_portal_block(world, rng, (pos.0, y, pos.2), props, reg);
    }
}

/// `RuinedPortalPiece.canReplace`.
fn portal_can_replace<W: TemplateWorld>(
    world: &mut W,
    pos: Pos,
    placement: PortalPlacement,
    reg: &BReg,
) -> bool {
    let state = world.get(pos.0, pos.1, pos.2);
    let name = reg.name_of(state);
    !reg.is_air(state)
        && name != "minecraft:obsidian"
        && !reg.in_tag(state, "minecraft:features_cannot_replace")
        && (placement == PortalPlacement::InNether || name != "minecraft:lava")
}

/// `RuinedPortalPiece.maybeAddVine` (`a(RandomSource, LevelAccessor, BlockPos)`).
fn maybe_vine<W: TemplateWorld>(world: &mut W, rng: &mut Xor, pos: Pos, reg: &BReg) {
    let state = world.get(pos.0, pos.1, pos.2);
    let name = reg.name_of(state);
    if reg.is_air(state) || name == "minecraft:vine" {
        return;
    }
    // `Direction.Plane.HORIZONTAL.getRandomDirection` = [NORTH, EAST, SOUTH, WEST].
    const HORIZONTAL: [(&str, u8, (i32, i32, i32)); 4] = [
        ("north", 2, (0, 0, -1)),
        ("east", 5, (1, 0, 0)),
        ("south", 3, (0, 0, 1)),
        ("west", 4, (-1, 0, 0)),
    ];
    let (_, direction, (dx, dy, dz)) = HORIZONTAL[rng.next_bound(4) as usize];
    let target = (pos.0 + dx, pos.1 + dy, pos.2 + dz);
    if !reg.is_air(world.get(target.0, target.1, target.2)) {
        return;
    }
    if !reg.is_collision_shape_full_block(state) {
        return;
    }
    // `VineBlock.getPropertyForFace(direction.getOpposite())`.
    let property = match direction {
        2 => "south",
        5 => "west",
        3 => "north",
        _ => "east",
    };
    let vine = reg.state_of("minecraft:vine", None);
    let vine = reg.with_prop(vine, property, "true");
    world.set(target.0, target.1, target.2, vine);
}

/// `RuinedPortalPiece.maybeAddLeaves` (`b(RandomSource, LevelAccessor, BlockPos)`).
fn maybe_leaves<W: TemplateWorld>(world: &mut W, rng: &mut Xor, pos: Pos, reg: &BReg) {
    if rng.next_f32() >= 0.5 {
        return;
    }
    if reg.name_of(world.get(pos.0, pos.1, pos.2)) != "minecraft:netherrack" {
        return;
    }
    let above = (pos.0, pos.1 + 1, pos.2);
    if !reg.is_air(world.get(above.0, above.1, above.2)) {
        return;
    }
    let leaves = reg.state_of("minecraft:jungle_leaves", None);
    let leaves = reg.with_prop(leaves, "persistent", "true");
    world.set(above.0, above.1, above.2, leaves);
}

/// `RuinedPortalPiece.updateShape`/`OceanRuinPieces` height search used by the
/// ocean ruin piece: walk down while the column is air/water/ice.
fn ocean_ruin_height<W: TemplateWorld>(
    world: &mut W,
    start: Pos,
    end: Pos,
    reg: &BReg,
) -> i32 {
    let start_y = start.1;
    let base_y = start_y - 1;
    let mut min_y = 512;
    let mut low = 0;
    let floor = world.min_y() + 1;
    for x in start.0..=end.0 {
        for z in start.2..=end.2 {
            let mut y = start_y - 1;
            loop {
                let state = world.get(x, y, z);
                let fluid = reg.fluid_name(state);
                let ice = reg.in_tag(state, "minecraft:ice");
                if !(reg.is_air(state) || fluid == "minecraft:water" || ice) || y <= floor {
                    break;
                }
                y -= 1;
            }
            min_y = min_y.min(y);
            if y < base_y - 2 {
                low += 1;
            }
        }
    }
    let dx = (start.0 - end.0).abs();
    if base_y - min_y > 2 && low > dx - 2 {
        min_y + 1
    } else {
        start_y
    }
}

/// `RandomizableContainer.setBlockEntityLootTable` for a chest below a marker.
fn set_block_entity_loot(out: &mut PlacementOut, pos: Pos, table: &str, seed: i64) {
    for event in out.be_events.iter_mut().rev() {
        if let BeEvent::Set(record) = event {
            if record.pos == pos && record.id == "minecraft:chest" {
                record.loot_table = Some(table.to_string());
                record.loot_seed = Some(seed);
                return;
            }
        }
    }
}

// ---------------------------------------------------------------- native fixture

#[cfg(all(test, megu_vanilla_data))]
mod template_structures_tests {
    use super::*;
    use crate::generation_math::sha256;
    use crate::json::{jarr, jint, jnum, jparse, jstr, J};
    use std::collections::HashMap;

    /// `VerticalAnchor.BELOW_TOP` is `minGenY + genDepth - 1 - n`: the n-th block *below*
    /// the top. Adding the offset instead asks for a y above the dimension, which widens
    /// the `HeightProvider` range and shifts every `nether_fossil` sample (measured on
    /// seed 4242 chunk (-22,20): the fossil landed 23 blocks too high).
    #[test]
    fn vertical_anchor_resolves_like_the_native_anchors() {
        assert_eq!(VerticalAnchor::Absolute(7).resolve(-64, 384), 7);
        assert_eq!(VerticalAnchor::Absolute(7).resolve(0, 128), 7);
        assert_eq!(VerticalAnchor::AboveBottom(4).resolve(0, 128), 4);
        assert_eq!(VerticalAnchor::AboveBottom(4).resolve(-64, 384), -60);
        // `below_top: 0` is the top block, so the dimension's top y is height - 1.
        assert_eq!(VerticalAnchor::BelowTop(0).resolve(0, 128), 127);
        assert_eq!(VerticalAnchor::BelowTop(2).resolve(0, 128), 125);
        assert_eq!(VerticalAnchor::BelowTop(5).resolve(-64, 384), 314);
    }

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    fn sha(bytes: &[u8]) -> String {
        hex(&sha256(bytes))
    }

    /// Java `DataOutputStream.writeUTF`.
    fn write_utf(out: &mut Vec<u8>, text: &str) {
        out.extend_from_slice(&(text.len() as u16).to_be_bytes());
        out.extend_from_slice(text.as_bytes());
    }

    fn state_tag(state: u32, reg: &BReg) -> Result<Tag, &'static str> {
        let mut out = BTreeMap::from([(
            NbtString::text("Name"),
            Tag::String(NbtString::text(reg.name_of(state))),
        )]);
        let properties = reg
            .properties_of(state)
            .ok_or("unknown state in template")?;
        if !properties.is_empty() {
            out.insert(
                NbtString::text("Properties"),
                Tag::Compound(
                    properties
                        .into_iter()
                        .map(|(k, v)| (NbtString::text(&k), Tag::String(NbtString::text(&v))))
                        .collect(),
                ),
            );
        }
        Ok(Tag::Compound(out))
    }

    /// `StructureTemplateOracle.writeBlockInfo`.
    fn write_block_info(out: &mut Vec<u8>, info: &BlockInfo, reg: &BReg) -> Result<(), &'static str> {
        for v in [info.pos.0, info.pos.1, info.pos.2] {
            out.extend_from_slice(&v.to_be_bytes());
        }
        out.extend(crate::nbt::write_named(
            &NbtString::text(""),
            &state_tag(info.state, reg)?,
        )?);
        out.push(u8::from(info.nbt.is_some()));
        if let Some(nbt) = &info.nbt {
            out.extend(crate::nbt::write_named(&NbtString::text(""), nbt)?);
        }
        Ok(())
    }

    fn point(value: &J) -> Pos {
        let a = value.as_arr().unwrap();
        (a[0].num() as i32, a[1].num() as i32, a[2].num() as i32)
    }

    fn box_of(value: &J) -> BB {
        let a = value.as_arr().unwrap();
        [
            a[0].num() as i32,
            a[1].num() as i32,
            a[2].num() as i32,
            a[3].num() as i32,
            a[4].num() as i32,
            a[5].num() as i32,
        ]
    }

    fn state_from_json(value: &J, reg: &BReg) -> u32 {
        let name = jstr(value, "Name").expect("state Name");
        let mut properties = Vec::new();
        if let Some(J::Obj(props)) = value.get("Properties") {
            for (key, value) in props {
                if let J::Str(value) = value {
                    properties.push((key.clone(), value.clone()));
                }
            }
        }
        reg.checked_state(&name, &properties)
            .unwrap_or_else(|| panic!("unknown state {name}"))
    }

    /// The oracle's `ScriptedTerrain`, including its `stateAt`/`baseHeight`.
    #[derive(Clone)]
    struct SceneSpec {
        min_y: i32,
        height: i32,
        sea: i32,
        ocean: bool,
        ws: HashMap<(i32, i32), i32>,
        oc: HashMap<(i32, i32), i32>,
        cells: HashMap<Pos, u32>,
        top: u32,
        fill: u32,
        air: u32,
        water: u32,
    }

    impl SceneSpec {
        fn parse(terrain: Option<&J>, reg: &BReg) -> Self {
            let mut scene = SceneSpec {
                min_y: -64,
                height: 384,
                sea: 63,
                ocean: false,
                ws: HashMap::new(),
                oc: HashMap::new(),
                cells: HashMap::new(),
                top: reg.state_of("minecraft:stone", None),
                fill: reg.state_of("minecraft:stone", None),
                air: reg.state_of("minecraft:air", None),
                water: reg.state_of("minecraft:water", None),
            };
            let Some(terrain) = terrain else {
                return scene;
            };
            scene.min_y = jint(terrain, "min_y", scene.min_y);
            scene.height = jint(terrain, "height", scene.height);
            scene.sea = jint(terrain, "sea_level", scene.sea);
            scene.ocean = matches!(terrain.get("ocean"), Some(J::Num(n)) if *n != 0.0);
            if let Some(name) = jstr(terrain, "top_block") {
                scene.top = reg.state_of(&name, None);
            }
            if let Some(name) = jstr(terrain, "fill_block") {
                scene.fill = reg.state_of(&name, None);
            }
            if let Some(J::Obj(heights)) = terrain.get("heights") {
                for (key, value) in heights {
                    let (x, z) = key.split_once(',').expect("height column");
                    let x: i32 = x.parse().expect("height x");
                    let z: i32 = z.parse().expect("height z");
                    match value {
                        J::Num(h) => {
                            let h = *h as i32;
                            scene.ws.insert((x, z), h);
                            scene.oc.insert((x, z), if scene.ocean { h - 8 } else { h });
                        }
                        J::Obj(kind) => {
                            let kind = J::Obj(kind.clone());
                            scene.ws.insert((x, z), jint(&kind, "ws", 0));
                            scene.oc.insert((x, z), jint(&kind, "oc", 0));
                        }
                        _ => panic!("height column must be int or object"),
                    }
                }
            }
            if let Some(J::Obj(cells)) = terrain.get("cells") {
                for (key, value) in cells {
                    let mut parts = key.split(',');
                    let x: i32 = parts.next().expect("cell x").parse().expect("cell x");
                    let y: i32 = parts.next().expect("cell y").parse().expect("cell y");
                    let z: i32 = parts.next().expect("cell z").parse().expect("cell z");
                    let state = match value {
                        J::Str(name) => reg.state_of(name, None),
                        other => state_from_json(other, reg),
                    };
                    scene.cells.insert((x, y, z), state);
                }
            }
            scene
        }

        fn base_height(&self, kind: HeightmapKind, x: i32, z: i32) -> i32 {
            let ocean_floor =
                matches!(kind, HeightmapKind::OceanFloor | HeightmapKind::OceanFloorWg);
            if self.ocean {
                let ws = self.ws.get(&(x, z)).copied().unwrap_or(64);
                let oc = self.oc.get(&(x, z)).copied().unwrap_or(ws - 8);
                if ocean_floor {
                    oc
                } else {
                    ws
                }
            } else {
                self.ws.get(&(x, z)).copied().unwrap_or(70)
            }
        }

        fn state_at(&self, x: i32, y: i32, z: i32) -> u32 {
            if let Some(&state) = self.cells.get(&(x, y, z)) {
                return state;
            }
            if self.ocean {
                let ws = self.ws.get(&(x, z)).copied().unwrap_or(64);
                let oc = self.oc.get(&(x, z)).copied().unwrap_or(ws - 8);
                if y >= ws {
                    self.air
                } else if y >= oc {
                    self.water
                } else if y == oc - 1 {
                    self.top
                } else {
                    self.fill
                }
            } else {
                let ws = self.ws.get(&(x, z)).copied().unwrap_or(70);
                if y >= ws {
                    self.air
                } else if y == ws - 1 {
                    self.top
                } else {
                    self.fill
                }
            }
        }
    }

    struct TestWorld {
        scene: SceneSpec,
        writes: HashMap<Pos, u32>,
        /// `getRawBrightness(pos, 0)` for the scripted scene: `None` models a world with no
        /// light engine (the default, and what a feature-time caller sees in the port),
        /// `Some(n)` a lit one at the promotion boundary.
        light: Option<u8>,
    }

    impl TestWorld {
        fn new(scene: SceneSpec) -> Self {
            TestWorld {
                scene,
                writes: HashMap::new(),
                light: None,
            }
        }
    }

    impl TemplateWorld for TestWorld {
        fn get(&mut self, x: i32, y: i32, z: i32) -> u32 {
            if let Some(&state) = self.writes.get(&(x, y, z)) {
                return state;
            }
            self.scene.state_at(x, y, z)
        }
        fn set(&mut self, x: i32, y: i32, z: i32, state: u32) -> bool {
            if y < self.scene.min_y || y >= self.scene.min_y + self.scene.height {
                return false;
            }
            self.writes.insert((x, y, z), state);
            true
        }
        fn heightmap(&mut self, kind: HeightmapKind, x: i32, z: i32) -> i32 {
            let reg = breg().unwrap();
            let top = self.scene.min_y + self.scene.height - 1;
            let mut y = top;
            while y >= self.scene.min_y {
                let state = self.get(x, y, z);
                if reg.heightmap_counts(state, kind) {
                    return y + 1;
                }
                y -= 1;
            }
            self.scene.min_y
        }
        fn base_height(&mut self, kind: HeightmapKind, x: i32, z: i32) -> i32 {
            self.scene.base_height(kind, x, z)
        }
        fn sea_level(&self) -> i32 {
            self.scene.sea
        }
        fn raw_brightness(&mut self, _pos: Pos) -> Option<u8> {
            self.light
        }
        fn min_y(&self) -> i32 {
            self.scene.min_y
        }
        fn height(&self) -> i32 {
            self.scene.height
        }
    }

    fn world_hash(reg: &BReg, writes: &HashMap<Pos, u32>) -> (usize, String) {
        let mut keys: Vec<Pos> = writes.keys().copied().collect();
        keys.sort_by_key(|&(x, y, z)| (x, y, z));
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&(keys.len() as i32).to_be_bytes());
        for &(x, y, z) in &keys {
            write_utf(&mut bytes, &format!("{x},{y},{z}"));
            write_utf(&mut bytes, &reg.canonical_state(writes[&(x, y, z)]));
        }
        (keys.len(), sha(&bytes))
    }

    fn be_lines(events: &[BeEvent]) -> Vec<String> {
        let mut map: BTreeMap<Pos, Option<BlockEntityRecord>> = BTreeMap::new();
        for event in events {
            match event {
                BeEvent::Set(record) => {
                    map.insert(record.pos, Some(record.clone()));
                }
                BeEvent::Clear(pos) => {
                    map.insert(*pos, None);
                }
            }
        }
        map.into_values().flatten()
            .map(|record| {
                let items = record
                    .items
                    .as_ref()
                    .and_then(|items| {
                        crate::nbt::write_named(&NbtString::text(""), items).ok()
                    })
                    .map(|bytes| sha(&bytes))
                    .unwrap_or_default();
                format!(
                    "{},{},{}|{}|{}|{}|{}",
                    record.pos.0,
                    record.pos.1,
                    record.pos.2,
                    record.id,
                    record.loot_table.clone().unwrap_or_default(),
                    if record.loot_table.is_some() {
                        record.loot_seed.unwrap_or(0)
                    } else {
                        0
                    },
                    items
                )
            })
            .collect()
    }

    fn hash_lines(lines: &[String]) -> String {
        if lines.is_empty() {
            "none".to_string()
        } else {
            sha(lines.join("\n").as_bytes())
        }
    }

    /// Measured on seed 4242 chunk (-96,35): the mineshaft corridor's oak fence
    /// rails sat beside a `tuff_bricks` wall the trial-chamber template wrote
    /// *after* them, and native's fence carried the `south` connection bit the
    /// port did not. `updateShapeAtEdge` over the written voxels is what sets it:
    /// the face from the new wall towards the existing fence is exposed, and the
    /// fence's own `updateShape` answers in that direction.
    #[test]
    fn shape_passes_connect_an_existing_fence_to_a_newly_written_neighbour() {
        let reg = breg().expect("block registry");
        let scene = SceneSpec::parse(None, reg);
        let mut world = TestWorld::new(scene);
        let fence_pos = (0, 70, 0);
        let fence = reg.with_prop(
            reg.state_of("minecraft:oak_fence", None),
            "west",
            "true",
        );
        world.set(fence_pos.0, fence_pos.1, fence_pos.2, fence);
        // The written voxel: the wall the later piece places, south of the fence.
        let wall_pos = (0, 70, 1);
        let wall = reg.state_of("minecraft:tuff_bricks", None);
        world.set(wall_pos.0, wall_pos.1, wall_pos.2, wall);

        let changed = apply_shape_passes(&[wall_pos], &mut world, reg);
        assert!(
            changed.contains(&(fence_pos, world.get(0, 70, 0)))
                || world.get(0, 70, 0) != fence,
            "the fence must be reported as a shape write"
        );
        let after = world.get(0, 70, 0);
        assert_eq!(
            reg.get_prop(after, "south").as_deref(),
            Some("true"),
            "the fence is adjacent to the new wall on its south face"
        );
        assert_eq!(reg.get_prop(after, "west").as_deref(), Some("true"));
        assert_eq!(reg.get_prop(after, "east").as_deref(), Some("false"));
        assert_eq!(reg.get_prop(after, "north").as_deref(), Some("false"));
        assert_eq!(world.get(0, 70, 1), wall, "the written cell keeps its state");
    }

    #[test]
    fn shape_passes_remove_tall_seagrass_upper_when_template_breaks_its_lower_half() {
        let reg = breg().expect("block registry");
        let scene = SceneSpec::parse(None, reg);
        let mut world = TestWorld::new(scene);
        let upper = reg.with_prop(reg.state_of("minecraft:tall_seagrass", None), "half", "upper");
        let lower = reg.with_prop(reg.state_of("minecraft:tall_seagrass", None), "half", "lower");
        let gravel = reg.state_of("minecraft:gravel", None);
        let air = reg.state_of("minecraft:air", None);

        // Seed 4242 ocean_ruin_cold (-151,-180): the upper half at
        // (-2414,40,-2880) is exposed over gravel by a structure template.
        world.set(0, 70, 0, upper);
        world.set(0, 69, 0, gravel);
        let writes = apply_shape_passes(&[(0, 69, 0)], &mut world, reg);
        assert_eq!(world.get(0, 70, 0), air);
        assert!(writes.contains(&((0, 70, 0), air)));

        world.set(0, 70, 0, upper);
        world.set(0, 69, 0, lower);
        apply_shape_passes(&[(0, 69, 0)], &mut world, reg);
        assert_eq!(world.get(0, 70, 0), upper);
    }

    /// The other measured half. A fence the placement state left pointing at air
    /// (`mineshaft` rails are written with one bit only) is the *written* cell in
    /// a jigsaw piece here, so `updateFromNeighbourShapes` recomputes all four
    /// bits from the world as it stands — north/the-log-neighbour set, the rest
    /// cleared. A fence outside both the written set and every written face is
    /// untouched, which is how native keeps the stale bits it is known to keep.
    #[test]
    fn shape_passes_recompute_a_written_fence_from_the_final_world() {
        let reg = breg().expect("block registry");
        let scene = SceneSpec::parse(None, reg);
        let mut world = TestWorld::new(scene);
        let log = reg.state_of("minecraft:oak_log", None);
        // West support only; nothing else connects.
        world.set(-1, 70, 0, log);
        let fence_pos = (0, 70, 0);
        let placement = reg.with_prop(
            reg.state_of("minecraft:oak_fence", None),
            "north",
            "true",
        );
        world.set(fence_pos.0, fence_pos.1, fence_pos.2, placement);
        // A fence somewhere else, carrying a stale bit toward air.
        let stale_pos = (6, 70, 6);
        world.set(stale_pos.0, stale_pos.1, stale_pos.2, placement);

        apply_shape_passes(&[fence_pos], &mut world, reg);

        let after = world.get(0, 70, 0);
        assert_eq!(
            reg.get_prop(after, "west").as_deref(),
            Some("true"),
            "the recompute picks up the log the placement bit did not describe"
        );
        assert_eq!(
            reg.get_prop(after, "north").as_deref(),
            Some("false"),
            "the placement bit pointed at air and is cleared"
        );
        assert_eq!(reg.get_prop(after, "east").as_deref(), Some("false"));
        assert_eq!(reg.get_prop(after, "south").as_deref(), Some("false"));
        assert_eq!(
            world.get(stale_pos.0, stale_pos.1, stale_pos.2),
            placement,
            "a fence outside the written set keeps its state"
        );
    }

    /// `SnowyDirtBlock.updateShape` with `UP`: the three blocks in the pinned 1.21.1
    /// table that carry `snowy` (`grass_block`, `podzol`, `mycelium`) flip to `true`
    /// under either form of snow. This is the arm the igloo `(37,-512)` residual
    /// was missing (`evidence/gameplay/feature-parity/igloo-snowy-under-floor-20261119.md`).
    #[test]
    fn snowy_dirt_flips_true_under_snow_block_and_snow_layer() {
        let reg = breg().expect("block registry");
        let mut world = TestWorld::new(SceneSpec::parse(None, reg));
        let pos = (0, 70, 0);
        let snow_block = reg.state_of("minecraft:snow_block", None);
        let snow_layer = reg
            .checked_state("minecraft:snow", &[("layers".into(), "1".into())])
            .expect("snow[layers=1]");
        assert!(reg.in_tag(snow_block, "minecraft:snow"));
        assert!(reg.in_tag(snow_layer, "minecraft:snow"));

        for name in ["minecraft:grass_block", "minecraft:podzol", "minecraft:mycelium"] {
            let dry = reg.with_prop(reg.state_of(name, None), "snowy", "false");
            assert_eq!(reg.get_prop(dry, "snowy").as_deref(), Some("false"));
            for (neighbor, label) in [(snow_block, "snow_block"), (snow_layer, "snow[layers=1]")] {
                let next = update_shape(reg, dry, 1, neighbor, &mut world, pos);
                assert_eq!(reg.name_of(next), name, "{name} under {label} keeps its block");
                assert_eq!(
                    reg.get_prop(next, "snowy").as_deref(),
                    Some("true"),
                    "{name} under {label} must become snowy"
                );
                assert_ne!(next, dry, "{name} under {label} must actually change state");
            }
        }
    }

    /// The other direction of the same update: removing the snow above a snowy
    /// dirt block (`UP` with a non-snow neighbour) clears the property.
    #[test]
    fn snowy_dirt_flips_false_when_the_snow_above_is_removed() {
        let reg = breg().expect("block registry");
        let mut world = TestWorld::new(SceneSpec::parse(None, reg));
        let pos = (0, 70, 0);
        let air = reg.state_of("minecraft:air", None);
        let stone = reg.state_of("minecraft:stone", None);
        for name in ["minecraft:grass_block", "minecraft:podzol", "minecraft:mycelium"] {
            let snowy = reg.with_prop(reg.state_of(name, None), "snowy", "true");
            for (neighbor, label) in [(air, "air"), (stone, "stone")] {
                let next = update_shape(reg, snowy, 1, neighbor, &mut world, pos);
                assert_eq!(
                    reg.get_prop(next, "snowy").as_deref(),
                    Some("false"),
                    "{name} under {label} must not stay snowy"
                );
                assert_eq!(reg.name_of(next), name);
            }
        }
    }

    /// `SnowyDirtBlock.updateShape` delegates to `super` for every other side, so
    /// a snow neighbour below or beside the block must not touch `snowy`.
    #[test]
    fn snowy_dirt_ignores_snow_on_non_up_faces() {
        let reg = breg().expect("block registry");
        let mut world = TestWorld::new(SceneSpec::parse(None, reg));
        let pos = (0, 70, 0);
        let snow_block = reg.state_of("minecraft:snow_block", None);
        let grass = reg.state_of("minecraft:grass_block", None);
        for snowy in ["false", "true"] {
            let state = reg.with_prop(grass, "snowy", snowy);
            for direction in [0u8, 2, 3, 4, 5] {
                let next = update_shape(reg, state, direction, snow_block, &mut world, pos);
                assert_eq!(
                    next, state,
                    "direction {direction} with a snow neighbour must leave snowy={snowy}"
                );
            }
        }
    }

    /// The real caller, not just the arm: writing the igloo's bottom `snow_block`
    /// (or later replacing it with stone) must be visible on the grass below.
    #[test]
    fn shape_passes_flip_the_grass_below_a_written_snow_block() {
        let reg = breg().expect("block registry");
        let mut world = TestWorld::new(SceneSpec::parse(None, reg));
        let below = (0, 70, 0);
        let above = (0, 71, 0);
        let grass = reg.with_prop(reg.state_of("minecraft:grass_block", None), "snowy", "false");
        world.set(below.0, below.1, below.2, grass);
        let snow_block = reg.state_of("minecraft:snow_block", None);
        world.set(above.0, above.1, above.2, snow_block);

        apply_shape_passes(&[above], &mut world, reg);
        assert_eq!(
            reg.get_prop(world.get(below.0, below.1, below.2), "snowy").as_deref(),
            Some("true"),
            "the grass under a newly written snow_block becomes snowy"
        );

        // Removing the snow (replacing it with stone) is the same face, now UP
        // with a non-snow neighbour, so the grass must flip back.
        let stone = reg.state_of("minecraft:stone", None);
        world.set(above.0, above.1, above.2, stone);
        apply_shape_passes(&[above], &mut world, reg);
        assert_eq!(
            reg.get_prop(world.get(below.0, below.1, below.2), "snowy").as_deref(),
            Some("false"),
            "the grass under a removed snow_block stops being snowy"
        );
    }

    /// `StructurePiece.SHAPE_CHECK_BLOCKS` is a `Blocks` *identity* set of eleven
    /// blocks, not a name pattern. The other fence woods, the panes and the
    /// soul/redstone torch family are absent, so native's `placeBlock` never marks
    /// them for a connection recompute. An `ends_with("_fence")` predicate would mark
    /// six more fence families, and a `_torch` one four more torches.
    #[test]
    fn the_postprocess_shape_set_is_exactly_the_native_eleven() {
        let reg = breg().expect("block registry");
        let state =
            |name: &str| reg.checked_state(name, &[]).unwrap_or_else(|| panic!("{name}"));
        for name in [
            "minecraft:nether_brick_fence",
            "minecraft:torch",
            "minecraft:wall_torch",
            "minecraft:oak_fence",
            "minecraft:spruce_fence",
            "minecraft:birch_fence",
            "minecraft:jungle_fence",
            "minecraft:acacia_fence",
            "minecraft:dark_oak_fence",
            "minecraft:ladder",
            "minecraft:iron_bars",
        ] {
            assert!(reg.needs_postprocess_shape(state(name)), "{name}");
        }
        for name in [
            "minecraft:crimson_fence",
            "minecraft:warped_fence",
            "minecraft:mangrove_fence",
            "minecraft:cherry_fence",
            "minecraft:bamboo_fence",
            "minecraft:glass_pane",
            "minecraft:white_stained_glass_pane",
            "minecraft:soul_torch",
            "minecraft:soul_wall_torch",
            "minecraft:redstone_torch",
            "minecraft:redstone_wall_torch",
            "minecraft:oak_fence_gate",
            "minecraft:lantern",
            "minecraft:chain",
        ] {
            assert!(!reg.needs_postprocess_shape(state(name)), "{name}");
        }
    }

    /// `MultifaceBlock.updateShape`, the rule behind the two `glow_lichen` cells of the
    /// seed 4242 chunk (-96,35) capture: the trial chamber writes them with `east=true`
    /// and their support lies outside the placement box, so the face is cleared and the
    /// cell — with no face left — becomes air.
    #[test]
    fn postprocess_clears_a_multiface_face_whose_support_is_gone() {
        let reg = breg().expect("block registry");
        let full_cube = reg.state_of("minecraft:deepslate", None);
        assert!(
            reg.supports_multiface(full_cube, 2),
            "a full cube carries the face that looks at it"
        );
        let scene = SceneSpec::parse(None, reg);
        let mut world = TestWorld::new(scene);
        // The scene fills every column with stone up to its top, so the air the face
        // looks into has to be written explicitly.
        for (dx, dz) in [(1, 0), (0, 0)] {
            world.set(dx, -30, dz, reg.state_of("minecraft:air", None));
        }
        // North is a full cube, east is air.
        world.set(0, -30, -1, full_cube);
        let lichen = reg.with_prop(
            reg.with_prop(reg.state_of("minecraft:glow_lichen", None), "north", "true"),
            "east",
            "true",
        );
        world.set(0, -30, 0, lichen);

        assert_eq!(postprocess_shape_marks(&mut world, reg, &[(0, -30, 0)]), 1);
        let after = world.get(0, -30, 0);
        assert_eq!(
            reg.get_prop(after, "east").as_deref(),
            Some("false"),
            "nothing behind the east face"
        );
        assert_eq!(
            reg.get_prop(after, "north").as_deref(),
            Some("true"),
            "the north face keeps its support"
        );
    }

    /// `LevelChunk.postProcessGeneration`'s fluid branch, through the registered ticker: a
    /// marked **source** fluid cell is ticked and its flowing level lands in the neighbour.
    ///
    /// This is the branch that produced the reference's two `water[level=1]` cells
    /// (`dvi.H(534)` -> `FluidState.tick` in the capture's RUNTIME stack). The `flowing_fluid`
    /// machine lives below this layer, so the test registers the same kind of ticker `main`
    /// does and drives the real pass over a marked source.
    #[test]
    fn postprocess_ticks_a_marked_source_fluid_into_its_neighbour() {
        let reg = breg().expect("block registry");
        // The seam is process-global (`register_fluid_ticker`), so the case that installs a
        // ticker must not overlap another test that runs the pass.
        static TICKER: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _guard = TICKER.lock().unwrap_or_else(|e| e.into_inner());
        let scene = SceneSpec::parse(None, reg);
        let mut world = TestWorld::new(scene);
        let air = reg.state_of("minecraft:air", None);
        let stone = reg.state_of("minecraft:stone", None);
        let source = reg.state_of("minecraft:water", None);
        // A one-wide channel: floor, source, and open air to the south (+z).
        for z in -1..=1 {
            world.set(0, -31, z, stone);
            world.set(0, -30, z, air);
        }
        world.set(0, -30, -1, source);

        // Stand-in for `flowing_fluid::tick`: spread one flowing level south from a source.
        fn spread_south(world: &mut dyn FluidTickTarget, pos: Pos) {
            let Some(FluidCell::Fluid { water, level }) = world.fluid_cell(pos) else {
                return;
            };
            if !water || level != 0 {
                return;
            }
            let south = (pos.0, pos.1, pos.2 + 1);
            if world.fluid_cell(south) == Some(FluidCell::Air) {
                world.set_fluid(south, FluidCell::Fluid { water: true, level: 1 });
            }
        }
        register_fluid_ticker(spread_south);
        let changed = postprocess_shape_marks(&mut world, reg, &[(0, -30, -1)]);
        // The housekeeping ticker back: a later test must not inherit this stand-in.
        *GLOBAL_FLUID_TICKER.lock().unwrap_or_else(|e| e.into_inner()) = None;

        assert_eq!(changed, 0, "a liquid block is skipped by the shape branch");
        let south = world.get(0, -30, 0);
        assert_eq!(
            reg.get_prop(south, "level").as_deref(),
            Some("1"),
            "the tick wrote a flowing level into the neighbour"
        );
        assert_eq!(
            reg.get_prop(world.get(0, -30, -1), "level").as_deref(),
            Some("0"),
            "the marked source cell itself is untouched"
        );
    }

    /// The pass' cell vocabulary is `FlowingFluid.canHoldFluid` / `canPassThrough`, not "air or
    /// full cube": a cell with an **empty collision shape** and no fluid is `Passable`.
    ///
    /// The measured cell is `(100,19,-513)` of the village capture: `minecraft:short_grass` in
    /// this port and water in native, and the tick at the marked source `(98,19,-513)` stopped
    /// there before this arm existed. A waterlogged state is *not* `Passable` (vanilla enters it
    /// through `placeLiquid`, which this machine cannot write) and neither is a state vanilla's
    /// `canHoldFluid` refuses by name.
    #[test]
    fn the_pass_sees_an_empty_collision_shape_as_a_cell_a_fluid_can_hold() {
        let reg = breg().expect("block registry");
        let scene = SceneSpec::parse(None, reg);
        let mut world = TestWorld::new(scene);
        let air = reg.state_of("minecraft:air", None);
        world.set(0, -30, 0, reg.state_of("minecraft:short_grass", None));
        world.set(0, -30, 1, reg.state_of("minecraft:stone", None));
        world.set(0, -30, 2, reg.state_of("minecraft:water", None));
        world.set(0, -30, 3, air);
        world.set(0, -30, 4, reg.state_of("minecraft:ladder", None));
        world.set(
            0,
            -30,
            5,
            reg.state_of("minecraft:sugar_cane", None),
        );
        let sign = reg.state_of("minecraft:oak_sign", None);
        world.set(0, -30, 6, sign);
        assert_eq!(
            world.fluid_cell((0, -30, 0)),
            Some(FluidCell::Passable),
            "short_grass: empty collision, not in canHoldFluid's refusal list"
        );
        assert_eq!(world.fluid_cell((0, -30, 1)), Some(FluidCell::Solid));
        assert_eq!(
            world.fluid_cell((0, -30, 2)),
            Some(FluidCell::Fluid { water: true, level: 0 })
        );
        assert_eq!(world.fluid_cell((0, -30, 3)), Some(FluidCell::Air));
        for (z, name) in [
            (4, "ladder"),
            (5, "sugar_cane"),
            (6, "oak_sign (waterlogged-capable)"),
        ] {
            assert_eq!(
                world.fluid_cell((0, -30, z)),
                None,
                "{name} keeps its refusal: canHoldFluid answers it by name"
            );
        }
    }

    /// The other half of the same rule: once the last face is gone the cell is air, not
    /// a multiface block with every property `false`. `sculk_vein` shares the rule.
    #[test]
    fn postprocess_removes_a_multiface_cell_whose_last_face_is_gone() {
        let reg = breg().expect("block registry");
        let scene = SceneSpec::parse(None, reg);
        let mut world = TestWorld::new(scene);
        let air = reg.state_of("minecraft:air", None);
        for (pos, name, face, support) in [
            ((0, -30, 0), "minecraft:glow_lichen", "east", (1, -30, 0)),
            ((0, -30, 4), "minecraft:sculk_vein", "up", (0, -29, 4)),
        ] {
            world.set(support.0, support.1, support.2, air);
            let block = reg.with_prop(reg.state_of(name, None), face, "true");
            world.set(pos.0, pos.1, pos.2, block);
            assert_eq!(postprocess_shape_marks(&mut world, reg, &[pos]), 1);
            assert_eq!(world.get(pos.0, pos.1, pos.2), air, "{name}");
        }
    }

    /// `LevelChunk.postProcessGeneration`'s cell rule over a `StructurePiece` list: the
    /// fence's stale `north` bit is cleared and the log to its west is picked up, while a
    /// liquid cell in the same list is skipped (`!(state.getBlock() instanceof
    /// LiquidBlock)`) instead of being shape-updated into air.
    #[test]
    fn postprocess_recomputes_a_marked_fence_and_skips_liquid() {
        let reg = breg().expect("block registry");
        let scene = SceneSpec::parse(None, reg);
        let mut world = TestWorld::new(scene);
        let log = reg.state_of("minecraft:oak_log", None);
        let fence = reg.with_prop(
            reg.state_of("minecraft:oak_fence", None),
            "north",
            "true",
        );
        let water = reg.with_prop(reg.state_of("minecraft:water", None), "level", "0");
        world.set(-1, 70, 0, log);
        world.set(0, 70, 0, fence);
        world.set(4, 70, 4, water);

        let changed = postprocess_shape_marks(&mut world, reg, &[(0, 70, 0), (4, 70, 4)]);
        assert_eq!(changed, 1);
        let after = world.get(0, 70, 0);
        assert_eq!(reg.get_prop(after, "west").as_deref(), Some("true"));
        assert_eq!(
            reg.get_prop(after, "north").as_deref(),
            Some("false"),
            "the placement bit pointed at air"
        );
        assert_eq!(world.get(4, 70, 4), water, "liquid cells are skipped");
    }

    /// The brightness half of `MushroomBlock.canSurvive`, read at the boundary: the same cell a
    /// feature wrote is removed by the shape pass once the engine is lit, kept while it is dark,
    /// and left alone when there is no lit engine to ask (`None`). The support test is
    /// `isSolidRender` (not `#minecraft:dirt`), and `#minecraft:mushroom_grow_block` skips the
    /// brightness test entirely.
    #[test]
    fn postprocess_removes_a_mushroom_only_where_the_lit_engine_reaches_it() {
        let reg = breg().expect("block registry");
        let scene = SceneSpec::parse(None, reg);
        let air = reg.state_of("minecraft:air", None);
        for (light, support, removed) in [
            // `15 - 2` from a fire two cells away: `rawBrightness < 13` is false.
            (Some(13u8), "minecraft:netherrack", true),
            (Some(12), "minecraft:netherrack", false),
            (Some(15), "minecraft:netherrack", true),
            // `#minecraft:mushroom_grow_block` answers canSurvive before the light is read.
            (Some(15), "minecraft:podzol", false),
            // `isSolidRender(support)` is the other half: water is not a support.
            (Some(0), "minecraft:water", true),
            // No lit engine (a feature-time caller, or a scripted scene): do not guess.
            (None, "minecraft:netherrack", false),
        ] {
            for mushroom in ["minecraft:brown_mushroom", "minecraft:red_mushroom"] {
                let mut world = TestWorld::new(scene.clone());
                world.light = light;
                world.set(0, -31, 0, reg.state_of(support, None));
                world.set(0, -30, 0, reg.state_of(mushroom, None));
                let changed = postprocess_shape_marks(&mut world, reg, &[(0, -30, 0)]);
                assert_eq!(
                    world.get(0, -30, 0) == air,
                    removed,
                    "{mushroom} on {support} with light {light:?}"
                );
                assert_eq!(changed, usize::from(removed), "changed count {mushroom}");
            }
        }
    }

    /// The third block of the set: `MagmaBlock.updateShape` only calls
    /// `level.scheduleTick(pos.above(), Fluids.WATER, 20)` when water sits above and returns the
    /// state unchanged, so its mark is consumed without a rewrite. A light-driven `canSurvive`
    /// arm copied from the mushrooms would turn this cell into air.
    #[test]
    fn postprocess_leaves_a_marked_magma_block_unchanged() {
        let reg = breg().expect("block registry");
        let scene = SceneSpec::parse(None, reg);
        let mut world = TestWorld::new(scene);
        world.light = Some(15);
        let magma = reg.state_of("minecraft:magma_block", None);
        world.set(0, -30, 0, magma);
        world.set(0, -29, 0, reg.state_of("minecraft:water", None));
        assert_eq!(postprocess_shape_marks(&mut world, reg, &[(0, -30, 0)]), 0);
        assert_eq!(world.get(0, -30, 0), magma);
    }

    /// A template's hanging entity NBT carries its facing and framed stack, and the
    /// piece rotation turns the facing with the blocks (native
    /// `HangingEntity.rotate`, which `StructureTemplate.placeInWorld` applies after
    /// reading `Facing`). A vertical facing is not a hanging entity: native
    /// `HangingEntity.setDirection` validates a horizontal direction and throws.
    #[test]
    fn a_hanging_entity_reads_its_facing_and_stack_from_template_nbt() {
        let frame_nbt = Tag::Compound(BTreeMap::from([
            (
                NbtString::text("id"),
                Tag::String(NbtString::text("minecraft:item_frame")),
            ),
            // `Direction.get3DDataValue` of SOUTH is 3 in 1.21.1.
            (NbtString::text("Facing"), Tag::Byte(3)),
            (
                NbtString::text("Item"),
                Tag::Compound(BTreeMap::from([
                    (
                        NbtString::text("id"),
                        Tag::String(NbtString::text("minecraft:elytra")),
                    ),
                    (NbtString::text("count"), Tag::Int(1)),
                ])),
            ),
        ]));
        let hanging = hanging_spawn(&frame_nbt, Mirror::None, Rotation::None).expect("hanging payload");
        assert_eq!(hanging.facing, 3, "south keeps its own 3D data value");
        assert_eq!(
            hanging.item,
            Some(("minecraft:elytra".to_string(), 1)),
            "the framed stack survives"
        );
        assert_eq!(
            hanging_spawn(&frame_nbt, Mirror::None, Rotation::Clockwise90)
                .expect("hanging payload")
                .facing,
            4,
            "a quarter turn clockwise turns a south-facing frame west"
        );
        assert_eq!(
            hanging_spawn(&frame_nbt, Mirror::None, Rotation::Clockwise180)
                .expect("hanging payload")
                .facing,
            2,
            "half a turn faces north"
        );
        // `Mirror.getRotation` is 180 only for a direction on the axis the mirror
        // flips: a z flip turns north/south, an x flip turns west/east.
        assert_eq!(
            hanging_spawn(&frame_nbt, Mirror::LeftRight, Rotation::None)
                .expect("hanging payload")
                .facing,
            2,
            "mirroring z turns a south-facing frame north"
        );
        assert_eq!(
            hanging_spawn(&frame_nbt, Mirror::FrontBack, Rotation::None)
                .expect("hanging payload")
                .facing,
            3,
            "mirroring x leaves a south-facing frame alone"
        );

        let empty = Tag::Compound(BTreeMap::from([
            (NbtString::text("Facing"), Tag::Byte(5)),
        ]));
        assert_eq!(
            hanging_spawn(&empty, Mirror::None, Rotation::None).map(|h| (h.facing, h.item)),
            Some((5, None)),
            "an empty frame is still a frame"
        );

        let vertical = Tag::Compound(BTreeMap::from([
            (NbtString::text("Facing"), Tag::Byte(0)),
        ]));
        assert!(
            hanging_spawn(&vertical, Mirror::None, Rotation::None).is_none(),
            "a hanging entity cannot face down"
        );
        assert!(hanging_spawn(&Tag::Compound(BTreeMap::new()), Mirror::None, Rotation::None).is_none());
    }

    /// Staging routes a hanging spawn to its own payload: the frame is neither a
    /// mob (the `Entity` payload has no facing) nor dropped (what `stage_payloads`
    /// did while the payload had nowhere to go).
    #[test]
    fn a_hanging_spawn_stages_an_item_frame_payload() {
        use crate::generation_payload::GenerationPayload;
        let mut g = crate::worldgen::Gen::load(0, crate::name_to_state).unwrap();
        let mut ctx = crate::generation_context::FC::new(&mut g);
        ctx.test_no_terrain = true;
        ctx.test_default_air = true;
        let out = PlacementOut {
            entities: vec![
                EntitySpawn {
                    pos: (4, 70, 4),
                    id: "minecraft:item_frame".to_string(),
                    hanging: Some(HangingSpawn {
                        facing: 3,
                        item: Some(("minecraft:elytra".to_string(), 1)),
                    }),
                },
                EntitySpawn {
                    pos: (5, 70, 4),
                    id: "minecraft:glow_item_frame".to_string(),
                    hanging: Some(HangingSpawn {
                        facing: 4,
                        item: None,
                    }),
                },
                EntitySpawn {
                    pos: (6, 70, 4),
                    id: "minecraft:shulker".to_string(),
                    hanging: None,
                },
            ],
            ..Default::default()
        };
        stage_payloads(&mut ctx, out);
        assert_eq!(
            ctx.payloads,
            vec![
                (
                    (4, 70, 4),
                    GenerationPayload::ItemFrame {
                        glow: false,
                        facing: 3,
                        item: Some(("minecraft:elytra".to_string(), 1)),
                    }
                ),
                (
                    (5, 70, 4),
                    GenerationPayload::ItemFrame {
                        glow: true,
                        facing: 4,
                        item: None,
                    }
                ),
                (
                    (6, 70, 4),
                    // The anchor is the block, and the living payload is its centre.
                    GenerationPayload::Entity {
                        id: "minecraft:shulker".to_string(),
                        pos: [6.5, 70.0, 4.5],
                        yaw: 0.0,
                    }
                ),
            ]
        );
    }

    /// The oracle records the obfuscated entity class name.
    fn oracle_entity_class(id: &str) -> &'static str {
        match id {
            "minecraft:zombie_villager" => "ckx",
            "minecraft:drowned" => "cjr",
            "minecraft:villager" => "cmk",
            other => panic!("no oracle class name for entity {other}"),
        }
    }

    fn entity_lines(entities: &[EntitySpawn]) -> Vec<String> {
        let mut lines: Vec<String> = entities
            .iter()
            .map(|spawn| {
                format!(
                    "{}@{},{},{}",
                    oracle_entity_class(&spawn.id),
                    spawn.pos.0,
                    spawn.pos.1,
                    spawn.pos.2
                )
            })
            .collect();
        lines.sort();
        lines
    }
    /// The 40 MB scripted-input corpus is parsed once and shared by both tests.
    fn load_cases() -> &'static J {
        static CASES: std::sync::OnceLock<J> = std::sync::OnceLock::new();
        CASES.get_or_init(|| {
            let path = concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/data/template_structure_cases_1211.json"
            );
            let bytes = std::fs::read(path).expect("template structure cases");
            jparse(&bytes).expect("cases parse")
        })
    }

    fn ext_processors(entry: &J, reg: &BReg) -> Vec<ExtProc> {
        let raw = entry
            .get("processors")
            .and_then(J::as_arr)
            .cloned()
            .unwrap_or_default();
        raw.iter()
            .map(|processor| match jstr(processor, "processor_type").as_deref() {
                Some("minecraft:block_age") => ExtProc::BlockAge {
                    mossiness: jnum(processor, "mossiness").unwrap_or(0.0) as f32,
                },
                _ => ExtProc::Native(
                    Processor::from_json(processor, reg).expect("processor"),
                ),
            })
            .collect()
    }

    fn template_blocks(entry: &J, reg: &BReg) -> Vec<BlockInfo> {
        jarr(entry, "blocks")
            .expect("blocks")
            .iter()
            .map(|block| BlockInfo {
                pos: point(block.get("pos").expect("block pos")),
                state: state_from_json(block.get("state").expect("block state"), reg),
                nbt: jstr(block, "nbt").map(|raw| {
                    uq_parse_tag(&raw)
                }),
            })
            .collect()
    }

    /// `CompoundTagParser.parseTag` for the synthetic placement cases.
    fn uq_parse_tag(raw: &str) -> Tag {
        let root = |text: &str| -> Tag {
            let mut parser = TagText {
                bytes: text.as_bytes(),
                i: 0,
            };
            parser.compound()
        };
        root(raw)
    }

    struct TagText<'a> {
        bytes: &'a [u8],
        i: usize,
    }

    impl TagText<'_> {
        fn ws(&mut self) {
            while self.i < self.bytes.len() && self.bytes[self.i].is_ascii_whitespace() {
                self.i += 1;
            }
        }
        fn eat(&mut self, byte: u8) -> bool {
            self.ws();
            if self.i < self.bytes.len() && self.bytes[self.i] == byte {
                self.i += 1;
                true
            } else {
                false
            }
        }
        fn string(&mut self) -> String {
            self.ws();
            let quote = self.bytes[self.i];
            self.i += 1;
            let mut out = String::new();
            while self.i < self.bytes.len() && self.bytes[self.i] != quote {
                if self.bytes[self.i] == b'\\' {
                    self.i += 1;
                }
                out.push(self.bytes[self.i] as char);
                self.i += 1;
            }
            self.i += 1;
            out
        }
        fn number(&mut self) -> i64 {
            self.ws();
            let start = self.i;
            while self.i < self.bytes.len()
                && (self.bytes[self.i] == b'-' || self.bytes[self.i].is_ascii_digit())
            {
                self.i += 1;
            }
            std::str::from_utf8(&self.bytes[start..self.i])
                .unwrap()
                .parse()
                .unwrap()
        }
        fn compound(&mut self) -> Tag {
            assert!(self.eat(b'{'));
            let mut map = BTreeMap::new();
            loop {
                self.ws();
                if self.eat(b'}') {
                    break;
                }
                let key = if self.bytes[self.i] == b'"' {
                    self.string()
                } else {
                    let start = self.i;
                    while self.bytes[self.i] != b':' {
                        self.i += 1;
                    }
                    std::str::from_utf8(&self.bytes[start..self.i])
                        .unwrap()
                        .to_string()
                };
                self.ws();
                assert!(self.eat(b':'));
                let value = self.value();
                map.insert(NbtString::text(&key), value);
                self.eat(b',');
            }
            Tag::Compound(map)
        }
        fn value(&mut self) -> Tag {
            self.ws();
            match self.bytes[self.i] {
                b'{' => self.compound(),
                b'[' => {
                    self.i += 1;
                    let mut items = Vec::new();
                    loop {
                        self.ws();
                        if self.eat(b']') {
                            break;
                        }
                        items.push(self.value());
                        self.eat(b',');
                    }
                    Tag::List(10, items)
                }
                b'"' => Tag::String(NbtString::text(&self.string())),
                b'1' | b'0' if self.i + 4 < self.bytes.len()
                    && &self.bytes[self.i..self.i + 4] == b"true" =>
                {
                    self.i += 4;
                    Tag::Byte(1)
                }
                b'f' if self.i + 5 < self.bytes.len()
                    && &self.bytes[self.i..self.i + 5] == b"false" =>
                {
                    self.i += 5;
                    Tag::Byte(0)
                }
                _ => Tag::Int(self.number() as i32),
            }
        }
    }

    fn run_block_age(case: &J, reg: &BReg) -> (usize, String, i32) {
        let mirror = Mirror::from_index(jnum(case, "mirror").unwrap_or(0.0) as u8).unwrap();
        let rotation = Rotation::from_index(jnum(case, "rotation").unwrap_or(0.0) as u8).unwrap();
        let pivot = point(case.get("pivot").expect("pivot"));
        let origin = point(case.get("origin").expect("origin"));
        let seed = jnum(case, "seed").unwrap_or(0.0) as i64;
        let shared_mode = jstr(case, "random").as_deref() == Some("shared");
        let template = Template {
            size: (1, 1, 1),
            palettes: vec![template_blocks(case, reg)],
            entities: Vec::new(),
        };
        let settings = Settings {
            mirror,
            rotation,
            pivot,
            clip: None,
            apply_waterlogging: false,
            ignore_entities: true,
            processors: ext_processors(case, reg),
        };
        let scene = SceneSpec::parse(None, reg);
        let mut world = TestWorld::new(scene);
        let mut shared_rng = Xor::seed_worldgen(seed as u64);
        let processed = {
            let mut shared = if shared_mode {
                Some(&mut shared_rng as &mut Xor)
            } else {
                None
            };
            process_block_infos(
                &mut world,
                &template,
                0,
                origin,
                origin,
                &settings,
                &mut shared,
                jnum(case, "seed").unwrap_or(0.0) as i64 as u64,
                reg,
            )
            .unwrap()
        };
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&1i32.to_be_bytes());
        bytes.extend_from_slice(&(processed.len() as i32).to_be_bytes());
        for info in &processed {
            write_block_info(&mut bytes, info, reg).unwrap();
        }
        let probe = shared_rng.next_bound(1000003);
        (processed.len(), sha(&bytes), probe)
    }

    fn structure_template_id(path: &str) -> String {
        let id = path
            .strip_prefix("datapacks/vanilla/data/minecraft/structure/")
            .expect("structure template prefix")
            .strip_suffix(".nbt")
            .expect("structure template suffix");
        format!("minecraft:{id}")
    }

    fn run_place_extended(
        case: &J,
        reg: &BReg,
        templates: &Templates,
    ) -> (bool, HashMap<Pos, u32>, Vec<BeEvent>, Vec<EntitySpawn>, i32) {
        let template = match jstr(case, "template") {
            Some(path) => {
                let id = structure_template_id(&path);
                (*templates.get(&id, reg).expect("template")).clone()
            }
            None => Template {
                size: (8, 8, 8),
                palettes: vec![template_blocks(case, reg)],
                entities: Vec::new(),
            },
        };
        let mirror = Mirror::from_index(jnum(case, "mirror").unwrap_or(0.0) as u8).unwrap();
        let rotation = Rotation::from_index(jnum(case, "rotation").unwrap_or(0.0) as u8).unwrap();
        let pivot = point(case.get("pivot").expect("pivot"));
        let origin = point(case.get("origin").expect("origin"));
        let mut settings = Settings::new(mirror, rotation, pivot);
        settings.clip = case.get("clip").map(box_of);
        settings.apply_waterlogging = !matches!(case.get("ignore_waterlogging"), Some(J::Num(n)) if *n != 0.0);
        settings.ignore_entities = matches!(case.get("ignore_entities"), Some(J::Num(n)) if *n != 0.0);
        settings.processors = ext_processors(case, reg);
        let scene = SceneSpec::parse(case.get("terrain"), reg);
        let mut world = TestWorld::new(scene);
        let mut rng = Xor::seed_worldgen(jnum(case, "seed").unwrap_or(0.0) as i64 as u64);
        let world_seed = jnum(case, "world_seed").unwrap_or(0.0) as i64 as u64;
        let out = place_in_world(
            &mut world,
            &template,
            origin,
            origin,
            &settings,
            &mut rng,
            world_seed,
            reg,
        )
        .unwrap();
        let probe = rng.next_bound(1000003);
        (out.wrote, world.writes, out.be_events, out.entities, probe)
    }

    fn row_failure(what: &str, got: impl std::fmt::Display, want: impl std::fmt::Display) {
        eprintln!("FAIL {what}: got={got} want={want}");
    }

    /// The native `StructurePiece` list itself, not only the writes it produces:
    /// per-piece bounding box, rotation index and mirror index, measured with the
    /// pinned oracle (`ejv.f()` / `ekb.a()` / `enp.c()` before `postProcess`).
    /// Indices are the native enum ordinals (`Rotation.values()` and
    /// `Mirror.values()`), which `Mirror::from_index` also keys off.
    ///
    /// The fixture hashes already pin these indirectly (a wrong box clips
    /// differently, a wrong rotation rotates states), but a structural mismatch
    /// there reads as a hash difference; here it names the piece and the field.
    #[test]
    fn template_structure_piece_geometry_matches_1211_native() {
        type NativePieceGeometry = (i32, i32, i32, i32, i32, i32, u8, u8);
        const NATIVE: &[(&str, usize, &[NativePieceGeometry])] = &[
            ("igloo-seed1-c0-0", 1, &[(1, 90, 2, 8, 94, 8, 1, 0)]),
            (
                "igloo-seed9-chunk-clip",
                8,
                &[
                    (0, 66, 4, 6, 71, 12, 2, 0),
                    (2, 87, 4, 4, 89, 6, 2, 0),
                    (2, 84, 4, 4, 86, 6, 2, 0),
                    (2, 81, 4, 4, 83, 6, 2, 0),
                    (2, 78, 4, 4, 80, 6, 2, 0),
                    (2, 75, 4, 4, 77, 6, 2, 0),
                    (2, 72, 4, 4, 74, 6, 2, 0),
                    (0, 90, 3, 6, 94, 10, 2, 0),
                ],
            ),
            ("nether-fossil-valid-1", 1, &[(10, 34, 4, 10, 40, 6, 1, 0)]),
            ("nether-fossil-valid-2", 1, &[(21, 34, 16, 23, 37, 17, 2, 0)]),
            ("shipwreck-ocean-1", 1, &[(0, 90, 7, 8, 98, 30, 2, 0)]),
            ("shipwreck-beached-1", 1, &[(0, 90, 3, 8, 110, 30, 2, 0)]),
            (
                "ocean-ruin-cold-2",
                15,
                &[
                    (-15, 90, -15, 0, 105, 0, 2, 0),
                    (-15, 90, -15, 0, 105, 0, 2, 0),
                    (-15, 90, -15, 0, 105, 0, 2, 0),
                    (-23, 90, -1, -17, 96, 4, 3, 0),
                    (-23, 90, -1, -17, 96, 4, 3, 0),
                    (-23, 90, -1, -17, 96, 4, 3, 0),
                    (-24, 90, -31, -18, 96, -26, 3, 0),
                    (-24, 90, -31, -18, 96, -26, 3, 0),
                    (-24, 90, -31, -18, 96, -26, 3, 0),
                    (0, 90, 4, 6, 96, 9, 1, 0),
                    (0, 90, 4, 6, 96, 9, 1, 0),
                    (0, 90, 4, 6, 96, 9, 1, 0),
                    (7, 90, -19, 13, 96, -14, 3, 0),
                    (7, 90, -19, 13, 96, -14, 3, 0),
                    (7, 90, -19, 13, 96, -14, 3, 0),
                ],
            ),
            ("ruined_portal-warm-s0", 1, &[(-7, 67, 0, 0, 75, 8, 0, 2)]),
            ("ruined_portal_nether-warm-s1", 1, &[(1, 46, 11, 10, 53, 22, 3, 2)]),
        ];
        let reg = breg().expect("block registry");
        let cases = load_cases();
        let structure_cases = jarr(cases, "structures").expect("structures");
        let templates = Templates::new(|id| {
            crate::worldgen::Gen::dp_resource("structure", id, "nbt")
        });
        for (id, count, pieces) in NATIVE {
            let case = structure_cases
                .iter()
                .find(|case| jstr(case, "id").as_deref() == Some(id))
                .unwrap_or_else(|| panic!("missing case {id}"));
            let (_, _, _, built) = generate(case, reg, &templates);
            let (_, got) = built.unwrap_or_else(|| panic!("{id}: no generation point"));
            assert_eq!(got.len(), *count, "{id}: piece count");
            for (index, want) in pieces.iter().enumerate() {
                let piece = &got[index];
                assert_eq!(
                    &(
                        piece.bb[0],
                        piece.bb[1],
                        piece.bb[2],
                        piece.bb[3],
                        piece.bb[4],
                        piece.bb[5],
                        piece.rotation as u8,
                        piece.mirror as u8
                    ),
                    want,
                    "{id}: piece {index} geometry"
                );
            }
        }
    }

    #[test]
    fn template_structures_match_1211_native_fixture() {
        let reg = breg().expect("block registry");
        let cases = load_cases();
        let block_age_cases = jarr(cases, "block_age").expect("block_age");
        let place_cases = jarr(cases, "place_extended").expect("place_extended");
        let structure_cases = jarr(cases, "structures").expect("structures");
        let tsv = include_str!("../tests/data/template_structures_1211.tsv");
        let templates = Templates::new(|id| {
                crate::worldgen::Gen::dp_resource("structure", id, "nbt")
            });
        let mut rows = 0usize;
        let mut failures = 0usize;
        let mut kinds: HashMap<&str, usize> = HashMap::new();
        for line in tsv.lines() {
            if line.trim().is_empty() {
                continue;
            }
            let f: Vec<&str> = line.split('\t').collect();
            let kind = f[0];
            let id = f[1];
            *kinds.entry(kind).or_default() += 1;
            rows += 1;
            match kind {
                "block_age" => {
                    let case = block_age_cases
                        .iter()
                        .find(|case| jstr(case, "id").as_deref() == Some(id))
                        .expect("block_age case");
                    let (count, hash, probe) = run_block_age(case, reg);
                    let want_count: usize = f[2].parse().unwrap();
                    let want_hash = f[3];
                    let want_probe: i32 = f[4].parse().unwrap();
                    let mut ok = true;
                    if count != want_count {
                        row_failure(&format!("block_age/{id} count"), count, want_count);
                        ok = false;
                    }
                    if hash != want_hash {
                        row_failure(&format!("block_age/{id} hash"), &hash, want_hash);
                        ok = false;
                    }
                    if probe != want_probe {
                        row_failure(&format!("block_age/{id} probe"), probe, want_probe);
                        ok = false;
                    }
                    if !ok {
                        failures += 1;
                    }
                }
                "place_extended" => {
                    let case = place_cases
                        .iter()
                        .find(|case| jstr(case, "id").as_deref() == Some(id))
                        .expect("place_extended case");
                    let (wrote, writes, events, entities, probe) =
                        run_place_extended(case, reg, &templates);
                    let want_wrote: usize = f[2].parse().unwrap();
                    let want_hash = f[3];
                    let want_be_count: usize = f[4].parse().unwrap();
                    let want_be_hash = f[5];
                    let want_ent_count: usize = f[6].parse().unwrap();
                    let want_ent_hash = f[7];
                    let _extra1: i64 = f[8].parse().unwrap();
                    let want_probe: i32 = f[9].parse().unwrap();
                    let (_count, hash) = world_hash(reg, &writes);
                    let be = be_lines(&events);
                    let be_hash = hash_lines(&be);
                    let ent = entity_lines(&entities);
                    let ent_hash = hash_lines(&ent);
                    let mut ok = true;
                    if usize::from(wrote) != want_wrote {
                        row_failure(&format!("{id} wrote"), usize::from(wrote), want_wrote);
                        ok = false;
                    }
                    if hash != want_hash {
                        row_failure(&format!("{id} writes"), &hash, want_hash);
                        ok = false;
                    }
                    if be.len() != want_be_count {
                        row_failure(&format!("{id} be count"), be.len(), want_be_count);
                        ok = false;
                    }
                    if be_hash != want_be_hash {
                        row_failure(&format!("{id} be hash"), &be_hash, want_be_hash);
                        eprintln!("  be lines: {be:?}");
                        ok = false;
                    }
                    if ent.len() != want_ent_count {
                        row_failure(&format!("{id} entity count"), ent.len(), want_ent_count);
                        ok = false;
                    }
                    if ent_hash != want_ent_hash {
                        row_failure(&format!("{id} entity hash"), &ent_hash, want_ent_hash);
                        ok = false;
                    }
                    if probe != want_probe {
                        row_failure(&format!("{id} probe"), probe, want_probe);
                        ok = false;
                    }
                    if !ok {
                        failures += 1;
                    }
                }
                "structure" => {
                    let case = structure_cases
                        .iter()
                        .find(|case| jstr(case, "id").as_deref() == Some(id))
                        .expect("structure case");
                    if let Some(message) = run_structure_row(case, &f, reg, &templates) {
                        eprintln!("FAIL structure/{id}: {message}");
                        failures += 1;
                    }
                }
                other => panic!("unknown fixture kind {other}"),
            }
        }
        eprintln!("template structures: {rows} rows, {kinds:?}");
        assert_eq!(rows, 71);
        assert_eq!(failures, 0, "{failures} fixture row(s) failed");
    }

    fn structure_json(id: &str) -> J {
        let bytes = crate::worldgen::Gen::dp_resource("worldgen/structure", id, "json")
            .expect("structure json");
        jparse(&bytes).expect("structure json parse")
    }

    /// `findGenerationPoint` + the harness's `rngState` probe + the lazy consumer,
    /// in the order the native oracle runs them.
    fn generate(
        case: &J,
        reg: &BReg,
        templates: &Templates,
    ) -> (TestWorld, Lcg, i64, Option<(Pos, Vec<Piece>)>) {
        let path = jstr(case, "structure").expect("structure path");
        let struct_id = path
            .strip_prefix("datapacks/vanilla/data/minecraft/worldgen/structure/")
            .expect("structure path prefix")
            .strip_suffix(".json")
            .expect("structure path suffix");
        let kind = parse_structure(&structure_json(&format!("minecraft:{struct_id}")))
            .expect("structure definition");
        let scene = SceneSpec::parse(case.get("terrain"), reg);
        let mut world = TestWorld::new(scene);
        let world_seed = jnum(case, "world_seed").unwrap_or(0.0) as i64;
        let cx = jint(case, "cx", 0);
        let cz = jint(case, "cz", 0);
        let mut gen = Lcg::for_large_feature(world_seed as u64, cx, cz);
        let cold_biome = matches!(case.get("cold_biome"), Some(J::Num(n)) if *n != 0.0);
        let found = {
            let mut ctx = FindCtx {
                cx,
                cz,
                cold_biome,
                rng: &mut gen,
            };
            find_generation_point(&mut world, templates, &kind, &mut ctx, reg)
                .expect("generation point")
        };
        // The oracle records `rngState(random)` = one `nextLong` *between*
        // `findGenerationPoint` and the lazy consumer, so pieces are built from
        // the stream after that draw.
        let gen_probe = gen.next_i64();
        let built = found.map(|(stub, build)| {
            let pieces = build(&mut world, templates, &mut gen, reg).expect("pieces");
            (stub, pieces)
        });
        (world, gen, gen_probe, built)
    }

    fn run_structure_row(
        case: &J,
        row: &[&str],
        reg: &BReg,
        templates: &Templates,
    ) -> Option<String> {
        let want_pieces: usize = row[2].parse().unwrap();
        let want_state_hash = row[3];
        let want_be_count: usize = row[4].parse().unwrap();
        let want_be_hash = row[5];
        let want_ent_count: usize = row[6].parse().unwrap();
        let want_ent_hash = row[7];
        let want_gen_probe: i64 = row[8].parse().unwrap();
        let want_post_probe: i32 = row[9].parse().unwrap();

        let (mut world, _gen, gen_probe, built) = generate(case, reg, templates);
        let world_seed = jnum(case, "world_seed").unwrap_or(0.0) as i64 as u64;
        if gen_probe != want_gen_probe {
            return Some(format!("gen rng probe got={gen_probe} want={want_gen_probe}"));
        }
        let Some((stub, mut pieces)) = built else {
            if want_pieces != 0 {
                return Some(format!("no generation point, want {want_pieces} pieces"));
            }
            if want_post_probe != 0 {
                return Some(format!("empty generation, want post probe {want_post_probe}"));
            }
            return None;
        };
        if pieces.len() != want_pieces {
            return Some(format!("piece count got={} want={want_pieces}", pieces.len()));
        }
        let mut chunk_bb = box_of(case.get("chunk_bb").expect("chunk_bb"));
        let post_seed = jnum(case, "post_seed").unwrap_or(0.0) as i64;
        let mut post = Xor::seed_worldgen(post_seed as u64);
        let out = place_structure(
            &mut world,
            templates,
            &mut pieces,
            &mut post,
            world_seed,
            &mut chunk_bb,
            stub,
            reg,
        )
        .expect("structure placement");
        let post_probe = post.next_bound(1000003);
        let (_count, state_hash) = world_hash(reg, &world.writes);
        let be = be_lines(&out.be_events);
        let be_hash = hash_lines(&be);
        let ent = entity_lines(&out.entities);
        let ent_hash = hash_lines(&ent);
        if state_hash != want_state_hash {
            return Some(format!("state hash got={state_hash} want={want_state_hash}"));
        }
        if be.len() != want_be_count {
            return Some(format!("be count got={} want={want_be_count}", be.len()));
        }
        if be_hash != want_be_hash {
            return Some(format!("be hash got={be_hash} want={want_be_hash}"));
        }
        if ent.len() != want_ent_count {
            return Some(format!("entity count got={} want={want_ent_count}", ent.len()));
        }
        if ent_hash != want_ent_hash {
            return Some(format!("entity hash got={ent_hash} want={want_ent_hash}"));
        }
        if post_probe != want_post_probe {
            return Some(format!("post probe got={post_probe} want={want_post_probe}"));
        }
        None
    }

    /// P2-1-T0: `place_in_world` marks transformed `SHAPE_CHECK_BLOCKS` cells on `FC` so
    /// promotion rewrites them when a later carver/lake removes their neighbour.
    #[test]
    fn place_in_world_marks_transformed_shape_check_block_on_fc() {
        let reg = breg().expect("block registry");
        let mut g = crate::worldgen::Gen::load(0, crate::name_to_state).expect("gen");
        let mut ctx = crate::generation_context::FC::new(&mut g);
        ctx.test_default_air = true;

        // With `Rotation::Clockwise90` around pivot `(0, 0, 0)`, local `(1, 0, 0)` transforms to
        // `(0, 0, 1)`, so origin `(4, 70, 4)` places the fence at `(4, 70, 5)`.
        let transformed_pos = (4, 70, 5);
        ctx.set(5, 70, 5, reg.state_of("minecraft:stone", None));

        let tpl = crate::structure_template::Template {
            size: (2, 1, 1),
            palettes: vec![vec![crate::structure_template::BlockInfo {
                pos: (1, 0, 0),
                state: reg.state_of("minecraft:oak_fence", None),
                nbt: None,
            }]],
            entities: Vec::new(),
        };
        let settings = Settings::new(Mirror::None, Rotation::Clockwise90, (0, 0, 0));
        let mut rng = Xor::seed_worldgen(13);
        place_in_world(&mut ctx, &tpl, (4, 70, 4), (0, 0, 0), &settings, &mut rng, 0, reg)
            .expect("place_in_world succeeds");

        assert!(
            ctx.shape_marks.contains(&transformed_pos),
            "transformed oak_fence must file a shape mark at {transformed_pos:?}, got {:?}",
            ctx.shape_marks
        );
        assert_eq!(
            reg.get_prop(ctx.get(4, 70, 5), "east").as_deref(),
            Some("true")
        );

        ctx.set(5, 70, 5, reg.state_of("minecraft:air", None));
        let marks = std::mem::take(&mut ctx.shape_marks);
        postprocess_shape_marks(&mut ctx, reg, &marks);
        assert_eq!(
            reg.get_prop(ctx.get(4, 70, 5), "east").as_deref(),
            Some("false"),
            "promotion must disconnect the marked fence after its neighbour is removed"
        );
    }

    /// `BushBlock.updateShape` (`dgh.a`) and `DoublePlantBlock.updateShape` (`dig.a`) run during
    /// `LevelChunk.postProcessGeneration` (`postprocess_shape_marks`), NOT shared `update_shape`
    /// (`apply_shape_passes`), removing plants above non-`#minecraft:dirt`/non-`farmland` blocks
    /// (e.g. `fern` above `dirt_path` at `(82,69,-500)`).
    #[test]
    fn postprocess_shape_marks_removes_unsupported_bush_blocks_only_at_promotion() {
        let reg = breg().expect("block registry");
        let mut g = crate::worldgen::Gen::load(0, crate::name_to_state).expect("gen");
        let mut ctx = crate::generation_context::FC::new(&mut g);
        ctx.test_default_air = true;

        let fern = reg.state_of("minecraft:fern", None);
        let short_grass = reg.state_of("minecraft:short_grass", None);
        let dandelion = reg.state_of("minecraft:dandelion", None);
        let sapling = reg.state_of("minecraft:spruce_sapling", None);
        let tall_lower = reg.with_prop(reg.state_of("minecraft:tall_grass", None), "half", "lower");
        let tall_upper = reg.with_prop(reg.state_of("minecraft:tall_grass", None), "half", "upper");
        let double_ok_lower = reg.with_prop(reg.state_of("minecraft:large_fern", None), "half", "lower");
        let double_ok_upper = reg.with_prop(reg.state_of("minecraft:large_fern", None), "half", "upper");

        // Unsupported on `dirt_path` and `stone`, supported on `grass_block` and `farmland`.
        ctx.set(0, 69, 0, reg.state_of("minecraft:dirt_path", None));
        ctx.set(0, 70, 0, fern);

        ctx.set(1, 69, 0, reg.state_of("minecraft:stone", None));
        ctx.set(1, 70, 0, short_grass);

        ctx.set(2, 69, 0, reg.state_of("minecraft:grass_block", None));
        ctx.set(2, 70, 0, fern);

        ctx.set(3, 69, 0, reg.state_of("minecraft:farmland", None));
        ctx.set(3, 70, 0, dandelion);

        ctx.set(4, 69, 0, reg.state_of("minecraft:stone", None));
        ctx.set(4, 70, 0, sapling);

        ctx.set(5, 69, 0, reg.state_of("minecraft:stone", None));
        ctx.set(5, 70, 0, tall_lower);
        ctx.set(5, 71, 0, tall_upper);

        ctx.set(6, 69, 0, reg.state_of("minecraft:dirt", None));
        ctx.set(6, 70, 0, double_ok_lower);
        ctx.set(6, 71, 0, double_ok_upper);

        // Shared `apply_shape_passes` must NOT remove plants during structure placement.
        apply_shape_passes(&[(0, 70, 0), (1, 70, 0)], &mut ctx, reg);
        assert_eq!(ctx.get(0, 70, 0), fern);
        assert_eq!(ctx.get(1, 70, 0), short_grass);

        let marks = vec![
            (0, 70, 0),
            (1, 70, 0),
            (2, 70, 0),
            (3, 70, 0),
            (4, 70, 0),
            (5, 70, 0),
            (5, 71, 0),
            (6, 70, 0),
            (6, 71, 0),
        ];
        postprocess_shape_marks(&mut ctx, reg, &marks);

        let air = reg.state_of("minecraft:air", None);
        assert_eq!(ctx.get(0, 70, 0), air, "fern over dirt_path must be removed");
        assert_eq!(ctx.get(1, 70, 0), air, "short_grass over stone must be removed");
        assert_eq!(ctx.get(2, 70, 0), fern, "fern over grass_block must remain");
        assert_eq!(ctx.get(3, 70, 0), dandelion, "dandelion over farmland must remain");
        assert_eq!(ctx.get(4, 70, 0), air, "sapling over stone must be removed");
        assert_eq!(ctx.get(5, 70, 0), air, "tall_grass lower over stone must be removed");
        assert_eq!(ctx.get(5, 71, 0), air, "tall_grass upper without lower must be removed");
        assert_eq!(ctx.get(6, 70, 0), double_ok_lower, "large_fern lower over dirt must remain");
        assert_eq!(ctx.get(6, 71, 0), double_ok_upper, "large_fern upper over lower must remain");
    }

    // ------------------------------------------------------------ canSurvive matrix

    /// One `updateShape` call: `state` sits at `pos` and the neighbour on
    /// `direction` just became `neighbor`. `cells` are the rest of the world
    /// (substrates below, the block above, partner halves) - the caller places
    /// the neighbour cell itself only when the same block must survive the
    /// `updateShape` read AND stay on the map.
    fn shape_case(
        reg: &BReg,
        state: u32,
        pos: (i32, i32, i32),
        direction: u8,
        neighbor: u32,
        cells: &[((i32, i32, i32), &str)],
    ) -> u32 {
        let mut world = TestWorld::new(SceneSpec::parse(None, reg));
        world.set(pos.0, pos.1, pos.2, state);
        for &(cpos, name) in cells {
            let st = reg.state_of(name, None);
            world.set(cpos.0, cpos.1, cpos.2, st);
        }
        update_shape(reg, state, direction, neighbor, &mut world, pos)
    }

    /// The DOWN-change shorthand used by most `canSurviveOn` rules.
    fn below_case(reg: &BReg, name: &str, below: &str) -> (u32, u32) {
        let air = reg.state_of("minecraft:air", None);
        let st = reg.state_of(name, None);
        let below_st = reg.state_of(below, None);
        (
            shape_case(reg, st, (0, 100, 0), 0, below_st, &[]),
            air,
        )
    }

    #[test]
    fn update_shape_torches_ask_the_attached_face_sturdiness() {
        let reg = breg().unwrap();
        let air = reg.state_of("minecraft:air", None);
        // Floor torches ask CENTER on UP (`TorchBlock.canSurvive` /
        // `canSupportCenter`): a fence post supports them, a bottom slab does
        // not, and air below drops them.
        for name in ["minecraft:torch", "minecraft:soul_torch", "minecraft:redstone_torch"] {
            for (below, keeps) in [
                ("minecraft:stone", true),
                ("minecraft:oak_fence", true),
                ("minecraft:glass", true),
                ("minecraft:smooth_stone_slab[type=bottom]", false),
                ("minecraft:air", false),
            ] {
                let st = reg.state_of(name, None);
                let below_st = reg
                    .checked_state(below, &[])
                    .unwrap_or_else(|| reg.state_of(below.split('[').next().unwrap(), None));
                let out = shape_case(reg, st, (0, 100, 0), 0, below_st, &[]);
                assert_eq!(
                    out == st,
                    keeps,
                    "{name} on {below}: survival answer differs"
                );
                assert!(out == st || out == air);
            }
        }
        // Wall torches ask FULL sturdiness on the block behind (`facing`'s
        // opposite): solid render is not enough (glass is sturdy but not
        // solid), and a fence's bare post is not a full face.
        let wall = reg.with_prop(
            reg.state_of("minecraft:wall_torch", None),
            "facing",
            "north",
        );
        for (behind, keeps) in [
            ("minecraft:stone", true),
            ("minecraft:glass", true),
            ("minecraft:oak_fence", false),
            ("minecraft:oak_leaves", false),
            ("minecraft:air", false),
        ] {
            let behind_st = reg.state_of(behind, None);
            // `facing=north` means attached to the block at z-1, which is
            // direction 2 (NORTH) from the torch.
            let out = shape_case(reg, wall, (0, 100, 0), 2, behind_st, &[]);
            assert_eq!(
                out == wall,
                keeps,
                "wall_torch attached to {behind}: survival answer differs"
            );
            assert!(out == wall || out == air);
            // A change anywhere else (e.g. the block in front) never drops it.
            let front = shape_case(reg, wall, (0, 100, 0), 3, air, &[]);
            assert_eq!(front, wall, "wall_torch dropped by a front-side change");
        }
        // Lanterns: standing asks CENTER UP, hanging asks CENTER DOWN.
        let standing = reg.state_of("minecraft:lantern", None);
        let hanging = reg.with_prop(standing, "hanging", "true");
        let fence = reg.state_of("minecraft:oak_fence", None);
        let slab = reg.with_prop(
            reg.state_of("minecraft:smooth_stone_slab", None),
            "type",
            "bottom",
        );
        assert_eq!(
            shape_case(reg, standing, (0, 100, 0), 0, fence, &[]),
            standing,
            "standing lantern rides a fence post"
        );
        assert_eq!(
            shape_case(reg, standing, (0, 100, 0), 0, slab, &[]),
            air,
            "standing lantern drops off a bottom slab"
        );
        assert_eq!(
            shape_case(reg, hanging, (0, 100, 0), 1, fence, &[]),
            hanging,
            "hanging lantern rides under a fence post"
        );
        assert_eq!(
            shape_case(reg, hanging, (0, 100, 0), 1, air, &[]),
            air,
            "hanging lantern drops under air"
        );
    }

    #[test]
    fn update_shape_face_attached_blocks_need_a_full_face() {
        let reg = breg().unwrap();
        let air = reg.state_of("minecraft:air", None);
        let pos = (0, 100, 0);
        for name in [
            "minecraft:stone_button",
            "minecraft:lever",
            "minecraft:grindstone",
        ] {
            // face=floor reads the block below (dir DOWN -> neighbour dir 0).
            let floor = reg.with_prop(reg.state_of(name, None), "face", "floor");
            let floor = reg.with_prop(floor, "facing", "north");
            for (below, keeps) in [
                ("minecraft:stone", true),
                ("minecraft:smooth_stone_slab[type=top]", true),
                ("minecraft:oak_fence", false),
                ("minecraft:air", false),
            ] {
                let st = reg.state_of(below.split('[').next().unwrap(), None);
                let st = if below.contains("type=top") {
                    reg.with_prop(st, "type", "top")
                } else {
                    st
                };
                let out = shape_case(reg, floor, pos, 0, st, &[]);
                assert_eq!(out == floor, keeps, "{name} floor on {below}");
                assert!(out == floor || out == air);
            }
            // face=wall attached to the north block; face=ceiling reads UP.
            let wallf = reg.with_prop(reg.state_of(name, None), "face", "wall");
            let wallf = reg.with_prop(wallf, "facing", "north");
            let glass = reg.state_of("minecraft:glass", None);
            let leaves = reg.state_of("minecraft:oak_leaves", None);
            assert_eq!(
                shape_case(reg, wallf, pos, 2, glass, &[]),
                wallf,
                "{name} on glass wall (sturdy full face)"
            );
            assert_eq!(
                shape_case(reg, wallf, pos, 2, leaves, &[]),
                air,
                "{name} drops off leaves (no full face)"
            );
            let ceil = reg.with_prop(reg.state_of(name, None), "face", "ceiling");
            let ceil = reg.with_prop(ceil, "facing", "north");
            assert_eq!(
                shape_case(reg, ceil, pos, 1, glass, &[]),
                ceil,
                "{name} hangs under a sturdy face"
            );
            assert_eq!(shape_case(reg, ceil, pos, 1, air, &[]), air);
        }
    }

    #[test]
    fn update_shape_ladders_need_solid_render_behind() {
        let reg = breg().unwrap();
        let air = reg.state_of("minecraft:air", None);
        let ladder = reg.with_prop(reg.state_of("minecraft:ladder", None), "facing", "east");
        // facing=east attaches to the block EAST (dir 5).
        for (behind, keeps) in [
            ("minecraft:stone", true),
            ("minecraft:glowstone", true),
            ("minecraft:glass", false),
            ("minecraft:oak_leaves", false),
            ("minecraft:air", false),
        ] {
            let st = reg.state_of(behind, None);
            let out = shape_case(reg, ladder, (0, 100, 0), 5, st, &[]);
            assert_eq!(out == ladder, keeps, "ladder against {behind}");
            assert!(out == ladder || out == air);
        }
    }

    #[test]
    fn update_shape_redstone_support_planes() {
        let reg = breg().unwrap();
        let air = reg.state_of("minecraft:air", None);
        // Redstone dust: FULL sturdy UP or a hopper. Glass is FULL-sturdy in
        // vanilla even though it is not a redstone conductor.
        for (below, keeps) in [
            ("minecraft:stone", true),
            ("minecraft:hopper", true),
            ("minecraft:glass", true),
            ("minecraft:oak_leaves", false),
            ("minecraft:oak_fence", false),
            ("minecraft:air", false),
        ] {
            let (out, _) = below_case(reg, "minecraft:redstone_wire", below);
            let st = reg.state_of("minecraft:redstone_wire", None);
            assert_eq!(out == st, keeps, "redstone_wire on {below}");
            assert!(out == st || out == air);
        }
        // Repeaters/comparators: the RIGID plane, without the hopper special
        // case. Fences are not rigid; stone, glass and hoppers' UP face are.
        for name in ["minecraft:repeater", "minecraft:comparator"] {
            for (below, keeps) in [
                ("minecraft:stone", true),
                ("minecraft:glass", true),
                ("minecraft:hopper", true),
                ("minecraft:oak_leaves", false),
                ("minecraft:oak_fence", false),
                ("minecraft:air", false),
            ] {
                let (out, _) = below_case(reg, name, below);
                let st = reg.state_of(name, None);
                assert_eq!(out == st, keeps, "{name} on {below}");
                assert!(out == st || out == air);
            }
        }
        // Pressure plates accept RIGID or CENTER, so a fence post is enough.
        for (below, keeps) in [
            ("minecraft:stone", true),
            ("minecraft:oak_fence", true),
            ("minecraft:hopper", true),
            ("minecraft:oak_leaves", false),
            ("minecraft:air", false),
        ] {
            let (out, _) = below_case(reg, "minecraft:stone_pressure_plate", below);
            let st = reg.state_of("minecraft:stone_pressure_plate", None);
            assert_eq!(out == st, keeps, "stone_pressure_plate on {below}");
            assert!(out == st || out == air);
        }
        let st = reg.state_of("minecraft:redstone_wire", None);
        for d in [1u8, 2, 3, 4, 5] {
            assert_eq!(
                shape_case(reg, st, (0, 100, 0), d, air, &[]),
                st,
                "redstone dust dropped by a non-DOWN neighbour change"
            );
        }
    }

    #[test]
    fn update_shape_carpet_and_snow_layer() {
        let reg = breg().unwrap();
        let air = reg.state_of("minecraft:air", None);
        // Carpet: `!canSurvive` on any changed face; canSurvive = below not air.
        for name in ["minecraft:white_carpet", "minecraft:moss_carpet"] {
            let st = reg.state_of(name, None);
            for (below, keeps) in [
                ("minecraft:stone", true),
                ("minecraft:water", true),
                ("minecraft:air", false),
            ] {
                let (out, _) = below_case(reg, name, below);
                assert_eq!(out == st, keeps, "{name} over {below}");
                assert!(out == st || out == air);
            }
            // A horizontal change drops it too when nothing is below.
            assert_eq!(shape_case(reg, st, (0, 100, 0), 2, air, &[]), air);
        }
        // Snow layers: `canSurvive` = `#snow_layer_can_survive_on` below OR a
        // FULL sturdy UP face. The tag is honey_block/soul_sand/mud only.
        for (below, keeps) in [
            ("minecraft:stone", true),
            ("minecraft:honey_block", true),
            ("minecraft:soul_sand", true),
            ("minecraft:mud", true),
            ("minecraft:oak_fence", false),
            ("minecraft:oak_leaves", false),
            ("minecraft:air", false),
        ] {
            let (out, _) = below_case(reg, "minecraft:snow", below);
            let st = reg.state_of("minecraft:snow", None);
            assert_eq!(out == st, keeps, "snow over {below}");
            assert!(out == st || out == air);
        }
    }

    #[test]
    fn update_shape_fire_bases() {
        let reg = breg().unwrap();
        let air = reg.state_of("minecraft:air", None);
        // Regular fire needs a FULL sturdy UP face below.
        for (below, keeps) in [
            ("minecraft:stone", true),
            ("minecraft:magma_block", true),
            ("minecraft:oak_fence", false),
            ("minecraft:air", false),
        ] {
            let (out, _) = below_case(reg, "minecraft:fire", below);
            let st = reg.state_of("minecraft:fire", None);
            assert_eq!(out == st, keeps, "fire over {below}");
            assert!(out == st || out == air);
        }
        // Soul fire rides `#soul_fire_base_blocks` (soul_sand + soul_soil),
        // not sturdiness.
        for (below, keeps) in [
            ("minecraft:soul_sand", true),
            ("minecraft:soul_soil", true),
            ("minecraft:stone", false),
            ("minecraft:netherrack", false),
        ] {
            let (out, _) = below_case(reg, "minecraft:soul_fire", below);
            let st = reg.state_of("minecraft:soul_fire", None);
            assert_eq!(out == st, keeps, "soul_fire over {below}");
        }
    }

    #[test]
    fn update_shape_surface_blocks_use_their_native_support_predicates() {
        let reg = breg().unwrap();
        let air = reg.state_of("minecraft:air", None);
        // Cake/candle cake use legacy `isSolid`; bottom slabs pass the legacy
        // dimensional heuristic even though their UP face is not FULL-sturdy.
        for name in ["minecraft:cake", "minecraft:candle_cake"] {
            for (below, keeps, top) in [
                ("minecraft:smooth_stone_slab", true, false),
                ("minecraft:oak_leaves", true, false),
                ("minecraft:smooth_stone_slab", true, true),
                ("minecraft:air", false, false),
            ] {
                let mut st = reg.state_of(below, None);
                st = reg.with_prop(st, "type", if top { "top" } else { "bottom" });
                let placed = reg.state_of(name, None);
                let out = shape_case(reg, placed, (0, 100, 0), 0, st, &[]);
                assert_eq!(out == placed, keeps, "{name} on {below} top={top}");
                assert!(out == placed || out == air);
            }
        }
        // Candles use CENTER; sea pickles accept a non-empty collision face as
        // well as a FULL face. Both therefore sit on fence posts.
        for name in ["minecraft:candle", "minecraft:sea_pickle"] {
            let fence = reg.state_of("minecraft:oak_fence", None);
            let placed = reg.state_of(name, None);
            assert_eq!(shape_case(reg, placed, (0, 100, 0), 0, fence, &[]), placed, "{name} on fence");
            assert_eq!(shape_case(reg, placed, (0, 100, 0), 0, air, &[]), air, "{name} in air");
        }
        // These four do not override canSurvive in 1.21.1 and survive in air.
        for name in [
            "minecraft:flower_pot",
            "minecraft:potted_oak_sapling",
            "minecraft:end_rod",
            "minecraft:turtle_egg",
            "minecraft:conduit",
        ] {
            let placed = reg.state_of(name, None);
            assert_eq!(shape_case(reg, placed, (0, 100, 0), 0, air, &[]), placed, "{name} survives in air");
        }
    }

    #[test]
    fn update_shape_directional_attachments() {
        let reg = breg().unwrap();
        let air = reg.state_of("minecraft:air", None);
        let pos = (0, 100, 0);
        // End rods do not override canSurvive; they remain even in air.
        let rod = reg.with_prop(reg.state_of("minecraft:end_rod", None), "facing", "up");
        assert_eq!(shape_case(reg, rod, pos, 0, air, &[]), rod);
        // Amethyst buds/clusters attach to the FULL sturdy face on facing's
        // opposite side - glass passes, leaves fail.
        let bud = reg.with_prop(
            reg.state_of("minecraft:amethyst_cluster", None),
            "facing",
            "up",
        );
        for (below, keeps) in [
            ("minecraft:stone", true),
            ("minecraft:glowstone", true),
            ("minecraft:glass", true),
            ("minecraft:oak_leaves", false),
            ("minecraft:air", false),
        ] {
            let st = reg.state_of(below, None);
            let out = shape_case(reg, bud, pos, 0, st, &[]);
            assert_eq!(out == bud, keeps, "amethyst_cluster on {below}");
            assert!(out == bud || out == air);
        }
        // Cocoa hangs on `#jungle_logs` only.
        let cocoa = reg.with_prop(reg.state_of("minecraft:cocoa", None), "facing", "south");
        for (log, keeps) in [
            ("minecraft:jungle_log", true),
            ("minecraft:stripped_jungle_log", true),
            ("minecraft:jungle_wood", true),
            ("minecraft:oak_log", false),
            ("minecraft:stone", false),
        ] {
            let st = reg.state_of(log, None);
            let out = shape_case(reg, cocoa, pos, 3, st, &[]);
            assert_eq!(out == cocoa, keeps, "cocoa on {log}");
            assert!(out == cocoa || out == air);
        }
        // Pointed dripstone: a stalactite (tip down) rides under a sturdy
        // ceiling or under another down-tipped dripstone.
        let tip = reg.with_prop(
            reg.state_of("minecraft:pointed_dripstone", None),
            "tip_direction",
            "down",
        );
        let stone = reg.state_of("minecraft:stone", None);
        let leaves = reg.state_of("minecraft:oak_leaves", None);
        assert_eq!(shape_case(reg, tip, pos, 1, stone, &[]), tip);
        assert_eq!(shape_case(reg, tip, pos, 1, leaves, &[]), air);
        // Chained dripstone: same block above with the same tip keeps it.
        let world_cells = &[];
        let above = shape_case(reg, tip, pos, 1, tip, &world_cells[..]);
        assert_eq!(above, tip, "chained pointed_dripstone holds");
        // Tripwire hook needs a redstone conductor on facing's opposite side.
        let hook = reg.with_prop(
            reg.state_of("minecraft:tripwire_hook", None),
            "facing",
            "south",
        );
        for (behind, keeps) in [
            ("minecraft:stone", true),
            ("minecraft:glass", false),
            ("minecraft:oak_leaves", false),
            ("minecraft:air", false),
        ] {
            let st = reg.state_of(behind, None);
            let out = shape_case(reg, hook, pos, 3, st, &[]);
            assert_eq!(out == hook, keeps, "tripwire_hook on {behind}");
            assert!(out == hook || out == air);
        }
    }

    #[test]
    fn update_shape_vines_sugar_cane_cactus_bamboo() {
        let reg = breg().unwrap();
        let air = reg.state_of("minecraft:air", None);
        let pos = (0, 100, 0);
        // Sugar cane: stays while the block below is cane or a substrate; the
        // water check lives in the bush pass, but the diagonal-arm regression
        // is that DOWN changes no longer fall through to "keep everything".
        let cane = reg.state_of("minecraft:sugar_cane", None);
        let sand = reg.state_of("minecraft:sand", None);
        let water = reg.state_of("minecraft:water", None);
        let stone = reg.state_of("minecraft:stone", None);
        // Cane over cane over sand next to water survives both changes.
        let cells = &[
            ((0, 98, 0), "minecraft:sand"),
            ((1, 98, 0), "minecraft:water"),
        ];
        let out = shape_case(reg, cane, pos, 0, cane, &cells[..]);
        assert_eq!(out, cane, "cane over cane stays");
        // Cane directly over sand without adjacent water dies on its own
        // `canSurvive` (placement validity is out of scope here; the update
        // answer is).
        let out = shape_case(reg, cane, pos, 0, sand, &[]);
        assert_eq!(out, air, "cane over dry sand dies");
        let out = shape_case(reg, cane, pos, 0, stone, &[]);
        assert_eq!(out, air, "cane over stone dies");
        // Cactus on cactus keeps; cactus on sand (no neighbours) keeps on the
        // DOWN rule alone - side-contact popping is the other arm of the same
        // vanilla check and `canSurvive` also verifies nothing solid touches.
        let cactus = reg.state_of("minecraft:cactus", None);
        assert_eq!(shape_case(reg, cactus, pos, 0, cactus, &[]), cactus);
        assert_eq!(
            shape_case(reg, cactus, pos, 0, sand, &[]),
            cactus,
            "cactus over open sand stays"
        );
        assert_eq!(shape_case(reg, cactus, pos, 0, stone, &[]), air);
        // Cactus pops when a solid block appears beside it.
        let out = shape_case(reg, cactus, pos, 2, stone, &[]);
        assert_eq!(out, air, "cactus next to a new solid block pops");
        // Bamboo follows the same below-is-bamboo-or-substrate rule.
        let bamboo = reg.state_of("minecraft:bamboo", None);
        assert_eq!(shape_case(reg, bamboo, pos, 0, bamboo, &[]), bamboo);
        assert_eq!(shape_case(reg, bamboo, pos, 0, sand, &[]), bamboo);
        assert_eq!(shape_case(reg, bamboo, pos, 0, water, &[]), air);
        assert_eq!(shape_case(reg, bamboo, pos, 0, stone, &[]), air);
        // Chorus plant: end_stone below, chorus below, or a same-height
        // side-chain back to end_stone.
        let chorus = reg.state_of("minecraft:chorus_plant", None);
        let end_stone = reg.state_of("minecraft:end_stone", None);
        assert_eq!(shape_case(reg, chorus, pos, 0, end_stone, &[]), chorus);
        assert_eq!(shape_case(reg, chorus, pos, 0, chorus, &[]), chorus);
        assert_eq!(shape_case(reg, chorus, pos, 0, stone, &[]), air);
        // Kelp/water plants: water above + supported-or-kelp below.
        let kelp = reg.state_of("minecraft:kelp", None);
        let cells = &[((0, 101, 0), "minecraft:water")];
        let out = shape_case(reg, kelp, pos, 0, stone, &cells[..]);
        assert_eq!(out, kelp, "kelp over a full block under water");
        let out = shape_case(reg, kelp, pos, 0, air, &cells[..]);
        assert_eq!(out, air, "kelp over air dies");
    }

    #[test]
    fn update_shape_bed_bell_scaffolding_doors() {
        let reg = breg().unwrap();
        let air = reg.state_of("minecraft:air", None);
        let pos = (0, 100, 0);
        // Bed foot keeps only while its head half occupies the facing side.
        let foot = reg.with_prop(reg.state_of("minecraft:red_bed", None), "part", "foot");
        let foot = reg.with_prop(foot, "facing", "north");
        let head = reg.with_prop(reg.state_of("minecraft:red_bed", None), "part", "head");
        let head = reg.with_prop(head, "facing", "north");
        assert_eq!(shape_case(reg, foot, pos, 2, head, &[]), foot);
        assert_eq!(shape_case(reg, foot, pos, 2, air, &[]), air);
        // Bell attachments ride the sturdy rule per `attachment` value.
        let floor_bell = reg.with_prop(reg.state_of("minecraft:bell", None), "attachment", "floor");
        let ceil_bell = reg.with_prop(reg.state_of("minecraft:bell", None), "attachment", "ceiling");
        let wall_bell = reg.with_prop(reg.state_of("minecraft:bell", None), "attachment", "single_wall");
        let wall_bell = reg.with_prop(wall_bell, "facing", "north");
        let dwall_bell = reg.with_prop(reg.state_of("minecraft:bell", None), "attachment", "double_wall");
        let dwall_bell = reg.with_prop(dwall_bell, "facing", "north");
        let stone = reg.state_of("minecraft:stone", None);
        assert_eq!(shape_case(reg, floor_bell, pos, 0, stone, &[]), floor_bell);
        assert_eq!(shape_case(reg, floor_bell, pos, 0, air, &[]), air);
        assert_eq!(shape_case(reg, ceil_bell, pos, 1, stone, &[]), ceil_bell);
        assert_eq!(shape_case(reg, ceil_bell, pos, 1, air, &[]), air);
        assert_eq!(shape_case(reg, wall_bell, pos, 2, stone, &[]), wall_bell);
        assert_eq!(shape_case(reg, wall_bell, pos, 2, air, &[]), air);
        // double_wall reads the two side faces: with facing=north those are
        // west(4)/east(5) - losing one drops it, losing a front/back does not.
        assert_eq!(shape_case(reg, dwall_bell, pos, 4, air, &[]), air);
        // Scaffolding's shape callback does not rewrite `bottom`; it returns
        // the state unchanged and schedules the distance recompute tick.
        let scaf = reg.state_of("minecraft:scaffolding", None);
        assert_eq!(shape_case(reg, scaf, pos, 0, stone, &[]), scaf);
        assert_eq!(shape_case(reg, scaf, pos, 0, scaf, &[]), scaf);
        // Door lower half: below must offer a FULL sturdy UP face; a change to
        // a non-sturdy floor drops the lower half.
        let door = reg.with_prop(reg.state_of("minecraft:oak_door", None), "half", "lower");
        let door = reg.with_prop(door, "facing", "north");
        let fence = reg.state_of("minecraft:oak_fence", None);
        assert_eq!(shape_case(reg, door, pos, 0, stone, &[]), door);
        assert_eq!(shape_case(reg, door, pos, 0, fence, &[]), air);
    }

    #[test]
    fn update_shape_signs_banners_multiface_and_chains() {
        let reg = breg().unwrap();
        let air = reg.state_of("minecraft:air", None);
        let pos = (0, 100, 0);
        let stone = reg.state_of("minecraft:stone", None);
        // Standing signs use the legacy `isSolid` predicate; wall signs do
        // the same for their backing block.
        let sign = reg.state_of("minecraft:oak_sign", None);
        let fence = reg.state_of("minecraft:oak_fence", None);
        assert_eq!(shape_case(reg, sign, pos, 0, fence, &[]), sign);
        assert_eq!(shape_case(reg, sign, pos, 0, air, &[]), air);
        let wall_sign = reg.with_prop(reg.state_of("minecraft:oak_wall_sign", None), "facing", "west");
        assert_eq!(shape_case(reg, wall_sign, pos, 5, stone, &[]), wall_sign);
        assert_eq!(shape_case(reg, wall_sign, pos, 5, air, &[]), air);
        // Wall banners use legacy `isSolid` backing, so glass qualifies.
        let banner = reg.with_prop(reg.state_of("minecraft:white_wall_banner", None), "facing", "south");
        let glass = reg.state_of("minecraft:glass", None);
        assert_eq!(shape_case(reg, banner, pos, 3, stone, &[]), banner);
        assert_eq!(shape_case(reg, banner, pos, 3, glass, &[]), banner);
        // Standing banners use the same legacy solid predicate.
        let floor_banner = reg.state_of("minecraft:white_banner", None);
        assert_eq!(shape_case(reg, floor_banner, pos, 0, fence, &[]), floor_banner);
        assert_eq!(shape_case(reg, floor_banner, pos, 0, air, &[]), air);
        // Hanging signs: ceiling variants need a sturdy DOWN face above; wall
        // variants attach like wall signs. A chain rides a sturdy face above,
        // another chain, or a lantern-like hanging parent; below alone is air.
        let chain = reg.state_of("minecraft:chain", None);
        assert_eq!(shape_case(reg, chain, pos, 1, stone, &[]), chain);
        assert_eq!(shape_case(reg, chain, pos, 1, chain, &[]), chain);
        assert_eq!(shape_case(reg, chain, pos, 1, air, &[]), air);
        // A horizontal chain (axis=x) hangs from east/west neighbours.
        let hchain = reg.with_prop(chain, "axis", "x");
        assert_eq!(shape_case(reg, hchain, pos, 5, chain, &[]), hchain);
        assert_eq!(shape_case(reg, hchain, pos, 5, air, &[]), air);
        // Multiface blocks (glow lichen, sculk vein, moss, resin) stay while
        // any claimed face has sturdy backing; they vanish when the last one
        // goes. A lichen claiming only `down` reads the block above.
        let lichen = reg.with_prop(reg.state_of("minecraft:glow_lichen", None), "down", "true");
        assert_eq!(shape_case(reg, lichen, pos, 1, stone, &[]), lichen);
        assert_eq!(shape_case(reg, lichen, pos, 1, air, &[]), air);
        // Tripwire itself updates its attachment props; only hooks have a
        // support question. A wire with no hooks keeps a state.
        let wire = reg.state_of("minecraft:tripwire", None);
        let out = shape_case(reg, wire, pos, 2, air, &[]);
        assert_eq!(reg.name_of(out), "minecraft:tripwire");
    }

    #[test]
    fn postprocess_bush_shape_substrate_matrix() {
        let reg = breg().unwrap();
        let pos = (0, 100, 0);
        let mut world = TestWorld::new(SceneSpec::parse(None, reg));
        // `postprocess_bush_shape` answers Some(air) exactly when the vanilla
        // `canSurvive` substrate rule fails for the neighbour-aware arms added
        // here; the bush/farm arm is covered by
        // `postprocess_shape_marks_removes_unsupported_bush_blocks_only_at_promotion`.
        let cases: &[(&str, &str, bool)] = &[
            ("minecraft:nether_wart", "minecraft:soul_sand", true),
            ("minecraft:nether_wart", "minecraft:stone", false),
            ("minecraft:wheat", "minecraft:farmland", true),
            ("minecraft:pumpkin_stem", "minecraft:farmland", true),
            ("minecraft:wheat", "minecraft:dirt", false),
            ("minecraft:dead_bush", "minecraft:sand", true),
            ("minecraft:dead_bush", "minecraft:red_terracotta", true),
            ("minecraft:dead_bush", "minecraft:dirt", true),
            ("minecraft:dead_bush", "minecraft:grass_block", false),
            ("minecraft:dead_bush", "minecraft:farmland", false),
            ("minecraft:wither_rose", "minecraft:netherrack", true),
            ("minecraft:wither_rose", "minecraft:soul_sand", true),
            ("minecraft:wither_rose", "minecraft:stone", false),
            ("minecraft:crimson_roots", "minecraft:crimson_nylium", true),
            ("minecraft:nether_sprouts", "minecraft:soul_soil", true),
            ("minecraft:crimson_fungus", "minecraft:mycelium", true),
            ("minecraft:azalea", "minecraft:clay", true),
            ("minecraft:azalea", "minecraft:stone", false),
            ("minecraft:big_dripleaf", "minecraft:clay", true),
            ("minecraft:big_dripleaf_stem", "minecraft:clay", true),
            ("minecraft:mangrove_propagule", "minecraft:mud", true),
            ("minecraft:mangrove_propagule", "minecraft:stone", false),
        ];
        for &(name, below, keeps) in cases {
            let st = reg.state_of(name, None);
            let below_st = reg.state_of(below, None);
            world.set(pos.0, pos.1, pos.2, st);
            world.set(pos.0, pos.1 - 1, pos.2, below_st);
            let out = postprocess_bush_shape(reg, st, &mut world, pos);
            assert_eq!(
                out.is_none(),
                keeps,
                "{name} over {below}: postprocess answer differs"
            );
        }
        // Hanging roots need a FULL sturdy DOWN face above.
        let roots = reg.state_of("minecraft:hanging_roots", None);
        world.set(pos.0, pos.1, pos.2, roots);
        world.set(pos.0, pos.1 + 1, pos.2, reg.state_of("minecraft:stone", None));
        assert!(postprocess_bush_shape(reg, roots, &mut world, pos).is_none());
        world.set(pos.0, pos.1 + 1, pos.2, reg.state_of("minecraft:oak_fence", None));
        assert!(postprocess_bush_shape(reg, roots, &mut world, pos).is_some());
        // A hanging propagule ignores the floor and reads the block above.
        let prop = reg.with_prop(
            reg.state_of("minecraft:mangrove_propagule", None),
            "hanging",
            "true",
        );
        world.set(pos.0, pos.1, pos.2, prop);
        world.set(pos.0, pos.1 - 1, pos.2, reg.state_of("minecraft:air", None));
        world.set(
            pos.0,
            pos.1 + 1,
            pos.2,
            reg.state_of("minecraft:mangrove_leaves", None),
        );
        assert!(postprocess_bush_shape(reg, prop, &mut world, pos).is_none());
        world.set(pos.0, pos.1 + 1, pos.2, reg.state_of("minecraft:air", None));
        assert!(postprocess_bush_shape(reg, prop, &mut world, pos).is_some());
        // Small dripleaf lower half: `#small_dripleaf_placeable` (clay, and
        // moss under water) or `#dirt` while submerged.
        let small = reg.state_of("minecraft:small_dripleaf", None);
        world.set(pos.0, pos.1, pos.2, small);
        world.set(pos.0, pos.1 - 1, pos.2, reg.state_of("minecraft:clay", None));
        assert!(postprocess_bush_shape(reg, small, &mut world, pos).is_none());
        world.set(pos.0, pos.1 - 1, pos.2, reg.state_of("minecraft:dirt", None));
        assert!(
            postprocess_bush_shape(reg, small, &mut world, pos).is_some(),
            "small_dripleaf over dry dirt dies"
        );
        world.set(pos.0, pos.1 + 1, pos.2, reg.state_of("minecraft:water", None));
        assert!(
            postprocess_bush_shape(reg, small, &mut world, pos).is_none(),
            "small_dripleaf over dirt under water stays"
        );
        world.set(pos.0, pos.1 + 1, pos.2, reg.state_of("minecraft:air", None));
        // Upper half reads the lower half.
        let upper = reg.with_prop(small, "half", "upper");
        let lower = reg.with_prop(small, "half", "lower");
        world.set(pos.0, pos.1, pos.2, upper);
        world.set(pos.0, pos.1 - 1, pos.2, lower);
        assert!(postprocess_bush_shape(reg, upper, &mut world, pos).is_none());
        world.set(pos.0, pos.1 - 1, pos.2, reg.state_of("minecraft:stone", None));
        assert!(postprocess_bush_shape(reg, upper, &mut world, pos).is_some());
    }
}

// This small suite is intentionally outside the vanilla-data-only structure
// suite above: the support predicates are registry facts, not generated-test
// data, and must be pinned in the ordinary `cargo test --bin megu` build too.
#[cfg(test)]
mod support_shape_matrix_tests {
    use super::*;
    use crate::block_registry::breg;
    use std::collections::HashMap;

    struct MiniWorld {
        cells: HashMap<Pos, u32>,
    }
    impl MiniWorld {
        fn new() -> Self {
            Self { cells: HashMap::new() }
        }
    }
    impl TemplateWorld for MiniWorld {
        fn get(&mut self, x: i32, y: i32, z: i32) -> u32 {
            *self.cells.get(&(x, y, z)).unwrap_or(&0)
        }
        fn set(&mut self, x: i32, y: i32, z: i32, state: u32) -> bool {
            self.cells.insert((x, y, z), state);
            true
        }
        fn heightmap(&mut self, _kind: HeightmapKind, _x: i32, _z: i32) -> i32 { 0 }
        fn base_height(&mut self, _kind: HeightmapKind, _x: i32, _z: i32) -> i32 { 0 }
        fn sea_level(&self) -> i32 { 63 }
        fn min_y(&self) -> i32 { -64 }
        fn height(&self) -> i32 { 384 }
    }

    fn state(reg: &BReg, spec: &str) -> u32 {
        let (name, prop) = spec.split_once('[').unwrap_or((spec, ""));
        let mut st = reg.state_of(name, None);
        if !prop.is_empty() {
            for kv in prop.trim_end_matches(']').split(',') {
                let (k, v) = kv.split_once('=').unwrap();
                st = reg.with_prop(st, k, v);
            }
        }
        st
    }

    fn below_case(reg: &BReg, placed_spec: &str, below_spec: &str) -> u32 {
        let mut world = MiniWorld::new();
        let placed = state(reg, placed_spec);
        let below = state(reg, below_spec);
        update_shape(reg, placed, 0, below, &mut world, (0, 64, 0))
    }

    fn side_case(reg: &BReg, placed_spec: &str, direction: u8, neighbor_spec: &str) -> u32 {
        let mut world = MiniWorld::new();
        update_shape(reg, state(reg, placed_spec), direction, state(reg, neighbor_spec), &mut world, (0, 64, 0))
    }

    #[test]
    fn support_shape_torch_and_wall_torch_planes() {
        let Some(reg) = breg() else { return };
        let air = state(reg, "minecraft:air");
        for below in ["minecraft:stone", "minecraft:glass", "minecraft:oak_fence"] {
            assert_eq!(below_case(reg, "minecraft:torch", below), state(reg, "minecraft:torch"), "torch on {below}");
        }
        for below in ["minecraft:oak_leaves", "minecraft:air"] {
            assert_eq!(below_case(reg, "minecraft:torch", below), air, "torch on {below}");
        }
        // facing=north attaches to the south neighbour (direction 3) on the
        // FULL plane: glass is sturdy, a fence post is not.
        let wall = "minecraft:wall_torch[facing=north]";
        assert_eq!(side_case(reg, wall, 3, "minecraft:glass"), state(reg, wall));
        assert_eq!(side_case(reg, wall, 3, "minecraft:oak_fence"), air);
        assert_eq!(side_case(reg, wall, 3, "minecraft:air"), air);
    }

    #[test]
    fn support_shape_redstone_uses_three_distinct_planes() {
        let Some(reg) = breg() else { return };
        let air = state(reg, "minecraft:air");
        // Dust: FULL or hopper. Glass is full-sturdy even though it is not a
        // redstone conductor.
        assert_eq!(below_case(reg, "minecraft:redstone_wire", "minecraft:glass"), state(reg, "minecraft:redstone_wire"));
        assert_eq!(below_case(reg, "minecraft:redstone_wire", "minecraft:oak_fence"), air);
        // Gates: RIGID. Dust's fence failure also applies here.
        for gate in ["minecraft:repeater", "minecraft:comparator"] {
            assert_eq!(below_case(reg, gate, "minecraft:stone"), state(reg, gate));
            assert_eq!(below_case(reg, gate, "minecraft:oak_fence"), air);
        }
        // Pressure plates: RIGID or CENTER, so a fence post supports them.
        assert_eq!(below_case(reg, "minecraft:stone_pressure_plate", "minecraft:oak_fence"), state(reg, "minecraft:stone_pressure_plate"));
    }

    #[test]
    fn support_shape_signs_and_banners_use_legacy_solid() {
        let Some(reg) = breg() else { return };
        let air = state(reg, "minecraft:air");
        // Leaves fail FULL face-sturdiness but pass legacy `isSolid`; vanilla
        // signs/banners therefore keep them as support.
        for name in ["minecraft:oak_sign", "minecraft:white_banner"] {
            assert_eq!(below_case(reg, name, "minecraft:oak_leaves"), state(reg, name), "{name} over leaves");
            assert_eq!(below_case(reg, name, "minecraft:air"), air, "{name} in air");
        }
        let wall_sign = "minecraft:oak_wall_sign[facing=west]";
        assert_eq!(side_case(reg, wall_sign, 5, "minecraft:glass"), state(reg, wall_sign));
        assert_eq!(side_case(reg, wall_sign, 5, "minecraft:air"), air);
        // Ceiling hanging signs always ask CENTER; wall hanging signs have no
        // canSurvive override in 1.21.1 and survive even unsupported.
        let hang = "minecraft:oak_hanging_sign[attached=false,rotation=0]";
        assert_eq!(side_case(reg, hang, 1, "minecraft:oak_fence"), state(reg, hang));
        assert_eq!(side_case(reg, hang, 1, "minecraft:air"), air);
        let wall_hang = "minecraft:oak_wall_hanging_sign[facing=north]";
        assert_eq!(side_case(reg, wall_hang, 3, "minecraft:air"), state(reg, wall_hang));
    }

    #[test]
    fn support_shape_surface_family_matrix() {
        let Some(reg) = breg() else { return };
        // Cake uses legacy solid, so even leaves support it; candles and sea
        // pickles sit on fence posts by their center/collision rules.
        assert_eq!(below_case(reg, "minecraft:cake", "minecraft:oak_leaves"), state(reg, "minecraft:cake"));
        assert_eq!(below_case(reg, "minecraft:candle", "minecraft:oak_fence"), state(reg, "minecraft:candle"));
        assert_eq!(below_case(reg, "minecraft:sea_pickle", "minecraft:oak_fence"), state(reg, "minecraft:sea_pickle"));
        for floating in ["minecraft:flower_pot", "minecraft:end_rod", "minecraft:turtle_egg", "minecraft:conduit"] {
            assert_eq!(below_case(reg, floating, "minecraft:air"), state(reg, floating), "{floating} survives unsupported");
        }
    }

    #[test]
    fn support_shape_water_and_column_families() {
        let Some(reg) = breg() else { return };
        let air = state(reg, "minecraft:air");
        // Seagrass needs a full top face and explicitly rejects magma.
        assert_eq!(below_case(reg, "minecraft:seagrass", "minecraft:stone"), state(reg, "minecraft:seagrass"));
        assert_eq!(below_case(reg, "minecraft:seagrass", "minecraft:magma_block"), air);
        // Coral floor fans need a full top face; wall fans attach opposite
        // their facing.
        assert_eq!(below_case(reg, "minecraft:tube_coral_fan", "minecraft:stone"), state(reg, "minecraft:tube_coral_fan"));
        assert_eq!(below_case(reg, "minecraft:tube_coral_fan", "minecraft:oak_fence"), air);
        let wallfan = "minecraft:tube_coral_wall_fan[facing=north]";
        assert_eq!(side_case(reg, wallfan, 3, "minecraft:stone"), state(reg, wallfan));
        assert_eq!(side_case(reg, wallfan, 3, "minecraft:air"), air);
        // Vertical growing plants attach opposite their growth direction.
        let twisting = state(reg, "minecraft:twisting_vines");
        assert_eq!(below_case(reg, "minecraft:twisting_vines", "minecraft:stone"), twisting);
        assert_eq!(below_case(reg, "minecraft:twisting_vines", "minecraft:air"), air);
        let weeping = state(reg, "minecraft:weeping_vines");
        assert_eq!(side_case(reg, "minecraft:weeping_vines", 1, "minecraft:stone"), weeping);
        assert_eq!(side_case(reg, "minecraft:weeping_vines", 1, "minecraft:air"), air);
    }
}
