package dev.mcspine.tools;

import java.lang.reflect.Method;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.ArrayList;
import java.util.List;

/** Native BlockState.updateShape on a scripted level, not a reimplementation.
 * This proves only isolated fence/full-cube transitions. Whole placement order,
 * fluid delivery, other block families and live-server behavior are separate.
 */
public final class NeighborShapeOracle {
    private static final String[][] CASES = {
        {"oak_north_bone", "minecraft:oak_fence", "minecraft:bone_block", "NORTH", "north"},
        {"oak_south_bone", "minecraft:oak_fence", "minecraft:bone_block", "SOUTH", "south"},
        {"oak_west_bone", "minecraft:oak_fence", "minecraft:bone_block", "WEST", "west"},
        {"oak_east_bone", "minecraft:oak_fence", "minecraft:bone_block", "EAST", "east"},
        {"nether_west_bone", "minecraft:nether_brick_fence", "minecraft:bone_block", "WEST", "west"},
        {"oak_west_air", "minecraft:oak_fence", "minecraft:air", "WEST", "-"},
        {"bone_west_bone", "minecraft:bone_block", "minecraft:bone_block", "WEST", "-"}
    };

    @SuppressWarnings({"unchecked", "rawtypes"})
    public static void main(String[] args) throws Exception {
        if (args.length != 1) throw new IllegalArgumentException("output TSV path required");
        NativeHarness.bootstrap();
        Class<?> state = NativeHarness.C("dtc");
        Class<?> direction = NativeHarness.C("ji");
        Class<?> accessor = NativeHarness.C("dcx");
        Class<?> pos = NativeHarness.C("jd");

        // BlockBehaviour.BlockStateBase.updateShape: a(Direction, BlockState,
        // LevelAccessor, BlockPos, BlockPos). The actual registered block decides.
        Method update = state.getMethod("a", direction, state, accessor, pos, pos);
        List<String> rows = new ArrayList<>();
        for (String[] c : CASES) {
            Object initial = NativeHarness.state(c[1]);
            Object neighbor = NativeHarness.state(c[2]);
            Enum<?> face = Enum.valueOf((Class) direction.asSubclass(Enum.class), c[3]);
            int dx = 0, dy = 0, dz = 0;
            switch (c[3]) {
                case "NORTH": dz = -1; break;
                case "SOUTH": dz = 1; break;
                case "WEST": dx = -1; break;
                case "EAST": dx = 1; break;
                default: throw new AssertionError("unhandled case direction " + c[3]);
            }
            final int nx = dx, ny = 64 + dy, nz = dz;
            NativeHarness.Level level = NativeHarness.levelFromStates((x, y, z) -> {
                if (x == 0 && y == 64 && z == 0) return initial;
                if (x == nx && y == ny && z == nz) return neighbor;
                return NativeHarness.state("minecraft:air");
            });
            Object actual = update.invoke(initial, face, neighbor, level.proxy,
                    NativeHarness.pos(0, 64, 0), NativeHarness.pos(nx, ny, nz));
            String before = NativeHarness.normalizedStateName(initial);
            String after = NativeHarness.normalizedStateName(actual);
            String expected = before;
            if (!c[4].equals("-")) {
                String key = c[4] + "=false";
                if (!before.contains(key))
                    throw new AssertionError("native fence default lacks " + key + ": " + before);
                expected = before.replace(key, c[4] + "=true");
            }
            if (!expected.equals(after))
                throw new AssertionError(c[0] + " native state mismatch: " + after
                        + " expected " + expected + " from " + before);
            rows.add(c[0] + "\t" + before + "\t" + after);
        }
        Files.write(Path.of(args[0]), rows, StandardCharsets.UTF_8);
        System.out.println("NATIVE_NEIGHBOR_SHAPES " + rows.size());
    }
}
