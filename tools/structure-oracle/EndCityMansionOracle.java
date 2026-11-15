package dev.mcspine.tools;

import com.google.gson.*;
import com.mojang.serialization.Codec;
import com.mojang.serialization.JsonOps;
import java.io.*;
import java.lang.reflect.*;
import java.nio.charset.StandardCharsets;
import java.nio.file.*;
import java.util.*;

/**
 * Native 1.21.1 oracle for {@code minecraft:end_city} (EndCityStructure /
 * EndCityPieces) and {@code minecraft:mansion} (WoodlandMansionStructure /
 * WoodlandMansionPieces).
 *
 * <p>For each case this runs the real
 * {@code Structure.findGenerationPoint}, records the generation RNG state
 * between the stub and the lazy piece consumer (exactly where the port's
 * {@code rngState()} probe lands), builds the piece list, and then drives
 * {@code StructureStart.placeInChunk} over a list of chunk boxes sharing one
 * {@code RandomSource}. That is the native multi-chunk placement path: the
 * pieces whose bounding box intersects the chunk are post-processed in list
 * order and {@code Structure.afterPlace} runs once after them, which is where
 * the mansion's cobblestone foundation comes from.
 *
 * <p>Entity payloads are recorded as registry ids ({@code EntityType.getKey}),
 * not as obfuscated class names, so the fixture does not encode the jar's
 * package layout.
 */
public final class EndCityMansionOracle {
    static Class<?> stateC, posC, rlC, blockEntityC, compoundC, bboxC, chunkPosC;
    static Class<?> genCtxC, genStubC, piecesBuilderC, structureC, piecesContainerC;
    static Class<?> heightAccC, registryAccessC, biomeSourceC, chunkGenC, randomStateC;
    static Class<?> enumRotationC, enumMirrorC, structurePieceC, templatePieceC, entityC, entityTypeC;

    static Object ops, regAccess, randomState, templateManager, warmBiomeHolder;
    static Object genBiomeSourceC, genChunkGenC;

    static void init() throws Exception {
        // `TemplateStructureOracle.init()` owns the shared reflective bootstrap
        // (state/pos/bounding-box/context classes, the registry-backed ops, the
        // allocated `RandomState` and structure manager). Reusing it keeps this
        // oracle from drifting away from the one the other structure oracles use.
        TemplateStructureOracle.init();
        stateC = TemplateStructureOracle.stateC;
        posC = TemplateStructureOracle.posC;
        blockEntityC = TemplateStructureOracle.blockEntityC;
        compoundC = TemplateStructureOracle.compoundC;
        bboxC = TemplateStructureOracle.bboxC;
        chunkPosC = TemplateStructureOracle.chunkPosC;
        genCtxC = TemplateStructureOracle.genCtxC;
        genStubC = TemplateStructureOracle.genStubC;
        piecesBuilderC = TemplateStructureOracle.piecesBuilderC;
        structureC = TemplateStructureOracle.structureC;
        piecesContainerC = Class.forName("ekg");
        heightAccC = TemplateStructureOracle.heightAccC;
        registryAccessC = TemplateStructureOracle.registryAccessC;
        biomeSourceC = TemplateStructureOracle.biomeSourceC;
        chunkGenC = TemplateStructureOracle.chunkGenC;
        randomStateC = TemplateStructureOracle.randomStateC;
        enumRotationC = Class.forName("dmm");
        enumMirrorC = Class.forName("dkv");
        structurePieceC = Class.forName("ejv");
        templatePieceC = Class.forName("ekb");
        entityC = Class.forName("bsr");
        entityTypeC = Class.forName("bsx");

        ops = TemplateStructureOracle.ops;
        regAccess = TemplateStructureOracle.regAccess;
        randomState = TemplateStructureOracle.randomState;
        warmBiomeHolder = TemplateStructureOracle.loadBiome(
                Path.of(System.getProperty("user.dir")), "plains");
    }

    @SuppressWarnings("unchecked")
    static Object newContext(Object cg, Object bs, long worldSeed, Object chunkPos,
                             TemplateStructureOracle.ScriptedTerrain terrain) throws Exception {
        Object heightAcc = heightAccC.getMethod("e", int.class, int.class)
                .invoke(null, terrain.minY, terrain.height);
        java.util.function.Predicate<Object> validBiome = h -> true;
        return genCtxC.getConstructor(
                registryAccessC, chunkGenC, biomeSourceC, randomStateC,
                Class.forName("enu"), long.class, chunkPosC, heightAccC,
                java.util.function.Predicate.class)
                .newInstance(regAccess, cg, bs, randomState, templateManager, worldSeed, chunkPos, heightAcc, validBiome);
    }

