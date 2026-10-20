# vanilla-probe

Java probes that call the 1.21.1 server's own classes, so a question about
worldgen behaviour is answered by the code that ships rather than by a
reimplementation.

## OreCells.java

Runs `OreFeature.place` against a fake level and a counting `RandomSource`,
and prints every cell the vein writes:

```sh
# the outer server.jar is a signed bundler; extract the nested jar and use the
# extracted classes (a subclass of a signed class in the same package is
# rejected at runtime)
unzip -p <server.jar> META-INF/versions/1.21.1/server-1.21.1.jar > nested.jar
mkdir sj && (cd sj && unzip -q ../nested.jar)
CP="sj:$(find <server dir>/libraries -name '*.jar' | tr '\n' ':')"
javac -nowarn -cp "$CP" -d classes OreCells.java
java -cp "classes:$CP" OreCells 60007 7 49 13 64   # seed x y z size
```

For `seed=60007, pos=(7,49,13), size=64` it writes **666 cells**, and
`tools/ore_oracle.py` (the bytecode port) produces the **same 666 cells** -
exactly, cell for cell. That pins the vein geometry: radius from `Mth.sin`'s
table, centre from `Math.sin`, double `Mth.lerp`, the collapse pass, the
r=1 ellipsoid test and the x→y→z scan are all correct in the port.

The fake level needs a chunk because vanilla writes through `duw` (a section
cache) into a `LevelChunkSection`: `dcx.a(int,int)` returns a per-chunk
`duy` (allocated with `Unsafe`, since its constructor wants a height
accessor), whose `b(sectionIdx)` returns a `FakeSection` that records
`(chunkX*16+lx, sectionIdx*16-64+ly, chunkZ*16+lz)`.

## CarverProbe.java

Runs `ebb` (CaveWorldCarver) / `eaw` (CanyonWorldCarver) against an all-stone fake
chunk and prints every cell written:

```sh
CP="sj:$(find <server dir>/libraries -name '*.jar' | tr '\n' ':')"
javac -nowarn -cp "$CP" -d classes CarverProbe.java
java -cp "classes:$CP" CarverProbe 0 0 -3          # seed tx tz  (cave, k=0)
java -Dextra=1 -Dk=1 -cp "classes:$CP" CarverProbe 0 -5 -3
java -Dcanyon=1 -Dk=2 -cp "classes:$CP" CarverProbe 0 -5 -3
```

It walks the same -8..8 origin neighbourhood `applyCarvers` walks, with one shared
`CarvingMask`, and records `setBlockState`; the aquifer is a proxy that answers
cave air, so the recorded set is exactly the carver's geometry. Compare it with
mc-spine's all-stone probe:

```sh
MC_SPINE_NOFEAT=1 CARVE_PROBE="0,-3" CARVE_ONLY=minecraft:cave \
  MC_SPINE_WG_DUMP=/tmp/x.bin MC_SPINE_WG_DUMP_N=1 MC_SPINE_WG_DUMP_Z=-3 \
  ./target/release/mc-spine        # prints CELL x y z state
```

Two findings came out of this comparison, both invisible in the port by reading
alone: `carveEllipsoid` normalizes the y cell centre with `- 0.5` while x/z use
`+ 0.5`, and `Lcg::next_i64` must sign-extend both halves of `nextLong`.
