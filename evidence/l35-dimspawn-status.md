# L35 Nether and End chunk-generation creature spawning

## Change

Source base: `4b9866027efd7fe9447644b47a0d85c60b930881` (`main` at task start).

The existing first-visit chunk-generation creature pass now visits chunks in
the player's current Overworld, Nether, or End dimension. Overworld keeps its
existing candidate set and per-chunk call path. The generic creature pass still
queries only the biome `creature` table. Striders use lava spawn placement and
the live adapter selects a lava block as their candidate Y coordinate.

End's vanilla `the_end` biome has an empty `creature` table; its Endermen are
`monster` entries. The chunk-generation creature pass therefore runs for End
chunks but correctly adds no Endermen. Endermen remain the responsibility of
the existing monster spawning path.

## Vanilla reference

- Pinned runtime: Minecraft 1.21.1 dedicated server jar at
  `/home/yuu/.cache/mc-spine-audit/load/vanilla/versions/1.21.1/server-1.21.1.jar`
  (SHA-256 `c301de10f575027d13eac18c7f34409d60648cf56a35d566aa1f530ff617840a`).
- Official server mappings for 1.21.1 map
  `NaturalSpawner.spawnMobsForChunkGeneration` to `ddh.a(ddl,jm,dcd,ayw)` and
  `MobCategory.CREATURE` to `btq.b`. Bytecode for the method queries the biome's
  `CREATURE` spawner list before selecting entries.
- The repository's vanilla biome table at
  `evidence/foundation/biome-spawners/vanilla.json` lists `strider` in the
  `creature` category in Nether biomes, and an empty `creature` list plus
  `enderman` under `monster` for `minecraft:the_end`.

## Tests and evidence boundaries

- Existing Overworld unit coverage compares same-seed serial runs, spawn
  positions, placement queries, and final RNG state.
- Added coverage asserts Nether's creature table produces only lava-placed
  striders with the same positions, placement queries, and final RNG state as
  a serial reference. End's creature table produces no mobs and does not query
  its monster entries.
- These are deterministic unit tests of the chunk-generation algorithm. They
  do not establish a live vanilla wire comparison or prove actual server
  chunk-generation timing.
- `cargo check --offline --locked -j2`: PASS on the final shared checkout
  (99 existing dead-code warnings).
- `cargo test --release --offline -j2 chunk_generation`: PASS (3 passed).
- `cargo test --release --offline -j2
  chunk_gen_and_natural_spawning_cover_passive_aquatic_and_end`: PASS (1
  passed).
- `cargo fmt --all -- --check`: FAIL on extensive pre-existing formatting
  differences across unrelated workspace files; no formatter write was made.
- Runtime/device comparison: NOT RUN.

## Scope note

`src/main.rs` has the minimal additive change needed to include the current
dimension in the existing chunk-visit scheduler and to resolve Strider lava
placement. Concurrent edits in `src/play_protocol.rs`, client modules, server
files, and other workspace paths were observed during this task and left
untouched.
