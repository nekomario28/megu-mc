package dev.megu.tools;

import java.lang.reflect.InvocationHandler;
import java.lang.reflect.*;
import java.nio.charset.StandardCharsets;
import java.nio.file.*;
import java.security.MessageDigest;
import java.util.*;
import java.util.concurrent.CompletableFuture;
import java.util.concurrent.TimeUnit;

/**
 * Native 1.21.1 oracle for `minecraft:stronghold` and for biome-aware
 * `ConcentricRingsStructurePlacement` relocation.
 *
 * Two arms, both driven by the real classes (never a re-implementation):
 *
 * 1. stronghold: the `StrongholdStructure.generatePieces` loop is reproduced as a
 *    *driver* only (clear / setLargeFeatureSeed / StartPiece / pending-children
 *    shuffle / moveBelowSeaLevel, all native calls), because the loop itself is
 *    plain control flow over `StrongholdPieces` statics. Piece construction,
 *    piece weights, child placement and `postProcess` are all native. The
 *    piece stream is the one `Structure.GenerationContext` builds
 *    (`WorldgenRandom(new LegacyRandomSource(0L))`, legacy LCG); only the
 *    `postProcess` arm uses the Xoroshiro decoration stream, as
 *    `applyBiomeDecoration` does.
 * 2. rings: `ChunkGeneratorStructureState.generateRingPositions` over a real
 *    Overworld `MultiNoiseBiomeSource`, so `BiomeSource.findBiomeHorizontal`
 *    (the quart spiral) runs against the actual noise biome source.
 *
 * Output: one TSV row per case, see `tools/stronghold_oracle.py` for the schema.
 */
public final class StrongholdOracle {
    static final int MIN_Y = NativeHarness.MIN_Y;
    static final int HEIGHT = NativeHarness.HEIGHT;
    static final int SEA_LEVEL = 63;

    static Class<?> c(String name) { return NativeHarness.C(name); }

    /** Obfuscated class -> official simple name, for the piece signature column. */
    static final Map<String, String> PIECE_NAMES = new LinkedHashMap<>();

    static {
        PIECE_NAMES.put("emj$m", "StartPiece");
        PIECE_NAMES.put("emj$l", "StairsDown");
        PIECE_NAMES.put("emj$n", "Straight");
        PIECE_NAMES.put("emj$d", "LeftTurn");
        PIECE_NAMES.put("emj$i", "RightTurn");
        PIECE_NAMES.put("emj$j", "RoomCrossing");
        PIECE_NAMES.put("emj$o", "StraightStairsDown");
        PIECE_NAMES.put("emj$c", "FiveCrossing");
        PIECE_NAMES.put("emj$a", "ChestCorridor");
        PIECE_NAMES.put("emj$e", "Library");
        PIECE_NAMES.put("emj$g", "PortalRoom");
        PIECE_NAMES.put("emj$b", "FillerCorridor");
        PIECE_NAMES.put("emj$h", "PrisonHall");
    }

    static String pieceName(Object piece) {
        String raw = piece.getClass().getName(); // "emj$m": the classes are in the default package
        String name = PIECE_NAMES.get(raw);
        if (name == null) throw new AssertionError("unmapped piece class " + raw);
        return name;
    }

    // ---------- RNG ----------

    static Object xorRng(long seed) throws Exception {
        return c("dzx").getConstructor(c("ayw"))
                .newInstance(c("dzz").getConstructor(long.class).newInstance(seed));
    }

    static Object legacyRng(long seed) throws Exception {
        return c("dyz").getConstructor(long.class).newInstance(seed);
    }

    /**
     * The RNG `Structure.GenerationContext` builds: `new WorldgenRandom(new
     * LegacyRandomSource(0L))`, i.e. the legacy LCG, **not** the Xoroshiro source
     * `applyBiomeDecoration` hands to `placeInChunk`. Using `xorRng` here made the
     * oracle agree with a port that had the same bug.
     */
    static Object legacyWorldgenRng(long seed) throws Exception {
        return c("dzx").getConstructor(c("ayw")).newInstance(legacyRng(seed));
    }

    static long nextLong(Object rng) throws Exception {
        return (long) c("ayw").getMethod("g").invoke(rng);
    }

    // ---------- bounding boxes ----------

