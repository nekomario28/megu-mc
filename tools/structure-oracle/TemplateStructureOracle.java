package dev.mcspine.tools;

import com.google.gson.*;
import com.mojang.serialization.Codec;
import com.mojang.serialization.DataResult;
import com.mojang.serialization.JsonOps;
import com.mojang.serialization.MapCodec;
import java.io.*;
import java.lang.reflect.*;
import java.nio.charset.StandardCharsets;
import java.nio.file.*;
import java.util.*;
import java.util.concurrent.ConcurrentHashMap;
import java.util.concurrent.atomic.AtomicInteger;

/**
 * Native 1.21.1 oracle for BlockAgeProcessor, extended StructureTemplate placement
 * (waterlogging, BARRIER clear, LootTableSeed, JIGSAW final_state, entities, palette pick),
 * and the 5 template structure types (Igloo, NetherFossil, Shipwreck, OceanRuin, RuinedPortal).
 */
public final class TemplateStructureOracle {
    static Class<?> stateC, posC, rlC, blockC, entityBlockC, blockEntityC, randomizableC;
    static Class<?> templateC, settingsC, placementInfoC, processorC, compoundC, heightAccC;
    static Class<?> structureC, genCtxC, genStubC, piecesBuilderC, bboxC, chunkPosC, rngGenC;
    static Class<?> heightmapTypeC, biomeC, holderC, registryAccessC, registryC, randomStateC;
    static Class<?> chunkGenC, biomeSourceC, noiseColumnC, structureManagerC;

    static Object ops, regAccess, blockRegistry, worldGenHeightAcc, randomState;
    static Object templateManager, airState, waterState, soulSandState, netherrackState, stoneState;
    static Object warmBiomeHolder, coldBiomeHolder, defaultStructureManager;

    static void init() throws Exception {
        NativeHarness.bootstrap();
        StructureTemplateOracle.initNbt();
        stateC = Class.forName("dtc");
        posC = Class.forName("jd");
        rlC = Class.forName("akr");
        blockC = Class.forName("dfy");
        blockEntityC = Class.forName("dqh");
        randomizableC = Class.forName("bqz");
        templateC = Class.forName("ent");
        settingsC = Class.forName("enp");
        placementInfoC = Class.forName("ent$c");
        processorC = Class.forName("enq");
        compoundC = Class.forName("ub");
        heightAccC = Class.forName("dcy");
        structureC = Class.forName("ejr");
        genCtxC = Class.forName("ejr$a");
        genStubC = Class.forName("ejr$b");
        piecesBuilderC = Class.forName("ekj");
        bboxC = Class.forName("ejj");
        chunkPosC = Class.forName("dcd");
        rngGenC = Class.forName("dzx");
        heightmapTypeC = Class.forName("dyy$a");
        biomeC = Class.forName("ddw");
        holderC = Class.forName("jm");
        registryAccessC = Class.forName("ka");
        registryC = Class.forName("jz");
        randomStateC = Class.forName("dzm");
        chunkGenC = Class.forName("duz");
        biomeSourceC = Class.forName("dea");
        noiseColumnC = Class.forName("ddi");
        structureManagerC = Class.forName("ddq");

        Object registry = Class.forName("lt").getField("aA").get(null);
        regAccess = registryAccessC.getMethod("a", registryC).invoke(null, registry);
        ops = Class.forName("akp").getMethod("a", com.mojang.serialization.DynamicOps.class, Class.forName("jo$a"))
                .invoke(null, JsonOps.INSTANCE, regAccess);
        blockRegistry = Class.forName("lt").getField("e").get(null);

        airState = NativeHarness.state("minecraft:air");
        waterState = NativeHarness.state("minecraft:water");
        soulSandState = NativeHarness.state("minecraft:soul_sand");
        netherrackState = NativeHarness.state("minecraft:netherrack");
        stoneState = NativeHarness.state("minecraft:stone");

        worldGenHeightAcc = heightAccC.getMethod("e", int.class, int.class).invoke(null, -64, 384);
        randomState = allocate(randomStateC);
        Field fSampler = randomStateC.getDeclaredField("d");
        fSampler.setAccessible(true);
        fSampler.set(randomState, Class.forName("def").getMethod("a").invoke(null));
        defaultStructureManager = allocate(structureManagerC);
    }

    static Object allocate(Class<?> c) throws Exception {
        Field uf = Class.forName("sun.misc.Unsafe").getDeclaredField("theUnsafe");
        uf.setAccessible(true);
        Object unsafe = uf.get(null);
        return unsafe.getClass().getMethod("allocateInstance", Class.class).invoke(unsafe, c);
    }

    static Object pos(JsonArray a) throws Exception {
        return NativeHarness.pos(a.get(0).getAsInt(), a.get(1).getAsInt(), a.get(2).getAsInt());
    }

    static Object loadTemplate(Path file) throws Exception {
        Object blockLookup = registryC.getMethod("q").invoke(blockRegistry);
        Object root = Class.forName("uo").getMethod("a", Path.class, Class.forName("uk"))
                .invoke(null, file, Class.forName("uk").getMethod("a").invoke(null));
        Object loaded = templateC.getConstructor().newInstance();
        templateC.getMethod("a", Class.forName("jn"), compoundC).invoke(loaded, blockLookup, root);
        return loaded;
    }

    static Object buildTemplateManager(Path vanillaRoot) throws Exception {
        Object mgr = allocate(Class.forName("enu"));
        Field fMap = Class.forName("enu").getDeclaredField("f");
        fMap.setAccessible(true);
        Map<Object, Optional<Object>> cache = new ConcurrentHashMap<>();
        Path structRoot = vanillaRoot.resolve("datapacks/vanilla/data/minecraft/structure");
        try (var stream = Files.walk(structRoot)) {
            for (Path file : stream.filter(p -> p.toString().endsWith(".nbt")).sorted().toList()) {
                String rel = structRoot.relativize(file).toString().replace('\\', '/');
                rel = rel.substring(0, rel.length() - 4);
                Object rl = rlC.getMethod("a", String.class).invoke(null, "minecraft:" + rel);
                cache.put(rl, Optional.of(loadTemplate(file)));
            }
        }
        fMap.set(mgr, cache);
        return mgr;
    }

