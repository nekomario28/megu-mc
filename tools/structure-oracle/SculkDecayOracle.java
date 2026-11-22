package dev.megu.tools;

import java.lang.reflect.Method;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Path;

/** Run SculkBlock.getDecayPenalty itself; do not transcribe its arithmetic. */
public final class SculkDecayOracle {
    public static void main(String[] args) throws Exception {
        if (args.length != 1) throw new IllegalArgumentException("output TSV path required");
        NativeHarness.bootstrap();
        Class<?> spreaderClass = Class.forName("dmu");
        Class<?> posClass = Class.forName("jd");
        Object spreader = spreaderClass.getMethod("b").invoke(null);
        var position = posClass.getConstructor(int.class, int.class, int.class);
        Object origin = position.newInstance(0, 0, 0);
        Method penalty = Class.forName("dmq").getDeclaredMethod(
                "a", spreaderClass, posClass, posClass, int.class);
        penalty.setAccessible(true);
        int[][] points = {{0,0,0}, {1,0,0}, {2,0,0}, {4,0,0}, {6,0,0},
                {0,4,0}, {-4,-2,3}, {-5,-2,2}, {8,0,0}, {12,0,0},
                {23,0,0}, {24,0,0}, {25,0,0}, {20,10,-5}};
        try (var out = Files.newBufferedWriter(Path.of(args[0]), StandardCharsets.UTF_8)) {
            for (int[] p : points) {
                for (int charge : new int[]{1, 10, 16, 32, 100, 1000}) {
                    Object pos = position.newInstance(p[0], p[1], p[2]);
                    int result = (int) penalty.invoke(null, spreader, pos, origin, charge);
                    out.write(p[0]+"\t"+p[1]+"\t"+p[2]+"\t"+charge+"\t"+result+"\n");
                }
            }
        }
    }
}