    static Object bb(int x0, int y0, int z0, int x1, int y1, int z1) throws Exception {
        return c("ejj").getConstructor(int.class, int.class, int.class, int.class, int.class, int.class)
                .newInstance(x0, y0, z0, x1, y1, z1);
    }

    static int[] bbCoords(Object box) throws Exception {
        Class<?> ejj = c("ejj");
        return new int[] {
            (int) ejj.getMethod("h").invoke(box),
            (int) ejj.getMethod("i").invoke(box),
            (int) ejj.getMethod("j").invoke(box),
            (int) ejj.getMethod("k").invoke(box),
            (int) ejj.getMethod("l").invoke(box),
            (int) ejj.getMethod("m").invoke(box),
        };
    }

    static String fmtBox(int[] b) {
        return b[0] + "," + b[1] + "," + b[2] + "," + b[3] + "," + b[4] + "," + b[5];
    }

    static String dirName(Object d) throws Exception {
        return d == null ? "none" : (String) c("ji").getMethod("n").invoke(d);
    }

    static int[] parseBox(String s) {
        String[] p = s.split(",");
        return new int[] {
            Integer.parseInt(p[0]), Integer.parseInt(p[1]), Integer.parseInt(p[2]),
            Integer.parseInt(p[3]), Integer.parseInt(p[4]), Integer.parseInt(p[5]),
        };
    }

    // ---------- structure generation driver ----------

    static Object generationBuilder() throws Exception {
        return c("ekj").getConstructor().newInstance();
    }

    static List<?> builderPieces(Object builder) throws Exception {
        Field f = c("ekj").getDeclaredField("a");
        f.setAccessible(true);
        return (List<?>) f.get(builder);
    }

    /** One `generatePieces` retry-loop iteration, assembled from native calls. */
    static Object oneAttempt(Object builder, Object rng, long seed, int cx, int cz, int i)
            throws Exception {
        c("ekj").getMethod("b").invoke(builder); // clear
        c("dzx").getMethod("c", long.class, int.class, int.class)
                .invoke(rng, seed + (long) i, cx, cz); // setLargeFeatureSeed
        c("emj").getMethod("a").invoke(null); // resetPieces
        Object start = c("emj$m").getConstructor(c("ayw"), int.class, int.class)
                .newInstance(rng, (cx << 4) + 2, (cz << 4) + 2);
        c("ekj").getMethod("a", c("ejv")).invoke(builder, start);
        c("ejv").getMethod("a", c("ejv"), c("ejw"), c("ayw")).invoke(start, start, builder, rng);
        Field pendingField = c("emj$m").getField("c");
        @SuppressWarnings("unchecked")
        List<Object> pending = (List<Object>) pendingField.get(start);
        while (!pending.isEmpty()) {
            int pick = (int) c("ayw").getMethod("a", int.class).invoke(rng, pending.size());
            Object piece = pending.remove(pick);
            c("ejv").getMethod("a", c("ejv"), c("ejw"), c("ayw"))
                    .invoke(piece, start, builder, rng);
        }
        c("ekj").getMethod("a", int.class, int.class, c("ayw"), int.class)
                .invoke(builder, SEA_LEVEL, MIN_Y, rng, 10); // moveBelowSeaLevel
        return start;
    }

    static final class Start {
        Object builder;
        Object startPiece;
        int attempts;
        long rngAfter;
        List<Object> pieces;
    }

    static Start generate(long seed, int cx, int cz) throws Exception {
        Object builder = generationBuilder();
        // `Structure.GenerationContext`, not the decoration stream: legacy LCG.
        Object rng = legacyWorldgenRng(0L);
        Start out = new Start();
        out.builder = builder;
        int attempts = 0;
        while (true) {
            attempts++;
            if (attempts > 4096) throw new AssertionError("stronghold retry loop did not converge");
            Object start = oneAttempt(builder, rng, seed, cx, cz, attempts - 1);
            boolean empty = (boolean) c("ekj").getMethod("c").invoke(builder);
            Object portal = c("emj$m").getField("b").get(start);
            if (!empty && portal != null) {
                out.startPiece = start;
                out.pieces = new ArrayList<>(builderPieces(builder));
                out.attempts = attempts;
                break;
            }
        }
        out.rngAfter = nextLong(rng);
        return out;
    }

