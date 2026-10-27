package dev.mcspine.tools;

import com.google.gson.*;
import java.nio.charset.StandardCharsets;
import java.nio.file.*;
import java.security.MessageDigest;
import java.util.*;

/**
 * Data-driven native feature oracle: one cases file drives the pinned 1.21.1 feature class.
 *
 * The per-type oracles in this directory spell out their scenes as Java. That is fine for a
 * feature with a shape worth writing down, but it costs ~90 lines per type and the same lines
 * again on the Rust side. This oracle takes the scenes as data instead, so a new type is a
 * cases file (type, config, scene plies, seeds, origins) and one arm in `p_cf`.
 *
 * Usage: `FeatureOracle <cases.json> <out.tsv> [cells.tsv]`. The runner
 * (`tools/feature_oracles.py --cases <file>`) supplies these paths; `feature_type` is asserted
 * against the registered class so a registry change cannot silently compare the wrong feature.
 *
 * Cases schema (all coordinates are offsets from the feature origin; `dy = 0` is the origin's
 * own layer, and a ply's ranges are inclusive):
 * <pre>
 * {
 *   "type": "minecraft:pointed_dripstone",
 *   "class": "eda",
 *   "config": { ...the configured feature's config object... },
 *   "scenes": [
 *     {"name": "air", "plies": []},
 *     {"name": "floor", "plies": [
 *        {"block": "minecraft:stone", "dx": [-3, 3], "dy": [-1, -1], "dz": [-3, 3]},
 *        {"block": {"Name": "minecraft:pointed_dripstone",
 *                   "Properties": {"thickness": "tip", "vertical_direction": "up"}},
 *         "dx": [0, 0], "dy": [0, 0], "dz": [0, 0]}]}
 *   ],
 *   "seeds": [0, 1, -1, 42, 60007],
 *   "oy": 64,
 *   "extra": [{"scene": "air", "oy": -63, "seed": 0}]
 * }
 * </pre>
 *
 * A cell matching no ply is air; later plies overwrite earlier ones. The row format is
 * `scene \t oy \t seed \t wrote \t cells \t md5 \t rngProbe`. `wrote` is
 * `place(...) && cells > 0`, not the feature's own return value: vanilla's coral shapes
 * report success after a piece fails (`return true` from the trunk loop), so a raw boolean
 * records a fact the Rust port has no equivalent for - the port's `Cf` dispatch returns no
 * success flag. What both sides can state is whether the placement produced cells.
 * `rngProbe` is three
 * `nextInt(1000)` draws taken *after* the placement: it pins the number of draws a feature
 * consumed even when the drawn value does not reach a block (the coral features' `{}` config
 * is exactly that case), and any extra or missing draw shifts it.
 */
public final class FeatureOracle {
    /** A block-state ply: a block plus the inclusive ranges it fills. */
    record Ply(Object state, int[] dx, int[] dy, int[] dz) {}

