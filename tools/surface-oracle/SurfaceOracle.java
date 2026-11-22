package dev.megu.tools;

import com.google.gson.*;
import com.mojang.serialization.Codec;
import com.mojang.serialization.JsonOps;
import java.lang.reflect.*;
import java.nio.file.*;

/** Pinned 1.21.1 codecs and Biome temperature methods, not a surface-world emulator. */
public final class SurfaceOracle {
    static Object field(String owner, String name) throws Exception {
        Field f = Class.forName(owner).getDeclaredField(name);
        f.setAccessible(true);
        return f.get(null);
    }

    public static void main(String[] args) throws Exception {
        Class.forName("ab").getMethod("a").invoke(null); // SharedConstants.tryDetectVersion
        Class.forName("akt").getMethod("a").invoke(null); // Bootstrap.bootStrap
        JsonObject input = JsonParser.parseString(Files.readString(Path.of(args[0]))).getAsJsonObject();
        Codec<?> rules = (Codec<?>) field("dzp$o", "b"); // SurfaceRules.RuleSource.CODEC
        Codec<?> states = (Codec<?>) field("dtc", "b"); // BlockState.CODEC
        Class<?> posClass = Class.forName("jd");
        Class<?> climateClass = Class.forName("ddw$b");
        Class<?> modifierClass = Class.forName("ddw$d");
        Constructor<?> climate = climateClass.getDeclaredConstructor(boolean.class, float.class,
                modifierClass, float.class);
        climate.setAccessible(true);
        Constructor<?> biome = Class.forName("ddw").getDeclaredConstructor(climateClass,
                Class.forName("dec"), Class.forName("ddx"), Class.forName("dei"));
        biome.setAccessible(true);
        Method temperature = Class.forName("ddw").getDeclaredMethod("f", posClass);
        temperature.setAccessible(true);
        Method cold = Class.forName("ddw").getMethod("b", posClass);
        Method stateId = Class.forName("dfy").getMethod("i", Class.forName("dtc"));
        JsonArray output = new JsonArray();
        for (JsonElement value : input.getAsJsonArray("cases")) {
            JsonObject row = value.getAsJsonObject().deepCopy();
            JsonObject result = new JsonObject();
            switch (row.get("op").getAsString()) {
                case "surface_rule" -> {
                    rules.parse(JsonOps.INSTANCE, input.get("surface_rule")).getOrThrow();
                    result.addProperty("decoded", true);
                }
                case "state" -> {
                    Object state = states.parse(JsonOps.INSTANCE, row.get("state")).getOrThrow();
                    result.addProperty("state_id", (int) stateId.invoke(null, state));
                }
                case "simplex" -> {
                    String source = row.get("source").getAsString();
                    String name = switch (source) {
                        case "height" -> "f";
                        case "frozen" -> "g";
                        case "info" -> "e";
                        default -> throw new IllegalArgumentException("unknown noise field: " + source);
                    };
                    JsonArray pos = row.getAsJsonArray("pos");
                    double value2d = (double) Class.forName("eoj")
                            .getMethod("a", double.class, double.class, boolean.class)
                            .invoke(field("ddw", name), pos.get(0).getAsDouble(), pos.get(1).getAsDouble(), false);
                    result.addProperty("value_bits", Long.toHexString(Double.doubleToRawLongBits(value2d)));
                }
                case "temperature" -> {
                    Object settings = climate.newInstance(true, row.get("base_temperature").getAsFloat(),
                            field("ddw$d", row.get("frozen").getAsBoolean() ? "b" : "a"), 0.5f);
                    // Normal Biome constructor; mob/effects/generation fields are unused by these
                    // two methods (verified in pinned bytecode). No Unsafe or world execution.
                    Object instance = biome.newInstance(settings, null, null, null);
                    JsonArray xyz = row.getAsJsonArray("pos");
                    Object pos = posClass.getConstructor(int.class, int.class, int.class).newInstance(
                            xyz.get(0).getAsInt(), xyz.get(1).getAsInt(), xyz.get(2).getAsInt());
                    float t = (float) temperature.invoke(instance, pos);
                    result.addProperty("temperature_bits", Float.floatToRawIntBits(t));
                    result.addProperty("cold", (boolean) cold.invoke(instance, pos));
                }
                default -> throw new IllegalArgumentException("unknown operation: " + row);
            }
            row.add("expected", result);
            output.add(row);
        }
        Files.writeString(Path.of(args[1]), new GsonBuilder().setPrettyPrinting().create().toJson(output));
    }
}
