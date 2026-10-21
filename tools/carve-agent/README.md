# carve-agent

`javaagent` that instruments the vanilla 1.21.1 server so the carver stage can be
measured from the inside instead of inferred. Built for the cave/canyon parity
work in `evidence/gameplay/carvers/README.md`.

Two hooks (both ASM, applied at class load):

- `dzb` (`NoiseBasedChunkGenerator`) / `applyCarvers`: around the
  `ConfiguredWorldCarver.isStartChunk` call it logs
  `APRE t=<target chunk> s=<source chunk> k=<carver index> seed=<seed+k>` and
  `APOST ... start=<bool>`. Bytecode is inserted with the local slots the
  1.21.1 method uses (12 = target `ChunkPos`, 19 = source `ChunkPos`, 23 =
  carver index, 2 = seed).
- `ebe` (`WorldCarver`) / `getCarveState(CarvingContext, C, BlockPos, Aquifer)`:
  before every `ARETURN` it logs `ASTATE <x> <y> <z> <block state id>` with `-1`
  for `null` (the aquifer refused the cell). The id comes from
  `Block.BLOCK_STATE_REGISTRY` in the instrumented bytecode, so no game class is
  referenced from the bootstrap logger.

`CarveLog` is appended to the bootstrap class loader (the game classes live in
the bundler's child loader, so an app-loader class is not visible to them) and
takes only primitives, so it never needs a game type. Frames are recomputed with
`COMPUTE_FRAMES` and `getCommonSuperClass` pinned to `Object`, because ASM's
default would try to load obfuscated classes.

## Build

```sh
ASM=~/.m2/repository/org/ow2/asm/asm/9.8/asm-9.8.jar
javac -nowarn -cp "$ASM" -d c1 tools/carve-agent/Agent.java
javac -nowarn -d c2 tools/carve-agent/CarveLog.java
(cd c2 && jar cf ../carvelog.jar .)
cp -r c1/* j1/ && (cd c1 && jar xf "$ASM" org) && cp -r c1/org j1/
printf 'Premain-Class: Agent\n' > j1/META-INF/MANIFEST.MF
(cd j1 && jar cfm ../carveagent.jar META-INF/MANIFEST.MF .)
```

`carvelog.jar` and `carveagent.jar` must sit in the same directory.

## Use

```sh
CARVER_FIXTURE_FRESH=1 CARVE_AGENT=/path/to/carveagent.jar \
  tools/carver_fixture.sh measured - zero zero 2
grep -o 'ASTATE .*' /tmp/carverfix-measured/run.log | head
```

The fixture output is unchanged by the agent (same 617 cells), so the
instrumentation does not perturb generation.
