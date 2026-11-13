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
    Chest,
    Furnace,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum GenerationPayload {
    /// `Items`: slot, item id, count, plus the store that owns them.
    Container {
        kind: ContainerKind,
        slots: Vec<Slot>,
    },
    /// A sign side: the four lines of its front text.
    Sign([String; 4]),
    /// A freshly configured cave-spider spawner (its native setter consumes no RNG).
    Spawner,
    /// Entity position is the supplied block anchor plus (0.5, 0.5, 0.5).
    ChestMinecart { loot_seed: i64 },
}
