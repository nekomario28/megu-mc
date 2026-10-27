package dev.mcspine.tools;

import java.nio.charset.StandardCharsets;
import java.nio.file.*;
import java.security.MessageDigest;
import java.util.*;

/**
 * Actual 1.21.1 `minecraft:end_platform` (`ecc`, EndPlatformFeature).
 *
 * The platform is obsidian at dy = -1 and air at dy = 0..2 over a 5x5 box, and the
 * feature skips a cell that already holds the target *block* (so a cell that is air
 * with different properties would still be rewritten — none of these blocks have
 * properties, but the comparison is by block, not by state).
 */
public final class EndPlatformOracle {
    static final String CONFIG = "{}";

    public static void main(String[] args) throws Exception {
        NativeHarness.bootstrap();
        Class<?> cls = NativeHarness.featureClass("end_platform");
        if (!cls.getName().equals("ecc")) throw new AssertionError("class changed: " + cls);
        int scenes = 5;
        List<String> rows = new ArrayList<>();
        for (int scene = 0; scene < scenes; scene++) {
            final int sc = scene;
            NativeHarness.Level level = NativeHarness.level((x, y, z) -> {
                int dy = y - 64;
                int dx = x, dz = z;
                switch (sc) {
                    case 0: // empty world
                        return null;
                    case 1: // already built
                        return dy == -1 ? "minecraft:obsidian" : null;
                    case 2: // half-built floor, rock above
                        if (dy == -1) return dx > 0 ? "minecraft:obsidian" : null;
                        return dy >= 0 && dz < 0 ? "minecraft:stone" : null;
                    case 3: // floor is air, ceiling is stone
                        return dy >= 2 ? "minecraft:stone" : null;
                    default: // obsidian everywhere around, air column
                        return dy <= -1 ? "minecraft:obsidian" : null;
                }
            });
            Object rng = NativeHarness.rng(0);
            boolean placed = NativeHarness.place("end_platform", level, rng, 0, 64, 0, CONFIG);
            List<String> cells = new ArrayList<>();
            for (Map.Entry<String, Object> e : level.writes.entrySet()) {
                String[] p = e.getKey().split(",");
                cells.add((Integer.parseInt(p[0])) + "," + (Integer.parseInt(p[1]) - 64) + ","
                        + (Integer.parseInt(p[2])) + " " + NativeHarness.nameOf(e.getValue()) + "\n");
            }
            Collections.sort(cells);
            String digest = HexFormat.of().formatHex(MessageDigest.getInstance("MD5")
                    .digest(String.join("", cells).getBytes(StandardCharsets.UTF_8)));
            rows.add(scene + "\t0,64,0\t" + placed + "\t" + cells.size() + "\t"
                    + NativeHarness.rngState(rng) + "\t" + digest);
        }
        Files.write(Path.of(args[0]), rows);
        System.out.println("native end_platform cases: " + rows.size());
    }
}
