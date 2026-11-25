// SPDX-License-Identifier: LGPL-3.0-or-later

//! Guest-side contract for **Megu Native Mod ABI v1 (`megu_v1`)** — the
//! WebAssembly lane implemented by the `wasm-mod-runtime` crate.
//!
//! A Rust mod compiles to `wasm32-unknown-unknown`, links against this
//! crate, derives [`MeguMod`]-style behaviour from a struct, and stamps the
//! four exported entry points with [`megu_native_mod!`]. At load time the
//! server validates `megu_abi_version() == 1`, then meters every call with a
//! hard fuel budget and gates each `host_*` import on the capabilities the
//! mod declared in `megu-mod.toml`'s `[capabilities]` table.
//!
//! ```toml
//! [capabilities]
//! read_blocks = true
//! write_blocks = true
//! chat_broadcast = true
//! ```
//!
//! Undeclared capabilities fail closed at the ABI boundary — there is no
//! ambient authority. When this crate is compiled for a **native** target
//! (tests, host-side tooling) the same `log`/`get_block`/`set_block`/
//! `emit_chat` calls route through a thread-local [`HostBackend`] mock
//! instead of wasm imports, so a mod's logic is unit-testable without a wasm
//! toolchain.

use std::fmt;

/// ABI version every `megu_v1` guest must report from `megu_abi_version`.
pub const ABI_VERSION: i32 = 1;

/// Import module name for ABI v1 host functions. The runtime also accepts
/// `"env"` for toolchain compatibility.
pub const IMPORT_MODULE: &str = "megu_v1";

/// Success return code for every `megu_*` export.
pub const STATUS_OK: i32 = 0;

/// Required export: guest reports [`ABI_VERSION`].
pub const EXPORT_ABI_VERSION: &str = "megu_abi_version";
/// Optional export: `() -> i32`, invoked once after instantiation.
pub const EXPORT_ON_LOAD: &str = "megu_on_load";
/// Optional export: `(tick: i64) -> i32`, invoked once per server tick.
pub const EXPORT_ON_TICK: &str = "megu_on_tick";
/// Optional export: `(x, y, z, player_id: i32) -> i32`.
pub const EXPORT_ON_BLOCK_INTERACT: &str = "megu_on_block_interact";

/// Capabilities a wasm mod may declare in `megu-mod.toml`'s `[capabilities]`
/// table. Each maps 1:1 onto the host imports it unlocks; an import call
/// without the matching capability traps with `capability denied`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Capability {
    /// `host_get_block` — read block state at world coordinates.
    ReadBlocks,
    /// `host_set_block` — write block state at world coordinates.
    WriteBlocks,
    /// `host_emit_chat` — send a chat message to a player.
    ChatBroadcast,
}

impl Capability {
    /// Every capability key, in manifest order.
    pub const ALL: &'static [Capability] = &[
        Capability::ReadBlocks,
        Capability::WriteBlocks,
        Capability::ChatBroadcast,
    ];

    /// The `megu-mod.toml` `[capabilities]` key for this capability.
    pub const fn manifest_key(self) -> &'static str {
        match self {
            Capability::ReadBlocks => "read_blocks",
            Capability::WriteBlocks => "write_blocks",
            Capability::ChatBroadcast => "chat_broadcast",
        }
    }

    /// Look a capability up by its manifest key (`read_blocks`, ...).
    pub fn from_manifest_key(key: &str) -> Option<Capability> {
        Self::ALL.iter().copied().find(|c| c.manifest_key() == key)
    }
}

impl fmt::Display for Capability {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.manifest_key())
    }
}

/// Render a `[capabilities]` TOML table granting `caps` — for scaffolding a
/// `megu-mod.toml` or generating manifest fixtures.
///
/// ```
/// use megu_mod_sdk::native_abi::{capabilities_manifest, Capability};
/// let s = capabilities_manifest(&[Capability::ReadBlocks]);
/// assert_eq!(s, "[capabilities]\nread_blocks = true\n");
/// ```
pub fn capabilities_manifest(caps: &[Capability]) -> String {
    let mut out = String::from("[capabilities]\n");
    for c in caps {
        out.push_str(c.manifest_key());
        out.push_str(" = true\n");
    }
    out
}

// ---------------------------------------------------------------------------
// Host calls
// ---------------------------------------------------------------------------

