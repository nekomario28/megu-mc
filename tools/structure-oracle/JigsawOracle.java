package dev.megu.tools;

import com.google.gson.*;
import java.lang.reflect.*;
import java.nio.charset.StandardCharsets;
import java.nio.file.*;
import java.security.MessageDigest;
import java.util.*;
import java.util.function.Function;
import java.util.function.Predicate;
import java.util.stream.Stream;

/**
 * Native 1.21.1 oracle for JigsawStructure / JigsawPlacement / PoolElementStructurePiece.
 * Drives the actual vanilla classes from the pinned 1.21.1 server jar.
 */
public final class JigsawOracle {
    static Class<?> c(String s) { return NativeHarness.C(s); }
    static Object f(String cl, String n) throws Exception { return c(cl).getField(n).get(null); }
    static Object registry(Object access, String n) throws Exception {
        return c("ka").getMethod("d", c("akq")).invoke(access, f("lu", n));
    }
    static Object value(Object reg, String n) throws Exception {
        Object id = c("akr").getMethod("a", String.class).invoke(null, n);
        Object v = c("jz").getMethod("a", c("akr")).invoke(reg, id);
        if (v == null) throw new AssertionError("missing " + n);
        return v;
    }
    static Object holder(Object reg, String n) throws Exception {
        Object id = c("akr").getMethod("a", String.class).invoke(null, n);
        Object key = c("akq").getMethod("a", c("akq"), c("akr")).invoke(null, c("jz").getMethod("d").invoke(reg), id);
        Optional<?> opt = (Optional<?>) c("jz").getMethod("b", c("akq")).invoke(reg, key);
        return opt.orElseThrow(() -> new AssertionError("missing holder " + n));
    }
    static void field(Object obj, Class<?> owner, String name, Object val) throws Exception {
        Field fld = owner.getDeclaredField(name);
        fld.setAccessible(true);
        fld.set(obj, val);
    }
    static Object allocate(String name) throws Exception {
        Field f = c("sun.misc.Unsafe").getDeclaredField("theUnsafe");
        f.setAccessible(true);
        Object unsafe = f.get(null);
        return unsafe.getClass().getMethod("allocateInstance", Class.class).invoke(unsafe, c(name));
    }
    static String sha256(String s) throws Exception {
        return HexFormat.of().formatHex(MessageDigest.getInstance("SHA-256").digest(s.getBytes(StandardCharsets.UTF_8)));
    }
    static void bindBiomeTags(Object biomes, Object manager) throws Exception {
        Function<Object, Object> lookup = id -> {
            try { return c("jz").getMethod("c", c("akr")).invoke(biomes, id); }
            catch (Exception e) { throw new AssertionError(e); }
        };
        Object loader = c("awv").getConstructor(Function.class, String.class).newInstance(
            lookup, c("lu").getMethod("d", c("akq")).invoke(null, f("lu", "aF")));
        Map<?, ?> loaded = (Map<?, ?>) c("awv").getMethod("b", c("aue")).invoke(loader, manager);
        Map<Object, Object> tags = new HashMap<>();
        for (var e : loaded.entrySet()) {
            tags.put(c("awu").getMethod("a", c("akq"), c("akr")).invoke(null, f("lu", "aF"), e.getKey()),
                     List.copyOf((Collection<?>) e.getValue()));
        }
        c("jz").getMethod("a", Map.class).invoke(biomes, tags);
    }
    static void bindBlockTagsFromManager(Object manager) throws Exception {
        Object blocks = f("lt", "e");
        Function<Object, Object> lookup = id -> {
            try { return c("jz").getMethod("c", c("akr")).invoke(blocks, id); }
            catch (Exception e) { throw new AssertionError(e); }
        };
        Object loader = c("awv").getConstructor(Function.class, String.class).newInstance(
            lookup, c("lu").getMethod("d", c("akq")).invoke(null, f("lu", "f")));
        Map<?, ?> loaded = (Map<?, ?>) c("awv").getMethod("b", c("aue")).invoke(loader, manager);
        Map<Object, Object> tags = new HashMap<>();
        for (var e : loaded.entrySet()) {
            tags.put(c("awu").getMethod("a", c("akq"), c("akr")).invoke(null, f("lu", "f"), e.getKey()),
                     List.copyOf((Collection<?>) e.getValue()));
        }
        c("jz").getMethod("a", Map.class).invoke(blocks, tags);
    }

