# Protocol 767 villager trade and container wire status

## Source inspection

- Villagers are registered as a summonable entity in `src/entity_registry.rs`; `/summon` handling is implemented in `src/commands_core.rs`.
- The live entity interaction handler in `src/play_protocol.rs` routes villager interactions to `open_live_merchant`.
- `src/menus_and_items.rs` creates the farmer fallback with wheat-for-emerald and emerald-for-bread offers, sends the offers packet, selects a trade, and resolves merchant clicks.
- `proto/src/data_v767.rs` assigns clientbound `merchant_offers` packet `0x2d`, serverbound `select_trade` packet `0x2d`, serverbound `interact` packet `0x16`, and serverbound `container_click` packet `0x0e`.
- `tests/container_wire.py` provides the RCON-backed server launcher, creative-slot fixture, chest placement helpers, and the existing protocol click framing. The new test carries the observed chest state ID forward on every click.

## Test authored

`tests/trade_container_wire.py` logs in over protocol 767, summons and interacts with a villager, checks the merchant offers payload, selects and clicks a trade, and requires the result to appear in the player's inventory. It then places a chest, transfers 12 stone with window-click packets, and checks slot 0 using a second client connection.

## Live run

- Command: `python3 tests/trade_container_wire.py target/release/megu`
- Target present: `target/release/megu` (14,525,256 bytes)
- Result: **NOT RUN**. The run was attempted once but stopped before server startup. `tests/container_wire.py` obtains ephemeral ports through `tests/wire_smoke.py:19-21`, which binds `127.0.0.1`; the environment returned `Operation not permitted` on that first socket operation.
- JSON emitted by the test:

```json
{"container_transfer": "NOT RUN", "failure": "[Errno 1] Operation not permitted", "protocol": 767, "status": "FAIL", "villager_trade": "NOT RUN"}
```

The nonzero exit correctly records the environmental failure. No live packet, trade, or container behavior was verified. No unsupported stage was skipped: source inspection found both requested support paths.

The one live invocation occurred before the final assertion refinements in the test file. It failed in ephemeral-port setup before either version could enter a wire stage. The final file received a syntax-only compile check; it was not run again, preserving the one-run limit.

## Resolution (2026-11-24, local live run)

`villager_trade: PASS`, `container_transfer: PASS` on the release binary.

Fixes required in the harness (server behavior was correct):

- `client.pos` is None until `0x40` teleport; the test now waits for it.
- Villager protocol entity type is 113, not 117 (`wind_charge`).
- The entity-spawn wait now has a deadline instead of hanging.
- A creative player floats, so the summoned villager fell out of the 3.2
  interact range; a stone floor is filled under the summon point first.
- The result-slot click uses clickType 1 (vanilla quick-move) so the result
  stack lands in the player inventory instead of riding the cursor.