    static Object loadBiome(Path vanillaRoot, String name) throws Exception {
        String text = Files.readString(vanillaRoot.resolve("datapacks/vanilla/data/minecraft/worldgen/biome/" + name + ".json"));
        Codec<?> codec = (Codec<?>) biomeC.getField("a").get(null);
        Object biome = NativeHarness.getOrThrow(parseWithOps(codec, JsonParser.parseString(text)));
        return holderC.getMethod("a", Object.class).invoke(null, biome);
    }

    static Object parseWithOps(Codec<?> codec, JsonElement el) throws Exception {
        for (Method m : Codec.class.getMethods()) {
            if (m.getName().equals("parse") && m.getParameterCount() == 2
                    && m.getParameterTypes()[0].isAssignableFrom(ops.getClass())) {
                try {
                    return m.invoke(codec, ops, el);
                } catch (IllegalArgumentException ignored) {}
            }
        }
        throw new AssertionError("Codec.parse failed");
    }

    static Object parseStructure(Path file) throws Exception {
        String text = Files.readString(file);
        JsonObject obj = JsonParser.parseString(text).getAsJsonObject();
        String type = obj.get("type").getAsString();
        Map<String, String> classMap = Map.of(
                "minecraft:igloo", "elr",
                "minecraft:nether_fossil", "ema",
                "minecraft:shipwreck", "emi",
                "minecraft:ocean_ruin", "eme",
                "minecraft:ruined_portal", "emg"
        );
        Class<?> cls = Class.forName(classMap.get(type));
        MapCodec<?> mc = (MapCodec<?>) cls.getField("d").get(null);
        // Replace "biomes" with a direct biome holder or empty HolderSet<Biome> via JsonOps if registry lookup isn't loaded
        // Note: Structure.StructureSettings (`ejr$c`) uses `biomes` only for RegistryCodecs.homogeneousList;
        // we bind a dummy empty Tag/List `biomes: []` for codec decode since findGenerationPoint uses ctx.validBiome() predicate!
        JsonObject patched = obj.deepCopy();
        patched.add("biomes", new JsonArray());
        return NativeHarness.getOrThrow(parseWithOps(mc.codec(), patched));
    }

    static Object newBlockEntityForState(Object pos, Object state) throws Exception {
        Object block = stateC.getMethod("b").invoke(state);
        for (Method m : block.getClass().getMethods()) {
            if (m.getParameterCount() == 2 && m.getParameterTypes()[0] == posC
                    && m.getParameterTypes()[1] == stateC
                    && m.getReturnType() == blockEntityC) {
                return m.invoke(block, pos, state);
            }
        }
        return null;
    }

    static Object difficultyAt() throws Exception {
        Class<?> diffEnum = Class.forName("bqo");
        Object normal = diffEnum.getEnumConstants()[2]; // NORMAL
        return Class.forName("bqp").getConstructor(diffEnum, long.class, long.class, float.class)
                .newInstance(normal, 0L, 0L, 0.0f);
    }

    static Object minimalWorldServerLevel(long worldSeed) throws Exception {
        Field unsafeField = Class.forName("sun.misc.Unsafe").getDeclaredField("theUnsafe");
        unsafeField.setAccessible(true);
        Object unsafe = unsafeField.get(null);
        Method alloc = unsafe.getClass().getMethod("allocateInstance", Class.class);

        Object vanillaFlags = Class.forName("cpn").getField("f").get(null); // FeatureFlags.VANILLA_SET
        Object worldOpts = Class.forName("dzw").getConstructor(long.class, boolean.class, boolean.class)
                .newInstance(worldSeed, false, false);
        Object worldData = Proxy.newProxyInstance(
                TemplateStructureOracle.class.getClassLoader(),
                new Class<?>[]{Class.forName("erl")},
                (proxy, method, args) -> {
                    if (method.getName().equals("K") && method.getParameterCount() == 0) {
                        return vanillaFlags;
                    }
                    if (method.getName().equals("y") && method.getParameterCount() == 0) {
                        return worldOpts;
                    }
                    throw new UnsupportedOperationException("minimal worldData: " + method);
                });
        Class<?> serverC = Class.forName("net.minecraft.server.MinecraftServer");
        Object server = alloc.invoke(unsafe, Class.forName("apn"));
        Field worldDataField = serverC.getDeclaredField("j");
        worldDataField.setAccessible(true);
        worldDataField.set(server, worldData);

        Class<?> slC = Class.forName("aqu");
        Object sl = alloc.invoke(unsafe, slC);
        Field serverField = slC.getDeclaredField("J");
        serverField.setAccessible(true);
        serverField.set(sl, server);
        return sl;
    }

    static Object buildDimType(int minY, int height, boolean ultraWarm) throws Exception {
        String json = String.format(
                "{\"ultrawarm\":%s,\"natural\":true,\"piglin_safe\":false,\"respawn_anchor_works\":false,"
                + "\"bed_works\":true,\"has_raids\":true,\"has_skylight\":true,\"has_ceiling\":false,"
                + "\"coordinate_scale\":1.0,\"ambient_light\":0.0,\"logical_height\":%d,"
                + "\"infiniburn\":\"#minecraft:infiniburn_overworld\",\"min_y\":%d,\"height\":%d,"
                + "\"monster_spawn_light_level\":0,\"monster_spawn_block_light_limit\":0,\"effects\":\"minecraft:overworld\"}",
                ultraWarm, height, minY, height);
        Codec<?> codec = (Codec<?>) Class.forName("dwy").getField("h").get(null);
        return NativeHarness.getOrThrow(parseWithOps(codec, JsonParser.parseString(json)));
    }