    static Object pos(int x, int y, int z) throws Exception {
        return posC.getConstructor(int.class, int.class, int.class).newInstance(x, y, z);
    }

    static int[] xyz(Object p) throws Exception {
        return new int[]{
                (int) posC.getMethod("u").invoke(p),
                (int) posC.getMethod("v").invoke(p),
                (int) posC.getMethod("w").invoke(p)
        };
    }

    static Object parseStructure(Path file) throws Exception {
        JsonObject obj = JsonParser.parseString(Files.readString(file)).getAsJsonObject();
        String type = obj.get("type").getAsString();
        String obf = switch (type) {
            case "minecraft:end_city" -> "elp";
            case "minecraft:woodland_mansion" -> "emo";
            default -> throw new AssertionError("unexpected structure type " + type);
        };
        Class<?> cls = Class.forName(obf);
        com.mojang.serialization.MapCodec<?> mc =
                (com.mojang.serialization.MapCodec<?>) cls.getField("d").get(null);
        JsonObject patched = obj.deepCopy();
        patched.add("biomes", new JsonArray());
        return NativeHarness.getOrThrow(TemplateStructureOracle.parseWithOps(mc.codec(), patched));
    }

    // ------------------------------------------------------------------ world

    /**
     * A scripted {@code ServerLevelAccessor}: blocks and block entities are
     * recorded, entities are recorded by registry id, and every read the native
     * code performs is served from the same scripted terrain the port uses.
     */
    static final class World {
        final NativeHarness.Level inner;
        final Object proxy;
        final Map<String, Object> blockEntities = new LinkedHashMap<>();
        final List<String> entityIds = new ArrayList<>();
        final Object levelRng;
        final Object biomeHolder;
        final Object difficulty;
        final int seaLevel;
        final Object dimType;
        final int minY, height;

        World(TemplateStructureOracle.ScriptedTerrain terrain, long worldSeed, Object biomeHolder,
              long levelRngSeed) throws Exception {
            this.minY = terrain.minY;
            this.height = terrain.height;
            this.inner = NativeHarness.levelFromStates(terrain::stateAt, terrain.minY, terrain.height);
            this.inner.worldSeed = worldSeed;
            // `WaterloggedBlock`/fluid placement asks the level for its light
            // engine while copying fluid state. The harness's opt-in engine is a
            // real (empty, both engines disabled) one; a scripted level has no
            // lighting context to offer instead.
            this.inner.useUninitializedLightEngine = true;
            this.inner.entityServerLevel = TemplateStructureOracle.minimalWorldServerLevel(worldSeed);
            prepareEntityLevel(this.inner.entityServerLevel);
            this.seaLevel = terrain.seaLevel;
            this.dimType = TemplateStructureOracle.buildDimType(terrain.minY, terrain.height, false);
            this.biomeHolder = biomeHolder;
            this.levelRng = NativeHarness.rng(levelRngSeed);
            this.difficulty = TemplateStructureOracle.difficultyAt();
            this.proxy = Proxy.newProxyInstance(
                    EndCityMansionOracle.class.getClassLoader(),
                    new Class<?>[]{Class.forName("dds")},
                    this::handle);
        }

