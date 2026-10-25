package dev.mcspine.tools;

import com.google.gson.*;
import java.lang.reflect.*;
import java.nio.file.*;
import java.util.*;

/** Actual 1.21.1 Slot.safeInsert and ChestMenu.quickMoveStack, not a second model.
 * Reflection names below are resolved from Mojang's 1.21.1 server mappings.
 * No stand-in Player, Unsafe allocation, server startup, or signature stripping.
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
            // All generated patches here contain only MAX_STACK_SIZE.
            int max = (int) stack.getMethod("j").invoke(value);
            int natural = switch (entry.getKey()) {
                case "snowball" -> 16;
                case "diamond_sword" -> 1;
                default -> 64;
            };
            if (max != natural) result.addProperty("cap", max);
            return result;
        }
        throw new IllegalStateException("unrecognized reference item");
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
            } else throw new IllegalArgumentException("unknown operation");
            row.add("expected", result);
            output.add(row);
        }
        if (output.isEmpty()) throw new IllegalArgumentException("empty reference cases");
        Files.writeString(Path.of(args[1]), gson.toJson(output) + "\n");
    }
}
