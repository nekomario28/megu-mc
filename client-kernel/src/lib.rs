// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
//! Protocol 767 kernel with checked Rust APIs and a versioned C facade.

#![deny(unsafe_op_in_unsafe_fn)]

#[rustfmt::skip]
#[allow(clippy::all)]
#[path = "../../src/brigadier.rs"]
mod brigadier;
mod command;
mod ffi;
mod gzip;
mod physics;
mod section;

// These projected modules retain server-only methods and lint conventions.
#[allow(dead_code, clippy::all)]
mod nbt {
    include!(concat!(env!("OUT_DIR"), "/nbt.rs"));
}

pub use command::{CommandDispatcher, CommandParse, Suggestions};
pub use physics::{
    collide, fluid_impulse, Aabb, Collision, FluidCell, FluidImpulse, FluidKind, FluidQuery,
};
pub use section::PalettedSection;

pub const ABI_VERSION: u32 = 1;
pub const PROTOCOL: u32 = 767;
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
