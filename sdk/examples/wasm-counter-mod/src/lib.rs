// SPDX-License-Identifier: LGPL-3.0-or-later

//! Example `megu_v1` guest: a per-tick counter that writes a block every 20
//! ticks and chats back when a player right-clicks it.
//!
//! Build for the runtime:
//!
//! ```sh
//! cargo build --release --target wasm32-unknown-unknown
//! ```
//!
//! then place `wasm_counter_mod.wasm` at `META-INF/megu/mod.wasm` inside a
//! `.mmod`/`.jar` archive alongside `megu-mod.toml` (see README.md).

// `deny` (not `forbid`): `megu_native_mod!` stamps `#[no_mangle]` exports,
// which rustc counts under `unsafe_code`. Guest code itself stays unsafe-free.
#![deny(unsafe_code)]

use megu_mod_sdk::native_abi::{self, MeguMod, STATUS_OK};

#[derive(Default)]
pub struct CounterMod {
    ticks: u64,
}

const COUNTER_POS: (i32, i32, i32) = (0, 64, 0);
const STATE_ON: i32 = 7;
const STATE_OFF: i32 = 0;

impl MeguMod for CounterMod {
    fn on_load(&mut self) -> i32 {
        native_abi::log("counter-mod loaded");
        STATUS_OK
    }

    fn on_tick(&mut self, tick: i64) -> i32 {
        self.ticks += 1;
        if self.ticks % 20 == 0 {
            let (x, y, z) = COUNTER_POS;
            let lit = native_abi::get_block(x, y, z) == STATE_ON;
            native_abi::set_block(x, y, z, if lit { STATE_OFF } else { STATE_ON });
        }
        let _ = tick;
        STATUS_OK
    }

    fn on_block_interact(&mut self, x: i32, y: i32, z: i32, player: i32) -> i32 {
        if (x, y, z) == COUNTER_POS {
            native_abi::emit_chat(player, "counter block!");
        }
        STATUS_OK
    }
}

// Stamps `megu_abi_version`, `megu_on_load`, `megu_on_tick`, and
// `megu_on_block_interact` as the wasm exports the runtime binds to.
megu_mod_sdk::megu_native_mod!(CounterMod);
