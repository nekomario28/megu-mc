//! The block-entity payloads a template can carry.
//!
//! Its own module, below both `worldgen` and `generation_context`: those two are adjacent
//! layers and the lower one may not reach up, so a shared payload type has to live beneath
//! both. Decoding lives with the template (which owns the NBT contract); this is only the
//! data that crosses the generation boundary.
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
pub(crate) enum StagedBlockEntity {
    /// `Items`: slot, item id, count, plus the store that owns them.
    Container {
        kind: ContainerKind,
        slots: Vec<Slot>,
    },
    /// A sign side: the four lines of its front text.
    Sign([String; 4]),
}
