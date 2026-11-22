package dev.megu.tools;

import com.google.gson.*;
import com.mojang.serialization.DynamicOps;
import com.mojang.serialization.JsonOps;
import java.lang.reflect.*;
import java.nio.charset.StandardCharsets;
import java.nio.file.*;
import java.security.MessageDigest;
import java.util.*;
import java.util.function.Predicate;

/**
 * Native 1.21.1 oracle for the two multi-piece structures:
 *   - minecraft:fortress (NetherFortressStructure/NetherFortressPieces, {@code ely}/{@code elx})
 *   - minecraft:monument (OceanMonumentStructure/OceanMonumentPieces, {@code emc}/{@code emb})
 *
 * Generation runs the structure's own private {@code generatePieces(builder, context)}, so the
 * piece tree, its draws, and {@code StructurePiecesBuilder.moveInsideHeights} are the game's.
 * Placement is the per-chunk dispatch {@code StructureStart.placeInChunk} performs, written out
 * as "for each piece whose box meets the clip, call {@code postProcess}" - that is the only line
 * of vanilla re-expressed here, and the clips are scripted boxes so one row can place a whole
 * structure or walk it chunk by chunk.
 *
 * Every piece is also serialised through its own {@code createTag}, so the recorded piece list
 * carries the registry id, bounding box, orientation, gen depth and the per-piece flags
 * ({@code Seed}/{@code Chest}/{@code Mob}) the port has to reproduce.
 */
public final class FortressMonumentOracle {
    static Class<?> c(String n) { return NativeHarness.C(n); }
    static Object f(String cls, String name) throws Exception { return c(cls).getField(name).get(null); }

    static Object bb(int x0, int y0, int z0, int x1, int y1, int z1) throws Exception {
        return c("ejj").getConstructor(int.class, int.class, int.class, int.class, int.class, int.class)
                .newInstance(x0, y0, z0, x1, y1, z1);
    }

    static int[] bbCoords(Object box) throws Exception {
        Class<?> ejj = c("ejj");
        return new int[]{
                (int) ejj.getMethod("h").invoke(box), (int) ejj.getMethod("i").invoke(box),
                (int) ejj.getMethod("j").invoke(box), (int) ejj.getMethod("k").invoke(box),
                (int) ejj.getMethod("l").invoke(box), (int) ejj.getMethod("m").invoke(box)};
    }

    static String fmtBox(int[] b) {
        return b[0] + "," + b[1] + "," + b[2] + "," + b[3] + "," + b[4] + "," + b[5];
    }

    static Object legacyRng(long seed) throws Exception {
        return c("dyz").getConstructor(long.class).newInstance(seed);
    }

    static Object xorRng(long seed) throws Exception {
        return c("dzx").getConstructor(c("ayw")).newInstance(c("dzz").getConstructor(long.class).newInstance(seed));
    }

    static Method declared(Class<?> cls, String name, Class<?>... params) throws Exception {
        Method m = cls.getDeclaredMethod(name, params);
        m.setAccessible(true);
        return m;
    }

    static long nextLong(Object rng) throws Exception {
        return (long) c("ayw").getMethod("g").invoke(rng);
    }

    static Object context(Object access, Object random, long seed, int cx, int cz) throws Exception {
        Constructor<?> ctor = c("ejr$a").getDeclaredConstructor(c("ka"), c("duz"), c("dea"),
                c("dzm"), c("enu"), c("dzx"), long.class, c("dcd"), c("dcy"), Predicate.class);
        ctor.setAccessible(true);
        return ctor.newInstance(access, null, null, null, null, random, seed,
                c("dcd").getConstructor(int.class, int.class).newInstance(cx, cz),
                c("dcy").getMethod("e", int.class, int.class).invoke(null, -64, 384),
                (Predicate<Object>) h -> true);
    }

    // ---------- pieces ----------

    static List<?> builderPieces(Object builder) throws Exception {
        Field p = c("ekj").getDeclaredField("a");
        p.setAccessible(true);
        return (List<?>) p.get(builder);
    }

