package dev.megu.tools;

import java.lang.reflect.*;
import java.nio.charset.StandardCharsets;
import java.nio.file.*;
import java.security.MessageDigest;
import java.util.*;

/**
 * Native 1.21.1 oracle for the 4 single-piece procedural box structures:
 *   - minecraft:buried_treasure  (BuriedTreasurePieces$BuriedTreasurePiece, elk$a)
 *   - minecraft:swamp_hut        (SwampHutPiece, eml)
 *   - minecraft:desert_pyramid   (DesertPyramidPiece, elm + DesertPyramidStructure.afterPlace, eln)
 *   - minecraft:jungle_pyramid   (JungleTemplePiece, elt)
 */
public final class SinglePieceOracle {
    static Class<?> c(String name) { return NativeHarness.C(name); }
    static Object f(String cls, String field) throws Exception { return c(cls).getField(field).get(null); }

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

    static Object dir(String name) throws Exception {
        return switch (name) {
            case "north" -> f("ji", "c");
            case "south" -> f("ji", "d");
            case "west"  -> f("ji", "e");
            case "east"  -> f("ji", "f");
            default -> throw new IllegalArgumentException("bad dir: " + name);
        };
    }

    static String dirName(Object d) throws Exception {
        if (d == null) return "none";
        return (String) c("ji").getMethod("n").invoke(d);
    }

    static Object legacyRng(long seed) throws Exception {
        return c("dyz").getConstructor(long.class).newInstance(seed);
    }

    static Object xorRng(long seed) throws Exception {
        return c("dzx").getConstructor(c("ayw")).newInstance(c("dzz").getConstructor(long.class).newInstance(seed));
    }

    static long nextLong(Object rng) throws Exception {
        return (long) c("ayw").getMethod("g").invoke(rng);
    }

    static void setOrientation(Object piece, String orient) throws Exception {
        if (!orient.equals("none")) {
            c("ejv").getMethod("a", c("ji")).invoke(piece, dir(orient));
            // Rebuild bounding box from piece's minX, minY, minZ and width/height/depth
        }
    }

    static Object constructPiece(String kind, String orient, int cx, int cz, long orientSeed) throws Exception {
        int minX = cx << 4;
        int minZ = cz << 4;
        if (kind.equals("buried_treasure")) {
            Object pos = NativeHarness.pos(minX + 9, 90, minZ + 9);
            return c("elk$a").getConstructor(c("jd")).newInstance(pos);
        }
        // Choose a legacy/worldgen RNG or deterministic orientation seed
        Object ctorRng = xorRng(orientSeed);
        Object piece = switch (kind) {
            case "swamp_hut" -> c("eml").getConstructor(c("ayw"), int.class, int.class).newInstance(ctorRng, minX, minZ);
            case "desert_pyramid" -> c("elm").getConstructor(c("ayw"), int.class, int.class).newInstance(ctorRng, minX, minZ);
            case "jungle_pyramid" -> c("elt").getConstructor(c("ayw"), int.class, int.class).newInstance(ctorRng, minX, minZ);
            default -> throw new IllegalArgumentException("unknown kind: " + kind);
        };
        if (!orient.equals("auto")) {
            // Re-orient and re-make bounding box matching ScatteredFeaturePiece constructor
            int width = switch (kind) {
                case "swamp_hut" -> 7;
                case "desert_pyramid" -> 21;
                case "jungle_pyramid" -> 12;
                default -> 0;
            };
            int height = switch (kind) {
                case "swamp_hut" -> 7;
                case "desert_pyramid" -> 15;
                case "jungle_pyramid" -> 10;
                default -> 0;
            };
            int depth = switch (kind) {
                case "swamp_hut" -> 9;
                case "desert_pyramid" -> 21;
                case "jungle_pyramid" -> 15;
                default -> 0;
            };
            Object d = dir(orient);
            Method mkBb = c("ejv").getDeclaredMethod("a", int.class, int.class, int.class, c("ji"), int.class, int.class, int.class);
            mkBb.setAccessible(true);
            Object newBb = mkBb.invoke(null, minX, 64, minZ, d, width, height, depth);
            Field fField = c("ejv").getDeclaredField("f");
            fField.setAccessible(true);
            fField.set(piece, newBb);
            c("ejv").getMethod("a", c("ji")).invoke(piece, d);
        }
        return piece;
    }

