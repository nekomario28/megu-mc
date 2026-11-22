// Recompute the tie-heavy biome answers from the pinned 1.21.1 Climate tree.
// Reads the committed probe TSV; it does not trust the recorded answer.
package dev.megu.tools;

import java.lang.reflect.*;
import java.nio.file.*;
import java.util.*;

public final class BiomeTieOracle {
    public static void main(String[] args) throws Exception {
        Class.forName("ab").getMethod("a").invoke(null);
        Class.forName("akt").getMethod("a").invoke(null);
        Map<?, ?> presets = (Map<?, ?>) Class.forName("dek").getMethod("b").invoke(null);
        Object list = null;
        for (Map.Entry<?, ?> entry : presets.entrySet()) {
            String id = entry.getKey().getClass().getMethod("b").invoke(entry.getKey()).toString();
            if (id.equals("minecraft:overworld")) { list = entry.getValue(); break; }
        }
        if (list == null) throw new IllegalStateException("overworld preset not found");
        Class<?> targetC = Class.forName("def$h");
        Constructor<?> targetCtor = targetC.getConstructor(long.class, long.class, long.class,
            long.class, long.class, long.class);
        Method find = list.getClass().getMethod("a", targetC);
        Field treeField = list.getClass().getDeclaredField("b");
        treeField.setAccessible(true);
        Object tree = treeField.get(list);
        Field previousField = tree.getClass().getDeclaredField("c");
        previousField.setAccessible(true);
        ThreadLocal<?> previous = (ThreadLocal<?>) previousField.get(tree);
        int checked = 0, failed = 0;
        for (String line : Files.readAllLines(Path.of(args[0]))) {
            if (line.isBlank()) continue;
            String[] f = line.split("\t");
            if (f.length != 8) throw new IllegalStateException("bad row " + line);
            long[] v = new long[6];
            for (int i = 0; i < 6; i++) v[i] = Long.parseLong(f[i]);
            Object target = targetCtor.newInstance(v[0], v[1], v[2], v[3], v[4], v[5]);
            previous.remove();
            Object got = find.invoke(list, target);
            String name = got.getClass().getMethod("a").invoke(got).toString();
            checked++;
            if (!name.equals(f[6])) {
                failed++;
                if (failed <= 5) System.out.println("MISMATCH " + line + " got " + name);
            }
        }
        System.out.println("biome tie oracle: " + checked + " rows, " + failed + " mismatches");
        if (failed != 0) System.exit(1);
    }
}