    static Object loadTemplate(String id) throws Exception {
        String path = id.startsWith("minecraft:") ? id.substring(10) : id;
        try (var in = c("ent").getResourceAsStream("/data/minecraft/structure/" + path + ".nbt")) {
            if (in == null) return null; // e.g. ancient_city/walls/intact_horizontal_wall_stairs_5
            Path tmp = Files.createTempFile("jigsaw-tpl-", ".nbt");
            try {
                Files.copy(in, tmp, StandardCopyOption.REPLACE_EXISTING);
                Object root = c("uo").getMethod("a", Path.class, c("uk")).invoke(null, tmp, c("uk").getMethod("a").invoke(null));
                Object lookup = c("jz").getMethod("q").invoke(f("lt", "e"));
                Object template = c("ent").getConstructor().newInstance();
                c("ent").getMethod("a", c("jn"), c("ub")).invoke(template, lookup, root);
                return template;
            } finally {
                Files.deleteIfExists(tmp);
            }
        }
    }

    static Object buildStructureTemplateManager() throws Exception {
        Map<Object, Object> cache = new HashMap<>() {
            @Override
            public Object computeIfAbsent(Object key, Function<? super Object, ? extends Object> mappingFunction) {
                if (containsKey(key)) return get(key);
                try {
                    Object tpl = loadTemplate(key.toString());
                    Object opt = Optional.ofNullable(tpl);
                    put(key, opt);
                    return opt;
                } catch (Exception e) {
                    throw new AssertionError("Failed loading template " + key, e);
                }
            }
        };
        Object manager = allocate("enu");
        field(manager, c("enu"), "f", cache);
        return manager;
    }

    static Object serverLevelWithSeed(long worldSeed, Object finalAccess, Object dimHolder, Object biomes) throws Exception {
        Object vanillaFlags = c("cpn").getField("f").get(null);
        Object worldOptions = c("dzw").getConstructor(long.class, boolean.class, boolean.class).newInstance(worldSeed, true, false);
        Object worldData = Proxy.newProxyInstance(
            JigsawOracle.class.getClassLoader(),
            new Class<?>[]{c("erl")},
            (proxy, method, args) -> {
                if (method.getName().equals("K") && method.getParameterCount() == 0) return vanillaFlags;
                if (method.getName().equals("y") && method.getParameterCount() == 0) return worldOptions;
                throw new UnsupportedOperationException("worldData: " + method);
            });
        Class<?> serverC = Class.forName("net.minecraft.server.MinecraftServer");
        Object server = allocate("apn");
        field(server, serverC, "j", worldData);
        Object sl = allocate("aqu");
        field(sl, c("aqu"), "J", server);
        field(sl, c("dcw"), "z", c("dzz").getConstructor(long.class).newInstance(0L));
        field(sl, c("dcw"), "I", finalAccess);
        field(sl, c("dcw"), "D", dimHolder);
        Object writableData = Proxy.newProxyInstance(
            JigsawOracle.class.getClassLoader(),
            new Class<?>[]{c("erm")},
            (p, m, a) -> {
                if (m.getParameterCount() == 0 && m.getReturnType() == long.class && (m.getName().equals("c") || m.getName().equals("d"))) return 0L;
                if (m.isDefault()) return InvocationHandler.invokeDefault(p, m, a);
                throw new UnsupportedOperationException("writableLevelData: " + m);
            });
        field(sl, c("dcw"), "A", writableData);
        Map<Long, Object> smChunks = new HashMap<>();
        Object smAccessor = Proxy.newProxyInstance(
            JigsawOracle.class.getClassLoader(),
            new Class<?>[]{c("dcx")},
            (p, m, a) -> {
                Class<?>[] pt = m.getParameterTypes();
                if (pt.length == 0 && m.getName().equals("H_")) return finalAccess;
                if (m.getName().equals("a") && Arrays.equals(pt, new Class<?>[]{int.class, int.class, c("dvz"), boolean.class})) {
                    long key = ((long) (int) a[0] & 0xffffffffL) | ((long) (int) a[1] << 32);
                    return smChunks.computeIfAbsent(key, k -> {
                        try { return protoChunk((int) a[0], (int) a[1], biomes); }
                        catch (Exception e) { throw new AssertionError(e); }
                    });
                }
                if (m.isDefault()) return InvocationHandler.invokeDefault(p, m, a);
                throw new UnsupportedOperationException("smAccessor: " + m);
            });
        Object innerSm = c("ddq").getConstructor(c("dcx"), c("dzw"), c("ejs")).newInstance(smAccessor, worldOptions, null);
        field(sl, c("aqu"), "ad", innerSm);
        return sl;
    }

