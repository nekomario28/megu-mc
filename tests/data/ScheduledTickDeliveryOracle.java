package dev.megu.tools;

import java.lang.reflect.Constructor;
import java.lang.reflect.Method;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.ArrayList;
import java.util.Comparator;
import java.util.HashMap;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;
import java.util.Optional;
import java.util.Set;
import java.util.function.BiConsumer;
import java.util.function.Function;
import java.util.function.LongPredicate;
import java.util.function.Supplier;
import java.util.stream.Collectors;
import java.util.stream.Stream;

/** Pinned 1.21.1 names: eyr=ProtoChunkTicks, eyo=LevelChunkTicks, eyq=LevelTicks. */
public final class ScheduledTickDeliveryOracle {
    // Reflection keeps this probe outside the signed jar's default package.
    private static final Class<?> POS = type("jd");
    private static final Class<?> CHUNK = type("dcd");
    private static final Class<?> PROTO = type("eyr");
    private static final Class<?> CONTAINER = type("eyo");
    private static final Class<?> LEVEL = type("eyq");
    private static final Class<?> TICK = type("eyt");
    private static final Class<?> PRIORITY = type("eyx");
    private static final Class<?> NBT_LIST = type("uh");
    private static final Object PROFILER = profiler();

    private static final Method PRIORITY_BY_VALUE = method(PRIORITY, "a", int.class);
    private static final Method TYPE = method(TICK, "a");
    private static final Method POSITION = method(TICK, "b");
    private static final Method DUE = method(TICK, "c");
    private static final Method PRIORITY_OF = method(TICK, "d");
    private static final Method ORDER = method(TICK, "e");
    private static final Method X = method(POS, "u");
    private static final Method Y = method(POS, "v");
    private static final Method Z = method(POS, "w");
    private static final Method STAGE = method(PROTO, "a", TICK);
    private static final Method PENDING = method(PROTO, "b");
    private static final Method UNPACK = method(CONTAINER, "a", long.class);
    private static final Method STREAM = method(CONTAINER, "d");
    private static final Method SIZE = method(CONTAINER, "a");
    private static final Method SAVE = method(CONTAINER, "a", long.class, Function.class);
    private static final Method LOAD = method(CONTAINER, "a", NBT_LIST, Function.class, CHUNK);
    private static final Method ADD = method(LEVEL, "a", CHUNK, CONTAINER);
    private static final Method REMOVE = method(LEVEL, "a", CHUNK);
    private static final Method SCHEDULE = method(LEVEL, "a", TICK);
    private static final Method RUN = method(LEVEL, "a", long.class, int.class, BiConsumer.class);

    private static final Constructor<?> NEW_POS = constructor(POS, int.class, int.class, int.class);
    private static final Constructor<?> NEW_CHUNK = constructor(CHUNK, int.class, int.class);
    private static final Constructor<?> NEW_PROTO = constructor(PROTO);
    private static final Constructor<?> EMPTY_CONTAINER = constructor(CONTAINER);
    private static final Constructor<?> PENDING_CONTAINER = constructor(CONTAINER, List.class);
    private static final Constructor<?> NEW_LEVEL =
        constructor(LEVEL, LongPredicate.class, Supplier.class);
    private static final Constructor<?> NEW_TICK =
        constructor(TICK, Object.class, POS, long.class, PRIORITY, long.class);

    private static Class<?> type(String name) {
        try {
            return Class.forName(name);
        } catch (ReflectiveOperationException e) {
            throw new IllegalStateException(e);
        }
    }

    private static Object profiler() {
        try {
            return type("bnc").getField("a").get(null);
        } catch (ReflectiveOperationException e) {
            throw new IllegalStateException(e);
        }
    }

    private static Method method(Class<?> owner, String name, Class<?>... parameters) {
        try {
            return owner.getMethod(name, parameters);
        } catch (ReflectiveOperationException e) {
            throw new IllegalStateException(e);
        }
    }

    private static Constructor<?> constructor(Class<?> owner, Class<?>... parameters) {
        try {
            return owner.getConstructor(parameters);
        } catch (ReflectiveOperationException e) {
            throw new IllegalStateException(e);
        }
    }

    private static Object invoke(Method method, Object receiver, Object... args) {
        try {
            return method.invoke(receiver, args);
        } catch (ReflectiveOperationException e) {
            throw new IllegalStateException(e);
        }
    }

