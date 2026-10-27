package dev.mcspine.tools;

import java.nio.charset.StandardCharsets;
import java.nio.file.*;
import java.security.MessageDigest;
import java.util.*;

/**
 * Actual 1.21.1 `minecraft:huge_red_mushroom` (`ecp` over `ebg`, config
 * `{cap_provider, stem_provider, foliage_radius}`), with the pack's own values
 * (`foliage_radius` 2 and a stem whose `up` is false).
 *
 * Two measured differences from the brown cap drive the scenes:
 * - the cap is four layers `height-3 ..= height`, so a cell at `dy == 5` can be inside
 *   it (scene 4 puts an obstruction there);
 * - `getTreeRadiusForHeight(-1, -1, r, dy)` is 0 for every `dy >= 0` for this species,
 *   so the position check covers the centre column only (scene 4's obstruction is
 *   off-centre: the native still grows the mushroom and then skips that cell).
 *
 * Cells are recorded as `minecraft:block[prop=value,...]` with the properties sorted,
 * because `up` (two top layers) and the four side properties are the part of this
 * feature a name-only fixture cannot see.
 */
public final class HugeRedMushroomOracle {
    static String provider(String name, String up) {
        return "{\"type\":\"minecraft:simple_state_provider\",\"state\":{\"Name\":\"" + name
                + "\",\"Properties\":{\"down\":\"false\",\"east\":\"true\",\"north\":\"true\","
                + "\"south\":\"true\",\"up\":\"" + up + "\",\"west\":\"true\"}}}";
    }

    static final String CONFIG = "{\"cap_provider\":"
            + provider("minecraft:red_mushroom_block", "true")
            + ",\"foliage_radius\":2,\"stem_provider\":"
            + provider("minecraft:mushroom_stem", "false") + "}";

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
                // leaves everywhere the cap can land: the position check accepts them and
                // the cap overwrites them (leaves are not solid-render)
                return dy <= -1 ? "minecraft:dirt"
                        : (dy >= 4 && dy <= 8 && Math.abs(x) <= 2 && Math.abs(z) <= 2
                                ? "minecraft:oak_leaves" : null);
            case 4:
                // an obstruction in the cap band but *off* the centre column
                return dy <= -1 ? "minecraft:dirt"
                        : (dy == 5 && Math.abs(x) <= 1 && Math.abs(z) <= 1 && (x != 0 || z != 0)
                                ? "minecraft:stone" : null);
            default:
                // a ceiling above every undoubled cap: only a doubled height reaches it
                return dy <= -1 ? "minecraft:dirt" : (dy >= 9 ? "minecraft:stone" : null);
        }
    }

    public static void main(String[] args) throws Exception {
        NativeHarness.bootstrap();
        Class<?> cls = NativeHarness.featureClass("huge_red_mushroom");
        if (!cls.getName().equals("ecp")) throw new AssertionError("class changed: " + cls);
        List<String> rows = new ArrayList<>();
        List<String> cellRows = new ArrayList<>();
        for (int scene = 0; scene < 6; scene++) {
            for (long seed : new long[]{0, 1, -1, 42, 60007}) {
                rows.add(run(scene, 64, seed, cellRows));
            }
        }
        rows.add(run(0, NativeHarness.MIN_Y + 1, 0, cellRows));
        rows.add(run(0, NativeHarness.MIN_Y, 0, cellRows));
        rows.add(run(0, NativeHarness.MIN_Y + NativeHarness.HEIGHT - 2, 0, cellRows));
        Files.write(Path.of(args[0]), rows);
        if (args.length > 1) Files.write(Path.of(args[1]), cellRows);
        System.out.println("native huge_red_mushroom cases: " + rows.size());
    }

    static String run(int scene, int oy, long seed, List<String> cellRows) throws Exception {
        final int sc = scene;
        final int origin = oy;
        NativeHarness.Level level = NativeHarness.level((x, y, z) -> scene(sc, x, y - origin + 64, z));
        Object rng = NativeHarness.rng(seed);
        boolean placed = NativeHarness.place("huge_red_mushroom", level, rng, 0, oy, 0, CONFIG);
        List<String> cells = new ArrayList<>();
        for (Map.Entry<String, Object> e : level.writes.entrySet()) {
            String[] p = e.getKey().split(",");
            cells.add((Integer.parseInt(p[0])) + "," + (Integer.parseInt(p[1]) - oy) + ","
                    + (Integer.parseInt(p[2])) + " " + NativeHarness.normalizedStateName(e.getValue()) + "\n");
        }
        Collections.sort(cells);
        String digest = HexFormat.of().formatHex(MessageDigest.getInstance("MD5")
                .digest(String.join("", cells).getBytes(StandardCharsets.UTF_8)));
        for (String cell : cells) {
            cellRows.add(scene + "\t" + oy + "\t" + seed + "\t" + cell.stripTrailing());
        }
        return scene + "\t" + oy + "\t" + seed + "\t" + placed + "\t" + cells.size() + "\t" + digest;
    }
}