    static final class SceneSpec {
        final String spec;
        final int ocFloor;
        final int groundHeight;
        final String floorBlock;
        final String fillBlock;
        final String neighborBlock;
        final boolean sloped;

        SceneSpec(String spec) {
            this.spec = spec;
            int oc = 58;
            int gh = 64;
            String fb = "minecraft:sandstone";
            String fl = "minecraft:sand";
            String nb = "minecraft:air";
            boolean sl = false;
            for (String part : spec.split(",")) {
                String[] kv = part.split("=", 2);
                switch (kv[0]) {
                    case "oc" -> oc = Integer.parseInt(kv[1]);
                    case "hm" -> gh = Integer.parseInt(kv[1]);
                    case "sub" -> fb = kv[1];
                    case "fill" -> fl = kv[1];
                    case "adj" -> nb = kv[1];
                    case "slope" -> sl = Boolean.parseBoolean(kv[1]);
                }
            }
            this.ocFloor = oc;
            this.groundHeight = gh;
            this.floorBlock = fb;
            this.fillBlock = fl;
            this.neighborBlock = nb;
            this.sloped = sl;
        }

        int heightAt(int x, int z) {
            if (!sloped) return groundHeight;
            int dx = ((x % 16) + 16) % 16;
            int dz = ((z % 16) + 16) % 16;
            return groundHeight + ((dx + dz) % 5) - 2;
        }

        Object stateAt(int x, int y, int z, String kind) {
            if (kind.equals("buried_treasure")) {
                if (y < ocFloor - 3) return NativeHarness.state(floorBlock);
                if (y < ocFloor) return NativeHarness.state(fillBlock);
                if (y == ocFloor) {
                    // Check if neighbor override requested around (x%16, z%16) = (8,9), (10,9), etc.
                    if (!neighborBlock.equals("minecraft:air") && (Math.abs((x & 15) - 9) + Math.abs((z & 15) - 9) == 1)) {
                        return NativeHarness.state(neighborBlock);
                    }
                    return NativeHarness.state(fillBlock);
                }
                if (y <= 62 && fillBlock.equals("minecraft:water")) return NativeHarness.state("minecraft:water");
                if (y <= 62 && spec.contains("water=true")) return NativeHarness.state("minecraft:water");
                return NativeHarness.state("minecraft:air");
            }
            int h = heightAt(x, z);
            if (y < h - 3) return NativeHarness.state("minecraft:stone");
            if (y < h) {
                if (spec.contains("water=true") && y >= h - 2) return NativeHarness.state("minecraft:water");
                return NativeHarness.state(floorBlock);
            }
            if (spec.contains("water=true") && y <= 62) return NativeHarness.state("minecraft:water");
            return NativeHarness.state("minecraft:air");
        }
    }

    static final class LootEntry implements Comparable<LootEntry> {
        final int x, y, z;
        final String kind;
        final String table;
        final long seed;

        LootEntry(int x, int y, int z, String kind, String table, long seed) {
            this.x = x; this.y = y; this.z = z;
            this.kind = kind; this.table = table; this.seed = seed;
        }

        @Override
        public int compareTo(LootEntry o) {
            if (x != o.x) return Integer.compare(x, o.x);
            if (y != o.y) return Integer.compare(y, o.y);
            if (z != o.z) return Integer.compare(z, o.z);
            return kind.compareTo(o.kind);
        }

        String format() {
            return x + "," + y + "," + z + "|" + kind + "|" + table + "|" + seed;
        }
    }

