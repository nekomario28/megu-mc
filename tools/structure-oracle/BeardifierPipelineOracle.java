package dev.megu.tools;

import com.google.gson.*;
import java.lang.reflect.*;
import java.nio.file.*;
import java.util.*;
import java.util.function.Function;
import java.util.stream.Stream;

/**
 * Native 1.21.1 `Beardifier` density and `StructurePlacement.isStructureChunk` vectors.
 *
 * Two kinds of evidence, both produced by running the pinned jar's own classes:
 *
 *  - `SCENE`/`RIGID`/`JUNC`/`SAMPLE`: `new Beardifier(rigids, junctions).compute(ctx)` for
 *    authored rigid/junction sets. Every `TerrainAdjustment` value, both `Projection`
 *    values on the junctions, out-of-kernel-range positions and an empty set are covered,
 *    so the port's kernel, `fastInvSqrt` bit-hack and per-adjustment branches are each
 *    exercised. The double is recorded as raw bits: a tolerance would hide a wrong kernel.
 *  - `ADMIT`: `StructurePlacement.isStructureChunk(state, x, z)` for every structure set of
 *    the pinned data, with the real `ChunkGeneratorStructureState` built by
 *    `createForNormal`. This is the placement-chunk + frequency-reducer + exclusion-zone
 *    chain, run by the class that owns it, including `pillager_outposts`' zone against
 *    `villages`.
 *
 * Not claimed here: native starts for every structure (they need a full world), or the
 * decoration writes (the mineshaft pipeline oracle already runs those on real chunks).
 */
public final class BeardifierPipelineOracle {
    static Class<?> c(String s) { return NativeHarness.C(s); }
    static Object f(String cl, String n) throws Exception { return c(cl).getField(n).get(null); }
    static Object registry(Object access, String n) throws Exception {
        return c("ka").getMethod("d", c("akq")).invoke(access, f("lu", n));
    }
    static Object value(Object r, String n) throws Exception {
        Object id = c("akr").getMethod("a", String.class).invoke(null, n);
        Object v = c("jz").getMethod("a", c("akr")).invoke(r, id);
        if (v == null) throw new AssertionError("missing " + n);
        return v;
    }
    static Object holder(Object v) throws Exception { return c("jm").getMethod("a", Object.class).invoke(null, v); }
    static String name(Object r, Object v) throws Exception { return c("jz").getMethod("b", Object.class).invoke(r, v).toString(); }

    /** `DensityFunction.FunctionContext` with a fixed block position. */
    static Object context(int x, int y, int z) {
        return Proxy.newProxyInstance(BeardifierPipelineOracle.class.getClassLoader(),
            new Class<?>[]{c("dyr$b")}, (proxy, m, a) -> switch (m.getName()) {
                case "a" -> x;
                case "b" -> y;
                case "c" -> z;
                case "toString" -> "beard-ctx(" + x + "," + y + "," + z + ")";
                case "hashCode" -> System.identityHashCode(proxy);
                case "equals" -> proxy == a[0];
                default -> throw new UnsupportedOperationException(m.toString());
            });
    }

    static int[] box(Object bb) throws Exception {
        return new int[]{
            (int) c("ejj").getMethod("h").invoke(bb), (int) c("ejj").getMethod("i").invoke(bb),
            (int) c("ejj").getMethod("j").invoke(bb), (int) c("ejj").getMethod("k").invoke(bb),
            (int) c("ejj").getMethod("l").invoke(bb), (int) c("ejj").getMethod("m").invoke(bb)};
    }

    static Object boundingBox(int[] b) throws Exception {
        return c("ejj").getConstructor(int.class, int.class, int.class, int.class, int.class, int.class)
            .newInstance(b[0], b[1], b[2], b[3], b[4], b[5]);
    }