    static final class OracleWorld {
        final NativeHarness.Level inner;
        final Object proxy;
        final Map<String, Object> blockEntities = new LinkedHashMap<>();
        final Object levelRng;
        final Object biomeHolder;
        final Object difficulty;
        final int seaLevel;
        final Object dimType;

        OracleWorld(NativeHarness.StateScene scene, int minY, int height, int seaLevel, long worldSeed,
                    Object biomeHolder, long levelRngSeed) throws Exception {
            this.inner = NativeHarness.levelFromStates(scene, minY, height);
            this.inner.worldSeed = worldSeed;
            this.inner.entityServerLevel = minimalWorldServerLevel(worldSeed);
            this.seaLevel = seaLevel;
            this.dimType = buildDimType(minY, height, seaLevel == 32);
            this.biomeHolder = biomeHolder;
            this.levelRng = NativeHarness.rng(levelRngSeed);
            this.difficulty = difficultyAt();
            this.proxy = Proxy.newProxyInstance(
                    OracleWorld.class.getClassLoader(),
                    new Class<?>[]{Class.forName("dds")},
                    this::handle);
        }

        private Object handle(Object p, Method m, Object[] a) throws Throwable {
            Class<?>[] pt = m.getParameterTypes();
            String name = m.getName();
            if (name.equals("c_") && pt.length == 1 && pt[0] == posC) {
                int[] xyz = xyz(a[0]);
                return blockEntities.get(xyz[0] + "," + xyz[1] + "," + xyz[2]);
            }
            if (name.equals("H_") && pt.length == 0) {
                return regAccess;
            }
            if (name.equals("J") && pt.length == 0) {
                return Class.forName("cpn").getField("f").get(null);
            }
            if (name.equals("D_") && pt.length == 0) {
                return dimType;
            }
            if (name.equals("d_") && pt.length == 1 && pt[0] == posC) {
                return difficulty;
            }
            if (name.equals("E_") && pt.length == 0) {
                return levelRng;
            }
            if (name.equals("x_") && pt.length == 0) {
                return false; // isClientSide
            }
            if (name.equals("z_") && pt.length == 0) {
                return seaLevel;
            }
            if (name.equals("t") && pt.length == 1 && pt[0] == posC) {
                return biomeHolder;
            }
            if (name.equals("a") && pt.length == 3 && pt[0] == heightmapTypeC
                    && pt[1] == int.class && pt[2] == int.class) {
                return computeHeight((Enum<?>) a[0], (int) a[1], (int) a[2]);
            }
            if (name.equals("a_") && pt.length == 1 && pt[0].getName().equals("bsr")) {
                // addFreshEntityWithPassengers: call b(Entity) on inner proxy
                Class.forName("ddl").getMethod("b", Class.forName("bsr")).invoke(inner.proxy, a[0]);
                return null;
            }
            if (name.equals("a") && (pt.length == 3 || pt.length == 4)
                    && pt[0] == posC && pt[1] == stateC && pt[2] == int.class) {
                int[] xyz = xyz(a[0]);
                String key = xyz[0] + "," + xyz[1] + "," + xyz[2];
                Object res = m.invoke(inner.proxy, a);
                if (Boolean.TRUE.equals(res)) {
                    Object existingBe = blockEntities.get(key);
                    if (existingBe != null && !Boolean.TRUE.equals(
                            blockEntityC.getMethod("b", stateC).invoke(existingBe, a[1]))) {
                        blockEntities.remove(key);
                        existingBe = null;
                    }
                    if (existingBe == null) {
                        Object newBe = newBlockEntityForState(a[0], a[1]);
                        if (newBe != null) {
                            blockEntities.put(key, newBe);
                        }
                    }
                }
                return res;
            }
            if (m.isDefault()) {
                return java.lang.invoke.MethodHandles
                        .privateLookupIn(m.getDeclaringClass(), java.lang.invoke.MethodHandles.lookup())
                        .unreflectSpecial(m, m.getDeclaringClass())
                        .bindTo(p)
                        .invokeWithArguments(a == null ? new Object[0] : a);
            }
            return m.invoke(inner.proxy, a);
        }

        int computeHeight(Enum<?> kind, int x, int z) throws Exception {
            @SuppressWarnings("unchecked")
            java.util.function.Predicate<Object> pred = (java.util.function.Predicate<Object>)
                    heightmapTypeC.getMethod("e").invoke(kind);
            for (int y = inner.minY + inner.height - 1; y >= inner.minY; y--) {
                Object st = Class.forName("dcc").getMethod("a_", posC)
                        .invoke(inner.proxy, NativeHarness.pos(x, y, z));
                if (pred.test(st)) {
                    return y + 1;
                }
            }
            return inner.minY;
        }
    }

    static int[] xyz(Object p) throws Exception {
        return new int[]{
                (int) posC.getMethod("u").invoke(p),
                (int) posC.getMethod("v").invoke(p),
                (int) posC.getMethod("w").invoke(p)
        };
    }

    static final class ScriptedTerrain {
        final int minY, height, seaLevel;
        final Map<Long, Integer> wsHeight = new HashMap<>();
        final Map<Long, Integer> ocHeight = new HashMap<>();
        final Map<String, Object> cells = new HashMap<>();
        final Object defaultTopState;
        final Object defaultFillState;
        final boolean oceanWorld;