    static Object makeDummyServerLevel(Object registryAccess, Object levelRng) throws Exception {
        // Minimal ServerLevel (aqu) that satisfies EntityType.create(ServerLevel) + Witch/Cat finalizeSpawn without crashing
        Field unsafeField = Class.forName("sun.misc.Unsafe").getDeclaredField("theUnsafe");
        unsafeField.setAccessible(true);
        Object unsafe = unsafeField.get(null);
        Method alloc = unsafe.getClass().getMethod("allocateInstance", Class.class);

        Object vanillaFlags = f("cpn", "f"); // FeatureFlags.VANILLA_SET
        Object worldData = Proxy.newProxyInstance(
                SinglePieceOracle.class.getClassLoader(),
                new Class<?>[]{c("erl")},
                (proxy, method, args) -> {
                    if (method.getName().equals("K") && method.getParameterCount() == 0) return vanillaFlags;
                    if (method.getName().equals("r") && method.getParameterCount() == 0) return f("bqo", "c"); // Difficulty.NORMAL
                    throw new UnsupportedOperationException("worldData: " + method);
                });
        Class<?> serverC = Class.forName("net.minecraft.server.MinecraftServer");
        Object server = alloc.invoke(unsafe, c("apn"));
        Field worldDataField = serverC.getDeclaredField("j");
        worldDataField.setAccessible(true);
        worldDataField.set(server, worldData);

        Class<?> slC = c("aqu");
        Object sl = alloc.invoke(unsafe, slC);
        Field serverField = slC.getDeclaredField("J");
        serverField.setAccessible(true);
        serverField.set(sl, server);
        // Also populate Level.x (random), Level.D (biomeRegistry / registryAccess), Level.A (writableLevelData)
        for (Field lf : c("dcw").getDeclaredFields()) {
            lf.setAccessible(true);
            if (lf.getType() == c("ayw")) {
                lf.set(sl, levelRng);
            } else if (lf.getType() == c("ka")) {
                lf.set(sl, registryAccess);
            } else if (lf.getType() == c("erc")) {
                // WritableLevelData
                lf.set(sl, Proxy.newProxyInstance(SinglePieceOracle.class.getClassLoader(),
                        new Class<?>[]{c("erc")}, (p, m, a) -> {
                            if (m.getName().equals("q") && m.getParameterCount() == 0) return f("bqo", "c");
                            if (m.getReturnType() == boolean.class) return false;
                            if (m.getReturnType() == long.class) return 100L;
                            if (m.getReturnType() == int.class) return 0;
                            throw new UnsupportedOperationException("wld: " + m);
                        }));
            }
        }
        return sl;
    }

