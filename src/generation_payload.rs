//! Generation payloads crossing the generator/runtime lock boundary.
//!
//! Its own module, below both `worldgen` and `generation_context`: those two are adjacent
//! layers and the lower one may not reach up, so a shared payload type has to live beneath
//! both. Templates decode their own items/signs; procedural writers author entity
//! and spawner data. This is only the payload crossing the generation boundary.
type Slot = (i16, i64, u8);

/// Which container store owns a placed container's contents.
///
/// The port keeps one store per menu, so a furnace's `Items` written into the chest store
/// would be invisible: the furnace window reads `furnaces()`. The kind is decided from the
/// placed block state at admission, and a container with no store is refused.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum ContainerKind {
    /// The one position-keyed block-container store (`chests()`), which the 27-slot blocks
    /// open as a window: chest, trapped chest, barrel, shulker box.
    Chest,
    /// `furnaces()`, slots 0/1/2 → input/fuel/output.
    Furnace,
    /// A block container the runtime also keeps in `chests()` but whose native menu is
    /// smaller than a chest's 27 slots: dispenser/dropper/crafter 9, hopper 5, decorated pot
    /// one slot. No window opens on these, yet the store is read by position anyway — the hopper
    /// tick draws from a hopper, `fire_dispenser` draws from a dispenser, and a broken block
    /// spills its entry — so the contents are materialized rather than dropped. The number is
    /// the native container size, which is how many slots the loot roll may fill.
    BlockContainer(u8),
}

impl ContainerKind {
    /// Which store owns a placed block entity's contents, keyed on the placed state.
    ///
    /// One owner for the three admission sites (template NBT, template-structure block-entity
    /// events, jigsaw pieces), so a state cannot be accepted by one pipeline and refused by
    /// another. `None` means no store reads this block's contents: staging them there would be
    /// dead data, so the caller refuses them instead.
    ///
    /// Measured over the pinned 1.21.1 template corpus
    /// (`datapacks/vanilla/data/minecraft/structure`, every `blocks[].nbt`): the states that
    /// carry a `LootTable` are chest (139), dispenser (4), decorated pot (4) and barrel (3),
    /// and the states that carry container `Items` are chest (98), barrel (18), furnace (28),
    /// blast furnace (11), smoker (9), trapped chest (1) and dispenser (1). No furnace carries a
    /// `LootTable`; it carries `Items`, which is why the furnace family is admitted here for its
    /// contents while still drawing no loot seed — a 1.21.1 furnace block entity is not a
    /// `RandomizableContainer`.
    pub(crate) fn for_block(name: &str) -> Option<Self> {
        Some(match name {
            "minecraft:chest" | "minecraft:trapped_chest" | "minecraft:barrel" => {
                ContainerKind::Chest
            }
            "minecraft:furnace" | "minecraft:blast_furnace" | "minecraft:smoker" => {
                ContainerKind::Furnace
            }
            "minecraft:dispenser" | "minecraft:dropper" | "minecraft:crafter" => {
                ContainerKind::BlockContainer(9)
            }
            "minecraft:hopper" => ContainerKind::BlockContainer(5),
            "minecraft:decorated_pot" => ContainerKind::BlockContainer(1),
            "minecraft:shulker_box" => ContainerKind::Chest,
            other if other.ends_with("_shulker_box") => ContainerKind::Chest,
            _ => return None,
        })
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum GenerationPayload {
    /// `Items`: slot, item id, count, plus the store that owns them.
    Container {
        kind: ContainerKind,
        slots: Vec<Slot>,
    },
    /// Container populated from a loot table (`LootTable` + optional `LootTableSeed`).
    LootContainer {
        kind: ContainerKind,
        loot_table: String,
        loot_seed: Option<i64>,
    },
    /// A sign side: the four lines of its front text.
    Sign([String; 4]),
    /// A freshly configured cave-spider spawner (its native setter consumes no RNG).
    Spawner,
    /// A freshly configured spawner for a specific entity id (e.g. `minecraft:blaze`, `minecraft:silverfish`).
    SpawnerEntity(String),
    /// A living structure-spawned entity (`id` like `minecraft:elder_guardian`, `minecraft:witch`, etc.).
    Entity {
        id: String,
        pos: [f64; 3],
        yaw: f32,
    },
    /// A hanging entity a template or an end-city data marker places
    /// (`minecraft:item_frame` / `minecraft:glow_item_frame`). It is not a mob: the
    /// runtime needs its facing and its framed stack, which the generic `Entity`
    /// payload has no field for.
    ///
    /// `facing` is the placed `Direction.get3DDataValue()`. In 1.21.1 that value is
    /// the direction's declaration ordinal (`north` 2, `south` 3, `west` 4, `east` 5)
    /// and it is the same number the `Facing` save byte carries and the
    /// `spawn_entity` object data sends (`ItemFrame.getAddEntityPacket`). `item` is
    /// the framed stack as the template's `Item` NBT names it; the end-city marker
    /// is the one place that authors it instead of a template.
    ItemFrame {
        glow: bool,
        facing: u8,
        item: Option<(String, u8)>,
    },
    /// Entity position is the supplied block anchor plus (0.5, 0.5, 0.5).
    ChestMinecart { loot_seed: i64 },
    /// Archaeology loot held by a suspicious sand/gravel block until brushing.
    Brushable {
        loot_table: String,
        loot_seed: Option<i64>,
    },
    /// A trial spawner's or vault's full block-entity NBT, carried verbatim as
    /// the serialized unnamed root compound (`nbt::write_named` bytes). These
    /// block entities are configured almost entirely by data-driven fields
    /// (`normal_config`/`ominous_config`/`spawn_potentials`, `config`/
    /// `shared_data`) that no flat payload field can enumerate, so the runtime
    /// handoff re-parses the bytes into the live block-entity store instead of
    /// dropping the configuration on the floor.
    BlockEntityNbt(Vec<u8>),
}
