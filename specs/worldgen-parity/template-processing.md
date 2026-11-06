# Template processing is not world placement

A template pipeline has different state and RNG owners at each stage. Merging
those stages into a transformed block list changes native behavior even when
the final position is correct.

## Read boundaries

`Template::process_blocks` in [`structure_template.rs`](../../src/structure_template.rs)
models native `StructureTemplate.processBlockInfos`. It transforms coordinates,
clones block-entity NBT, and visits blocks and processors in their declared
order. Block states have **not** been mirrored or rotated yet. A rejected block
stops visiting processors; processing does not write into the world or clip to
the eventual placement bounding box.

Processors must distinguish three values: the source template snapshot, the
candidate after preceding processors, and the existing world cell at the
transformed position. Rot eligibility uses the first; rule input tests use the
second; protection and rule location tests read the third. A generic live reread
or a single shared “current block” cannot substitute for these inputs.

The caller owns `settings.getRandom(world_position)`. With an explicit random
source, eligible rot checks advance that shared stream; without one, each check
creates the native position-seeded legacy source. Full integrity still consumes
a float, and equality retains the block. Deterministic rules use their own
position-seeded source without consuming the shared stream. Random rule tests
are not supported by this slice.

## Bounded supported shapes

`Processor::list` owns parser acceptance. It accepts the fossil lists' rot,
protected-tag and deterministic-rule shapes, including explicit rot block lists
and deterministic input/location block tests. Unsupported processors, required
tests, holder-set forms, position tests and NBT modifiers return an error;
they must not disappear from a list. Native codec rejection controls include
untagged protected identities and negative-zero integrity. Float parsing rounds
to native f32 before the range comparison.

The native oracle decodes actual processor codecs and invokes the actual whole
processing method, including cases loaded from pinned fossil templates.
[`template_processor_oracle.py`](../../tools/template_processor_oracle.py) owns
the inputs and native regeneration; Rust tests bind complete ordered typed-cell
digests, the following shared RNG value, shared float calls and world reads.
Compiling mutants in `tools/mutation_check.sh` distinguish source/current/world
state, coordinate and rotation stages, consumption, equality and child loss.
The existing native loader oracle remains independent.

```sh
python3 tools/template_processor_oracle.py
python3 tools/structure_template_oracle.py
cargo test structure_template::tests -- --test-threads=1
cargo run --locked -- --worldgen-template \
  datapacks/vanilla/data/minecraft/structure/fossil/skull_1.nbt 1 1 \
  datapacks/vanilla/data/minecraft/worldgen/processor_list/fossil_rot.json
```

The last command is explicitly an **air-world, position-seeded, read-only
preview** of the pre-placement list, not a real world default. Existing template
preview calls without a processor list retain their transform-only behavior.
Native Java checks are explicit; the portable gate does not require a JDK or
cached native jar.

## What remains outside this contract

The [fossil placement contract](fossil-placement.md) connects production dispatch,
shared palette/RNG selection, post-processing clipping and write-through placement
for the bounded full-cube fossil path. That separate contract includes actual
native feature execution on controlled worlds and states its remaining neighbor
shape and runtime limitations. This processing helper and its read-only preview
still do not establish world placement on their own. Other processors, generic
block/entity initialization and full-list finalizers require additional native
contracts before becoming accepted inputs.