    // ---------- piece signature ----------

    static int smallDoor(Object piece) throws Exception {
        Field f = c("emj$p").getDeclaredField("h");
        f.setAccessible(true);
        Object door = f.get(piece);
        return ((Enum<?>) door).ordinal();
    }

    /** Per-class flags the port has to reproduce; `-` when the class carries none. */
    static String flags(Object piece) throws Exception {
        String name = pieceName(piece);
        List<String> parts = new ArrayList<>();
        switch (name) {
            case "StartPiece":
            case "StairsDown": {
                Field f = c("emj$l").getDeclaredField("d");
                f.setAccessible(true);
                parts.add("source=" + ((boolean) f.get(piece) ? 1 : 0));
                break;
            }
            case "Straight": {
                Field l = c("emj$n").getDeclaredField("d");
                l.setAccessible(true);
                Field r = c("emj$n").getDeclaredField("i");
                r.setAccessible(true);
                parts.add("left=" + ((boolean) l.get(piece) ? 1 : 0));
                parts.add("right=" + ((boolean) r.get(piece) ? 1 : 0));
                break;
            }
            case "RoomCrossing": {
                Field t = c("emj$j").getDeclaredField("d");
                t.setAccessible(true);
                parts.add("type=" + t.getInt(piece));
                break;
            }
            case "FiveCrossing": {
                String[] names = {"d", "i", "j", "k"};
                String[] keys = {"leftLow", "leftHigh", "rightLow", "rightHigh"};
                for (int n = 0; n < names.length; n++) {
                    Field f = c("emj$c").getDeclaredField(names[n]);
                    f.setAccessible(true);
                    parts.add(keys[n] + "=" + ((boolean) f.get(piece) ? 1 : 0));
                }
                break;
            }
            case "ChestCorridor": {
                Field ch = c("emj$a").getDeclaredField("d");
                ch.setAccessible(true);
                parts.add("chest=" + ((boolean) ch.get(piece) ? 1 : 0));
                break;
            }
            case "Library": {
                Field t = c("emj$e").getDeclaredField("i");
                t.setAccessible(true);
                parts.add("tall=" + ((boolean) t.get(piece) ? 1 : 0));
                break;
            }
            case "PortalRoom": {
                Field m = c("emj$g").getDeclaredField("d");
                m.setAccessible(true);
                parts.add("mob=" + ((boolean) m.get(piece) ? 1 : 0));
                break;
            }
            default:
                break;
        }
        return parts.isEmpty() ? "-" : String.join(",", parts);
    }

    static String signature(List<Object> pieces) throws Exception {
        List<String> rows = new ArrayList<>();
        for (Object piece : pieces) {
            int[] b = bbCoords(c("ejv").getMethod("f").invoke(piece));
            String orient = dirName(c("ejv").getMethod("i").invoke(piece));
            int depth = (int) c("ejv").getMethod("g").invoke(piece);
            rows.add(pieceName(piece) + "," + fmtBox(b) + "," + depth + "," + orient + ","
                    + smallDoor(piece) + "," + flags(piece));
        }
        return String.join(";", rows);
    }

    // ---------- registry access / scratch chunks ----------

    static Object REGISTRY_ACCESS;

    /** The pinned pack's dynamic registries, loaded once (same path the ring arm uses). */
    static Object registryAccess() throws Exception {
        if (REGISTRY_ACCESS != null) return REGISTRY_ACCESS;
        Object access = c("ka").getMethod("a", c("jz")).invoke(null, field("lt", "aA"));
        Object pack = c("ats").getMethod("b").invoke(null);
        Object manager = c("atx").getConstructor(c("ass"), List.class)
                .newInstance(field("ass", "b"), List.of(pack));
        REGISTRY_ACCESS = c("akm").getMethod("a", c("aue"), c("ka"), List.class)
                .invoke(null, manager, access, field("akm", "a"));
        return REGISTRY_ACCESS;
    }

    static final Map<Long, Object> SCRATCH = new HashMap<>();