        ScriptedTerrain(JsonObject terrainJson) throws Exception {
            this.minY = terrainJson.has("min_y") ? terrainJson.get("min_y").getAsInt() : -64;
            this.height = terrainJson.has("height") ? terrainJson.get("height").getAsInt() : 384;
            this.seaLevel = terrainJson.has("sea_level") ? terrainJson.get("sea_level").getAsInt() : 63;
            this.oceanWorld = terrainJson.has("ocean") && terrainJson.get("ocean").getAsBoolean();
            this.defaultTopState = terrainJson.has("top_block")
                    ? NativeHarness.state(terrainJson.get("top_block").getAsString())
                    : NativeHarness.state("minecraft:stone");
            this.defaultFillState = terrainJson.has("fill_block")
                    ? NativeHarness.state(terrainJson.get("fill_block").getAsString())
                    : NativeHarness.state("minecraft:stone");
            if (terrainJson.has("heights")) {
                for (var entry : terrainJson.getAsJsonObject("heights").entrySet()) {
                    String[] xz = entry.getKey().split(",");
                    int x = Integer.parseInt(xz[0]), z = Integer.parseInt(xz[1]);
                    long key = (((long) x) << 32) ^ (z & 0xffff_ffffL);
                    if (entry.getValue().isJsonObject()) {
                        JsonObject ho = entry.getValue().getAsJsonObject();
                        wsHeight.put(key, ho.get("ws").getAsInt());
                        ocHeight.put(key, ho.get("oc").getAsInt());
                    } else {
                        int h = entry.getValue().getAsInt();
                        wsHeight.put(key, h);
                        ocHeight.put(key, oceanWorld ? h - 8 : h);
                    }
                }
            }
            if (terrainJson.has("cells")) {
                for (var entry : terrainJson.getAsJsonObject("cells").entrySet()) {
                    JsonElement val = entry.getValue();
                    Object st = val.isJsonObject()
                            ? NativeHarness.stateFromJson(val)
                            : NativeHarness.state(val.getAsString());
                    cells.put(entry.getKey(), st);
                }
            }
        }

        int baseHeight(int x, int z, Enum<?> kind) {
            long key = (((long) x) << 32) ^ (z & 0xffff_ffffL);
            String kname = kind.name();
            boolean isOc = kname.equals("c") || kname.equals("d"); // OCEAN_FLOOR_WG or OCEAN_FLOOR
            if (isOc && ocHeight.containsKey(key)) return ocHeight.get(key);
            if (!isOc && wsHeight.containsKey(key)) return wsHeight.get(key);
            int base = wsHeight.getOrDefault(key, oceanWorld ? 64 : 70);
            return isOc && oceanWorld ? base - 8 : base;
        }

        Object stateAt(int x, int y, int z) {
            Object exact = cells.get(x + "," + y + "," + z);
            if (exact != null) return exact;
            long key = (((long) x) << 32) ^ (z & 0xffff_ffffL);
            if (oceanWorld) {
                int ws = wsHeight.getOrDefault(key, 64);
                int oc = ocHeight.getOrDefault(key, ws - 8);
                if (y >= ws) return airState;
                if (y >= oc) return waterState;
                if (y == oc - 1) return defaultTopState;
                return defaultFillState;
            } else {
                int ws = wsHeight.getOrDefault(key, 70);
                if (y >= ws) return airState;
                if (y == ws - 1) return defaultTopState;
                return defaultFillState;
            }
        }

        Object noiseColumn(int x, int z) throws Exception {
            Object arr = Array.newInstance(stateC, height);
            for (int i = 0; i < height; i++) {
                Array.set(arr, i, stateAt(x, minY + i, z));
            }
            return noiseColumnC.getConstructor(int.class, Array.newInstance(stateC, 0).getClass())
                    .newInstance(minY, arr);
        }
    }

    static ScriptedTerrain activeTerrain;
    static Object activeBiomeHolder;

    public static int cgMinY() { return activeTerrain.minY; }
    public static int cgHeight() { return activeTerrain.height; }
    public static int cgSeaLevel() { return activeTerrain.seaLevel; }
    public static int cgBaseHeight(int x, int z, Object kind) {
        return activeTerrain.baseHeight(x, z, (Enum<?>) kind);
    }
    public static Object cgColumn(int x, int z) {
        try { return activeTerrain.noiseColumn(x, z); }
        catch (Exception e) { throw new RuntimeException(e); }
    }
    public static Object bsBiome() { return activeBiomeHolder; }
    public static java.util.stream.Stream<Object> bsStream() {
        return java.util.stream.Stream.of(activeBiomeHolder);
    }

    static Class<?> buildBiomeSourceClass() throws Throwable {
        var oracleDesc = java.lang.constant.ClassDesc.of("dev.mcspine.tools.TemplateStructureOracle");
        var thisDesc = java.lang.constant.ClassDesc.of("dev.mcspine.tools.GenBiomeSource");
        var deaDesc = java.lang.constant.ClassDesc.of("dea");
        var jmDesc = java.lang.constant.ClassDesc.of("jm");
        var defFDesc = java.lang.constant.ClassDesc.of("def$f");
        var streamDesc = java.lang.constant.ClassDesc.of("java.util.stream.Stream");
        var mapCodecDesc = java.lang.constant.ClassDesc.of("com.mojang.serialization.MapCodec");
        var objDesc = java.lang.constant.ConstantDescs.CD_Object;

        byte[] bytes = java.lang.classfile.ClassFile.of().build(thisDesc, cb -> {
            cb.withSuperclass(deaDesc);
            cb.withFlags(java.lang.classfile.ClassFile.ACC_PUBLIC | java.lang.classfile.ClassFile.ACC_FINAL);
            cb.withMethodBody("<init>", java.lang.constant.MethodTypeDesc.of(java.lang.constant.ConstantDescs.CD_void),
                    java.lang.classfile.ClassFile.ACC_PUBLIC, xb -> {
                xb.aload(0);
                xb.invokespecial(deaDesc, "<init>", java.lang.constant.MethodTypeDesc.of(java.lang.constant.ConstantDescs.CD_void));
                xb.return_();
            });
            cb.withMethodBody("a", java.lang.constant.MethodTypeDesc.of(mapCodecDesc),
                    java.lang.classfile.ClassFile.ACC_PROTECTED, xb -> {
                xb.aconst_null();
                xb.areturn();
            });
            cb.withMethodBody("b", java.lang.constant.MethodTypeDesc.of(streamDesc),
                    java.lang.classfile.ClassFile.ACC_PROTECTED, xb -> {
                xb.invokestatic(oracleDesc, "bsStream", java.lang.constant.MethodTypeDesc.of(streamDesc));
                xb.areturn();
            });
            cb.withMethodBody("getNoiseBiome", java.lang.constant.MethodTypeDesc.of(jmDesc,
                    java.lang.constant.ConstantDescs.CD_int,
                    java.lang.constant.ConstantDescs.CD_int,
                    java.lang.constant.ConstantDescs.CD_int,
                    defFDesc),
                    java.lang.classfile.ClassFile.ACC_PUBLIC, xb -> {
                xb.invokestatic(oracleDesc, "bsBiome", java.lang.constant.MethodTypeDesc.of(objDesc));
                xb.checkcast(jmDesc);
                xb.areturn();
            });
        });
        return java.lang.invoke.MethodHandles.lookup().defineClass(bytes);
    }

