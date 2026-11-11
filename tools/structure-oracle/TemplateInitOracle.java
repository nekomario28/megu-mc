package dev.mcspine.tools;
import java.io.*;
import java.lang.reflect.*;
import java.nio.file.*;
import java.util.*;
/**
 * Actual native StructureTemplate placement of a block that carries NBT.
 *
 * The level is a real `ProtoChunk` plus a `RegistryAccess` loaded from the vanilla
 * datapack, so `getBlockEntity` and `registryAccess` answer for real: the biome wall
 * that blocks a bare `ProtoChunk` (1.21 has no static biome registry) is gone.
 *
 * Boundary, stated because it is not the whole story: the level creates the block
 * entity in `LevelChunk.setBlockState`, and a `ProtoChunk` does not, so this file
 * registers one at the target before placing. Everything after that is native: the
 * assert on `getBlockEntity`, the clear to air with flags 20, the state write, the
 * re-fetch and `loadWithComponents(nbt, registryAccess)`. So this pins how NBT is
 * *decoded and applied* (including side effects such as a generated
 * `LootTableSeed`), not that a level creates the entity.
 *
 * Reports every written cell with its canonical state and the SHA-256 of the block
 * entity's own `saveWithFullMetadata` output, or `absent`.
 */
public final class TemplateInitOracle {
    static Class<?> C(String n) { try { return Class.forName(n); } catch (ClassNotFoundException e) { throw new AssertionError(e); } }
    /** Native classes are package-private in places: open the declared ctor instead of the public one. */
    static Object make(Class<?> owner, Class<?>[] types, Object... args) throws Exception {
        Constructor<?> c = owner.getDeclaredConstructor(types);
        c.setAccessible(true);
        return c.newInstance(args);
    }
    static Object fget(Class<?> owner, String name, Object target) throws Exception {
        Field f = owner.getDeclaredField(name);
        f.setAccessible(true);
        return f.get(target);
    }
    static void fset(Class<?> owner, String name, Object target, Object value) throws Exception {
        Field f = owner.getDeclaredField(name);
        f.setAccessible(true);
        f.set(target, value);
    }

    static Object registryAccess(Path data) throws Exception {
        Object component = C("wz").getMethod("a", String.class).invoke(null, "vanilla");
        Object location = C("asp").getConstructor(String.class, C("wz"), C("atq"), Optional.class)
                .newInstance("vanilla", component, C("atq").getField("b").get(null), Optional.empty());
        Object pack = C("ast").getConstructor(C("asp"), Path.class).newInstance(location, data);
        Object rm = C("atx").getConstructor(C("ass"), List.class)
                .newInstance(C("ass").getField("b").get(null), List.of(pack));
        Field rootField = C("lt").getDeclaredField("aD");
        rootField.setAccessible(true);
        Object base = C("ka").getMethod("a", C("jz")).invoke(null, rootField.get(null));
        return C("akm").getMethod("a", C("aue"), C("ka"), List.class)
                .invoke(null, rm, base, C("akm").getField("a").get(null));
    }

    static Object chunk(int cx, int cz, Object worldgen) throws Exception {
        Object biomes = C("ka").getMethod("d", C("akq")).invoke(worldgen, C("lu").getField("aF").get(null));
        Object chunkPos = C("dcd").getConstructor(int.class, int.class).newInstance(cx, cz);
        Object heights = C("dcy").getMethod("e", int.class, int.class).invoke(null, -64, 384);
        return C("dvs").getConstructor(C("dcd"), C("dvv"), C("dcy"), C("jz"), C("eab"))
                .newInstance(chunkPos, C("dvv").getField("a").get(null), heights, biomes, null);
    }

    static Object blockEntityFor(Object state, Object pos) throws Exception {
        Class<?> entityBlock = C("diq");
        Object block = C("dtc").getMethod("b").invoke(state);
        if (!entityBlock.isInstance(block)) return null;
        return entityBlock.getMethod("a", C("jd"), C("dtc")).invoke(block, pos, state);
    }