    static JsonObject pieceJson(Object piece) throws Exception {
        Object tag = c("ejv").getMethod("a", c("ekh")).invoke(piece, new Object[]{null});
        DynamicOps<Object> ops = (DynamicOps<Object>) c("up").getField("a").get(null);
        return (JsonObject) new Gson().toJsonTree(ops.convertTo(JsonOps.INSTANCE, tag));
    }

    static String pieceKey(Object piece, boolean withRoom) throws Exception {
        JsonObject json = pieceJson(piece);
        StringBuilder sb = new StringBuilder();
        // The registry id (`minecraft:nescsc`), which is what the piece's own tag carries.
        sb.append(json.get("id").getAsString());
        sb.append(',').append(json.get("O").getAsInt());
        sb.append(',').append(json.get("GD").getAsInt());
        if (withRoom) sb.append(',').append(roomIndex(piece));
        sb.append(',').append(fmtBox(pieceBox(piece)));
        for (String extra : new String[]{"Seed", "Chest", "Mob"}) {
            if (json.has(extra)) sb.append(',').append(extra).append('=').append(json.get(extra));
        }
        return sb.toString();
    }

    static int[] pieceBox(Object piece) throws Exception {
        return bbCoords(c("ejv").getMethod("f").invoke(piece));
    }

    static int roomIndex(Object piece) throws Exception {
        if (!c("emb$r").isInstance(piece)) return -1;
        Field fld = c("emb$r").getDeclaredField("B");
        fld.setAccessible(true);
        Object room = fld.get(piece);
        if (room == null) return -1;
        Field idx = c("emb$v").getDeclaredField("a");
        idx.setAccessible(true);
        return (int) idx.get(room);
    }

    static List<?> monumentChildren(Object building) throws Exception {
        Field fld = c("emb$h").getDeclaredField("I");
        fld.setAccessible(true);
        return (List<?>) fld.get(building);
    }

    /** `MonumentBuilding.sourceRoom`, reachable through its connection graph. */
    static Object sourceRoom(Object building) throws Exception {
        Field f = c("emb$h").getDeclaredField("G");
        f.setAccessible(true);
        return f.get(building);
    }

    static int roomIndexField(Object room) throws Exception {
        Field f = c("emb$v").getDeclaredField("a");
        f.setAccessible(true);
        return (int) f.get(room);
    }

    static Object[] roomConnections(Object room) throws Exception {
        Field f = c("emb$v").getDeclaredField("b");
        f.setAccessible(true);
        return (Object[]) f.get(room);
    }

    static boolean[] roomOpenings(Object room) throws Exception {
        Field f = c("emb$v").getDeclaredField("c");
        f.setAccessible(true);
        return (boolean[]) f.get(room);
    }

    static boolean roomClaimed(Object room) throws Exception {
        Field f = c("emb$v").getDeclaredField("d");
        f.setAccessible(true);
        return (boolean) f.get(room);
    }

    /**
     * The whole room graph, reachable from the source room: `index,openMask,claimed` per room
     * in index order. This is what decides which fitters fire, so it is fixture data too.
     */
    static String roomGraphColumn(Object building) throws Exception {
        Object root = sourceRoom(building);
        Deque<Object> queue = new ArrayDeque<>();
        Map<Integer, Object> seen = new TreeMap<>();
        queue.add(root);
        seen.put(roomIndexField(root), root);
        while (!queue.isEmpty()) {
            Object room = queue.poll();
            for (Object next : roomConnections(room)) {
                if (next == null) continue;
                int idx = roomIndexField(next);
                if (seen.containsKey(idx)) continue;
                seen.put(idx, next);
                queue.add(next);
            }
        }
        StringBuilder sb = new StringBuilder();
        boolean first = true;
        for (var e : seen.entrySet()) {
            Object room = e.getValue();
            boolean[] open = roomOpenings(room);
            int mask = 0;
            for (int d = 0; d < 6; d++) if (open[d]) mask |= 1 << d;
            if (!first) sb.append('|');
            first = false;
            sb.append(e.getKey()).append(',').append(mask).append(',').append(roomClaimed(room) ? 1 : 0);
        }
        return sb.toString();
    }

    // ---------- scene ----------

    static final class Scene {
        final int stoneLo;
        final int stoneHi;
        final int waterLo;
        final int waterHi;
        final int iceY;