    /**
     * A real `ProtoChunk` for `getChunkAt`.
     *
     * `StructurePiece.placeBlock` marks bars/ladders/torches for post-processing
     * through `level.getChunkAt(pos).markPosForPostprocessing(pos)`. That is
     * bookkeeping with no block write and no RNG draw, but it needs a chunk: a
     * `ProtoChunk` only needs the biome registry, which the pinned pack has.
     */
    static Object chunkAt(Object blockPos) throws Exception {
        int[] p = NativeHarness.xyz(blockPos);
        long key = (((long) (p[0] >> 4)) << 32) ^ ((p[2] >> 4) & 0xFFFF_FFFFL);
        Object chunk = SCRATCH.get(key);
        if (chunk != null) return chunk;
        Object biomes = registryAccess().getClass().getMethod("d", c("akq"))
                .invoke(registryAccess(), field("lu", "aF"));
        chunk = c("dvs")
                .getConstructor(c("dcd"), c("dvv"), c("dcy"), c("jz"), c("eab"))
                .newInstance(
                        c("dcd").getConstructor(int.class, int.class)
                                .newInstance(p[0] >> 4, p[2] >> 4),
                        field("dvv", "a"),
                        c("dcy").getMethod("e", int.class, int.class).invoke(null, MIN_Y, HEIGHT),
                        biomes,
                        null);
        SCRATCH.put(key, chunk);
        return chunk;
    }

    // ---------- scene ----------

    /**
     * Terrain for a stronghold case: stone below an undulating surface with scattered
     * cave pockets, air above, and one water pocket so that a piece writing over a
     * liquid has to behave.
     *
     * The surface is not flat on purpose. `StructurePiece.generateBox(...)`'s
     * `skipAir` argument decides per voxel whether the selector draws at all, so a
     * scene that is stone below `ground` and air above makes every call take the same
     * side of that branch: a port whose air test disagreed with `BlockState.isAir()`
     * still reproduced every row. Only a scene with both kinds in one box can see it.
     */
    static final class Scene implements NativeHarness.StateScene {
        final int ground;
        final int waterY;

        Scene(int ground, int waterY) {
            this.ground = ground;
            this.waterY = waterY;
        }

        /** Surface height: `ground` +/- 3, deterministic in the column. */
        static int surface(int ground, int x, int z) {
            return ground + (((x * 31 + z * 17) >> 2 & 7) - 3);
        }

        @Override
        public Object stateAt(int x, int y, int z) {
            try {
                if (waterY != Integer.MIN_VALUE && y == waterY && ((x * 31 + z * 17) & 63) == 0) {
                    return NativeHarness.state("minecraft:water");
                }
                if (y < surface(ground, x, z)) {
                    if (((x * 13 + y * 7 + z * 5) & 15) == 0) {
                        return NativeHarness.state("minecraft:cave_air");
                    }
                    return NativeHarness.state("minecraft:stone");
                }
                return NativeHarness.state("minecraft:air");
            } catch (Exception e) {
                throw new AssertionError(e);
            }
        }
    }

    // ---------- one stronghold chunk case ----------

