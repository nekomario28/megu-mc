package dev.mcspine.tools;

import com.google.gson.JsonParser;
import com.mojang.serialization.Codec;
import com.mojang.serialization.JsonOps;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Path;
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

    /**
     * The same scene at block-state granularity.
     *
     * A block id cannot carry properties, and several features read them: a scene that must
     * hold `pointed_dripstone[thickness=tip]` under a feature is not the block's default
     * state. Kept as a separate interface so the string-returning scenes stay unambiguous.
     */
    public interface StateScene {
        /** BlockState at this position; null means air. */
        Object stateAt(int x, int y, int z);
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
        String tags = System.getenv("MC_NATIVE_BLOCK_TAGS");
        if (tags != null && !tags.isEmpty()) bindBlockTags(Path.of(tags));
        // Fluid tags are not optional trivia: `LevelReader.isWaterAt` is
        // `getFluidState(pos).is(FluidTags.WATER)`, so without this binding water is not
        // water and every `waterlogged` property a feature derives comes out false - the
        // dripstone tip wrote `waterlogged=false` into a water cell until this existed.
        String fluidTags = System.getenv("MC_NATIVE_FLUID_TAGS");
        if (fluidTags != null && !fluidTags.isEmpty()) bindFluidTags(Path.of(fluidTags));
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

    /** Full state string (`block[prop=value,...]`), for fixtures that compare properties. */
    public static String stateName(Object st) {
        return String.valueOf(st);
    }

    /**
     * `minecraft:block[prop=value,...]` with the properties sorted by name.
     *
     * `BlockState.toString()` is the native object's own rendering (`Block{...}[...]`);
     * a portable comparison needs one spelling both sides can produce, so the fixture
     * uses this canonical form and sorts the properties.
     */
    public static String normalizedStateName(Object st) throws Exception {
        String name = nameOf(st);
        // `duf` is `properties.Property` and `dte` is `StateHolder`. Both lookups are by
        // name because the shape heuristic this replaced was wrong twice over: it took
        // `Property.toString()` as the property name ("dtt{name=down, ...}") and the
        // first one-argument method it met as the value, which is `hasProperty` — so
        // every property read `true`. A fixture built on that compared presence, not
        // values, and a name-only fixture would have looked the same.
        Class<?> propC = C("duf");
        Method propName = propC.getMethod("f"); // Property.getName()
        Method value = C("dte").getMethod("c", propC); // StateHolder.getValue(Property)
        Object props = C("dte").getMethod("B").invoke(st); // StateHolder.getProperties()
        if (!(props instanceof java.util.Collection<?> c) || c.isEmpty()) return name;
        java.util.TreeMap<String, String> sorted = new java.util.TreeMap<>();
        for (Object prop : c) {
            sorted.put(String.valueOf(propName.invoke(prop)), String.valueOf(value.invoke(st, prop)));
        }
        StringBuilder sb = new StringBuilder(name).append('[');
        boolean firstEntry = true;
        for (var e : sorted.entrySet()) {
            if (!firstEntry) sb.append(',');
            sb.append(e.getKey()).append('=').append(e.getValue());
            firstEntry = false;
        }
        return sb.append(']').toString();
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

    /**
     * Bind the pinned pack's block tags, as the real data pack would.
     *
     * Without this every `BlockState.is(TagKey)` is false in a bare bootstrap, which
     * would make tag-gated features refuse for a reason that does not exist in game.
     * The file is `tag<TAB>block,block,...`; nested `#tag` entries are expanded by
     * the runner from the same datapack the port reads.
     */
    public static void bindBlockTags(Path file) throws Exception {
        Object reg = C("lt").getField("e").get(null);
        Method wrap = null, bind = null;
        for (Method m : reg.getClass().getMethods()) {
            if (m.getParameterCount() == 1 && m.getReturnType().getName().length() == 2
                    && m.getReturnType() != void.class && wrap == null) {
                wrap = m;
            }
            if (m.getParameterCount() == 1 && m.getReturnType() == void.class
                    && Map.class.isAssignableFrom(m.getParameterTypes()[0])) {
                bind = m;
            }
        }
        if (wrap == null || bind == null) throw new AssertionError("tag binding API not found");
        Map<String, Object> tagKeys = new HashMap<>();
        for (java.lang.reflect.Field f : C("awe").getFields()) {
            if (!Modifier.isStatic(f.getModifiers())) continue;
            Object v = f.get(null);
            if (v == null) continue;
            for (Method m : v.getClass().getMethods()) {
                if (m.getParameterCount() == 0 && m.getReturnType().getName().equals("akr")) {
                    tagKeys.put(String.valueOf(m.invoke(v)), v);
                    break;
                }
            }
        }
        Map<Object, List<Object>> bound = new HashMap<>();
        for (String line : Files.readAllLines(file, StandardCharsets.UTF_8)) {
            if (line.isBlank()) continue;
            String[] parts = line.split("\t");
            Object tagKey = tagKeys.get(parts[0]);
            if (tagKey == null) continue; // a tag the vanilla BlockTags class does not name
            List<Object> holders = new ArrayList<>();
            if (parts.length > 1 && !parts[1].isBlank()) {
                for (String blockId : parts[1].split(",")) {
                    Object rl = newId(blockId);
                    Object block = C("jz").getMethod("a", rlC).invoke(reg, rl);
                    if (block == null) throw new AssertionError("tag member is not a block: " + blockId);
                    holders.add(wrap.invoke(reg, block));
                }
            }
            bound.put(tagKey, holders);
        }
        bind.invoke(reg, bound);
        System.err.println("DBG bound " + bound.size() + " block tags; nylium has "
                + bound.get(tagKeys.get("minecraft:nylium")).size() + " members");
    }

    /**
     * Bind the pinned jar's fluid tags (`water`, `lava`), as the tag loader would.
     *
     * Same shape of binding as {@link #bindBlockTags}: the registry's own
     * `wrapAsHolder`/`bindTags` pair is found by shape, and the tag keys come from the
     * `FluidTags` class fields so a renamed tag is a loud miss rather than a silent empty
     * set. The file is `tag<TAB>fluid,fluid,...`.
     */
    public static void bindFluidTags(Path file) throws Exception {
        Object reg = C("lt").getField("c").get(null); // BuiltInRegistries.FLUID
        Method wrap = null, bind = null;
        for (Method m : reg.getClass().getMethods()) {
            if (m.getParameterCount() == 1 && m.getReturnType().getName().length() == 2
                    && m.getReturnType() != void.class && wrap == null) {
                wrap = m;
            }
            if (m.getParameterCount() == 1 && m.getReturnType() == void.class
                    && Map.class.isAssignableFrom(m.getParameterTypes()[0])) {
                bind = m;
            }
        }
        if (wrap == null || bind == null) throw new AssertionError("fluid tag binding API not found");
        Map<String, Object> tagKeys = new HashMap<>();
        for (Field f : C("awk").getFields()) { // net.minecraft.tags.FluidTags
            if (!Modifier.isStatic(f.getModifiers())) continue;
            Object v = f.get(null);
            if (v == null) continue;
            for (Method m : v.getClass().getMethods()) {
                if (m.getParameterCount() == 0 && m.getReturnType().getName().equals("akr")) {
                    tagKeys.put(String.valueOf(m.invoke(v)), v);
                    break;
                }
            }
        }
        Map<Object, List<Object>> bound = new HashMap<>();
        for (String line : Files.readAllLines(file, StandardCharsets.UTF_8)) {
            if (line.isBlank()) continue;
            String[] parts = line.split("\t");
            Object tagKey = tagKeys.get(parts[0]);
            if (tagKey == null) continue;
            List<Object> holders = new ArrayList<>();
            if (parts.length > 1 && !parts[1].isBlank()) {
                for (String fluidId : parts[1].split(",")) {
                    Object rl = newId(fluidId);
                    Object fluid = reg.getClass().getMethod("a", rlC).invoke(reg, rl);
                    if (fluid == null) throw new AssertionError("tag member is not a fluid: " + fluidId);
                    holders.add(wrap.invoke(reg, fluid));
                }
            }
            bound.put(tagKey, holders);
        }
        bind.invoke(reg, bound);
        System.err.println("DBG bound " + bound.size() + " fluid tags");
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
        return getOrThrow(parse(codec, JsonParser.parseString(json)));
    }

    /**
     * Decode a datapack block state (`{"Name": ..., "Properties": {...}}`) with the native
     * `BlockState.CODEC`, so a scene can hold a non-default state without this harness
     * guessing how a property's value is spelled.
     */
    public static Object stateFromJson(com.google.gson.JsonElement json) throws Exception {
        return getOrThrow(parse((Codec<?>) stateC.getField("b").get(null), json));
    }

    /**
     * `Codec.parse(DynamicOps, T)`; the overload is picked by assignability.
     *
     * The ops are **registry-backed** (`RegistryOps` over `BuiltInRegistries.REGISTRY`). A
     * plain `JsonOps` decodes most configs but not the ones whose codecs resolve block or
     * biome references: `huge_fungus`'s `replaceable_blocks` (`matching_blocks`) failed with
     * "Can't access registry minecraft:block" until this existed. The bootstrap the harness
     * already runs is what fills that registry, so the provider is available here.
     */
    static Object parse(Codec<?> codec, com.google.gson.JsonElement element) throws Exception {
        Object ops = ops();
        Object result = null;
        for (Method m : Codec.class.getMethods()) {
            if (!m.getName().equals("parse") || m.getParameterCount() != 2) continue;
            if (!m.getParameterTypes()[0].isAssignableFrom(ops.getClass())) continue;
            try {
                result = m.invoke(codec, ops, element);
                break;
            } catch (IllegalArgumentException e) {
                // try the next overload
            }
        }
        if (result == null) throw new AssertionError("codec parse(DynamicOps, T) not found");
        return result;
    }

    static Object OPS;

    /** `RegistryOps.create(JsonOps.INSTANCE, RegistryAccess.fromRegistryOfRegistries(REGISTRY))`. */
    static Object ops() throws Exception {
        if (OPS != null) return OPS;
        Object registry = C("lt").getField("aA").get(null); // BuiltInRegistries.REGISTRY
        // `ka` = RegistryAccess, `jz` = Registry, `akp` = RegistryOps, `jo$a` = HolderLookup$Provider
        Object access = C("ka").getMethod("a", C("jz")).invoke(null, registry);
        OPS = C("akp")
                .getMethod("a", Class.forName("com.mojang.serialization.DynamicOps"), C("jo$a"))
                .invoke(null, JsonOps.INSTANCE, access);
        return OPS;
    }

    static Object getOrThrow(Object result) throws Exception {
        try {
            return result.getClass().getMethod("getOrThrow").invoke(result);
        } catch (NoSuchMethodException e) {
            return result.getClass()
                    .getMethod("getOrThrow", java.util.function.Function.class)
                    .invoke(result, (java.util.function.Function<String, IllegalStateException>)
                            msg -> new IllegalStateException(msg));
        }
    }

    /** Parse a state-provider JSON with its own codec (probe helper). */
    public static Object provider(String json) throws Exception {
        Class<?> type = C("egj"); // StateProvider
        Object element = JsonParser.parseString(json);
        for (java.lang.reflect.Field f : type.getFields()) {
            if (Modifier.isStatic(f.getModifiers()) && Codec.class.isAssignableFrom(f.getType())) {
                Codec<?> codec = (Codec<?>) f.get(null);
                Object result = null;
                for (Method m : Codec.class.getMethods()) {
                    if (!m.getName().equals("parse") || m.getParameterCount() != 2) continue;
                    if (!m.getParameterTypes()[0].isAssignableFrom(JsonOps.class)) continue;
                    try {
                        result = m.invoke(codec, JsonOps.INSTANCE, element);
                        break;
                    } catch (IllegalArgumentException e) {
                        // next overload
                    }
                }
                return result.getClass().getMethod("getOrThrow").invoke(result);
            }
        }
        throw new AssertionError("no StateProvider codec");
    }

    public static Object providerState(Object provider, Object rng, int x, int y, int z)
            throws Exception {
        return C("egj").getMethod("a", C("ayw"), C("jd"))
                .invoke(provider, rng, pos(x, y, z));
    }

    /**
     * An RNG whose next `nextInt(bound)` is the given roll: a seed search is
     * unacceptable, so the first draw is produced by an RNG we can prime. The
     * native provider draws exactly one bound, so any RNG whose stream starts at
     * that value works.
     */
    public static Object fixedRoll(int roll, int bound) throws Exception {
        for (long seed = 0; seed < 100000; seed++) {
            Object rng = rng(seed);
            if ((int) C("ayw").getMethod("a", int.class).invoke(rng, bound) == roll) {
                return rng(seed);
            }
        }
        throw new AssertionError("no seed for roll " + roll);
    }

    /** `RandomSource.nextInt(bound)` (probe helper). */
    public static int draw(Object rng, int bound) throws Exception {
        return (int) C("ayw").getMethod("a", int.class).invoke(rng, bound);
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
        public final List<String> attempts = new ArrayList<>();
        public final StateScene scene;
        public final int minY, height;
        private final Object heights;

        Level(StateScene scene, int minY, int height, Object extraGenerator) {
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
                attempts.add(p[0] + "," + p[1] + "," + p[2]);
                // Write-through: a feature that reads back a cell it just set must see
                // its own write, as a real level would. Serving the static scene instead
                // let a second attempt at the same cell overwrite it in the native run
                // while the port (which writes through) skipped it.
                Object written = writes.get(p[0] + "," + p[1] + "," + p[2]);
                if (written != null) return written;
                Object read = scene.stateAt(p[0], p[1], p[2]);
                return read == null ? state("minecraft:air") : read;
            }
            // getFluidState: a worldgen level has no flowing fluids, so the block state's own
            // fluid state is the whole answer. `Level.isWaterAt` (used by the dripstone tip's
            // waterlogged property) goes through here.
            if (m.getName().equals("b_") && pt.length == 1 && pt[0] == posC
                    && m.getReturnType() == C("epe")) {
                int[] p = xyz(a[0]);
                Object at = writes.get(p[0] + "," + p[1] + "," + p[2]);
                if (at == null) {
                    at = scene.stateAt(p[0], p[1], p[2]);
                    if (at == null) at = state("minecraft:air");
                }
                return stateC.getMethod("u").invoke(at); // BlockStateBase.getFluidState()
            }
            // `isStateAtPosition(pos, predicate)` (`ddc.a(jd, Predicate)`): the predicate is
            // applied to the state this level already serves, so no block knowledge is
            // duplicated here. `Feature.isReplaceable` reaches this through
            // `BlockState::canBeReplaced`.
            if (m.getName().equals("a") && pt.length == 2 && pt[0] == posC
                    && pt[1] == java.util.function.Predicate.class
                    && m.getReturnType() == boolean.class) {
                int[] p = xyz(a[0]);
                Object at = writes.get(p[0] + "," + p[1] + "," + p[2]);
                if (at == null) {
                    at = scene.stateAt(p[0], p[1], p[2]);
                    if (at == null) at = state("minecraft:air");
                }
                return ((java.util.function.Predicate<Object>) a[1]).test(at);
            }
            // setBlock(pos, state, flags) must be recorded.
            if (m.getName().equals("a") && pt.length == 3 && pt[0] == posC && pt[1] == stateC
                    && m.getReturnType() == boolean.class) {
                int[] p = xyz(a[0]);
                // The log is the cell-level diagnostic: a name-only line cannot show a
                // property difference (the mushroom cap's four rim properties), so it
                // prints the same canonical form the fixtures compare.
                log.add("set " + p[0] + "," + p[1] + "," + p[2] + " "
                        + normalizedStateName(a[1]));
                if (p[1] < minY || p[1] >= minY + height) return false;
                writes.put(p[0] + "," + p[1] + "," + p[2], a[1]);
                return true;
            }
            // Build height: the interface that declares the same name and signature
            // (`dcy`) has its own implementation; delegate to it. This is name-matched
            // but asserted against that interface's declared methods, and it avoids
            // running a default body from an interface that shares the obfuscated name.
            Method heightMethod = heightFamily(m);
            if (heightMethod != null) {
                log.add("height " + m.getName());
                return heightMethod.invoke(heights, a);
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
            System.err.println("harness log: " + String.join(" | ", log.subList(Math.max(0, log.size() - 12), log.size())));
            throw new UnsupportedOperationException(m.toString());
        }
    }

    public static Level level(Scene scene) {
        return levelFromStates((x, y, z) -> {
            String block = scene.blockAt(x, y, z);
            return block == null ? state("minecraft:air") : state(block);
        });
    }

    /** A level whose scene resolves properties itself (see {@link StateScene}). */
    public static Level levelFromStates(StateScene scene) {
        return new Level(scene, MIN_Y, HEIGHT, null);
    }

    /**
     * A real `NoiseBasedChunkGenerator` over vanilla's `NoiseGeneratorSettings.dummy()`.
     *
     * `dummy()` is `NoiseSettings.create(-64, 384, 1, 2)` with sea level 63, so
     * `getGenDepth()` and `getSeaLevel()` answer with vanilla's numbers for this world
     * rather than a stub. Only the biome source is `null`: neither `dummy()` nor the two
     * accessors touch it.
     */
    static Object probeGenerator() throws Exception {
        Object settings = C("dzd").getMethod("e").invoke(null);
        Object holder = C("jm").getMethod("a", Object.class).invoke(null, settings);
        return C("dzb").getConstructor(C("dea"), C("jm")).newInstance(null, holder);
    }

    /** Same name and parameter types declared by the height-accessor interface. */
    static Method heightFamily(Method m) {
        try {
            for (Method h : heightC.getDeclaredMethods()) {
                if (h.getName().equals(m.getName())
                        && java.util.Arrays.equals(h.getParameterTypes(), m.getParameterTypes())) {
                    return h;
                }
            }
        } catch (Throwable t) {
            throw new AssertionError(t);
        }
        return null;
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
        // The context's generator slot is a real one: `ChunkGenerator.getSeaLevel()` and
        // `getGenDepth()` are abstract, so a feature that reads either (basalt_columns,
        // huge_fungus) dies on a null. Vanilla's own `NoiseGeneratorSettings.dummy()` is a
        // real settings object with min y -64 / height 384 / sea level 63 (the same numbers
        // this harness's level reports), and `NoiseBasedChunkGenerator` is constructible
        // from it without registries. Subclassing `ChunkGenerator` from here is not an
        // option: the game's classes are signed, and a class we add to their package fails
        // with "signer information does not match".
        Object generator = probeGenerator();
        Object ctx = ctxCtor.newInstance(Optional.empty(), level.proxy, generator, rng,
                pos(x, y, z), cfg);
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
