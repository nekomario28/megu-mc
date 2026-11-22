package dev.megu.tools;

import java.nio.charset.StandardCharsets;
import java.nio.file.*;
import java.security.MessageDigest;
import java.util.*;

/**
 * Actual 1.21.1 `minecraft:twisting_vines` (`edr`, TwistingVinesFeature), config
 * `{spread_width, spread_height, max_height}`.
 *
 * The origin must be air with netherrack, warped nylium or warped wart block below.
 * Then `spread_width^2` offsets are tried; each found ground column draws a height in
 * 1..max_height (doubled on `nextInt(6) == 0`, forced to 1 on `nextInt(5) == 0`) and
 * grows upward: the last cell is `twisting_vines` with age 17..25, the ones under it
 * are `twisting_vines_plant`.
 */
public final class TwistingVinesOracle {
    static final String CONFIG = "{\"spread_width\":8,\"spread_height\":4,\"max_height\":8}";

    static String scene(int sc, int x, int y, int z) {
        int dy = y - 64;
        switch (sc) {
            case 0: // netherrack floor one below the origin
                return dy <= -1 ? "minecraft:netherrack" : null;
            case 1: // floor plus a ceiling 3 above the origin
                return dy <= -1 || dy >= 3 ? "minecraft:netherrack" : null;
            case 2: // the origin is solid
                return dy <= 0 ? "minecraft:netherrack" : null;
            case 3: // warped nylium floor
                return dy <= -1 ? "minecraft:warped_nylium" : null;
            case 4: // soul sand floor: not a valid ground
                return dy <= -1 ? "minecraft:soul_sand" : null;
            default: // a stepped floor
                return dy <= -1 - (Math.floorMod(x + z, 3)) ? "minecraft:netherrack" : null;
        }
    }

    public static void main(String[] args) throws Exception {
        NativeHarness.bootstrap();
        Class<?> cls = NativeHarness.featureClass("twisting_vines");
        if (!cls.getName().equals("edr")) throw new AssertionError("class changed: " + cls);
        List<String> rows = new ArrayList<>();
        for (int scene = 0; scene < 6; scene++) {
            for (long seed : new long[]{0, 1, -1, 42, 60007, Long.MIN_VALUE, Long.MAX_VALUE}) {
                rows.add(run(scene, seed));
            }
        }
        Files.write(Path.of(args[0]), rows);
        System.out.println("native twisting_vines cases: " + rows.size());
    }

    static String run(int scene, long seed) throws Exception {
        final int sc = scene;
        NativeHarness.Level level = NativeHarness.level((x, y, z) -> scene(sc, x, y, z));
        Object rng = NativeHarness.rng(seed);
        boolean placed = NativeHarness.place("twisting_vines", level, rng, 0, 64, 0, CONFIG);
        List<String> cells = new ArrayList<>();
        for (Map.Entry<String, Object> e : level.writes.entrySet()) {
            String[] p = e.getKey().split(",");
            cells.add((Integer.parseInt(p[0])) + "," + (Integer.parseInt(p[1]) - 64) + ","
                    + (Integer.parseInt(p[2])) + " " + NativeHarness.nameOf(e.getValue()) + "\n");
        }
        Collections.sort(cells);
        String digest = HexFormat.of().formatHex(MessageDigest.getInstance("MD5")
                .digest(String.join("", cells).getBytes(StandardCharsets.UTF_8)));
        return scene + "\t" + seed + "\t" + placed + "\t" + cells.size() + "\t" + digest;
    }
}
