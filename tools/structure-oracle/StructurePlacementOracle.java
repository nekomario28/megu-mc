package dev.megu.tools;

import com.google.gson.*;
import java.lang.reflect.*;
import java.nio.file.*;
import java.util.*;

/** Calls the pinned RandomSpreadStructurePlacement and all frequency reducers. */
public final class StructurePlacementOracle {
    static int integer(JsonObject obj, String name, int fallback) {
        return obj.has(name) ? obj.get(name).getAsInt() : fallback;
    }
    public static void main(String[] args) throws Exception {
        Class.forName("ab").getMethod("a").invoke(null);
        Class.forName("akt").getMethod("a").invoke(null);
        Class<?> random = Class.forName("ekm"), spread = Class.forName("ekn");
        Class<?> frequency = Class.forName("eko$c"), vector = Class.forName("kh");
        Object zero = vector.getField("g").get(null);
        Constructor<?> ctor = random.getConstructor(vector, frequency, float.class, int.class,
            Optional.class, int.class, int.class, spread);
        Method candidate = random.getMethod("a", long.class, int.class, int.class);
        Method restriction = Class.forName("eko").getMethod("a", int.class, int.class, long.class);
        Field cx = Class.forName("dcd").getField("e"), cz = Class.forName("dcd").getField("f");
        Object[] modes = frequency.getEnumConstants(), spreads = spread.getEnumConstants();
        long[] seeds = {0L,1L,-1L,42L,Long.MIN_VALUE,Long.MAX_VALUE,0x123456789abcdef0L};
        int[][] coordinates = {{0,0},{-1,-1},{31,-32},{-33,64},{1875000,-1875000},
            {Integer.MIN_VALUE,Integer.MAX_VALUE},{-1024,512},{1024,-513}};
        List<String> rows = new ArrayList<>();
        try (var files = Files.walk(Path.of(args[0]))) {
            for (Path file : files.filter(p->p.toString().endsWith(".json")).sorted().toList()) {
                JsonObject place = JsonParser.parseString(Files.readString(file)).getAsJsonObject()
                    .getAsJsonObject("placement");
                if (!place.get("type").getAsString().equals("minecraft:random_spread")) continue;
                int spacing = integer(place,"spacing",0), separation = integer(place,"separation",0);
                int salt = integer(place,"salt",0);
                int spreadIndex = place.has("spread_type") && place.get("spread_type").getAsString().equals("triangular") ? 1 : 0;
                float[] rates = {1.0f,0.2f,0.004f,0.0f};
                for (long seed : seeds) for (int[] coord : coordinates) for (int mode=0;mode<4;mode++) for (float rate : rates) {
                    Object placement = ctor.newInstance(zero,modes[mode],rate,salt,Optional.empty(),spacing,separation,spreads[spreadIndex]);
                    Object c = candidate.invoke(placement,seed,coord[0],coord[1]);
                    boolean pass = (boolean) restriction.invoke(placement,coord[0],coord[1],seed);
                    rows.add(file.getFileName()+"\t"+seed+"\t"+coord[0]+"\t"+coord[1]+"\t"+spacing+"\t"+separation+"\t"+salt+"\t"+spreadIndex+"\t"+Float.floatToIntBits(rate)+"\t"+mode+"\t"+cx.getInt(c)+"\t"+cz.getInt(c)+"\t"+pass);
                }
            }
        }
        if (rows.size()!=19*7*8*4*4) throw new IllegalStateException("incomplete pinned placement sets: "+rows.size());
        Files.write(Path.of(args[1]),rows);
        System.out.println("native structure placement vectors: "+rows.size());
    }
}
