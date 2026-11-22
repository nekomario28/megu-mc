// Actual pinned 1.21.1 biome carver lists from the jar's own datapack.
// Ordinary RegistryDataLoader/Biome methods; no server, world or constructor bypass.
package dev.megu.tools;

import com.google.gson.*;
import java.lang.reflect.*;
import java.nio.file.*;
import java.util.*;

public final class BiomeCarverOracle {
    static Class<?> cls(String name) throws Exception { return Class.forName(name); }
    static Object field(Class<?> owner, String name) throws Exception {
        Field f = owner.getDeclaredField(name);
        f.setAccessible(true);
        return f.get(null);
    }
    static String id(Object key) throws Exception {
        return key.getClass().getMethod("a").invoke(key).toString();
    }
    public static void main(String[] args) throws Exception {
        // SharedConstants.tryDetectVersion / Bootstrap.bootStrap
        cls("ab").getMethod("a").invoke(null);
        cls("akt").getMethod("a").invoke(null);
        Object builtin = field(cls("lt"), "aA");
        Object access = cls("ka").getMethod("a", cls("jz")).invoke(null, builtin);
        Object pack = cls("ats").getMethod("b").invoke(null);
        Object manager = cls("atx").getConstructor(cls("ass"), List.class)
            .newInstance(field(cls("ass"), "b"), List.of(pack));
        access = cls("akm").getMethod("a", cls("aue"), cls("ka"), List.class)
            .invoke(null, manager, access, field(cls("akm"), "a"));
        Object registry = access.getClass().getMethod("d", cls("akq")).invoke(access, field(cls("lu"), "aF"));
        Object air = field(cls("dyu$a"), "a");
        JsonObject out = new JsonObject();
        Set<?> entries = (Set<?>) registry.getClass().getMethod("h").invoke(registry);
        for (Object entry : entries) {
            Object key = ((java.util.Map.Entry<?, ?>) entry).getKey();
            Object biome = ((java.util.Map.Entry<?, ?>) entry).getValue();
            Object generation = biome.getClass().getMethod("d").invoke(biome);
            Iterable<?> carvers = (Iterable<?>) generation.getClass()
                .getMethod("a", cls("dyu$a")).invoke(generation, air);
            JsonArray list = new JsonArray();
            for (Object holder : carvers) {
                Optional<?> optional = (Optional<?>) holder.getClass().getMethod("e").invoke(holder);
                list.add(id(optional.orElseThrow()));
            }
            out.add(id(key), list);
        }
        Files.writeString(Path.of(args[0]), new GsonBuilder().serializeNulls().create().toJson(out));
    }
}