    private static Object create(Constructor<?> constructor, Object... args) {
        try {
            return constructor.newInstance(args);
        } catch (ReflectiveOperationException e) {
            throw new IllegalStateException(e);
        }
    }

    private record Chunk(int x, int z) {
        long key() {
            return Integer.toUnsignedLong(x) | ((long) z << 32);
        }

        Object nativePos() {
            return create(NEW_CHUNK, x, z);
        }
    }

    private record Entry(String type, int x, int y, int z, long due, Enum<?> priority, long order) {
        Chunk chunk() {
            return new Chunk(x >> 4, z >> 4);
        }

        Object nativeTick() {
            return create(NEW_TICK, type, create(NEW_POS, x, y, z), due, priority, order);
        }

        @Override
        public String toString() {
            return type + "," + x + "," + y + "," + z + "," + due + "," + priority.name()
                + "," + order;
        }
    }

    private static final Comparator<Chunk> CHUNK_ORDER =
        Comparator.comparingInt(Chunk::x).thenComparingInt(Chunk::z);

    private static String events(List<Entry> ticks) {
        return ticks.isEmpty() ? "-" :
            ticks.stream().map(Entry::toString).collect(Collectors.joining(";"));
    }

    private static Entry tick(int id, int x, int y, int z, long due, int priority, long order) {
        var pr = (Enum<?>) invoke(PRIORITY_BY_VALUE, null, priority);
        return new Entry(Integer.toString(id).intern(), x, y, z, due, pr, order);
    }

    private static Entry proto(int id, int x, int z) {
        return tick(id, x, 70, z, 0, 0, 500);
    }

    private static Entry readTick(Object tick, Object pos, String type) {
        return new Entry(type, (int) invoke(X, pos), (int) invoke(Y, pos), (int) invoke(Z, pos),
            (long) invoke(DUE, tick), (Enum<?>) invoke(PRIORITY_OF, tick), (long) invoke(ORDER, tick));
    }

    private record Identity(Object pos, String type) {}

    private static final class Queue {
        private final Map<Chunk, Object> chunks = new LinkedHashMap<>();
        private Set<Long> cold = Set.of();
        private Object level = newLevel();

        private Object newLevel() {
            return create(NEW_LEVEL, (LongPredicate) key -> !cold.contains(key),
                (Supplier<Object>) () -> PROFILER);
        }

        void unpack(long now, List<Entry> ticks) {
            Map<Chunk, Object> staging = new LinkedHashMap<>();
            for (var tick : ticks) {
                var proto = staging.computeIfAbsent(tick.chunk(), ignored -> create(NEW_PROTO));
                invoke(STAGE, proto, tick.nativeTick());
            }
            staging.forEach((chunk, proto) -> {
                var active = create(PENDING_CONTAINER, invoke(PENDING, proto));
                invoke(UNPACK, active, now);
                add(chunk, active);
            });
        }

        private void add(Chunk chunk, Object container) {
            if (chunks.putIfAbsent(chunk, container) != null) {
                throw new AssertionError("chunk is already registered: " + chunk);
            }
            attach(chunk);
        }

        void schedule(List<Entry> ticks) {
            for (var tick : ticks) {
                var chunk = tick.chunk();
                if (!chunks.containsKey(chunk)) {
                    add(chunk, create(EMPTY_CONTAINER));
                }
                invoke(SCHEDULE, level, tick.nativeTick());
            }
        }

        List<Entry> deliver(long now, int limit, Set<Long> cold) {
            this.cold = cold;
            var metadata = new HashMap<Identity, Object>();
            chunks.values().forEach(c -> ((Stream<?>) invoke(STREAM, c)).forEach(t -> {
                var key = new Identity(invoke(POSITION, t), (String) invoke(TYPE, t));
                if (metadata.put(key, t) != null) {
                    throw new AssertionError("duplicate live identity");
                }
            }));
            var delivered = new ArrayList<Entry>();
            BiConsumer<Object, String> callback = (pos, type) -> {
                var tick = metadata.remove(new Identity(pos, type));
                if (tick == null) {
                    throw new AssertionError("unexpected callback: " + pos + " " + type);
                }
                var entry = readTick(tick, pos, type);
                if (entry.due() > now) {
                    throw new AssertionError("early callback: " + entry);
                }
                delivered.add(entry);
            };
            invoke(RUN, level, now, limit, callback);
            return delivered;
        }