    static String runCase(
            String kind, String caseId, long seed, int cx, int cz,
            String orient, int[] clipCoords, String sceneStr, boolean runAfterPlace,
            Object registryAccess, Object generator
    ) throws Exception {
        SceneSpec scene = new SceneSpec(sceneStr);
        NativeHarness.Level baseLevel = NativeHarness.levelFromStates((x, y, z) -> scene.stateAt(x, y, z, kind));
        baseLevel.worldSeed = seed;

        Object piece = constructPiece(kind, orient, cx, cz, seed);
        String actualOrient = dirName(c("ejv").getMethod("i").invoke(piece));
        int[] initBb = bbCoords((Object) getField(piece, c("ejv"), "f"));

        Object postRng = xorRng(seed ^ 0x9E3779B97F4A7C15L);
        Object levelRng = legacyRng(seed ^ 0x517CC1B727220A95L);

        Map<List<Integer>, Object> blockEntities = new HashMap<>();
        Map<Long, Object> chunks = new HashMap<>();
        Object biomesRegistry = c("ka").getMethod("d", c("akq")).invoke(registryAccess, f("lu", "aF"));
        Object dimTypes = c("ka").getMethod("d", c("akq")).invoke(registryAccess, f("lu", "aK"));
        Object overworldId = c("akr").getMethod("a", String.class).invoke(null, "minecraft:overworld");
        Object overworldDimType = c("jz").getMethod("a", c("akr")).invoke(dimTypes, overworldId);
        Object serverLevel = kind.equals("swamp_hut") ? makeDummyServerLevel(registryAccess, levelRng) : null;

        Object world = Proxy.newProxyInstance(
                SinglePieceOracle.class.getClassLoader(),
                new Class<?>[]{c("dds")},
                (self, m, a) -> {
                    Class<?>[] p = m.getParameterTypes();
                    if (m.getName().equals("D_") && p.length == 0) {
                        return overworldDimType;
                    }
                    if (m.getName().equals("E_") && p.length == 0) {
                        return levelRng;
                    }
                    if (m.getName().equals("C") && p.length == 0) {
                        return seed;
                    }
                    if (m.getName().equals("H_") && p.length == 0) {
                        return registryAccess;
                    }
                    if (m.getName().equals("E") && p.length == 0) {
                        return serverLevel;
                    }
                    if (m.getName().equals("I_") && p.length == 0) {
                        return -64;
                    }
                    if (m.getName().equals("J_") && p.length == 0) {
                        return 384;
                    }
                    if (m.getName().equals("a") && p.length == 4
                            && p[0] == int.class && p[1] == int.class && p[2] == c("dvz") && p[3] == boolean.class
                            && c("duy").isAssignableFrom(m.getReturnType())) {
                        long key = ((long) (int) a[0] & 0xFFFFFFFFL) | ((long) (int) a[1] << 32);
                        Object ch = chunks.get(key);
                        if (ch == null) {
                            ch = c("dvs").getConstructor(c("dcd"), c("dvv"), c("dcy"), c("jz"), c("eab"))
                                    .newInstance(
                                            c("dcd").getConstructor(int.class, int.class).newInstance((int) a[0], (int) a[1]),
                                            f("dvv", "a"),
                                            c("dcy").getMethod("e", int.class, int.class).invoke(null, -64, 384),
                                            biomesRegistry,
                                            null);
                            chunks.put(key, ch);
                        }
                        return ch;
                    }
                    if (m.getName().equals("a_") && p.length == 1 && p[0] == c("bsr")) {
                        // addFreshEntityWithPassengers
                        return null;
                    }
                    if (m.getName().equals("b") && p.length == 1 && p[0] == c("bsr")) {
                        return true;
                    }
                    if (m.getName().equals("d_") && p.length == 1 && p[0] == c("jd")) {
                        // DifficultyInstance(Difficulty, dayTime, chunkInhabitedTime, moonPhaseFactor)
                        return c("bqp").getConstructor(c("bqo"), long.class, long.class, float.class)
                                .newInstance(f("bqo", "c"), 100L, 0L, 0.0f);
                    }
                    if (m.getName().equals("t") && p.length == 1 && p[0] == c("jd")) {
                        // getBiome
                        Object id = c("akr").getMethod("a", String.class).invoke(null, "minecraft:plains");
                        return c("jz").getMethod("c", c("akr")).invoke(biomesRegistry, id);
                    }
                    if (m.getName().equals("c_") && p.length == 1 && p[0] == c("jd")) {
                        int[] xyz = NativeHarness.xyz(a[0]);
                        return blockEntities.get(List.of(xyz[0], xyz[1], xyz[2]));
                    }
                    if (m.getName().equals("a") && p.length == 2 && p[0] == c("jd") && p[1] == c("dqj")) {
                        int[] xyz = NativeHarness.xyz(a[0]);
                        Object be = blockEntities.get(List.of(xyz[0], xyz[1], xyz[2]));
                        if (be != null && (boolean) c("dqj").getMethod("a", c("dtc")).invoke(a[1], baseLevel.writes.get(xyz[0] + "," + xyz[1] + "," + xyz[2]))) {
                            return Optional.of(be);
                        }
                        return Optional.empty();
                    }
                    if (m.getName().equals("a") && p.length == 2 && p[0] == c("dyy$a") && p[1] == c("jd")) {
                        int[] xyz = NativeHarness.xyz(a[1]);
                        int h = kind.equals("buried_treasure") ? scene.ocFloor : scene.heightAt(xyz[0], xyz[2]);
                        return NativeHarness.pos(xyz[0], h, xyz[2]);
                    }
                    if (m.getName().equals("a") && p.length == 3 && p[0] == c("dyy$a") && p[1] == int.class && p[2] == int.class) {
                        return kind.equals("buried_treasure") ? scene.ocFloor : scene.heightAt((int) a[1], (int) a[2]);
                    }
                    if (m.getName().equals("a") && (p.length == 3 || p.length == 4)
                            && p[0] == c("jd") && p[1] == c("dtc") && p[2] == int.class) {
                        int[] xyz = NativeHarness.xyz(a[0]);
                        Object posObj = NativeHarness.pos(xyz[0], xyz[1], xyz[2]);
                        Object res = m.invoke(baseLevel.proxy, a);
                        String bname = NativeHarness.nameOf(a[1]);
                        List<Integer> key = List.of(xyz[0], xyz[1], xyz[2]);
                        if (bname.equals("minecraft:chest")) {
                            blockEntities.put(key, c("dqo").getConstructor(c("jd"), c("dtc")).newInstance(posObj, a[1]));
                        } else if (bname.equals("minecraft:dispenser")) {
                            blockEntities.put(key, c("dra").getConstructor(c("jd"), c("dtc")).newInstance(posObj, a[1]));
                        } else if (bname.equals("minecraft:suspicious_sand")) {
                            blockEntities.put(key, c("dql").getConstructor(c("jd"), c("dtc")).newInstance(posObj, a[1]));
                        } else {
                            blockEntities.remove(key);
                        }
                        return res;
                    }
                    if (m.isDefault()) {
                        return InvocationHandler.invokeDefault(self, m, a);
                    }
                    try {
                        return m.invoke(baseLevel.proxy, a);
                    } catch (InvocationTargetException e) {
                        throw e.getCause();
                    }
                });

        Object sm = c("ddq").getConstructor(c("dcx"), c("dzw"), c("ejs")).newInstance(world, null, null);
        if (serverLevel != null) {
            Field smField = c("aqu").getDeclaredField("ad");
            smField.setAccessible(true);
            smField.set(serverLevel, sm);
        }
        Object clip = bb(clipCoords[0], clipCoords[1], clipCoords[2], clipCoords[3], clipCoords[4], clipCoords[5]);
        Object chunkPos = c("dcd").getConstructor(int.class, int.class).newInstance(cx, cz);
        Object structurePos = NativeHarness.pos(cx << 4, 0, cz << 4);

        c("ejv").getMethod("a", c("dds"), c("ddq"), c("duz"), c("ayw"), c("ejj"), c("dcd"), c("jd"))
                .invoke(piece, world, sm, generator, postRng, clip, chunkPos, structurePos);

        if (runAfterPlace && kind.equals("desert_pyramid")) {
            Object piecesContainer = c("ekg").getConstructor(List.class).newInstance(List.of(piece));
            Object structures = c("ka").getMethod("d", c("akq")).invoke(registryAccess, f("lu", "aR"));
            Object id = c("akr").getMethod("a", String.class).invoke(null, "minecraft:desert_pyramid");
            Object dpStruct = c("jz").getMethod("a", c("akr")).invoke(structures, id);
            c("eln").getMethod("a", c("dds"), c("ddq"), c("duz"), c("ayw"), c("ejj"), c("dcd"), c("ekg"))
                    .invoke(dpStruct, world, sm, generator, postRng, clip, chunkPos, piecesContainer);
        }

        int[] finalBb = bbCoords((Object) getField(piece, c("ejv"), "f"));

        // Sort writes by (x, y, z) and compute SHA-256
        List<int[]> keys = new ArrayList<>();
        for (String k : baseLevel.writes.keySet()) {
            String[] ps = k.split(",");
            keys.add(new int[]{Integer.parseInt(ps[0]), Integer.parseInt(ps[1]), Integer.parseInt(ps[2])});
        }
        keys.sort((a, b) -> {
            if (a[0] != b[0]) return Integer.compare(a[0], b[0]);
            if (a[1] != b[1]) return Integer.compare(a[1], b[1]);
            return Integer.compare(a[2], b[2]);
        });
        MessageDigest md = MessageDigest.getInstance("SHA-256");
        for (int[] k : keys) {
            Object st = baseLevel.writes.get(k[0] + "," + k[1] + "," + k[2]);
            String line = k[0] + "," + k[1] + "," + k[2] + " " + NativeHarness.normalizedStateName(st) + "\n";
            md.update(line.getBytes(StandardCharsets.UTF_8));
        }
        StringBuilder sha = new StringBuilder();
        for (byte b : md.digest()) sha.append(String.format("%02x", b));

        // Extract loot entries from blockEntities
        List<LootEntry> loots = new ArrayList<>();
        for (var e : blockEntities.entrySet()) {
            List<Integer> xyz = e.getKey();
            Object be = e.getValue();
            if (c("drn").isInstance(be)) {
                Object lt = c("drn").getMethod("aB_").invoke(be);
                if (lt != null) {
                    Object rl = c("akq").getMethod("a").invoke(lt);
                    long lseed = (long) c("drn").getMethod("aC_").invoke(be);
                    String lkind = c("dra").isInstance(be) ? "dispenser" : "chest";
                    loots.add(new LootEntry(xyz.get(0), xyz.get(1), xyz.get(2), lkind, rl.toString(), lseed));
                }
            } else if (c("dql").isInstance(be)) {
                Field qf = c("dql").getDeclaredField("q");
                qf.setAccessible(true);
                Object lt = qf.get(be);
                if (lt != null) {
                    Field rf = c("dql").getDeclaredField("r");
                    rf.setAccessible(true);
                    long lseed = rf.getLong(be);
                    Object rl = c("akq").getMethod("a").invoke(lt);
                    loots.add(new LootEntry(xyz.get(0), xyz.get(1), xyz.get(2), "brushable", rl.toString(), lseed));
                }
            }
        }
        Collections.sort(loots);
        StringBuilder lootStr = new StringBuilder();
        if (loots.isEmpty()) {
            lootStr.append("-");
        } else {
            for (int i = 0; i < loots.size(); i++) {
                if (i > 0) lootStr.append(';');
                lootStr.append(loots.get(i).format());
            }
        }

        long rngAfter = nextLong(postRng);
        long levelRngAfter = nextLong(levelRng);

        return String.join("\t",
                kind,
                caseId,
                Long.toString(seed),
                Integer.toString(cx),
                Integer.toString(cz),
                actualOrient,
                fmtBox(initBb),
                fmtBox(clipCoords),
                sceneStr,
                fmtBox(finalBb),
                Integer.toString(keys.size()),
                sha.toString(),
                Long.toString(rngAfter),
                Long.toString(levelRngAfter),
                lootStr.toString()
        );
    }

