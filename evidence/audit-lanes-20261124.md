# Delegated audit lanes — consolidated findings (2026-11-24)

Five codex/Luna audit lanes ran read-only sandboxes and could not write their
evidence files; their verified findings are consolidated here with an honesty
note per item.

## persistence-parity (source-level, concrete)

Confirmed non-persisted vanilla state, by source inspection:

- map data (`live_map_states` — item frame / cartography map payloads)
- ~~scoreboard teams beyond partial fields~~ — persisted as TEM1 (beeaa890)
- ~~custom bossbars~~ — persisted as BAR1 (beeaa890)
- End-fight tracking (dragon fight sequence state)
- portal link records
- wandering-trader scheduler timers
- live redstone tick queue
- beehive occupant lists

Already persisted (verified present): modeled villager restock data, raids,
world time, weather countdowns, player respawn targets, scheduled fluid
ticks, falling blocks, custom bossbars, scoreboard teams.

Note: `save_v2_panics_abort_the_boundary_and_corruption_cannot_apply_the_new_state`
fails on clean HEAD (pre-existing, another lane's refactor) — the fixture's
poisoned-mutex premise no longer panics inside `WorldSaveSnapshot::capture`
because `saved_value` is poison-tolerant; the writer runs outside the catch
boundary. Needs a lane to re-establish the guard's coverage or update the test.

Partial: scoreboards, projectile ownership (raw entity IDs rather than UUID),
item-frame and armor-stand state.

## waste-audit top-8 (source-level; severity self-assessed below)

1. Mob activation scans all players per mob under overload shrink —
   `main.rs` env/mob loop: `players.iter().any(...)` is a ≤50-entry snapshot
   with early exit; ~100k cheap distance ops/tick worst case at 2k mobs.
   Bounded; a per-dim player index only helps when players spread across
   dims. Marginal.
2. Entity fanout rebuilds payloads per recipient — most constant payloads
   already go through `send_broadcast_frame` (fixed 2026-11-23); remaining
   per-recipient encoding is recipient-dependent by construction.
3. Mob tick rebuilds live-ID set + sweeps state maps per tick — bounded by
   mob count; cleanup-on-removal would need lifecycle hooks.
4. Spawn-cap recounts all mobs per check — counters on spawn/death would
   fix; several recounts per spawn pass at most.
5. Environment tick repeated player-state locks + stack clone for
   submerged players — a compact snapshot would help at 50 players.
6. Client world sync recounts 3 maps per rebuild call — exactness
   invariant; delta tracking possible.
7. Hi-Z submission rebuilds section list + Arc clones on view/residency
   changes — handle clones only, no vertex copies.
8. Mob loop locks pos+hp before inactive-mob gates — pos/hp locks are
   semantically required ahead of the death path; the `hot` snapshot check
   cannot move first without skipping env-death cleanup for out-of-range
   mobs. Mostly already bounded.

## wiregap / playgap / unwired (incomplete — recorded as leads only)

- wiregap: several inspected areas have Rust unit/in-process "live wire"
  tests only (UNIT-ONLY), not protocol-level wire coverage; portal frame
  ignition and fishing bite-signal are honestly skipped, matching the
  committed tests' records.
- playgap: server-side presence confirmed for boats/minecarts, portals,
  signs, books, beacons, banners, note blocks, jukeboxes; client-side
  advancement + recipe-book state and offhand HUD exist. The lane did not
  complete feature-by-feature classification — needs a writable re-run or
  manual pass before treating any gap list as complete.
- unwired: no reliable findings produced (sandbox rejected the write before
  candidate paths were verified).
