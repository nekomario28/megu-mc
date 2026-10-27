package dev.mcspine.tools;

import java.lang.reflect.*;
import java.nio.file.*;
import java.util.*;
import java.util.function.Function;

/** Invoke pinned 1.21.1 OreFeature.canPlaceOre, including its real neighbour scan.
 * Synthetic block lookup only; no server/world startup or method replacements.
 * Obfuscated names are resolved from the official 1.21.1 server mappings.
 */
public final class OreAirOracle {
    public static void main(String[] args) throws Exception {
        Class.forName("ab").getMethod("a").invoke(null); // SharedConstants
        Class.forName("akt").getMethod("a").invoke(null); // Bootstrap
        Class<?> blocks = Class.forName("dga"), block = Class.forName("dfy");
        Object stoneBlock = blocks.getField("b").get(null); // Blocks.STONE
        Object stone = block.getMethod("o").invoke(stoneBlock); // defaultBlockState
        Object air = block.getMethod("o").invoke(blocks.getField("a").get(null));
        Class<?> state = Class.forName("dtc"), random = Class.forName("ayw");
        Class<?> config = Class.forName("eeq"), target = Class.forName("eeq$a");
        Class<?> rule = Class.forName("enn"), pos = Class.forName("jd");
        Class<?> mutable = Class.forName("jd$a"), xor = Class.forName("dzz");
        Object test = Class.forName("emv").getConstructor(block).newInstance(stoneBlock);
        Object output = config.getMethod("a", rule, state).invoke(null, test, stone);
        Method canPlace = Class.forName("ecz").getMethod("a", state, Function.class,
                random, config, target, mutable);
        Method nextLong = random.getMethod("g");
        int[][] offsets = {{0,-1,0},{0,1,0},{0,0,-1},{0,0,1},{-1,0,0},{1,0,0}};
        List<String> rows = new ArrayList<>();
        for (long seed : new long[]{0, 1, 60007}) {
            for (float chance : new float[]{0, 0.5f, 1}) {
                for (int face = -1; face < 6; face++) {
                    Object exposed = face < 0 ? null : pos.getConstructor(int.class, int.class, int.class)
                            .newInstance(offsets[face][0], 16 + offsets[face][1], offsets[face][2]);
                    Function<Object,Object> lookup = p -> p.equals(exposed) ? air : stone;
                    for (boolean matches : new boolean[]{false, true}) {
                        Object rng = xor.getConstructor(long.class).newInstance(seed);
                        Object cfg = config.getConstructor(List.class, int.class, float.class)
                                .newInstance(List.of(output), 8, chance);
                        Object origin = mutable.getConstructor(int.class, int.class, int.class)
                                .newInstance(0, 16, 0);
                        boolean result = (boolean) canPlace.invoke(null, matches ? stone : air,
                                lookup, rng, cfg, output, origin);
                        rows.add(seed + "\t" + chance + "\t" + face + "\t" + matches
                                + "\t" + result + "\t" + nextLong.invoke(rng));
                    }
                }
            }
        }
        Files.write(Path.of(args[0]), rows);
        System.out.println("ore air oracle: " + rows.size() + " rows from OreFeature.canPlaceOre");
    }
}
