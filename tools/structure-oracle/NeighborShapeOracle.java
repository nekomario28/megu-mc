package dev.megu.tools;

import com.google.gson.JsonParser;
import java.lang.reflect.Method;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.ArrayList;
import java.util.List;

/** Calls registered 1.21.1 BlockState.updateShape with actual native states.
 * This is isolated updateShape, not all of StructureTemplate.updateShapeAtEdge.
 */
public final class NeighborShapeOracle {
    record Case(String id, String original, String neighbor, String direction,
                String from, String to) {}
    static final String STAIR = "{\"Name\":\"minecraft:oak_stairs\",\"Properties\":{\"facing\":\"north\",\"half\":\"bottom\",\"shape\":\"straight\",\"waterlogged\":\"false\"}}";
    static final String HANGING_LANTERN = "{\"Name\":\"minecraft:lantern\",\"Properties\":{\"hanging\":\"true\",\"waterlogged\":\"false\"}}";
    static final String WEST_STAIR = "{\"Name\":\"minecraft:oak_stairs\",\"Properties\":{\"facing\":\"west\",\"half\":\"bottom\",\"shape\":\"straight\",\"waterlogged\":\"false\"}}";
    static final String WALL_NORTH = "{\"Name\":\"minecraft:cobblestone_wall\",\"Properties\":{\"north\":\"low\",\"south\":\"none\",\"east\":\"none\",\"west\":\"none\",\"up\":\"true\",\"waterlogged\":\"false\"}}";
    static final String WALL_STRAIGHT = "{\"Name\":\"minecraft:cobblestone_wall\",\"Properties\":{\"north\":\"low\",\"south\":\"low\",\"east\":\"none\",\"west\":\"none\",\"up\":\"false\",\"waterlogged\":\"false\"}}";
    static final String WALL_T = "{\"Name\":\"minecraft:cobblestone_wall\",\"Properties\":{\"north\":\"low\",\"south\":\"low\",\"east\":\"low\",\"west\":\"none\",\"up\":\"true\",\"waterlogged\":\"false\"}}";
    static final String PANE_EAST = "{\"Name\":\"minecraft:glass_pane\",\"Properties\":{\"east\":\"true\"}}";
    static final String FENCE_EAST = "{\"Name\":\"minecraft:oak_fence\",\"Properties\":{\"east\":\"true\"}}";
    static final Case[] CASES = {
        new Case("oak_north_bone","minecraft:oak_fence","minecraft:bone_block","NORTH","north=false","north=true"),
        new Case("oak_south_bone","minecraft:oak_fence","minecraft:bone_block","SOUTH","south=false","south=true"),
        new Case("oak_west_bone","minecraft:oak_fence","minecraft:bone_block","WEST","west=false","west=true"),
        new Case("oak_east_bone","minecraft:oak_fence","minecraft:bone_block","EAST","east=false","east=true"),
        new Case("nether_west_bone","minecraft:nether_brick_fence","minecraft:bone_block","WEST","west=false","west=true"),
        new Case("pane_west_bone","minecraft:glass_pane","minecraft:bone_block","WEST","west=false","west=true"),
        new Case("bars_east_bone","minecraft:iron_bars","minecraft:bone_block","EAST","east=false","east=true"),
        new Case("wall_west_bone","minecraft:cobblestone_wall","minecraft:bone_block","WEST","west=none","west=low"),
        new Case("stairs_outer_left",STAIR,WEST_STAIR,"NORTH","shape=straight","shape=outer_left"),
        new Case("stairs_inner_left",STAIR,WEST_STAIR,"SOUTH","shape=straight","shape=inner_left"),
        new Case("torch_down_air","minecraft:torch","minecraft:air","DOWN","minecraft:torch","minecraft:air"),
        new Case("wall_torch_south_air","minecraft:wall_torch","minecraft:air","SOUTH","minecraft:wall_torch","minecraft:air"),
        new Case("lantern_up_air",HANGING_LANTERN,"minecraft:air","UP","minecraft:lantern","minecraft:air"),
        new Case("wall_post_to_straight",WALL_NORTH,"minecraft:bone_block","SOUTH","south=none;up=true","south=low;up=false"),
        new Case("wall_straight_to_tee",WALL_STRAIGHT,"minecraft:bone_block","EAST","east=none;up=false","east=low;up=true"),
        new Case("wall_straight_disconnect",WALL_STRAIGHT,"minecraft:air","SOUTH","south=low;up=false","south=none;up=true"),
        new Case("wall_tee_disconnect",WALL_T,"minecraft:air","EAST","east=low;up=true","east=none;up=false"),
        new Case("pane_east_disconnect",PANE_EAST,"minecraft:air","EAST","east=true","east=false"),
        new Case("fence_east_disconnect",FENCE_EAST,"minecraft:air","EAST","east=true","east=false"),
        new Case("pane_bars_join","minecraft:glass_pane","minecraft:iron_bars","WEST","west=false","west=true"),
        new Case("wood_fence_join","minecraft:oak_fence","minecraft:spruce_fence","EAST","east=false","east=true"),
        new Case("nether_no_wood_join","minecraft:nether_brick_fence","minecraft:oak_fence","EAST","",""),
        new Case("oak_west_air","minecraft:oak_fence","minecraft:air","WEST","",""),
        new Case("pane_west_air","minecraft:glass_pane","minecraft:air","WEST","",""),
        new Case("bone_west_bone","minecraft:bone_block","minecraft:bone_block","WEST","","")
    };

