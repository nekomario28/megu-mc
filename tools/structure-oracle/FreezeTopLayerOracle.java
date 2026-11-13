package dev.mcspine.tools;

import java.lang.invoke.MethodHandles;
import java.lang.reflect.*;
import java.nio.charset.StandardCharsets;
import java.nio.file.*;
import java.util.*;
import java.util.function.Predicate;

/**
 * Actual 1.21.1 `minecraft:freeze_top_layer` (`edn`, SnowAndFreezeFeature).
 *
 * Drives the pinned class's `place` method with a real `WorldGenLevel` proxy that
 * loads real pinned-pack `Biome` objects, computes `MOTION_BLOCKING` via the native
 * heightmap predicate (`dyy$a.e`), enforces `LightLayer.BLOCK` explicitly, and
 * delegates block state / fluid state / height / tags to the pinned jar.
 */
public final class FreezeTopLayerOracle {
    interface BiomeAt {
        String biome(int x, int y, int z);
    }

    interface LightAt {
        int blockLight(int x, int y, int z);
    }

    static Object F(String cls, String field) throws Exception {
        return NativeHarness.C(cls).getField(field).get(null);
    }

    static Map<String, Object> loadBiomes() throws Exception {
        Object access = NativeHarness.C("ka").getMethod("a", NativeHarness.C("jz"))
                .invoke(null, F("lt", "aA"));
        Object pack = NativeHarness.C("ats").getMethod("b").invoke(null);
        Object resource = NativeHarness.C("atx")
                .getConstructor(NativeHarness.C("ass"), List.class)
                .newInstance(F("ass", "b"), List.of(pack));
        try {
            access = NativeHarness.C("akm")
                    .getMethod("a", NativeHarness.C("aue"), NativeHarness.C("ka"), List.class)
                    .invoke(null, resource, access, F("akm", "a"));
        } finally {
            if (resource instanceof AutoCloseable c) c.close();
        }
        Object registry = access.getClass().getMethod("d", NativeHarness.C("akq"))
                .invoke(access, F("lu", "aF"));
        Map<String, Object> biomes = new HashMap<>();
        for (Object e : (Set<?>) registry.getClass().getMethod("h").invoke(registry)) {
            var entry = (Map.Entry<?, ?>) e;
            String name = entry.getKey().getClass().getMethod("a").invoke(entry.getKey()).toString();
            biomes.put(name, entry.getValue());
        }
        return biomes;
    }

