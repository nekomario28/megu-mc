package dev.mcspine.tools;

import java.lang.reflect.*;
import java.nio.file.*;
import java.util.*;

/**
 * Pinned 1.21.1 block-state physical facts for all 26,684 states.
 *
 * <p>Every column is a {@code BlockBehaviour.BlockStateBase} answer, evaluated against
 * {@code EmptyBlockGetter.INSTANCE} at {@code BlockPos.ZERO}. That getter is the right one
 * because {@code initCache()} resolves all of these when the state is constructed: sampling
 * three different worlds (stone everywhere, air everywhere, {@code EmptyBlockGetter}) produced
 * byte-identical results for all 34 columns, so the table is a property of the state and not of
 * a neighbourhood.
 *
 * <p>Row format: {@code <state text> TAB <flags> TAB <lightEmission> TAB <lightBlock> TAB <faces>
 * TAB <hasPostProcess>}. {@code faces} packs the 18 face/support answers as {@code support * 6 +
 * direction}, and both indexes are resolved from the enum constants' own names, never from array
 * position, because obfuscation is free to reorder the constants.
 *
 * <p>{@code hasPostProcess} is the predicate {@code WorldGenRegion.setBlock} reads to decide
 * whether a written cell is filed with {@code ChunkAccess.markPosForPostprocessing}. It is a
 * cached {@code StatePredicate}, so like every other column it is a property of the state; it is
 * read here for all states and the anchors pin that exactly three blocks answer true.
 *
 * <p>The anchors at the end are the guard: a wrong obfuscated name, a shifted bit, or a swapped
 * enum order changes one of them, and the run fails instead of writing a plausible table.
 */
public class BlockStateFactsOracle {
    static final int F_IS_AIR = 1;
    static final int F_LIQUID = 1 << 1;      // BlockStateBase.liquid(), not "has a fluid state"
    static final int F_BLOCKS_MOTION = 1 << 2;
    static final int F_CAN_OCCLUDE = 1 << 3;
    static final int F_SOLID_RENDER = 1 << 4;
    static final int F_COLLISION_EMPTY = 1 << 5;
    static final int F_COLLISION_FULL = 1 << 6;
    static final int F_FLUID_EMPTY = 1 << 7; // getFluidState().isEmpty()

    static final String[] DIRECTIONS = {"down", "up", "north", "south", "west", "east"};
    static final String[] SUPPORTS = {"FULL", "CENTER", "RIGID"};
    static final String[] LIQUID_BLOCKS = {"water", "lava", "bubble_column"};
    /** The only blocks whose SupportType.RIGID answer differs from their FULL answer. */
    static final String[] RIGID_NOT_FULL = {
        "cauldron", "composter", "hopper", "lava_cauldron", "powder_snow_cauldron", "water_cauldron"};