        void reload(long now) {
            var before = new LinkedHashMap<>(chunks);
            chunks.clear();
            level = newLevel();
            before.keySet().stream().sorted(CHUNK_ORDER).forEach(chunk -> {
                var container = before.get(chunk);
                int remaining = (int) invoke(SIZE, container);
                if (remaining > 0) {
                    var saved = invoke(SAVE, container, now, (Function<String, String>) type -> type);
                    var loaded = invoke(LOAD, null, saved,
                        (Function<String, Optional<String>>) type -> Optional.of(type.intern()),
                        chunk.nativePos());
                    if ((int) invoke(SIZE, loaded) != remaining) {
                        throw new AssertionError("saved tick disappeared from " + chunk);
                    }
                    invoke(UNPACK, loaded, now);
                    add(chunk, loaded);
                }
            });
        }

        void detach(Chunk chunk) {
            invoke(REMOVE, level, chunk.nativePos());
        }

        void attach(Chunk chunk) {
            invoke(ADD, level, chunk.nativePos(), chunks.get(chunk));
        }

        void assertEmpty() {
            if (chunks.values().stream().anyMatch(c -> (int) invoke(SIZE, c) != 0)) {
                throw new AssertionError("probe ended with undelivered ticks");
            }
        }
    }

    private final List<String> rows = new ArrayList<>();
    private Queue blocks;
    private Queue fluids;

    private void scenario(String name) {
        if (blocks != null) {
            blocks.assertEmpty();
            fluids.assertEmpty();
        }
        blocks = new Queue();
        fluids = new Queue();
        rows.add("case\t" + name);
    }

    private void unpack(String phase, long now, Entry... ticks) {
        (phase.equals("B") ? blocks : fluids).unpack(now, List.of(ticks));
        rows.add(String.join("\t", "unpack", phase, "" + now, events(List.of(ticks))));
    }

    private void schedule(String phase, Entry... ticks) {
        (phase.equals("B") ? blocks : fluids).schedule(List.of(ticks));
        rows.add(String.join("\t", "schedule", phase, events(List.of(ticks))));
    }

    private void run(long now, int limit, Chunk... cold) {
        String mask = List.of(cold).stream().map(c -> c.x() + "," + c.z())
            .collect(Collectors.joining(";"));
        var coldKeys = List.of(cold).stream().map(Chunk::key).collect(Collectors.toSet());
        rows.add(String.join("\t", "tick", "" + now, "" + limit, mask,
            events(blocks.deliver(now, limit, coldKeys)),
            events(fluids.deliver(now, limit, coldKeys))));
    }

    private void priorities() {
        scenario("priority_and_requeue");
        // A repeated proto identity must not change the other sub-orders.
        unpack("B", 100, proto(0, 0, 0), proto(0, 0, 0), proto(1, 0, 0), proto(0, 16, 0));
        unpack("F", 100, proto(0, 0, 0), proto(1, 16, 0), proto(2, 17, 0));
        schedule("B",
            tick(0, -16, 70, 0, 100, -3, 8), tick(1, -15, 70, 0, 100, -3, 2),
            tick(0,  17, 70, 0,  99, -2, 9), tick(1,  18, 70, 0, 100, -1, 3),
            tick(1,  19, 70, 0, 100,  1, 1), tick(1, -14, 70, 0, 100,  2, 4),
            tick(0,   1, 70, 0, 100,  3, 5), tick(0,   2, 70, 0, 102, -3, 6));
        schedule("F",
            tick(0,  18, 70, 0, 100, -3, 4), tick(1, 1, 70, 0, 100, -3, 1),
            tick(2, -16, 70, 0, 100, -2, 3), tick(3, 2, 70, 0, 100, -1, 2),
            tick(1, -15, 70, 0, 100,  1, 5), tick(2, 3, 70, 0, 100,  2, 6),
            tick(3,  19, 70, 0, 100,  3, 7), tick(0, 4, 70, 0, 102, -3, 8));
        run(98, 100);
        run(100, 4);
        run(101, 4);
        run(102, 100);
    }