    @SuppressWarnings("unchecked")
    public static void main(String[] args) throws Exception {
        NativeHarness.bootstrap();
        Class<?> cls = NativeHarness.featureClass("freeze_top_layer");
        if (!cls.getName().equals("edn")) throw new AssertionError("class changed: " + cls);

        Map<String, Object> biomes = loadBiomes();
        Object motionBlocking = F("dyy$a", "e"); // Heightmap.Types.MOTION_BLOCKING
        Predicate<Object> motionPredicate = (Predicate<Object>) NativeHarness.C("dyy$a")
                .getMethod("e").invoke(motionBlocking);
        Object blockLightLayer = F("ddf", "b"); // LightLayer.BLOCK
        Object air = NativeHarness.state("minecraft:air");
        Object grass = NativeHarness.state("minecraft:grass_block");
        Object grassSnowy = setBool(grass, "dnj", "c", true);
        Object podzol = NativeHarness.state("minecraft:podzol");
        Object mycelium = NativeHarness.state("minecraft:mycelium");
        Object water0 = NativeHarness.state("minecraft:water");
        Object water1 = setInt(water0, "level", 1);
        Object slabBottom = NativeHarness.state("minecraft:oak_slab");
        Object slabTop = setEnum(slabBottom, "type", "top");
        Object slabWaterlogged = setBoolField(slabBottom, "waterlogged", true);
        Object snow8 = setInt(NativeHarness.state("minecraft:snow"), "layers", 8);

        List<String> rows = new ArrayList<>();

        // 1. Positive snow placement & SNOWY=true propagation (grass, podzol, mycelium, stone)
        rows.add(run("cold_grass_air", 0, 64, 0, biomes, motionBlocking, motionPredicate, blockLightLayer,
                (x, y, z) -> (x == 0 && z == 0 && y == 64) ? grass : air,
                (x, y, z) -> "minecraft:snowy_plains", (x, y, z) -> 0));
        rows.add(run("cold_podzol_air", 0, 64, 0, biomes, motionBlocking, motionPredicate, blockLightLayer,
                (x, y, z) -> (x == 0 && z == 0 && y == 64) ? podzol : air,
                (x, y, z) -> "minecraft:snowy_plains", (x, y, z) -> 0));
        rows.add(run("cold_mycelium_air", 0, 64, 0, biomes, motionBlocking, motionPredicate, blockLightLayer,
                (x, y, z) -> (x == 0 && z == 0 && y == 64) ? mycelium : air,
                (x, y, z) -> "minecraft:snowy_plains", (x, y, z) -> 0));
        rows.add(run("cold_stone_air", 0, 64, 0, biomes, motionBlocking, motionPredicate, blockLightLayer,
                (x, y, z) -> (x == 0 && z == 0 && y == 64) ? NativeHarness.state("minecraft:stone") : air,
                (x, y, z) -> "minecraft:snowy_plains", (x, y, z) -> 0));

        // 2. Cold + plant above grass (must NOT place snow and must NOT set snowy=true)
        for (String plant : List.of("short_grass", "poppy", "dandelion")) {
            Object plantSt = NativeHarness.state("minecraft:" + plant);
            rows.add(run("cold_grass_" + plant, 0, 64, 0, biomes, motionBlocking, motionPredicate, blockLightLayer,
                    (x, y, z) -> {
                        if (x == 0 && z == 0 && y == 64) return grass;
                        if (x == 0 && z == 0 && y == 65) return plantSt;
                        return air;
                    },
                    (x, y, z) -> "minecraft:snowy_plains", (x, y, z) -> 0));
        }

        // 3. Cold + already snowy grass with plant above: FreezeTop does NOT revert or write
        rows.add(run("cold_snowy_grass_poppy", 0, 64, 0, biomes, motionBlocking, motionPredicate, blockLightLayer,
                (x, y, z) -> {
                    if (x == 0 && z == 0 && y == 64) return grassSnowy;
                    if (x == 0 && z == 0 && y == 65) return NativeHarness.state("minecraft:poppy");
                    return air;
                },
                (x, y, z) -> "minecraft:snowy_plains", (x, y, z) -> 0));

        // 4. Existing snow layer on grass_block[snowy=false]
        rows.add(run("cold_grass_existing_snow", 0, 64, 0, biomes, motionBlocking, motionPredicate, blockLightLayer,
                (x, y, z) -> {
                    if (x == 0 && z == 0 && y == 64) return grass;
                    if (x == 0 && z == 0 && y == 65) return NativeHarness.state("minecraft:snow");
                    return air;
                },
                (x, y, z) -> "minecraft:snowy_plains", (x, y, z) -> 0));

        // 5. Water source vs flowing water vs waterlogged slab
        rows.add(run("cold_water_source", 0, 64, 0, biomes, motionBlocking, motionPredicate, blockLightLayer,
                (x, y, z) -> (x == 0 && z == 0 && y == 63) ? water0 : air,
                (x, y, z) -> "minecraft:snowy_plains", (x, y, z) -> 0));
        rows.add(run("cold_flowing_water", 0, 64, 0, biomes, motionBlocking, motionPredicate, blockLightLayer,
                (x, y, z) -> (x == 0 && z == 0 && y == 63) ? water1 : air,
                (x, y, z) -> "minecraft:snowy_plains", (x, y, z) -> 0));
        rows.add(run("cold_waterlogged_slab", 0, 64, 0, biomes, motionBlocking, motionPredicate, blockLightLayer,
                (x, y, z) -> (x == 0 && z == 0 && y == 63) ? slabWaterlogged : air,
                (x, y, z) -> "minecraft:snowy_plains", (x, y, z) -> 0));

        // 6. SnowLayerBlock.canSurvive cases: ice / packed_ice (cannot), soul_sand / mud / leaves / slabs / snow[layers=8]
        for (String name : List.of("ice", "packed_ice", "soul_sand", "mud", "oak_leaves")) {
            Object st = NativeHarness.state("minecraft:" + name);
            rows.add(run("cold_" + name, 0, 64, 0, biomes, motionBlocking, motionPredicate, blockLightLayer,
                    (x, y, z) -> (x == 0 && z == 0 && y == 64) ? st : air,
                    (x, y, z) -> "minecraft:snowy_plains", (x, y, z) -> 0));
        }
        rows.add(run("cold_slab_bottom", 0, 64, 0, biomes, motionBlocking, motionPredicate, blockLightLayer,
                (x, y, z) -> (x == 0 && z == 0 && y == 64) ? slabBottom : air,
                (x, y, z) -> "minecraft:snowy_plains", (x, y, z) -> 0));
        rows.add(run("cold_slab_top", 0, 64, 0, biomes, motionBlocking, motionPredicate, blockLightLayer,
                (x, y, z) -> (x == 0 && z == 0 && y == 64) ? slabTop : air,
                (x, y, z) -> "minecraft:snowy_plains", (x, y, z) -> 0));
        rows.add(run("cold_snow8", 0, 64, 0, biomes, motionBlocking, motionPredicate, blockLightLayer,
                (x, y, z) -> (x == 0 && z == 0 && y == 64) ? snow8 : air,
                (x, y, z) -> "minecraft:snowy_plains", (x, y, z) -> 0));

        // 7. Leaves above grass: MOTION_BLOCKING stops at leaves, grass below untouched
        rows.add(run("cold_leaves_above_grass", 0, 64, 0, biomes, motionBlocking, motionPredicate, blockLightLayer,
                (x, y, z) -> {
                    if (x == 0 && z == 0 && y == 64) return grass;
                    if (x == 0 && z == 0 && y == 70) return NativeHarness.state("minecraft:oak_leaves");
                    return air;
                },
                (x, y, z) -> "minecraft:snowy_plains", (x, y, z) -> 0));

        // 8. Block light >= 10 refuses both freeze and snow
        rows.add(run("cold_light10_grass", 0, 64, 0, biomes, motionBlocking, motionPredicate, blockLightLayer,
                (x, y, z) -> (x == 0 && z == 0 && y == 64) ? grass : air,
                (x, y, z) -> "minecraft:snowy_plains", (x, y, z) -> 10));
        rows.add(run("cold_light10_water", 0, 64, 0, biomes, motionBlocking, motionPredicate, blockLightLayer,
                (x, y, z) -> (x == 0 && z == 0 && y == 63) ? water0 : air,
                (x, y, z) -> "minecraft:snowy_plains", (x, y, z) -> 10));

        // 9. Warm biome (plains) and altitude-dependent biome (taiga at y=64 vs y=170)
        rows.add(run("warm_plains", 0, 64, 0, biomes, motionBlocking, motionPredicate, blockLightLayer,
                (x, y, z) -> {
                    if (x == 0 && z == 0 && y == 64) return grass;
                    if (x == 1 && z == 0 && y == 63) return water0;
                    return air;
                },
                (x, y, z) -> "minecraft:plains", (x, y, z) -> 0));
        rows.add(run("taiga_low_64", 0, 64, 0, biomes, motionBlocking, motionPredicate, blockLightLayer,
                (x, y, z) -> (x == 0 && z == 0 && y == 64) ? grass : air,
                (x, y, z) -> "minecraft:taiga", (x, y, z) -> 0));
        rows.add(run("taiga_high_170", 0, 170, 0, biomes, motionBlocking, motionPredicate, blockLightLayer,
                (x, y, z) -> (x == 0 && z == 0 && y == 170) ? grass : air,
                (x, y, z) -> "minecraft:taiga", (x, y, z) -> 0));

        // 10. Frozen ocean modifier (at y=63 water, multiple columns to capture warm vs frozen pockets)
        rows.add(run("frozen_ocean_grid", 0, 64, 0, biomes, motionBlocking, motionPredicate, blockLightLayer,
                (x, y, z) -> (y == 63 && x >= 0 && x < 16 && z >= 0 && z < 16) ? water0 : air,
                (x, y, z) -> "minecraft:frozen_ocean", (x, y, z) -> 0));

        // 11. Build ceiling guard (solid at max_y - 1 = 319 -> top=320 out of bounds)
        rows.add(run("build_ceiling_319", 0, 319, 0, biomes, motionBlocking, motionPredicate, blockLightLayer,
                (x, y, z) -> (x == 0 && z == 0 && y == 319) ? grass : air,
                (x, y, z) -> "minecraft:snowy_plains", (x, y, z) -> 0));

        // 12. Non-zero origin offsets (x=48, z=-32 like chunk 3,-2)
        rows.add(run("chunk_3_m2_offset", 48, 64, -32, biomes, motionBlocking, motionPredicate, blockLightLayer,
                (x, y, z) -> {
                    if (x == 50 && z == -31 && y == 64) return grass;
                    if (x == 51 && z == -30 && y == 64) return grass;
                    if (x == 51 && z == -30 && y == 65) return NativeHarness.state("minecraft:short_grass");
                    return air;
                },
                (x, y, z) -> "minecraft:snowy_plains", (x, y, z) -> 0));

        Files.write(Path.of(args[0]), rows);
        System.out.println("native freeze_top_layer cases: " + rows.size());
    }

