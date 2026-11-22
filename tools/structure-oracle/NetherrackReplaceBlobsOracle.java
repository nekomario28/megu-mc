package dev.megu.tools;

import java.nio.charset.StandardCharsets;
import java.nio.file.*;
import java.security.MessageDigest;
import java.util.*;

/**
 * Actual 1.21.1 `minecraft:netherrack_replace_blobs` (`ede`, ReplaceBlobsFeature).
 *
 * The origin is clamped into the build height, walked down to the first `target`
 * block, then up to three `radius` samples bound a manhattan ball: every `target`
 * cell in it becomes `state`. The scenes pin the walk, the ball, and that a
 * non-target cell is left alone.
 */
public final class NetherrackReplaceBlobsOracle {
    static final String CONFIG =
            "{\"radius\":{\"type\":\"minecraft:uniform\",\"min_inclusive\":3,\"max_inclusive\":7},"
            + "\"state\":{\"Name\":\"minecraft:basalt\",\"Properties\":{\"axis\":\"y\"}},"
            + "\"target\":{\"Name\":\"minecraft:netherrack\"}}";

    static String scene(int sc, int x, int y, int z) {
        int dy = y - 64;
        int r2 = x * x + dy * dy + z * z;
        switch (sc) {
            case 0: // solid netherrack below y=64, air above
                return dy <= -1 ? "minecraft:netherrack" : null;
            case 1: // a floating netherrack pocket the walk has to reach
                return (r2 <= 9 || (dy <= -4 && dy >= -8 && Math.abs(x) <= 4 && Math.abs(z) <= 4))
                        ? "minecraft:netherrack" : null;
            case 2: // nothing to replace
                return null;
            case 3: // netherrack with basalt inside it
                if (dy <= -1) return (Math.abs(x) <= 2 && Math.abs(z) <= 2 && dy >= -3)
                        ? "minecraft:basalt" : "minecraft:netherrack";
                return null;
            case 4: // netherrack only above the origin: the walk goes down, so nothing
                return dy >= 1 && dy <= 3 ? "minecraft:netherrack" : null;
            default: // a slab with a hole in the middle
                if (dy <= -1) return Math.abs(x) <= 1 && Math.abs(z) <= 1 ? null : "minecraft:netherrack";
                return null;
        }
    }

    public static void main(String[] args) throws Exception {
        NativeHarness.bootstrap();
        Class<?> cls = NativeHarness.featureClass("netherrack_replace_blobs");
        if (!cls.getName().equals("ede")) throw new AssertionError("class changed: " + cls);
        List<String> rows = new ArrayList<>();
        for (int scene = 0; scene < 6; scene++) {
            for (long seed : new long[]{0, 1, -1, 42, 60007, Long.MIN_VALUE, Long.MAX_VALUE}) {
                rows.add(run(scene, 64, seed));
            }
        }
        // The clamp: far below and far above the build height.
        rows.add(run(0, NativeHarness.MIN_Y - 10, 7));
        rows.add(run(0, NativeHarness.MIN_Y + 1, 7));
        rows.add(run(0, NativeHarness.MIN_Y + NativeHarness.HEIGHT + 10, 7));
        rows.add(run(0, NativeHarness.MIN_Y + NativeHarness.HEIGHT - 1, 7));
        Files.write(Path.of(args[0]), rows);
        System.out.println("native netherrack_replace_blobs cases: " + rows.size());
    }

    static String run(int sc, int oy, long seed) throws Exception {
        final int scene = sc;
        NativeHarness.Level level = NativeHarness.level(
                (x, y, z) -> scene(scene, x, y - oy + 64, z));
        Object rng = NativeHarness.rng(seed);
        boolean placed = NativeHarness.place("netherrack_replace_blobs", level, rng, 0, oy, 0, CONFIG);
        List<String> cells = new ArrayList<>();
        for (Map.Entry<String, Object> e : level.writes.entrySet()) {
            String[] p = e.getKey().split(",");
            cells.add((Integer.parseInt(p[0])) + "," + (Integer.parseInt(p[1]) - oy) + ","
                    + (Integer.parseInt(p[2])) + " " + NativeHarness.nameOf(e.getValue()) + "\n");
        }
        Collections.sort(cells);
        String digest = HexFormat.of().formatHex(MessageDigest.getInstance("MD5")
                .digest(String.join("", cells).getBytes(StandardCharsets.UTF_8)));
        return sc + "\t" + oy + "\t" + seed + "\t" + placed + "\t" + cells.size() + "\t" + digest;
    }
}
