# Block/entity initialization

Observe actual StructureTemplate placement with NBT-bearing blocks and entities,
including rejection, rotation, position/UUID handling, loot seeds and finalizers.
The native level must perform initialization, not merely record a requested NBT
blob. Existing no-NBT admission remains until its replacement has a passing
contract. Reuse typed NBT and runtime state stores; do not introduce a competing
live block-entity map. Test handoff, packet NBT and save/reload independently.
No data migration is authorized by this plan; stop for a concrete decision if an
existing persistence contract cannot be preserved. Acceptance requires positive
initialized instances and a compiling omission mutant, not empty entity lists.
