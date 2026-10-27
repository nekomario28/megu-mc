package dev.mcspine.tools;

import java.nio.charset.StandardCharsets;
import java.nio.file.*;
import java.security.MessageDigest;
import java.util.*;

/**
 * Actual 1.21.1 `minecraft:huge_brown_mushroom` (`ecm` over `ebg`, config
 * `{cap_provider, stem_provider, foliage_radius}`).
 *
 * Cells are recorded as `minecraft:block[prop=value,...]` with the properties sorted,
 * because the cap's four rim properties are the part of this feature a name-only
 * fixture cannot see.
 */
public final class HugeBrownMushroomOracle {
    static String provider(String name) {
        return "{\"type\":\"minecraft:simple_state_provider\",\"state\":{\"Name\":\"" + name
                + "\",\"Properties\":{\"down\":\"false\",\"east\":\"true\",\"north\":\"true\","
                + "\"south\":\"true\",\"up\":\"true\",\"west\":\"true\"}}}";
    }

    static final String CONFIG = "{\"cap_provider\":" + provider("minecraft:brown_mushroom_block")
            + ",\"foliage_radius\":3,\"stem_provider\":" + provider("minecraft:mushroom_stem") + "}";

    static String scene(int sc, int x, int y, int z) {
        int dy = y - 64;
        switch (sc) {
            case 0:
                return dy <= -1 ? "minecraft:dirt" : null;
            case 1:
                return dy <= -1 ? "minecraft:mycelium" : null;
            case 2:
                return dy <= -1 ? "minecraft:stone" : null;
            case 3:
                return dy <= -1 ? "minecraft:dirt"
                        : (dy == 5 && Math.abs(x) <= 2 && Math.abs(z) <= 2 ? "minecraft:oak_leaves" : null);
            case 4:
                return dy <= -1 ? "minecraft:dirt"
                        : (dy == 5 && Math.abs(x) <= 2 && Math.abs(z) <= 2 ? "minecraft:stone" : null);
            default:
                return dy <= -1 ? "minecraft:dirt" : (dy >= 8 ? "minecraft:stone" : null);
        }
    }

    public static void main(String[] args) throws Exception {
        NativeHarness.bootstrap();
        Class<?> cls = NativeHarness.featureClass("huge_brown_mushroom");
        if (!cls.getName().equals("ecm")) throw new AssertionError("class changed: " + cls);
        List<String> rows = new ArrayList<>();
        for (int scene = 0; scene < 6; scene++) {
            for (long seed : new long[]{0, 1, -1, 42, 60007}) {
                rows.add(run(scene, 64, seed));
            }
        }
        rows.add(run(0, NativeHarness.MIN_Y + 1, 0));
        rows.add(run(0, NativeHarness.MIN_Y, 0));
        rows.add(run(0, NativeHarness.MIN_Y + NativeHarness.HEIGHT - 2, 0));
        Files.write(Path.of(args[0]), rows);
        System.out.println("native huge_brown_mushroom cases: " + rows.size());
    }

    static String run(int scene, int oy, long seed) throws Exception {
        final int sc = scene;
        final int origin = oy;
        NativeHarness.Level level = NativeHarness.level((x, y, z) -> scene(sc, x, y - origin + 64, z));
        Object rng = NativeHarness.rng(seed);
        boolean placed = NativeHarness.place("huge_brown_mushroom", level, rng, 0, oy, 0, CONFIG);
        List<String> cells = new ArrayList<>();
        for (Map.Entry<String, Object> e : level.writes.entrySet()) {
            String[] p = e.getKey().split(",");
            cells.add((Integer.parseInt(p[0])) + "," + (Integer.parseInt(p[1]) - oy) + ","
                    + (Integer.parseInt(p[2])) + " " + NativeHarness.normalizedStateName(e.getValue()) + "\n");
        }
        Collections.sort(cells);
        String digest = HexFormat.of().formatHex(MessageDigest.getInstance("MD5")
                .digest(String.join("", cells).getBytes(StandardCharsets.UTF_8)));
        return scene + "\t" + oy + "\t" + seed + "\t" + placed + "\t" + cells.size() + "\t" + digest;
    }
}