    public static void main(String[] args) throws Exception {
        Class.forName("ab").getMethod("a").invoke(null);
        Class.forName("akt").getMethod("a").invoke(null);
        Class<?> getterType = Class.forName("dcc");
        Class<?> blockPos = Class.forName("jd");
        Class<?> directionType = Class.forName("ji");
        Class<?> stateType = Class.forName("dtc");
        Class<?> baseType = Class.forName("dtb$a");
        Class<?> blockType = Class.forName("dfy");
        Class<?> supportType = Class.forName("doa");
        Object zero = blockPos.getConstructor(int.class, int.class, int.class).newInstance(0, 0, 0);
        Object world = Class.forName("dcl").getField("a").get(null);

        Method isAir = zeroArity(baseType, "i", boolean.class);
        Method liquid = zeroArity(baseType, "k", boolean.class);
        Method blocksMotion = zeroArity(baseType, "e", boolean.class);
        Method canOcclude = zeroArity(baseType, "p", boolean.class);
        Method emission = zeroArity(baseType, "h", int.class);
        Method fluidState = zeroArity(baseType, "u", null);
        Method solidRender = twoArity(baseType, "i", getterType, blockPos, boolean.class);
        // `BlockBehaviour.BlockStateBase.hasPostProcess(BlockGetter, BlockPos)` (`dtb$a.q`,
        // mappings 1213:1213). One declaration, no per-block override in the pinned version.
        Method postProcess = twoArity(baseType, "q", getterType, blockPos, boolean.class);
        Method collisionFull = twoArity(baseType, "r", getterType, blockPos, boolean.class);
        Method lightBlock = twoArity(baseType, "b", getterType, blockPos, int.class);
        Method collisionShape = twoArity(baseType, "k", getterType, blockPos, null);
        Method sturdy = faceSturdy(baseType, getterType, blockPos, directionType, supportType);

        Class<?> fluidStateType = fluidState.getReturnType();
        Class<?> shapeType = collisionShape.getReturnType();
        Object[] directions = constantsByName(directionType, DIRECTIONS);
        Object[] supports = constantsByName(supportType, SUPPORTS);
        require(supports.length == 3 && directions.length == 6, "enum sizes");

        Map<String, Object> states = new TreeMap<>();
        for (Object state : (Iterable<?>) blockType.getField("q").get(null))
            states.put(state.toString(), state);
        require(states.size() == 26684, "state count " + states.size());

        // Emptiness is identified by behaviour, not by name: `isEmpty` is obfuscated per type.
        Method fluidEmpty = emptyBySample(fluidStateType,
                fluidState.invoke(states.get("Block{minecraft:air}")),
                fluidState.invoke(states.get(findState(states, "water", "level=0]"))));
        Object airShape = collisionShape.invoke(states.get("Block{minecraft:air}"), world, zero);
        Object stoneShape = collisionShape.invoke(states.get("Block{minecraft:stone}"), world, zero);
        Method shapeEmpty = emptyBySample(shapeType, airShape, stoneShape);

        Map<String, int[]> facts = new HashMap<>();
        Map<String, String> bareName = new HashMap<>();
        int airStates = 0;
        for (Map.Entry<String, Object> entry : states.entrySet()) {
            String text = entry.getKey();
            Object state = entry.getValue();
            int flags = 0;
            if ((boolean) isAir.invoke(state)) { flags |= F_IS_AIR; airStates++; }
            if ((boolean) liquid.invoke(state)) flags |= F_LIQUID;
            if ((boolean) blocksMotion.invoke(state)) flags |= F_BLOCKS_MOTION;
            if ((boolean) canOcclude.invoke(state)) flags |= F_CAN_OCCLUDE;
            if ((boolean) solidRender.invoke(state, world, zero)) flags |= F_SOLID_RENDER;
            if ((boolean) shapeEmpty.invoke(collisionShape.invoke(state, world, zero)))
                flags |= F_COLLISION_EMPTY;
            if ((boolean) collisionFull.invoke(state, world, zero)) flags |= F_COLLISION_FULL;
            if ((boolean) fluidEmpty.invoke(fluidState.invoke(state))) flags |= F_FLUID_EMPTY;
            int light = (Integer) emission.invoke(state);
            int block = (Integer) lightBlock.invoke(state, world, zero);
            require(light >= 0 && light <= 15, "emission " + text);
            require(block >= 0 && block <= 15, "lightBlock " + text);
            require((flags & F_COLLISION_EMPTY) == 0 || (flags & F_COLLISION_FULL) == 0,
                    "collision both empty and full: " + text);
            int faces = 0;
            for (int s = 0; s < SUPPORTS.length; s++)
                for (int d = 0; d < DIRECTIONS.length; d++)
                    if ((boolean) sturdy.invoke(state, world, zero, directions[d], supports[s]))
                        faces |= 1 << (s * 6 + d);
            int post = (boolean) postProcess.invoke(state, world, zero) ? 1 : 0;
            facts.put(text, new int[]{flags, light, block, faces, post});
            bareName.put(text, bareName(text));
        }
        auditAnchors(facts, bareName, states);
        auditFaces(facts, bareName);
        auditPostProcess(facts, bareName);
        require(airStates == 3, "isAir states = " + airStates);
        for (String block : LIQUID_BLOCKS)
            require(taggedAny(facts, bareName, block, F_LIQUID), block + " is not liquid()");

        List<String> rows = new ArrayList<>();
        for (Map.Entry<String, int[]> entry : facts.entrySet()) {
            int[] f = entry.getValue();
            rows.add(entry.getKey() + "\t" + f[0] + "\t" + f[1] + "\t" + f[2] + "\t" + f[3]
                    + "\t" + f[4]);
        }
        Path output = Path.of(args[0]);
        Files.write(output, rows);
        System.out.println(rows.size() + " native block-state fact rows");
    }

