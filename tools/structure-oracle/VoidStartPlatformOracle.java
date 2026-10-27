package dev.mcspine.tools;

import java.nio.charset.StandardCharsets;
import java.nio.file.*;
import java.security.MessageDigest;
import java.util.*;

/**
 * Actual 1.21.1 `minecraft:void_start_platform` (`edv`, VoidStartPlatformFeature).
 *
 * The class writes the start platform in the *origin's* chunk when that chunk is
 * within chebyshev distance 1 of chunk (0,0), at y = origin.y + 3, and only where
 * the cell is within chebyshev 16 of (8,8). The scene is air everywhere: this
 * feature reads no block.
 */
public final class VoidStartPlatformOracle {
    static final String CONFIG = "{}";

    public static void main(String[] args) throws Exception {
        NativeHarness.bootstrap();
        Class<?> cls = NativeHarness.featureClass("void_start_platform");
        if (!cls.getName().equals("edv")) throw new AssertionError("class changed: " + cls);
        // origin x/z chosen to cover chunk -1, 0, 1 (writes) and 2 (refusal)
        int[][] origins = {{8, 8}, {0, 0}, {24, 8}, {8, 24}, {-8, -8}, {-24, 8}, {40, 8}, {8, -40}};
        List<String> rows = new ArrayList<>();
        List<String> dump = new ArrayList<>();
        for (int scene = 0; scene < origins.length; scene++) {
            int ox = origins[scene][0], oz = origins[scene][1], oy = 64;
            NativeHarness.Level level = NativeHarness.level((x, y, z) -> null);
            Object rng = NativeHarness.rng(0);
            boolean placed = NativeHarness.place("void_start_platform", level, rng, ox, oy, oz, CONFIG);
            List<String> cells = new ArrayList<>();
            for (Map.Entry<String, Object> e : level.writes.entrySet()) {
                String[] p = e.getKey().split(",");
                cells.add((Integer.parseInt(p[0]) - ox) + "," + (Integer.parseInt(p[1]) - oy) + ","
                        + (Integer.parseInt(p[2]) - oz) + " " + NativeHarness.nameOf(e.getValue()) + "\n");
            }
            Collections.sort(cells);
            for (String c : cells) dump.add(scene + "\t" + c.trim());
            String digest = HexFormat.of().formatHex(MessageDigest.getInstance("MD5")
                    .digest(String.join("", cells).getBytes(StandardCharsets.UTF_8)));
            rows.add(scene + "\t" + ox + "," + oy + "," + oz + "\t" + placed + "\t" + cells.size()
                    + "\t" + NativeHarness.rngState(rng) + "\t" + digest);
        }
        Files.write(Path.of(args[0]), rows);
        if (args.length > 1) Files.write(Path.of(args[1]), dump);
        System.out.println("native void_start_platform cases: " + rows.size());
    }
}