    public static void main(String[] args) throws Exception {
        NativeHarness.bootstrap();
        JsonObject cases = JsonParser.parseString(Files.readString(Path.of(args[0]))).getAsJsonObject();
        String type = cases.get("type").getAsString().replace("minecraft:", "");
        Class<?> want = NativeHarness.featureClass(type);
        String expect = cases.has("class") ? cases.get("class").getAsString() : null;
        if (expect != null && !want.getName().equals(expect)) {
            throw new AssertionError(type + " is registered as " + want.getName() + ", not " + expect);
        }
        String config = cases.get("config").toString();

        Map<String, List<Ply>> scenes = new LinkedHashMap<>();
        for (JsonElement e : cases.getAsJsonArray("scenes")) {
            JsonObject scene = e.getAsJsonObject();
            List<Ply> plies = new ArrayList<>();
            for (JsonElement p : scene.getAsJsonArray("plies")) {
                JsonObject ply = p.getAsJsonObject();
                plies.add(new Ply(
                        blockState(ply.get("block")),
                        range(ply, "dx"),
                        range(ply, "dy"),
                        range(ply, "dz")));
            }
            scenes.put(scene.get("name").getAsString(), plies);
        }

        List<String> rows = new ArrayList<>();
        List<String> cellRows = new ArrayList<>();
        int oy = cases.has("oy") ? cases.get("oy").getAsInt() : 64;
        for (Map.Entry<String, List<Ply>> scene : scenes.entrySet()) {
            for (JsonElement seed : cases.getAsJsonArray("seeds")) {
                rows.add(run(type, config, scene.getKey(), scene.getValue(), oy, seed.getAsLong(), cellRows));
            }
        }
        if (cases.has("extra")) {
            for (JsonElement e : cases.getAsJsonArray("extra")) {
                JsonObject ex = e.getAsJsonObject();
                String name = ex.get("scene").getAsString();
                List<Ply> plies = scenes.get(name);
                if (plies == null) throw new AssertionError("extra names an unknown scene: " + name);
                rows.add(run(type, config, name, plies, ex.get("oy").getAsInt(),
                        ex.get("seed").getAsLong(), cellRows));
            }
        }
        Files.write(Path.of(args[1]), rows);
        if (args.length > 2) Files.write(Path.of(args[2]), cellRows);
        System.out.println(type + " generic cases: " + rows.size());
    }

    /** A ply's block: a block id, a datapack state object, or null for air. */
    static Object blockState(JsonElement block) throws Exception {
        if (block == null || block.isJsonNull()) return null;
        if (block.isJsonPrimitive()) return NativeHarness.state(block.getAsString());
        return NativeHarness.stateFromJson(block);
    }

    static int[] range(JsonObject ply, String key) {
        JsonArray a = ply.getAsJsonArray(key);
        return new int[]{a.get(0).getAsInt(), a.get(1).getAsInt()};
    }

    static String run(String type, String config, String sceneName, List<Ply> plies, int oy,
                      long seed, List<String> cellRows) throws Exception {
        NativeHarness.Level level =
                NativeHarness.levelFromStates((x, y, z) -> blockAt(plies, x, y - oy, z));
        Object rng = NativeHarness.rng(seed);
        boolean placed = NativeHarness.place(type, level, rng, 0, oy, 0, config);
        List<String> cells = new ArrayList<>();
        for (Map.Entry<String, Object> e : level.writes.entrySet()) {
            String[] p = e.getKey().split(",");
            cells.add((Integer.parseInt(p[0])) + "," + (Integer.parseInt(p[1]) - oy) + ","
                    + (Integer.parseInt(p[2])) + " " + NativeHarness.normalizedStateName(e.getValue()) + "\n");
        }
        Collections.sort(cells);
        String digest = HexFormat.of().formatHex(MessageDigest.getInstance("MD5")
                .digest(String.join("", cells).getBytes(StandardCharsets.UTF_8)));
        // three post-placement draws: the stream position, expressed as draws the Rust side
        // can reproduce with the same `nextInt(bound)` it already has
        String probe = NativeHarness.draw(rng, 1000) + "," + NativeHarness.draw(rng, 1000)
                + "," + NativeHarness.draw(rng, 1000);
        for (String cell : cells) {
            cellRows.add(sceneName + "\t" + oy + "\t" + seed + "\t" + cell.stripTrailing());
        }
        boolean wrote = placed && !cells.isEmpty();
        return sceneName + "\t" + oy + "\t" + seed + "\t" + wrote + "\t" + cells.size()
                + "\t" + digest + "\t" + probe;
    }

    /** The block state at an origin-relative offset, or null (air) when no ply covers it. */
    static Object blockAt(List<Ply> plies, int x, int dy, int z) {
        Object state = null;
        for (Ply ply : plies) { // later plies overwrite earlier ones
            if (x >= ply.dx()[0] && x <= ply.dx()[1]
                    && dy >= ply.dy()[0] && dy <= ply.dy()[1]
                    && z >= ply.dz()[0] && z <= ply.dz()[1]) {
                state = ply.state();
            }
        }
        return state;
    }
}
