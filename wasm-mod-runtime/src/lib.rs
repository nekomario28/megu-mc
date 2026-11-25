// SPDX-License-Identifier: LGPL-3.0-or-later

//! `megu-wasm-mod-runtime` — the safe Rust→WASM native mod runtime.
//!
//! Megu's v1 native mod runtime executes **sandboxed, fuel-metered
//! WebAssembly MVP modules** packaged inside `.mmod` or standard
//! Modrinth-uploadable `.jar` archives at `META-INF/megu/mod.wasm`.
//! Unlike raw `cdylib` plugins (Horizon `.hnmod`), a guest module cannot
//! crash or compromise the host:
//!
//! * **Memory isolation** — bounded linear memory pages, every access
//!   bounds-checked, zero `unsafe` in this crate.
//! * **Fuel metering** — every executed op costs exactly one unit;
//!   infinite loops deterministically exhaust the per-call budget and
//!   quarantine the mod instead of hanging the tick loop.
//! * **Capability gating** — host imports check the mod's declared
//!   [`CapabilitySet`]; undeclared calls fail closed.
//! * **Fault isolation** — traps produce a structured [`ModFault`] and
//!   quarantine the single mod; the server never sees a panic.
//!
//! # ABI v1 (`megu_v1`)
//!
//! Guest exports: `megu_abi_version() -> i32` (required, must return `1`),
//! `megu_on_load() -> i32`, `megu_on_tick(tick: i64) -> i32`,
//! `megu_on_block_interact(x, y, z, player: i32) -> i32`.
//!
//! Host imports (module `env` or `megu_v1`): `host_log(ptr, len)`,
//! `host_get_block(x, y, z) -> i32`, `host_set_block(x, y, z, state) -> i32`,
//! `host_emit_chat(player, ptr, len) -> i32`, plus the canonical-spine
//! imports `megu_section_rev(dim, cx, sy, cz) -> i64`,
//! `megu_chunk_rev(dim, cx, cz) -> i64` and
//! `megu_bump_block(dim, x, y, z) -> i64` delegating to the shared
//! `megu_core::section_rev::SharedSectionRevTable` (wire one in with
//! [`SpineRevHost`]).
//!
//! See `docs/rust-native-mods.md` for the full spec and packaging guide.

#![forbid(unsafe_code)]

mod abi;
mod archive;
mod decode;
mod error;
mod exec;
mod module;
mod validate;
mod wasm_mod;

/// Binary-encoder helpers for assembling real `.wasm`/`.zip` fixtures in
/// tests — no external toolchain needed.
#[doc(hidden)]
pub mod testutil;

pub use abi::{
    Capability, CapabilitySet, MeguHost, SpineRevHost, ABI_VERSION, EXPORT_ABI_VERSION,
    EXPORT_ON_BLOCK_INTERACT, EXPORT_ON_LOAD, EXPORT_ON_TICK, IMPORT_BUMP_BLOCK,
    IMPORT_CHUNK_REV, IMPORT_EMIT_CHAT, IMPORT_GET_BLOCK, IMPORT_LOG, IMPORT_MODULES,
    IMPORT_SECTION_REV, IMPORT_SET_BLOCK, MANIFEST_ENTRY, MOD_WASM_ENTRY, STATUS_OK,
};
pub use archive::{inspect_archive, ArchiveMod};
pub use error::{LoadError, ModFault, Trap};
pub use module::{ValType, Value};
pub use wasm_mod::{ModConfig, WasmMod};