    static String strongholdRow(String caseId, long seed, int cx, int cz, int ground,
                                long postSeed, int[] clipCoords) throws Exception {
        Start start = generate(seed, cx, cz);
        String pieceSig = signature(start.pieces);

        Scene scene = new Scene(ground, Integer.MIN_VALUE);
        NativeHarness.Level base = NativeHarness.levelFromStates(scene);
        base.worldSeed = seed;

        Map<List<Integer>, Object> blockEntities = new HashMap<>();
        Map<List<Integer>, String> lootTables = new HashMap<>();
        Map<List<Integer>, Long> lootSeeds = new HashMap<>();
        Map<List<Integer>, String> spawnerMobs = new HashMap<>();

        Object world = Proxy.newProxyInstance(
                StrongholdOracle.class.getClassLoader(),
                new Class<?>[]{c("dds")},
                (self, m, a) -> {
                    Class<?>[] p = m.getParameterTypes();
                    if (m.getName().equals("c_") && p.length == 1 && p[0] == c("jd")) {
                        int[] xyz = NativeHarness.xyz(a[0]);
                        return blockEntities.get(List.of(xyz[0], xyz[1], xyz[2]));
                    }
                    if (m.getName().equals("y") && p.length == 1 && p[0] == c("jd")) {
                        return chunkAt(a[0]); // post-processing mark only
                    }
                    if (m.getName().equals("H_") && p.length == 0) {
                        return registryAccess();
                    }
                    if (m.getName().equals("C") && p.length == 0) {
                        return seed;
                    }
                    if (m.getName().equals("a") && (p.length == 3 || p.length == 4)
                            && p[0] == c("jd") && p[1] == c("dtc") && p[2] == int.class) {
                        int[] xyz = NativeHarness.xyz(a[0]);
                        List<Integer> key = List.of(xyz[0], xyz[1], xyz[2]);
                        String name = NativeHarness.nameOf(a[1]);
                        Object res = m.invoke(base.proxy, a);
                        if (name.equals("minecraft:chest")) {
                            blockEntities.put(key,
                                    c("dqo").getConstructor(c("jd"), c("dtc"))
                                            .newInstance(NativeHarness.pos(xyz[0], xyz[1], xyz[2]), a[1]));
                        } else if (name.equals("minecraft:spawner")) {
                            blockEntities.put(key,
                                    c("drw").getConstructor(c("jd"), c("dtc"))
                                            .newInstance(NativeHarness.pos(xyz[0], xyz[1], xyz[2]), a[1]));
                        } else {
                            blockEntities.remove(key);
                        }
                        return res;
                    }
                    if (m.isDefault()) {
                        return InvocationHandler.invokeDefault(self, m, a);
                    }
                    try {
                        return m.invoke(base.proxy, a);
                    } catch (InvocationTargetException e) {
                        throw e.getCause();
                    }
                });

        Object postRng = xorRng(postSeed);
        Object box = bb(clipCoords[0], clipCoords[1], clipCoords[2],
                clipCoords[3], clipCoords[4], clipCoords[5]);
        Object chunkPos = c("dcd").getConstructor(int.class, int.class).newInstance(cx, cz);
        Object locator = NativeHarness.pos(cx << 4, 0, cz << 4);

        Method postProcess = c("ejv").getMethod("a", c("dds"), c("ddq"), c("duz"),
                c("ayw"), c("ejj"), c("dcd"), c("jd"));
        for (Object piece : start.pieces) {
            int[] b = bbCoords(c("ejv").getMethod("f").invoke(piece));
            if (!intersects(b, clipCoords)) continue;
            postProcess.invoke(piece, world, null, null, postRng, box, chunkPos, locator);
        }

        // Loot/spawner payloads: the block entity objects keep what the native
        // setter wrote, which is the only place the loot seed is observable.
        for (var e : blockEntities.entrySet()) {
            List<Integer> key = e.getKey();
            Object be = e.getValue();
            if (c("dqo").isInstance(be)) { // ChestBlockEntity
                Object table = c("drn").getMethod("aB_").invoke(be);
                if (table != null) {
                    Object rl = c("akq").getMethod("a").invoke(table);
                    lootTables.put(key, rl.toString());
                    lootSeeds.put(key, (long) c("drn").getMethod("aC_").invoke(be));
                }
            } else if (c("drw").isInstance(be)) { // SpawnerBlockEntity
                // BaseSpawner.nextSpawnData is what `setEntityId` wrote; reading the
                // record's own save() avoids guessing how the field is named.
                Object spawner = c("drw").getMethod("c").invoke(be);
                Field next = c("dby").getDeclaredField("f");
                next.setAccessible(true);
                Object data = next.get(spawner);
                if (data != null) {
                    Object tag = c("ddo").getMethod("a").invoke(data);
                    Object id = c("ub").getMethod("l", String.class).invoke(tag, "id");
                    if (id != null && !id.toString().isEmpty()) spawnerMobs.put(key, id.toString());
                }
            }
        }

        List<int[]> keys = new ArrayList<>();
        for (String k : base.writes.keySet()) {
            String[] ps = k.split(",");
            keys.add(new int[]{Integer.parseInt(ps[0]), Integer.parseInt(ps[1]), Integer.parseInt(ps[2])});
        }
        keys.removeIf(k -> k[0] < clipCoords[0] || k[0] > clipCoords[3]
                || k[1] < clipCoords[1] || k[1] > clipCoords[4]
                || k[2] < clipCoords[2] || k[2] > clipCoords[5]);
        keys.sort((x, y) -> {
            if (x[0] != y[0]) return Integer.compare(x[0], y[0]);
            if (x[1] != y[1]) return Integer.compare(x[1], y[1]);
            return Integer.compare(x[2], y[2]);
        });
        MessageDigest md = MessageDigest.getInstance("SHA-256");
        for (int[] k : keys) {
            Object st = base.writes.get(k[0] + "," + k[1] + "," + k[2]);
            md.update((k[0] + "," + k[1] + "," + k[2] + " " + NativeHarness.normalizedStateName(st) + "\n")
                    .getBytes(StandardCharsets.UTF_8));
        }
        StringBuilder sha = new StringBuilder();
        for (byte b : md.digest()) sha.append(String.format("%02x", b));

        List<String> payloads = new ArrayList<>();
        for (var e : lootTables.entrySet()) {
            List<Integer> k = e.getKey();
            payloads.add(k.get(0) + "," + k.get(1) + "," + k.get(2) + "|chest|" + e.getValue()
                    + "|" + lootSeeds.get(k));
        }
        for (var e : spawnerMobs.entrySet()) {
            List<Integer> k = e.getKey();
            payloads.add(k.get(0) + "," + k.get(1) + "," + k.get(2) + "|spawner|" + e.getValue() + "|0");
        }
        Collections.sort(payloads);

        return String.join("\t",
                "stronghold",
                caseId,
                Long.toString(seed),
                Integer.toString(cx),
                Integer.toString(cz),
                Integer.toString(start.attempts),
                Integer.toString(ground),
                Long.toString(postSeed),
                fmtBox(clipCoords),
                pieceSig,
                Integer.toString(keys.size()),
                sha.toString(),
                Long.toString(start.rngAfter),
                payloads.isEmpty() ? "-" : String.join(";", payloads),
                "-",
                Long.toString(nextLong(postRng)));
    }