    static List<String> scenes(List<String> rows) throws Exception {
        Object[] adjustments = c("ekc").getEnumConstants();
        String[] names = {"none", "bury", "beard_thin", "beard_box", "encapsulate"};
        if (adjustments.length != names.length) throw new AssertionError("TerrainAdjustment arity");
        // One box per adjustment, plus a vertically offset box (ground-level delta is only
        // read through `minY + delta`) and a box that never overlaps the sample column.
        int[][] boxes = {
            {-15, 60, -15, 15, 80, 15},
            {-15, 60, -15, 15, 80, 15},
            {-15, 60, -15, 15, 80, 15},
            {-15, 60, -15, 15, 80, 15},
            {-15, 60, -15, 15, 80, 15},
            {-4, 62, -4, 4, 66, 4},
            {100, 0, 100, 110, 10, 110},
        };
        int[] deltas = {0, 0, 0, 0, 0, 7, 0};
        for (int i = 0; i < boxes.length; i++) {
            String id = "adj-" + names[i % names.length] + (i >= names.length ? "-offset" : "");
            Object adjustment = adjustments[i % names.length];
            List<Object> rigids = new ArrayList<>();
            rigids.add(c("dyl$a").getConstructor(c("ejj"), c("ekc"), int.class)
                .newInstance(boundingBox(boxes[i]), adjustment, deltas[i]));
            emitScene(rows, id, rigids, List.of(), boxes[i], deltas[i]);
        }
        // Junctions: both projections, positive and negative vertical offsets, and a source
        // far outside the kernel.
        Object[] projections = c("elb$a").getEnumConstants();
        if (projections.length != 2) throw new AssertionError("Projection arity");
        for (int p = 0; p < projections.length; p++) {
            for (int dy : new int[]{-3, 0, 4}) {
                for (int[] src : new int[][]{{3, 65, -2}, {-40, 70, 12}}) {
                    String id = "junc-" + (p == 0 ? "terrain_matching" : "rigid") + "-dy" + dy
                        + "-src" + src[0] + "_" + src[2];
                    List<Object> junctions = new ArrayList<>();
                    junctions.add(c("eku").getConstructor(int.class, int.class, int.class, int.class, c("elb$a"))
                        .newInstance(src[0], src[1], src[2], dy, projections[p]));
                    emitScene(rows, id, List.of(), junctions, new int[]{-15, 60, -15, 15, 80, 15}, 0);
                }
            }
        }
        // A scene with several rigids and junctions: the port must sum in the same order.
        {
            List<Object> rigids = new ArrayList<>();
            for (int i = 0; i < 3; i++) {
                rigids.add(c("dyl$a").getConstructor(c("ejj"), c("ekc"), int.class).newInstance(
                    boundingBox(new int[]{-8 + i * 4, 58 + i, -8, 8 - i, 72 + i, 8}),
                    adjustments[(i + 1) % adjustments.length], i));
            }
            List<Object> junctions = new ArrayList<>();
            for (int i = 0; i < 3; i++) {
                junctions.add(c("eku").getConstructor(int.class, int.class, int.class, int.class, c("elb$a"))
                    .newInstance(i * 6 - 6, 60 + i, 5, i - 1, projections[i % 2]));
            }
            emitScene(rows, "mixed-rigids-and-junctions", rigids, junctions,
                new int[]{-15, 60, -15, 15, 80, 15}, 0);
        }
        // Empty: the beardifier a chunk with no adapting structure gets.
        emitScene(rows, "empty", List.of(), List.of(), new int[]{-15, 60, -15, 15, 80, 15}, 0);
        return rows;
    }

