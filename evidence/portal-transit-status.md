# Nether portal transit wire evidence

Date: 2026-11-24

## Scope

Added `tests/portal_transit_wire.py`, a protocol 767 live-wire probe modeled on
`tests/dimension_wire.py` and the `Client`/`local_server` helpers in
`tests/protocol767.py`. It builds a 4x5 obsidian frame, tries `/setblock fire`
in the opening, records frame ignition as skipped because the source has no
obsidian-frame ignition path, then places a portal block directly and checks
the delayed Overworld-to-Nether and Nether-to-Overworld `0x47 Respawn` changes.
The probe emits one JSON summary and returns nonzero on failure.

Source inspection found portal-block detection and transit in
`src/play_protocol.rs` (`tick_portal`, `enter_portal`), Nether/Overworld
coordinate conversion, landing portal materialization, and an 80-game-tick
survival delay in `src/player_mechanisms.rs`. `/fill` and `/setblock` are
implemented in `src/commands_core.rs`. These findings establish the intended
test path, not live behavior.

## Run

Command:

```text
python3 tests/portal_transit_wire.py target/release/megu
```

Result: **FAIL before login; live transit not observed.** The local server
could not open its loopback listener in this execution environment. The test
returned status 1 and emitted:

```json
{"error": "[Errno 1] Operation not permitted", "protocol": 767, "result": "FAIL", "steps": []}
```

Because `steps` is empty, login, frame construction, ignition, portal delay,
both dimension transitions, and packet/chunk assertions were not reached. No
runtime or wire claim is made from this run. Frame ignition is an expected
skipped stage in the test design when that stage is reached; this execution
did not reach or observe it.

## Resolution (2026-11-24, local live run)

Run locally the full scenario passes end-to-end:

- `portal-trigger-placed`: `setblock minecraft:nether_portal` emits state 5864
  over `block_update` (0x09).
- `overworld-to-nether`: standing in the portal completes the survival
  `playersNetherPortalDefaultDelay` (80 GT) and emits `0x47 Respawn` into
  `minecraft:the_nether` (16 sections, no skylight).
- `nether-to-overworld`: after leaving the materialized arrival portal the
  bounce guard clears and the return trip lands at [0, 17, 0] (1:8 scaling).

Two defects were fixed for this result:

1. Test-side: `wait_for_update` scanned the whole update history, so stale
   fill/fire writes satisfied the wait. It now takes `since` and
   `wait_for_state` matches the target state itself.
2. Server-side (`src/play_protocol.rs`, `src/main.rs`): `tick_portal` set
   `in_portal` on first detection, which short-circuited the delay branch on
   every later call — a nonzero survival delay could never complete. It also
   ran only on movement packets, so a player standing still would never
   transit. `in_portal` is now set only by `enter_portal` (the arrival guard),
   and the check runs on the game tick like vanilla `handleInsidePortal`.

Frame ignition stays honestly `skipped`: the runtime has no
obsidian-frame ignition path.