    static Class<?> buildChunkGeneratorClass() throws Throwable {
        var oracleDesc = java.lang.constant.ClassDesc.of("dev.mcspine.tools.TemplateStructureOracle");
        var thisDesc = java.lang.constant.ClassDesc.of("dev.mcspine.tools.GenChunkGenerator");
        var duzDesc = java.lang.constant.ClassDesc.of("duz");
        var deaDesc = java.lang.constant.ClassDesc.of("dea");
        var ddiDesc = java.lang.constant.ClassDesc.of("ddi");
        var dyyADesc = java.lang.constant.ClassDesc.of("dyy$a");
        var dcyDesc = java.lang.constant.ClassDesc.of("dcy");
        var dzmDesc = java.lang.constant.ClassDesc.of("dzm");
        var arbDesc = java.lang.constant.ClassDesc.of("arb");
        var ddyDesc = java.lang.constant.ClassDesc.of("ddy");
        var ddqDesc = java.lang.constant.ClassDesc.of("ddq");
        var duyDesc = java.lang.constant.ClassDesc.of("duy");
        var dyuADesc = java.lang.constant.ClassDesc.of("dyu$a");
        var eaaDesc = java.lang.constant.ClassDesc.of("eaa");
        var jdDesc = java.lang.constant.ClassDesc.of("jd");
        var listDesc = java.lang.constant.ClassDesc.of("java.util.List");
        var futureDesc = java.lang.constant.ClassDesc.of("java.util.concurrent.CompletableFuture");
        var mapCodecDesc = java.lang.constant.ClassDesc.of("com.mojang.serialization.MapCodec");
        var cdVoid = java.lang.constant.ConstantDescs.CD_void;
        var cdInt = java.lang.constant.ConstantDescs.CD_int;
        var cdLong = java.lang.constant.ConstantDescs.CD_long;
        var objDesc = java.lang.constant.ConstantDescs.CD_Object;

        byte[] bytes = java.lang.classfile.ClassFile.of().build(thisDesc, cb -> {
            cb.withSuperclass(duzDesc);
            cb.withFlags(java.lang.classfile.ClassFile.ACC_PUBLIC | java.lang.classfile.ClassFile.ACC_FINAL);
            cb.withMethodBody("<init>", java.lang.constant.MethodTypeDesc.of(cdVoid, deaDesc),
                    java.lang.classfile.ClassFile.ACC_PUBLIC, xb -> {
                xb.aload(0);
                xb.aload(1);
                xb.invokespecial(duzDesc, "<init>", java.lang.constant.MethodTypeDesc.of(cdVoid, deaDesc));
                xb.return_();
            });
            cb.withMethodBody("b", java.lang.constant.MethodTypeDesc.of(mapCodecDesc),
                    java.lang.classfile.ClassFile.ACC_PROTECTED, xb -> {
                xb.aconst_null();
                xb.areturn();
            });
            cb.withMethodBody("a", java.lang.constant.MethodTypeDesc.of(cdVoid, arbDesc, cdLong, dzmDesc, ddyDesc, ddqDesc, duyDesc, dyuADesc),
                    java.lang.classfile.ClassFile.ACC_PUBLIC, xb -> xb.return_());
            cb.withMethodBody("a", java.lang.constant.MethodTypeDesc.of(cdVoid, arbDesc, ddqDesc, dzmDesc, duyDesc),
                    java.lang.classfile.ClassFile.ACC_PUBLIC, xb -> xb.return_());
            cb.withMethodBody("a", java.lang.constant.MethodTypeDesc.of(cdVoid, arbDesc),
                    java.lang.classfile.ClassFile.ACC_PUBLIC, xb -> xb.return_());
            cb.withMethodBody("e", java.lang.constant.MethodTypeDesc.of(cdInt),
                    java.lang.classfile.ClassFile.ACC_PUBLIC, xb -> {
                xb.invokestatic(oracleDesc, "cgHeight", java.lang.constant.MethodTypeDesc.of(cdInt));
                xb.ireturn();
            });
            cb.withMethodBody("a", java.lang.constant.MethodTypeDesc.of(futureDesc, eaaDesc, dzmDesc, ddqDesc, duyDesc),
                    java.lang.classfile.ClassFile.ACC_PUBLIC, xb -> {
                xb.aconst_null();
                xb.areturn();
            });
            cb.withMethodBody("f", java.lang.constant.MethodTypeDesc.of(cdInt),
                    java.lang.classfile.ClassFile.ACC_PUBLIC, xb -> {
                xb.invokestatic(oracleDesc, "cgSeaLevel", java.lang.constant.MethodTypeDesc.of(cdInt));
                xb.ireturn();
            });
            cb.withMethodBody("g", java.lang.constant.MethodTypeDesc.of(cdInt),
                    java.lang.classfile.ClassFile.ACC_PUBLIC, xb -> {
                xb.invokestatic(oracleDesc, "cgMinY", java.lang.constant.MethodTypeDesc.of(cdInt));
                xb.ireturn();
            });
            cb.withMethodBody("a", java.lang.constant.MethodTypeDesc.of(cdInt, cdInt, cdInt, dyyADesc, dcyDesc, dzmDesc),
                    java.lang.classfile.ClassFile.ACC_PUBLIC, xb -> {
                xb.iload(1);
                xb.iload(2);
                xb.aload(3);
                xb.invokestatic(oracleDesc, "cgBaseHeight", java.lang.constant.MethodTypeDesc.of(cdInt, cdInt, cdInt, objDesc));
                xb.ireturn();
            });
            cb.withMethodBody("a", java.lang.constant.MethodTypeDesc.of(ddiDesc, cdInt, cdInt, dcyDesc, dzmDesc),
                    java.lang.classfile.ClassFile.ACC_PUBLIC, xb -> {
                xb.iload(1);
                xb.iload(2);
                xb.invokestatic(oracleDesc, "cgColumn", java.lang.constant.MethodTypeDesc.of(objDesc, cdInt, cdInt));
                xb.checkcast(ddiDesc);
                xb.areturn();
            });
            cb.withMethodBody("a", java.lang.constant.MethodTypeDesc.of(cdVoid, listDesc, dzmDesc, jdDesc),
                    java.lang.classfile.ClassFile.ACC_PUBLIC, xb -> xb.return_());
        });
        return java.lang.invoke.MethodHandles.lookup().defineClass(bytes);
    }