    static Object setBool(Object state, String cls, String fieldName, boolean val) throws Exception {
        Object prop = F(cls, fieldName);
        return NativeHarness.C("dte").getMethod("a", NativeHarness.C("duf"), Comparable.class)
                .invoke(state, prop, val);
    }

    static Object findProp(Object state, String propName) throws Exception {
        Method nameM = NativeHarness.C("duf").getMethod("f");
        for (Object p : (Collection<?>) NativeHarness.C("dte").getMethod("B").invoke(state)) {
            if (propName.equals(nameM.invoke(p))) return p;
        }
        throw new AssertionError("missing prop " + propName + " on " + state);
    }

    static Object setInt(Object state, String propName, int val) throws Exception {
        return NativeHarness.C("dte").getMethod("a", NativeHarness.C("duf"), Comparable.class)
                .invoke(state, findProp(state, propName), val);
    }

    static Object setBoolField(Object state, String propName, boolean val) throws Exception {
        return NativeHarness.C("dte").getMethod("a", NativeHarness.C("duf"), Comparable.class)
                .invoke(state, findProp(state, propName), val);
    }

    @SuppressWarnings({"unchecked", "rawtypes"})
    static Object setEnum(Object state, String propName, String valName) throws Exception {
        Object prop = findProp(state, propName);
        Method parse = NativeHarness.C("duf").getMethod("b", String.class);
        Optional<?> opt = (Optional<?>) parse.invoke(prop, valName);
        Comparable<?> val = (Comparable<?>) opt.orElseThrow();
        return NativeHarness.C("dte").getMethod("a", NativeHarness.C("duf"), Comparable.class)
                .invoke(state, prop, val);
    }

