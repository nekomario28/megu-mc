package dev.mcspine.tools;

import com.google.gson.*;
import java.lang.reflect.*;
import java.nio.file.*;
import java.util.*;

/** Actual 1.21.1 slot/menu and Inventory insertion, not a second model.
 * Reflection names below are resolved from Mojang's 1.21.1 server mappings.
 * Slot/menu probes use no Player. Inventory probes use a constructor-bypassed real
 * ServerPlayer ONLY for hasInfiniteMaterials (native abilities.instabuild read).
 * This does not validate player construction, ItemEntity callbacks, or a world.
 * No overridden game methods, server startup, or signature stripping.
 * Harness approach informed by Horizon's VanillaMenuOracle (MIT notice in repo).
 */
public final class ContainerOracle {
    static Class<?> stack, container, slot, inventory, menu;
    static Object empty;
    static final Map<String, Object> items = new LinkedHashMap<>();
    static final Gson gson = new GsonBuilder().serializeNulls().create();

    static Object make(JsonElement value) throws Exception {
        if (value == null || value.isJsonNull()) return empty;
        JsonObject spec = value.getAsJsonObject();
        Object result = stack.getConstructor(Class.forName("dcv"), int.class).newInstance(
                items.get(spec.get("item").getAsString()), spec.get("count").getAsInt());
        if (spec.has("cap")) {
            Field type = Class.forName("kq").getField("c"); // DataComponents.MAX_STACK_SIZE
            stack.getMethod("b", type.getType(), Object.class).invoke(
                    result, type.get(null), Integer.valueOf(spec.get("cap").getAsInt()));
        }
        if (spec.has("damage")) {
            Field type = Class.forName("kq").getField("e"); // DataComponents.DAMAGE
            stack.getMethod("b", type.getType(), Object.class).invoke(
                    result, type.get(null), Integer.valueOf(spec.get("damage").getAsInt()));
        }
        if (spec.has("unbreakable")) {
            Field type = Class.forName("kq").getField("f"); // DataComponents.UNBREAKABLE
            Object patch = Class.forName("cxz").getConstructor(boolean.class).newInstance(true);
            stack.getMethod("b", type.getType(), Object.class).invoke(result, type.get(null), patch);
        }
        return result;
    }

    static JsonElement describe(Object value) throws Exception {
        int count = (int) stack.getMethod("H").invoke(value);
        if (count == 0 || (boolean) stack.getMethod("e").invoke(value)) return JsonNull.INSTANCE;
        Object item = stack.getMethod("g").invoke(value);
        for (var entry : items.entrySet()) {
            if (entry.getValue() != item) continue;
            JsonObject result = new JsonObject();
            result.addProperty("item", entry.getKey());
            result.addProperty("count", count);
            // Describe the supported effective patches, not arbitrary component data.
            int max = (int) stack.getMethod("j").invoke(value);
            int natural = switch (entry.getKey()) {
                case "snowball" -> 16;
                case "diamond_sword" -> 1;
                default -> 64;
            };
            if (max != natural) result.addProperty("cap", max);
            int damage = (int) stack.getMethod("n").invoke(value); // getDamageValue
            if (damage > 0) result.addProperty("damage", damage);
            Field unbreakable = Class.forName("kq").getField("f");
            if (stack.getMethod("a", unbreakable.getType()).invoke(value, unbreakable.get(null)) != null)
                result.addProperty("unbreakable", true);
            return result;
        }
        throw new IllegalStateException("unrecognized reference item");
    }

    static Object inventoryPlayer(boolean infinite) throws Exception {
        Class<?> player = Class.forName("cmx");
        // In this authenticated build Player.hasInfiniteMaterials reads exactly
        // Player.abilities.instabuild; no level, connection or gameMode is touched.
        if (player.getMethod("fL").getDeclaringClass() != player)
            throw new IllegalStateException("unexpected infinite-materials owner");
        Class<?> unsafeClass = Class.forName("sun.misc.Unsafe");
        Field singleton = unsafeClass.getDeclaredField("theUnsafe");
        singleton.setAccessible(true);
        Object p = unsafeClass.getMethod("allocateInstance", Class.class).invoke(
                singleton.get(null), Class.forName("aqv")); // real ServerPlayer, no overrides
        Object abilities = Class.forName("cmu").getConstructor().newInstance();
        abilities.getClass().getField("d").setBoolean(abilities, infinite);
        Field field = player.getDeclaredField("cA");
        field.setAccessible(true);
        field.set(p, abilities);
        if ((boolean) player.getMethod("fL").invoke(p) != infinite)
            throw new IllegalStateException("native infinite-materials callback disagrees");
        return p;
    }