    /** Named anchors for every predicate that the obfuscated names could swap or shift. */
    static void auditAnchors(Map<String, int[]> facts, Map<String, String> bare,
                             Map<String, Object> states) {
        int[] stone = facts.get("Block{minecraft:stone}");
        require(stone != null, "no stone");
        require(stone[0] == (F_BLOCKS_MOTION | F_CAN_OCCLUDE | F_SOLID_RENDER | F_COLLISION_FULL
                | F_FLUID_EMPTY), "stone flags " + stone[0]);
        require(stone[1] == 0 && stone[2] == 15, "stone light " + stone[1] + "/" + stone[2]);
        require(stone[3] == (1 << 18) - 1, "stone faces " + stone[3]);

        int[] glass = facts.get("Block{minecraft:glass}");
        require(glass[0] == (F_BLOCKS_MOTION | F_COLLISION_FULL | F_FLUID_EMPTY),
                "glass flags " + glass[0]);
        require(glass[2] == 0, "glass lightBlock " + glass[2]);

        int[] path = facts.get("Block{minecraft:dirt_path}");
        require((path[0] & F_CAN_OCCLUDE) != 0 && (path[0] & F_SOLID_RENDER) == 0,
                "dirt_path must separate canOcclude from isSolidRender: " + path[0]);
        require((path[3] & 0x3F) == 1, "dirt_path FULL must be down only: " + (path[3] & 0x3F));

        int[] air = facts.get("Block{minecraft:air}");
        require(air[0] == (F_IS_AIR | F_COLLISION_EMPTY | F_FLUID_EMPTY), "air flags " + air[0]);
        require(air[3] == 0, "air faces " + air[3]);

        int[] water = facts.get(findState(states, "water", "level=0]"));
        require((water[0] & (F_LIQUID | F_BLOCKS_MOTION | F_FLUID_EMPTY)) == F_LIQUID,
                "water flags " + water[0]);
        require((water[0] & F_COLLISION_EMPTY) != 0, "water collision must be empty");

        String slab = findState(states, "oak_slab", "type=bottom,waterlogged=false]");
        int[] bottom = facts.get(slab);
        require((bottom[0] & (F_COLLISION_FULL | F_COLLISION_EMPTY)) == 0, "slab collision " + bottom[0]);
        require((bottom[3] & 0x3F) == 1, "bottom slab FULL must be down only: " + bottom[3]);
        require(((bottom[3] >> 6) & 0x3F) == 1, "bottom slab CENTER must be down only: " + bottom[3]);

        // A fence answers differently per connection state, which is the point of keying the
        // table by state id rather than by block: the isolated post supports only vertically,
        // while a post with all four arms covers the centre of every face.
        int[] post = facts.get(findState(states, "oak_fence",
                "east=false,north=false,south=false,waterlogged=false,west=false]"));
        require((post[3] & 0x3F) == 0, "isolated fence post FULL " + post[3]);
        require(((post[3] >> 6) & 0x3F) == 3, "isolated fence post CENTER " + post[3]);
        require(((post[3] >> 12) & 0x3F) == 0, "isolated fence post RIGID " + post[3]);
        require((post[0] & (F_BLOCKS_MOTION | F_COLLISION_EMPTY)) == F_BLOCKS_MOTION,
                "fence flags " + post[0]);
        int[] arms = facts.get(findState(states, "oak_fence",
                "east=true,north=true,south=true,waterlogged=false,west=true]"));
        require(((arms[3] >> 6) & 0x3F) == 0x3F, "armed fence CENTER " + arms[3]);

        // A chest is not a support in any direction but down, and a cauldron is the one family
        // whose RIGID answer leaves CENTER. Both pin the plane order.
        int[] chest = facts.get(findState(states, "chest", "facing=north,type=single"));
        require((chest[3] & 0x3F) == 0 && ((chest[3] >> 6) & 0x3F) == 1,
                "chest FULL/CENTER " + chest[3]);

        // The collision shape is not the outline: a sapling, a torch and short grass are drawn
        // with a shape and collide with nothing, and a dirt path collides without covering a
        // whole face. Reading the outline method here instead put 5,124 states on the wrong side.
        require((facts.get("Block{minecraft:short_grass}")[0] & F_COLLISION_EMPTY) != 0,
                "short grass must have an empty collision shape");
        require((facts.get("Block{minecraft:torch}")[0] & F_COLLISION_EMPTY) != 0,
                "a torch must have an empty collision shape");
        require((path[0] & (F_COLLISION_EMPTY | F_COLLISION_FULL)) == 0,
                "dirt_path collides without being a full cube: " + path[0]);

        int[] glow = facts.get("Block{minecraft:glowstone}");
        require(glow[1] == 15, "glowstone emission " + glow[1]);

        // A waterlogged plant has a fluid state but is not a liquid block: the two columns are
        // different questions and this is the state that proves the export kept both.
        String logged = findState(states, "mangrove_propagule", "waterlogged=true");
        int[] prop = facts.get(logged);
        require((prop[0] & F_LIQUID) == 0 && (prop[0] & F_FLUID_EMPTY) == 0,
                "waterlogged propagule flags " + prop[0]);

        int[] cauldron = facts.get("Block{minecraft:cauldron}");
        require((cauldron[3] & 0x3F) == 0 && ((cauldron[3] >> 12) & 0x3F) == 2,
                "cauldron FULL/RIGID " + cauldron[3]);
    }