    public static void main(String[] args) throws Throwable {
        init();
        Class<?> genBiomeSourceC = buildBiomeSourceClass();
        Class<?> genChunkGenC = buildChunkGeneratorClass();
        Path inputPath = Path.of(args[0]);
        Path outputPath = Path.of(args[1]);
        Path repoRoot = Path.of(args[2]);

        templateManager = buildTemplateManager(repoRoot);
        warmBiomeHolder = loadBiome(repoRoot, "plains");
        coldBiomeHolder = loadBiome(repoRoot, "snowy_taiga");

        JsonObject root = JsonParser.parseString(Files.readString(inputPath)).getAsJsonObject();
        List<String> outRows = new ArrayList<>();

        // 1. BlockAgeProcessor oracle
        Codec<?> procCodec = (Codec<?>) Class.forName("ens").getField("a").get(null);
        Method processInfos = templateC.getMethod("a", Class.forName("ddl"), posC, posC, settingsC, List.class);
        Constructor<?> infoCtor = placementInfoC.getConstructor(posC, stateC, compoundC);
        for (JsonElement el : root.getAsJsonArray("block_age")) {
            JsonObject c = el.getAsJsonObject();
            Object opts = settingsC.getConstructor().newInstance();
            settingsC.getMethod("a", Class.forName("dkv")).invoke(opts, Class.forName("dkv").getEnumConstants()[c.get("mirror").getAsInt()]);
            settingsC.getMethod("a", Class.forName("dmm")).invoke(opts, Class.forName("dmm").getEnumConstants()[c.get("rotation").getAsInt()]);
            settingsC.getMethod("a", posC).invoke(opts, pos(c.getAsJsonArray("pivot")));
            Object rng = NativeHarness.rng(c.get("seed").getAsLong());
            if (c.get("random").getAsString().equals("shared")) {
                settingsC.getMethod("a", Class.forName("ayw")).invoke(opts, rng);
            }
            for (JsonElement pe : c.getAsJsonArray("processors")) {
                Object proc = NativeHarness.getOrThrow(parseWithOps(procCodec, pe));
                settingsC.getMethod("a", processorC).invoke(opts, proc);
            }
            List<Object> blocks = new ArrayList<>();
            for (JsonElement be : c.getAsJsonArray("blocks")) {
                JsonObject bo = be.getAsJsonObject();
                blocks.add(infoCtor.newInstance(pos(bo.getAsJsonArray("pos")), NativeHarness.stateFromJson(bo.get("state")), null));
            }
            var level = NativeHarness.levelFromStates((x, y, z) -> airState);
            Object origin = pos(c.getAsJsonArray("origin"));
            List<?> processed = (List<?>) processInfos.invoke(null, level.proxy, origin, origin, opts, blocks);
            ByteArrayOutputStream bytes = new ByteArrayOutputStream();
            DataOutputStream dos = new DataOutputStream(bytes);
            dos.writeInt(1);
            dos.writeInt(processed.size());
            for (Object b : processed) StructureTemplateOracle.writeBlockInfo(b, dos);
            int probe = (int) Class.forName("ayw").getMethod("a", int.class).invoke(rng, 1000003);
            outRows.add("block_age\t" + c.get("id").getAsString() + "\t" + processed.size() + "\t"
                    + StructureTemplateOracle.hash(bytes.toByteArray()) + "\t" + probe);
        }

        // 2. Extended placeInWorld oracle (waterlogging, BARRIER pre-clear, LootTableSeed, entities, palette selection)
        Method placeInWorld = templateC.getMethod("a", Class.forName("ddl"), posC, posC, settingsC, Class.forName("ayw"), int.class);
        for (JsonElement el : root.getAsJsonArray("place_extended")) {
            JsonObject c = el.getAsJsonObject();
            Object tmpl;
            if (c.has("template")) {
                tmpl = loadTemplate(repoRoot.resolve(c.get("template").getAsString()));
            } else {
                tmpl = buildSyntheticTemplate(c.getAsJsonArray("blocks"));
            }
            Object opts = settingsC.getConstructor().newInstance();
            settingsC.getMethod("a", Class.forName("dkv")).invoke(opts, Class.forName("dkv").getEnumConstants()[c.get("mirror").getAsInt()]);
            settingsC.getMethod("a", Class.forName("dmm")).invoke(opts, Class.forName("dmm").getEnumConstants()[c.get("rotation").getAsInt()]);
            settingsC.getMethod("a", posC).invoke(opts, pos(c.getAsJsonArray("pivot")));
            if (c.has("clip")) {
                JsonArray cl = c.getAsJsonArray("clip");
                settingsC.getMethod("a", bboxC).invoke(opts,
                        bboxC.getConstructor(int.class, int.class, int.class, int.class, int.class, int.class)
                                .newInstance(cl.get(0).getAsInt(), cl.get(1).getAsInt(), cl.get(2).getAsInt(),
                                        cl.get(3).getAsInt(), cl.get(4).getAsInt(), cl.get(5).getAsInt()));
            }
            if (c.has("ignore_waterlogging") && c.get("ignore_waterlogging").getAsBoolean()) {
                settingsC.getMethod("a", Class.forName("end")).invoke(opts, Class.forName("end").getField("a").get(null));
            }
            if (c.has("ignore_entities") && c.get("ignore_entities").getAsBoolean()) {
                settingsC.getMethod("a", boolean.class).invoke(opts, true);
            }
            for (JsonElement pe : c.getAsJsonArray("processors")) {
                Object proc = NativeHarness.getOrThrow(parseWithOps(procCodec, pe));
                settingsC.getMethod("a", processorC).invoke(opts, proc);
            }
            ScriptedTerrain terrain = new ScriptedTerrain(c.getAsJsonObject("terrain"));
            OracleWorld world = new OracleWorld(terrain::stateAt, terrain.minY, terrain.height, terrain.seaLevel,
                    c.get("world_seed").getAsLong(), warmBiomeHolder, 777L);
            Object rng = NativeHarness.rng(c.get("seed").getAsLong());
            Object origin = pos(c.getAsJsonArray("origin"));
            boolean ok = (boolean) placeInWorld.invoke(tmpl, world.proxy, origin, origin, opts, rng,
                    c.has("flags") ? c.get("flags").getAsInt() : 2);
            int probe = (int) Class.forName("ayw").getMethod("a", int.class).invoke(rng, 1000003);
            outRows.add(formatWorldRow("place_extended", c.get("id").getAsString(), ok ? 1 : 0, world, -1, probe));
        }

        // 3. Full 5 structures oracle
        for (JsonElement el : root.getAsJsonArray("structures")) {
            JsonObject c = el.getAsJsonObject();
            String id = c.get("id").getAsString();
            String structPath = c.get("structure").getAsString();
            Object structure = parseStructure(repoRoot.resolve(structPath));
            ScriptedTerrain terrain = new ScriptedTerrain(c.getAsJsonObject("terrain"));
            boolean isCold = c.has("cold_biome") && c.get("cold_biome").getAsBoolean();
            Object biomeH = isCold ? coldBiomeHolder : warmBiomeHolder;
            activeBiomeHolder = biomeH;
            activeTerrain = terrain;
            Object bs = genBiomeSourceC.getConstructor().newInstance();
            Object cg = genChunkGenC.getConstructor(biomeSourceC).newInstance(bs);
            long worldSeed = c.get("world_seed").getAsLong();
            int cx = c.get("cx").getAsInt();
            int cz = c.get("cz").getAsInt();
            Object chunkPos = chunkPosC.getConstructor(int.class, int.class).newInstance(cx, cz);
            Object heightAcc = heightAccC.getMethod("e", int.class, int.class)
                    .invoke(null, terrain.minY, terrain.height);
            @SuppressWarnings("unchecked")
            java.util.function.Predicate<Object> validBiome = h -> true;
            Object gctx = genCtxC.getConstructor(
                    registryAccessC, chunkGenC, biomeSourceC, randomStateC,
                    Class.forName("enu"), long.class, chunkPosC, heightAccC,
                    java.util.function.Predicate.class)
                    .newInstance(regAccess, cg, bs, randomState, templateManager, worldSeed, chunkPos, heightAcc, validBiome);
            @SuppressWarnings("unchecked")
            Optional<Object> stubOpt = (Optional<Object>) structureC.getMethod("b", genCtxC).invoke(structure, gctx);
            long genRngState = NativeHarness.rngState(genCtxC.getMethod("f").invoke(gctx));
            if (stubOpt.isEmpty()) {
                outRows.add("structure\t" + id + "\t0\tnone\t0\tnone\t0\tnone\t" + genRngState + "\t0");
                continue;
            }
            Object stub = stubOpt.get();
            Object builder = genStubC.getMethod("a").invoke(stub);
            // Access ekj.a (List<ejv> pieces)
            Field fPieces = piecesBuilderC.getDeclaredField("a");
            fPieces.setAccessible(true);
            List<?> pieces = (List<?>) fPieces.get(builder);

            OracleWorld world = new OracleWorld(terrain::stateAt, terrain.minY, terrain.height, terrain.seaLevel,
                    worldSeed, biomeH, c.has("level_rng_seed") ? c.get("level_rng_seed").getAsLong() : 999L);
            Object postRng = NativeHarness.rng(c.get("post_seed").getAsLong());
            JsonArray ca = c.getAsJsonArray("chunk_bb");
            Object chunkBb = bboxC.getConstructor(int.class, int.class, int.class, int.class, int.class, int.class)
                    .newInstance(ca.get(0).getAsInt(), ca.get(1).getAsInt(), ca.get(2).getAsInt(),
                            ca.get(3).getAsInt(), ca.get(4).getAsInt(), ca.get(5).getAsInt());
            Method postProcess = Class.forName("ejv").getMethod(
                    "a", Class.forName("dds"), structureManagerC, chunkGenC, Class.forName("ayw"),
                    bboxC, chunkPosC, posC);
            for (Object piece : pieces) {
                Object pbb = Class.forName("ejv").getMethod("f").invoke(piece);
                boolean intersects = (boolean) bboxC.getMethod("a", bboxC).invoke(pbb, chunkBb);
                if (intersects) {
                    Object stubPos = genStubC.getMethod("b").invoke(stub);
                    postProcess.invoke(piece, world.proxy, defaultStructureManager, cg, postRng, chunkBb, chunkPos, stubPos);
                }
            }
            int postProbe = (int) Class.forName("ayw").getMethod("a", int.class).invoke(postRng, 1000003);
            outRows.add(formatWorldRow("structure", id, pieces.size(), world, genRngState, postProbe));
        }

        Files.write(outputPath, outRows, StandardCharsets.UTF_8);
        System.out.println("native template structures: " + outRows.size() + " cases");
    }