    private void eligibility() {
        scenario("eligibility_and_unload");
        unpack("B", 110, proto(0, 0, 0), proto(1, 16, 0), proto(1, 17, 0));
        unpack("F", 110, proto(0, 0, 0), proto(1, 16, 0), proto(2, 17, 0));
        var detached = new Chunk(1, 0);
        blocks.detach(detached);
        fluids.detach(detached);
        run(110, 0, detached);
        run(111, 1, detached);
        blocks.attach(detached);
        fluids.attach(detached);
        run(112, 2, detached);
        run(113, 1);
        run(114, 1);
        run(115, 1);
    }

    private void wrapped() {
        scenario("wrapped_deadline_table");
        // All four nonzero chunk keys hash to slot 30 in the initial 32-slot map.
        // Removing the two high slots moves the low slots across the iterator.
        unpack("B", 120, proto(0, 0, -16), proto(0, 0, 32),
            proto(0, 16, 96), proto(0, 64, -16));
        unpack("F", 120, proto(0, 0, -16), proto(0, 0, 32),
            proto(0, 16, 96), proto(0, 64, -16));
        run(120, 2);
        run(121, 2, new Chunk(0, -1));
        run(122, 2);
    }

    private void grownDeadlineTable() {
        scenario("grown_deadline_table");
        // Twenty-five chunks grow the table to 64 slots. Four colliding keys
        // wrap there as well, with fewer than twelve entries left by the time
        // the native iterator removes the first wrapped key.
        int[][] positions = {
            {0, 2}, {4, -1}, {5, 2}, {6, 9},
            {-7, -7}, {-7, -6}, {-7, -5}, {-7, -4}, {-7, -2}, {-7, -1},
            {-7, 1}, {-7, 2}, {-7, 4}, {-7, 5}, {-6, -6}, {-6, -5},
            {-6, -1}, {-6, 0}, {-6, 1}, {-6, 3}, {-6, 4}, {-6, 7},
            {-5, -7}, {-5, -5}, {-5, -4}
        };
        var ticks = new ArrayList<Entry>();
        for (int[] position : positions) {
            ticks.add(proto(0, position[0] * 16, position[1] * 16));
        }
        unpack("B", 125, ticks.toArray(Entry[]::new));
        unpack("F", 125, ticks.toArray(Entry[]::new));
        var cold = new Chunk(-6, -6);
        run(125, 18, cold);
        run(126, 2, cold);
        run(127, 100);
    }

    private void restart(boolean reload) {
        scenario(reload ? "save_reload" : "uninterrupted");
        unpack("B", 130, proto(0, 0, 0), proto(1, 16, 0), proto(1, 17, 0));
        unpack("F", 130, proto(0, 0, 0), proto(1, 16, 0), proto(2, 17, 0));
        schedule("B",
            tick(0,  1, 70, 0, 130, -3, 10), tick(0,  2, 70, 0, 130, 0, 11),
            tick(1, 18, 70, 0, 130,  0, 12), tick(1, 19, 70, 0, 130, 0, 13),
            tick(0,  3, 70, 0, 134, -2, 14), tick(1,  2, 70, 0, 130, 0, 15));
        schedule("F",
            tick(1, 18, 70, 0, 130, -3, 10), tick(2,  1, 70, 0, 130, 0, 11),
            tick(2, 19, 70, 0, 130,  0, 12), tick(3, 20, 70, 0, 130, 0, 13),
            tick(0,  3, 70, 0, 134, -2, 14), tick(1,  1, 70, 0, 130, 0, 15));
        var cold = new Chunk(1, 0);
        run(130, 2, cold);
        if (reload) {
            blocks.reload(131);
            fluids.reload(131);
            rows.add("reload\t131");
        }
        // Runtime counters continue independently of the negative loaded order.
        schedule("B", tick(1, 4, 70, 0, 131, 0, 16));
        schedule("F", tick(3, 4, 70, 0, 131, 0, 16));
        run(131, 3, cold);
        run(132, 3);
        run(133, 100);
        run(134, 100);
    }

    public static void main(String[] args) throws Exception {
        var probe = new ScheduledTickDeliveryOracle();
        probe.priorities();
        probe.eligibility();
        probe.wrapped();
        probe.grownDeadlineTable();
        probe.restart(false);
        probe.restart(true);
        probe.blocks.assertEmpty();
        probe.fluids.assertEmpty();
        Files.write(Path.of(args[0]), probe.rows);
    }
}
