# Tick delivery

## Pinned handoff

[`scheduled_tick_oracle.py`](../../../tools/scheduled_tick_oracle.py) preserves
the existing direct-collection cases and adds actual native proto staging,
LevelChunkTicks.unpack and LevelTicks callbacks. The fixture pins callback order,
trigger time, priority and sub-order with explicit activation/eligibility inputs,
bounded collection, mixed identities and ordinary runtime ticks around import.
Both generated block and fluid consumers use one unpack owner in
[`scheduled_ticks.rs`](../../../src/scheduled_ticks.rs). Each chunk's pending
list starts at its own negative length. Import must not consume the ordinary
runtime counter. Fluid import also adopts the pending positions.

This is a zero-delay, normal-priority generated-queue contract, not arbitrary
SavedTick decoding. The fluid consumer is tested through its real intake method;
both runtime dispatchers also drain a published generator's queues with chunks
ineligible, so the connection is checked without running physics. That dispatcher
fixture initializes Gen in an isolated child process: restoring world maps alone
leaves generation active in later cadence tests. A parent epoch guard pins this
boundary. Some replays encode/decode the port queue; that pins port persistence invariants,
not the native save format or a running server's save/reload. Fixtures do not run
block/fluid physics and do not establish unrestricted chunk activation order.

## Remaining delivery contract

Use real native ProtoChunkTicks, LevelChunkTicks and LevelTicks to observe
per-chunk insertion, inflation, priority, sub-order, eligibility, limits and
cross-chunk interleaving. Check existing scheduled_ticks before adding anything.
Membership equality is insufficient: compare ordered delivered events and the
resulting world at the same game times. Preserve generator ownership and the
no-GEN-lock drain contract. Compare block and fluid phases separately, then their
integration; include save/reload and ineligible chunks. Existing approximate
runtime gravity must not be relabeled native delivery without a native trace.
