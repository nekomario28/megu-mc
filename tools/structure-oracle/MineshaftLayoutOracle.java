package dev.mcspine.tools;

import com.google.gson.*;
import com.mojang.serialization.DynamicOps;
import com.mojang.serialization.JsonOps;
import java.lang.reflect.*;
import java.nio.file.*;
import java.util.*;
import java.util.concurrent.atomic.AtomicLong;
import java.util.function.Predicate;

/** Actual normal-mineshaft layout, with non-consuming RNG boundary snapshots.
 * This isolates layout after a candidate is selected; it does not admit starts,
 * query biome eligibility, or place blocks. Mesa's terrain-dependent adjustment
 * is deliberately outside this oracle's contract.
 */
public final class MineshaftLayoutOracle {
    static final Class<?> C(String n) { return NativeHarness.C(n); }

    static JsonObject checkpoint(Object rng) throws Exception {
        Field source = C("dzx").getDeclaredField("d"); // WorldgenRandom.randomSource
        source.setAccessible(true);
        Object legacy = source.get(rng);
        if (legacy.getClass() != C("dyz")) throw new AssertionError("not LegacyRandomSource");
        Field seed = C("dyz").getDeclaredField("h"); // LegacyRandomSource.seed
        seed.setAccessible(true);
        JsonObject p = new JsonObject();
        p.addProperty("state48", ((AtomicLong) seed.get(legacy)).get());
        p.addProperty("draws", (int) C("dzx").getMethod("l").invoke(rng));
        return p;
    }

    static Object context(long seed, int x, int z) throws Exception {
        Object generator = NativeHarness.probeGenerator();
        if ((int) C("duz").getMethod("f").invoke(generator) != 63
                || (int) C("duz").getMethod("g").invoke(generator) != -64)
            throw new AssertionError("normal Overworld adjustment inputs changed");
        return C("ejr$a").getConstructor(C("ka"), C("duz"), C("dea"), C("dzm"),
                C("enu"), long.class, C("dcd"), C("dcy"), Predicate.class)
                .newInstance(null, generator, null, null, null, seed,
                        C("dcd").getConstructor(int.class, int.class).newInstance(x, z),
                        C("dcy").getMethod("e", int.class, int.class).invoke(null, -64, 384),
                        (Predicate<Object>) ignored -> true);
    }

    static JsonElement tag(Object piece) throws Exception {
        Object nbt = C("ejv").getMethod("a", C("ekh")).invoke(piece, new Object[]{null});
        DynamicOps<Object> ops = (DynamicOps<Object>) C("up").getField("a").get(null);
        return ops.convertTo(JsonOps.INSTANCE, nbt);
    }

    static JsonArray pieces(Object builder) throws Exception {
        Field f = C("ekj").getDeclaredField("a"); // StructurePiecesBuilder.pieces
        f.setAccessible(true);
        JsonArray result = new JsonArray();
        for (Object p : (List<?>) f.get(builder)) result.add(tag(p));
        return result;
    }

