package dev.mcspine.tools;

import com.google.gson.JsonParser;
import com.mojang.serialization.Codec;
import com.mojang.serialization.JsonOps;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.lang.reflect.*;
import java.util.*;

/**
 * One reflective level for running an actual native feature against a scripted world.
 *
 * The point is to let the native class compute, not to re-implement it. Block reads
 * come from a scene; everything the native class can derive on its own is derived by
 * running the class's own default method bodies bound to this proxy, and the build
 * height answers come from the interface's own factory (`dcy.e(minY, height)`).
 * An abstract call the harness does not serve throws, so an oracle cannot silently
 * compare against a half-modelled level.
 *
 * Block reads are served for `a_(jd)` -> BlockState. The two BlockPos predicates are
 * not guessed by name: the height accessor is any interface with no getBlockState,
 * the block predicate any interface with one.
 */
public final class NativeHarness {
    public static final int MIN_Y = -64;
    public static final int HEIGHT = 384;

    public interface Scene {
        /** Block id at this position, e.g. "minecraft:stone"; null means air. */
        String blockAt(int x, int y, int z);
    }

    static final Map<String, Object> STATES = new HashMap<>();
    static Class<?> stateC, posC, rlC, worldC, heightC;

    static Class<?> C(String name) {
        try {
            return Class.forName(name);
        } catch (ClassNotFoundException e) {
            throw new AssertionError(e);
        }
    }

    static void bootstrap() throws Exception {
        if (stateC != null) return;
        C("ab").getMethod("a").invoke(null);
        C("akt").getMethod("a").invoke(null);
        stateC = C("dtc");
        posC = C("jd");
        rlC = C("akr");
        worldC = C("dds");
        heightC = C("dcy");
        STATES.clear();
    }

    /** BlockState for a block id, cached: `Blocks.X.defaultBlockState()`. */
    public static Object state(String block) {
        String id = block.startsWith("minecraft:") ? block : "minecraft:" + block;
        Object cached = STATES.get(id);
        if (cached != null) return cached;
        try {
            Object rl = rlC.getMethod("a", String.class).invoke(null, id);
            Object blockObj = C("jz").getMethod("a", rlC)
                    .invoke(C("lt").getField("e").get(null), rl);
            if (blockObj == null) throw new AssertionError("unknown block " + id);
            Object st = C("dfy").getMethod("o").invoke(blockObj);
            STATES.put(id, st);
            return st;
        } catch (ReflectiveOperationException e) {
            throw new AssertionError(e);
        }
    }

    /** Registry name of a BlockState's block. */
    public static String nameOf(Object st) {
        try {
            Object block = stateC.getMethod("b").invoke(st);
            Object rl = C("lt").getField("e").get(null).getClass()
                    .getMethod("b", Object.class).invoke(C("lt").getField("e").get(null), block);
            return String.valueOf(rl);
        } catch (ReflectiveOperationException e) {
            throw new AssertionError(e);
        }
    }

    // ---------- native objects ----------

    static Object featureRegistry() throws Exception {
        for (Field f : C("lt").getFields()) {
            if (!Modifier.isStatic(f.getModifiers())) continue;
            Object v;
            try {
                v = f.get(null);
            } catch (Throwable t) {
                continue;
            }
            if (v == null) continue;
            for (Method m : v.getClass().getMethods()) {
                if (!m.getName().equals("a") || m.getParameterCount() != 1
                        || m.getParameterTypes()[0] != rlC) continue;
                try {
                    Object a = m.invoke(v, newId("minecraft:ore"));
                    Object b = m.invoke(v, newId("minecraft:huge_fungus"));
                    if (a != null && b != null && a.getClass() != b.getClass()
                            && a.getClass().getSuperclass() == b.getClass().getSuperclass()) {
                        return v;
                    }
                } catch (Throwable t) {
                    // not the feature registry
                }
            }
        }
        throw new AssertionError("FEATURE registry not identified");
    }

    static Object newId(String id) throws Exception {
        return rlC.getMethod("a", String.class).invoke(null, id);
    }

