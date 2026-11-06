# Fossil placement owns two world snapshots

A fossil is not two independent transformed block lists. The base and overlay
share selection, placement origin and random source, while the overlay must see
the world **after** the base has been placed.

## Ownership boundaries

`Fossil` in [`feature.rs`](../../src/feature.rs) owns the configured-feature
caller: paired template selection, the rotated footprint, frozen ocean-floor
reads, depth and empty-corner acceptance. It resolves resources at configuration
load time using the normal later-pack-wins policy. Non-directory pack entries
and non-directory path prefixes are absent resources, not shadowing overrides;
other IO errors fail closed. Required resources and processor children must not
silently disappear. Admission is intentionally
bounded to entity-free, NBT-free templates with full-cube outputs; arbitrary
structure templates are not accepted by this fossil path.

[`structure_template.rs`](../../src/structure_template.rs) owns the shared
solid-block placement stage. Palette selection consumes the caller's random
source even for one palette. A whole processor pass finishes before that
individual template writes anything. Only then are cells clipped, states
rotated and writes sent through the existing generation context. Processing
outside the clipping box still consumes its random draws.

The next template starts a new processor pass over the updated world. Reversing
the templates, processing both before either writes, or serving an immutable
initial scene to the overlay changes the result. Placement clipping and template
bounds are distinct: the caller's generation window limits writes, not which
source blocks processors inspect.

The generation context retains its existing write-through and heightmap owners.
Live maps follow writes; frozen `*_WG` maps do not. Source-liquid neighbors on
exposed placement faces schedule into the existing generator-owned fluid queue.
The shared [`voxel_faces`](../../src/voxel_faces.rs) owner provides the native
z/y/x face-transition order to both template placement and tree leaf edges;
write order is not face order. Native per-chunk `ProtoChunkTicks` insertion order
is compared separately from sorted membership. A separate
[handoff contract](../template-finalization/slices/03-delivery.md) compares the
native proto-to-level conversion and controlled collector callbacks. This does
not establish unrestricted runtime delivery, activation order or fluid physics.

## What the native comparison executes

[`fossil_oracle.py`](../../tools/fossil_oracle.py) invokes the registered native
`FossilFeature`, which selects and places both templates through native
`StructureTemplate.placeInWorld`. Template data and default configuration come
from the authenticated game jar. Explicit inline processor holders use the
native codecs; this standalone harness does not emulate a live dynamic-registry
bootstrap. Native manager getters lead to a populated native template cache,
and an unresolved binding fails rather than manufacturing an empty template.

The supplied level is a **scripted write-through world**, not a running server
or a native chunk capture. Native default methods remain bound to that level;
fluid requests reach real native `WorldGenTickAccess`/`ProtoChunkTicks` containers.
Unknown level operations fail. The cases own their initial cells and frozen
height inputs, separately from the expected output digests. Portable Rust replay
refuses to fall through to generated terrain when a required input is missing.

The fixture checks the native return value, complete block states, successful
in-build-height write order, following shared RNG and pending source-fluid
membership and per-chunk insertion order. Rejected out-of-height setter attempts are not part of that write
digest; this is not a comparison of every level-interface call. The production
parser/dispatcher has an additional native-backed consumer test. Actual native
RNG calls and template lookups attest the requested pair/rotation coverage;
synthetic sparse palettes exercise clipping without replacing the feature's
algorithm. Case membership lives in the executable inputs, not a second roster
in this document.

```sh
python3 tools/fossil_oracle.py
cargo test fossil_ -- --test-threads=1
```

Native regeneration requires an explicit `--write`. Native prerequisites are
not mandatory for the portable gate, and mocked adapter tests prove argument
handling and failure preservation only—not native behavior. Compiling mutants
in `tools/mutation_check.sh` distinguish the caller, state/RNG stages, world
visibility and queued-fluid effects.

## Remaining acceptance boundary

The controlled terrain comparison is not a claim of general template placement
or unrestricted fossil parity. Neighbor state rewrites and other block-specific
side effects beyond the tested terrain/source-liquid boundary remain open—for
example connected shapes, waterlogged neighbors and falling-block ticks. The
currently enabled fossil path does not implement those generic shape effects;
this is a limitation, not a promise that later runtime will repair them.

Block/entity initialization, fluid-containing template outputs, arbitrary
processors and nontrivial finalizers need their own native contracts. Native
whole-chunk fossil captures, unrestricted population scheduling, tick delivery,
light, persistence and protocol output remain separate acceptance gates.
The [finalization plan](../template-finalization/README.md) keeps these stages
separate; shared face ordering alone does not close neighbor-state finalization.
