# Template placement: finalization and delivery

Retire four separate limitations of the bounded fossil path without substituting
scripted-level acceptance for a running world. Keep the authenticated 1.21.1 game
as a behavioral reference, not distributable source. Decompilations and captures
stay outside the repository.

## Current handoff (2026-11-13)

The old uncommitted 34-file fossil batch is historical context, not the current
working tree. Preserve its original fixtures and nine-column expectations, but
inspect the actual source/index before working. Use the
[current roadmap](../ROADMAP.md) and
[causal investigation workflow](../worldgen-parity/investigation-workflow.md).

The shared voxel visitor, native-style generated tick handoff, bounded
processor/fossil path, supported container/sign staging and entity offset
transforms exist. None closes all four finalization contracts. Actual template
entity creation remains refused; arbitrary NBT, general neighbor recursion,
world tick effects and whole-chunk structure acceptance remain separate work.

Next, make the second-seed comparison reproducible and inspect the snowy cells'
upper/lower positions, biome lookup and writer before changing behavior.
Investigate air/ore/plank residuals independently; do not infer a carver cause
from the final air state or merge causes because their coordinate bands overlap.

- [ ] [Neighbor finalization](slices/01-neighbors.md):
  - [x] Shared face sequence and per-chunk source-fluid reservation order.
  - [ ] Native block-state changes and the remaining scheduled effects.
- [ ] [Initialization](slices/02-initialization.md):
  - Supported container Items and sign front text use staging into existing stores.
  - Entity offset transforms are compared; actual entity creation is still refused.
  - General live initialization, arbitrary NBT and finalizers remain open.
- [ ] [Tick delivery](slices/03-delivery.md):
  - [x] Generated proto-to-level handoff and controlled cross-chunk callbacks.
  - [ ] Actual world tick effects, server activation order and native persistence.
- [ ] [Whole chunks](slices/04-chunks.md):
  - A seed-0 Overworld canonical-state fixture exists; seed 4242 remains exploratory.
  - Positive Fossil/structure chunks, heightmaps, initialization and pending ticks
    need matching input/stage/order contracts, followed by separate runtime gates.

## Single-owner invariants

- Geometry belongs to one shared visitor, not separate tree/template loops.
- State transitions must eventually serve both generation and runtime callers;
  do not copy main's scheduling/gravity implementation into the template placer.
- Gen owns generated ticks and staged initialization; existing runtime stores
  own live instances. A pending handoff is not a second live instance.
- Preserve frozen WG maps, live write-through reads, source/current/existing
  processor distinctions and original fixtures.
- Compare state, write order, RNG, initialization and ticks separately. Pending
  membership is not delivery. A helper match is not whole-chunk acceptance.

## Evidence and bounded work

Reference: authenticated official 1.21.1 server jar, pinned by
`tools/vanilla_java.py`; mappings and bytecode remain in the host audit.
Existing reuse: `tree::schedule_leaf_edges`, `structure_template`, `FC`,
`scheduled_ticks`, per-Gen tick queues, runtime block/entity maps, and existing
chunk capture/replay tools. Read both implementations before extracting an owner.

Use focused native/portable checks and compiling mutants while iterating, then
one completed unfiltered gate for a settled batch. Preserve failed and interrupted
attempts. Do not require Java or a native jar in the portable gate. Broad plan
fan-out is not used: this is not a speed-critical task and host RS4 forbids that
expense. Unknowns are probed sequentially at their owning seam.