    static Object feature(String type) throws Exception {
        Object reg = featureRegistry();
        for (Method m : reg.getClass().getMethods()) {
            if (m.getName().equals("a") && m.getParameterCount() == 1
                    && m.getParameterTypes()[0] == rlC) {
                Object f = m.invoke(reg, newId("minecraft:" + type));
                if (f == null) throw new AssertionError("feature type not registered: " + type);
                return f;
            }
        }
        throw new AssertionError("no registry lookup");
    }

    /** The registered feature class, so an oracle can assert it is the class it read. */
    public static Class<?> featureClass(String type) throws Exception {
        return feature(type).getClass();
    }

    /** Config class from the feature's own generic superclass parameter. */
    static Class<?> configClass(Object feature) {
        for (Class<?> c = feature.getClass(); c != null; c = c.getSuperclass()) {
            Type t = c.getGenericSuperclass();
            if (t instanceof ParameterizedType p && p.getActualTypeArguments().length == 1) {
                return (Class<?>) p.getActualTypeArguments()[0];
            }
        }
        throw new AssertionError("no config type parameter on " + feature.getClass());
    }

    /** Parse the datapack config JSON with the config class's own codec. */
    public static Object config(String type, String json) throws Exception {
        Object feature = feature(type);
        Class<?> cfg = configClass(feature);
        Codec<?> codec = null;
        for (Field f : cfg.getFields()) {
            if (Modifier.isStatic(f.getModifiers()) && Codec.class.isAssignableFrom(f.getType())) {
                codec = (Codec<?>) f.get(null);
                break;
            }
        }
        if (codec == null) throw new AssertionError("no codec on " + cfg);
        Object element = JsonParser.parseString(json);
        Class<?> ops = Class.forName("com.mojang.serialization.DynamicOps");
        Object result = null;
        for (Method m : Codec.class.getMethods()) {
            if (!m.getName().equals("parse") || m.getParameterCount() != 2) continue;
            if (!m.getParameterTypes()[0].isAssignableFrom(JsonOps.class)) continue;
            try {
                result = m.invoke(codec, JsonOps.INSTANCE, element);
                break;
            } catch (IllegalArgumentException e) {
                // try the next overload
            }
        }
        if (result == null) throw new AssertionError("codec parse(DynamicOps, T) not found");
        Object cfgInstance;
        try {
            cfgInstance = result.getClass().getMethod("getOrThrow").invoke(result);
        } catch (NoSuchMethodException e) {
            cfgInstance = result.getClass()
                    .getMethod("getOrThrow", java.util.function.Function.class)
                    .invoke(result, (java.util.function.Function<String, IllegalStateException>)
                            msg -> new IllegalStateException(msg));
        }
        return cfgInstance;
    }

    public static Object rng(long seed) throws Exception {
        return C("dzx").getConstructor(C("ayw"))
                .newInstance(C("dzz").getConstructor(long.class).newInstance(seed));
    }

    /** The RNG value other fixtures record: the stream position after the call. */
    public static long rngState(Object rng) throws Exception {
        return (long) C("ayw").getMethod("g").invoke(rng);
    }

    // ---------- the level ----------

    public static final class Level {
        public final Object proxy;
        public final Map<String, Object> writes = new LinkedHashMap<>();
        public final List<String> log = new ArrayList<>();
        public final Scene scene;
        public final int minY, height;
        private final Object heights;

        Level(Scene scene, int minY, int height, Object extraGenerator) {
            this.scene = scene;
            this.minY = minY;
            this.height = height;
            try {
                this.heights = heightC.getMethod("e", int.class, int.class).invoke(null, minY, height);
                this.proxy = Proxy.newProxyInstance(NativeHarness.class.getClassLoader(),
                        new Class<?>[]{worldC}, this::handle);
            } catch (ReflectiveOperationException e) {
                throw new AssertionError(e);
            }
        }

        public Set<String> writeKeys() {
            return writes.keySet();
        }

        private boolean blockFamily(Method m) {
            return declaresBlockState(m.getDeclaringClass());
        }