    static JsonObject scenario(long seed, int x, int z) throws Exception {
        Object type = C("elw$a").getField("a").get(null); // NORMAL
        // findGenerationPoint does not read StructureSettings; biome admission is
        // intentionally not claimed by this isolated actual-method comparison.
        Object structure = C("elw").getConstructor(C("ejr$c"), C("elw$a"))
                .newInstance(null, type);
        Object actualContext = context(seed, x, z);
        Object actualRng = C("ejr$a").getMethod("f").invoke(actualContext);
        Object optional = C("elw").getMethod("a", C("ejr$a")).invoke(structure, actualContext);
        Object stub = ((Optional<?>) optional).orElseThrow();
        Method getBuilder = C("ejr$b").getMethod("a"); // GenerationStub.getPiecesBuilder
        if (getBuilder.getReturnType() != C("ekj")) throw new AssertionError("wrong builder mapping");
        Object actualBuilder = getBuilder.invoke(stub);
        JsonArray actual = pieces(actualBuilder);
        JsonObject actualEnd = checkpoint(actualRng);

        // Partition the *native* calls and compare to the complete native body.
        // This also detects a checkpoint helper that accidentally advances RNG.
        Object ctx = context(seed, x, z);
        Object rng = C("ejr$a").getMethod("f").invoke(ctx);
        JsonObject checkpoints = new JsonObject();
        checkpoints.add("context", checkpoint(rng));
        C("ayw").getMethod("j").invoke(rng); // findGenerationPoint's discarded nextDouble
        checkpoints.add("before_room", checkpoint(rng));
        Object room = C("elv$d").getConstructor(int.class, C("ayw"), int.class,
                int.class, C("elw$a")).newInstance(0, rng, (x << 4) + 2, (z << 4) + 2, type);
        JsonElement initialRoom = tag(room);
        checkpoints.add("after_room", checkpoint(rng));
        Object builder = C("ekj").getConstructor().newInstance();
        C("ekj").getMethod("a", C("ejv")).invoke(builder, room);
        C("elv$d").getMethod("a", C("ejv"), C("ejw"), C("ayw"))
                .invoke(room, room, builder, rng);
        checkpoints.add("after_children", checkpoint(rng));
        int shift = (int) C("ekj").getMethod("a", int.class, int.class, C("ayw"), int.class)
                .invoke(builder, 63, -64, rng, 10);
        checkpoints.add("after_adjustment", checkpoint(rng));
        JsonArray partitioned = pieces(builder);
        if (!actual.equals(partitioned) || !actualEnd.equals(checkpoint(rng)))
            throw new AssertionError("partitioned native calls / snapshots changed the actual layout");
        JsonObject result = new JsonObject();
        result.addProperty("seed", seed); result.addProperty("cx", x); result.addProperty("cz", z);
        result.addProperty("type", "normal"); result.addProperty("sea_level", 63);
        result.addProperty("min_y", -64); result.addProperty("margin", 10);
        result.addProperty("vertical_shift", shift);
        result.add("initial_room", initialRoom); result.add("checkpoints", checkpoints);
        result.add("pieces", actual);
        return result;
    }

    static String csv(JsonArray values) {
        List<String> out = new ArrayList<>();
        for (JsonElement v : values) out.add(v.getAsString());
        return String.join(",", out);
    }

    static String vectors(JsonArray cases) {
        StringBuilder out = new StringBuilder();
        for (JsonElement e : cases) {
            JsonObject c = e.getAsJsonObject();
            out.append("C");
            for (String k : List.of("seed", "cx", "cz", "vertical_shift")) out.append('\t').append(c.get(k).getAsString());
            for (String k : List.of("context", "before_room", "after_room", "after_children", "after_adjustment"))
                out.append('\t').append(c.getAsJsonObject("checkpoints").getAsJsonObject(k).get("state48").getAsString());
            out.append('\t').append(c.getAsJsonArray("pieces").size()).append('\n');
            for (JsonElement v : c.getAsJsonArray("pieces")) {
                JsonObject p = v.getAsJsonObject();
                if (p.get("MST").getAsInt() != 0) throw new AssertionError("not NORMAL");
                String kind = p.get("id").getAsString().replace("minecraft:", "");
                out.append("P\t").append(kind).append('\t').append(p.get("GD").getAsInt())
                        .append('\t').append(p.get("O").getAsInt()).append('\t')
                        .append(csv(p.getAsJsonArray("BB"))).append('\t');
                String payload = switch (kind) {
                    case "msroom" -> {
                        List<String> entrances = new ArrayList<>();
                        for (JsonElement b : p.getAsJsonArray("Entrances")) entrances.add(csv(b.getAsJsonArray()));
                        yield String.join(";", entrances);
                    }
                    case "mscorridor" -> p.get("hr").getAsInt() + "," + p.get("sc").getAsInt()
                            + "," + p.get("hps").getAsInt() + "," + p.get("Num").getAsInt();
                    case "mscrossing" -> p.get("D").getAsInt() + "," + p.get("tf").getAsInt();
                    case "msstairs" -> "";
                    default -> throw new AssertionError("unhandled piece " + kind);
                };
                // Explicit empty payload keeps generated TSV free of trailing tabs.
                out.append(payload.isEmpty() ? "-" : payload).append('\n');
            }
        }
        return out.toString();
    }

    public static void main(String[] args) throws Exception {
        NativeHarness.bootstrap();
        JsonArray result = new JsonArray();
        long[][] cases = {{4242,4,3},{0,0,0},{1,-1,2},{-1,-7,-11},
                {0x123456789abcdef0L,-37,64},{Long.MAX_VALUE,64,-13}};
        for (long[] c : cases) result.add(scenario(c[0], (int)c[1], (int)c[2]));
        Files.writeString(Path.of(args[0]), vectors(result));
        if (args.length > 1)
            Files.writeString(Path.of(args[1]), new GsonBuilder().setPrettyPrinting().create().toJson(result) + "\n");
        System.out.println("native normal-mineshaft layout cases=" + result.size());
    }
}
