# P1-3 scheduled tick delivery, 2026-11-20

Phase E Wave 4 E4-22b replays Vanilla 1.21.1 scheduled tick callbacks against
the Rust scheduler and world-save tick reader. The implementation base was
`861f4c70d00d7221f0634a2887d843c74ba8b933`.

## Native capture and replay

The oracle checks the server JAR SHA-1 before opening the embedded game JAR.
The input was `<home>/tmpx/carverfresh/server.jar`, SHA-1
`59353fb40c36d304f2035d51e7d6e6baa98dc05c`, run with OpenJDK 21.
The original fixtures reproduced byte-for-byte.

| Fixture | Native coverage |
| --- | --- |
| [Scheduled collection](../../../tests/data/scheduled_tick_1211.tsv) | 48 cases with runtime insertion, deduplication, bounded collection and rescheduling. |
| [Proto handoff](../../../tests/data/proto_tick_1211.tsv) | 18 cases with zero-delay, normal-priority proto staging and runtime ticks around intake. |
| [Delivery and reload](../../../tests/data/scheduled_tick_delivery_1211.tsv) | 6 cases across 26 game-tick steps, with 63 ordered block callbacks, 63 ordered fluid callbacks and one simultaneous reload. |

The [delivery probe](../../../tests/data/ScheduledTickDeliveryOracle.java)
calls native `ProtoChunkTicks`, `LevelChunkTicks` and `LevelTicks`. It runs
independent block and fluid queues in that order, with numeric identity aliases.
Each callback is joined to the corresponding native tick in its chunk
container, yielding position, identity, trigger time, priority and sub-order.
The Rust replay checks the complete ordered sequence for each phase and game
tick. It includes all seven priorities, capped and zero-budget collection,
container requeue, tied chunk heads, deadline-map growth, and a detached
container that is reattached while its eligibility predicate is still false.

Three replay differences were observed and corrected.

- A duplicate proto identity made the first block callback's sub-order
  `-3` in Rust and `-2` in Vanilla at game tick 101. Rust now deduplicates
  before computing the per-chunk pending-list length.
- At game tick 120 the second capped callback went to `(16, 70, 96)` in Rust
  and `(64, 70, -16)` in Vanilla. The native deadline-map iterator defers
  entries shifted across slot zero until its descending scan finishes. Rust
  now removes in place and visits those wrapped entries in shift order.
- The reload case first differed at game tick 131, when an overdue block
  tick retained sub-order `11` in Rust and received `-3` in Vanilla.
  Vanilla saves each heap in iteration order, stores signed 32-bit delays,
  and inflates the list at load time with per-chunk negative sub-orders.
  For example, after reload the fluid callbacks at game tick 132 visit
  `x=17` before `x=16`, reversing their order in the uninterrupted run.

The reload probe calls native `LevelChunkTicks.save` and NBT load for both
queues, then unpacks at the checkpoint game time. It activates pending chunks
in ascending `(x, z)` order, the deterministic order used by the Rust world
save. The Rust test exercises `TickState::append` and `TickState::read` on the
same checkpoint and verifies both runtime counters. The existing `TIK1`
encoding remains readable and the raw queue codec still roundtrips exact
heap and deadline-table state. World restart applies native inflation to those
decoded queues.

## Verification

- `cargo test --offline --locked -- scheduled_tick tick_persistence --test-threads=1`
  ran 9 tests. The two new replay tests account for all 6 delivery cases.
- The existing proto handoff, published-queue dispatcher and generator-lock
  drain tests ran separately. The drain test holds the generator lock while
  another thread drains both published queues.
- The oracle reproduced all three fixtures with the pinned JAR. Its
  `--help` invocation and
  `cargo check --offline --locked --tests --features fabric-probe,neoforge-events`
  passed.
- The layers gate reported **44 modules, 111 edges, 0 named errors**.
  The debt ratchet, provenance check and Git whitespace check passed.

## Live-world boundary

These traces establish scheduler callback and world-save tick-section parity
for the captured activation order and timestamps. Rust represents the tested
detach interval through its eligibility predicate. Live server physics and
chunk lifecycle remain outside the capture, including resulting block/fluid
states, falling entities, fluid propagation, and arbitrary chunk activation
order or timing after a restart.