    static Object flatGenerator(Object biomeHolder, int topY) throws Exception {
        Object settings = c("eht").getConstructor(Optional.class, c("jm"), List.class).newInstance(
            Optional.empty(), biomeHolder, List.of());
        int thickness = topY - NativeHarness.MIN_Y;
        if (thickness > 0) {
            Object stoneBlock = value(f("lt", "e"), "minecraft:stone");
            Object layer = c("ehq").getConstructor(int.class, c("dfy")).newInstance(thickness, stoneBlock);
            @SuppressWarnings("unchecked")
            List<Object> layersInfo = (List<Object>) c("eht").getMethod("e").invoke(settings);
            layersInfo.add(layer);
            c("eht").getMethod("g").invoke(settings);
        }
        return c("dyt").getConstructor(c("eht")).newInstance(settings);
    }

    static String describeElement(Object el, Object placedFeatures) throws Exception {
        Class<?> cls = el.getClass();
        if (cls == c("eks")) return "empty";
        if (cls == c("ekw") || cls == c("eky")) {
            String prefix = cls == c("ekw") ? "legacy:" : "single:";
            Field fld = c("eky").getDeclaredField("c");
            fld.setAccessible(true);
            Object either = fld.get(el);
            Object id = Class.forName("com.mojang.datafixers.util.Either").getMethod("left").invoke(either);
            return prefix + ((Optional<?>) id).orElseThrow().toString();
        }
        if (cls == c("ekt")) {
            Field fld = c("ekt").getDeclaredField("b");
            fld.setAccessible(true);
            Object holder = fld.get(el);
            Object key = c("jm").getMethod("e").invoke(holder);
            String id = ((Optional<?>) key).map(k -> k.toString()).orElse("unknown");
            // ResourceKey[minecraft:worldgen/placed_feature / minecraft:oak] -> extract minecraft:oak
            int slash = id.indexOf(" / ");
            if (slash >= 0 && id.endsWith("]")) {
                id = id.substring(slash + 3, id.length() - 1);
            }
            return "feature:" + id;
        }
        if (cls == c("ekx")) {
            Field fld = c("ekx").getDeclaredField("b");
            fld.setAccessible(true);
            List<?> list = (List<?>) fld.get(el);
            StringBuilder sb = new StringBuilder("list:[");
            for (int i = 0; i < list.size(); i++) {
                if (i > 0) sb.append(',');
                sb.append(describeElement(list.get(i), placedFeatures));
            }
            return sb.append(']').toString();
        }
        throw new AssertionError("Unknown PoolElement class: " + cls);
    }

    static int sceneHeight(String scene, int x, int z) {
        if (scene.equals("sloped")) return 60 + Math.floorMod(x * 3 + z * 7, 9);
        if (scene.equals("nether")) return 120;
        return 64;
    }

