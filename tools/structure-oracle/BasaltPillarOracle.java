package dev.megu.tools;

import com.mojang.serialization.Codec;
import java.lang.reflect.*;
import java.nio.charset.StandardCharsets;
import java.nio.file.*;
import java.security.MessageDigest;
import java.util.*;

/**
 * Actual 1.21.1 basalt_pillar (`ebj`, BasaltPillarFeature) over controlled columns.
 *
 * The class is identified from `BuiltInRegistries.FEATURE` (see
 * tools/feature_class_map.py), not from the name of the decompiled file. Every
 * level call is served by a proxy, and the run asserts which interface the two
 * boolean BlockPos calls belong to, so "the second one is isOutsideBuildHeight"
 * is checked against the class shape instead of assumed:
 *
 *  - the block predicate is declared by the interface that also declares
 *    getBlockState (BlockGetter),
 *  - the other one is declared by an interface that declares no getBlockState,
 *    exactly the height accessors (getHeight/getMinBuildHeight/getMaxBuildHeight
 *    and their int forms), and exactly one boolean BlockPos method.
 */
public final class BasaltPillarOracle {
    static final int MIN_Y = -64;
    static final int HEIGHT = 384;

    static Class<?> cls(String name) throws Exception { return Class.forName(name); }

    static Object block(String name) throws Exception {
        Object id = cls("akr").getMethod("a", String.class).invoke(null, "minecraft:" + name);
        Object block = cls("jz").getMethod("a", cls("akr")).invoke(cls("lt").getField("e").get(null), id);
        return cls("dfy").getMethod("o").invoke(block);
    }

    static boolean inside(int y) { return y >= MIN_Y && y < MIN_Y + HEIGHT; }

    /**
     * Scene terrain around the origin at y=64. The native check reads the cell
     * *above* the origin (`jd.d()` is above, measured by tools/pos_offsets probe),
     * so a basalt pillar hangs from a ceiling and grows downward.
     */
    static boolean solid(int scene, int dx, int dy, int dz) {
        switch (scene) {
            case 0: // ceiling one cell up, floor five below
                return dy == 1 || dy <= -5;
            case 1: // ceiling one cell up, deep cave below
                return dy == 1 || dy <= -24;
            case 2: // the cell above the origin is air: the feature must refuse
                return dy <= -5;
            case 3: // the origin itself is solid: the feature must refuse
                return dy == 0 || dy == 1 || dy <= -5;
            case 4: // ceiling plus side ledges, so hang-off writes land on air and rock
                if (dy == 1 || dy <= -5) return true;
                if (Math.abs(dx) == 1 && dz == 0 && dy % 3 == 0) return true;
                if (Math.abs(dz) == 1 && dx == 0 && dy % 2 == 0) return true;
                return false;
            case 5: // ceiling only: the column must stop at the build height floor
                return dy == 1;
            case 6: // ceiling plus a stepped base under the 7x7 patch
                if (dy == 1) return true;
                return dy <= -1 - ((dx * dx + dz * dz) % 3);
            default:
                throw new AssertionError("unknown scene " + scene);
        }
    }