    /**
     * Two invariants, both measured over every state: FULL is inside CENTER, and RIGID equals
     * FULL except for the six container-shaped blocks (a cauldron, composter or hopper answers
     * RIGID for its open top face, which is not a full face). Anything else moving here means a
     * plane or a direction bit was reordered.
     */
    /**
     * `hasPostProcess` is a `Blocks.<clinit>` fact, so the whole set is re-derived here rather
     * than typed into Rust: exactly three blocks carry the constant-true predicate, one state
     * each, and every other state of every other block answers false. `oak_leaves` is the
     * control for "a full-cube-shaped block that is not in the set".
     */
    static void auditPostProcess(Map<String, int[]> facts, Map<String, String> bare) {
        Set<String> blocks = new TreeSet<>();
        int states = 0;
        for (Map.Entry<String, int[]> entry : facts.entrySet()) {
            if (entry.getValue()[4] == 0) continue;
            states++;
            blocks.add(bare.get(entry.getKey()));
            require(entry.getValue()[4] == 1, "hasPostProcess is not 0/1: " + entry.getKey());
        }
        require(blocks.equals(new TreeSet<>(Arrays.asList(
                "brown_mushroom", "red_mushroom", "magma_block"))),
                "hasPostProcess blocks " + blocks);
        require(states == 3, "hasPostProcess states " + states);
        for (Map.Entry<String, int[]> entry : facts.entrySet()) {
            String name = bare.get(entry.getKey());
            if (name.equals("brown_mushroom") || name.equals("red_mushroom")
                    || name.equals("magma_block")) continue;
            require(entry.getValue()[4] == 0, "unexpected hasPostProcess: " + entry.getKey());
        }
    }

    static void auditFaces(Map<String, int[]> facts, Map<String, String> bare) {
        Set<String> rigidExceptions = new TreeSet<>(Arrays.asList(RIGID_NOT_FULL));
        int exceptions = 0;
        for (Map.Entry<String, int[]> entry : facts.entrySet()) {
            int faces = entry.getValue()[3];
            int full = faces & 0x3F, center = (faces >> 6) & 0x3F, rigid = (faces >> 12) & 0x3F;
            require((full & ~center) == 0, "FULL outside CENTER: " + entry.getKey());
            if (rigid == full) continue;
            exceptions++;
            require(rigidExceptions.contains(bare.get(entry.getKey())),
                    "RIGID differs from FULL on " + entry.getKey());
            // These six answer RIGID for the open top face their FULL shape does not cover.
            require(rigid == (full | 2), "container RIGID must be FULL plus up: " + entry.getKey());
        }
        require(exceptions == 27, "RIGID/FULL exception states " + exceptions);
    }

    /** The obfuscated face-sturdy method, pinned by its parameter types rather than its name. */
    static Method faceSturdy(Class<?> base, Class<?> getter, Class<?> pos, Class<?> direction,
                             Class<?> support) {
        Method found = null;
        for (Method m : base.getMethods()) {
            Class<?>[] p = m.getParameterTypes();
            if (p.length == 4 && p[0] == getter && p[1] == pos && p[2] == direction && p[3] == support
                    && m.getReturnType() == boolean.class) {
                require(found == null, "two face-sturdy methods");
                found = m;
            }
        }
        require(found != null, "no isFaceSturdy(level, pos, direction, support)");
        found.setAccessible(true);
        return found;
    }