    static Object sceneState(String scene, int x, int y, int z) throws Exception {
        int top = sceneHeight(scene, x, z);
        if (scene.equals("flat")) {
            return NativeHarness.state(y < top ? "minecraft:stone" : "minecraft:air");
        }
        if (scene.equals("grass") || scene.equals("sloped")) {
            if (y < top - 1) return NativeHarness.state("minecraft:stone");
            if (y == top - 1) return NativeHarness.state("minecraft:grass_block");
            return NativeHarness.state("minecraft:air");
        }
        if (scene.equals("flooded")) {
            if (y < 52) return NativeHarness.state("minecraft:stone");
            if (y < 64) return NativeHarness.state("minecraft:water");
            return NativeHarness.state("minecraft:air");
        }
        if (scene.equals("underground")) {
            return NativeHarness.state(y < 64 ? "minecraft:deepslate" : "minecraft:air");
        }
        if (scene.equals("nether")) {
            return NativeHarness.state(y < 120 ? "minecraft:netherrack" : "minecraft:air");
        }
        throw new AssertionError("Unknown scene: " + scene);
    }

    static Object protoChunk(int x, int z, Object biomes) throws Exception {
        return c("dvs").getConstructor(c("dcd"), c("dvv"), c("dcy"), c("jz"), c("eab")).newInstance(
            c("dcd").getConstructor(int.class, int.class).newInstance(x, z), f("dvv", "a"),
            c("dcy").getMethod("e", int.class, int.class).invoke(null, NativeHarness.MIN_Y, NativeHarness.HEIGHT),
            biomes, null);
    }

