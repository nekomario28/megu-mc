# carve-agent

`javaagent` that instruments the vanilla 1.21.1 server so the carver stage can be
measured from the inside instead of inferred. Built for the cave/canyon parity
work in `evidence/gameplay/carvers/README.md`.

Original carver hooks (both ASM, applied at class load):

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

The carver control's output was unchanged by the agent (same 617 cells). This
control is specific to that instrumentation and fixture, not a universal
non-perturbation guarantee for every diagnostic.

## Native stage captures

`native_fixture.sh <NEW-directory> <chunk-x> <chunk-z> <n> [frozen|ticked]`
captures seed 0 with structures disabled. It refuses overwrite, authenticates
the supplied 1.21.1 bundler and uses the existing cached ASM dependency; it does
not download or execute a newly fetched artifact.

Set `MC_SPINE_VANILLA_SERVER_JAR` (or the existing fixture base), then run only
after host resource admission, inside a bounded scope. The server uses a 1GB
heap and two active processors; the measured capture scope was limited to
1800MB and one CPU. Use disk-backed `TMPDIR` if tmpfs is full. The driver does
not acquire the host budget for its caller.

The frozen arm acknowledges `tick freeze` before force-loading the target.
Readiness, FULL coverage and save completion are observed, not timed guesses.
It records decoration order, complete feature/runtime write boxes and stacks,
input hashes and start/finish stamps. An unchanged game-time boundary and zero
runtime writes are required for a frozen capture. Ticked output remains a
saved-runtime state.

Only one to three target chunks per side are accepted by this bounded fixture.
The output contains the saved world, mapped `native.bin`, `order.tsv` and
`boundary.json`. The canonical world is never edited. Full-state production
comparison and its limits are in
[forest feature contracts](../../evidence/gameplay/forest-r1/feature-contracts.md).

Box selectors complement explicit cell selectors so a previously unknown
changed cell can still be attributed. The Java logger's primitive-only
boundary remains unchanged; unsupported proxy world operations must fail,
never return a plausible default.

## sculk hook (2026-10-28)

`SculkTransformer` hooks `SculkSpreader$ChargeCursor.update` (`dmu$a.a(dcx,jd,ayw,dmu,boolean)`)
and prints, at method entry, `SCURSOR <x> <y> <z> <charge> <decayDelay> <updateDelay> <mask>`
via the bootstrap `SculkLog`. `mask` is `-1` for a null facing set (the spreader's
SAME_POSITION-only variant), `0` for an empty set (the all-types variant), otherwise the face
mask over Direction's declaration order (down, up, north, south, west, east) - the same index
the port's own face mask uses, so a cursor's trajectory becomes two index-aligned lists.

The hook only reads fields and calls `jd.u()/v()/w()`, so it draws no random and cannot change
the spread. `-Dsculkquiet=true` silences it. Build it with `SculkLog.java` alongside
`CarveLog.java` (same jar list), and drive a features capture with

```sh
JAVA_AGENT=/path/to/carveagent.jar tools/chunk_world_fixture.sh sculk -16 112 1 features
grep -o 'SCURSOR .*' /tmp/chunkfix-sculk/run.log | head
```