        Scene(String spec) {
            int sl = 0, sh = -1, wl = 0, wh = -1, ice = Integer.MIN_VALUE;
            for (String part : spec.split(",")) {
                if (part.isEmpty()) continue;
                String[] kv = part.split("=", 2);
                String[] range = kv.length > 1 ? kv[1].split(":", 2) : new String[]{"0", "-1"};
                switch (kv[0]) {
                    case "stone" -> {
                        sl = Integer.parseInt(range[0]);
                        sh = Integer.parseInt(range[1]);
                    }
                    case "water" -> {
                        wl = Integer.parseInt(range[0]);
                        wh = Integer.parseInt(range[1]);
                    }
                    case "ice" -> ice = Integer.parseInt(kv[1]);
                    default -> throw new IllegalArgumentException("bad scene part: " + part);
                }
            }
            this.stoneLo = sl;
            this.stoneHi = sh;
            this.waterLo = wl;
            this.waterHi = wh;
            this.iceY = ice;
        }

        int lowestNonAir() {
            int lo = Integer.MAX_VALUE;
            if (stoneHi >= stoneLo) lo = Math.min(lo, stoneLo);
            if (waterHi >= waterLo) lo = Math.min(lo, waterLo);
            return lo == Integer.MAX_VALUE ? 0 : lo;
        }

        int highestNonAir() {
            int hi = Integer.MIN_VALUE;
            if (stoneHi >= stoneLo) hi = Math.max(hi, stoneHi);
            if (waterHi >= waterLo) hi = Math.max(hi, waterHi);
            return hi == Integer.MIN_VALUE ? 0 : hi;
        }

        String stateAt(int y) {
            if (y == iceY) return "minecraft:ice";
            if (y >= stoneLo && y <= stoneHi) return "minecraft:stone";
            if (y >= waterLo && y <= waterHi) return "minecraft:water";
            return "minecraft:air";
        }

        Object stateAt(int x, int y, int z) {
            return NativeHarness.state(stateAt(y));
        }
    }

    // ---------- the level ----------

