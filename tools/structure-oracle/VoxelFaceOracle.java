package dev.megu.tools;

import com.google.gson.*;
import java.lang.reflect.*;
import java.nio.charset.StandardCharsets;
import java.nio.file.*;
import java.security.MessageDigest;
import java.util.*;

/** Calls native DiscreteVoxelShape.forAllFaces; no traversal is replayed here. */
public final class VoxelFaceOracle {
    public static void main(String[] args) throws Exception {
        NativeHarness.bootstrap();
        Class<?> volume=Class.forName("exf"), shape=Class.forName("exl"), consumer=Class.forName("exl$a");
        Constructor<?> constructor=volume.getConstructor(int.class,int.class,int.class);
        Method fill=shape.getMethod("c",int.class,int.class,int.class);
        Method visit=shape.getMethod("a",consumer);
        List<String> output=new ArrayList<>();
        for(JsonElement element:JsonParser.parseString(Files.readString(Path.of(args[0]))).getAsJsonArray()) {
            JsonObject test=element.getAsJsonObject();JsonArray size=test.getAsJsonArray("size"),origin=test.getAsJsonArray("origin");
            Object voxel=constructor.newInstance(size.get(0).getAsInt(),size.get(1).getAsInt(),size.get(2).getAsInt());
            for(JsonElement v:test.getAsJsonArray("filled")) {
                JsonArray p=v.getAsJsonArray();fill.invoke(voxel,p.get(0).getAsInt(),p.get(1).getAsInt(),p.get(2).getAsInt());
            }
            StringBuilder cells=new StringBuilder();int[] count={0};
            Object callback=Proxy.newProxyInstance(VoxelFaceOracle.class.getClassLoader(),new Class<?>[]{consumer},(proxy,m,a)-> {
                if(!m.getName().equals("consume"))throw new UnsupportedOperationException(m.toString());
                cells.append(((Enum<?>)a[0]).ordinal()).append(' ')
                    .append((int)a[1]+origin.get(0).getAsInt()).append(',')
                    .append((int)a[2]+origin.get(1).getAsInt()).append(',')
                    .append((int)a[3]+origin.get(2).getAsInt()).append('\n');count[0]++;return null;
            });
            visit.invoke(voxel,callback);
            String hash=HexFormat.of().formatHex(MessageDigest.getInstance("SHA-256").digest(cells.toString().getBytes(StandardCharsets.UTF_8)));
            output.add(test.get("id").getAsString()+"\t"+count[0]+"\t"+hash);
        }
        Files.write(Path.of(args[1]),output,StandardCharsets.UTF_8);
        System.out.println("VOXEL_NATIVE_CASES "+output.size());
    }
}
