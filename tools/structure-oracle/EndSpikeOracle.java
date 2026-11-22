package dev.megu.tools;

import java.nio.charset.StandardCharsets;
import java.nio.file.*;
import java.security.MessageDigest;
import java.util.*;

/**
 * Actual 1.21.1 `minecraft:end_spike` (`edo`, `SpikeFeature`) native oracle.
 *
 * Drives the real `edo` class (including `edo$a`, `edo$b`, `efc`, `EndCrystal` creation via
 * `EntityType.END_CRYSTAL.create(level.getLevel())`, `IronBarsBlock` properties, `BaseFireBlock.getState`,
 * and `SpikeCacheLoader` seed shuffling) and records cell count, full-state SHA-256, spawned entity
 * count, and post-placement `nextInt(1000)` RNG probe.
 */
public final class EndSpikeOracle {
    static final String DEFAULT_CONFIG = "{\"crystal_invulnerable\":false,\"spikes\":[]}";

    public static void main(String[] args) throws Exception {
        NativeHarness.bootstrap();
        Class<?> cls = NativeHarness.featureClass("end_spike");
        if (!cls.getName().equals("edo")) {
            throw new AssertionError("class changed: " + cls);
        }
        Object serverLevel = NativeHarness.minimalEntityServerLevel();
        List<String> rows = new ArrayList<>();
        List<String> cellRows = args.length > 1 ? new ArrayList<>() : null;

        // 1. Default seed-derived spike cache (`spikes: []`) across all 10 spike positions for seed 0
        //    in End bounds (0..128) and Overworld bounds (-64..320).
        for (int i = 0; i < 10; i++) {
            double angle = 2.0 * (-Math.PI + (Math.PI / 10.0) * i);
            int cx = (int) Math.floor(42.0 * Math.cos(angle));
            int cz = (int) Math.floor(42.0 * Math.sin(angle));
            int ox = (cx >> 4) * 16;
            int oz = (cz >> 4) * 16;
            rows.add(run("default_end_s0_i" + i, 0, 128, 0L, ox, 0, oz, 100L + i, DEFAULT_CONFIG, serverLevel, cellRows));
        }
        // Non-corner origin inside spike chunk + empty chunk (0, 0)
        rows.add(run("default_end_s0_inchunk_offset", 0, 128, 0L, 42 & ~15 | 7, 15, 0 | 9, 42L, DEFAULT_CONFIG, serverLevel, cellRows));
        rows.add(run("default_end_s0_empty_chunk", 0, 128, 0L, 0, 0, 0, 42L, DEFAULT_CONFIG, serverLevel, cellRows));

        // Overworld bounds (-64..320) for a guarded & unguarded spike from worldSeed 0
        rows.add(run("default_ow_s0_i0", -64, 384, 0L, (42 >> 4) * 16, 0, 0, 7L, DEFAULT_CONFIG, serverLevel, cellRows));
        rows.add(run("default_ow_s0_i3", -64, 384, 0L, (int) Math.floor(42.0 * Math.cos(2.0 * (-Math.PI + 0.3 * Math.PI))) & ~15, 0, (int) Math.floor(42.0 * Math.sin(2.0 * (-Math.PI + 0.3 * Math.PI))) & ~15, 8L, DEFAULT_CONFIG, serverLevel, cellRows));

        // Other world seeds (1, -1, 42, 60007) testing different cache keys & permutations
        for (long wseed : new long[]{1L, -1L, 42L, 60007L}) {
            for (int i : new int[]{0, 2, 5, 7}) {
                double angle = 2.0 * (-Math.PI + (Math.PI / 10.0) * i);
                int ox = ((int) Math.floor(42.0 * Math.cos(angle)) >> 4) * 16;
                int oz = ((int) Math.floor(42.0 * Math.sin(angle)) >> 4) * 16;
                rows.add(run("default_end_ws" + wseed + "_i" + i, 0, 128, wseed, ox, 0, oz, wseed ^ i, DEFAULT_CONFIG, serverLevel, cellRows));
            }
        }

        // 2. Explicit spikes configs
        String explicitTwo = "{\"crystal_invulnerable\":true,\"crystal_beam_target\":[0,128,0],"
                + "\"spikes\":[{\"centerX\":2,\"centerZ\":2,\"radius\":2,\"height\":76,\"guarded\":true},"
                + "{\"centerX\":10,\"centerZ\":10,\"radius\":3,\"height\":80,\"guarded\":false}]}";
        rows.add(run("explicit_two_in_chunk", 0, 128, 0L, 0, 0, 0, 123L, explicitTwo, serverLevel, cellRows));
        rows.add(run("explicit_two_out_of_chunk", 0, 128, 0L, 16, 0, 0, 123L, explicitTwo, serverLevel, cellRows));

        String explicitClip125 = "{\"crystal_invulnerable\":false,\"spikes\":[{\"centerX\":0,\"centerZ\":0,\"radius\":3,\"height\":125,\"guarded\":true}]}";
        rows.add(run("explicit_clip_125", 0, 128, 0L, 0, 0, 0, 99L, explicitClip125, serverLevel, cellRows));

        String explicitClip127 = "{\"crystal_invulnerable\":false,\"spikes\":[{\"centerX\":0,\"centerZ\":0,\"radius\":2,\"height\":127,\"guarded\":false}]}";
        rows.add(run("explicit_clip_127", 0, 128, 0L, 0, 0, 0, 99L, explicitClip127, serverLevel, cellRows));

        String explicitLow60 = "{\"crystal_invulnerable\":false,\"spikes\":[{\"centerX\":0,\"centerZ\":0,\"radius\":3,\"height\":60,\"guarded\":false}]}";
        rows.add(run("explicit_low_60", 0, 128, 0L, 0, 0, 0, 55L, explicitLow60, serverLevel, cellRows));

        String explicitZero = "{\"crystal_invulnerable\":false,\"spikes\":[{\"centerX\":0,\"centerZ\":0,\"radius\":2,\"height\":0,\"guarded\":false}]}";
        rows.add(run("explicit_height_0", 0, 128, 0L, 0, 0, 0, 55L, explicitZero, serverLevel, cellRows));

        String explicitNeg1 = "{\"crystal_invulnerable\":false,\"spikes\":[{\"centerX\":0,\"centerZ\":0,\"radius\":2,\"height\":-1,\"guarded\":false}]}";
        rows.add(run("explicit_height_neg1", 0, 128, 0L, 0, 0, 0, 55L, explicitNeg1, serverLevel, cellRows));

        Files.write(Path.of(args[0]), rows);
        if (cellRows != null) {
            Files.write(Path.of(args[1]), cellRows);
        }
        System.out.println("native end_spike cases: " + rows.size());
    }