        private Object handle(Object p, Method m, Object[] a) throws Throwable {
            Class<?>[] pt = m.getParameterTypes();
            String name = m.getName();
            if (name.equals("c_") && pt.length == 1 && pt[0] == posC) {
                int[] p3 = xyz(a[0]);
                return blockEntities.get(p3[0] + "," + p3[1] + "," + p3[2]);
            }
            if (name.equals("H_") && pt.length == 0) {
                return regAccess;
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
                return false;
            }
            if (name.equals("z_") && pt.length == 0) {
                return seaLevel;
            }
            if (name.equals("t") && pt.length == 1 && pt[0] == posC) {
                return biomeHolder;
            }
            if (name.equals("E") && pt.length == 0) {
                return inner.entityServerLevel;
            }
            if ((name.equals("a_") || name.equals("b")) && pt.length == 1 && pt[0] == entityC) {
                Object type = entityC.getMethod("am").invoke(a[0]);
                Object key = entityTypeC.getMethod("a", entityTypeC).invoke(null, type);
                int[] p3 = xyz(entityC.getMethod("do").invoke(a[0]));
                entityIds.add(String.valueOf(key) + "@" + p3[0] + "," + p3[1] + "," + p3[2]);
                return name.equals("b");
            }
            if (name.equals("a") && (pt.length == 3 || pt.length == 4)
                    && pt[0] == posC && pt[1] == stateC && pt[2] == int.class) {
                int[] p3 = xyz(a[0]);
                String key = p3[0] + "," + p3[1] + "," + p3[2];
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

        Object newBlockEntityForState(Object pos, Object state) throws Exception {
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
    }

    /**
     * The minimal `ServerLevel` is allocated with `Unsafe`, so its
     * `RandomSource` field is null. `ItemFrame`'s constructor reaches
     * `Level.getRandom()` (the elytra data marker builds one), so bind a real
     * stream: it is the level's own noise, not the placement stream, and the
     * fixture only records the entity's block position.
     */
    /** `x,y,z` -> `beId|loot|seed`, sorted by position. */
    static Map<String, String> sortedBe(World world) throws Exception {
        List<String> keys = new ArrayList<>(world.blockEntities.keySet());
        keys.sort(EndCityMansionOracle::compareXyz);
        Map<String, String> out = new LinkedHashMap<>();
        for (String key : keys) {
            Object be = world.blockEntities.get(key);
            Object tag = blockEntityC.getMethod("b", Class.forName("jo$a")).invoke(be, regAccess);
            String beId = (String) compoundC.getMethod("l", String.class).invoke(tag, "id");
            String loot = "";
            long lootSeed = 0L;
            Class<?> randomizableC = Class.forName("bqz");
            if (randomizableC.isInstance(be)) {
                Object lt = randomizableC.getMethod("aB_").invoke(be);
                if (lt != null) {
                    loot = String.valueOf(lt.getClass().getMethod("a").invoke(lt));
                    lootSeed = (long) randomizableC.getMethod("aC_").invoke(be);
                }
            }
            out.put(key, beId + "|" + loot + "|" + lootSeed);
        }
        return out;
    }

    static Object unsafe() throws Exception {
        Field uf = Class.forName("sun.misc.Unsafe").getDeclaredField("theUnsafe");
        uf.setAccessible(true);
        return uf.get(null);
    }

    static void putField(Object unsafe, Object target, Class<?> owner, String name, Object value)
            throws Exception {
        Field field = owner.getDeclaredField(name);
        long offset = (long) unsafe.getClass().getMethod("objectFieldOffset", Field.class).invoke(unsafe, field);
        unsafe.getClass().getMethod("putObject", Object.class, long.class, Object.class)
                .invoke(unsafe, target, offset, value);
    }

    static Object allocate(Object unsafe, Class<?> c) throws Exception {
        return unsafe.getClass().getMethod("allocateInstance", Class.class).invoke(unsafe, c);
    }

    /**
     * Give the `Unsafe`-allocated `ServerLevel` the noise a real one has.
     *
     * The elytra data marker builds a `minecraft:item_frame`, whose constructor
     * reaches `Entity.playSound` → `Level.playSound` → the level's own
     * `RandomSource` and then `MinecraftServer.getPlayerList().broadcast(...)`.
     * None of that noise touches the placement stream: the fixture records the
     * entity's type and block position, and nothing else in this oracle draws
     * from the level's RNG or from the player list.
     */
    static void prepareEntityLevel(Object serverLevel) throws Exception {
        Object unsafe = unsafe();
        Object rng = NativeHarness.rng(1234L);
        Class<?> levelC = Class.forName("dcw");
        putField(unsafe, serverLevel, levelC, "f", rng);
        putField(unsafe, serverLevel, levelC, "z", rng);
        Field serverField = Class.forName("aqu").getDeclaredField("J");
        serverField.setAccessible(true);
        Object server = serverField.get(serverLevel);
        if (server == null) {
            return;
        }
        Class<?> playerListC = Class.forName("aur");
        Object playerList = emptyPlayerList(unsafe);
        for (Class<?> c = server.getClass(); c != null; c = c.getSuperclass()) {
            for (Field f : c.getDeclaredFields()) {
                if (f.getType() == playerListC) {
                    putField(unsafe, server, c, f.getName(), playerList);
                }
            }
        }
    }

    /**
     * An empty `PlayerList`. `MinecraftServer.getPlayerList()` narrows its
     * `PlayerList` field to `DedicatedPlayerList`, so that is the class to
     * allocate; it is constructed with `Unsafe` and only the player list field
     * is bound, to an empty `ArrayList`. The broadcast native performs when the
     * elytra data marker's item frame plays its place sound then iterates
     * nothing.
     */
    static Object emptyPlayerList(Object unsafe) throws Exception {
        if (EMPTY_PLAYER_LIST != null) {
            return EMPTY_PLAYER_LIST;
        }
        Object list = allocate(unsafe, Class.forName("apm"));
        putField(unsafe, list, Class.forName("aur"), "l", new java.util.ArrayList<>());
        EMPTY_PLAYER_LIST = list;
        return list;
    }

    static Object EMPTY_PLAYER_LIST;

    /** One `MansionGrid`/`floorRooms` grid as a row of rows. */
    static String gridRow(String kind, String id, String name, Object grid, String which)
            throws Exception {
        Object target;
        switch (which) {
            case "r" -> target = field(grid, "r");
            case "s" -> target = field(grid, "s");
            case "t0" -> target = ((Object[]) field(grid, "t"))[0];
            case "t1" -> target = ((Object[]) field(grid, "t"))[1];
            default -> target = ((Object[]) field(grid, "t"))[2];
        }
        int width = (int) field(target, "b");
        int height = (int) field(target, "c");
        Method get = Class.forName("emn$g").getDeclaredMethod("a", int.class, int.class);
        get.setAccessible(true);
        StringBuilder cells = new StringBuilder();
        for (int y = 0; y < height; y++) {
            if (y > 0) {
                cells.append(';');
            }
            for (int x = 0; x < width; x++) {
                if (x > 0) {
                    cells.append(',');
                }
                cells.append(get.invoke(target, x, y));
            }
        }
        return String.join("\t", kind, id, name,
                String.valueOf((int) field(grid, "u")),
                String.valueOf((int) field(grid, "v")),
                cells.toString());
    }

    static Object field(Object target, String name) throws Exception {
        Field f = target.getClass().getDeclaredField(name);
        f.setAccessible(true);
        return f.get(target);
    }

    // ------------------------------------------------------------------ rows

    static String hash(byte[] bytes) throws Exception {
        return StructureTemplateOracle.hash(bytes);
    }

    static void writeUtf(DataOutputStream out, String text) throws IOException {
        out.writeUTF(text);
    }

    static String worldRow(String kind, String id, int admitted, String stub, int pieces,
                           String geometry, World world, long genProbe, int postProbe) throws Exception {
        List<Map.Entry<String, Object>> writes = new ArrayList<>(world.inner.writes.entrySet());
        writes.sort((l, r) -> compareXyz(l.getKey(), r.getKey()));
        ByteArrayOutputStream wb = new ByteArrayOutputStream();
        DataOutputStream wdos = new DataOutputStream(wb);
        wdos.writeInt(writes.size());
        for (var e : writes) {
            writeUtf(wdos, e.getKey());
            writeUtf(wdos, NativeHarness.normalizedStateName(e.getValue()));
        }

        List<Map.Entry<String, Object>> beEntries = new ArrayList<>(world.blockEntities.entrySet());
        beEntries.sort((l, r) -> compareXyz(l.getKey(), r.getKey()));
        List<String> beSummaries = new ArrayList<>();
        for (var e : beEntries) {
            Object be = e.getValue();
            Object tag = blockEntityC.getMethod("b", Class.forName("jo$a")).invoke(be, regAccess);
            String beId = (String) compoundC.getMethod("l", String.class).invoke(tag, "id");
            String loot = "";
            long lootSeed = 0L;
            Class<?> randomizableC = Class.forName("bqz");
            if (randomizableC.isInstance(be)) {
                Object lt = randomizableC.getMethod("aB_").invoke(be);
                if (lt != null) {
                    loot = String.valueOf(lt.getClass().getMethod("a").invoke(lt));
                    lootSeed = (long) randomizableC.getMethod("aC_").invoke(be);
                }
            }
            beSummaries.add(e.getKey() + "|" + beId + "|" + loot + "|" + lootSeed);
        }
        String beHash = beSummaries.isEmpty()
                ? "none"
                : hash(String.join("\n", beSummaries).getBytes(StandardCharsets.UTF_8));

        List<String> ents = new ArrayList<>(world.entityIds);
        Collections.sort(ents);
        String entHash = ents.isEmpty()
                ? "none"
                : hash(String.join("\n", ents).getBytes(StandardCharsets.UTF_8));

        return String.join("\t", kind, id, String.valueOf(admitted), stub, String.valueOf(pieces),
                geometry, String.valueOf(writes.size()), hash(wb.toByteArray()),
                String.valueOf(beEntries.size()), beHash,
                String.valueOf(ents.size()), entHash,
                String.valueOf(genProbe), String.valueOf(postProbe));
    }

    /** `(x, y, z)` order, the same key the port's fixture writer sorts by. */
    static int compareXyz(String left, String right) {
        String[] a = left.split(",");
        String[] b = right.split(",");
        for (int i = 0; i < 3; i++) {
            int cmp = Integer.compare(Integer.parseInt(a[i]), Integer.parseInt(b[i]));
            if (cmp != 0) {
                return cmp;
            }
        }
        return 0;
    }

    public static void main(String[] args) throws Throwable {
        Path inputPath = Path.of(args[0]);
        Path outputPath = Path.of(args[1]);
        Path repoRoot = Path.of(args[2]);
        System.setProperty("user.dir", repoRoot.toString());
        init();
        Class<?> genBiomeSourceClass = TemplateStructureOracle.buildBiomeSourceClass();
        Class<?> genChunkGenClass = TemplateStructureOracle.buildChunkGeneratorClass();
        templateManager = TemplateStructureOracle.buildTemplateManager(repoRoot);

        JsonObject root = JsonParser.parseString(Files.readString(inputPath)).getAsJsonObject();
        List<String> outRows = new ArrayList<>();
        for (JsonElement el : root.getAsJsonArray("structures")) {
            JsonObject c = el.getAsJsonObject();
            String id = c.get("id").getAsString();
            String kind = c.get("kind").getAsString();
            Object structure = parseStructure(repoRoot.resolve(c.get("structure").getAsString()));
            var terrain = new TemplateStructureOracle.ScriptedTerrain(c.getAsJsonObject("terrain"));
            TemplateStructureOracle.activeTerrain = terrain;
            TemplateStructureOracle.activeBiomeHolder = warmBiomeHolder;
            Object bs = genBiomeSourceClass.getConstructor().newInstance();
            Object cg = genChunkGenClass.getConstructor(biomeSourceC).newInstance(bs);
            long worldSeed = c.get("world_seed").getAsLong();
            int cx = c.get("cx").getAsInt();
            int cz = c.get("cz").getAsInt();
            Object chunkPos = chunkPosC.getConstructor(int.class, int.class).newInstance(cx, cz);
            Object gctx = newContext(cg, bs, worldSeed, chunkPos, terrain);
            @SuppressWarnings("unchecked")
            Optional<Object> stubOpt = (Optional<Object>) structureC.getMethod("b", genCtxC).invoke(structure, gctx);
            long genProbe = NativeHarness.rngState(genCtxC.getMethod("f").invoke(gctx));
            String rowKind = kind;
            if (stubOpt.isEmpty()) {
                outRows.add(String.join("\t", rowKind, id, "0", "none", "0", "none", "0", "none",
                        "0", "none", "0", "none", String.valueOf(genProbe), "0"));
                continue;
            }
            Object stub = stubOpt.get();
            Object stubPos = genStubC.getMethod("b").invoke(stub);
            int[] stubXyz = xyz(stubPos);
            if (kind.equals("mansion")) {
                // Dump the two layout grids the piece placer reads. The grid is
                // built from the generation stream, so a second, identical
                // `GenerationContext` is advanced to the same point and its
                // `new MansionGrid(random)` is dumped. That turns "the piece
                // list differs" into "grid cell (4, 7) is BLOCKED in the port".
                Object gctxGrid = newContext(cg, bs, worldSeed, chunkPos, terrain);
                structureC.getMethod("b", genCtxC).invoke(structure, gctxGrid);
                Object gridRng = genCtxC.getMethod("f").invoke(gctxGrid);
                NativeHarness.rngState(gridRng);
                Constructor<?> gridCtor = Class.forName("emn$c")
                        .getDeclaredConstructor(Class.forName("ayw"));
                gridCtor.setAccessible(true);
                Object grid = gridCtor.newInstance(gridRng);
                for (String which : new String[]{"r:base", "s:third", "t0:rooms0", "t1:rooms1", "t2:rooms2"}) {
                    String[] parts = which.split(":");
                    outRows.add(gridRow("grid", id, parts[1], grid, parts[0]));
                }
            }
            Object builder = genStubC.getMethod("a").invoke(stub);
            Field fPieces = piecesBuilderC.getDeclaredField("a");
            fPieces.setAccessible(true);
            List<?> pieces = (List<?>) fPieces.get(builder);

            Field fTemplateName = templatePieceC.getDeclaredField("a");
            fTemplateName.setAccessible(true);
            Field fSettings = templatePieceC.getDeclaredField("c");
            fSettings.setAccessible(true);

            ByteArrayOutputStream gb = new ByteArrayOutputStream();
            DataOutputStream gdos = new DataOutputStream(gb);
            gdos.writeInt(pieces.size());
            for (int index = 0; index < pieces.size(); index++) {
                Object piece = pieces.get(index);
                Object bb = structurePieceC.getMethod("f").invoke(piece);
                String templateName = String.valueOf(fTemplateName.get(piece));
                Object settings = fSettings.get(piece);
                int rotation = ((Enum<?>) settings.getClass().getMethod("d").invoke(settings)).ordinal();
                int mirror = ((Enum<?>) settings.getClass().getMethod("c").invoke(settings)).ordinal();
                int genDepth = (int) structurePieceC.getMethod("g").invoke(piece);
                Map<String, Integer> names = new LinkedHashMap<>();
                names.put("minX", (int) bboxC.getMethod("h").invoke(bb));
                names.put("minY", (int) bboxC.getMethod("i").invoke(bb));
                names.put("minZ", (int) bboxC.getMethod("j").invoke(bb));
                names.put("maxX", (int) bboxC.getMethod("k").invoke(bb));
                names.put("maxY", (int) bboxC.getMethod("l").invoke(bb));
                names.put("maxZ", (int) bboxC.getMethod("m").invoke(bb));
                String box = names.get("minX") + "," + names.get("minY") + "," + names.get("minZ")
                        + "," + names.get("maxX") + "," + names.get("maxY") + "," + names.get("maxZ");
                gdos.writeUTF(templateName);
                for (int v : names.values()) {
                    gdos.writeInt(v);
                }
                gdos.writeInt(rotation);
                gdos.writeInt(mirror);
                gdos.writeInt(genDepth);
                outRows.add(String.join("\t", "pieces", id, String.valueOf(index), templateName,
                        box, String.valueOf(rotation), String.valueOf(mirror), String.valueOf(genDepth)));
            }
            String geometry = hash(gb.toByteArray());

            World world = new World(terrain, worldSeed, warmBiomeHolder, 999L);
            Object postRng = NativeHarness.rng(c.get("post_seed").getAsLong());
            Object piecesContainer = piecesContainerC.getConstructor(List.class).newInstance(pieces);
            Object structureStart = Class.forName("ejz").getConstructor(
                    structureC, chunkPosC, int.class, piecesContainerC)
                    .newInstance(structure, chunkPos, 0, piecesContainer);
            Method placeInChunk = Class.forName("ejz").getMethod(
                    "a", Class.forName("dds"), Class.forName("ddq"), chunkGenC,
                    Class.forName("ayw"), bboxC, chunkPosC);
            for (JsonElement chunkEl : c.getAsJsonArray("chunks")) {
                JsonArray arr = chunkEl.getAsJsonArray();
                int ccx = arr.get(0).getAsInt();
                int ccz = arr.get(1).getAsInt();
                int minX = ccx << 4;
                int minZ = ccz << 4;
                Object box = bboxC.getConstructor(int.class, int.class, int.class, int.class, int.class, int.class)
                        .newInstance(minX, terrain.minY, minZ, minX + 15, terrain.minY + terrain.height - 1, minZ + 15);
                Object cpos = chunkPosC.getConstructor(int.class, int.class).newInstance(ccx, ccz);
                placeInChunk.invoke(structureStart, world.proxy,
                        TemplateStructureOracle.defaultStructureManager, cg, postRng, box, cpos);
            }
            int postProbe = (int) Class.forName("ayw").getMethod("a", int.class).invoke(postRng, 1000003);
            for (var entry : sortedBe(world).entrySet()) {
                outRows.add(String.join("\t", "be", id, entry.getKey(), entry.getValue()));
            }
            outRows.add(worldRow(rowKind, id, 1,
                    stubXyz[0] + "," + stubXyz[1] + "," + stubXyz[2],
                    pieces.size(), geometry, world, genProbe, postProbe));
        }
        Files.write(outputPath, outRows, StandardCharsets.UTF_8);
        System.out.println("native end city / mansion: " + outRows.size() + " rows");
    }
}
