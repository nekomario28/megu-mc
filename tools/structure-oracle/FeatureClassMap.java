package dev.mcspine.tools;

import java.lang.reflect.*;
import java.nio.charset.StandardCharsets;
import java.nio.file.*;
import java.util.*;

/**
 * Which actual 1.21.1 class implements each configured-feature type.
 *
 * The datapack JSON gives a type name ("minecraft:netherrack_replace_blobs");
 * the port needs the class whose body it must match. The name-to-class table is
 * read out of the live `BuiltInRegistries.FEATURE` registry instead of being
 * assumed from a decompiler dump, so a renamed or re-based class is visible.
 */
public final class FeatureClassMap {
    public static void main(String[] args) throws Exception {
        Class<?> rlC = Class.forName("akr");
        Class.forName("ab").getMethod("a").invoke(null);
        Class.forName("akt").getMethod("a").invoke(null);
        Method parse = rlC.getMethod("a", String.class);

        List<String> types = Files.readAllLines(Path.of(args[0]), StandardCharsets.UTF_8);
        Object registry = featureRegistry(rlC, parse);
        Method value = null;
        for (Method m : registry.getClass().getMethods()) {
            if (m.getName().equals("a") && m.getParameterCount() == 1
                    && m.getParameterTypes()[0] == rlC) {
                value = m;
                break;
            }
        }
        if (value == null) throw new AssertionError("registry lookup method not found");
        List<String> rows = new ArrayList<>();
        for (String type : types) {
            if (type.isBlank()) continue;
            Object feature;
            try {
                feature = value.invoke(registry, parse.invoke(null, "minecraft:" + type));
            } catch (InvocationTargetException e) {
                feature = null;
            }
            if (feature == null) {
                rows.add(type + "\t<unregistered>\t<unregistered>");
                continue;
            }
            Class<?> cls = feature.getClass();
            StringBuilder chain = new StringBuilder(cls.getName());
            for (Class<?> c = cls.getSuperclass(); c != null && c != Object.class; c = c.getSuperclass()) {
                chain.append('>').append(c.getName());
            }
            rows.add(type + "\t" + cls.getName() + "\t" + chain);
        }
        Files.write(Path.of(args[1]), rows);
        System.out.println("feature type classes: " + rows.size());
    }

    /** The `BuiltInRegistries` field whose one-arg lookup resolves feature types. */
    private static Object featureRegistry(Class<?> rlC, Method parse) throws Exception {
        Object probeKey = parse.invoke(null, "minecraft:ore");
        Object probeKey2 = parse.invoke(null, "minecraft:huge_fungus");
        for (Field f : Class.forName("lt").getFields()) {
            if (!Modifier.isStatic(f.getModifiers())) continue;
            Object v;
            try {
                v = f.get(null);
            } catch (Throwable t) {
                continue;
            }
            if (v == null) continue;
            for (Method m : v.getClass().getMethods()) {
                if (!m.getName().equals("a") || m.getParameterCount() != 1
                        || m.getParameterTypes()[0] != rlC) {
                    continue;
                }
                Object a, b;
                try {
                    a = m.invoke(v, probeKey);
                    b = m.invoke(v, probeKey2);
                } catch (Throwable t) {
                    continue;
                }
                if (a == null || b == null) continue;
                // Two different feature types must resolve to two different
                // Feature subclasses, which is what distinguishes FEATURE from
                // a registry that happens to contain one of the words.
                if (a.getClass() != b.getClass()
                        && a.getClass().getSuperclass() == b.getClass().getSuperclass()) {
                    return v;
                }
            }
        }
        throw new AssertionError("FEATURE registry field not identified");
    }
}