    public static void main(String[] args) throws Exception {
        cls("ab").getMethod("a").invoke(null);
        cls("akt").getMethod("a").invoke(null);
        Class<?> pos = cls("jd"), state = cls("dtc"), world = cls("dds"), random = cls("ayw");
        Object codec = cls("eep").getField("a").get(null), cfg = cls("eep").getField("b").get(null);
        Object feature = cls("ebj").getConstructor(Codec.class).newInstance(codec);
        Method place = cls("ebj").getMethod("a", cls("ecg"));
        Constructor<?> context = cls("ecg").getConstructor(Optional.class, world, cls("duz"), random, pos, cls("eei"));
        Method px = pos.getMethod("u"), py = pos.getMethod("v"), pz = pos.getMethod("w");
        Object basalt = block("basalt"), netherrack = block("netherrack"), air = block("air");

        Map<String, Method> levelCalls = new TreeMap<>();
        List<String> rows = new ArrayList<>();
        List<String> cellDump = new ArrayList<>();
        for (int scene = 0; scene < 7; scene++) {
            for (long seed : new long[]{0, 1, -1, 42, 60007, Long.MIN_VALUE, Long.MAX_VALUE}) {
                final int sc = scene;
                Map<String, Object> writes = new TreeMap<>();
                Object level = Proxy.newProxyInstance(BasaltPillarOracle.class.getClassLoader(),
                        new Class<?>[]{world}, (proxy, m, a) -> {
                            String name = m.getName();
                            levelCalls.put(m.toString(), m);
                            int x = 0, y = 0, z = 0;
                            if (a != null && a.length >= 1 && pos.isInstance(a[0])) {
                                x = (int) px.invoke(a[0]);
                                y = (int) py.invoke(a[0]);
                                z = (int) pz.invoke(a[0]);
                            }
                            if (m.getReturnType() == boolean.class && a != null && a.length == 1
                                    && pos.isInstance(a[0])) {
                                // Two predicates only. Which is which is decided by the
                                // declaring interface's shape, not by its obfuscated name:
                                // the block test lives next to getBlockState, the height
                                // test lives with the int build-height getters.
                                boolean blockTest = declaresBlockState(m.getDeclaringClass());
                                if (blockTest) {
                                    if (Math.abs(x) > 12 || Math.abs(z) > 12 || y < MIN_Y - 8 || y > 96)
                                        throw new AssertionError("read outside controlled world " + x + "," + y + "," + z);
                                    return !solid(sc, x, y - 64, z);
                                }
                                assertHeightAccessor(m);
                                return !inside(y);
                            }
                            if (name.equals("a") && a.length == 3 && pos.isInstance(a[0])
                                    && state.isInstance(a[1])) {
                                if (!a[1].equals(basalt)) throw new AssertionError("non-basalt write");
                                if (!a[2].equals(2)) throw new AssertionError("unexpected update flag");
                                if (!inside(y)) return false;
                                if (Math.abs(x) > 12 || Math.abs(z) > 12 || y < MIN_Y || y > 96)
                                    throw new AssertionError("write outside controlled world " + x + "," + y + "," + z);
                                writes.put(x + "," + y + "," + z, a[1]);
                                return true;
                            }
                            throw new UnsupportedOperationException(m.toString());
                        });
                Object rng = cls("dzx").getConstructor(random)
                        .newInstance(cls("dzz").getConstructor(long.class).newInstance(seed));
                boolean placed = (boolean) place.invoke(feature, context.newInstance(Optional.empty(), level,
                        null, rng, pos.getConstructor(int.class, int.class, int.class).newInstance(0, 64, 0), cfg));
                List<String> cells = new ArrayList<>();
                for (String key : writes.keySet()) {
                    String[] p = key.split(",");
                    cells.add(p[0] + "," + (Integer.parseInt(p[1]) - 64) + "," + p[2] + "\n");
                }
                Collections.sort(cells);
                String digest = HexFormat.of().formatHex(MessageDigest.getInstance("MD5")
                        .digest(String.join("", cells).getBytes(StandardCharsets.UTF_8)));
                rows.add(scene + "\t" + seed + "\t" + placed + "\t" + writes.size() + "\t"
                        + random.getMethod("g").invoke(rng) + "\t" + digest);
                if (args.length > 1) {
                    for (String cell : cells) {
                        cellDump.add(scene + "\t" + seed + "\t" + cell.trim());
                    }
                }
            }
        }
        Files.write(Path.of(args[0]), rows);
        if (args.length > 1) {
            Files.write(Path.of(args[1]), cellDump);
        }
        System.out.println("native basalt_pillar cases: " + rows.size());
    }

    /**
     * True when `decl` or any interface it extends declares getBlockState
     * (`a_(jd)` -> `dtc`). That family is the block-reader one, so a boolean
     * BlockPos method declared there is a block predicate (isEmptyBlock).
     */
    private static boolean declaresBlockState(Class<?> decl) {
        return declaresBlockState(decl, new HashSet<>());
    }

    private static boolean declaresBlockState(Class<?> decl, Set<Class<?>> seen) {
        if (decl == null || !seen.add(decl)) return false;
        Class<?> state, pos;
        try {
            state = Class.forName("dtc");
            pos = Class.forName("jd");
        } catch (ClassNotFoundException e) {
            throw new AssertionError(e);
        }
        for (Method d : decl.getDeclaredMethods()) {
            if (d.getName().equals("a_") && d.getParameterCount() == 1
                    && d.getParameterTypes()[0] == pos && d.getReturnType() == state) {
                return true;
            }
        }
        for (Class<?> i : decl.getInterfaces()) {
            if (declaresBlockState(i, seen)) return true;
        }
        return declaresBlockState(decl.getSuperclass(), seen);
    }

    /**
     * `LevelHeightAccessor` shape: no getBlockState, at least three `int` getters,
     * and exactly two boolean methods (one for an int, one for a BlockPos) — so the
     * BlockPos one is isOutsideBuildHeight.
     */
    private static void assertHeightAccessor(Method m) {
        Class<?> decl = m.getDeclaringClass();
        if (declaresBlockState(decl)) {
            throw new AssertionError("height predicate shares an interface with getBlockState: " + decl);
        }
        int booleanMethods = 0;
        int intGetters = 0;
        boolean hasIntForm = false;
        for (Method d : decl.getDeclaredMethods()) {
            if (d.getReturnType() == boolean.class) {
                booleanMethods++;
                if (d.getParameterCount() == 1 && d.getParameterTypes()[0] == int.class) hasIntForm = true;
                if (d.getParameterCount() == 0) {
                    throw new AssertionError("height accessor gained a boolean getter: " + d);
                }
            }
            if (d.getReturnType() == int.class && d.getParameterCount() == 0) intGetters++;
        }
        if (booleanMethods != 2 || !hasIntForm || intGetters < 3) {
            throw new AssertionError("height accessor shape changed: " + decl
                    + " boolean=" + booleanMethods + " intGetters=" + intGetters + " intForm=" + hasIntForm);
        }
    }
}
