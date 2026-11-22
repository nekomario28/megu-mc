// Evaluate actual 1.21.1 SurfaceRules conditions against synthetic contexts.
// No chunk/IWorld execution: this pins the condition formulas and field mapping.
package dev.megu.tools;

import com.google.gson.*;
import java.lang.reflect.*;
import java.nio.file.*;
import java.util.*;

public final class SurfaceConditionOracle {
    static Class<?> cls(String n) throws Exception { return Class.forName(n); }
    static void set(Object o, String n, Object v) throws Exception {
        Field f = o.getClass().getDeclaredField(n); f.setAccessible(true); f.set(o, v);
    }
    static Object newContext(Object worldCtx) throws Exception {
        Class<?> c = cls("dzp$g");
        Constructor<?> ctor = c.getDeclaredConstructors()[0]; ctor.setAccessible(true);
        Object ctx = ctor.newInstance(new Object[]{null, null, null, null, null, null, worldCtx});
        // Make lazy caches return the explicitly supplied synthetic values.
        set(ctx, "q", 0L); set(ctx, "u", 0L); set(ctx, "w", 0L);
        return ctx;
    }
    static boolean test(Object source, Object ctx) throws Exception {
        Method apply = source.getClass().getMethod("apply", Object.class);
        apply.setAccessible(true);
        if (apply == null) throw new NoSuchMethodException("ConditionSource.apply");
        Object cond = apply.invoke(source, ctx);
        Method test = cond.getClass().getMethod("b");
        test.setAccessible(true);
        return (Boolean) test.invoke(cond);
    }
    static Object source(JsonObject r) throws Exception {
        String kind = r.get("kind").getAsString();
        Class<?> rules = cls("dzp");
        Class<?> anchor = cls("dzs");
        Class<?> cave = cls("eil");
        switch (kind) {
            case "stone": {
                boolean ceiling = r.get("ceiling").getAsBoolean();
                Object surface = cave.getField(ceiling ? "a" : "b").get(null);
                return rules.getMethod("a", int.class, boolean.class, int.class, cave)
                    .invoke(null, r.get("offset").getAsInt(), r.get("add").getAsBoolean(),
                        r.get("secondary").getAsInt(), surface);
            }
            case "water": {
                Method m = rules.getMethod("a", int.class, int.class);
                if (r.get("start").getAsBoolean()) m = rules.getMethod("b", int.class, int.class);
                return m.invoke(null, r.get("offset").getAsInt(), r.get("mult").getAsInt());
            }
            case "hole": return rules.getMethod("b").invoke(null);
            case "prelim": return rules.getMethod("c").invoke(null);
            case "y": {
                String a = r.get("anchor").getAsString();
                int v = r.get("value").getAsInt();
                Object va = a.equals("absolute") ? anchor.getMethod("a", int.class).invoke(null, v)
                    : a.equals("above_bottom") ? anchor.getMethod("b", int.class).invoke(null, v)
                    : anchor.getMethod("c", int.class).invoke(null, v);
                Method m = r.get("start").getAsBoolean() ? rules.getMethod("b", anchor, int.class)
                    : rules.getMethod("a", anchor, int.class);
                return m.invoke(null, va, r.get("offset").getAsInt());
            }
            default: throw new IllegalArgumentException(kind);
        }
    }
    public static void main(String[] args) throws Exception {
        cls("ab").getMethod("a").invoke(null);
        cls("akt").getMethod("a").invoke(null);
        JsonArray out = new JsonArray();
        int[] ys = {0, 120};
        int[] depths = {-1, 0, 1};
        int[] stones = {0, 4};
        int[] secs = {-1, 1};
        int[] waters = {Integer.MIN_VALUE, 63};
        int[] mins = {0};
        int[] offs = {-1, 0, 1};
        int[] mults = {-1, 0, 2};
        int id = 0;
        for (int y : ys) for (int depth : depths) for (int above : stones) for (int below : stones)
        for (int sec : secs) for (int water : waters) for (int min : mins) {
            for (boolean ceiling : new boolean[]{false, true}) for (int off : offs)
            for (boolean add : new boolean[]{false, true}) for (int sr : new int[]{0, 5}) {
                JsonObject r = new JsonObject();
                r.addProperty("id", "sc-" + (id++)); r.addProperty("kind", "stone");
                r.addProperty("offset", off); r.addProperty("add", add);
                r.addProperty("secondary", sr); r.addProperty("ceiling", ceiling);
                r.addProperty("y", y); r.addProperty("surface_depth", depth);
                r.addProperty("stone_above", above); r.addProperty("stone_below", below);
                r.addProperty("water_height", water); r.addProperty("min_surface", min);
                r.addProperty("surface_secondary", (sec + 1) * 0.25);
                Object ctx = newContext(null);
                set(ctx, "r", 5); set(ctx, "s", 7); set(ctx, "t", depth); set(ctx, "x", min);
                set(ctx, "B", y); set(ctx, "C", water); set(ctx, "D", below); set(ctx, "E", above);
                set(ctx, "v", (sec + 1) * 0.25);
                r.addProperty("expected", test(source(r), ctx));
                out.add(r);
            }
            for (boolean start : new boolean[]{false, true}) for (int off : offs) for (int mult : mults) {
                JsonObject r = new JsonObject();
                r.addProperty("id", "sc-" + (id++)); r.addProperty("kind", "water");
                r.addProperty("offset", off); r.addProperty("mult", mult); r.addProperty("start", start);
                r.addProperty("y", y); r.addProperty("surface_depth", depth);
                r.addProperty("stone_above", above); r.addProperty("stone_below", below);
                r.addProperty("water_height", water); r.addProperty("min_surface", min);
                Object ctx = newContext(null);
                set(ctx, "r", 5); set(ctx, "s", 7); set(ctx, "t", depth); set(ctx, "x", min);
                set(ctx, "B", y); set(ctx, "C", water); set(ctx, "D", below); set(ctx, "E", above);
                r.addProperty("expected", test(source(r), ctx));
                out.add(r);
            }
        }
        for (int depth : depths) {
            JsonObject r = new JsonObject(); r.addProperty("id", "sc-" + (id++)); r.addProperty("kind", "hole");
            r.addProperty("surface_depth", depth);
            Object ctx = newContext(null); set(ctx, "t", depth);
            r.addProperty("expected", test(source(r), ctx)); out.add(r);
        }
        for (int y : ys) for (int min : mins) {
            JsonObject r = new JsonObject(); r.addProperty("id", "sc-" + (id++)); r.addProperty("kind", "prelim");
            r.addProperty("y", y); r.addProperty("min_surface", min);
            Object ctx = newContext(null); set(ctx, "B", y); set(ctx, "x", min);
            r.addProperty("expected", test(source(r), ctx)); out.add(r);
        }
        JsonObject root = new JsonObject(); root.addProperty("rows", out.size()); root.add("cases", out);
        Files.writeString(Path.of(args[0]), root.toString());
        System.out.println("surface condition oracle: " + out.size() + " rows");
    }
}