    static void emitScene(List<String> rows, String id, List<Object> rigids, List<Object> junctions,
                          int[] probe, int groundLevelDelta) throws Exception {
        Object beard = c("dyl").getConstructor(c("it.unimi.dsi.fastutil.objects.ObjectListIterator"),
                c("it.unimi.dsi.fastutil.objects.ObjectListIterator"))
            .newInstance(iter(rigids), iter(junctions));
        // Sample a vertical column inside the box, a column outside it, and both horizontal
        // edges of the kernel range (dx == 12 is the last in-range offset).
        List<int[]> samples = new ArrayList<>();
        int cx = (probe[0] + probe[3]) / 2;
        int cz = (probe[2] + probe[5]) / 2;
        for (int dy = -14; dy <= 14; dy += 4) {
            samples.add(new int[]{cx, probe[1] + dy, cz});
        }
        for (int dx : new int[]{1, 8, 12, 13}) {
            samples.add(new int[]{probe[3] + dx, probe[1] + 6, cz});
            samples.add(new int[]{cx, probe[1] + 6, probe[5] + dx});
            samples.add(new int[]{probe[3] + dx, probe[4] + dx, probe[5] + dx});
        }
        List<String> sampleRows = new ArrayList<>();
        for (int[] s : samples) {
            double v = (double) c("dyl").getMethod("a", c("dyr$b")).invoke(beard, context(s[0], s[1], s[2]));
            sampleRows.add("SAMPLE\t" + id + "\t" + s[0] + "\t" + s[1] + "\t" + s[2]
                + "\t" + Long.toHexString(Double.doubleToRawLongBits(v)));
        }
        rows.add("SCENE\t" + id + "\t" + rigids.size() + "\t" + junctions.size() + "\t" + samples.size());
        for (Object rigid : rigids) {
            int[] b = box(c("dyl$a").getMethod("a").invoke(rigid));
            Object adjustment = c("dyl$a").getMethod("b").invoke(rigid);
            int delta = (int) c("dyl$a").getMethod("c").invoke(rigid);
            rows.add("RIGID\t" + id + "\t" + b[0] + "\t" + b[1] + "\t" + b[2] + "\t" + b[3]
                + "\t" + b[4] + "\t" + b[5] + "\t" + adjustment.toString().toLowerCase(Locale.ROOT)
                + "\t" + delta);
        }
        for (Object junction : junctions) {
            int sx = (int) c("eku").getMethod("a").invoke(junction);
            int sy = (int) c("eku").getMethod("b").invoke(junction);
            int sz = (int) c("eku").getMethod("c").invoke(junction);
            int delta = (int) c("eku").getMethod("d").invoke(junction);
            Object projection = c("eku").getMethod("e").invoke(junction);
            rows.add("JUNC\t" + id + "\t" + sx + "\t" + sy + "\t" + sz + "\t" + delta + "\t"
                + projection.toString().toLowerCase(Locale.ROOT));
        }
        rows.addAll(sampleRows);
        // The ground-level delta is only read through `minY + delta`; record the scene's so a
        // reader can tell the two offset cases apart.
        rows.add("SCENEMETA\t" + id + "\t" + groundLevelDelta);
    }

    static Object iter(List<Object> values) throws Exception {
        Object list = c("it.unimi.dsi.fastutil.objects.ObjectArrayList")
            .getConstructor(int.class).newInstance(Math.max(1, values.size()));
        for (Object v : values) list.getClass().getMethod("add", Object.class).invoke(list, v);
        return list.getClass().getMethod("iterator").invoke(list);
    }

    static void bindTags(Object biomes, Object manager) throws Exception {
        Function<Object, Object> lookup = id -> {
            try {
                return c("jz").getMethod("c", c("akr")).invoke(biomes, id);
            } catch (Exception e) {
                throw new AssertionError(e);
            }
        };
        Object loader = c("awv").getConstructor(Function.class, String.class).newInstance(lookup,
            c("lu").getMethod("d", c("akq")).invoke(null, f("lu", "aF")));
        Map<?, ?> loaded = (Map<?, ?>) c("awv").getMethod("b", c("aue")).invoke(loader, manager);
        if (loaded.isEmpty()) throw new AssertionError("missing native biome tags");
        Map<Object, Object> tags = new HashMap<>();
        for (var e : loaded.entrySet()) {
            tags.put(c("awu").getMethod("a", c("akq"), c("akr")).invoke(null, f("lu", "aF"), e.getKey()),
                List.copyOf((Collection<?>) e.getValue()));
        }
        c("jz").getMethod("a", Map.class).invoke(biomes, tags);
    }