        private Object handle(Object proxy, Method m, Object[] a) throws Throwable {
            Class<?>[] pt = m.getParameterTypes();
            // getBlockState: the one read the harness supplies.
            if (m.getName().equals("a_") && pt.length == 1 && pt[0] == posC
                    && m.getReturnType() == stateC) {
                int[] p = xyz(a[0]);
                String block = scene.blockAt(p[0], p[1], p[2]);
                return state(block == null ? "minecraft:air" : block);
            }
            // setBlock(pos, state, flags) must be recorded.
            if (m.getName().equals("a") && pt.length == 3 && pt[0] == posC && pt[1] == stateC
                    && m.getReturnType() == boolean.class) {
                int[] p = xyz(a[0]);
                log.add("set " + p[0] + "," + p[1] + "," + p[2] + " " + nameOf(a[1]));
                if (p[1] < minY || p[1] >= minY + height) return false;
                writes.put(p[0] + "," + p[1] + "," + p[2], a[1]);
                return true;
            }
            // Height family: run the interface's own implementation.
            if (!blockFamily(m)) {
                log.add("height " + m.getName());
                return m.invoke(heights, a);
            }
            // A default body the native class can compute itself, bound to this proxy.
            if (m.isDefault()) {
                log.add("default " + m.getName());
                return MethodHandles.privateLookupIn(m.getDeclaringClass(), MethodHandles.lookup())
                        .unreflectSpecial(m, m.getDeclaringClass())
                        .bindTo(proxy)
                        .invokeWithArguments(a == null ? new Object[0] : a);
            }
            log.add("unsupported " + m);
            throw new UnsupportedOperationException(m.toString());
        }
    }

    public static Level level(Scene scene) {
        return new Level(scene, MIN_Y, HEIGHT, null);
    }

    static boolean declaresBlockState(Class<?> decl) {
        return declaresBlockState(decl, new HashSet<>());
    }

    static boolean declaresBlockState(Class<?> decl, Set<Class<?>> seen) {
        if (decl == null || !seen.add(decl)) return false;
        for (Method d : decl.getDeclaredMethods()) {
            if (d.getName().equals("a_") && d.getParameterCount() == 1
                    && d.getParameterTypes()[0].getName().equals("jd")
                    && d.getReturnType().getName().equals("dtc")) {
                return true;
            }
        }
        for (Class<?> i : decl.getInterfaces()) {
            if (declaresBlockState(i, seen)) return true;
        }
        return declaresBlockState(decl.getSuperclass(), seen);
    }

    static int[] xyz(Object pos) {
        try {
            return new int[]{(int) posC.getMethod("u").invoke(pos),
                    (int) posC.getMethod("v").invoke(pos),
                    (int) posC.getMethod("w").invoke(pos)};
        } catch (ReflectiveOperationException e) {
            throw new AssertionError(e);
        }
    }

    public static Object pos(int x, int y, int z) throws Exception {
        bootstrap();
        return posC.getConstructor(int.class, int.class, int.class).newInstance(x, y, z);
    }

    /** Run the registered feature for `type` at the origin. */
    public static boolean place(String type, Level level, Object rng, int x, int y, int z,
                                String configJson) throws Exception {
        Object feature = feature(type);
        Object cfg = config(type, configJson);
        Constructor<?> ctxCtor = null;
        for (Constructor<?> c : C("ecg").getConstructors()) {
            Class<?>[] pt = c.getParameterTypes();
            if (pt.length == 6 && pt[0] == Optional.class && pt[1] == worldC
                    && pt[3] == C("ayw") && pt[4] == posC && pt[5].isInstance(cfg)) {
                ctxCtor = c;
                break;
            }
        }
        if (ctxCtor == null) throw new AssertionError("FeaturePlaceContext constructor not found");
        Object ctx = ctxCtor.newInstance(Optional.empty(), level.proxy, null, rng, pos(x, y, z), cfg);
        Method place = null;
        for (Class<?> c = feature.getClass(); c != null && place == null; c = c.getSuperclass()) {
            for (Method m : c.getDeclaredMethods()) {
                if (m.getName().equals("a") && m.getParameterCount() == 1
                        && m.getParameterTypes()[0].getName().equals("ecg")) {
                    place = m;
                    break;
                }
            }
        }
        if (place == null) throw new AssertionError("place method not found");
        place.setAccessible(true);
        return (boolean) place.invoke(feature, ctx);
    }
}
