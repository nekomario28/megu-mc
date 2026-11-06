package dev.mcspine.tools;

import java.lang.reflect.Constructor;
import java.lang.reflect.Method;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.ArrayList;
import java.util.LinkedHashMap;
import java.util.LinkedHashSet;
import java.util.List;
import java.util.Map;
import java.util.Set;
import java.util.function.BiConsumer;
import java.util.function.LongPredicate;
import java.util.function.Supplier;

/** Replay *native-created* tick appointments through native LevelTicks queues.
 *
 * The Block and Fluid queue phases are distinct. All touched chunks are eligible;
 * cold/warm selection and save/reload still need separate real-world baselines.
 * No fake "tick now" callback is accepted as native queue ordering.
 */
public final class NativeTraceDeliveryOracle {
    record Tick(String kind, String identity, int x, int y, int z,
                long due, String priority, long suborder) {}

    public static void main(String[] args) throws Exception {
        if (args.length != 2)
            throw new IllegalArgumentException("native requests.tsv delivered.tsv");
        Map<String, List<Tick>> cases = new LinkedHashMap<>();
        for (String line : Files.readAllLines(Path.of(args[0]), StandardCharsets.UTF_8)) {
            String[] columns = line.split("\t", 3);
            if (columns.length != 3)
                throw new IllegalArgumentException("malformed native request row: " + line);
            String[] event = columns[2].split(" ");
            if (!event[0].equals("tick")) continue;
            if (event.length != 7)
                throw new IllegalArgumentException("invalid native tick record: " + columns[2]);
            String[] pos = event[3].split(",");
            if (pos.length != 3) throw new IllegalArgumentException("invalid tick coordinates");
            cases.computeIfAbsent(columns[0], k -> new ArrayList<>()).add(new Tick(
                    event[1], event[2], Integer.parseInt(pos[0]),
                    Integer.parseInt(pos[1]), Integer.parseInt(pos[2]),
                    Long.parseLong(event[4]), event[5], Long.parseLong(event[6])));
        }
        Class<?> level = Class.forName("eyq");
        Class<?> container = Class.forName("eyo");
        Class<?> scheduled = Class.forName("eyt");
        Class<?> pos = Class.forName("jd");
        Class<?> chunk = Class.forName("dcd");
        Class<?> priority = Class.forName("eyx");
        Object profiler = Class.forName("bnc").getField("a").get(null);
        Constructor<?> position = pos.getConstructor(int.class, int.class, int.class);
        Constructor<?> chunkPos = chunk.getConstructor(int.class, int.class);
        Constructor<?> tick = scheduled.getConstructor(
                Object.class, pos, long.class, priority, long.class);
        Method addChunk = level.getMethod("a", chunk, container);
        Method schedule = level.getMethod("a", scheduled);
        Method collect = level.getMethod("a", long.class, int.class, BiConsumer.class);
        Method px = pos.getMethod("u"), py = pos.getMethod("v"), pz = pos.getMethod("w");

        List<String> output = new ArrayList<>();
        for (var entry : cases.entrySet()) {
            for (String phase : new String[]{"block", "fluid"}) {
                List<Tick> appointments = entry.getValue().stream()
                        .filter(t -> t.kind().equals(phase)).toList();
                if (appointments.isEmpty()) continue;
                Object queue = level.getConstructor(LongPredicate.class, Supplier.class)
                        .newInstance((LongPredicate) ignored -> true,
                                (Supplier<Object>) () -> profiler);
                Set<String> chunks = new LinkedHashSet<>();
                for (Tick t : appointments) chunks.add(
                        (t.x() >> 4) + "," + (t.z() >> 4));
                for (String key : chunks) {
                    String[] coords = key.split(",");
                    addChunk.invoke(queue,
                            chunkPos.newInstance(Integer.parseInt(coords[0]),
                                    Integer.parseInt(coords[1])),
                            container.getConstructor().newInstance());
                }
                for (Tick t : appointments) {
                    @SuppressWarnings({"rawtypes", "unchecked"})
                    Object priorityValue = Enum.valueOf(
                            (Class<? extends Enum>) priority.asSubclass(Enum.class),
                            t.priority());
                    schedule.invoke(queue, tick.newInstance(t.identity().intern(),
                            position.newInstance(t.x(), t.y(), t.z()), t.due(),
                            priorityValue, t.suborder()));
                }
                for (long at = 100; at <= 140; at++) {
                    final long now = at;
                    BiConsumer<Object, Object> callback = (cell, identity) -> {
                        try {
                            output.add(entry.getKey() + "\t" + phase + "\t" + now
                                    + "\t" + identity + "\t"
                                    + px.invoke(cell) + "," + py.invoke(cell) + ","
                                    + pz.invoke(cell));
                        } catch (ReflectiveOperationException e) {
                            throw new IllegalStateException(e);
                        }
                    };
                    collect.invoke(queue, now, 65_536, callback);
                }
            }
        }
        Files.write(Path.of(args[1]), output, StandardCharsets.UTF_8);
        System.out.println("NATIVE_LEVEL_TICKS_DELIVERY " + output.size());
    }
}