    static String run(
            String id, int ox, int oy, int oz,
            Map<String, Object> biomes,
            Object motionBlocking,
            Predicate<Object> motionPredicate,
            Object blockLightLayer,
            NativeHarness.StateScene scene,
            BiomeAt biomeAt,
            LightAt lightAt
    ) throws Exception {
        Map<String, Object> writes = new LinkedHashMap<>();
        List<String> events = new ArrayList<>();
        Object heights = NativeHarness.heightC.getMethod("e", int.class, int.class)
                .invoke(null, NativeHarness.MIN_Y, NativeHarness.HEIGHT);
        Method fluidMethod = NativeHarness.stateC.getMethod("u");
        Method directHolder = NativeHarness.C("jm").getMethod("a", Object.class);
        Method holderGet = NativeHarness.C("jm").getMethod("a");

        Object proxy = Proxy.newProxyInstance(
                FreezeTopLayerOracle.class.getClassLoader(),
                new Class<?>[]{NativeHarness.worldC},
                (self, m, a) -> {
                    String n = m.getName();
                    Class<?>[] pt = m.getParameterTypes();
                    if (n.equals("a_") && pt.length == 1 && pt[0] == NativeHarness.posC) {
                        int[] p = NativeHarness.xyz(a[0]);
                        Object w = writes.get(p[0] + "," + p[1] + "," + p[2]);
                        return w != null ? w : scene.stateAt(p[0], p[1], p[2]);
                    }
                    if (n.equals("b_") && pt.length == 1 && pt[0] == NativeHarness.posC) {
                        int[] p = NativeHarness.xyz(a[0]);
                        Object w = writes.get(p[0] + "," + p[1] + "," + p[2]);
                        Object st = w != null ? w : scene.stateAt(p[0], p[1], p[2]);
                        return fluidMethod.invoke(st);
                    }
                    if (n.equals("c_") && pt.length == 1 && pt[0] == NativeHarness.posC) {
                        return null;
                    }
                    if (n.equals("a") && pt.length == 3 && pt[0] == NativeHarness.C("dyy$a")
                            && pt[1] == int.class && pt[2] == int.class) {
                        if (a[0] != motionBlocking) throw new AssertionError("unexpected heightmap: " + a[0]);
                        int x = (int) a[1], z = (int) a[2];
                        for (int y = NativeHarness.MIN_Y + NativeHarness.HEIGHT - 1; y >= NativeHarness.MIN_Y; y--) {
                            Object w = writes.get(x + "," + y + "," + z);
                            Object st = w != null ? w : scene.stateAt(x, y, z);
                            if (motionPredicate.test(st)) return y + 1;
                        }
                        return NativeHarness.MIN_Y;
                    }
                    if (n.equals("t") && pt.length == 1 && pt[0] == NativeHarness.posC) {
                        int[] p = NativeHarness.xyz(a[0]);
                        Object b = biomes.get(biomeAt.biome(p[0], p[1], p[2]));
                        if (b == null) throw new AssertionError("unknown biome at " + Arrays.toString(p));
                        return directHolder.invoke(null, b);
                    }
                    if (n.equals("a") && pt.length == 2 && pt[0] == NativeHarness.C("ddf")
                            && pt[1] == NativeHarness.posC) {
                        if (a[0] != blockLightLayer) throw new AssertionError("unexpected light layer: " + a[0]);
                        int[] p = NativeHarness.xyz(a[1]);
                        return lightAt.blockLight(p[0], p[1], p[2]);
                    }
                    if (n.equals("a") && (pt.length == 3 || pt.length == 4)
                            && pt[0] == NativeHarness.posC && pt[1] == NativeHarness.stateC
                            && pt[2] == int.class && (pt.length == 3 || pt[3] == int.class)
                            && m.getReturnType() == boolean.class) {
                        int[] p = NativeHarness.xyz(a[0]);
                        if (p[1] < NativeHarness.MIN_Y || p[1] >= NativeHarness.MIN_Y + NativeHarness.HEIGHT) {
                            return false;
                        }
                        writes.put(p[0] + "," + p[1] + "," + p[2], a[1]);
                        events.add(p[0] + "," + p[1] + "," + p[2] + "=" + NativeHarness.normalizedStateName(a[1]));
                        return true;
                    }
                    Method hm = NativeHarness.heightFamily(m);
                    if (hm != null) return hm.invoke(heights, a);
                    if (m.isDefault()) {
                        return MethodHandles.privateLookupIn(m.getDeclaringClass(), MethodHandles.lookup())
                                .unreflectSpecial(m, m.getDeclaringClass())
                                .bindTo(self)
                                .invokeWithArguments(a == null ? new Object[0] : a);
                    }
                    throw new UnsupportedOperationException(m.toString());
                });

        Object feature = NativeHarness.feature("freeze_top_layer");
        Object cfg = NativeHarness.config("freeze_top_layer", "{}");
        Constructor<?> ctxCtor = null;
        for (Constructor<?> c : NativeHarness.C("ecg").getConstructors()) {
            Class<?>[] pt = c.getParameterTypes();
            if (pt.length == 6 && pt[0] == Optional.class && pt[1] == NativeHarness.worldC
                    && pt[3] == NativeHarness.C("ayw") && pt[4] == NativeHarness.posC && pt[5].isInstance(cfg)) {
                ctxCtor = c;
                break;
            }
        }
        Object rng = NativeHarness.rng(0L);
        Object ctx = ctxCtor.newInstance(Optional.empty(), proxy, NativeHarness.probeGenerator(),
                rng, NativeHarness.pos(ox, oy, oz), cfg);
        Method place = feature.getClass().getMethod("a", NativeHarness.C("ecg"));
        boolean placed = (boolean) place.invoke(feature, ctx);
        return id + "\t" + ox + "," + oy + "," + oz + "\t" + placed + "\t" + String.join(";", events);
    }
}