    static boolean intersects(int[] a, int[] b) {
        return a[0] <= b[3] && a[3] >= b[0] && a[1] <= b[4] && a[4] >= b[1]
                && a[2] <= b[5] && a[5] >= b[2];
    }

    // ---------- ring placement over a real Overworld biome source ----------

    static Object field(String owner, String name) throws Exception {
        Field f = c(owner).getDeclaredField(name);
        f.setAccessible(true);
        return f.get(null);
    }

    static final class Overworld {
        Object generatorState;
        Object placement;
        Method ringPositions;

        static Overworld build(long seed, String[] preferredBiomes, int distance, int spread,
                               int count) throws Exception {
            Object access = c("ka").getMethod("a", c("jz")).invoke(null, field("lt", "aA"));
            Object pack = c("ats").getMethod("b").invoke(null);
            Object manager = c("atx").getConstructor(c("ass"), List.class)
                    .newInstance(field("ass", "b"), List.of(pack));
            access = c("akm").getMethod("a", c("aue"), c("ka"), List.class)
                    .invoke(null, manager, access, field("akm", "a"));

            Object settingsKey = field("dzd", "c");
            Object settingsRegistry = access.getClass().getMethod("d", c("akq"))
                    .invoke(access, settingsKey.getClass().getMethod("c").invoke(settingsKey));
            Object settings = settingsRegistry.getClass().getMethod("f", c("akq"))
                    .invoke(settingsRegistry, settingsKey);
            Object noises = access.getClass().getMethod("d", c("akq")).invoke(access, field("lu", "aP"));
            Object noiseGetter = c("jz").getMethod("q").invoke(noises);
            // The climate sampler is seeded by the world seed, so the biome source the
            // ring search queries must be built for the same seed the world uses.
            Object randomState = c("dzm").getMethod("a", c("dzd"), c("jn"), long.class)
                    .invoke(null, settings, noiseGetter, seed);

            Object presetRegistry = access.getClass().getMethod("d", c("akq"))
                    .invoke(access, field("lu", "aZ"));
            Object presetId = c("akr").getMethod("a", String.class).invoke(null, "minecraft:overworld");
            Object presetHolder = ((Optional<?>) c("jz").getMethod("c", c("akr"))
                    .invoke(presetRegistry, presetId)).orElseThrow();
            Object source = c("dej").getMethod("a", c("jm")).invoke(null, presetHolder);

            Object biomes = access.getClass().getMethod("d", c("akq")).invoke(access, field("lu", "aF"));
            List<Object> holders = new ArrayList<>();
            for (String name : preferredBiomes) {
                Object id = c("akr").getMethod("a", String.class).invoke(null, name);
                Object holder = ((Optional<?>) c("jz").getMethod("c", c("akr")).invoke(biomes, id))
                        .orElseThrow(() -> new AssertionError("unknown biome " + name));
                holders.add(holder);
            }
            Object preferred = c("jq").getMethod("a", List.class).invoke(null, holders);

            Constructor<?> ctor = c("dva").getDeclaredConstructor(
                    c("dzm"), c("dea"), long.class, long.class, List.class);
            ctor.setAccessible(true);
            Overworld out = new Overworld();
            out.generatorState = ctor.newInstance(randomState, source, seed, seed, List.of());
            out.placement = c("ekl").getConstructor(int.class, int.class, int.class, c("jq"))
                    .newInstance(distance, spread, count, preferred);
            out.ringPositions = c("dva").getDeclaredMethod("a", c("jm"), c("ekl"));
            out.ringPositions.setAccessible(true);
            return out;
        }

