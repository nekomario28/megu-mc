# Procedural mineshaft connection

The Rust block-generation path is connected and checked in the
[production checkpoint](../../evidence/gameplay/feature-parity/mineshaft-production-20261113.md).
The subsequent [entity/save and coverage checkpoint](../../evidence/gameplay/feature-parity/mineshaft-entities-20261113.md)
adds generated chest carts, spawner NBT and restart coverage. Neither checkpoint
accepts every mineshaft, complete vehicle/spawner gameplay, or whole-world parity.
The recovered [admission](../../evidence/gameplay/feature-parity/mineshaft-admission-20261113.md),
[layout](../../evidence/gameplay/feature-parity/mineshaft-layout-20261113.md) and
[stage/writer](../../evidence/gameplay/feature-parity/mineshaft-stage-writer-20261113.md)
contracts remain the native authority.

## Admission is not placement

The [admission owner](../../src/mineshaft.rs) composes existing placement/frequency
primitives with fresh procedural layouts and the generation point's noise biome.
A failed weighted entry is removed and retried; its layout RNG must not advance
the selector or the next attempt. NORMAL and Mesa share the
[layout owner](../../src/mineshaft_layout.rs), not a captured list of pieces.

Decoration slots belong to the structure registry, not to successful starts or
the weighted selector. The dispatcher seeds once per structure type, traverses
references in native container order, and shares that stream across ordered
pieces. Each structure type owns a separate reference set: sharing capacity
between NORMAL and Mesa changes encounter order even after filtering by type. Structures execute before placed features at their step, including when
the datapack contains no placed biome features.

## The context owns the world

The [piece writer](../../src/mineshaft_piece.rs) uses the existing FC and registry
contracts. Inclusive chunk clipping does not imply clipping to a piece's own
bounds: floors and supports can extend beyond those bounds. Native state defaults,
face-sturdiness and anchor predicates must not be replaced by render/collision
approximations.

WG means frozen producer state. The captured read status was CARVERS, but the WG
producer is SURFACE; later carver/feature writes must not be rescanned into that
map. The native status canary in the stage checkpoint distinguishes these cases.
Captured heights, saved pieces, reference scenes and coordinate corrections never
enter production generation.

## Payloads cross an ownership boundary

[GenerationPayload](../../src/generation_payload.rs) extends the existing template
handoff rather than creating a second placement stack. Gen queues data and owns
source claims/one-shot piece flags. The [runtime content owner](../../src/generated_content.rs)
preserves typed spawner NBT and chest-cart identity/loot data; carts are vehicles,
not walking mobs. Source claims survive saving so evicted block caches cannot
recreate carts or reset edited NBT. Generated spawner blocks survive in the world
overlay; replacing them removes their NBT without undoing source claims.

A block read may hold mob/player locks, so it must not take SAVE_LOCK or materialize
payloads. Tick/chunk boundaries and saving transfer ownership in save → gen → runtime
order. The existing raw snapshot builder retains its caller-held save-lock contract.

## Remaining boundaries

- Reference discovery is bounded per target chunk but recomputes starts; no start
  cache or performance gain is claimed.
- Vehicle motion, chest-cart loot unpacking/container interaction and spawner
  server ticking remain unimplemented. Spawn packets and persisted NBT do not
  establish those gameplay effects. Restoring generation metadata requires an
  initialized generator; switching generation settings is not a migration contract.
- Natural Mesa admission/ordered geometry now has two native locations. Multiple
  starts now have actual native reference-manager/ordered-write/shared-RNG coverage
  on authored terrain. These do not establish complete natural Mesa chunk parity,
  general mixed-type runtime traversal, original A/B population history or broader
  parity acceptance.
- The checkpoint records the executed regression and mutation scopes. The complete
  multi-round/wire/all-mutation promotion gate has not been rerun for this change.
  Publication is not authorized by the implementation request.