    static Object buildSyntheticTemplate(JsonArray blocksJson) throws Exception {
        Object tmpl = templateC.getConstructor().newInstance();
        Field fSize = templateC.getDeclaredField("n");
        fSize.setAccessible(true);
        fSize.set(tmpl, Class.forName("kh").getConstructor(int.class, int.class, int.class).newInstance(8, 8, 8));
        Constructor<?> infoCtor = placementInfoC.getConstructor(posC, stateC, compoundC);
        List<Object> list = new ArrayList<>();
        for (JsonElement el : blocksJson) {
            JsonObject bo = el.getAsJsonObject();
            Object nbt = null;
            if (bo.has("nbt")) {
                nbt = Class.forName("uz").getMethod("a", String.class).invoke(null, bo.get("nbt").getAsString());
            }
            list.add(infoCtor.newInstance(pos(bo.getAsJsonArray("pos")), NativeHarness.stateFromJson(bo.get("state")), nbt));
        }
        Constructor<?> palCtor = Class.forName("ent$a").getDeclaredConstructor(List.class);
        palCtor.setAccessible(true);
        Field fPalettes = templateC.getDeclaredField("l");
        fPalettes.setAccessible(true);
        List<Object> pals = new ArrayList<>();
        pals.add(palCtor.newInstance(list));
        fPalettes.set(tmpl, pals);
        return tmpl;
    }