    public static void main(String[] args) throws Exception {
        NativeHarness.bootstrap();
        Object access = c("ka").getMethod("a", c("jz")).invoke(null, f("lt", "aA"));
        Object manager = c("atx").getConstructor(c("ass"), List.class)
            .newInstance(f("ass", "b"), List.of(c("ats").getMethod("b").invoke(null)));
        access = c("akm").getMethod("a", c("aue"), c("ka"), List.class)
            .invoke(null, manager, access, f("akm", "a"));
        Object biomes = registry(access, "aF"), sets = registry(access, "aT");
        bindTags(biomes, manager);

        List<String> rows = new ArrayList<>();
        scenes(rows);

        Object settings = value(registry(access, "aO"), "minecraft:overworld");
        Object source = c("dej").getMethod("a", c("jm"))
            .invoke(null, holder(value(registry(access, "aZ"), "minecraft:overworld")));
        Object noises = c("jz").getMethod("q").invoke(registry(access, "aP"));
        Object setLookup = c("jz").getMethod("q").invoke(sets);
        long[] seeds = {4242L, -1L};
        // Dense enough to contain positives for the spacing-1 mineshaft set and a few of
        // the spacing-32 sets; five far-apart chunks alone would pin almost only zeros.
        List<int[]> chunks = new ArrayList<>();
        for (int x = -2; x <= 6; x++) for (int z = -4; z <= 4; z++) chunks.add(new int[]{x, z});
        for (int[] far : new int[][]{{1875000, -1875000}, {-33, 64}}) chunks.add(far);
        List<Object> placements = new ArrayList<>();
        List<String> setIds = new ArrayList<>();
        for (Object set : ((Stream<?>) c("jz").getMethod("t").invoke(sets)).toList()) {
            setIds.add(name(sets, set));
            placements.add(c("ejx").getMethod("b").invoke(set));
        }
        // Stable order: the fixture must not depend on registry iteration order.
        List<Integer> order = new ArrayList<>();
        for (int i = 0; i < setIds.size(); i++) order.add(i);
        order.sort(Comparator.comparing(setIds::get));
        for (long seed : seeds) {
            Object random = c("dzm").getMethod("a", c("dzd"), c("jn"), long.class)
                .invoke(null, settings, noises, seed);
            Object state = c("dva").getMethod("a", c("dzm"), long.class, c("dea"), c("jo"))
                .invoke(null, random, seed, source, setLookup);
            for (int index : order) {
                String id = setIds.get(index);
                Object placement = placements.get(index);
                for (int[] chunk : chunks) {
                    boolean admitted = (boolean) c("eko").getMethod("b", c("dva"), int.class, int.class)
                        .invoke(placement, state, chunk[0], chunk[1]);
                    rows.add("ADMIT\t" + id + "\t" + seed + "\t" + chunk[0] + "\t" + chunk[1]
                        + "\t" + (admitted ? 1 : 0));
                }
            }
            // The exclusion zone needs a chunk where the zone *changes* the answer: a
            // pillager placement chunk with a village inside the 10-chunk radius. Emitting
            // only the surviving chunks would pin the zone on the rows it never touches.
            Object pillager = null, villages = null;
            for (int index : order) {
                if (setIds.get(index).equals("minecraft:pillager_outposts")) pillager = placements.get(index);
                if (setIds.get(index).equals("minecraft:villages")) villages = placements.get(index);
            }
            if (pillager == null || villages == null) throw new AssertionError("missing pillager/villages sets");
            // `isPlacementChunk` is protected in `StructurePlacement`; the base-class
            // method is the one `isStructureChunk` calls first, and the reason the fixture
            // can show the exclusion zone changing an answer.
            Method placementChunk = c("eko").getDeclaredMethod("a", c("dva"), int.class, int.class);
            placementChunk.setAccessible(true);
            Method admittedChunk = c("eko").getMethod("b", c("dva"), int.class, int.class);
            int suppressed = 0;
            for (int x = -96; x <= 96; x++) {
                for (int z = -96; z <= 96; z++) {
                    boolean placement = (boolean) placementChunk.invoke(pillager, state, x, z);
                    boolean admitted = (boolean) admittedChunk.invoke(pillager, state, x, z);
                    if (!placement && !admitted) continue;
                    rows.add("EXCL\t" + seed + "\t" + x + "\t" + z + "\t"
                        + (placement ? 1 : 0) + "\t" + (admitted ? 1 : 0));
                    if (placement && !admitted) suppressed++;
                }
            }
            if (suppressed == 0) {
                throw new AssertionError("no exclusion-zone-suppressed chunk for seed " + seed);
            }
            // Stronghold rings: the port does not place them yet, but the fixture still has
            // to show which chunks the native ring search admits, so the port's refusal is a
            // recorded unsupported arm rather than an unknown.
            Object strongholds = null;
            for (int index : order) {
                if (setIds.get(index).equals("minecraft:strongholds")) strongholds = placements.get(index);
            }
            if (strongholds != null) {
                int ringChunks = 0;
                for (int x = -256; x <= 256 && ringChunks < 12; x += 1) {
                    for (int z = -256; z <= 256 && ringChunks < 12; z += 1) {
                        if ((boolean) admittedChunk.invoke(strongholds, state, x, z)) {
                            rows.add("RING\t" + seed + "\t" + x + "\t" + z);
                            ringChunks++;
                        }
                    }
                }
                if (ringChunks == 0) throw new AssertionError("no stronghold ring chunk found");
            }
        }
        Files.write(Path.of(args[0]), rows);
        System.out.println("beardifier pipeline vectors: " + rows.size());
    }
}
