package dev.megu.tools;

import java.lang.reflect.*;
import java.nio.file.*;
import java.io.*;
import com.google.gson.*;

/** Actual native spawner and chest-minecart constructors and NBT writers.
 * No server/world tick or loot unpacking is performed. */
public final class MineshaftDataOracle {
    static Class<?> c(String n) { return NativeHarness.C(n); }
    static void write(Object tag, Path path) throws Exception {
        try (var out = new DataOutputStream(Files.newOutputStream(path))) {
            c("uo").getMethod("a", c("ub"), DataOutput.class).invoke(null, tag, out);
        }
    }
    public static void main(String[] args) throws Exception {
        NativeHarness.bootstrap();
        Path dir = Path.of(args[0]); Files.createDirectories(dir);
        Object access = c("ka").getMethod("a", c("jz")).invoke(null, c("lt").getField("aA").get(null));
        Object at = NativeHarness.pos(7, 24, -19);
        Object spawner = c("drw").getConstructor(c("jd"), c("dtc")).newInstance(at, NativeHarness.state("minecraft:spawner"));
        Object random = c("dzz").getConstructor(long.class).newInstance(123L);
        JsonArray before = state(random);
        c("drw").getMethod("a", c("bsx"), c("ayw")).invoke(spawner, c("bsx").getField("q").get(null), random);
        Object tag = c("dqh").getMethod("b", c("jo$a")).invoke(spawner, access);
        write(tag, dir.resolve("spawner.nbt"));
        JsonObject result = new JsonObject(); result.add("before", before); result.add("after", state(random));
        Object type = c("dqh").getMethod("r").invoke(spawner);
        result.addProperty("spawner_packet_type_id", (int)c("jr").getMethod("a", Object.class).invoke(c("lt").getField("j").get(null), type));
        Object cart = c("cpa").getConstructor(c("dcw"), double.class, double.class, double.class).newInstance(null, 7.5, 24.5, -18.5);
        c("cou").getMethod("a", c("akq"), long.class).invoke(cart, c("ern").getField("u").get(null), -9223372036854775807L);
        Object data = c("ub").getConstructor().newInstance();
        // cou.b delegates to this exact ContainerEntity default method, but
        // first obtains registryAccess from a Level. Supply the same provider
        // explicitly; the null-Level probe does not claim full entity saving.
        c("cox").getMethod("a", c("ub"), c("jo$a")).invoke(cart, data, access);
        write(data, dir.resolve("chest-minecart.nbt"));
        Files.writeString(dir.resolve("rng.json"), new GsonBuilder().setPrettyPrinting().create().toJson(result)+"\n");
        System.out.println("native mineshaft entity data written");
    }
    static JsonArray state(Object random) throws Exception {
        Field core = c("dzz").getDeclaredField("e"); core.setAccessible(true); Object bits = core.get(random);
        JsonArray out = new JsonArray();
        for (String n : new String[]{"b", "c"}) { Field f = c("dzy").getDeclaredField(n); f.setAccessible(true); out.add(Long.toString(f.getLong(bits))); }
        return out;
    }
}
