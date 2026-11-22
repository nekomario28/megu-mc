package dev.megu.tools;

import java.lang.reflect.*;
import java.nio.charset.StandardCharsets;
import java.nio.file.*;
import java.util.*;

/**
 * `ji` (Direction) constant name -> real direction name, for reading bytecode.
 *
 * Feature bodies move and offset by `ji.a`, `ji.b`, `ji.c`... Assuming that
 * `ji.a` is DOWN because it is first would silently mirror a column. The table
 * is read from the enum's own names so a port's offsets are not guesses.
 */
public final class EnumFieldsMap {
    public static void main(String[] args) throws Exception {
        Class<?> dir = Class.forName("ji");
        Method name = null;
        for (Method m : dir.getMethods()) {
            if (m.getParameterCount() == 0 && m.getReturnType() == String.class
                    && !m.getName().equals("toString")) {
                name = m;
                break;
            }
        }
        if (name == null) throw new AssertionError("direction name accessor not found");
        List<String> rows = new ArrayList<>();
        for (Field f : dir.getFields()) {
            if (!Modifier.isStatic(f.getModifiers()) || f.getType() != dir) continue;
            Object v = f.get(null);
            if (v == null) continue;
            rows.add(f.getName() + '\t' + name.invoke(v) + '\t' + v);
        }
        Collections.sort(rows);
        Files.write(Path.of(args[0]), rows, StandardCharsets.UTF_8);
        System.out.println("direction constants: " + rows.size());
    }
}
