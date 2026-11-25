# Rust ネイティブ Mod（WASM ABI v1） / Rust Native Mods (WASM ABI v1)

対象: `wasm-mod-runtime` の **サンドボックス化された WebAssembly レーン**で
Rust ロジックを走らせたい Mod 作者。宣言的なデータ Mod（`megu-mod-bundle.json`）
とは別の lane で、燃料計量・メモリ隔離・ケイパビリティゲート付きの MVP wasm
モジュールを実行します。

Audience: authors who want **real Rust logic** in a Megu mod without native
`.so`/`.dll` risk. The `wasm-mod-runtime` crate executes fuel-metered,
memory-isolated, capability-gated WebAssembly MVP modules — packaged inside a
standard `.mmod` or a Modrinth-uploadable `.jar`.

- ランタイム / runtime: `wasm-mod-runtime/` (pure `std`, zero deps, `#![forbid(unsafe_code)]`)
- ゲスト SDK / guest SDK: `sdk/megu-mod-sdk/src/native_abi.rs`
- 完全な例 / complete example: `sdk/examples/wasm-counter-mod/`

## 1. アーカイブレイアウト / Archive layout

```
my-mod.jar  (or .mmod — any zip container)
├── megu-mod.toml             # manifest; [capabilities] grants live here
└── META-INF/megu/mod.wasm    # wasm32 MVP module (required entry)
```

The runtime reads the archive **in memory** (via `megu-core`'s bounded
`ZipSliceIndex`) — nothing is extracted to disk. Both entries are optional
except `META-INF/megu/mod.wasm`; a missing `megu-mod.toml` simply grants zero
capabilities. An oversized or corrupt entry fails closed at load time.

## 2. ケイパビリティ / Capabilities

```toml
[capabilities]
read_blocks = true
write_blocks = true
chat_broadcast = true
```

| key | unlocks |
| --- | --- |
| `read_blocks` | `host_get_block`, `megu_section_rev`, `megu_chunk_rev` |
| `ui_surface` | Mark `UiLayer::ModHud` / `UiLayer::ModOverlay` dirty via `megu_ui_mark_dirty` |
| `write_blocks` | `host_set_block`, `megu_bump_block` |
| `chat_broadcast` | `host_emit_chat` |

The manifest is the **whole** authority: an unknown capability name rejects
the archive, and a `host_*` call without its grant traps with
`capability denied` and quarantines the mod. `host_log` is always allowed.

## 3. ABI v1 契約 / ABI v1 contract

Module namespace: imports arrive under `"megu_v1"` (or `"env"`).

### Guest exports (the runtime binds by name)

| export | signature | required |
| --- | --- | --- |
| `megu_abi_version` | `() -> i32`, must return `1` | yes |
| `megu_on_load` | `() -> i32` | no |
| `megu_on_tick` | `(tick: i64) -> i32` | no |
| `megu_on_block_interact` | `(x, y, z, player_id: i32) -> i32` | no |

Any signature mismatch rejects the module at load. Return `0`
(`STATUS_OK`) on success; a nonzero return is a fault.

### Host imports (capability-gated)

| import | signature | capability |
| --- | --- | --- |
| `host_log` | `(ptr: i32, len: i32) -> ()` | — (ungated) |
| `host_get_block` | `(x, y, z: i32) -> i32` | `read_blocks` |
| `host_set_block` | `(x, y, z, state: i32) -> i32` | `write_blocks` |
| `host_emit_chat` | `(player, ptr, len: i32) -> i32` | `chat_broadcast` |
| `megu_section_rev` | `(dim: u32, cx, sy, cz: i32) -> i64` | `read_blocks` |
| `megu_chunk_rev` | `(dim: u32, cx, cz: i32) -> i64` | `read_blocks` |
| `megu_bump_block` | `(dim: u32, x, y, z: i32) -> i64` | `write_blocks` |

`ptr`/`len` read a UTF-8 string from the guest's own linear memory — host
pointers can never escape in either direction.

The three `megu_*_rev`/`megu_bump_block` imports are the **generic mod
spine**: they delegate to the same canonical
`megu_core::section_rev::SharedSectionRevTable` every other mod lane
(Fabric bridge, NeoForge bridge, datapack, mod-offload) observes, via the
host's `MeguHost::spine_*` hooks (`SpineRevHost` wires a shared table in).
`dim` is a canonical `DimIndex` (`0` overworld, `1` nether, `2` end, `3+`
modded dimensions assigned by `ModDimRegistry`), `sy` is a full `i32`
section index (`block_y >> 4`), and `megu_bump_block` returns the edited
section's new revision — border edits also bump the adjacent sections
other mods watch. A host with no spine attached reads `0` and treats a
bump as a no-op.

## 4. 実行時の保証 / Runtime guarantees

- **Fuel**: every guest entry point runs under its own instruction budget
  (`ModConfig::{fuel_on_load, fuel_per_tick, fuel_per_event}`); an infinite
  loop exhausts fuel and traps in microseconds — it can never stall server
  TPS.
- **Memory**: linear memory is capped (`max_mem_pages`); out-of-bounds
  loads/stores trap.
- **Quarantine**: any trap, fuel exhaustion, or capability denial flips the
  mod's `ModFault` latch; subsequent ticks short-circuit. Other mods and the
  host process are unaffected — no panic, no UB, zero `unsafe` in the runtime.
- **MVP only**: `i32`/`i64`/`f32`/`f64`, structured control flow, locals,
  globals, linear memory, `call`/`call_indirect`. Threads, SIMD, GC,
  exceptions, and multi-memory are rejected by the validator.

## 5. Rust ゲストを書く / Writing a Rust guest

```rust
#![deny(unsafe_code)]
use megu_mod_sdk::native_abi::{self, MeguMod, STATUS_OK};

#[derive(Default)]
pub struct MyMod { ticks: u64 }

impl MeguMod for MyMod {
    fn on_tick(&mut self, tick: i64) -> i32 {
        self.ticks += 1;
        if self.ticks % 20 == 0 {
            native_abi::set_block(0, 64, 0, 7); // needs write_blocks
        }
        STATUS_OK
    }
}

megu_mod_sdk::megu_native_mod!(MyMod); // stamps the four exports
```

```sh
rustup target add wasm32-unknown-unknown
cargo build --release --target wasm32-unknown-unknown   # crate-type = "cdylib"
```

Host calls compile to the `megu_v1` imports on wasm32 and route through a
thread-local `HostBackend` mock on native targets — mod logic is unit-testable
without a wasm engine. Full walkthrough:
[sdk/examples/wasm-counter-mod/README.md](../sdk/examples/wasm-counter-mod/README.md).

## 6. 設計上の非目標 / Non-goals

- No filesystem, network, or host-pointer access — there is no WASI surface.
- No Java-loader interop; the wasm lane is Megu-native.
- `megu_mod_dev.py pack` does not yet emit `META-INF/megu/mod.wasm` — zip it
  manually or via CI until tooling catches up.