    static String formatWorldRow(String kind, String id, int count, OracleWorld world, long extra1, int extra2) throws Exception {
        // Canonical sorted writes (by x, y, z)
        List<Map.Entry<String, Object>> writeEntries = new ArrayList<>(world.inner.writes.entrySet());
        writeEntries.sort((a, b) -> {
            String[] pa = a.getKey().split(","), pb = b.getKey().split(",");
            for (int i = 0; i < 3; i++) {
                int cmp = Integer.compare(Integer.parseInt(pa[i]), Integer.parseInt(pb[i]));
                if (cmp != 0) return cmp;
            }
            return 0;
        });
        ByteArrayOutputStream wb = new ByteArrayOutputStream();
        DataOutputStream wdos = new DataOutputStream(wb);
        wdos.writeInt(writeEntries.size());
        for (var e : writeEntries) {
            wdos.writeUTF(e.getKey());
            wdos.writeUTF(NativeHarness.normalizedStateName(e.getValue()));
        }

        // Canonical sorted block entities / loot tables / containers
        List<Map.Entry<String, Object>> beEntries = new ArrayList<>(world.blockEntities.entrySet());
        beEntries.sort((a, b) -> {
            String[] pa = a.getKey().split(","), pb = b.getKey().split(",");
            for (int i = 0; i < 3; i++) {
                int cmp = Integer.compare(Integer.parseInt(pa[i]), Integer.parseInt(pb[i]));
                if (cmp != 0) return cmp;
            }
            return 0;
        });
        List<String> beSummaries = new ArrayList<>();
        for (var e : beEntries) {
            Object be = e.getValue();
            Object tag = blockEntityC.getMethod("b", Class.forName("jo$a")).invoke(be, regAccess);
            String beId = (String) compoundC.getMethod("l", String.class).invoke(tag, "id");
            String loot = "";
            long lootSeed = 0L;
            if (randomizableC.isInstance(be)) {
                Object lt = randomizableC.getMethod("aB_").invoke(be);
                if (lt != null) {
                    loot = String.valueOf(lt.getClass().getMethod("a").invoke(lt));
                    lootSeed = (long) randomizableC.getMethod("aC_").invoke(be);
                }
            } else if (Boolean.TRUE.equals(compoundC.getMethod("e", String.class).invoke(tag, "LootTable"))) {
                loot = (String) compoundC.getMethod("l", String.class).invoke(tag, "LootTable");
                if (Boolean.TRUE.equals(compoundC.getMethod("e", String.class).invoke(tag, "LootTableSeed"))) {
                    lootSeed = (long) compoundC.getMethod("i", String.class).invoke(tag, "LootTableSeed");
                }
            }
            String itemsHash = "";
            if (Boolean.TRUE.equals(compoundC.getMethod("e", String.class).invoke(tag, "Items"))) {
                Object items = compoundC.getMethod("c", String.class).invoke(tag, "Items");
                if (items instanceof List<?> itemList && !itemList.isEmpty()) {
                    ByteArrayOutputStream ib = new ByteArrayOutputStream();
                    StructureTemplateOracle.named(items, new DataOutputStream(ib));
                    itemsHash = StructureTemplateOracle.hash(ib.toByteArray());
                }
            }
            beSummaries.add(e.getKey() + "|" + beId + "|" + loot + "|" + lootSeed + "|" + itemsHash);
        }
        String beHash = beSummaries.isEmpty()
                ? "none"
                : StructureTemplateOracle.hash(String.join("\n", beSummaries).getBytes(StandardCharsets.UTF_8));

        // Entities (typeId@x,y,z) sorted
        List<String> ents = new ArrayList<>(world.inner.entities);
        Collections.sort(ents);
        String entHash = ents.isEmpty()
                ? "none"
                : StructureTemplateOracle.hash(String.join("\n", ents).getBytes(StandardCharsets.UTF_8));

        return kind + "\t" + id + "\t" + count + "\t" + StructureTemplateOracle.hash(wb.toByteArray())
                + "\t" + beEntries.size() + "\t" + beHash
                + "\t" + ents.size() + "\t" + entHash
                + "\t" + extra1 + "\t" + extra2;
    }
}
