package dev.megu.tools;

import java.nio.charset.StandardCharsets;
import java.nio.file.*;
import java.security.MessageDigest;
import java.util.*;

/**
 * Actual 1.21.1 `minecraft:end_gateway` (`eca`, EndGatewayFeature), config `{exact}`.
 *
 * A 3x5x3 box: the origin becomes `end_gateway`, every cell at the origin's y becomes
 * air, the two straight-above and straight-below cells and the axis lines become
 * bedrock, everything else air. The block-entity exit position is not modelled (the
 * port has no worldgen block entities), which the scenes cannot show either.
 */
public final class EndGatewayOracle {
    static final String CONFIG = "{\"exact\":false}";

    public static void main(String[] args) throws Exception {
        NativeHarness.bootstrap();
        Class<?> cls = NativeHarness.featureClass("end_gateway");
        if (!cls.getName().equals("eca")) throw new AssertionError("class changed: " + cls);
        List<String> rows = new ArrayList<>();
        // scene 0: empty world; scene 1: the box is already filled with stone
        // scene 2: half filled; scene 3: origin at the top of the build height
        for (int scene = 0; scene < 4; scene++) {
            for (long seed : new long[]{0, 42, -1}) {
                rows.add(run(scene, 64, seed));
            }
        }
        rows.add(run(0, NativeHarness.MIN_Y + NativeHarness.HEIGHT - 1, 0));
        rows.add(run(0, NativeHarness.MIN_Y + NativeHarness.HEIGHT - 2, 0));
        Files.write(Path.of(args[0]), rows);
        System.out.println("native end_gateway cases: " + rows.size());
    }

    static String run(int scene, int oy, long seed) throws Exception {
        final int sc = scene;
        final int origin = oy;
        NativeHarness.Level level = NativeHarness.level((x, y, z) -> {
            int dy = y - origin;
            switch (sc) {
                case 0:
                    return null;
                case 1:
                    return "minecraft:stone";
                case 2:
                    return (dy > 0 ? "minecraft:stone" : null);
                default:
                    return (Math.abs(x) == 1 || Math.abs(z) == 1) ? "minecraft:dirt" : null;
            }
        });
        Object rng = NativeHarness.rng(seed);
        boolean placed = NativeHarness.place("end_gateway", level, rng, 0, oy, 0, CONFIG);
        List<String> cells = new ArrayList<>();
        for (Map.Entry<String, Object> e : level.writes.entrySet()) {
            String[] p = e.getKey().split(",");
            cells.add((Integer.parseInt(p[0])) + "," + (Integer.parseInt(p[1]) - oy) + ","
                    + (Integer.parseInt(p[2])) + " " + NativeHarness.nameOf(e.getValue()) + "\n");
        }
        Collections.sort(cells);
        String digest = HexFormat.of().formatHex(MessageDigest.getInstance("MD5")
                .digest(String.join("", cells).getBytes(StandardCharsets.UTF_8)));
        return scene + "\t" + oy + "\t" + seed + "\t" + placed + "\t" + cells.size() + "\t" + digest;
    }
}
