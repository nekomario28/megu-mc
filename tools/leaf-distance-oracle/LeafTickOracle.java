package dev.megu.tools;

import java.lang.reflect.*;
import java.nio.file.*;
import java.util.*;

/** Invokes the real 1.21.1 LeavesBlock.updateDistance, used by scheduled tick. */
public final class LeafTickOracle {
    public static void main(String[] args) throws Exception {
        LeafDistanceOracle.cls("ab").getMethod("a").invoke(null);
        LeafDistanceOracle.cls("akt").getMethod("a").invoke(null);
        LeafDistanceOracle.registry = LeafDistanceOracle.cls("lt").getField("e").get(null);
        VanillaBlockTags.load(LeafDistanceOracle.registry);
        Class<?> pos = LeafDistanceOracle.cls("jd"), state = LeafDistanceOracle.cls("dtc");
        Class<?> level = LeafDistanceOracle.cls("dcx"), property = LeafDistanceOracle.cls("duf");
        LeafDistanceOracle.x = pos.getMethod("u");
        LeafDistanceOracle.y = pos.getMethod("v");
        LeafDistanceOracle.z = pos.getMethod("w");
        LeafDistanceOracle.get = state.getMethod("c", property);
        LeafDistanceOracle.set = state.getMethod("a", property, Comparable.class);
        LeafDistanceOracle.distance = LeafDistanceOracle.cls("dki").getField("d").get(null);
        LeafDistanceOracle.persistent = LeafDistanceOracle.cls("dki").getField("e").get(null);
        LeafDistanceOracle.waterlogged = LeafDistanceOracle.cls("dki").getField("f").get(null);
        LeafDistanceOracle.air = LeafDistanceOracle.state("A");
        Method update = LeafDistanceOracle.cls("dki").getDeclaredMethod("a", state, level, pos);
        update.setAccessible(true);
        Constructor<?> position = pos.getConstructor(int.class, int.class, int.class);
        List<String> rows = new ArrayList<>();
        String[] neighbors = {"A", "S", "T", "L1", "L4", "L7", "C7"};
        for (String center : List.of("L7", "P4", "W2", "B6")) {
            for (int direction = 0; direction < 6; direction++) {
                for (String neighbor : neighbors) {
                    var origin = new LeafDistanceOracle.P(-1024, 70, 512);
                    var nearby = origin.plus(LeafDistanceOracle.DIRS[direction]);
                    Object proxy = Proxy.newProxyInstance(LeafTickOracle.class.getClassLoader(),
                            new Class<?>[]{level}, (p, method, parameters) -> {
                                if (method.getName().equals("a_") && parameters.length == 1)
                                    return LeafDistanceOracle.point(parameters[0]).equals(nearby)
                                            ? LeafDistanceOracle.state(neighbor) : LeafDistanceOracle.air;
                                throw new UnsupportedOperationException(method.toString());
                            });
                    Object result = update.invoke(null, LeafDistanceOracle.state(center), proxy,
                            position.newInstance(origin.x(), origin.y(), origin.z()));
                    rows.add(center + "\t" + direction + "\t" + neighbor + "\t"
                            + LeafDistanceOracle.leaf(result));
                }
            }
        }
        // Put the stronger neighbour after the weaker one in Direction.values().
        for (String center : List.of("L7", "P4", "W2", "B6")) {
            var origin = new LeafDistanceOracle.P(-1024, 70, 512);
            var first = origin.plus(LeafDistanceOracle.DIRS[0]);
            var second = origin.plus(LeafDistanceOracle.DIRS[5]);
            Object proxy = Proxy.newProxyInstance(LeafTickOracle.class.getClassLoader(),
                    new Class<?>[]{level}, (p, method, parameters) -> {
                        if (method.getName().equals("a_") && parameters.length == 1) {
                            var point = LeafDistanceOracle.point(parameters[0]);
                            if (point.equals(first)) return LeafDistanceOracle.state("L4");
                            if (point.equals(second)) return LeafDistanceOracle.state("T");
                            return LeafDistanceOracle.air;
                        }
                        throw new UnsupportedOperationException(method.toString());
                    });
            Object result = update.invoke(null, LeafDistanceOracle.state(center), proxy,
                    position.newInstance(origin.x(), origin.y(), origin.z()));
            rows.add(center + "\t0,5\tL4,T\t" + LeafDistanceOracle.leaf(result));
        }
        Files.write(Path.of(args[0]), rows);
        System.out.println("scheduled leaf updateDistance native cases: " + rows.size());
    }
}