/// Host backend used when the crate is compiled for a native target — lets
/// mod logic be unit-tested without a wasm engine. Mirrors the `megu_v1`
/// import surface exactly.
#[cfg(not(target_arch = "wasm32"))]
pub trait HostBackend {
    /// Ungated log line (`host_log`).
    fn log(&mut self, _message: &str) {}
    /// `host_get_block` — returns the block state id at `(x, y, z)`.
    fn get_block(&mut self, _x: i32, _y: i32, _z: i32) -> i32 {
        0
    }
    /// `host_set_block` — returns `0` on success.
    fn set_block(&mut self, _x: i32, _y: i32, _z: i32, _state: i32) -> i32 {
        0
    }
    /// `host_emit_chat` — returns `0` on success.
    fn emit_chat(&mut self, _player: i32, _message: &str) -> i32 {
        0
    }
    /// `megu_section_rev` — canonical revision of the `16³` section
    /// `(dim, cx, sy, cz)`, shared with every other mod lane.
    fn section_rev(&mut self, _dim: u32, _cx: i32, _sy: i32, _cz: i32) -> u64 {
        0
    }
    /// `megu_chunk_rev` — canonical aggregate revision of chunk column
    /// `(dim, cx, cz)`.
    fn chunk_rev(&mut self, _dim: u32, _cx: i32, _cz: i32) -> u64 {
        0
    }
    /// `megu_bump_block` — record one block edit at `(x, y, z)` in
    /// dimension `dim`; returns the edited section's new revision.
    fn bump_block(&mut self, _dim: u32, _x: i32, _y: i32, _z: i32) -> u64 {
        0
    }
}

#[cfg(not(target_arch = "wasm32"))]
mod host {
    use super::HostBackend;
    use std::cell::RefCell;

    thread_local! {
        static BACKEND: RefCell<Option<Box<dyn HostBackend>>> = const { RefCell::new(None) };
    }

    /// Install a mock host for native-target tests (thread-local).
    pub fn install<H: HostBackend + 'static>(backend: H) {
        BACKEND.with(|b| *b.borrow_mut() = Some(Box::new(backend)));
    }

    /// Remove the mock host; subsequent calls use the no-op defaults.
    pub fn uninstall() {
        BACKEND.with(|b| *b.borrow_mut() = None);
    }

    pub fn log(msg: &str) {
        BACKEND.with(|b| {
            if let Some(h) = b.borrow_mut().as_mut() {
                h.log(msg);
            }
        });
    }

    pub fn get_block(x: i32, y: i32, z: i32) -> i32 {
        BACKEND.with(|b| match b.borrow_mut().as_mut() {
            Some(h) => h.get_block(x, y, z),
            None => 0,
        })
    }

    pub fn set_block(x: i32, y: i32, z: i32, state: i32) -> i32 {
        BACKEND.with(|b| match b.borrow_mut().as_mut() {
            Some(h) => h.set_block(x, y, z, state),
            None => 0,
        })
    }

    pub fn emit_chat(player: i32, msg: &str) -> i32 {
        BACKEND.with(|b| match b.borrow_mut().as_mut() {
            Some(h) => h.emit_chat(player, msg),
            None => 0,
        })
    }

    pub fn section_rev(dim: u32, cx: i32, sy: i32, cz: i32) -> u64 {
        BACKEND.with(|b| match b.borrow_mut().as_mut() {
            Some(h) => h.section_rev(dim, cx, sy, cz),
            None => 0,
        })
    }

    pub fn chunk_rev(dim: u32, cx: i32, cz: i32) -> u64 {
        BACKEND.with(|b| match b.borrow_mut().as_mut() {
            Some(h) => h.chunk_rev(dim, cx, cz),
            None => 0,
        })
    }

    pub fn bump_block(dim: u32, x: i32, y: i32, z: i32) -> u64 {
        BACKEND.with(|b| match b.borrow_mut().as_mut() {
            Some(h) => h.bump_block(dim, x, y, z),
            None => 0,
        })
    }
}

#[cfg(target_arch = "wasm32")]
mod host {
    // The wasm mod runtime is the memory-safety boundary: these imports can
    // only touch the guest's own linear memory. `#[allow]` is scoped to this
    // module so the crate-level `#![deny(unsafe_code)]` still covers all
    // mod-author-facing code.
    #[allow(unsafe_code)]
    extern "C" {
        #[link_name = "host_log"]
        fn host_log(ptr: u32, len: u32);
        #[link_name = "host_get_block"]
        fn host_get_block(x: i32, y: i32, z: i32) -> i32;
        #[link_name = "host_set_block"]
        fn host_set_block(x: i32, y: i32, z: i32, state: i32) -> i32;
        #[link_name = "host_emit_chat"]
        fn host_emit_chat(player: i32, ptr: u32, len: u32) -> i32;
        #[link_name = "megu_section_rev"]
        fn megu_section_rev(dim: u32, cx: i32, sy: i32, cz: i32) -> u64;
        #[link_name = "megu_chunk_rev"]
        fn megu_chunk_rev(dim: u32, cx: i32, cz: i32) -> u64;
        #[link_name = "megu_bump_block"]
        fn megu_bump_block(dim: u32, x: i32, y: i32, z: i32) -> u64;
    }

