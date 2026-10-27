package dev.mcspine.tools;

import java.nio.charset.StandardCharsets;
import java.nio.file.*;
import java.security.MessageDigest;
import java.util.*;

/**
 * Actual 1.21.1 `minecraft:nether_forest_vegetation` (`ecx`, NetherForestVegetationFeature).
 *
 * The origin's *below* block must be in the `minecraft:nylium` tag, then up to
 * `spread_width^2` offsets are tried, each drawing three pairs of `nextInt(spread)`
 * (x, y, z), sampling the state provider, and placing only where the cell is empty,
 * above the build floor and the state's own `canSurvive` accepts the support below.
 *
 * The support-block scenes are what pins `canSurvive` for the plants: the port must
 * reproduce the native answer for each (support, plant) pair, not a guessed tag.
 */
public final class NetherForestVegetationOracle {
    static final String[] SUPPORTS = {
        "minecraft:crimson_nylium", "minecraft:warped_nylium", "minecraft:netherrack",
        "minecraft:warped_wart_block", "minecraft:soul_sand", "minecraft:soul_soil",
        "minecraft:stone", "minecraft:dirt", "minecraft:crimson_planks",
    };
    static final String[] PLANTS = {
        "minecraft:crimson_roots", "minecraft:warped_roots", "minecraft:nether_sprouts",
        "minecraft:crimson_fungus", "minecraft:warped_fungus",
    };

    static String config(String plant) {
        return "{\"state_provider\":{\"type\":\"minecraft:simple_state_provider\",\"state\":{\"Name\":\""
                + plant + "\"}},\"spread_width\":8,\"spread_height\":4}";
    }

    public static void main(String[] args) throws Exception {
        NativeHarness.bootstrap();
        Class<?> cls = NativeHarness.featureClass("nether_forest_vegetation");
        if (!cls.getName().equals("ecx")) throw new AssertionError("class changed: " + cls);
        List<String> rows = new ArrayList<>();
        dump = args.length > 1 ? new ArrayList<>() : null;
        for (String support : SUPPORTS) {
            for (String plant : PLANTS) {
                rows.add(run(support, plant, 64, config(plant)));
            }
        }
        // The real weighted provider from the pack, so the selection draws are pinned.
        String weighted = "{\"state_provider\":{\"type\":\"minecraft:weighted_state_provider\",\"entries\":["
                + "{\"data\":{\"Name\":\"minecraft:crimson_roots\"},\"weight\":87},"
                + "{\"data\":{\"Name\":\"minecraft:crimson_fungus\"},\"weight\":11},"
                + "{\"data\":{\"Name\":\"minecraft:warped_fungus\"},\"weight\":1}]},"
                + "\"spread_width\":8,\"spread_height\":4}";
        for (long seed : new long[]{0, 1, 42, -7}) {
            rows.add(run("minecraft:crimson_nylium", "weighted", 64, weighted, seed));
        }
        // The build-height guard: origin at the floor and at the ceiling.
        rows.add(run("minecraft:crimson_nylium", "minecraft:crimson_roots", NativeHarness.MIN_Y + 1, config("minecraft:crimson_roots")));
        rows.add(run("minecraft:crimson_nylium", "minecraft:crimson_roots", NativeHarness.MIN_Y, config("minecraft:crimson_roots")));
        rows.add(run("minecraft:crimson_nylium", "minecraft:crimson_roots", NativeHarness.MIN_Y + NativeHarness.HEIGHT - 1, config("minecraft:crimson_roots")));
        Files.write(Path.of(args[0]), rows);
        if (dump != null) Files.write(Path.of(args[1]), dump);
        System.out.println("native nether_forest_vegetation cases: " + rows.size());
    }

    static List<String> dump;

    static String run(String support, String plant, int oy, String config) throws Exception {
        return run(support, plant, oy, config, 0);
    }

    static String run(String support, String plant, int oy, String config, long seed) throws Exception {
        final int floorY = oy - 1;
        NativeHarness.Level level = NativeHarness.level(
                (x, y, z) -> y == floorY ? support : null);
        Object rng = NativeHarness.rng(seed);
        boolean placed = NativeHarness.place("nether_forest_vegetation", level, rng, 0, oy, 0, config);
        List<String> cells = new ArrayList<>();
        for (Map.Entry<String, Object> e : level.writes.entrySet()) {
            String[] p = e.getKey().split(",");
            cells.add((Integer.parseInt(p[0])) + "," + (Integer.parseInt(p[1]) - oy) + ","
                    + (Integer.parseInt(p[2])) + " " + NativeHarness.nameOf(e.getValue()) + "\n");
        }
        Collections.sort(cells);
        if (dump != null) {
            for (String l2 : level.log) {
                if (l2.startsWith("set ")) dump.add("SET\t" + support + "\t" + plant + "\t" + seed + "\t" + l2.substring(4));
            }
            for (String a2 : level.attempts) {
                dump.add("ATT\t" + support + "\t" + plant + "\t" + seed + "\t" + a2);
            }
            for (String c : cells) dump.add(support + "\t" + plant + "\t" + seed + "\t" + c.trim());
        }
        String digest = HexFormat.of().formatHex(MessageDigest.getInstance("MD5")
                .digest(String.join("", cells).getBytes(StandardCharsets.UTF_8)));
        return support + "\t" + plant + "\t" + oy + "\t" + seed + "\t" + placed + "\t"
                + cells.size() + "\t" + digest;
    }
}