    static Object stateOf(String spec) throws Exception {
        return spec.startsWith("{")
                ? NativeHarness.stateFromJson(JsonParser.parseString(spec))
                : NativeHarness.state(spec);
    }

    @SuppressWarnings({"unchecked", "rawtypes"})
    public static void main(String[] args) throws Exception {
        if (args.length != 1) throw new IllegalArgumentException("output TSV path required");
        NativeHarness.bootstrap();
        // Shape behavior uses vanilla BLOCK/FLUID tags, which a bare bootstrap
        // does not populate. The native built-in pack is authoritative.
        VanillaBlockTags.load(NativeHarness.C("lt").getField("e").get(null));
        NativeFluidTags.load();
        Class<?> blockState = NativeHarness.C("dtc");
        Class<?> direction = NativeHarness.C("ji");
        Class<?> blockPos = NativeHarness.C("jd");
        List<String> rows = new ArrayList<>();
        for (Case c : CASES) {
            Object initial = stateOf(c.original());
            Object neighbor = stateOf(c.neighbor());
            Enum<?> face = Enum.valueOf((Class) direction.asSubclass(Enum.class), c.direction());
            int dx = 0, dy = 0, dz = 0;
            switch (c.direction()) {
                case "DOWN": dy = -1; break;
                case "UP": dy = 1; break;
                case "NORTH": dz = -1; break;
                case "SOUTH": dz = 1; break;
                case "WEST": dx = -1; break;
                case "EAST": dx = 1; break;
                default: throw new AssertionError(c.direction());
            }
            final int nx = dx, ny = 64 + dy, nz = dz;
            NativeHarness.Level level = NativeHarness.levelFromStates((x, y, z) -> {
                if (x == 0 && y == 64 && z == 0) return initial;
                if (x == nx && y == ny && z == nz) return neighbor;
                return NativeHarness.state("minecraft:air");
            });
            Method update = null;
            for (Method method : blockState.getMethods()) {
                Class<?>[] p = method.getParameterTypes();
                if (method.getName().equals("a") && method.getReturnType() == blockState
                        && p.length == 5 && p[0] == direction && p[1] == blockState
                        && p[2].isInstance(level.proxy) && p[3] == blockPos && p[4] == blockPos) {
                    if (update != null) throw new AssertionError("ambiguous native updateShape");
                    update = method;
                }
            }
            if (update == null) throw new AssertionError("native updateShape method unavailable");
            Object actual = update.invoke(initial, face, neighbor, level.proxy,
                    NativeHarness.pos(0, 64, 0), NativeHarness.pos(nx, ny, nz));
            String before = NativeHarness.normalizedStateName(initial);
            String after = NativeHarness.normalizedStateName(actual);
            String expected = before;
            if (!c.from().isEmpty()) {
                String[] from = c.from().split(";");
                String[] to = c.to().split(";");
                if (from.length != to.length) throw new AssertionError("invalid expected shape pairs");
                for (int i = 0; i < from.length; i++) {
                    if (!expected.contains(from[i]))
                        throw new AssertionError("missing native property " + c.id() + ": " + expected);
                    // A removed fragile block becomes plain air; replacing the
                    // name prefix must not carry its former state properties.
                    expected = to[i].equals("minecraft:air")
                            ? "minecraft:air" : expected.replace(from[i], to[i]);
                }
            }
            if (!expected.equals(after))
                throw new AssertionError(c.id() + " native state mismatch: " + after
                        + " expected " + expected + " from " + before);
            rows.add(c.id() + "\t" + before + "\t" + after + "\t"
                    + NativeHarness.normalizedStateName(neighbor) + "\t" + c.direction());
        }
        Files.write(Path.of(args[0]), rows, StandardCharsets.UTF_8);
        System.out.println("NATIVE_NEIGHBOR_SHAPES " + rows.size());
    }
}
