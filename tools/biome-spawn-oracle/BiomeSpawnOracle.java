// Dump the pinned 1.21.1 biome MobSpawnSettings through their real codecs.
// No server/world; ordinary RegistryDataLoader and Biome methods.
package dev.megu.tools;

import com.google.gson.*;
import java.lang.reflect.*;
import java.nio.file.*;
import java.util.*;

public final class BiomeSpawnOracle {
    static Class<?> cls(String n) throws Exception { return Class.forName(n); }
    static Object field(Class<?> c, String n) throws Exception {
        Field f = c.getDeclaredField(n); f.setAccessible(true); return f.get(null);
    }
    static Object instanceField(Object o, String n) throws Exception {
        Field f = o.getClass().getDeclaredField(n); f.setAccessible(true); return f.get(o);
    }
    static String keyName(Object key) throws Exception {
        return key.getClass().getMethod("a").invoke(key).toString();
    }
    public static void main(String[] args) throws Exception {
        cls("ab").getMethod("a").invoke(null);
        cls("akt").getMethod("a").invoke(null);
        Object builtin = field(cls("lt"), "aA");
        Object access = cls("ka").getMethod("a", cls("jz")).invoke(null, builtin);
        Object pack = cls("ats").getMethod("b").invoke(null);
        Object manager = cls("atx").getConstructor(cls("ass"), List.class)
            .newInstance(field(cls("ass"), "b"), List.of(pack));
        access = cls("akm").getMethod("a", cls("aue"), cls("ka"), List.class)
            .invoke(null, manager, access, field(cls("akm"), "a"));
        Object biomes = access.getClass().getMethod("d", cls("akq")).invoke(access, field(cls("lu"), "aF"));
        Object entities = field(cls("lt"), "f");
        Class<?> category = cls("btq");
        Class<?> spawnerData = cls("dei$c");
        Class<?> costData = cls("dei$b");
        Class<?> weighted = cls("bpd");
        JsonObject out = new JsonObject();
        for (Object entry : (Set<?>) biomes.getClass().getMethod("h").invoke(biomes)) {
            Map.Entry<?, ?> e = (Map.Entry<?, ?>) entry;
            String biomeName = keyName(e.getKey());
            Object mob = e.getValue().getClass().getMethod("b").invoke(e.getValue());
            JsonObject s = new JsonObject();
            for (Object cat : category.getEnumConstants()) {
                String catName = (String) cat.getClass().getMethod("c").invoke(cat);
                Object list = mob.getClass().getMethod("a", category).invoke(mob, cat);
                JsonArray arr = new JsonArray();
                for (Object data : (Iterable<?>) list.getClass().getMethod("e").invoke(list)) {
                    String type = entities.getClass().getMethod("b", Object.class)
                        .invoke(entities, spawnerData.getField("b").get(data)).toString();
                    int min = spawnerData.getField("c").getInt(data);
                    int max = spawnerData.getField("d").getInt(data);
                    Object weightObject = data.getClass().getMethod("a").invoke(data);
                    int weight = (Integer) weightObject.getClass().getMethod("a").invoke(weightObject);
                    JsonObject o = new JsonObject();
                    o.addProperty("type", type); o.addProperty("min", min);
                    o.addProperty("max", max); o.addProperty("weight", weight);
                    arr.add(o);
                }
                s.add(catName, arr);
            }
            Object costs = instanceField(mob, "h");
            JsonObject c = new JsonObject();
            for (Map.Entry<?, ?> cost : ((Map<?, ?>) costs).entrySet()) {
                String type = entities.getClass().getMethod("b", Object.class).invoke(entities, cost.getKey()).toString();
                Object v = cost.getValue();
                JsonObject o = new JsonObject();
                o.addProperty("energy_budget", (Double) v.getClass().getMethod("a").invoke(v));
                o.addProperty("charge", (Double) v.getClass().getMethod("b").invoke(v));
                c.add(type, o);
            }
            JsonObject row = new JsonObject();
            row.add("spawners", s);
            row.add("spawn_costs", c);
            row.addProperty("creature_generation_probability",
                (Number) mob.getClass().getMethod("a").invoke(mob));
            out.add(biomeName, row);
        }
        Files.writeString(Path.of(args[0]), new GsonBuilder().serializeNulls().create().toJson(out));
    }
}