    static Method zeroArity(Class<?> type, String name, Class<?> returns) {
        for (Method m : type.getDeclaredMethods())
            if (m.getName().equals(name) && m.getParameterCount() == 0
                    && (returns == null || m.getReturnType() == returns)
                    && !Modifier.isStatic(m.getModifiers()))
                return m;
        throw new IllegalStateException("no " + name + "() on " + type.getSimpleName());
    }

    static Method twoArity(Class<?> type, String name, Class<?> a, Class<?> b, Class<?> returns) {
        for (Method m : type.getDeclaredMethods())
            if (m.getName().equals(name) && m.getParameterCount() == 2
                    && m.getParameterTypes()[0] == a && m.getParameterTypes()[1] == b
                    && (returns == null || m.getReturnType() == returns)
                    && !Modifier.isStatic(m.getModifiers()))
                return m;
        throw new IllegalStateException("no " + name + "(a,b) on " + type.getSimpleName());
    }

    /**
     * `VoxelShape.isEmpty()` and `FluidState.isEmpty()` are separate obfuscated names, and a type
     * exposing several zero-arity booleans would let a wrong pick pass silently. Identify by
     * behaviour: true for the empty sample, false for the non-empty one, and only one candidate
     * may answer that way.
     *
     * The shape *source* needs the same care: `dtb$a` exposes five `(BlockGetter, BlockPos)` ->
     * `VoxelShape` methods - the outline, the collision shape, the support shape, the visual
     * shape and a full-cube shortcut - and four of them answer "empty" for air. They are
     * separated by the block whose outline is non-empty and whose collision is not (a sapling, a
     * torch, short grass): the collision method is the one that answers empty for all three, and
     * the anchors below fail the run if the wrong one is picked.
     */
    static Method emptyBySample(Class<?> type, Object empty, Object full) throws Exception {
        Method found = null;
        List<String> answers = new ArrayList<>();
        for (Method m : type.getMethods()) {
            if (m.getParameterCount() != 0 || m.getReturnType() != boolean.class
                    || Modifier.isStatic(m.getModifiers())) continue;
            m.setAccessible(true);
            Object a = m.invoke(empty), b = m.invoke(full);
            answers.add(m.getName() + "=" + a + "/" + b);
            if (Boolean.TRUE.equals(a) && Boolean.FALSE.equals(b)) {
                require(found == null, type.getSimpleName() + " has two empty() candidates: " + answers);
                found = m;
            }
        }
        require(found != null, type.getSimpleName() + " has no empty() candidate: " + answers);
        return found;
    }

    static Object[] constantsByName(Class<?> enumType, String[] names) {
        Object[] out = new Object[names.length];
        for (Object constant : enumType.getEnumConstants())
            for (int i = 0; i < names.length; i++)
                if (names[i].equals(constant.toString())) out[i] = constant;
        for (int i = 0; i < names.length; i++)
            require(out[i] != null, "missing enum constant " + names[i] + " in " + enumType);
        return out;
    }

    /**
     * The first state of `block` whose text contains every fragment in `required`, in the
     * registry's sorted text order. Anchors that must name one exact state pass its whole
     * property list; the waterlogged-plant anchor only needs any waterlogged variant.
     */
    static String findState(Map<String, Object> states, String block, String required) {
        for (String text : states.keySet()) {
            if (!bareName(text).equals(block)) continue;
            if (required.isEmpty() && text.contains("[")) continue;
            if (text.contains(required)) return text;
        }
        throw new IllegalStateException("no state " + block + " " + required);
    }

    static String bareName(String text) {
        String name = text.substring(text.indexOf("minecraft:") + "minecraft:".length());
        int cut = name.indexOf('}');
        if (cut >= 0) name = name.substring(0, cut);
        int bracket = name.indexOf('[');
        return bracket >= 0 ? name.substring(0, bracket) : name;
    }

    static boolean taggedAny(Map<String, int[]> facts, Map<String, String> bare,
                             String block, int masked) {
        for (Map.Entry<String, int[]> entry : facts.entrySet())
            if (bare.get(entry.getKey()).equals(block) && (entry.getValue()[0] & masked) != 0)
                return true;
        return false;
    }

    static void require(boolean ok, String message) {
        if (!ok) throw new IllegalStateException(message);
    }
}
