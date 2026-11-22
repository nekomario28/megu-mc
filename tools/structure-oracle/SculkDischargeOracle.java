package dev.megu.tools;

import com.google.gson.JsonObject;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.ArrayList;
import java.util.List;

/** Run the actual discharge callback with the caller's snapshot and a different live state. */
public final class SculkDischargeOracle {
    static Object vein(int mask) throws Exception {
        String[] directions = {"down", "up", "north", "south", "west", "east"};
        JsonObject state = new JsonObject();
        state.addProperty("Name", "minecraft:sculk_vein");
        JsonObject properties = new JsonObject();
        for (int d = 0; d < directions.length; d++) {
            properties.addProperty(directions[d], Boolean.toString((mask & (1 << d)) != 0));
        }
        state.add("Properties", properties);
        return NativeHarness.stateFromJson(state);
    }

    public static void main(String[] args) throws Exception {
        if (args.length != 1) throw new IllegalArgumentException("output TSV path required");
        NativeHarness.bootstrap();
        // SculkVeinBlock.onDischarged(LevelAccessor, BlockState, BlockPos, RandomSource).
        var discharge = Class.forName("dmv").getMethod("a", Class.forName("dcx"),
                Class.forName("dtc"), Class.forName("jd"), Class.forName("ayw"));
        Object block = Class.forName("dtc").getMethod("b")
                .invoke(NativeHarness.state("sculk_vein"));
        List<String> rows = new ArrayList<>();
        // captured faces, live faces, below is sculk (otherwise bedrock).
        int[][] cases = {{9,13,1}, {8,12,1}, {9,9,1}, {13,13,1}, {9,13,0}, {1,5,1}};
        for (int[] c : cases) {
            Object captured = vein(c[0]);
            Object live = vein(c[1]);
            Object below = NativeHarness.state(c[2] == 1 ? "sculk" : "bedrock");
            NativeHarness.Level level = NativeHarness.levelFromStates((x, y, z) -> {
                if (x == 0 && z == 0 && y == 80) return live;
                if (x == 0 && z == 0 && y == 79) return below;
                return null;
            });
            Object rng = NativeHarness.rng(0);
            discharge.invoke(block, level.proxy, captured, NativeHarness.pos(0, 80, 0), rng);
            Object result = level.writes.get("0,80,0");
            if (result == null) throw new AssertionError("native discharge did not write");
            rows.add(c[0] + "\t" + c[1] + "\t" + c[2] + "\t"
                    + NativeHarness.normalizedStateName(result) + "\t" + NativeHarness.draw(rng, 1000));
        }
        Files.write(Path.of(args[0]), rows);
    }
}
