// Call the pinned 1.21.1 NoiseChunk/Aquifer through their ordinary public API.
// No running server, world, or constructor bypass. See tools/aquifer_oracle.py.
package dev.mcspine.tools;

import com.google.gson.*;
import java.lang.reflect.*;
import java.nio.file.*;
import java.util.*;

public final class AquiferOracle {
    static Class<?> cls(String name) throws Exception { return Class.forName(name); }
    static Object field(Class<?> owner, String name) throws Exception {
        Field f = owner.getDeclaredField(name);
        f.setAccessible(true);
        return f.get(null);
    }
    static Object call(Object target, String name, Class<?>[] types, Object... args) throws Exception {
        return target.getClass().getMethod(name, types).invoke(target, args);
    }
    static String stateName(Object state) throws Exception {
        // BlockState.getBlock() -> e(); Block.getName? Block.getName() is b()? use toString: Block{...}
        return state.toString();
    }

    /** The overworld fluid picker from NoiseBasedChunkGenerator.createFluidPicker. */
    static Object picker(int sea) throws Exception {
        Class<?> api = cls("dyk$a");
        Class<?> status = cls("dyk$b");
        Class<?> blockState = cls("dtc");
        Object lavaBlock = cls("dga").getField("H").get(null);
        Object waterBlock = cls("dga").getField("G").get(null);
        Object lava = status.getConstructor(int.class, blockState).newInstance(-54,
            lavaBlock.getClass().getMethod("o").invoke(lavaBlock));
        Object water = status.getConstructor(int.class, blockState).newInstance(sea,
            waterBlock.getClass().getMethod("o").invoke(waterBlock));
        return Proxy.newProxyInstance(api.getClassLoader(), new Class<?>[]{api}, (proxy, method, args) -> {
            int y = (Integer) args[1];
            return y < Math.min(-54, sea) ? lava : water;
        });
    }

