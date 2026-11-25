# wasm-counter-mod

Minimal Rust guest for **Megu Native Mod ABI v1 (`megu_v1`)** — the sandboxed
WebAssembly mod lane implemented by `wasm-mod-runtime`. See
[docs/rust-native-mods.md](../../../docs/rust-native-mods.md) for the full ABI
and packaging contract.

## Build

```sh
rustup target add wasm32-unknown-unknown
cargo build --release --target wasm32-unknown-unknown
# -> target/wasm32-unknown-unknown/release/wasm_counter_mod.wasm
```

## Package

Place the `.wasm` and this directory's `megu-mod.toml` in a zip (`.mmod` or a
Modrinth-uploadable `.jar`):

```
counter_mod.mmod            (zip container)
├── megu-mod.toml           # manifest, incl. [capabilities] grants
└── META-INF/megu/mod.wasm  # the compiled guest
```

The `[capabilities]` table in `megu-mod.toml` is the whole authority: the
runtime grants `read_blocks`, `write_blocks`, and `chat_broadcast` exactly as
declared, and any `host_*` import call without its capability traps and
quarantines the mod — never the server.

## What it does

- `megu_on_load` logs a greeting (ungated `host_log`).
- `megu_on_tick` counts ticks; every 20th tick it toggles the block state at
  `(0, 64, 0)` between `0` and `7` (`host_get_block` + `host_set_block`).
- `megu_on_block_interact` sends `counter block!` to the interacting player
  (`host_emit_chat`) when they hit the counter position.
