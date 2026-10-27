package dev.mcspine.tools;

import java.lang.reflect.*;
import java.nio.charset.StandardCharsets;
import java.nio.file.*;
import java.util.*;

/**
 * `dga` field name -> block id, for reading the pinned bytecode with real names.
 *
 * The decompiled native classes refer to blocks as `dga.H`, `dga.dY`, ... A port
 * that guesses which field is netherrack cannot be checked against the class it
 * claims to match, so the table is read out of the live registry.
 */
public final class BlockFieldsMap {
    public static void main(String[] args) throws Exception {
        Class.forName("ab").getMethod("a").invoke(null);
        Class.forName("akt").getMethod("a").invoke(null);
        Class<?> blockC = Class.forName("dfy");
        Object blocks = Class.forName("lt").getField("e").get(null);
        // Registry.getKey(T) -> ResourceLocation
        Method key = null;
        for (Method m : blocks.getClass().getMethods()) {
            if (m.getParameterCount() != 1) continue;
            if (!m.getReturnType().getName().equals("akr")) continue;
            if (m.getParameterTypes()[0].isAssignableFrom(blockC)) { key = m; break; }
        }
        if (key == null) throw new AssertionError("registry getKey not found");
        Method path = Class.forName("akr").getMethod("a");
        List<String> rows = new ArrayList<>();
        for (Field f : Class.forName("dga").getFields()) {
            if (!Modifier.isStatic(f.getModifiers()) || f.getType() != blockC) continue;
            Object block = f.get(null);
            if (block == null) continue;
            Object id;
            try {
                id = key.invoke(blocks, block);
            } catch (InvocationTargetException e) {
                continue;
            }
            rows.add(f.getName() + "\t" + path.invoke(id));
        }
        Collections.sort(rows);
        Files.write(Path.of(args[0]), rows, StandardCharsets.UTF_8);
        System.out.println("block fields: " + rows.size());
    }
}