    #[allow(unsafe_code)]
    pub fn log(msg: &str) {
        unsafe { host_log(msg.as_ptr() as u32, msg.len() as u32) }
    }

    #[allow(unsafe_code)]
    pub fn get_block(x: i32, y: i32, z: i32) -> i32 {
        unsafe { host_get_block(x, y, z) }
    }

    #[allow(unsafe_code)]
    pub fn set_block(x: i32, y: i32, z: i32, state: i32) -> i32 {
        unsafe { host_set_block(x, y, z, state) }
    }

    #[allow(unsafe_code)]
    pub fn emit_chat(player: i32, msg: &str) -> i32 {
        unsafe { host_emit_chat(player, msg.as_ptr() as u32, msg.len() as u32) }
    }

    #[allow(unsafe_code)]
    pub fn section_rev(dim: u32, cx: i32, sy: i32, cz: i32) -> u64 {
        unsafe { megu_section_rev(dim, cx, sy, cz) }
    }

    #[allow(unsafe_code)]
    pub fn chunk_rev(dim: u32, cx: i32, cz: i32) -> u64 {
        unsafe { megu_chunk_rev(dim, cx, cz) }
    }

    #[allow(unsafe_code)]
    pub fn bump_block(dim: u32, x: i32, y: i32, z: i32) -> u64 {
        unsafe { megu_bump_block(dim, x, y, z) }
    }
}

/// Ungated: emit a log line through the host (`host_log`).
pub fn log(message: &str) {
    host::log(message);
}

/// Read the block state id at `(x, y, z)` — requires `read_blocks`.
pub fn get_block(x: i32, y: i32, z: i32) -> i32 {
    host::get_block(x, y, z)
}

/// Write `state` at `(x, y, z)`; returns `0` on success — requires
/// `write_blocks`.
pub fn set_block(x: i32, y: i32, z: i32, state: i32) -> i32 {
    host::set_block(x, y, z, state)
}

/// Send `message` to `player` over chat — requires `chat_broadcast`.
pub fn emit_chat(player: i32, message: &str) -> i32 {
    host::emit_chat(player, message)
}

/// Canonical revision of the `16³` section `(dim, cx, sy, cz)` — the same
/// `SharedSectionRevTable` number every other mod lane (Fabric, NeoForge,
/// datapack, mod-offload) observes; `0` means never modified. Requires
/// `read_blocks`.
pub fn section_rev(dim: u32, cx: i32, sy: i32, cz: i32) -> u64 {
    host::section_rev(dim, cx, sy, cz)
}

/// Canonical aggregate revision of chunk column `(dim, cx, cz)` —
/// requires `read_blocks`.
pub fn chunk_rev(dim: u32, cx: i32, cz: i32) -> u64 {
    host::chunk_rev(dim, cx, cz)
}

/// Record one block edit at `(x, y, z)` in dimension `dim` against the
/// shared canonical spine; returns the edited section's new revision.
/// Border edits also invalidate the adjacent sections other mods watch.
/// Requires `write_blocks`.
pub fn bump_block(dim: u32, x: i32, y: i32, z: i32) -> u64 {
    host::bump_block(dim, x, y, z)
}

/// Install a native-target mock host (no-op signature on wasm32).
#[cfg(not(target_arch = "wasm32"))]
pub fn install_host<H: HostBackend + 'static>(backend: H) {
    host::install(backend);
}

/// Remove the native-target mock host.
#[cfg(not(target_arch = "wasm32"))]
pub fn uninstall_host() {
    host::uninstall();
}

// ---------------------------------------------------------------------------
// Mod trait + entry-point macro
// ---------------------------------------------------------------------------

/// Behaviour contract for a `megu_v1` guest. Implement this on a `Default`
/// struct and stamp the exports with [`megu_native_mod!`]; every method
/// defaults to a no-op returning [`STATUS_OK`], so a mod overrides only the
/// lifecycle hooks it needs.
pub trait MeguMod: Default {
    /// Called once after instantiation (`megu_on_load`).
    fn on_load(&mut self) -> i32 {
        STATUS_OK
    }

    /// Called once per server tick (`megu_on_tick`).
    fn on_tick(&mut self, _tick: i64) -> i32 {
        STATUS_OK
    }

    /// Called when a player interacts with a block (`megu_on_block_interact`).
    fn on_block_interact(&mut self, _x: i32, _y: i32, _z: i32, _player: i32) -> i32 {
        STATUS_OK
    }
}

