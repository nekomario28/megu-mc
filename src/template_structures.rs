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

use crate::block_registry::{breg, HeightmapKind};
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
    fn min_y(&self) -> i32;
    fn height(&self) -> i32;
    /// `LevelAccessor.getRandom().nextInt(bound)` for non-placement level RNG
    /// (e.g. WoodlandMansion `Group of Allays` count), kept separate from the
    /// chunk decoration `RandomSource`.
    fn level_next_bound(&mut self, pos: Pos, bound: i32) -> i32 {
        Lcg::new(hash_block_pos(pos.0, pos.1, pos.2) as u64).next_bound(bound)
    }
}

impl TemplateWorld for crate::generation_context::FC<'_> {
    fn get(&mut self, x: i32, y: i32, z: i32) -> u32 {
        crate::generation_context::FC::get(self, x, y, z)
    }
    fn set(&mut self, x: i32, y: i32, z: i32, state: u32) -> bool {
        if y < self.g.min_y || y >= self.g.min_y + self.g.height {
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
    fn min_y(&self) -> i32 {
        self.g.min_y
    }
    fn height(&self) -> i32 {
        self.g.height
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
        if entity.id == "minecraft:item_frame" {
            if crate::trace_env::env_flag!("SPIPE_PAYLOAD") {
                eprintln!(
                    "SPIPE_PAYLOAD unrouted entity {} at {:?}",
                    entity.id, entity.pos
                );
            }
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
        _ if bare.ends_with("_sign") => "minecraft:sign",
        _ if bare.ends_with("_hanging_sign") => "minecraft:hanging_sign",
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
                    let mut rng = settings_rng(&mut **guard, current.pos);
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
                        let mut rng = settings_rng(&mut **guard, p);
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
    drop(world_cell);
    drop(rng_cell);
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
                let mut rng = settings_rng(&mut **guard, p);
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
        drop(world_cell);
        drop(rng_cell);
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
    if !written.is_empty() {
        for (direction, p, q) in crate::voxel_faces::exposed_faces(written.iter().copied()) {
            let mut inside = world.get(p.0, p.1, p.2);
            let outside = world.get(q.0, q.1, q.2);
            let next_inside = update_shape(reg, inside, direction, outside, world, p);
            if next_inside != inside {
                world.set(p.0, p.1, p.2, next_inside);
                record_shape_write(&mut out, reg, p, next_inside);
                inside = next_inside;
            }
            let next_outside = update_shape(reg, outside, direction ^ 1, inside, world, q);
            if next_outside != outside {
                world.set(q.0, q.1, q.2, next_outside);
                record_shape_write(&mut out, reg, q, next_outside);
            }
        }
        for p in written.clone() {
            let current = world.get(p.0, p.1, p.2);
            let updated = update_from_neighbour_shapes(reg, current, world, p);
            if updated != current {
                world.set(p.0, p.1, p.2, updated);
                record_shape_write(&mut out, reg, p, updated);
            }
        }
    }
    if !settings.ignore_entities {
        place_entities(world, template, origin, settings, &mut out);
    }
    out.wrote = true;
    Ok(out)
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
        out.entities.push(EntitySpawn { pos: block, id });
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
    // it, so a change on that side with nothing solid there drops it to air.
    // This is the one surviving attachment shape in the pinned corpus - the
    // end city's tower tops put `magenta_wall_banner`s on their outer edge, and
    // the tower's own wall is what keeps them.
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
            if facing == Some(direction ^ 1) && !reg.is_solid(neighbor) {
                return air;
            }
        }
    }
    if name.ends_with("_stairs") && matches!(direction, 2..=5) {
        if let Some(next) = reg.stair_after_neighbors(state, pos, |cell| {
            world.get(cell.0, cell.1, cell.2)
        }) {
            return next;
        }
    }
    let bare = name.strip_prefix("minecraft:").unwrap_or(name);
    // `WallBlock.updateShape`: both `UP` (direction 1) and horizontal sides
    // recompute `low`/`tall` against `above.getCollisionShape().getFaceShape(DOWN)`
    // and recompute `up` (`shouldRaisePost`).
    if bare.ends_with("_wall") || reg.in_tag(state, "minecraft:walls") {
        return wall_update_shape(reg, state, direction, neighbor, world, pos);
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
            return reg.with_prop(state, key, want);
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
            return reg.with_prop(state, key, want);
        }
    }
    // `DoorBlock.updateShape`: the vertical partner copies the partner's state
    // (keeping this half); a lower half whose `canSurvive` fails becomes air
    // (modelled as "nothing solid below").
    if bare.ends_with("_door") {
        let half = reg.get_prop(state, "half");
        let lower = half.as_deref() == Some("lower");
        let partner = (direction == 1 && lower) || (direction == 0 && !lower);
        if !partner {
            if lower && direction == 0 && (reg.is_air(neighbor) || reg.is_liquid(neighbor)) {
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
    if let Some(next) = reg.connection_after_neighbor(state, neighbor, 0, direction) {
        return next;
    }
    if reg.fragile_loses_support(state, direction, neighbor) {
        return air;
    }
    if name == "minecraft:ladder" {
        let facing = reg.get_prop(state, "facing");
        let behind = match facing.as_deref() {
            Some("north") => Some(3u8),
            Some("south") => Some(2),
            Some("west") => Some(5),
            Some("east") => Some(4),
            _ => None,
        };
        if behind == Some(direction)
            && (reg.is_air(neighbor) || reg.is_liquid(neighbor))
        {
            return air;
        }
    }
    if name.ends_with("_button") && reg.is_air(neighbor) {
        let attached = reg.get_prop(state, "face");
        let breaks = match (attached.as_deref(), direction) {
            (Some("floor"), 0) => true,
            (Some("ceiling"), 1) => true,
            (Some("wall"), 2..=5) => true,
            _ => false,
        };
        if breaks {
            return air;
        }
    }
    state
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

/// `BlockStateBase.isFaceSturdy` for the plain FULL support type: the
/// neighbour must cover that whole side (`side_flags` in the pinned asset).
fn is_face_sturdy(reg: &BReg, state: u32, direction: u8) -> bool {
    reg.side_solid(state, direction as usize)
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
pub(crate) struct Templates {
    cache: std::cell::RefCell<std::collections::HashMap<String, Rc<Template>>>,
    /// `StructureTemplateManager.getOrCreate` behind a seam: this layer may not
    /// reach for the worldgen pack reader (the module graph forbids that edge),
    /// so the caller that owns the datapack supplies the bytes.
    loader: std::cell::RefCell<Box<dyn FnMut(&str) -> Option<Vec<u8>>>>,
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
    let min_y = world.min_y();
    let depth = world.height();
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

#[cfg(test)]
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
    }

    impl TestWorld {
        fn new(scene: SceneSpec) -> Self {
            TestWorld {
                scene,
                writes: HashMap::new(),
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
        map.into_iter()
            .filter_map(|(_, record)| record)
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
        const NATIVE: &[(&str, usize, &[(i32, i32, i32, i32, i32, i32, u8, u8)])] = &[
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
        let structure_cases = jarr(&cases, "structures").expect("structures");
        let mut templates = Templates::new(|id| {
            crate::worldgen::Gen::dp_resource("structure", id, "nbt")
        });
        for (id, count, pieces) in NATIVE {
            let case = structure_cases
                .iter()
                .find(|case| jstr(case, "id").as_deref() == Some(id))
                .unwrap_or_else(|| panic!("missing case {id}"));
            let (_, _, _, built) = generate(case, reg, &mut templates);
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
        let block_age_cases = jarr(&cases, "block_age").expect("block_age");
        let place_cases = jarr(&cases, "place_extended").expect("place_extended");
        let structure_cases = jarr(&cases, "structures").expect("structures");
        let tsv = include_str!("../tests/data/template_structures_1211.tsv");
        let mut templates = Templates::new(|id| {
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
                        run_place_extended(case, reg, &mut templates);
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
                    if let Some(message) = run_structure_row(case, &f, reg, &mut templates) {
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

}
