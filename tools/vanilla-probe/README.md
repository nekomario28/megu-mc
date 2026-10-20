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