    public static void main(String[] args) throws Exception {
        NativeHarness.bootstrap();
        StructureTemplateOracle.initNbt();
        Object worldgen = registryAccess(Path.of(args[0]));
        Object provider = worldgen;
        Object chestState = NativeHarness.state("chest");
        Object stoneState = NativeHarness.state("stone");
        Object zero = C("jd").getConstructor(int.class, int.class, int.class).newInstance(0, 0, 0);

        // Template: a chest that carries NBT at (0,0,0), a plain stone block at (1,0,0).
        // The tag is produced by a real entity, then extended with the two keys a
        // structure would store, so the decoding path is the game's own.
        Object chunk0 = chunk(0, 0, worldgen);
        Object seedPos = C("jd").getConstructor(int.class, int.class, int.class).newInstance(4, 80, 4);
        Object seedEntity = blockEntityFor(chestState, seedPos);
        chunk0.getClass().getMethod("a", C("dqh")).invoke(chunk0, seedEntity);
        chunk0.getClass().getMethod("a", C("jd"), C("dtc"), boolean.class).invoke(chunk0, seedPos, chestState, false);
        Object saved = seedEntity.getClass().getMethod("b", C("jo$a")).invoke(seedEntity, provider);
        Object tag = C("ub").getConstructor().newInstance();
        for (Object key : (Set<?>) C("ub").getMethod("e").invoke(saved)) {
            Object value = C("ub").getMethod("c", String.class).invoke(saved, key);
            C("ub").getMethod("a", String.class, C("uy")).invoke(tag, key, value);
        }
        C("ub").getMethod("a", String.class, String.class).invoke(tag, "LootTable", "minecraft:chests/simple_dungeon");
        C("ub").getMethod("a", String.class, String.class).invoke(tag, "CustomName", "{\"text\":\"Crate\"}");

        Object chestInfo = make(C("ent$c"), new Class<?>[]{C("jd"), C("dtc"), C("ub")}, zero, chestState, tag);
        Object stoneInfo = make(C("ent$c"), new Class<?>[]{C("jd"), C("dtc"), C("ub")},
                C("jd").getConstructor(int.class, int.class, int.class).newInstance(1, 0, 0), stoneState, null);
        Object palette = make(C("ent$a"), new Class<?>[]{List.class}, List.of(chestInfo, stoneInfo));
        Object template = make(C("ent"), new Class<?>[]{});
        fset(C("ent"), "l", template, List.of(palette));
        fset(C("ent"), "n", template, make(C("kh"), new Class<?>[]{int.class, int.class, int.class}, 2, 1, 1));
        fset(C("ent"), "m", template, new ArrayList<>());

        String[] names = {"none", "clockwise_90"};
        Object[] rotations = {C("dmm").getField("a").get(null), C("dmm").getField("b").get(null)};
        for (int r = 0; r < names.length; r++) {
            Object chunk = chunk(0, 0, worldgen);
            Object target = C("jd").getConstructor(int.class, int.class, int.class).newInstance(10, 80, 10);
            Object entity = blockEntityFor(chestState, target);
            chunk.getClass().getMethod("a", C("dqh")).invoke(chunk, entity);
            NativeHarness.Level level = NativeHarness.levelOnChunk(
                    (x, y, z) -> NativeHarness.state("minecraft:air"), chunk, provider);
            Object settings = make(C("enp"), new Class<?>[]{});
            C("enp").getMethod("a", C("dmm")).invoke(settings, rotations[r]);
            Object random = C("ayw").getMethod("a", long.class).invoke(null, 42L);
            boolean placed = (boolean) C("ent")
                    .getMethod("a", C("ddl"), C("jd"), C("jd"), C("enp"), C("ayw"), int.class)
                    .invoke(template, level.proxy, target, target, settings, random, 2);
            List<String> cells = new ArrayList<>(level.writes.keySet());
            Collections.sort(cells);
            for (String cell : cells) {
                String[] xyz = cell.split(",");
                Object pos = C("jd").getConstructor(int.class, int.class, int.class)
                        .newInstance(Integer.parseInt(xyz[0]), Integer.parseInt(xyz[1]), Integer.parseInt(xyz[2]));
                Object be = chunk.getClass().getMethod("c_", C("jd")).invoke(chunk, pos);
                String nbtHash = "absent";
                if (be != null) {
                    Object out = be.getClass().getMethod("b", C("jo$a")).invoke(be, provider);
                    nbtHash = StructureTemplateOracle.hash(canonicalBytes(out));
                }
                System.out.println("INIT\t" + names[r] + "\t" + placed + "\t" + cell + "\t"
                        + NativeHarness.normalizedStateName(level.writes.get(cell)) + "\t" + nbtHash);
            }
        }
    }

    static byte[] canonicalBytes(Object tag) throws Exception {
        ByteArrayOutputStream buffer = new ByteArrayOutputStream();
        DataOutputStream out = new DataOutputStream(buffer);
        StructureTemplateOracle.named(tag, out);
        out.flush();
        return buffer.toByteArray();
    }
}