    static Object makeServerLevel(Object registryAccess, Object levelRng) throws Exception {
        Field unsafeField = Class.forName("sun.misc.Unsafe").getDeclaredField("theUnsafe");
        unsafeField.setAccessible(true);
        Object unsafe = unsafeField.get(null);
        Method alloc = unsafe.getClass().getMethod("allocateInstance", Class.class);

        Object vanillaFlags = f("cpn", "f");
        Object worldData = Proxy.newProxyInstance(FortressMonumentOracle.class.getClassLoader(),
                new Class<?>[]{c("erl")}, (proxy, method, args) -> {
                    if (method.getName().equals("K") && method.getParameterCount() == 0) return vanillaFlags;
                    if (method.getName().equals("r") && method.getParameterCount() == 0) return f("bqo", "c");
                    if (method.getDeclaringClass() == Object.class) return "oracle-world-data";
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
        for (Field lf : c("dcw").getDeclaredFields()) {
            lf.setAccessible(true);
            if (lf.getType() == c("ayw")) {
                lf.set(sl, levelRng);
            } else if (lf.getType() == c("ka")) {
                lf.set(sl, registryAccess);
            } else if (lf.getType() == c("erc")) {
                lf.set(sl, Proxy.newProxyInstance(FortressMonumentOracle.class.getClassLoader(),
                        new Class<?>[]{c("erc")}, (p, m, a) -> {
                            if (m.getName().equals("q") && m.getParameterCount() == 0) return f("bqo", "c");
                            if (m.getParameterCount() == 0 && m.getReturnType() == int.class) return 384;
                            if (m.getReturnType() == boolean.class) return false;
                            if (m.getReturnType() == long.class) return 100L;
                            if (m.getDeclaringClass() == Object.class) return "oracle-writable-level-data";
                            throw new UnsupportedOperationException("writableLevelData: " + m);
                        }));
            }
        }
        return sl;
    }

    static final class World {
        Object proxy;
        NativeHarness.Level base;
        final Map<List<Integer>, Object> blockEntities = new HashMap<>();
        final List<String> entities = new ArrayList<>();
    }

    static World makeWorld(Object registryAccess, Object levelRng, Object serverLevel, Scene scene,
                           long seed, int seaLevel) throws Exception {
        World world = new World();
        NativeHarness.Level base = NativeHarness.levelFromStates(scene::stateAt);
        world.base = base;
        Map<Long, Object> chunks = new HashMap<>();
        Object biomeRegistry = c("ka").getMethod("d", c("akq")).invoke(registryAccess, f("lu", "aF"));
        Object dimTypes = c("ka").getMethod("d", c("akq")).invoke(registryAccess, f("lu", "aK"));
        Object overworldId = c("akr").getMethod("a", String.class).invoke(null, "minecraft:overworld");
        Object overworldDimType = c("jz").getMethod("a", c("akr")).invoke(dimTypes, overworldId);

        world.proxy = Proxy.newProxyInstance(FortressMonumentOracle.class.getClassLoader(),
                new Class<?>[]{c("dds")}, (self, m, a) -> {
                    Class<?>[] p = m.getParameterTypes();
                    String name = m.getName();
                    if (name.equals("I_") && p.length == 0) return -64;
                    if (name.equals("J_") && p.length == 0) return 384;
                    if (name.equals("z_") && p.length == 0) return seaLevel;
                    if (name.equals("D_") && p.length == 0) return overworldDimType;
                    if (name.equals("E_") && p.length == 0) return levelRng;
                    if (name.equals("C") && p.length == 0) return seed;
                    if (name.equals("H_") && p.length == 0) return registryAccess;
                    if (name.equals("E") && p.length == 0) return serverLevel;
                    if (name.equals("t") && p.length == 1 && p[0] == c("jd")) {
                        Object id = c("akr").getMethod("a", String.class).invoke(null, "minecraft:plains");
                        return c("jz").getMethod("c", c("akr")).invoke(biomeRegistry, id);
                    }
                    if (name.equals("d_") && p.length == 1 && p[0] == c("jd")) {
                        return c("bqp").getConstructor(c("bqo"), long.class, long.class, float.class)
                                .newInstance(f("bqo", "c"), 100L, 0L, 0.0f);
                    }
                    if (name.equals("a_") && p.length == 1 && p[0] == c("bsr")) {
                        Object entity = a[0];
                        int[] at = NativeHarness.xyz(c("bsr").getMethod("do").invoke(entity));
                        Object type = c("bsr").getMethod("am").invoke(entity);
                        Object key = c("jz").getMethod("b", Object.class).invoke(f("lt", "f"), type);
                        world.entities.add(key + "@" + at[0] + "," + at[1] + "," + at[2]);
                        return null;
                    }
                    if (name.equals("b") && p.length == 1 && p[0] == c("bsr")) {
                        return true;
                    }
                    if (name.equals("c_") && p.length == 1 && p[0] == c("jd")) {
                        int[] xyz = NativeHarness.xyz(a[0]);
                        return world.blockEntities.get(List.of(xyz[0], xyz[1], xyz[2]));
                    }
                    if (name.equals("a") && p.length == 2 && p[0] == c("jd") && p[1] == c("dqj")) {
                        int[] xyz = NativeHarness.xyz(a[0]);
                        Object be = world.blockEntities.get(List.of(xyz[0], xyz[1], xyz[2]));
                        if (be != null && (boolean) c("dqj").getMethod("a", c("dtc"))
                                .invoke(a[1], base.writes.get(xyz[0] + "," + xyz[1] + "," + xyz[2]))) {
                            return Optional.of(be);
                        }
                        return Optional.empty();
                    }
                    if (name.equals("a") && p.length == 4 && p[0] == int.class && p[1] == int.class
                            && p[2] == c("dvz") && p[3] == boolean.class
                            && c("duy").isAssignableFrom(m.getReturnType())) {
                        long key = ((long) (int) a[0] & 0xFFFFFFFFL) | ((long) (int) a[1] << 32);
                        Object ch = chunks.get(key);
                        if (ch == null) {
                            ch = c("dvs").getConstructor(c("dcd"), c("dvv"), c("dcy"), c("jz"), c("eab"))
                                    .newInstance(
                                            c("dcd").getConstructor(int.class, int.class)
                                                    .newInstance((int) a[0], (int) a[1]),
                                            f("dvv", "a"),
                                            c("dcy").getMethod("e", int.class, int.class)
                                                    .invoke(null, -64, 384),
                                            biomeRegistry, null);
                            chunks.put(key, ch);
                        }
                        return ch;
                    }
                    // setBlock: keep the block entity a writer expects to find, then let the
                    // shared recorded level own the write.
                    if (name.equals("a") && (p.length == 3 || p.length == 4)
                            && p[0] == c("jd") && p[1] == c("dtc") && p[2] == int.class) {
                        int[] xyz = NativeHarness.xyz(a[0]);
                        Object res = m.invoke(base.proxy, a);
                        Object posObj = NativeHarness.pos(xyz[0], xyz[1], xyz[2]);
                        String bname = NativeHarness.nameOf(a[1]);
                        List<Integer> key = List.of(xyz[0], xyz[1], xyz[2]);
                        if (bname.equals("minecraft:chest")) {
                            world.blockEntities.put(key, c("dqo").getConstructor(c("jd"), c("dtc"))
                                    .newInstance(posObj, a[1]));
                        } else if (bname.equals("minecraft:spawner")) {
                            world.blockEntities.put(key, c("drw").getConstructor(c("jd"), c("dtc"))
                                    .newInstance(posObj, a[1]));
                        } else {
                            world.blockEntities.remove(key);
                        }
                        return res;
                    }
                    if (m.isDefault()) return InvocationHandler.invokeDefault(self, m, a);
                    try {
                        return m.invoke(base.proxy, a);
                    } catch (InvocationTargetException e) {
                        throw e.getCause();
                    }
                });
        return world;
    }

    // ---------- result plumbing ----------

    static String sha256(NativeHarness.Level base) throws Exception {
        List<int[]> keys = new ArrayList<>();
        for (String k : base.writes.keySet()) {
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
            Object st = base.writes.get(k[0] + "," + k[1] + "," + k[2]);
            md.update((k[0] + "," + k[1] + "," + k[2] + " " + NativeHarness.normalizedStateName(st) + "\n")
                    .getBytes(StandardCharsets.UTF_8));
        }
        StringBuilder sha = new StringBuilder();
        for (byte b : md.digest()) sha.append(String.format("%02x", b));
        return keys.size() + ":" + sha;
    }

    static String lootString(Map<List<Integer>, Object> blockEntities) throws Exception {
        List<String> rows = new ArrayList<>();
        for (var e : blockEntities.entrySet()) {
            Object be = e.getValue();
            if (!c("drn").isInstance(be)) continue;
            Object lt = c("drn").getMethod("aB_").invoke(be);
            if (lt == null) continue;
            Object rl = c("akq").getMethod("a").invoke(lt);
            long lseed = (long) c("drn").getMethod("aC_").invoke(be);
            List<Integer> xyz = e.getKey();
            rows.add(xyz.get(0) + "," + xyz.get(1) + "," + xyz.get(2) + "|chest|" + rl + "|" + lseed);
        }
        Collections.sort(rows);
        return rows.isEmpty() ? "-" : String.join(";", rows);
    }

    static String entityString(List<String> entities) {
        if (entities.isEmpty()) return "-";
        List<String> sorted = new ArrayList<>(entities);
        Collections.sort(sorted);
        return String.join(";", sorted);
    }

    static int[] parseBox(String s) {
        String[] parts = s.split(",");
        int[] out = new int[6];
        for (int i = 0; i < 6; i++) out[i] = Integer.parseInt(parts[i].trim());
        return out;
    }

    static boolean meets(int[] piece, int[] clip) {
        return piece[3] >= clip[0] && piece[0] <= clip[3]
                && piece[4] >= clip[1] && piece[1] <= clip[4]
                && piece[5] >= clip[2] && piece[2] <= clip[5];
    }

    /** Run one clip sequence over a piece list and hash every write it made. */
    static void placeAll(List<?> pieces, World world, Object postRng, Object sm, Object generator, String clips)
            throws Exception {
        for (String clipSpec : clips.split(";")) {
            int[] coords = parseBox(clipSpec);
            Object clip = bb(coords[0], coords[1], coords[2], coords[3], coords[4], coords[5]);
            Object chunkPos = c("dcd").getConstructor(int.class, int.class)
                    .newInstance(coords[0] >> 4, coords[2] >> 4);
            Object origin = NativeHarness.pos(coords[0], coords[1], coords[2]);
            for (Object piece : pieces) {
                if (!meets(pieceBox(piece), coords)) continue;
                c("ejv").getMethod("a", c("dds"), c("ddq"), c("duz"), c("ayw"), c("ejj"), c("dcd"), c("jd"))
                        .invoke(piece, world.proxy, sm, generator, postRng, clip, chunkPos, origin);
            }
        }
    }

    // ---------- cases ----------

    static String fortress(String caseId, long seed, int cx, int cz, String clips, String sceneSpec,
                           Object access) throws Exception {
        Scene scene = new Scene(sceneSpec);
        Object genRng = xorRng(seed ^ 0x9E3779B97F4A7C15L);
        Object ctx = context(access, genRng, seed, cx, cz);
        Object builder = c("ekj").getConstructor().newInstance();
        declared(c("ely"), "a", c("ekj"), c("ejr$a")).invoke(null, builder, ctx);
        long rngAfterGen = nextLong(genRng);
        List<?> pieces = builderPieces(builder);
        String bounds = fmtBox(bbCoords(c("ekj").getMethod("d").invoke(builder)));

        StringBuilder pieceCol = new StringBuilder();
        for (int i = 0; i < pieces.size(); i++) {
            if (i > 0) pieceCol.append('|');
            pieceCol.append(pieceKey(pieces.get(i), false));
        }
        String orient = (String) c("ji").getMethod("n").invoke(c("ejv").getMethod("i").invoke(pieces.get(0)));

        World world = makeWorld(access, legacyRng(seed ^ 0x517CC1B727220A95L), null, scene, seed, 63);
        Object postRng = xorRng(seed ^ 0x2B992DDFA23249D6L);
        Object sm = c("ddq").getConstructor(c("dcx"), c("dzw"), c("ejs")).newInstance(world.proxy, null, null);
        placeAll(pieces, world, postRng, sm, null, clips);
        long rngAfterPlace = nextLong(postRng);
        String writes = sha256(world.base);
        return String.join("\t", "fortress", caseId, Long.toString(seed), Integer.toString(cx),
                Integer.toString(cz), orient, clips, sceneSpec, bounds,
                Integer.toString(pieces.size()), pieceCol.toString(), writes,
                Long.toString(rngAfterGen), Long.toString(rngAfterPlace), lootString(world.blockEntities));
    }

    static String monument(String caseId, long seed, int cx, int cz, int seaLevel, String clips,
                           String sceneSpec, Object access) throws Exception {
        Scene scene = new Scene(sceneSpec);
        Object genRng = xorRng(seed ^ 0x9E3779B97F4A7C15L);
        Object ctx = context(access, genRng, seed, cx, cz);
        Object builder = c("ekj").getConstructor().newInstance();
        declared(c("emc"), "a", c("ekj"), c("ejr$a")).invoke(null, builder, ctx);
        long rngAfterGen = nextLong(genRng);
        List<?> top = builderPieces(builder);
        if (top.size() != 1) throw new AssertionError("monument builder should hold one building");
        Object building = top.get(0);
        List<?> children = monumentChildren(building);

        StringBuilder pieceCol = new StringBuilder();
        pieceCol.append(pieceKey(building, true));
        for (Object child : children) {
            pieceCol.append('|').append(pieceKey(child, true));
        }
        String orient = (String) c("ji").getMethod("n").invoke(c("ejv").getMethod("i").invoke(building));

        Object levelRng = legacyRng(seed ^ 0x517CC1B727220A95L);
        Object serverLevel = makeServerLevel(access, levelRng);
        World world = makeWorld(access, levelRng, serverLevel, scene, seed, seaLevel);
        Object postRng = xorRng(seed ^ 0x2B992DDFA23249D6L);
        Object sm = c("ddq").getConstructor(c("dcx"), c("dzw"), c("ejs")).newInstance(world.proxy, null, null);
        placeAll(top, world, postRng, sm, null, clips);
        long rngAfterPlace = nextLong(postRng);
        String writes = sha256(world.base);
        return String.join("\t", "monument", caseId, Long.toString(seed), Integer.toString(cx),
                Integer.toString(cz), orient, Integer.toString(seaLevel), clips, sceneSpec,
                Integer.toString(1 + children.size()), pieceCol.toString(), writes,
                Long.toString(rngAfterGen), Long.toString(rngAfterPlace),
                entityString(world.entities), roomGraphColumn(building));
    }

    static String clip(int cx, int cz) {
        return (cx << 4) + ",-64," + (cz << 4) + "," + ((cx << 4) + 15) + ",319," + ((cz << 4) + 15);
    }

    static String clip(int x0, int z0, int x1, int z1) {
        return x0 + ",-64," + z0 + "," + x1 + ",319," + z1;
    }

    public static void main(String[] args) throws Exception {
        NativeHarness.bootstrap();
        Object access = c("ka").getMethod("a", c("jz")).invoke(null, f("lt", "aA"));
        Object manager = c("atx").getConstructor(c("ass"), List.class)
                .newInstance(f("ass", "b"), List.of(c("ats").getMethod("b").invoke(null)));
        access = c("akm").getMethod("a", c("aue"), c("ka"), List.class).invoke(null, manager, access, f("akm", "a"));

        List<String> rows = new ArrayList<>();
        // Fortress: one whole-structure clip in pure air (fillColumnDown runs to the build
        // floor), one over a stone slab (it stops), one walked chunk by chunk (the turn
        // pieces latch their chest off), plus water and ice scenes for the fill rules.
        rows.add(fortress("whole_air_0_0", 4242L, 0, 0, clip(-64, -64, 64, 64), "", access));
        rows.add(fortress("whole_slab_3_-2", 4242L, 3, -2, clip(-16, -64, 80, 64), "stone=40:47", access));
        rows.add(fortress("chunks_walk_1_1", 4242L, 1, 1,
                clip(1, 1) + ";" + clip(2, 1) + ";" + clip(0, 1) + ";" + clip(1, 0) + ";" + clip(2, 2),
                "stone=52:58", access));
        rows.add(fortress("whole_air_neg1_5", 0L, -1, 5, clip(-64, 32, 64, 96), "", access));
        rows.add(fortress("whole_water_2_2", 1L, 2, 2, clip(-16, -16, 48, 48), "stone=30:38,water=39:62",
                access));
        rows.add(fortress("whole_ice_shelf", 7L, -3, 0, clip(-64, -32, 32, 48),
                "stone=44:49,water=50:62,ice=56", access));
        rows.add(fortress("clipped_tight", 99L, 0, 0, clip(8, 8, 23, 23), "stone=50:60", access));

        // Monument: whole structure, a chunk walk, alternate orientations, the sea-level
        // branch of generateWaterBox, and a shallow sea so the sponge rooms write air.
        rows.add(monument("whole_0_0_s63", 4242L, 0, 0, 63, clip(-48, -48, 96, 96), "stone=30:39,water=40:62",
                access));
        rows.add(monument("whole_0_0_s40", 4242L, 0, 0, 40, clip(-48, -48, 96, 96), "stone=30:39,water=40:62",
                access));
        rows.add(monument("whole_1_1_s63", 0L, 1, 1, 63, clip(-32, -32, 80, 80), "stone=30:39,water=40:62",
                access));
        rows.add(monument("chunks_walk_0_0", 4242L, 0, 0, 63,
                clip(-2, -2) + ";" + clip(-1, -2) + ";" + clip(-2, -1) + ";" + clip(-1, -1) + ";"
                        + clip(0, -1) + ";" + clip(-1, 0) + ";" + clip(0, 0) + ";" + clip(1, 0),
                "stone=30:39,water=40:62", access));
        rows.add(monument("whole_neg2_3_s63", 7L, -2, 3, 63, clip(-64, -16, 32, 64), "stone=30:43,water=44:62",
                access));
        rows.add(monument("whole_sponge_s63", 11L, 2, 2, 63, clip(-32, -32, 64, 64), "stone=30:39,water=40:55",
                access));
        rows.add(monument("clipped_core", 99L, 0, 0, 63, clip(-8, -8, 24, 24), "stone=30:39,water=40:62",
                access));

        Files.writeString(Path.of(args[0]), String.join("\n", rows) + "\n", StandardCharsets.UTF_8);
    }
}
