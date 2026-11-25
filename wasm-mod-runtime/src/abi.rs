// SPDX-License-Identifier: LGPL-3.0-or-later

//! Megu Native Mod ABI v1 (`megu_v1`).
//!
//! Guest exports and host imports are fixed by the ABI; every host import
//! is gated by a [`Capability`] the mod must declare in its
//! `megu-mod.toml` `[capabilities]` table — an undeclared call fails
//! closed with [`Trap::CapabilityDenied`].
//!
//! Import modules accepted: `env` (default wasm convention) and
//! `megu_v1` (explicit namespacing). Names and signatures must match
//! exactly; anything else is rejected at instantiation.

use std::cell::RefCell;
use std::rc::Rc;

use megu_core::coords::{ChunkKey, DimIndex, SectionKey};
use megu_core::rev::UiLayer;
use megu_core::section_rev::SharedSectionRevTable;

use crate::error::Trap;
use crate::module::ValType;

/// Current ABI version returned by `megu_abi_version`.
pub const ABI_VERSION: i32 = 1;

/// Archive entry carrying the guest module inside `.mmod`/`.jar`.
pub const MOD_WASM_ENTRY: &str = "META-INF/megu/mod.wasm";

/// Archive entry for the native manifest used to derive capabilities.
pub const MANIFEST_ENTRY: &str = "megu-mod.toml";

/// Import module names the runtime binds.
pub const IMPORT_MODULES: [&str; 2] = ["env", "megu_v1"];

// Guest export names.
pub const EXPORT_ABI_VERSION: &str = "megu_abi_version";
pub const EXPORT_ON_LOAD: &str = "megu_on_load";
pub const EXPORT_ON_TICK: &str = "megu_on_tick";
pub const EXPORT_ON_BLOCK_INTERACT: &str = "megu_on_block_interact";

// Host import names.
pub const IMPORT_LOG: &str = "host_log";
pub const IMPORT_GET_BLOCK: &str = "host_get_block";
pub const IMPORT_SET_BLOCK: &str = "host_set_block";
pub const IMPORT_EMIT_CHAT: &str = "host_emit_chat";
/// Canonical-spine read: `megu_section_rev(dim: u32, cx, sy, cz) -> u64`.
pub const IMPORT_SECTION_REV: &str = "megu_section_rev";
/// Canonical-spine read: `megu_chunk_rev(dim: u32, cx, cz) -> u64`.
pub const IMPORT_CHUNK_REV: &str = "megu_chunk_rev";
/// Canonical-spine mutation: `megu_bump_block(dim: u32, x, y, z) -> u64`,
/// returning the edited section's new revision.
pub const IMPORT_BUMP_BLOCK: &str = "megu_bump_block";
/// Retained-UI invalidation: `megu_ui_mark_dirty(layer: u32) -> i32`.
/// Only `UiLayer::ModHud` (5) and `UiLayer::ModOverlay` (6) are accepted.
pub const IMPORT_UI_MARK_DIRTY: &str = "megu_ui_mark_dirty";

/// Guest return-code convention: `>= 0` is success, `< 0` is a
/// mod-reported soft error (logged, not trapped).
pub const STATUS_OK: i32 = 0;

/// A declared mod capability from `megu-mod.toml`'s `[capabilities]`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Capability {
    /// `host_get_block`.
    ReadBlocks,
    /// `host_set_block`.
    WriteBlocks,
    /// Reserved: recipe registration API (no v1 import yet).
    RegisterRecipes,
    /// `host_emit_chat`.
    ChatBroadcast,
    /// Reserved: entity event subscriptions (no v1 import yet).
    EntityEvents,
    /// `megu_ui_mark_dirty` for mod-owned HUD/overlay surfaces.
    UiSurface,
}

impl Capability {
    /// The `megu-mod.toml` key for this capability.
    pub fn manifest_key(self) -> &'static str {
        match self {
            Capability::ReadBlocks => "read_blocks",
            Capability::WriteBlocks => "write_blocks",
            Capability::RegisterRecipes => "register_recipes",
            Capability::ChatBroadcast => "chat_broadcast",
            Capability::EntityEvents => "entity_events",
            Capability::UiSurface => "ui_surface",
        }
    }

    /// Inverse of [`manifest_key`](Self::manifest_key).
    pub fn from_manifest_key(key: &str) -> Option<Self> {
        match key {
            "read_blocks" => Some(Capability::ReadBlocks),
            "write_blocks" => Some(Capability::WriteBlocks),
            "register_recipes" => Some(Capability::RegisterRecipes),
            "chat_broadcast" => Some(Capability::ChatBroadcast),
            "entity_events" => Some(Capability::EntityEvents),
            "ui_surface" => Some(Capability::UiSurface),
            _ => None,
        }
    }

    pub const ALL: [Capability; 6] = [
        Capability::ReadBlocks,
        Capability::WriteBlocks,
        Capability::RegisterRecipes,
        Capability::ChatBroadcast,
        Capability::EntityEvents,
        Capability::UiSurface,
    ];
}

