// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
//! Protocol 767 kernel with checked Rust APIs and a versioned C facade.

#![deny(unsafe_op_in_unsafe_fn)]

#[rustfmt::skip]
#[allow(clippy::all)]
#[path = "../../src/brigadier.rs"]
mod brigadier;
mod command;
pub mod entity_interp;
mod ffi;
mod gzip;
#[rustfmt::skip]
#[allow(dead_code, clippy::all)]
#[path = "../../src/json.rs"]
mod json;
mod light;
pub mod occlusion_table;
mod physics;
mod registry;
mod section;

// These projected modules retain server-only methods and lint conventions.
#[allow(dead_code, clippy::all)]
mod nbt {
    include!(concat!(env!("OUT_DIR"), "/nbt.rs"));
}

pub use command::{CommandDispatcher, CommandParse, Suggestions};
pub use entity_interp::{
    evaluate_sky_and_fog, lerp_angle_deg, wrap_angle_deg, EntityInterpolationTable,
    InterpolatedPose, SkyFogInput, SkyFogOutput, INTERP_UPDATE_POSITION, INTERP_UPDATE_REMOVE,
    INTERP_UPDATE_ROTATION, INTERP_UPDATE_SNAP, MAX_INTERP_ENTITIES, MAX_INTERP_STEPS,
    TELEPORT_DISTANCE_SQ,
};
pub use light::{
    ChunkLightUpdate, ChunkLightUpdateMultiWord, LightEdit, LightEngine, LightFact, LightKind,
    LightMasksMultiWord, LightUpdate, LightUpdateMultiWord, LIGHT_SECTION_BYTES,
};
pub use mc_spine_core::id::ResourceLocation;
pub use mc_spine_core::version::{TargetVersion, VersionMeta};
pub use occlusion_table::{
    BlockLightState, BlockShapeKind, ChunkLightBatch, CoalescedLightQueue, Direction, FaceMask,
    HorizontalFacing, PackedLightProps, PrebakedLightTable, QueuedLightEdit,
};
pub use physics::{
    collide, fluid_impulse, Aabb, Collision, FluidCell, FluidImpulse, FluidKind, FluidQuery,
};
// The C-ABI entry points are also callable from Rust so integration tests can
// pin the pointer/length contract against the checked API bit-for-bit.
#[doc(hidden)]
pub use ffi::{
    mc_spine_interp_entities_batch, mc_spine_interp_table_free, mc_spine_interp_table_new,
    mc_spine_sky_fog_eval, InterpEntityUpdate, InterpPoseOut, SkyFogInputC, SkyFogOutputC,
};
pub use registry::{BlockFacts, Registry};
pub use section::PalettedSection;

pub const ABI_VERSION: u32 = 1;
/// The single vanilla release this kernel implements, sourced from the
/// `mc-spine-core` target registry every crate shares.
pub const TARGET_VERSION: TargetVersion = TargetVersion::V1_21_1;
/// Numeric protocol version sent in handshakes, derived from
/// [`TARGET_VERSION`] so the constant cannot drift from the core registry.
pub const PROTOCOL: u32 = TARGET_VERSION.protocol() as u32;

/// The canonical metadata for [`TARGET_VERSION`]: release id, protocol,
/// `DataVersion`, pack formats and the Java floor, pinned in `mc-spine-core`.
pub const fn version_meta() -> &'static VersionMeta {
    TARGET_VERSION.meta()
}
pub const SECTION_CELLS: usize = 4096;
/// Number of vanilla block states in the pinned 1.21.1 registry.
pub const STATE_COUNT: u32 = 26_684;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(i32)]
pub enum Error {
    Null = 1,
    OutOfRange = 2,
    InvalidUtf8 = 3,
    InvalidState = 4,
    InvalidNbt = 5,
    NbtDepth = 6,
    ByteBudget = 7,
    InvalidGzip = 8,
    InvalidArgument = 9,
    BufferTooSmall = 10,
    QueryFailed = 11,
    Panic = 12,
    InvalidWire = 13,
    InvalidJson = 14,
    NotFound = 15,
}

fn nbt_error(error: &str) -> Error {
    match error {
        "NBT exceeds container depth" => Error::NbtDepth,
        _ => Error::InvalidNbt,
    }
}

pub fn nbt_read_named_validate(input: &[u8]) -> Result<(), Error> {
    nbt::read_named(input).map(|_| ()).map_err(nbt_error)
}

pub fn nbt_read_gzip_validate(input: &[u8], max_uncompressed: usize) -> Result<(), Error> {
    nbt_read_named_validate(&gzip::decode(input, max_uncompressed)?)
}

pub fn nbt_roundtrip_named(input: &[u8]) -> Result<Vec<u8>, Error> {
    let (name, value) = nbt::read_named(input).map_err(nbt_error)?;
    nbt::write_named(&name, &value).map_err(nbt_error)
}