        List<int[]> rings() throws Exception {
            @SuppressWarnings("unchecked")
            CompletableFuture<List<Object>> future =
                    (CompletableFuture<List<Object>>) ringPositions.invoke(generatorState, null, placement);
            List<Object> points = future.get(300, TimeUnit.SECONDS);
            Field fx = c("dcd").getField("e");
            Field fz = c("dcd").getField("f");
            List<int[]> out = new ArrayList<>();
            for (Object point : points) out.add(new int[]{fx.getInt(point), fz.getInt(point)});
            return out;
        }
    }

    /** The `#minecraft:stronghold_biased_to` tag, in file order. */
    static String[] strongholdBiasedTo(Path tagFile) throws Exception {
        String json = Files.readString(tagFile, StandardCharsets.UTF_8);
        List<String> out = new ArrayList<>();
        int i = 0;
        while ((i = json.indexOf("minecraft:", i)) >= 0) {
            int start = i;
            int end = i;
            while (end < json.length() && "abcdefghijklmnopqrstuvwxyz0123456789_:".indexOf(json.charAt(end)) >= 0) end++;
            out.add(json.substring(start, end));
            i = end;
        }
        return out.toArray(new String[0]);
    }

    static int[] chunkClip(int chunkX, int chunkZ) {
        return new int[]{chunkX << 4, MIN_Y, chunkZ << 4, (chunkX << 4) + 15,
                MIN_Y + HEIGHT - 1, (chunkZ << 4) + 15};
    }

    /** Local piece coordinates to world, through the piece's own coord-base-mode. */
    static int[] worldPos(Object piece, int lx, int ly, int lz) throws Exception {
        Method m = c("ejv").getDeclaredMethod("b", int.class, int.class, int.class);
        m.setAccessible(true);
        Object pos = m.invoke(piece, lx, ly, lz);
        return NativeHarness.xyz(pos);
    }

    static Object findPortalRoom(List<Object> pieces) throws Exception {
        for (Object piece : pieces) {
            if (pieceName(piece).equals("PortalRoom")) return piece;
        }
        return null;
    }

    /** The non-start chunk shared by the most pieces, so a clipped run is non-empty. */
    static int[] busiestForeignChunk(List<Object> pieces, int cx, int cz) throws Exception {
        List<int[]> boxes = new ArrayList<>();
        for (Object piece : pieces) {
            boxes.add(bbCoords(c("ejv").getMethod("f").invoke(piece)));
        }
        int bestX = cx, bestZ = cz, best = -1;
        for (int[] b : boxes) {
            for (int x = Math.floorDiv(b[0], 16); x <= Math.floorDiv(b[3], 16); x++) {
                for (int z = Math.floorDiv(b[2], 16); z <= Math.floorDiv(b[5], 16); z++) {
                    if (x == cx && z == cz) continue;
                    int[] clip = chunkClip(x, z);
                    int n = 0;
                    for (int[] other : boxes) if (intersects(other, clip)) n++;
                    if (n > best) {
                        best = n;
                        bestX = x;
                        bestZ = z;
                    }
                }
            }
        }
        return new int[]{bestX, bestZ};
    }

