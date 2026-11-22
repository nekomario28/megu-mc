package dev.megu.tools;

import java.nio.charset.StandardCharsets;
import java.nio.file.*;
import java.security.MessageDigest;
import java.util.*;

/**
 * Actual 1.21.1 `minecraft:delta_feature` (`ebv`, DeltaFeature).
 *
 * A candidate cell qualifies when it is neither `contents` nor one of the fixed
 * cannot-replace blocks, every horizontal and downward neighbour is non-air, and the
 * cell above is air — a one-wide, two-high cavity. The rim goes at the candidate and
 * `contents` at the rim offset, and the loop keeps reading, so a write can reject a
 * later candidate: the scenes pin that too.
 */
public final class DeltaFeatureOracle {
    static final String CONFIG =
            "{\"contents\":{\"Name\":\"minecraft:lava\",\"Properties\":{\"level\":\"0\"}},"
            + "\"rim\":{\"Name\":\"minecraft:magma_block\"},"
            + "\"rim_size\":{\"type\":\"minecraft:uniform\",\"min_inclusive\":0,\"max_inclusive\":2},"
            + "\"size\":{\"type\":\"minecraft:uniform\",\"min_inclusive\":3,\"max_inclusive\":7}}";

    /** A one-wide two-high hole on a 3x3 grid, with `fill` as the block at the floor. */
    static String scene(int sc, int x, int y, int z) {
        int dy = y - 64;
        boolean hole = Math.floorMod(x, 3) == 0 && Math.floorMod(z, 3) == 0
                && Math.abs(x) <= 9 && Math.abs(z) <= 9;
        switch (sc) {
            case 0:
                return hole && (dy == 0 || dy == 1) ? null : "minecraft:netherrack";
            case 1: // no cavity at all
                return "minecraft:netherrack";
            case 2: // every hole is already contents
                return hole && (dy == 0 || dy == 1) ? (dy == 0 ? "minecraft:lava" : null)
                        : "minecraft:netherrack";
            case 3: // one-high holes: the cell above is solid
                return hole && dy == 0 ? null : "minecraft:netherrack";
            case 4: // the floor of each hole is bedrock
                return hole && dy == 0 ? "minecraft:bedrock" : "minecraft:netherrack";
            default: // the floor of each hole is nether bricks
                return hole && dy == 0 ? "minecraft:nether_bricks" : "minecraft:netherrack";
        }
    }

    public static void main(String[] args) throws Exception {
        NativeHarness.bootstrap();
        Class<?> cls = NativeHarness.featureClass("delta_feature");
        if (!cls.getName().equals("ebv")) throw new AssertionError("class changed: " + cls);
        List<String> rows = new ArrayList<>();
        sets = args.length > 1 ? new ArrayList<>() : null;
        for (int scene = 0; scene < 6; scene++) {
            for (long seed : new long[]{0, 1, -1, 42, 60007, Long.MIN_VALUE, Long.MAX_VALUE}) {
                rows.add(run(scene, seed));
            }
        }
        Files.write(Path.of(args[0]), rows);
        if (args.length > 1) Files.write(Path.of(args[1]), sets);
        System.out.println("native delta_feature cases: " + rows.size());
    }

    static List<String> sets;

    static String run(int scene, long seed) throws Exception {
        final int sc = scene;
        NativeHarness.Level level = NativeHarness.level((x, y, z) -> scene(sc, x, y, z));
        Object rng = NativeHarness.rng(seed);
        boolean placed = NativeHarness.place("delta_feature", level, rng, 0, 64, 0, CONFIG);
        List<String> cells = new ArrayList<>();
        for (Map.Entry<String, Object> e : level.writes.entrySet()) {
            String[] p = e.getKey().split(",");
            cells.add((Integer.parseInt(p[0])) + "," + (Integer.parseInt(p[1]) - 64) + ","
                    + (Integer.parseInt(p[2])) + " " + NativeHarness.nameOf(e.getValue()) + "\n");
        }
        if (sets != null) {
            for (String l : level.log) {
                if (l.startsWith("set ")) {
                    sets.add(scene + "\t" + seed + "\t" + l.substring(4));
                }
            }
        }
        Collections.sort(cells);
        String digest = HexFormat.of().formatHex(MessageDigest.getInstance("MD5")
                .digest(String.join("", cells).getBytes(StandardCharsets.UTF_8)));
        return scene + "\t" + seed + "\t" + placed + "\t" + cells.size() + "\t" + digest;
    }
}
