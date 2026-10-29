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

Sculk hook (the sculk cursor/parity work in `specs/worldgen-parity/sculk-patch.md`):

- `dmu$a` (`SculkSpreader$ChargeCursor`) / its update hook logs `SCURSOR r=<round> i=<index>
  x y z charge decayDelay updateDelay facingMask` and `SSPREAD`/`SSPREADALL`/`SCMARK` around
  the spread calls.
- `duz` (`ChunkGenerator`) / `applyBiomeDecoration(WorldGenLevel, ChunkAccess,
  StructureManager)`: at method entry, logs `SPASS cx cz` with the decorated chunk. One pass
  decorates the 3x3 biome sources around its chunk, so a chunk's placements are logged under
  nine different base positions and only the pass marker groups them correctly. The port emits
  the same marker from `populate` (`FEATURE_ATTEMPT_TRACE=1`).
- `ebq` (`ConfiguredFeature` - **not** `PlacedFeature`; the mapping is
  `net.minecraft.world.level.levelgen.feature.ConfiguredFeature -> ebq`) /
  `place(WorldGenLevel, ChunkGenerator, RandomSource, BlockPos)`: at method entry, logs
  `SPLACE <feature-class> <cx> <cz>` and `SRNG <rng-class>`, one line per feature placement.
  `PlacedFeature` is `eiv`: `eiv.b(...)` is `placeWithBiomeCheck` and `eiv.a(...)` is `place`.
  The placement modifiers run *before* `ConfiguredFeature.place` (inside
  `placeWithBiomeCheck`), so the `SPLACE` window cannot contain the placement's position draws -
  the armed window is owned by `eiv.b`, not by each `ebq` call. ConfiguredFeature returns
  must not disarm it, and nested placement returns must leave the outer window armed.
- Draw hooks: the legacy sources' bounded draws come from the mixin interface `dyn`
  (`BitRandomSource`), not from the concrete classes - measured 313289 `nextInt(bound)@dyn`
  lines; `dzx.c(I)I` is `next(bits)`, the raw generator (full-range values). The feature arrives as its *class* - `edi` is unique to `minecraft:sculk_patch`,
  but e.g. one ore class carries dozens of placed feature ids (measured: 550 attempts in one
  chunk over 14 classes), so this identifies a feature type, not a placed feature.
- `edi` (`SculkPatchFeature`) / `place(FeaturePlaceContext)`: at method entry, logs
  `SPATCH x y z` with `ecg.e()` (the placement origin). The trace of a run splits by patch on
  these, which is what makes two runs comparable: captures order patches differently and `r`
  counts every `updateCursors` call in the whole run (measured: the same patch is `r=0` in one
  capture, `r=192` in another). Only a few of the logged calls go on to spread (mk6 capture:
  1231 `SPATCH`, 57 origins spread).

`CarveLog` is appended to the bootstrap class loader (the game classes live in
the bundler's child loader, so an app-loader class is not visible to them) and
takes only primitives, so it never needs a game type. Frames are recomputed with
`COMPUTE_FRAMES` and `getCommonSuperClass` pinned to `Object`, because ASM's
default would try to load obfuscated classes.

## Build

```sh
bash tools/build_carve_agent.sh /path/to/new-agent-directory
```

The builder verifies jar membership and executes `AgentContractProbe` before publishing
jars. This runs a transformed ConfiguredFeature return inside an armed placement, including a
nested placement, so a logger that contains every marker but stops recording too early is refused.
Set `ASM_JAR` to select an explicit ASM dependency; otherwise the builder searches local caches.
Do not replace this gate with a hand-retyped compilation recipe.

For a bounded draw capture, `-Dsculkdrawpass=cx,cz` restricts expensive draw/stack traces to
one decoration pass without suppressing pass, patch, or cursor records. The trace records the
scope as `SDRAW_SCOPE`; omit the property to record all passes. Port `RAW_DRAW` records belong
to positional RNGs, not the native `@dyn` WorldgenRandom stream, and the comparator reports their exclusion.

`SculkLog` writes to its own file, not stdout: `-Dsculktrace=<path>` (default
`sculk-trace.txt`). The game routes stdout through log4j, whose console appender does not
preserve a thread's line order, so anything that attributes a line to the `SPASS`/`SPLACE`
window it sits between reads scrambled order (measured: a thread's sculk windows held none of
its 41208 draws). One lock plus a buffered writer keeps program order.

`SculkLog.patch` only takes three ints, like the other SculkLog methods, so it needs no game
class on the bootstrap classpath.

Both loggers go into `carvelog.jar` (the bootstrap-visible one); the agent jar carries ASM
itself, so nothing else has to be on the classpath.

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
