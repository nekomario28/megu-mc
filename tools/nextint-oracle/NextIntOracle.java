// Recompute the nextInt(bound) fixtures from the pinned 1.21.1 random classes.
// No server, world or constructor bypass; the tsv is a request, not trusted output.
package dev.megu.tools;

import java.lang.reflect.*;
import java.nio.file.*;
import java.util.*;

public final class NextIntOracle {
    static final long FNV = 0xcbf29ce484222325L;
    static long fnv(long h, int x) {
        for (int b = 0; b < 4; b++) { h ^= (x >>> (8 * b)) & 0xff; h *= 0x100000001b3L; }
        return h;
    }
    public static void main(String[] args) throws Exception {
        Class<?> rand = Class.forName("ayw");
        Class<?> xo = Class.forName("dzz");
        Class<?> wg = Class.forName("dzx");
        Class<?> legacy = Class.forName("dyz");
        Constructor<?> xoCtor = xo.getConstructor(long.class);
        Constructor<?> wgCtor = wg.getConstructor(rand);
        Constructor<?> legacyCtor = legacy.getConstructor(long.class);
        Method nextInt = rand.getMethod("a", int.class);
        Method nextLong = rand.getMethod("g");
        int checked = 0, failed = 0;
        for (String line : Files.readAllLines(Path.of(args[0]))) {
            if (line.isBlank()) continue;
            String[] f = line.split("\t");
            if (f.length != 6) throw new IllegalStateException("bad row " + line);
            String kind = f[0];
            long seed = Long.parseLong(f[1]);
            int bound = Integer.parseInt(f[2]);
            int draws = Integer.parseInt(f[3]);
            long wantHash = Long.parseUnsignedLong(f[4]);
            long wantAfter = Long.parseLong(f[5]);
            Object r = kind.equals("xoroshiro") ? xoCtor.newInstance(seed)
                : kind.equals("worldgen") ? wgCtor.newInstance(xoCtor.newInstance(seed))
                : legacyCtor.newInstance(seed);
            long h = FNV;
            for (int i = 0; i < draws; i++) h = fnv(h, (Integer) nextInt.invoke(r, bound));
            long after = (Long) nextLong.invoke(r);
            checked++;
            if (h != wantHash || after != wantAfter) {
                failed++;
                if (failed <= 5) System.out.println("MISMATCH " + kind + " seed " + seed + " bound " + bound
                    + " hash " + Long.toUnsignedString(h) + " want " + Long.toUnsignedString(wantHash));
            }
        }
        System.out.println("nextInt oracle: " + checked + " rows, " + failed + " mismatches");
        if (failed != 0) System.exit(1);
    }
}