    public static void main(String[] args) throws Exception {
        if (args.length != 2) throw new IllegalArgumentException("cases-json output-tsv");
        // Block tags are not optional trivia for a structure oracle: `BushBlock.mayPlaceOn`
        // (`state.is(BlockTags.DIRT)`) decides every flower/fern/short-grass placement, and the
        // processors read tags too, so an unbound tag set silently changes what the world
        // contains while every run still "succeeds".
        //
        // Measured: with `MC_NATIVE_BLOCK_TAGS` unset this oracle wrote a 140-row fixture that
        // differed from the tagged run on 43 rows (16 ancient_city, 15 trail_ruins, 12 village
        // chunks) - the untagged artifact looked healthy and was wrong. `tools/jigsaw_oracle.py`
        // always sets it; a direct `java` invocation must fail closed instead of producing one.
        if (System.getenv("MC_NATIVE_BLOCK_TAGS") == null) {
            throw new AssertionError("MC_NATIVE_BLOCK_TAGS is required: an unbound block-tag set "
                    + "changes vegetation and processor outcomes (43 of 140 fixture rows measured); "
                    + "run tools/jigsaw_oracle.py instead of the class directly");
        }
        NativeHarness.bootstrap();
        Object staticAccess = c("ka").getMethod("a", c("jz")).invoke(null, f("lt", "aA"));
        Object manager = c("atx").getConstructor(c("ass"), List.class).newInstance(
            f("ass", "b"), List.of(c("ats").getMethod("b").invoke(null)));
        Object dynAccess = c("akm").getMethod("a", c("aue"), c("ka"), List.class).invoke(null, manager, staticAccess, f("akm", "a"));
        Object access = Proxy.newProxyInstance(
            JigsawOracle.class.getClassLoader(),
            new Class<?>[]{c("ka$b")},
            (p, m, a) -> {
                if (m.getName().equals("c") && m.getParameterCount() == 1) {
                    Optional<?> opt = (Optional<?>) c("ka").getMethod("c", c("akq")).invoke(dynAccess, a[0]);
                    return opt.isPresent() ? opt : c("ka").getMethod("c", c("akq")).invoke(staticAccess, a[0]);
                }
                if (m.getName().equals("c") && m.getParameterCount() == 0) {
                    return Stream.concat(
                        (Stream<?>) c("ka").getMethod("c").invoke(staticAccess),
                        (Stream<?>) c("ka").getMethod("c").invoke(dynAccess)
                    );
                }
                if (m.isDefault()) return InvocationHandler.invokeDefault(p, m, a);
                throw new UnsupportedOperationException("RegistryAccess: " + m);
            });
        bindBlockTagsFromManager(manager);

        Object biomes = registry(access, "aF");
        bindBiomeTags(biomes, manager);
        Object structures = registry(access, "aR");
        Object placedFeatures = registry(access, "aQ");
        Object noiseSettings = value(registry(access, "aO"), "minecraft:overworld");
        Object noises = c("jz").getMethod("q").invoke(registry(access, "aP"));
        Object biomeHolder = holder(biomes, "minecraft:plains");
        Object biomeSource = c("deh").getConstructor(c("jm")).newInstance(biomeHolder);
        Object tm = buildStructureTemplateManager();
        Object sm = c("ddq").getConstructor(c("dcx"), c("dzw"), c("ejs")).newInstance(null, null, null);
        Object heightAccessor = c("dcy").getMethod("e", int.class, int.class).invoke(null, NativeHarness.MIN_Y, NativeHarness.HEIGHT);
        Object dimReg = registry(access, "aK");
        Object dimension = c("jz").getMethod("a", c("akr")).invoke(dimReg, NativeHarness.newId("minecraft:overworld"));
        Object dimHolder = holder(dimReg, "minecraft:overworld");
        if (dimension == null) {
            throw new AssertionError("missing overworld dimension_type");
        }

        List<String> lines = new ArrayList<>();
        JsonArray cases = JsonParser.parseString(Files.readString(Path.of(args[0]))).getAsJsonArray();
        for (JsonElement elem : cases) {
            JsonObject tc = elem.getAsJsonObject();
            String id = tc.get("id").getAsString();
            String structId = tc.get("structure").getAsString();
            long seed = tc.get("seed").getAsLong();
            int sx = tc.get("cx").getAsInt();
            int sz = tc.get("cz").getAsInt();
            String scene = tc.get("scene").getAsString();
            int flatTop = tc.has("flat_top") ? tc.get("flat_top").getAsInt() : 64;
            int maxChunks = tc.has("max_chunks") ? tc.get("max_chunks").getAsInt() : 8;

            Object generator = flatGenerator(biomeHolder, flatTop);
            Object randomState = c("dzm").getMethod("a", c("dzd"), c("jn"), long.class).invoke(null, noiseSettings, noises, seed);
            Object chunkPos = c("dcd").getConstructor(int.class, int.class).newInstance(sx, sz);
            Object ctx = c("ejr$a").getConstructor(
                c("ka"), c("duz"), c("dea"), c("dzm"), c("enu"), long.class, c("dcd"), c("dcy"), Predicate.class
            ).newInstance(access, generator, biomeSource, randomState, tm, seed, chunkPos, heightAccessor, (Predicate<Object>) h -> true);

            Object structure = value(structures, structId);
            Optional<?> stubOpt = (Optional<?>) c("ejr").getMethod("b", c("ejr$a")).invoke(structure, ctx);
            if (stubOpt.isEmpty()) {
                throw new AssertionError("No generation point for " + id + " (" + structId + " seed=" + seed + ")");
            }
            Object stub = stubOpt.get();
            Object builder = c("ejr$b").getMethod("a").invoke(stub);
            Object container = c("ekj").getMethod("a").invoke(builder);
            Object start = c("ejz").getConstructor(c("ejr"), c("dcd"), int.class, c("ekg")).newInstance(structure, chunkPos, 0, container);
            if (!(boolean) c("ejz").getMethod("b").invoke(start)) {
                throw new AssertionError("Invalid StructureStart for " + id);
            }

            List<?> pieces = (List<?>) c("ejz").getMethod("i").invoke(start);
            StringBuilder piecesDesc = new StringBuilder();
            for (int i = 0; i < pieces.size(); i++) {
                Object p = pieces.get(i);
                Object el = c("ejn").getMethod("b").invoke(p);
                int[] pos = NativeHarness.xyz(c("ejn").getMethod("c").invoke(p));
                int rot = ((Enum<?>) c("ejn").getMethod("a").invoke(p)).ordinal();
                int gld = (int) c("ejn").getMethod("d").invoke(p);
                Object bb = c("ejv").getMethod("f").invoke(p);
                int minX = (int) c("ejj").getMethod("h").invoke(bb);
                int minY = (int) c("ejj").getMethod("i").invoke(bb);
                int minZ = (int) c("ejj").getMethod("j").invoke(bb);
                int maxX = (int) c("ejj").getMethod("k").invoke(bb);
                int maxY = (int) c("ejj").getMethod("l").invoke(bb);
                int maxZ = (int) c("ejj").getMethod("m").invoke(bb);
                Field lsField = c("ejn").getDeclaredField("k");
                lsField.setAccessible(true);
                int ls = ((Enum<?>) lsField.get(p)).ordinal();
                piecesDesc.append(i).append('\t')
                    .append(describeElement(el, placedFeatures)).append('\t')
                    .append(pos[0]).append(',').append(pos[1]).append(',').append(pos[2]).append('\t')
                    .append(rot).append('\t').append(gld).append('\t')
                    .append(minX).append(',').append(minY).append(',').append(minZ).append(',')
                    .append(maxX).append(',').append(maxY).append(',').append(maxZ).append('\t')
                    .append(ls).append('\n');
            }
            String piecesSha = sha256(piecesDesc.toString());
            Object firstPiece = pieces.get(0);
            String firstEl = describeElement(c("ejn").getMethod("b").invoke(firstPiece), placedFeatures);
            int firstRot = ((Enum<?>) c("ejn").getMethod("a").invoke(firstPiece)).ordinal();

            lines.add("CASE\t" + id + "\t" + structId + "\t" + seed + "\t" + sx + "\t" + sz + "\t" + scene
                + "\t" + flatTop + "\t" + pieces.size() + "\t" + piecesSha + "\t" + firstEl + "\t" + firstRot);

            // Collect covered chunks from StructureStart bounding box
            Object startBB = c("ejz").getMethod("a").invoke(start);
            int cMinX = ((int) c("ejj").getMethod("h").invoke(startBB)) >> 4;
            int cMinZ = ((int) c("ejj").getMethod("j").invoke(startBB)) >> 4;
            int cMaxX = ((int) c("ejj").getMethod("k").invoke(startBB)) >> 4;
            int cMaxZ = ((int) c("ejj").getMethod("m").invoke(startBB)) >> 4;

            // Choose chunks with intersecting pieces, prioritize origin chunk then chunks with most intersecting pieces
            List<int[]> candidateChunks = new ArrayList<>();
            for (int cx = cMinX; cx <= cMaxX; cx++) {
                for (int cz = cMinZ; cz <= cMaxZ; cz++) {
                    Object clip = c("ejj").getConstructor(int.class, int.class, int.class, int.class, int.class, int.class)
                        .newInstance(cx << 4, NativeHarness.MIN_Y, cz << 4, (cx << 4) + 15, NativeHarness.MIN_Y + NativeHarness.HEIGHT - 1, (cz << 4) + 15);
                    int hits = 0;
                    for (Object p : pieces) {
                        Object pbb = c("ejv").getMethod("f").invoke(p);
                        if ((boolean) c("ejj").getMethod("a", c("ejj")).invoke(pbb, clip)) hits++;
                    }
                    if (hits > 0) candidateChunks.add(new int[]{cx, cz, (cx == sx && cz == sz) ? 1000000 : hits});
                }
            }
            candidateChunks.sort((a, b) -> {
                if (b[2] != a[2]) return Integer.compare(b[2], a[2]);
                if (a[0] != b[0]) return Integer.compare(a[0], b[0]);
                return Integer.compare(a[1], b[1]);
            });
            if (candidateChunks.size() > maxChunks) {
                candidateChunks = candidateChunks.subList(0, maxChunks);
            }

            Object finalAccess = access;
            Object serverLevel = serverLevelWithSeed(seed, finalAccess, dimHolder, biomes);
            for (int[] ch : candidateChunks) {
                int cx = ch[0], cz = ch[1];
                NativeHarness.Level level = NativeHarness.levelFromStates((x, y, z) -> {
                    try { return sceneState(scene, x, y, z); }
                    catch (Exception e) { throw new AssertionError(e); }
                });
                Map<List<Integer>, Object> blockEntities = new HashMap<>();
                Map<Long, Object> chunks = new HashMap<>();
                Object data = Proxy.newProxyInstance(JigsawOracle.class.getClassLoader(), new Class<?>[]{c("erb")}, (p, m, a) -> {
                    if (m.getParameterCount() == 0 && m.getReturnType() == long.class && (m.getName().equals("c") || m.getName().equals("d"))) return 0L;
                    throw new UnsupportedOperationException("levelData: " + m);
                });
                Map<String, Object> fluidContainers = new TreeMap<>();
                Function<Object, Object> fluidContainer = position -> {
                    try {
                        int[] v = NativeHarness.xyz(position);
                        String ck = (v[0] >> 4) + "," + (v[2] >> 4);
                        if (!fluidContainers.containsKey(ck)) fluidContainers.put(ck, c("eyr").getConstructor().newInstance());
                        return fluidContainers.get(ck);
                    } catch (Exception e) { throw new AssertionError(e); }
                };
                Object fluidTicks = c("eyy").getConstructor(Function.class).newInstance(fluidContainer);
                Object blockTicks = c("eyy").getConstructor(Function.class).newInstance(fluidContainer);
                long[] subTick = {0};

                Object vanillaFlags = c("cpn").getField("f").get(null);
                Object difficulty = c("bqp").getConstructor(c("bqo"), long.class, long.class, float.class)
                    .newInstance(f("bqo", "c"), 0L, 0L, 0.0f);
                Object levelRandom = c("dzz").getConstructor(long.class).newInstance(0L);
                Object world = Proxy.newProxyInstance(JigsawOracle.class.getClassLoader(), new Class<?>[]{c("dds")}, (p, m, a) -> {
                    Class<?>[] pt = m.getParameterTypes();
                    if (pt.length == 0 && m.getName().equals("I_")) return NativeHarness.MIN_Y;
                    if (pt.length == 0 && m.getName().equals("J_")) return NativeHarness.HEIGHT;
                    if (pt.length == 0 && m.getName().equals("J")) return vanillaFlags;
                    if (pt.length == 0 && m.getName().equals("E")) return serverLevel;
                    if (pt.length == 0 && m.getName().equals("H_")) return finalAccess;
                    if (pt.length == 0 && m.getName().equals("C")) return seed;
                    if (pt.length == 0 && m.getName().equals("D_")) return dimension;
                    if (pt.length == 0 && m.getName().equals("A_")) return data;
                    if (pt.length == 0 && m.getName().equals("O")) return fluidTicks;
                    if (pt.length == 0 && m.getName().equals("P")) return blockTicks;
                    if (pt.length == 0 && m.getName().equals("G_")) return subTick[0]++;
                    if (pt.length == 0 && m.getName().equals("x_")) return false;
                    if (pt.length == 0 && m.getName().equals("E_")) return levelRandom;
                    if (m.getName().equals("d_") && Arrays.equals(pt, new Class<?>[]{c("jd")})) return difficulty;
                    if (m.getName().equals("t") && Arrays.equals(pt, new Class<?>[]{c("jd")})) return biomeHolder;
                    if (m.getName().equals("c_") && Arrays.equals(pt, new Class<?>[]{c("jd")})) {
                        int[] at = NativeHarness.xyz(a[0]);
                        return blockEntities.get(List.of(at[0], at[1], at[2]));
                    }
                    if (m.getName().equals("b") && Arrays.equals(pt, new Class<?>[]{c("bsr")})) {
                        return true; // discard spawned entities safely
                    }
                    if (m.getName().equals("a") && Arrays.equals(pt, new Class<?>[]{int.class, int.class, c("dvz"), boolean.class}) && m.getReturnType() == c("duy")) {
                        long key = ((long) (int) a[0] & 0xffffffffL) | ((long) (int) a[1] << 32);
                        Object target = chunks.get(key);
                        if (target == null) {
                            target = protoChunk((int) a[0], (int) a[1], biomes);
                            chunks.put(key, target);
                        }
                        return target;
                    }
                    if (m.getName().equals("a") && Arrays.equals(pt, new Class<?>[]{c("dyy$a"), int.class, int.class})) {
                        return sceneHeight(scene, (int) a[1], (int) a[2]);
                    }
                    if (m.getName().equals("a") && (pt.length == 3 || pt.length == 4) && pt[0] == c("jd") && pt[1] == c("dtc") && pt[2] == int.class) {
                        int[] xyz = NativeHarness.xyz(a[0]);
                        List<Integer> key = List.of(xyz[0], xyz[1], xyz[2]);
                        boolean hasBe = (boolean) c("dtb$a").getMethod("t").invoke(a[1]);
                        if (hasBe) {
                            Object blk = c("dtc").getMethod("b").invoke(a[1]);
                            if (c("diq").isInstance(blk)) {
                                Object be = c("diq").getMethod("a", c("jd"), c("dtc")).invoke(blk, a[0], a[1]);
                                if (be != null) blockEntities.put(key, be);
                                else blockEntities.remove(key);
                            } else {
                                blockEntities.remove(key);
                            }
                        } else {
                            blockEntities.remove(key);
                        }
                        // Delegate write-through recording to level.proxy without calling invokeDefault
                    }
                    boolean isSet = m.getName().equals("a") && (pt.length == 3 || pt.length == 4)
                        && pt[0] == c("jd") && pt[1] == c("dtc") && pt[2] == int.class;
                    if (m.isDefault() && !isSet) {
                        return InvocationHandler.invokeDefault(p, m, a);
                    }
                    try { return m.invoke(level.proxy, a); }
                    catch (InvocationTargetException e) { throw e.getCause(); }
                });

                Object rng = c("dzx").getConstructor(c("ayw")).newInstance(c("dzz").getConstructor(long.class).newInstance(0L));
                c("dzx").getMethod("a", long.class, int.class, int.class).invoke(rng, seed, cx << 4, cz << 4);
                Object clip = c("ejj").getConstructor(int.class, int.class, int.class, int.class, int.class, int.class)
                    .newInstance(cx << 4, NativeHarness.MIN_Y, cz << 4, (cx << 4) + 15, NativeHarness.MIN_Y + NativeHarness.HEIGHT - 1, (cz << 4) + 15);
                Object cPos = c("dcd").getConstructor(int.class, int.class).newInstance(cx, cz);
                c("ejz").getMethod("a", c("dds"), c("ddq"), c("duz"), c("ayw"), c("ejj"), c("dcd"))
                    .invoke(start, world, sm, generator, rng, clip, cPos);

                for (String logLine : level.log) {
                    if (logLine.startsWith("unsupported")) throw new AssertionError(logLine);
                }

                // Collect final writes sorted by (x, y, z)
                List<Map.Entry<String, Object>> sorted = new ArrayList<>(level.writes.entrySet());
                sorted.sort((e1, e2) -> {
                    String[] p1 = e1.getKey().split(","), p2 = e2.getKey().split(",");
                    int c0 = Integer.compare(Integer.parseInt(p1[0]), Integer.parseInt(p2[0]));
                    if (c0 != 0) return c0;
                    int c1 = Integer.compare(Integer.parseInt(p1[1]), Integer.parseInt(p2[1]));
                    if (c1 != 0) return c1;
                    return Integer.compare(Integer.parseInt(p1[2]), Integer.parseInt(p2[2]));
                });
                StringBuilder wb = new StringBuilder();
                for (var e : sorted) {
                    wb.append(e.getKey()).append('=').append(NativeHarness.normalizedStateName(e.getValue())).append('\n');
                }
                String wSha = sha256(wb.toString());
                long rngAfter = (long) c("ayw").getMethod("g").invoke(rng);
                lines.add("CHUNK\t" + id + "\t" + cx + "\t" + cz + "\t" + sorted.size() + "\t" + wSha + "\t" + rngAfter);
            }
        }
        Files.write(Path.of(args[1]), lines);
        System.out.println("jigsaw oracle: wrote " + lines.size() + " rows");
    }
}