/// Stamp the four `megu_*` export entry points for a [`MeguMod`]
/// implementation. Invoke once at crate top level:
///
/// ```ignore
/// megu_native_mod!(MyMod);
/// ```
///
/// Generates `megu_abi_version`, `megu_on_load`, `megu_on_tick`, and
/// `megu_on_block_interact` as `extern "C"` no-mangle functions backed by a
/// thread-local instance created via `Default`.
#[macro_export]
macro_rules! megu_native_mod {
    ($ty:ty) => {
        ::std::thread_local! {
            static MEGU_MOD_INSTANCE: ::std::cell::RefCell<$ty> =
                ::std::cell::RefCell::new(<$ty as ::core::default::Default>::default());
        }

        // `no_mangle` trips `deny(unsafe_code)`; the attribute only fixes the
        // symbol name — the unsafety boundary is the wasm runtime itself.
        #[allow(unsafe_code)]
        #[no_mangle]
        pub extern "C" fn megu_abi_version() -> i32 {
            $crate::native_abi::ABI_VERSION
        }

        #[allow(unsafe_code)]
        #[no_mangle]
        pub extern "C" fn megu_on_load() -> i32 {
            MEGU_MOD_INSTANCE.with(|m| {
                <$ty as $crate::native_abi::MeguMod>::on_load(&mut *m.borrow_mut())
            })
        }

        #[allow(unsafe_code)]
        #[no_mangle]
        pub extern "C" fn megu_on_tick(tick: i64) -> i32 {
            MEGU_MOD_INSTANCE.with(|m| {
                <$ty as $crate::native_abi::MeguMod>::on_tick(&mut *m.borrow_mut(), tick)
            })
        }

        #[allow(unsafe_code)]
        #[no_mangle]
        pub extern "C" fn megu_on_block_interact(
            x: i32,
            y: i32,
            z: i32,
            player: i32,
        ) -> i32 {
            MEGU_MOD_INSTANCE.with(|m| {
                <$ty as $crate::native_abi::MeguMod>::on_block_interact(
                    &mut *m.borrow_mut(),
                    x,
                    y,
                    z,
                    player,
                )
            })
        }
    };
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    #[test]
    fn capability_keys_round_trip() {
        for cap in Capability::ALL {
            assert_eq!(Capability::from_manifest_key(cap.manifest_key()), Some(*cap));
        }
        assert_eq!(Capability::from_manifest_key("fly"), None);
    }

    #[test]
    fn manifest_renders_grants_only() {
        let s = capabilities_manifest(&[Capability::ReadBlocks, Capability::ChatBroadcast]);
        assert_eq!(s, "[capabilities]\nread_blocks = true\nchat_broadcast = true\n");
    }

    struct Recorder {
        log: RefCell<Vec<String>>,
        blocks: RefCell<Vec<(i32, i32, i32, i32)>>,
    }

    impl HostBackend for Recorder {
        fn log(&mut self, message: &str) {
            self.log.borrow_mut().push(message.to_string());
        }
        fn get_block(&mut self, x: i32, y: i32, z: i32) -> i32 {
            x + y + z
        }
        fn set_block(&mut self, x: i32, y: i32, z: i32, state: i32) -> i32 {
            self.blocks.borrow_mut().push((x, y, z, state));
            0
        }
    }

    #[test]
    fn native_host_routes_calls() {
        log("no backend: no-op");
        install_host(Recorder {
            log: RefCell::new(Vec::new()),
            blocks: RefCell::new(Vec::new()),
        });
        log("hello megu");
        assert_eq!(get_block(1, 2, 3), 6);
        assert_eq!(set_block(0, 64, 0, 7), 0);
        assert_eq!(emit_chat(3, "hi"), 0);
        uninstall_host();
    }

    #[derive(Default)]
    struct TickCounter {
        ticks: i64,
    }

    impl MeguMod for TickCounter {
        fn on_tick(&mut self, tick: i64) -> i32 {
            self.ticks += tick;
            STATUS_OK
        }
        fn on_block_interact(&mut self, _x: i32, _y: i32, _z: i32, _player: i32) -> i32 {
            42
        }
    }

    megu_native_mod!(TickCounter);

    #[test]
    fn macro_exports_drive_instance() {
        assert_eq!(megu_abi_version(), ABI_VERSION);
        assert_eq!(megu_on_load(), STATUS_OK);
        assert_eq!(megu_on_tick(5), STATUS_OK);
        assert_eq!(megu_on_tick(7), STATUS_OK);
        assert_eq!(megu_on_block_interact(0, 0, 0, 1), 42);
        MEGU_MOD_INSTANCE.with(|m| assert_eq!(m.borrow().ticks, 12));
    }
}
