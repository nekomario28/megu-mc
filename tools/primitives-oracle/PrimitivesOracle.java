package dev.mcspine.tools;

import com.google.gson.*;
import io.netty.buffer.*;
import java.io.*;
import java.lang.reflect.*;
import java.nio.file.*;
import java.security.MessageDigest;
import java.util.*;

/** Actual pinned container, StringTag and AABB methods, without a running world. */
public final class PrimitivesOracle {
    static Class<?> cls(String name) throws Exception { return Class.forName(name); }
    static String digest(byte[] bytes) throws Exception {
        return HexFormat.of().formatHex(MessageDigest.getInstance("SHA-1").digest(bytes));
    }
    static Object field(Object object, String name) throws Exception {
        Field f = object.getClass().getDeclaredField(name);
        f.setAccessible(true);
        return f.get(object);
    }
    static int[] integers(JsonArray array) {
        int[] out = new int[array.size()];
        for (int i = 0; i < out.length; i++) out[i] = array.get(i).getAsInt();
        return out;
    }
    static Object vec(JsonArray array) throws Exception {
        return cls("exc").getConstructor(double.class, double.class, double.class)
            .newInstance(array.get(0).getAsDouble(), array.get(1).getAsDouble(), array.get(2).getAsDouble());
    }
    static byte[] palette(Object registry, Object[] values, String strategy, int count) throws Exception {
        Class<?> container = cls("dvq"), strat = cls("dvq$d");
        Object instance = container.getConstructor(cls("jr"), Object.class, strat)
            .newInstance(registry, values[0], strat.getField(strategy).get(null));
        Method set = container.getDeclaredMethod("b", int.class, Object.class);
        set.setAccessible(true);
        for (int i = 0; i < count; i++) set.invoke(instance, i, values[i % values.length]);
        ByteBuf bytes = Unpooled.buffer();
        try {
            Object buf = cls("vw").getConstructor(ByteBuf.class).newInstance(bytes);
            container.getMethod("b", cls("vw")).invoke(instance, buf);
            byte[] result = new byte[bytes.readableBytes()];
            bytes.getBytes(0, result);
            return result;
        } finally { bytes.release(); }
    }
    static Object blockRegistry;
    static Object[] biomeValues;
    static Object biomeRegistry;
    static JsonObject section(JsonObject row) throws Exception {
        int[] states = integers(row.getAsJsonArray("states")), biomes = integers(row.getAsJsonArray("biomes"));
        Object[] blocks = new Object[states.length], bios = new Object[biomes.length];
        Method byId = cls("js").getMethod("a", int.class);
        int nonAir = 0;
        for (int i = 0; i < blocks.length; i++) {
            blocks[i] = byId.invoke(blockRegistry, states[i]);
            if (blocks[i] == null) throw new IllegalArgumentException("unknown block state");
        }
        for (int i = 0; i < 4096; i++)
            if (!(boolean) cls("dtc").getMethod("i").invoke(blocks[i % blocks.length])) nonAir++;
        for (int i = 0; i < bios.length; i++) bios[i] = biomeValues[biomes[i]];
        byte[] a = palette(blockRegistry, blocks, "d", 4096), b = palette(biomeRegistry, bios, "e", 64);
        ByteArrayOutputStream out = new ByteArrayOutputStream();
        new DataOutputStream(out).writeShort(nonAir);
        out.write(a); out.write(b);
        JsonObject expected = new JsonObject();
        expected.addProperty("sha1", digest(out.toByteArray()));
        expected.addProperty("length", out.size());
        expected.addProperty("non_air", nonAir);
        expected.addProperty("block_bits", a[0] & 255);
        expected.addProperty("biome_bits", b[0] & 255);
        return expected;
    }
    static JsonObject string(JsonObject row) throws Exception {
        StringBuilder text = new StringBuilder();
        int repeat = row.get("repeat").getAsInt();
        for (int i = 0; i < repeat; i++)
            for (int unit : integers(row.getAsJsonArray("units"))) text.append((char) unit);
        Object tag = cls("uw").getMethod("a", String.class).invoke(null, text.toString());
        ByteArrayOutputStream out = new ByteArrayOutputStream();
        cls("uw").getMethod("a", DataOutput.class).invoke(tag, new DataOutputStream(out));
        // Also exercise the JDK reader StringTag.load delegates to; keep raw UTF-16,
        // including isolated surrogates, rather than JSON-encoding a lossy String.
        String decoded = new DataInputStream(new ByteArrayInputStream(out.toByteArray())).readUTF();
        if (!decoded.equals(text.toString())) throw new AssertionError("readUTF mismatch");
        JsonObject expected = new JsonObject();
        expected.addProperty("sha1", digest(out.toByteArray()));
        expected.addProperty("length", out.size());
        if (out.size() < 100) expected.addProperty("hex", HexFormat.of().formatHex(out.toByteArray()));
        return expected;
    }
    static JsonObject clip(JsonObject row) throws Exception {
        JsonArray box = row.getAsJsonArray("box");
        Object aabb = cls("ewx").getConstructor(double.class, double.class, double.class,
            double.class, double.class, double.class).newInstance(box.get(0).getAsDouble(),
            box.get(1).getAsDouble(), box.get(2).getAsDouble(), box.get(3).getAsDouble(),
            box.get(4).getAsDouble(), box.get(5).getAsDouble());
        Optional<?> result = (Optional<?>) cls("ewx").getMethod("b", cls("exc"), cls("exc"))
            .invoke(aabb, vec(row.getAsJsonArray("start")), vec(row.getAsJsonArray("end")));
        JsonObject expected = new JsonObject();
        JsonArray bits = new JsonArray();
        if (result.isPresent()) for (String name : new String[]{"c", "d", "e"})
            bits.add(String.format("%016x", Double.doubleToRawLongBits((double) field(result.get(), name))));
        expected.add("hit_bits", bits);
        return expected;
    }
    public static void main(String[] args) throws Exception {
        cls("ab").getMethod("a").invoke(null);
        cls("akt").getMethod("a").invoke(null);
        blockRegistry = cls("dfy").getField("q").get(null);
        biomeRegistry = cls("js").getConstructor().newInstance();
        // Synthetic identity map: tests the real palette strategy, not biome content.
        biomeValues = new Object[64];
        for (int i = 0; i < 64; i++) {
            biomeValues[i] = new Object();
            cls("js").getMethod("a", Object.class, int.class).invoke(biomeRegistry, biomeValues[i], i);
        }
        JsonArray out = new JsonArray();
        JsonObject input = JsonParser.parseString(Files.readString(Path.of(args[0]))).getAsJsonObject();
        for (JsonElement element : input.getAsJsonArray("cases")) {
            JsonObject row = element.getAsJsonObject().deepCopy();
            row.add("expected", switch (row.get("kind").getAsString()) {
                case "section" -> section(row);
                case "string" -> string(row);
                case "clip" -> clip(row);
                default -> throw new IllegalArgumentException("unknown probe");
            });
            out.add(row);
        }
        Files.writeString(Path.of(args[1]), new GsonBuilder().setPrettyPrinting().create().toJson(out));
    }
}
