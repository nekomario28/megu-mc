package dev.mcspine.tools;

import com.google.gson.JsonParser;
import java.lang.reflect.Method;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.ArrayList;
import java.util.List;

/** Ordered native 1.21.1 shape-update and scheduling *request* trace.
 *
 * LevelAccessor's own scheduleTick default builds ScheduledTick records. The
 * harness records those native records (due, priority, sub-order) on insertion,
 * not a hand-authored schedule. This is not LevelTicks delivery or the complete
 * ServerLevel neighbor chain: unsupported harness calls abort the capture.
 */
public final class NativeNeighborTraceOracle {
    record Scene(String id, String stateJson, String changedNeighbor,
                 String direction) {}
    static final String WATER_PANE =
        "{\"Name\":\"minecraft:glass_pane\",\"Properties\":{\"waterlogged\":\"true\"}}";
    static final String WATER_FENCE =
        "{\"Name\":\"minecraft:oak_fence\",\"Properties\":{\"waterlogged\":\"true\"}}";
    static final String WATER_STAIRS =
        "{\"Name\":\"minecraft:oak_stairs\",\"Properties\":{\"waterlogged\":\"true\"}}";
    static final String LEAVES =
        "{\"Name\":\"minecraft:oak_leaves\",\"Properties\":{\"distance\":\"7\",\"persistent\":\"false\",\"waterlogged\":\"false\"}}";
    static final Scene[] SCENES = {
        new Scene("waterlogged_pane", WATER_PANE, "minecraft:bone_block", "NORTH"),
        new Scene("waterlogged_fence", WATER_FENCE, "minecraft:bone_block", "EAST"),
        new Scene("waterlogged_stairs", WATER_STAIRS, "minecraft:stone", "SOUTH"),
        new Scene("leaf_next_to_log", LEAVES, "minecraft:oak_log", "NORTH"),
        new Scene("falling_sand", "minecraft:sand", "minecraft:air", "DOWN"),
        new Scene("falling_gravel", "minecraft:gravel", "minecraft:air", "DOWN"),
        new Scene("falling_red_sand", "minecraft:red_sand", "minecraft:air", "DOWN"),
        new Scene("falling_concrete_powder", "minecraft:white_concrete_powder", "minecraft:air", "DOWN"),
        // Isolated updateShape against a full supporting block still books a
        // delayed callback; the callback must re-check support before spawning.
        new Scene("falling_sand_supported", "minecraft:sand", "minecraft:stone", "DOWN"),
        new Scene("fragile_torch", "minecraft:torch", "minecraft:air", "DOWN")
    };

    static Object stateOf(String jsonOrName) throws Exception {
        return jsonOrName.startsWith("{")
                ? NativeHarness.stateFromJson(JsonParser.parseString(jsonOrName))
                : NativeHarness.state(jsonOrName);
    }

    @SuppressWarnings({"unchecked", "rawtypes"})
    static void update(Scene scene, NativeHarness.Level level, Object center)
            throws Exception {
        Class<?> direction = NativeHarness.C("ji");
        Object face = Enum.valueOf((Class) direction.asSubclass(Enum.class),
                scene.direction());
        Object current = stateOf(scene.stateJson());
        Object neighbor = stateOf(scene.changedNeighbor());
        int dx = 0, dy = 0, dz = 0;
        switch (scene.direction()) {
            case "DOWN": dy = -1; break;
            case "UP": dy = 1; break;
            case "NORTH": dz = -1; break;
            case "SOUTH": dz = 1; break;
            case "WEST": dx = -1; break;
            case "EAST": dx = 1; break;
            default: throw new AssertionError("invalid direction " + scene.direction());
        }
        Method shape = NativeHarness.C("dtc").getMethod("a", direction,
                NativeHarness.C("dtc"), NativeHarness.C("dcx"),
                NativeHarness.C("jd"), NativeHarness.C("jd"));
        Object result = shape.invoke(current, face, neighbor, level.proxy, center,
                NativeHarness.pos(dx, 64 + dy, dz));
        level.events.add("shape " + scene.id() + " "
                + NativeHarness.normalizedStateName(result));
    }

    static void record(List<String> out, String id, List<String> events) {
        if (events.isEmpty()) throw new AssertionError("no events for " + id);
        for (int i = 0; i < events.size(); i++)
            out.add(id + "\t" + i + "\t" + events.get(i));
    }

    public static void main(String[] args) throws Exception {
        if (args.length != 1) throw new IllegalArgumentException("trace TSV path required");
        NativeHarness.bootstrap();
        VanillaBlockTags.load(NativeHarness.C("lt").getField("e").get(null));
        NativeFluidTags.load();
        Object center = NativeHarness.pos(0, 64, 0);
        List<String> output = new ArrayList<>();
        for (Scene scene : SCENES) {
            Object initial = stateOf(scene.stateJson());
            NativeHarness.Level level = NativeHarness.levelFromStates(
                    (x, y, z) -> x == 0 && y == 64 && z == 0 ? initial
                            : NativeHarness.state("minecraft:air"));
            update(scene, level, center);
            record(output, scene.id(), level.events);
        }
        // Three consecutive native updateShape callbacks share *one* level
        // subTickCount. This probes the exact cross-family appointment order;
        // it does not simulate a full ServerLevel tick drain.
        NativeHarness.Level ordered = NativeHarness.levelFromStates(
                (x, y, z) -> NativeHarness.state("minecraft:air"));
        for (Scene scene : new Scene[]{SCENES[0], SCENES[3], SCENES[4]})
            update(scene, ordered, center);
        record(output, "mixed_scheduling_requests", ordered.events);

        // A native BlockStateBase.updateNeighbourShapes pass invokes real
        // LevelAccessor.neighborShapeChanged callbacks. If the scripted level
        // lacks a necessary world operation, the probe fails closed here.
        Object bone = NativeHarness.state("minecraft:bone_block");
        Object fence = NativeHarness.state("minecraft:oak_fence");
        Object pane = NativeHarness.state("minecraft:glass_pane");
        NativeHarness.Level edge = NativeHarness.levelFromStates((x, y, z) -> {
            if (x == 0 && y == 64 && z == 0) return bone;
            if (x == 1 && y == 64 && z == 0) return fence;
            if (x == -1 && y == 64 && z == 0) return pane;
            return NativeHarness.state("minecraft:air");
        });
        Method nativePass = NativeHarness.C("dtc").getMethod("a",
                NativeHarness.C("dcx"), NativeHarness.C("jd"), int.class, int.class);
        nativePass.invoke(bone, edge.proxy, center, 3, 512);
        record(output, "native_neighbor_pass", edge.events);
        Files.write(Path.of(args[0]), output, StandardCharsets.UTF_8);
        System.out.println("NATIVE_NEIGHBOR_REQUEST_TRACE rows=" + output.size());
    }
}
