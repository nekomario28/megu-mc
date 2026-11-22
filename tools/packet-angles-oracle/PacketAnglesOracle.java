package dev.megu.tools;

import com.google.gson.*;
import java.lang.reflect.*;
import java.nio.file.*;
import java.util.UUID;

/** Actual 1.21.1 spawn/teleport constructors. No world ticks or constructor bypass. */
public final class PacketAnglesOracle {
    static int unsigned(Object packet, String name) throws Exception {
        Field field = packet.getClass().getDeclaredField(name);
        field.setAccessible(true);
        return Byte.toUnsignedInt(field.getByte(packet));
    }

    public static void main(String[] args) throws Exception {
        Class.forName("ab").getMethod("a").invoke(null);
        Class.forName("akt").getMethod("a").invoke(null);
        Class<?> entity = Class.forName("bsr"), type = Class.forName("bsx");
        Object markerType = type.getField("aq").get(null); // EntityType.MARKER
        // Normal Marker constructor. Entity's constructor accepts an absent Level;
        // no Level methods or Marker.getAddEntityPacket are invoked by this probe.
        Object marker = Class.forName("bto").getConstructor(type, Class.forName("dcw"))
                .newInstance(markerType, null);
        Object zero = Class.forName("exc").getConstructor(double.class, double.class, double.class)
                .newInstance(0.0, 0.0, 0.0);
        Constructor<?> spawn = Class.forName("abv").getConstructor(int.class, UUID.class,
                double.class, double.class, double.class, float.class, float.class,
                type, int.class, Class.forName("exc"), double.class);
        Constructor<?> teleport = Class.forName("afw").getConstructor(entity);
        Method setYaw = entity.getMethod("t", float.class), setPitch = entity.getMethod("u", float.class);
        Method floor = Class.forName("ayo").getMethod("d", float.class);
        JsonArray output = new JsonArray();
        JsonObject input = JsonParser.parseString(Files.readString(Path.of(args[0]))).getAsJsonObject();
        for (JsonElement value : input.getAsJsonArray("cases")) {
            JsonObject row = value.getAsJsonObject().deepCopy();
            float yaw = Float.intBitsToFloat((int) row.get("yaw_bits").getAsLong());
            float pitch = Float.intBitsToFloat((int) row.get("pitch_bits").getAsLong());
            if (!Float.isFinite(yaw) || !Float.isFinite(pitch)) throw new IllegalArgumentException("nonfinite rotation");
            Object added = spawn.newInstance(42, new UUID(0, 0), 1.0, 64.0, -2.0,
                    pitch, yaw, markerType, 0, zero, (double) yaw);
            setYaw.invoke(marker, yaw);
            setPitch.invoke(marker, pitch);
            Object moved = teleport.newInstance(marker);
            JsonObject result = new JsonObject();
            JsonArray addedAngles = new JsonArray();
            for (String field : new String[]{"m", "n", "o"}) addedAngles.add(unsigned(added, field));
            result.add("spawn", addedAngles);
            JsonArray movedAngles = new JsonArray();
            for (String field : new String[]{"f", "g"}) movedAngles.add(unsigned(moved, field));
            result.add("teleport", movedAngles);
            // ServerEntity quantizes head updates through Mth.floor(float).
            // This invokes that actual helper, not the complete ServerEntity tracker.
            result.addProperty("head_update", ((int) floor.invoke(null, yaw * 256.0f / 360.0f)) & 255);
            row.add("expected", result);
            output.add(row);
        }
        Files.writeString(Path.of(args[1]), new GsonBuilder().setPrettyPrinting().create().toJson(output));
    }
}