/// The set of capabilities a mod has been granted. Construction is by
/// manifest keys — unknown keys are rejected so a typo can never widen
/// authority.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CapabilitySet(u32);

impl CapabilitySet {
    pub const NONE: CapabilitySet = CapabilitySet(0);

    pub fn all() -> Self {
        let mut s = CapabilitySet(0);
        for c in Capability::ALL {
            s = s.granted(c);
        }
        s
    }

    pub fn granted(mut self, c: Capability) -> Self {
        self.0 |= 1 << c as u32;
        self
    }

    pub fn has(self, c: Capability) -> bool {
        self.0 & (1 << c as u32) != 0
    }

    /// Fail closed: returns [`Trap::CapabilityDenied`] when `c` is absent.
    pub(crate) fn require(self, c: Capability, import: &'static str) -> Result<(), Trap> {
        if self.has(c) {
            Ok(())
        } else {
            Err(Trap::CapabilityDenied {
                capability: c,
                import,
            })
        }
    }

    /// Build a set from manifest keys; unknown keys fail closed.
    pub fn from_manifest_keys<'a>(
        keys: impl IntoIterator<Item = &'a str>,
    ) -> Result<Self, String> {
        let mut s = CapabilitySet::NONE;
        for k in keys {
            match Capability::from_manifest_key(k.trim()) {
                Some(c) => s = s.granted(c),
                None => return Err(format!("unknown capability `{k}`")),
            }
        }
        Ok(s)
    }

    /// All granted capabilities, ordered by enum discriminant.
    pub fn iter(self) -> impl Iterator<Item = Capability> {
        Capability::ALL.into_iter().filter(move |c| self.has(*c))
    }
}

/// The host services a WASM mod may call into. The server implements
/// this; tests use mocks. All methods are infallible — a host that
/// cannot serve a call returns a negative status code rather than
/// trapping the guest.
pub trait MeguHost {
    /// `host_log(ptr, len)` — a mod-visible log line. Ungated: logging
    /// cannot mutate world state.
    fn log(&mut self, msg: &str);
    /// `host_get_block(x, y, z) -> i32` — block state id at the position.
    /// Gated by [`Capability::ReadBlocks`].
    fn get_block(&mut self, x: i32, y: i32, z: i32) -> i32;
    /// `host_set_block(x, y, z, state) -> i32` — write a block state.
    /// Returns 0 on success, `<0` on failure. Gated by
    /// [`Capability::WriteBlocks`].
    fn set_block(&mut self, x: i32, y: i32, z: i32, state: i32) -> i32;
    /// `host_emit_chat(player_id, ptr, len) -> i32` — send a chat line.
    /// Returns 0 on success. Gated by [`Capability::ChatBroadcast`].
    fn emit_chat(&mut self, player_id: i32, msg: &str) -> i32;

    /// `megu_section_rev` — canonical section revision behind the shared
    /// spine. Hosts wired into the engine's
    /// [`SharedSectionRevTable`] return `Some(table.section_rev(key))`;
    /// the default `None` reads `0` to the guest, keeping pre-spine mocks
    /// valid. Gated by [`Capability::ReadBlocks`].
    fn spine_section_rev(&mut self, _key: SectionKey) -> Option<u64> {
        None
    }
    /// `megu_chunk_rev` — canonical chunk-column aggregate revision.
    /// Same contract as [`spine_section_rev`](Self::spine_section_rev);
    /// gated by [`Capability::ReadBlocks`].
    fn spine_chunk_rev(&mut self, _key: ChunkKey) -> Option<u64> {
        None
    }
    /// `megu_bump_block` — record one block edit against the shared spine
    /// and return the edited section's new revision. The default `None`
    /// is a no-op returning `0`; gated by [`Capability::WriteBlocks`].
    fn spine_bump_block(&mut self, _dim: DimIndex, _x: i32, _y: i32, _z: i32) -> Option<u64> {
        None
    }

    /// `megu_ui_mark_dirty` — invalidate one mod-owned retained UI layer.
    /// Hosts return `0` on success; the default `-1` reports unsupported.
    /// Gated by [`Capability::UiSurface`].
    fn ui_mark_dirty(&mut self, _layer: UiLayer) -> i32 {
        -1
    }
}

/// `MeguHost` decorator wiring one shared canonical
/// [`SharedSectionRevTable`] behind the `megu_*_rev` imports while every
/// world-service call delegates to `inner`.
///
/// The table arrives as `Rc<RefCell<_>>` so the *same* canonical
/// revisions can be shared with a `mod-adapter-core` `ModSpineHost`, a
/// datapack lane or a test driver — a WASM mod, a NeoForge bridge and a
/// mod-offload network then provably read and bump one revision sequence
/// instead of drifting onto private tables.
pub struct SpineRevHost<H: MeguHost> {
    table: Rc<RefCell<SharedSectionRevTable>>,
    inner: H,
}

