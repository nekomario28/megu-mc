package dev.megu.tools;

import java.lang.reflect.Method;
import java.util.ArrayList;
import java.util.Collection;
import java.util.HashMap;
import java.util.HashSet;
import java.util.List;
import java.util.Map;
import java.util.Optional;
import java.util.Set;
import java.util.function.Function;

/** Load the pinned vanilla built-in fluid tags, rather than treating them as empty.
 * Same native tag-loading path used by VanillaBlockTags.load for BLOCK.
 * All types and methods are the official 1.21.1 server JAR's own classes.
 */
public final class NativeFluidTags {
    private NativeFluidTags() {}
    @SuppressWarnings("unchecked")
    public static void load() throws Exception {
        Object registry = NativeHarness.C("lt").getField("c").get(null);
        Class<?> regClass = NativeHarness.C("jz");
        Class<?> keyClass = NativeHarness.C("akq");
        Object registryKey = regClass.getMethod("d").invoke(registry);
        Object pack = NativeHarness.C("ats").getMethod("b").invoke(null);
        Object manager = NativeHarness.C("atx")
                .getConstructor(NativeHarness.C("ass"), List.class)
                .newInstance(NativeHarness.C("ass").getField("b").get(null), List.of(pack));
        try {
            Method holder = regClass.getMethod("c", NativeHarness.C("akr"));
            Function<Object, Optional<?>> resolve = id -> {
                try { return (Optional<?>) holder.invoke(registry, id); }
                catch (ReflectiveOperationException e) { throw new IllegalStateException(e); }
            };
            String directory = (String) NativeHarness.C("lu").getMethod("d", keyClass)
                    .invoke(null, registryKey);
            Object loader = NativeHarness.C("awv")
                    .getConstructor(Function.class, String.class)
                    .newInstance(resolve, directory);
            Map<Object, Object> definitions = (Map<Object, Object>)
                    NativeHarness.C("awv").getMethod("a", NativeHarness.C("aue"))
                            .invoke(loader, manager);
            Map<Object, Collection<?>> loaded = (Map<Object, Collection<?>>)
                    NativeHarness.C("awv").getMethod("a", Map.class)
                            .invoke(loader, definitions);
            Set<Object> unresolved = new HashSet<>(definitions.keySet());
            unresolved.removeAll(loaded.keySet());
            if (!unresolved.isEmpty() || loaded.isEmpty())
                throw new AssertionError("native fluid tags missing/unresolved " + unresolved);
            Object water = NativeHarness.C("akr").getMethod("a", String.class)
                    .invoke(null, "minecraft:water");
            Object lava = NativeHarness.C("akr").getMethod("a", String.class)
                    .invoke(null, "minecraft:lava");
            if (!loaded.containsKey(water) || !loaded.containsKey(lava)
                    || loaded.get(water).isEmpty() || loaded.get(lava).isEmpty())
                throw new AssertionError("required water/lava native tags missing or empty");
            Map<Object, List<?>> bound = new HashMap<>();
            Method makeKey = NativeHarness.C("awu")
                    .getMethod("a", keyClass, NativeHarness.C("akr"));
            for (var entry : loaded.entrySet()) {
                bound.put(makeKey.invoke(null, registryKey, entry.getKey()),
                        List.copyOf(entry.getValue()));
            }
            regClass.getMethod("n").invoke(registry);
            regClass.getMethod("a", Map.class).invoke(registry, bound);
            System.out.println("native fluid tags: " + loaded.size()
                    + " loaded, 0 unresolved");
        } finally {
            ((AutoCloseable) manager).close();
        }
    }
}