    static Object getField(Object target, Class<?> cls, String name) throws Exception {
        Field f = cls.getDeclaredField(name);
        f.setAccessible(true);
        return f.get(target);
    }

    public static void main(String[] args) throws Exception {
        NativeHarness.bootstrap();
        Object access = c("ka").getMethod("a", c("jz")).invoke(null, f("lt", "aA"));
        Object manager = c("atx").getConstructor(c("ass"), List.class).newInstance(f("ass", "b"), List.of(c("ats").getMethod("b").invoke(null)));
        access = c("akm").getMethod("a", c("aue"), c("ka"), List.class).invoke(null, manager, access, f("akm", "a"));
        Object generator = NativeHarness.probeGenerator();

        List<String> rows = new ArrayList<>();

        // 1. Buried Treasure
        rows.add(runCase("buried_treasure", "sand_on_stone", 101L, 0, 0, "none",
                new int[]{0, -64, 0, 15, 319, 15},
                "oc=58,sub=minecraft:stone,fill=minecraft:sand", false, access, generator));
        rows.add(runCase("buried_treasure", "water_underwater_sandstone", 202L, 3, -2, "none",
                new int[]{48, -64, -32, 63, 319, -17},
                "oc=48,sub=minecraft:sandstone,fill=minecraft:water,water=true", false, access, generator));
        rows.add(runCase("buried_treasure", "gravel_on_andesite_with_adj_stone", 303L, -4, 5, "none",
                new int[]{-64, -64, 80, -49, 319, 95},
                "oc=61,sub=minecraft:andesite,fill=minecraft:gravel,adj=minecraft:stone", false, access, generator));
        rows.add(runCase("buried_treasure", "air_on_granite", 404L, 1, 1, "none",
                new int[]{16, -64, 16, 31, 319, 31},
                "oc=66,sub=minecraft:granite,fill=minecraft:air", false, access, generator));
        rows.add(runCase("buried_treasure", "diorite_clipped", 505L, 0, 0, "none",
                new int[]{9, -64, 9, 15, 319, 15},
                "oc=55,sub=minecraft:diorite,fill=minecraft:water", false, access, generator));

        // 2. Swamp Hut (all 4 orientations + water + slope + clipped)
        for (String o : new String[]{"north", "south", "west", "east"}) {
            rows.add(runCase("swamp_hut", "flat_" + o, 1000L + o.hashCode(), 2, -1, o,
                    new int[]{32, -64, -16, 47, 319, -1},
                    "hm=64,sub=minecraft:dirt", false, access, generator));
        }
        rows.add(runCase("swamp_hut", "swamp_water_stilts", 1099L, 0, 0, "south",
                new int[]{0, -64, 0, 15, 319, 15},
                "hm=60,sub=minecraft:clay,water=true,slope=true", false, access, generator));
        rows.add(runCase("swamp_hut", "clipped_west", 1100L, 0, 0, "west",
                new int[]{0, -64, 0, 4, 319, 6},
                "hm=66,sub=minecraft:dirt", false, access, generator));

        // 3. Desert Pyramid (all 4 orientations + postProcess-only + afterPlace full + chunk-split)
        for (String o : new String[]{"north", "south", "west", "east"}) {
            rows.add(runCase("desert_pyramid", "full_" + o, 2000L + o.hashCode(), 0, 0, o,
                    new int[]{0, -64, 0, 31, 319, 31},
                    "hm=65,sub=minecraft:sand", true, access, generator));
        }
        rows.add(runCase("desert_pyramid", "postprocess_only_slope", 2099L, -1, 2, "north",
                new int[]{-16, -64, 32, 15, 319, 63},
                "hm=68,sub=minecraft:sand,slope=true", false, access, generator));
        rows.add(runCase("desert_pyramid", "chunk_00_clip", 2100L, 0, 0, "east",
                new int[]{0, -64, 0, 15, 319, 15},
                "hm=66,sub=minecraft:sand", true, access, generator));
        rows.add(runCase("desert_pyramid", "chunk_11_clip", 2100L, 0, 0, "east",
                new int[]{16, -64, 16, 31, 319, 31},
                "hm=66,sub=minecraft:sand", true, access, generator));

        // 4. Jungle Pyramid (all 4 orientations + slope + clipped)
        for (String o : new String[]{"north", "south", "west", "east"}) {
            rows.add(runCase("jungle_pyramid", "full_" + o, 3000L + o.hashCode(), 0, 0, o,
                    new int[]{0, -64, 0, 31, 319, 31},
                    "hm=64,sub=minecraft:dirt", false, access, generator));
        }
        rows.add(runCase("jungle_pyramid", "sloped_clipped", 3099L, 1, -1, "south",
                new int[]{16, -64, -16, 31, 319, -1},
                "hm=67,sub=minecraft:dirt,slope=true", false, access, generator));

        Files.writeString(Path.of(args[0]), String.join("\n", rows) + "\n", StandardCharsets.UTF_8);
    }
}