impl<H: MeguHost> SpineRevHost<H> {
    /// Wrap `inner`, serving the spine imports from `table`.
    pub fn new(inner: H, table: Rc<RefCell<SharedSectionRevTable>>) -> Self {
        Self { table, inner }
    }

    /// Cloneable handle to the wrapped shared table.
    pub fn shared_table(&self) -> Rc<RefCell<SharedSectionRevTable>> {
        Rc::clone(&self.table)
    }

    /// The wrapped host.
    pub fn inner(&self) -> &H {
        &self.inner
    }

    /// Unwrap into the inner host, dropping the spine wiring.
    pub fn into_inner(self) -> H {
        self.inner
    }
}

impl<H: MeguHost> MeguHost for SpineRevHost<H> {
    fn log(&mut self, msg: &str) {
        self.inner.log(msg);
    }
    fn get_block(&mut self, x: i32, y: i32, z: i32) -> i32 {
        self.inner.get_block(x, y, z)
    }
    fn set_block(&mut self, x: i32, y: i32, z: i32, state: i32) -> i32 {
        self.inner.set_block(x, y, z, state)
    }
    fn emit_chat(&mut self, player_id: i32, msg: &str) -> i32 {
        self.inner.emit_chat(player_id, msg)
    }
    fn spine_section_rev(&mut self, key: SectionKey) -> Option<u64> {
        Some(self.table.borrow().section_rev(key))
    }
    fn spine_chunk_rev(&mut self, key: ChunkKey) -> Option<u64> {
        Some(self.table.borrow().chunk_rev(key))
    }
    fn spine_bump_block(&mut self, dim: DimIndex, x: i32, y: i32, z: i32) -> Option<u64> {
        Some(self.table.borrow_mut().bump_block(dim, x, y, z).0)
    }
}

/// A bound host import.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum HostBinding {
    Log,
    GetBlock,
    SetBlock,
    EmitChat,
    SectionRev,
    ChunkRev,
    BumpBlock,
    UiMarkDirty,
}

impl HostBinding {
    /// Capability that gates this import; `None` for `host_log`, which
    /// is ungated (logging cannot mutate world state).
    pub(crate) fn capability(self) -> Option<Capability> {
        match self {
            HostBinding::Log => None,
            HostBinding::GetBlock => Some(Capability::ReadBlocks),
            HostBinding::SetBlock => Some(Capability::WriteBlocks),
            HostBinding::EmitChat => Some(Capability::ChatBroadcast),
            HostBinding::SectionRev | HostBinding::ChunkRev => Some(Capability::ReadBlocks),
            HostBinding::BumpBlock => Some(Capability::WriteBlocks),
            HostBinding::UiMarkDirty => Some(Capability::UiSurface),
        }
    }

    /// Resolve `(module, name)` and verify the declared signature.
    /// `I`/`V` abbreviate the param/result lists.
    pub(crate) fn resolve(
        module: &str,
        name: &str,
        params: &[ValType],
        results: &[ValType],
    ) -> Result<Self, String> {
        use ValType::*;
        if !IMPORT_MODULES.contains(&module) {
            return Err(format!("unknown import module `{module}`"));
        }
        let (binding, want_p, want_r): (HostBinding, &[ValType], &[ValType]) = match name {
            IMPORT_LOG => (HostBinding::Log, &[I32, I32], &[]),
            IMPORT_GET_BLOCK => (HostBinding::GetBlock, &[I32, I32, I32], &[I32]),
            IMPORT_SET_BLOCK => (HostBinding::SetBlock, &[I32, I32, I32, I32], &[I32]),
            IMPORT_EMIT_CHAT => (HostBinding::EmitChat, &[I32, I32, I32], &[I32]),
            IMPORT_SECTION_REV => (HostBinding::SectionRev, &[I32, I32, I32, I32], &[I64]),
            IMPORT_CHUNK_REV => (HostBinding::ChunkRev, &[I32, I32, I32], &[I64]),
            IMPORT_BUMP_BLOCK => (HostBinding::BumpBlock, &[I32, I32, I32, I32], &[I64]),
            IMPORT_UI_MARK_DIRTY => (HostBinding::UiMarkDirty, &[I32], &[I32]),
            _ => return Err(format!("unknown import `{module}.{name}`")),
        };
        if params != want_p || results != want_r {
            return Err(format!(
                "import `{module}.{name}` signature mismatch: want {want_p:?}->{want_r:?}, got {params:?}->{results:?}"
            ));
        }
        Ok(binding)
    }
}
