# Forest R1 — explicit order, explicit stage, every state

This fixture compares seed 0, structures disabled, chunks `(-64,32)` through
`(-62,34)`, all block states at y=-64..319. The decoration footprint is the
same 25 source chunks on both sides.

**The production generation dump matches all 884,736 cells of paused native
1.21.1: zero differences, including leaf distance and fluid levels.**
This uses an explicit observed source order, not an assumed vanilla coordinate
sort. It does not establish port runtime/ticket-scheduler parity.

| Comparison | Unequal cells |
|---|---:|
| Production port, explicit order → paused native | **0** |
| Fresh ticking native → unchanged canonical saved world | **0** |
| Paused native / port generation → canonical saved world | **1,803** |

Every one of the last row's differences has an actual native runtime last
writer: 1,763 `LeavesBlock.tick`, 40 `FlowingFluid.tick`. These cells are
**counted**, not excluded. Attribution identifies the remaining lifecycle
contract; it does not implement the port's ticks.

[Feature contracts and reproduction](feature-contracts.md) carry the oracles,
receipt and remaining work. [Historical diagnostics](history.md) retain the
earlier investigations. [Ore exposure](ore-air.md) is one historical isolation.

## Comparison invariants

- Compare the same seed, data, structures setting, source footprint **and order**.
  Same chunks in different orders can produce different vegetation.
- Compare the same lifecycle stage. A `distance` property alone cannot identify
  its writer: both TreeFeature and runtime leaves change it.
- Require every requested native chunk to be FULL. Partial coverage is an error,
  not a green subset.
- Authenticate the native executable; preserve its signatures and load real
  block tags when a probe depends on tags.
- Keep the canonical saved world unchanged. The paused world is a separate
  stage control, not a replacement acceptance reference.
- A regression capture is not a native oracle. Refresh captures only after
  independent native behavior establishes the correction.

## Limits and next boundary

One seed/region is not whole-world or structures acceptance. Normal lazy
population remains a local scan, not an implementation of vanilla's ticket
scheduler. The ordered dump exposes scheduling as input instead of hardcoding
this fixture's 25 coordinates.

Next: reproduce native scheduled-leaf/fluid queues and tick behavior through the
port's runtime owner, then compare the same post-generation input and time
boundary against the unchanged canonical world. JVM identity-hash tie-breaking
inside a leaf tree bin remains explicitly unsupported; it does not occur in
this accepted region. See the feature contract for the fail-closed guard.
