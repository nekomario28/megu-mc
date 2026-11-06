package dev.mcspine.tools;

import com.google.gson.JsonParser;
import java.io.BufferedReader;
import java.io.BufferedWriter;
import java.lang.reflect.InvocationTargetException;
import java.lang.reflect.Method;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.HashSet;
import java.util.Set;

/** Every pinned registry state under varied real 1.21.1 BlockState.canSurvive calls.
 * Results are observations, not a generated port implementation. Unsupported
 * harness calls are emitted as U but always make the process exit nonzero.
 */
public final class NativeSurvivalOracle {
    record Context(String id, int mask, String material) {}
    // Neighbor bits are DOWN, UP, NORTH, SOUTH, WEST, EAST.
    static final Context[] CONTEXTS = {
        new Context("empty", 0, "air"),
        new Context("floor_stone", 1, "stone"),
        new Context("ceiling_stone", 2, "stone"),
        new Context("north_stone", 4, "stone"),
        new Context("south_stone", 8, "stone"),
        new Context("west_stone", 16, "stone"),
        new Context("east_stone", 32, "stone"),
        new Context("all_stone", 63, "stone"),
        new Context("all_except_floor", 62, "stone"),
        new Context("all_except_north", 59, "stone"),
        new Context("floor_water", 1, "water"),
        new Context("floor_lava", 1, "lava")
    };
    static int bit(int x, int y, int z) {
        if (x == 0 && y == 63 && z == 0) return 1;
        if (x == 0 && y == 65 && z == 0) return 2;
        if (x == 0 && y == 64 && z == -1) return 4;
        if (x == 0 && y == 64 && z == 1) return 8;
        if (x == -1 && y == 64 && z == 0) return 16;
        if (x == 1 && y == 64 && z == 0) return 32;
        return 0;
    }

    public static void main(String[] args) throws Exception {
        if (args.length != 3)
            throw new IllegalArgumentException("input.tsv output.tsv expected-states");
        int expected = Integer.parseInt(args[2]);
        if (expected != 1060 && expected != 26684)
            throw new IllegalArgumentException("expected-states must be a complete scope");
        NativeHarness.bootstrap();
        // The native bootstrap alone does not populate block tags, and many
        // canSurvive implementations depend on them. Load and verify native
        // built-in pack tags rather than allowing an all-false false-positive.
        VanillaBlockTags.load(NativeHarness.C("lt").getField("e").get(null));
        Method survives = NativeHarness.C("dtc")
                .getMethod("a", NativeHarness.C("dcz"), NativeHarness.C("jd"));
        Object center = NativeHarness.pos(0, 64, 0);
        Set<Integer> seen = new HashSet<>();
        long observations = 0, unsupported = 0;
        try (BufferedReader input = Files.newBufferedReader(
                Path.of(args[0]), StandardCharsets.UTF_8);
             BufferedWriter output = Files.newBufferedWriter(
                Path.of(args[1]), StandardCharsets.UTF_8)) {
            String line;
            while ((line = input.readLine()) != null) {
                String[] fields = line.split("\t", 2);
                if (fields.length != 2)
                    throw new IllegalArgumentException("bad state input: " + line);
                int id = Integer.parseInt(fields[0]);
                if (!seen.add(id)) throw new IllegalArgumentException("duplicate state ID " + id);
                Object current = NativeHarness.stateFromJson(
                        JsonParser.parseString(fields[1]));
                String normalized = NativeHarness.normalizedStateName(current);
                for (Context context : CONTEXTS) {
                    Object support = NativeHarness.state("minecraft:" + context.material);
                    NativeHarness.Level level = NativeHarness.levelFromStates((x, y, z) -> {
                        if (x == 0 && y == 64 && z == 0) return current;
                        return (bit(x, y, z) & context.mask()) != 0
                                ? support : NativeHarness.state("minecraft:air");
                    });
                    String result;
                    try {
                        result = (boolean) survives.invoke(current, level.proxy, center) ? "T" : "F";
                        if (!level.events.isEmpty())
                            throw new AssertionError("canSurvive made world writes or ticks");
                    } catch (InvocationTargetException e) {
                        result = "U:" + e.getCause().getClass().getSimpleName();
                    } catch (Throwable e) {
                        result = "U:" + e.getClass().getSimpleName();
                    }
                    if (result.startsWith("U:")) unsupported++;
                    output.write(id + "\t" + context.id() + "\t" + result
                            + "\t" + normalized + "\t"
                            + String.join(";", level.attempts) + "\n");
                    observations++;
                }
            }
        }
        if (seen.size() != expected)
            throw new AssertionError("incomplete state inventory: " + seen.size()
                    + " expected " + expected);
        System.out.println("NATIVE_CAN_SURVIVE states=" + seen.size()
                + " scenarios=" + CONTEXTS.length + " observations=" + observations
                + " unsupported=" + unsupported);
        if (unsupported != 0)
            throw new AssertionError("unsupported native calls: " + unsupported
                    + "; inspect U rows; no success claim or partial fixture acceptance");
    }
}