    public static void main(String[] args) throws Exception {
        JsonObject req = JsonParser.parseString(Files.readString(Path.of(args[0]))).getAsJsonObject();
        long seed = req.get("seed").getAsLong();
        int cx = req.getAsJsonArray("chunk").get(0).getAsInt();
        int cz = req.getAsJsonArray("chunk").get(1).getAsInt();

        // SharedConstants.tryDetectVersion() / Bootstrap.bootStrap()
        cls("ab").getMethod("a").invoke(null);
        cls("akt").getMethod("a").invoke(null);

        Object builtin = field(cls("lt"), "aA");                       // BuiltInRegistries.REGISTRY
        Object access = cls("ka").getMethod("a", cls("jz")).invoke(null, builtin); // RegistryAccess
        // The vanilla datapack's own worldgen JSON through the actual loader and codecs.
        Object pack = cls("ats").getMethod("b").invoke(null);          // ServerPacksSource.createVanillaPackSource()
        Object manager = cls("atx").getConstructor(cls("ass"), List.class)
            .newInstance(field(cls("ass"), "b"), List.of(pack));        // PackType.SERVER_DATA
        access = cls("akm").getMethod("a", cls("aue"), cls("ka"), List.class)
            .invoke(null, manager, access, field(cls("akm"), "a"));    // WORLDGEN_REGISTRIES
        Object settingsKey = field(cls("dzd"), "c");                    // NoiseGeneratorSettings.OVERWORLD
        Object settingsRegistryKey = settingsKey.getClass().getMethod("c").invoke(settingsKey);
        Object settingsRegistry = access.getClass().getMethod("d", cls("akq")).invoke(access, settingsRegistryKey);
        Object settings = settingsRegistry.getClass().getMethod("f", cls("akq")).invoke(settingsRegistry, settingsKey);
        Object noiseRegistryKey = field(cls("lu"), "aP");
        Object noiseRegistry = access.getClass().getMethod("d", cls("akq")).invoke(access, noiseRegistryKey);
        Object noiseGetter = noiseRegistry.getClass().getMethod("q").invoke(noiseRegistry);
        Object randomState = cls("dzm").getMethod("a", cls("dzd"), cls("jn"), long.class)
            .invoke(null, settings, noiseGetter, seed);

        Object seaObj = settings.getClass().getMethod("l").invoke(settings);
        int sea = (Integer) seaObj;
        Object picker = picker(sea);

        Object chunkPos = cls("dcd").getConstructor(int.class, int.class).newInstance(cx, cz);
        Object height = cls("dcy").getMethod("e", int.class, int.class).invoke(null, -64, 384);
        Object upgrade = field(cls("dvv"), "a");
        Object biomeKey = field(cls("lu"), "aF");
        Object biomeRegistry = access.getClass().getMethod("d", cls("akq")).invoke(access, biomeKey);
        Object proto = cls("dvs").getConstructor(cls("dcd"), cls("dvv"), cls("dcy"), cls("jz"), cls("eab"))
            .newInstance(chunkPos, upgrade, height, biomeRegistry, null);
        Object marker = field(cls("dys$b"), "a");                     // BeardifierMarker.INSTANCE
        Object blender = cls("eaa").getMethod("a").invoke(null);         // Blender.empty()
        Object noiseChunk = cls("dzc").getMethod("a", cls("duy"), cls("dzm"), cls("dys$c"),
            cls("dzd"), cls("dyk$a"), cls("eaa")).invoke(null, proto, randomState, marker, settings, picker, blender);
        Object aquifer = noiseChunk.getClass().getMethod("i").invoke(noiseChunk);

        JsonObject out = new JsonObject();
        JsonArray prelim = new JsonArray();
        for (JsonElement e : req.getAsJsonArray("prelim")) {
            JsonArray p = e.getAsJsonArray();
            int x = p.get(0).getAsInt(), z = p.get(1).getAsInt();
            int level = (Integer) noiseChunk.getClass().getMethod("a", int.class, int.class).invoke(noiseChunk, x, z);
            JsonArray row = new JsonArray();
            row.add(x); row.add(z); row.add(level);
            prelim.add(row);
        }
        out.add("prelim", prelim);

        JsonArray states = new JsonArray();
        for (JsonElement e : req.getAsJsonArray("state")) {
            JsonArray p = e.getAsJsonArray();
            int x = p.get(0).getAsInt(), y = p.get(1).getAsInt(), z = p.get(2).getAsInt();
            double density = p.get(3).getAsDouble();
            Object ctx = cls("dyr$e").getConstructor(int.class, int.class, int.class).newInstance(x, y, z);
            Object state = aquifer.getClass().getMethod("a", cls("dyr$b"), double.class).invoke(aquifer, ctx, density);
            JsonArray row = new JsonArray();
            row.add(x); row.add(y); row.add(z); row.add(density);
            row.add(state == null ? JsonNull.INSTANCE : new JsonPrimitive(state.toString()));
            states.add(row);
        }
        out.add("state", states);

        JsonArray density = new JsonArray();
        for (JsonElement e : req.getAsJsonArray("density")) {
            JsonArray p = e.getAsJsonArray();
            int x = p.get(0).getAsInt(), y = p.get(1).getAsInt(), z = p.get(2).getAsInt();
            Object ctx = cls("dyr$e").getConstructor(int.class, int.class, int.class).newInstance(x, y, z);
            Object idwj = randomState.getClass().getMethod("a").invoke(randomState).getClass()
                .getMethod("k").invoke(randomState.getClass().getMethod("a").invoke(randomState));
            double v = (Double) idwj.getClass().getMethod("a", cls("dyr$b")).invoke(idwj, ctx);
            JsonArray row = new JsonArray();
            row.add(x); row.add(y); row.add(z);
            row.add(String.format("%016x", Double.doubleToRawLongBits(v)));
            density.add(row);
        }
        out.add("density", density);
        Files.writeString(Path.of(args[1]), new GsonBuilder().serializeNulls().create().toJson(out));
    }
}