    static String run(String id, int minY, int height, long worldSeed, int ox, int oy, int oz,
                      long rngSeed, String configJson, Object serverLevel,
                      List<String> cellRows) throws Exception {
        NativeHarness.Level level = NativeHarness.levelFromStates((x, y, z) -> null, minY, height);
        level.worldSeed = worldSeed;
        level.entityServerLevel = serverLevel;
        Object rng = NativeHarness.rng(rngSeed);
        boolean placed = NativeHarness.place("end_spike", level, rng, ox, oy, oz, configJson);
        List<String> cells = new ArrayList<>();
        for (Map.Entry<String, Object> e : level.writes.entrySet()) {
            cells.add(e.getKey() + " " + NativeHarness.normalizedStateName(e.getValue()) + "\n");
        }
        Collections.sort(cells);
        String digest = HexFormat.of().formatHex(MessageDigest.getInstance("SHA-256")
                .digest(String.join("", cells).getBytes(StandardCharsets.UTF_8)));
        String probe = NativeHarness.draw(rng, 1000) + "," + NativeHarness.draw(rng, 1000)
                + "," + NativeHarness.draw(rng, 1000);
        if (cellRows != null) {
            for (String cell : cells) {
                cellRows.add(id + "\t" + cell.stripTrailing());
            }
        }
        boolean wrote = placed && !cells.isEmpty();
        return id + "\t" + minY + "\t" + height + "\t" + worldSeed + "\t"
                + ox + "," + oy + "," + oz + "\t" + rngSeed + "\t" + configJson + "\t"
                + wrote + "\t" + cells.size() + "\t" + digest + "\t" + probe + "\t"
                + level.entities.size();
    }
}
