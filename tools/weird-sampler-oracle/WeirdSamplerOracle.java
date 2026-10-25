package dev.mcspine.tools;

import com.google.gson.*;
import it.unimi.dsi.fastutil.doubles.Double2DoubleFunction;
import java.lang.reflect.*;
import java.nio.file.*;
import java.util.*;

/** Actual pinned WeirdScaledSampler and name-seeded NormalNoise; no running world. */
public final class WeirdSamplerOracle {
    static String bits(double value) {
        return String.format(Locale.ROOT, "%016x", Double.doubleToRawLongBits(value));
    }

    static Object field(Class<?> owner, String name, Object target) throws Exception {
        Field field = owner.getDeclaredField(name);
        field.setAccessible(true);
        return field.get(target);
    }

    public static void main(String[] args) throws Exception {
        Class.forName("ab").getMethod("a").invoke(null); // SharedConstants.tryDetectVersion
        Class.forName("akt").getMethod("a").invoke(null); // Bootstrap.bootStrap
        JsonObject input = JsonParser.parseString(Files.readString(Path.of(args[0]))).getAsJsonObject();
        Class<?> density = Class.forName("dyr");
        Class<?> context = Class.forName("dyr$b");
        Class<?> holder = Class.forName("dyr$c");
        Class<?> normal = Class.forName("eoh");
        Class<?> rarity = Class.forName("dys$z$a");
        Constructor<?> sampler = Class.forName("dys$z").getDeclaredConstructor(density, holder, rarity);
        sampler.setAccessible(true);
        Constructor<?> noiseHolder = holder.getDeclaredConstructor(Class.forName("jm"), normal);
        noiseHolder.setAccessible(true);
        Method compute = density.getMethod("a", context);
        Method constant = Class.forName("dys").getMethod("a", double.class);
        Method getNoise = normal.getMethod("a", double.class, double.class, double.class);
        Map<String, Object> noises = new HashMap<>();
        JsonArray output = new JsonArray();
        for (JsonElement value : input.getAsJsonArray("cases")) {
            JsonObject row = value.getAsJsonObject().deepCopy();
            String mapperName = row.get("mapper").getAsString();
            Object mapper = field(rarity, switch (mapperName) {
                case "type_1" -> "a";
                case "type_2" -> "b";
                default -> throw new IllegalArgumentException("unknown mapper: " + mapperName);
            }, null);
            double argument = Double.longBitsToDouble(Long.parseUnsignedLong(row.get("input_bits").getAsString(), 16));
            double scale = ((Double2DoubleFunction) field(rarity, "e", mapper)).get(argument);
            JsonObject expected = new JsonObject();
            expected.addProperty("rarity_bits", bits(scale));
            switch (row.get("op").getAsString()) {
                case "mapper" -> { }
                case "sample" -> {
                    long seed = row.get("seed").getAsLong();
                    String name = row.get("noise").getAsString();
                    String key = seed + ":" + name;
                    Object noise = noises.get(key);
                    if (noise == null) {
                        JsonObject parameters = input.getAsJsonObject("noises").getAsJsonObject(name);
                        if (parameters == null) throw new IllegalArgumentException("missing noise: " + name);
                        Object random = Class.forName("dzz").getConstructor(long.class).newInstance(seed);
                        Object positional = Class.forName("dzz").getMethod("e").invoke(random);
                        Object named = Class.forName("dzl").getMethod("a", String.class).invoke(positional, name);
                        JsonArray amplitudes = parameters.getAsJsonArray("amplitudes");
                        double[] amps = new double[amplitudes.size()];
                        for (int i = 0; i < amps.length; i++) amps[i] = amplitudes.get(i).getAsDouble();
                        noise = normal.getMethod("a", Class.forName("ayw"), int.class, double[].class)
                                .invoke(null, named, parameters.get("firstOctave").getAsInt(), amps);
                        noises.put(key, noise);
                    }
                    JsonArray position = row.getAsJsonArray("pos");
                    int x = position.get(0).getAsInt(), y = position.get(1).getAsInt(), z = position.get(2).getAsInt();
                    Object point = Class.forName("dyr$e").getConstructor(int.class, int.class, int.class)
                            .newInstance(x, y, z);
                    // NoiseHolder.getValue only reads its real noise field. The unused registry
                    // holder is null: normal constructors, not allocation bypass or codec proof.
                    Object instance = sampler.newInstance(constant.invoke(null, argument),
                            noiseHolder.newInstance(null, noise), mapper);
                    double result = (double) compute.invoke(instance, point);
                    double nx = x / scale, ny = y / scale, nz = z / scale;
                    double raw = (double) getNoise.invoke(noise, nx, ny, nz);
                    expected.addProperty("noise_bits", bits(raw));
                    expected.addProperty("value_bits", bits(result));
                    JsonArray scaled = new JsonArray();
                    for (double coordinate : new double[] {nx, ny, nz}) scaled.add(bits(coordinate));
                    expected.add("scaled_bits", scaled);
                }
                default -> throw new IllegalArgumentException("unknown operation: " + row);
            }
            row.add("expected", expected);
            output.add(row);
        }
        Files.writeString(Path.of(args[1]), new GsonBuilder().setPrettyPrinting().create().toJson(output));
    }
}
