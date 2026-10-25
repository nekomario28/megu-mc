# Valence-inspired binary and geometry seams (local WIP)

Authority: Minecraft 1.21.1 / protocol 767 and its authenticated jar, not
Valence's 1.20.1 registries or packet constants. Donor revision and MIT license
are recorded in [reuse evidence](../../evidence/foundation/valence-reuse/README.md).
No Bevy, glam, java_string, ECS, or second protocol/collision engine is planned.

All three bounded slices are materialized locally; [the scoped receipt and
remaining limits](../../evidence/foundation/valence-primitives/README.md) are the
result authority. Publication and full migration acceptance remain separate.

Three independently testable slices, in risk-first order:

1. **Section palettes** — repair `section_encode`: singleton / indirect / global
   transitions, non-straddling long packing, real air-state counts. Share the
   packing algorithm for blocks and biomes instead of keeping two copies. Compare
   bytes with real `PalettedContainer.write` for deterministic initial-value and
   insertion order; use real BlockState registry, synthetic identity biome IDs
   with the installed registry's cardinality. This is encoding, not chunk/world
   generation parity. Do not port Valence's in-memory container.
2. **NBT strings** — keep `Comps` as opaque wire bytes. Replace ordinary UTF-8
   writes with modified UTF-8 through the existing `nbt_str` owner; make
   `nbt_string` use it. Retain the existing bounded-prefix policy for oversized
   server-authored text, now measured in encoded bytes without splitting a scalar;
   this policy is explicitly NOT Java writeUTF's overflow exception. Preserve old
   encoded lore on append rather than lossy decoding it through Rust String.
   Compare legal-size writes to actual StringTag.write, including NUL and astral
   characters. Raw Java lone-surrogate strings remain opaque; no JavaString-wide
   data-model migration or compatibility shim.
3. **Finite AABB clipping** — replace sampled entity intersection in `arrow_tick`
   with continuous segment geometry. Port pinned AABB.clip epsilon/open-endpoint
   rules, not Valence's inclusive infinite-ray slab test. Keep existing approximate
   entity boxes and existing start-inside contact policy explicit; block voxel
   shapes, pose dimensions, nearest-target ordering, physics and tracking require
   separate contracts. Compare clip coordinates with actual AABB.clip and prove
   production tick reachability with a between-sample hit.

Each slice needs a concrete red test, restored green control, and focused mutation
proof. Oracle checks must reject incomplete/changed inputs/results and stale jar or
source provenance. One shared authenticated Java probe can exercise the three
independent seams without starting a server. Retain the full-suite failing-name
baseline; do not re-record worldgen captures. Finish with formatting, clippy,
existing wire regressions, static independent review and source-bound evidence.
No hosted execution, publication, push, merge, save-format migration or old-data
compatibility machinery is included.