    public static void main(String[] args) throws Exception {
        Class.forName("ab").getMethod("a").invoke(null); // SharedConstants.tryDetectVersion
        Class.forName("akt").getMethod("a").invoke(null); // Bootstrap.bootStrap
        stack = Class.forName("cuq");
        container = Class.forName("bra");
        slot = Class.forName("crq");
        inventory = Class.forName("cmw");
        menu = Class.forName("cqc");
        empty = stack.getField("l").get(null);
        for (var e : Map.of("stone", "b", "dirt", "C", "snowball", "qC", "diamond_sword", "pg").entrySet())
            items.put(e.getKey(), Class.forName("cut").getField(e.getValue()).get(null));
        JsonArray input = JsonParser.parseString(Files.readString(Path.of(args[0]))).getAsJsonArray();
        JsonArray output = new JsonArray();
        for (JsonElement element : input) {
            JsonObject spec = element.getAsJsonObject();
            JsonObject row = spec.deepCopy(), result = new JsonObject();
            if (spec.get("op").getAsString().equals("insert")) {
                Object storage = container.getConstructor(int.class).newInstance(1);
                container.getMethod("a", int.class, stack).invoke(storage, 0, make(spec.get("target")));
                Object s = slot.getConstructor(Class.forName("bqk"), int.class, int.class, int.class)
                        .newInstance(storage, 0, 0, 0);
                Object remaining = slot.getMethod("b", stack, int.class).invoke(
                        s, make(spec.get("source")), spec.get("amount").getAsInt());
                result.add("target", describe(slot.getMethod("g").invoke(s)));
                result.add("source", describe(remaining));
            } else if (spec.get("op").getAsString().equals("quick_move")) {
                int rows = spec.get("rows").getAsInt();
                Object inv = inventory.getConstructor(Class.forName("cmx")).newInstance((Object) null);
                Object storage = container.getConstructor(int.class).newInstance(rows * 9);
                // ChestMenu.threeRows / sixRows. These real menus create all slot mappings.
                Object m = menu.getMethod(rows == 3 ? "a" : "b", int.class, inventory, Class.forName("bqk"))
                        .invoke(null, 1, inv, storage);
                List<?> slots = (List<?>) Class.forName("cpu").getField("i").get(m);
                for (var entry : spec.getAsJsonObject("slots").entrySet())
                    slot.getMethod("e", stack).invoke(slots.get(Integer.parseInt(entry.getKey())), make(entry.getValue()));
                menu.getMethod("b", Class.forName("cmx"), int.class).invoke(m, null, spec.get("clicked").getAsInt());
                JsonObject states = new JsonObject();
                for (int i = 0; i < slots.size(); i++) {
                    JsonElement state = describe(slot.getMethod("g").invoke(slots.get(i)));
                    if (!state.isJsonNull()) states.add(Integer.toString(i), state);
                }
                result.add("slots", states);
            } else if (spec.get("op").getAsString().equals("inventory_add")) {
                Object inv = inventory.getConstructor(Class.forName("cmx")).newInstance(
                        inventoryPlayer(spec.get("creative").getAsBoolean()));
                inventory.getField("k").setInt(inv, spec.get("selected").getAsInt());
                for (var entry : spec.getAsJsonObject("slots").entrySet())
                    inventory.getMethod("a", int.class, stack).invoke(
                            inv, Integer.parseInt(entry.getKey()), make(entry.getValue()));
                Object source = make(spec.get("source"));
                result.addProperty("first_space", (int) inventory.getMethod("e", stack).invoke(inv, source));
                result.addProperty("first_empty", (int) inventory.getMethod("h").invoke(inv));
                result.addProperty("accepted", (boolean) inventory.getMethod("f", stack).invoke(inv, source));
                result.add("source", describe(source));
                JsonObject states = new JsonObject();
                for (int i = 0; i < 41; i++) {
                    JsonElement state = describe(inventory.getMethod("a", int.class).invoke(inv, i));
                    if (!state.isJsonNull()) states.add(Integer.toString(i), state);
                }
                result.add("slots", states);
            } else throw new IllegalArgumentException("unknown operation");
            row.add("expected", result);
            output.add(row);
        }
        if (output.isEmpty()) throw new IllegalArgumentException("empty reference cases");
        Files.writeString(Path.of(args[1]), gson.toJson(output) + "\n");
    }
}
