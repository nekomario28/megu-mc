package dev.megu.tools;
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
        Object chunk0 = chunk(0, 0, worldgen);

        // Two tags, built from real entities so the encoding is the game's own:
        // a loot-table chest and a chest with explicit stacks. They must be separate
        // cells: a container that still holds a loot table saves the table INSTEAD of
        // its items (`RandomizableContainerBlockEntity.saveAdditional`), so one cell
        // cannot show both.
        Object lootTag = savedTag(chunk0, chestState, 4, 80, 4, provider);
        C("ub").getMethod("a", String.class, String.class).invoke(lootTag, "LootTable", "minecraft:chests/simple_dungeon");
        C("ub").getMethod("a", String.class, String.class).invoke(lootTag, "CustomName", "{\"text\":\"Crate\"}");
        Object itemsTag = savedTag(chunk0, chestState, 6, 80, 4, provider);

        Object itemsInfo = make(C("ent$c"), new Class<?>[]{C("jd"), C("dtc"), C("ub")},
                pos(0, 0, 0), chestState, itemsTag);
        Object chestInfo = make(C("ent$c"), new Class<?>[]{C("jd"), C("dtc"), C("ub")},
                pos(0, 0, 1), chestState, lootTag);
        // The sign stands on stone, because a standing sign without support is removed by
        // the shape pass. Its text comes from the game's own SignText codec: a hand-written
        // tag without `filtered_messages` and with a text object loads as empty lines.
        Object stoneInfo = make(C("ent$c"), new Class<?>[]{C("jd"), C("dtc"), C("ub")},
                pos(1, 0, 1), stoneState, null);
        Object signState = NativeHarness.state("oak_sign");
        Object signTag = make(C("ub"), new Class<?>[]{});
        C("ub").getMethod("a", String.class, C("uy")).invoke(signTag, "front_text", signTextOf(provider, "Hi"));
        C("ub").getMethod("a", String.class, byte.class).invoke(signTag, "is_waxed", (byte) 0);
        Object signInfo = make(C("ent$c"), new Class<?>[]{C("jd"), C("dtc"), C("ub")},
                pos(1, 1, 1), signState, signTag);
        Object palette = make(C("ent$a"), new Class<?>[]{List.class},
                List.of(chestInfo, itemsInfo, stoneInfo, signInfo));
        Object template = make(C("ent"), new Class<?>[]{});
        fset(C("ent"), "l", template, List.of(palette));
        fset(C("ent"), "n", template, make(C("kh"), new Class<?>[]{int.class, int.class, int.class}, 2, 2, 2));
        fset(C("ent"), "m", template, new ArrayList<>());

        String[] names = {"none", "clockwise_90"};
        Object[] rotations = {C("dmm").getField("a").get(null), C("dmm").getField("b").get(null)};
        for (int r = 0; r < names.length; r++) {
            Object chunk = chunk(0, 0, worldgen);
            Object target = pos(10, 80, 10);
            // Both chests need an entity where the rotation puts them: a ProtoChunk does
            // not create one. Offsets rotate with the template: (0,0,1) -> (-1,0,0) at 90.
            Object[][] beCells = {
                {0, 0, 0, chestState}, {0, 0, 1, chestState}, {1, 1, 1, signState}};
            for (Object[] cell : beCells) {
                int ox = (int) cell[0], oy = (int) cell[1], oz = (int) cell[2];
                int rx = r == 0 ? ox : -oz;
                int rz = r == 0 ? oz : ox;
                Object entity = blockEntityFor(cell[3], pos(10 + rx, 80 + oy, 10 + rz));
                chunk.getClass().getMethod("a", C("dqh")).invoke(chunk, entity);
            }
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
                Object at = pos(Integer.parseInt(xyz[0]), Integer.parseInt(xyz[1]), Integer.parseInt(xyz[2]));
                Object be = chunk.getClass().getMethod("c_", C("jd")).invoke(chunk, at);
                String nbtHash = "absent";
                String contents = "-";
                String text = "-";
                if (be != null) {
                    Object out = be.getClass().getMethod("b", C("jo$a")).invoke(be, provider);
                    nbtHash = StructureTemplateOracle.hash(canonicalBytes(out));
                    contents = contentsOf(be);
                    text = signText(out);
                }
                System.out.println("INIT\t" + names[r] + "\t" + placed + "\t" + cell + "\t"
                        + NativeHarness.normalizedStateName(level.writes.get(cell)) + "\t" + nbtHash
                        + "\t" + contents + "\t" + text);
            }
        }
    }

    /** The front side the game itself encodes for four literal components. */
    static Object signTextOf(Object worldgen, String first) throws Exception {
        Object ops = C("jo$a").getMethod("a", Class.forName("com.mojang.serialization.DynamicOps"))
                .invoke(worldgen, C("up").getField("a").get(null));
        Object front = java.lang.reflect.Array.newInstance(C("wz"), 4);
        Object back = java.lang.reflect.Array.newInstance(C("wz"), 4);
        for (int i = 0; i < 4; i++) {
            java.lang.reflect.Array.set(front, i,
                    C("wz").getMethod("a", String.class).invoke(null, i == 0 ? first : ""));
            java.lang.reflect.Array.set(back, i, C("wz").getMethod("a", String.class).invoke(null, ""));
        }
        Object text = make(C("drt"), new Class<?>[]{Class.forName("[Lwz;"), Class.forName("[Lwz;"),
                C("cti"), boolean.class}, front, back, C("cti").getField("a").get(null), false);
        Object codec = C("drt").getField("a").get(null);
        Object encoded = Class.forName("com.mojang.serialization.Codec")
                .getMethod("encodeStart", Class.forName("com.mojang.serialization.DynamicOps"), Object.class)
                .invoke(codec, ops, text);
        return encoded.getClass().getMethod("getOrThrow").invoke(encoded);
    }

    /** Front-side lines as native stored them, joined with `|`; `-` when not a sign. */
    static String signText(Object tag) throws Exception {
        Object front = C("ub").getMethod("c", String.class).invoke(tag, "front_text");
        if (front == null) return "-";
        Object messages = C("ub").getMethod("c", String.class).invoke(front, "messages");
        if (messages == null) return "-";
        int n = ((Number) C("uh").getMethod("size").invoke(messages)).intValue();
        StringBuilder out = new StringBuilder();
        for (int i = 0; i < n; i++) {
            if (i > 0) out.append('|');
            // The stored string, not Java's `'...'` rendering of the tag, so the fixture
            // carries exactly what native holds.
            String text = String.valueOf(C("uh").getMethod("k", int.class).invoke(messages, i));
            if (text.startsWith("'") && text.endsWith("'") && text.length() >= 2) {
                text = text.substring(1, text.length() - 1).replace("\\'", "'").replace("\\\\", "\\");
            }
            out.append(text);
        }
        return out.toString();
    }

    static Object pos(int x, int y, int z) throws Exception {
        return C("jd").getConstructor(int.class, int.class, int.class).newInstance(x, y, z);
    }

    /** A chest tag produced by a real entity: one redstone at slot 0, one diamond at slot 2. */
    static Object savedTag(Object chunk, Object chestState, int x, int y, int z, Object provider) throws Exception {
        Object at = pos(x, y, z);
        Object entity = blockEntityFor(chestState, at);
        chunk.getClass().getMethod("a", C("dqh")).invoke(chunk, entity);
        chunk.getClass().getMethod("a", C("jd"), C("dtc"), boolean.class).invoke(chunk, at, chestState, false);
        setStack(entity, 0, "minecraft:redstone", 3);
        setStack(entity, 2, "minecraft:diamond", 1);
        return entity.getClass().getMethod("b", C("jo$a")).invoke(entity, provider);
    }

    static void setStack(Object entity, int slot, String id, int count) throws Exception {
        Object item = C("lt").getField("g").get(null);
        Object rl = C("akr").getMethod("a", String.class).invoke(null, id);
        Object value = item.getClass().getMethod("a", C("akr")).invoke(item, rl);
        Object stack = C("cuq").getDeclaredConstructor(C("dcv"), int.class).newInstance(value, count);
        for (Method m : entity.getClass().getMethods()) {
            if (m.getName().equals("a") && m.getParameterCount() == 2
                    && m.getParameterTypes()[0] == int.class && m.getParameterTypes()[1] == C("cuq")) {
                m.invoke(entity, slot, stack);
                return;
            }
        }
        throw new AssertionError("no setItem on the container");
    }

    /** Contents read through the live container, so the fixture is semantic, not a blob. */
    static String contentsOf(Object entity) throws Exception {
        StringBuilder out = new StringBuilder();
        for (Method m : entity.getClass().getMethods()) {
            if (m.getName().equals("a") && m.getParameterCount() == 1
                    && m.getParameterTypes()[0] == int.class
                    && m.getReturnType().getSimpleName().equals("cuq")) {
                for (int slot = 0; slot < 9; slot++) {
                    Object stack = m.invoke(entity, slot);
                    String text = String.valueOf(stack);
                    // An empty stack renders as "0 minecraft:air"; it is not a stored item.
                    if (text.startsWith("0 ")) continue;
                    if (out.length() > 0) out.append(';');
                    out.append(slot).append('=').append(text);
                }
                return out.length() == 0 ? "-" : out.toString();
            }
        }
        return "-";
    }

    static byte[] canonicalBytes(Object tag) throws Exception {
        ByteArrayOutputStream buffer = new ByteArrayOutputStream();
        DataOutputStream out = new DataOutputStream(buffer);
        StructureTemplateOracle.named(tag, out);
        out.flush();
        return buffer.toByteArray();
    }
}