    public static void main(String[] args) throws Exception {
        Path out = Path.of(args[0]);
        Path tagFile = Path.of(args[1]);
        NativeHarness.pos(0, 0, 0); // SharedConstants + Bootstrap, via the harness
        List<String> rows = new ArrayList<>();

        // --- stronghold structure cases ---
        long[][] starts = {
            {1L, 16, -3}, {2L, -7, 21}, {3L, 40, 40}, {4L, -33, -12}, {5L, 0, 0},
            {6L, 100, -100}, {7L, -58, 9}, {8L, 13, 74},
        };
        int[] grounds = {-10, 20, 45, -40, 0, 30, 5, -20};
        long[] postSeeds = {0x51A7C0DEL, 0x0BADF00DL, 0x1234_5678_9ABCL, 77L,
                            0xDEADBEEFL, 0x5EEDL, 91L, 0x2B2B2BL};
        for (int s = 0; s < starts.length; s++) {
            long seed = starts[s][0];
            int cx = (int) starts[s][1];
            int cz = (int) starts[s][2];
            int ground = grounds[s];
            long postSeed = postSeeds[s];
            String tag = "s" + seed + "_" + cx + "_" + cz;
            rows.add(strongholdRow("start_" + tag, seed, cx, cz, ground, postSeed,
                    chunkClip(cx, cz)));
            Start start = generate(seed, cx, cz);
            int[] far = busiestForeignChunk(start.pieces, cx, cz);
            if (far[0] != cx || far[1] != cz) {
                rows.add(strongholdRow("chunk_" + tag, seed, cx, cz, ground, postSeed ^ 0x9E3779B97F4A7C15L,
                        chunkClip(far[0], far[1])));
            }
            Object portal = findPortalRoom(start.pieces);
            if (portal != null) {
                int[] at = worldPos(portal, 5, 3, 6); // the silverfish spawner cell
                rows.add(strongholdRow("portal_" + tag, seed, cx, cz, ground,
                        postSeed ^ 0x2545F4914F6CDD1DL, chunkClip(at[0] >> 4, at[2] >> 4)));
            }
        }

        // Retry coverage: chunks whose first attempt produced no portal room, so the
        // `while (builder.isEmpty() || portalRoom == null)` loop actually re-rolled.
        int retries = 0;
        int maxAttempts = 1;
        outer:
        for (long seed : new long[]{11L, 4242L, 777L, 9090L, 31337L}) {
            for (int cx = -120; cx <= 120; cx += 7) {
                for (int cz = -120; cz <= 120; cz += 11) {
                    int attempts = generate(seed, cx, cz).attempts;
                    maxAttempts = Math.max(maxAttempts, attempts);
                    if (attempts >= 2 && retries < 3) {
                        rows.add(strongholdRow("retry_" + attempts + "_" + seed + "_" + cx + "_" + cz,
                                seed, cx, cz, 55, 0xFEEDL + cx, chunkClip(cx, cz)));
                        retries++;
                    }
                    if (retries >= 3) break outer;
                }
            }
        }
        if (retries < 3) {
            throw new AssertionError("retry coverage not found: " + retries + ", max " + maxAttempts);
        }
        System.out.println("retry depth: rows=" + retries + " max attempts seen=" + maxAttempts);

        // --- ring placement over the real Overworld biome source ---
        String[] preferred = strongholdBiasedTo(tagFile);
        // Four seeds, not seven: every row costs a full 57x57 quart square per candidate
        // (~3.3k biome-source samples x 128 rings), and the port's biome source is the
        // expensive side of this comparison.
        long[] ringSeeds = {0L, -1L, 42L, Long.MAX_VALUE};
        for (long seed : ringSeeds) {
            Overworld over = Overworld.build(seed, preferred, 32, 3, 128);
            List<int[]> rings = over.rings();
            List<String> coords = new ArrayList<>();
            for (int[] p : rings) coords.add(p[0] + "," + p[1]);
            rows.add(String.join("\t",
                    "rings",
                    "overworld_" + seed,
                    Long.toString(seed),
                    "-", "-",
                    "32", "3", "128",
                    "-", "-",
                    Integer.toString(coords.size()),
                    "-",
                    "-",
                    "-",
                    coords.isEmpty() ? "EMPTY" : String.join(";", coords),
                    "-"));
        }
        Files.write(out, rows);
        System.out.println("stronghold/ring cases: " + rows.size() + " (retry rows " + retries
                + ", max attempts " + maxAttempts + ")");
    }
}
