package probe;

import java.nio.ByteBuffer;
import java.util.Arrays;
import java.util.Locale;
import java.util.function.LongFunction;

import static probe.NativeBridge.*;

public final class Probe {
    private static final int WARMUP = 2_000;
    private static final int FRAME_WARMUP = 12_000;
    private static final int SAMPLES = 10_000;
    private static final int MACHINES = 10_000;
    private static final int NODES = 2_000;
    private static final int ADAPTER_KIND = 0x100;
    private static final int BAD_HANDLE = -14;
    private static final int INVALID_ARGUMENT = -101;
    private static final int QUEUE_FULL = -105;
    private static final int TIME_WENT_BACKWARDS = -106;
    private static final int BUFFER_TOO_SMALL = -107;
    private static final int S5_MAGIC = 0x01013553;
    private static final int S5_HEADER = 32;
    private static final int S5_RECORD_HEADER = 16;

    private static void equal(long actual, long expected, String label) {
        if (actual != expected) {
            throw new AssertionError(label + " expected " + expected + ", got " + actual);
        }
    }

    private static long created(long handle) {
        if (handle <= 0) {
            throw new AssertionError("region creation failed: " + handle);
        }
        return handle;
    }

    private static double percentileUs(long[] sorted, double percentile) {
        return sorted[(int) Math.ceil(percentile * sorted.length) - 1] / 1_000.0;
    }

    private static void printStats(String label, long[] data) {
        Arrays.sort(data);
        System.out.printf(Locale.ROOT, "%s_us p50=%.3f p95=%.3f p99=%.3f%n", label,
                percentileUs(data, .50), percentileUs(data, .95), percentileUs(data, .99));
    }

    private static void tests() {
        equal(transfer(topology(0, NODES, 1)), NODES, "topology accepted");
        equal(transfer(posesAndPaletteDiffs(1)), 4, "pose accepted");
        equal(transfer(logicOutput(1)), 1, "server logic output accepted");
        ByteBuffer bad = topology(0, NODES, 2);
        bad.put(0, (byte) 0);
        equal(transfer(bad), -1, "corrupt magic rejected");
        ByteBuffer crcBad = topology(0, NODES, 2);
        crcBad.put(HEADER + 8, (byte) (crcBad.get(HEADER + 8) ^ 1));
        equal(transfer(crcBad), -2, "corrupt payload rejected");
        equal(transfer(direct(CAPACITY + 1)), -11, "oversized capacity rejected");
        equal(transfer(topology(0, NODES, 1)), -6, "out of order generation rejected");
        equal(transfer(topology(0, NODES, 2)), NODES, "next generation accepted");
        System.out.println("tests=PASS cases=8 (A/B/C accept, corrupt header/payload, oversized, stale generation, next generation)");
    }

    private static void s5Header(ByteBuffer b, int records, long sequence, int dimension, int x, int z) {
        equal(b.getInt(0), S5_MAGIC, "S5 magic/version");
        equal(b.getInt(4), records, "S5 record count");
        equal(b.getLong(8), sequence, "S5 batch sequence");
        equal(b.getInt(16), dimension, "S5 dimension");
        equal(b.getInt(20), x, "S5 region X");
        equal(b.getInt(24), z, "S5 region Z");
        equal(b.getInt(28), 0, "S5 reserved header");
    }

    private static int s5Record(ByteBuffer b, int offset, long tick, int source, int kind, int length) {
        equal(b.getLong(offset), tick, "S5 record tick");
        equal(b.getInt(offset + 8), source, "S5 record source");
        equal(Short.toUnsignedInt(b.getShort(offset + 12)), kind, "S5 record kind");
        equal(Short.toUnsignedInt(b.getShort(offset + 14)), length, "S5 record payload length");
        return offset + S5_RECORD_HEADER;
    }

    private static void adapterTests() {
        ByteBuffer out = direct(128);
        long handle = created(create(-2, -1, 3));
        try {
            ByteBuffer input = topology(0, 1, 1);
            int length = input.limit();
            equal(enqueueDelta(0, ADAPTER_KIND, 7, input, length), BAD_HANDLE, "zero handle");
            equal(enqueueDelta(handle, ADAPTER_KIND, 7, null, length), -10, "null input");
            equal(enqueueDelta(handle, ADAPTER_KIND, 7, input, length - 1), -2, "truncated input");
            equal(enqueueDelta(handle, ADAPTER_KIND, 7, input, CAPACITY + 1), -13, "length exceeds capacity");
            input.put(HEADER, (byte) 1);
            equal(enqueueDelta(handle, ADAPTER_KIND, 7, input, length), -2, "corrupt adapter delta");
            input = topology(0, 1, 1);
            equal(enqueueDelta(handle, 1, 7, input, length), INVALID_ARGUMENT, "reserved S5 kind");
            ByteBuffer invalidNode = topology(NODES, 1, 1);
            equal(enqueueDelta(handle, ADAPTER_KIND, 7, invalidNode, length), -8, "invalid node with valid CRC");
            equal(step(handle, 0, out), 0, "rejected inputs enqueue nothing");
            equal(enqueueDelta(handle, ADAPTER_KIND, 7, input, length), 0, "valid delta at same generation");
            equal(enqueueDelta(handle, ADAPTER_KIND, 7, input, length), -6, "replayed delta");
            ByteBuffer second = topology(1, 1, 2);
            equal(enqueueDelta(handle, ADAPTER_KIND, 8, second, length), 0, "second delta");

            equal(step(handle, 1, null), -10, "null output");
            equal(step(handle, 1, out.asReadOnlyBuffer()), -10, "read-only output");
            equal(step(handle, 1, ByteBuffer.allocate(128)), -11, "heap output");
            equal(step(handle, 1, direct(S5_CAPACITY + 1)), -11, "oversized output");
            equal(step(handle, 1, direct(S5_HEADER - 1)), -11, "short header");
            equal(step(handle, -1, out), INVALID_ARGUMENT, "negative absolute tick");

            int expectedBytes = S5_HEADER + S5_RECORD_HEADER + length;
            ByteBuffer shortOut = direct(expectedBytes - 1);
            for (int i = 0; i < shortOut.capacity(); i++) {
                shortOut.put(i, (byte) 0x5a);
            }
            equal(step(handle, 0, shortOut), BUFFER_TOO_SMALL, "one byte short");
            for (int i = 0; i < shortOut.capacity(); i++) {
                equal(shortOut.get(i), 0x5a, "short output stays untouched");
            }

            // Deliberately fit only one record to check prefix draining and FIFO.
            ByteBuffer prefix = direct(expectedBytes + 1);
            prefix.put(expectedBytes, (byte) 0x5a);
            equal(step(handle, 1, prefix), expectedBytes, "first S5 batch bytes");
            s5Header(prefix, 1, 1, -2, -1, 3);
            int payload = s5Record(prefix, S5_HEADER, 0, 7, ADAPTER_KIND, length);
            for (int i = 0; i < length; i++) {
                equal(prefix.get(payload + i), input.get(i), "adapter input bytes kept intact");
            }
            equal(prefix.get(expectedBytes), 0x5a, "S5 writes only the returned prefix");

            equal(step(handle, 0, out), TIME_WENT_BACKWARDS, "backwards tick");
            equal(step(handle, 1, out), expectedBytes, "same-tick retry drains second record");
            s5Header(out, 1, 2, -2, -1, 3);
            s5Record(out, S5_HEADER, 0, 8, ADAPTER_KIND, length);
            equal(step(handle, 1, out), 0, "drained output");
        } finally {
            equal(destroy(handle), 0, "region destroyed");
        }
        long other = created(create(0, 0, 0));
        try {
            equal(step(handle, 2, out), BAD_HANDLE, "stale handle");
            equal(destroy(handle), BAD_HANDLE, "double destroy");
            equal(step(Long.MAX_VALUE, 2, out), BAD_HANDLE, "forged handle");
            equal(enqueueDelta(other, ADAPTER_KIND, 7, topology(0, 1, 1), HEADER + 8), 0,
                    "generation scoped to region");
        } finally {
            equal(destroy(other), 0, "second region destroyed");
        }
    }

    private static void factoryTests() {
        ByteBuffer out = direct(S5_CAPACITY);
        long handle = created(createFactory(0, 1));
        try {
            equal(factoryPhase(handle, 1), INVALID_ARGUMENT, "early factory phase");
            equal(factoryPhase(handle, 0), 0, "factory feed");
            equal(factoryPhase(handle, 1), INVALID_ARGUMENT, "factory step required");
            equal(step(handle, 1, out), 107, "fluid and transport batch bytes");
            s5Header(out, 3, 1, 0, 0, 0);
            int offset = S5_HEADER;
            for (int tank = 0; tank < 2; tank++) {
                int payload = s5Record(out, offset, 0, tank, 2, 9);
                equal(out.get(payload), 1, "fluid present");
                equal(out.getInt(payload + 1), 1, "fluid id");
                equal(out.getInt(payload + 5), 1, "fluid amount after equalization");
                offset = payload + 9;
            }
            int transport = s5Record(out, offset, 1, 0, 1, 9);
            equal(out.get(transport), 0, "transport reached terminal machine");
            equal(out.getInt(transport + 1), 0, "no downstream id");
            equal(out.getInt(transport + 5), 1, "transported ore id");
            equal(factoryPhase(handle, 1), 0, "factory recipe input");
            equal(step(handle, 2, out), 53, "recipe batch bytes");
            s5Header(out, 1, 2, 0, 0, 0);
            int recipe = s5Record(out, S5_HEADER, 2, 0, 3, 5);
            equal(out.get(recipe), 1, "recipe completed");
            equal(out.getInt(recipe + 1), 1, "recipe id");
            equal(factoryPhase(handle, 2), 0, "factory product and byproduct collectible");
            equal(step(handle, 3, out), 0, "factory sleeps after collection");
        } finally {
            equal(destroy(handle), 0, "factory destroyed");
        }
    }

    private static void backpressureTests() {
        int limit = 4_096;
        int length = HEADER + 8;
        ByteBuffer out = direct(S5_CAPACITY);
        long handle = created(createFactory(0, 1));
        try {
            for (int generation = 1; generation <= limit; generation++) {
                equal(enqueueDelta(handle, ADAPTER_KIND, 7, topology(0, 1, generation), length),
                        0, "fill S5 output queue");
            }
            ByteBuffer retry = topology(0, 1, limit + 1L);
            equal(enqueueDelta(handle, ADAPTER_KIND, 7, retry, length), QUEUE_FULL, "S5 queue full");
            equal(factoryPhase(handle, 0), 0, "feed factory with a full output queue");
            equal(step(handle, 1, out), BACKLOG | (S5_HEADER + (S5_RECORD_HEADER + length) * limit),
                    "step reports due work blocked by output");
            s5Header(out, limit, 1, 0, 0, 0);
            equal(factoryPhase(handle, 1), INVALID_ARGUMENT, "factory still waiting for step");
            equal(enqueueDelta(handle, ADAPTER_KIND, 8, retry, length), 0, "retry same generation");
            equal(step(handle, 1, out), 107 + S5_RECORD_HEADER + length, "same tick completes due work");
            s5Header(out, 4, 2, 0, 0, 0);
            s5Record(out, S5_HEADER, 0, 8, ADAPTER_KIND, length);
            s5Record(out, S5_HEADER + S5_RECORD_HEADER + length, 0, 0, 2, 9);
            equal(factoryPhase(handle, 1), 0, "factory resumes after output drains");
        } finally {
            equal(destroy(handle), 0, "backpressure fixture destroyed");
        }
    }

    private static void frameBenchmark(String label, LongFunction<NativeBridge.Frame> write, int count) {
        long[] directWrites = new long[SAMPLES];
        long[] checksum = new long[SAMPLES];
        long[] frameBuild = new long[SAMPLES];
        long[] jniValidate = new long[SAMPLES];
        for (int i = -FRAME_WARMUP; i < SAMPLES; i++) {
            long generation = i + FRAME_WARMUP + 3;
            long start = System.nanoTime();
            NativeBridge.Frame frame = write.apply(generation);
            long written = System.nanoTime();
            ByteBuffer b = frame.finish();
            int length = b.limit();
            long filled = System.nanoTime();
            int result = transfer(b, length);
            long returned = System.nanoTime();
            equal(result, count, "frame benchmark accepted");
            if (i >= 0) {
                directWrites[i] = written - start;
                checksum[i] = filled - written;
                frameBuild[i] = filled - start;
                jniValidate[i] = returned - filled;
            }
        }
        printStats(label + "_direct_writes", directWrites);
        printStats(label + "_java_crc32_finalize", checksum);
        printStats(label + "_direct_build", frameBuild);
        printStats(label + "_jni_validate_decode", jniValidate);
    }

    private static void perNodeBenchmark() {
        long[] data = new long[100];
        long generation = FRAME_WARMUP + SAMPLES + 2;
        for (int i = -20; i < data.length; i++) {
            long start = System.nanoTime();
            for (int node = 0; node < NODES; node++) {
                equal(transfer(topology(node, 1, ++generation)), 1, "per-node call");
            }
            long elapsed = System.nanoTime() - start;
            if (i >= 0) {
                data[i] = elapsed;
            }
        }
        printStats("scenario_A_2000_per_node_build_and_jni", data);
    }

    private static final class JavaSettled {
        private final int[] input = new int[MACHINES];
        private final int[] output = new int[MACHINES];
        private final long[] readyAt = new long[MACHINES];
        private final boolean[] dirty = new boolean[NODES];

        private JavaSettled() {
            Arrays.fill(input, MACHINES / 2, MACHINES, 1);
            Arrays.fill(output, MACHINES / 2, MACHINES, 1);
            Arrays.fill(readyAt, 2L);
        }

        private int tick(long tick) {
            int ready = 0;
            for (int i = 0; i < MACHINES; i++) {
                if (input[i] != 0 && output[i] < 1 && readyAt[i] <= tick) {
                    ready++;
                }
            }
            for (boolean nodeDirty : dirty) {
                if (nodeDirty) {
                    ready++;
                }
            }
            return ready;
        }
    }

    private static void settledBenchmark() {
        JavaSettled java = new JavaSettled();
        java.input[0] = 1;
        java.dirty[0] = true;
        equal(java.tick(3), 2, "Java baseline recognizes ready machines and dirty nodes");
        java.input[0] = 0;
        java.dirty[0] = false;

        ByteBuffer out = direct(S5_CAPACITY);
        long[] baseline = new long[SAMPLES];
        long[] jniStep = new long[SAMPLES];
        long handle = created(createSettled());
        try {
            out.put(0, (byte) 0x5a);
            for (int i = -WARMUP; i < SAMPLES; i++) {
                long tick = i + WARMUP + 3L;
                long start = System.nanoTime();
                int ready = java.tick(tick);
                long scanned = System.nanoTime();
                long bytes = step(handle, tick, out);
                long returned = System.nanoTime();
                equal(ready, 0, "Java workload stays settled");
                equal(bytes, 0, "S5 workload stays settled");
                if (i >= 0) {
                    baseline[i] = scanned - start;
                    jniStep[i] = returned - scanned;
                }
            }
            equal(out.get(0), 0x5a, "empty S5 batch writes nothing");
            int ticks = 1_000_000;
            long elapsed = settledKernelNanos(handle, WARMUP + SAMPLES + 3L, ticks);
            if (elapsed < 0) {
                throw new AssertionError("settled native scheduler timing failed: " + elapsed);
            }
            System.out.printf(Locale.ROOT, "settled_native_c_abi_step_only ns_per_tick=%.2f ticks=%d%n",
                    (double) elapsed / ticks, ticks);
        } finally {
            equal(destroy(handle), 0, "settled region destroyed");
        }
        printStats("settled_java_full_scan", baseline);
        printStats("settled_jni_step_export", jniStep);
    }

    private static void factoryBenchmark() {
        int active = 500;
        int dormant = 5_000;
        int cycles = 250;
        int warmup = 20;
        long[] transport = new long[cycles];
        long[] recipe = new long[cycles];
        long[] host = new long[cycles];
        long[] whole = new long[cycles];
        ByteBuffer out = direct(S5_CAPACITY);
        long handle = created(createFactory(dormant, active));
        try {
            for (int cycle = 0; cycle < warmup + cycles; cycle++) {
                long start = System.nanoTime();
                equal(factoryPhase(handle, 0), 0, "feed factory");
                long fed = System.nanoTime();
                long first = step(handle, cycle * 2L + 1, out);
                long steppedFirst = System.nanoTime();
                equal(first, S5_HEADER + 75L * active, "transport/fluid batch length");
                equal(out.getInt(4), 3L * active, "transport/fluid record count");
                long verifiedFirst = System.nanoTime();

                equal(factoryPhase(handle, 1), 0, "supply recipes");
                long supplied = System.nanoTime();
                long second = step(handle, cycle * 2L + 2, out);
                long steppedSecond = System.nanoTime();
                equal(second, S5_HEADER + 21L * active, "recipe batch length");
                equal(out.getInt(4), active, "recipe record count");
                equal(out.getLong(8), cycle * 2L + 2, "mixed factory batch sequence");
                long verifiedSecond = System.nanoTime();
                equal(factoryPhase(handle, 2), 0, "collect recipe outputs");
                long end = System.nanoTime();

                if (cycle >= warmup) {
                    int sample = cycle - warmup;
                    transport[sample] = steppedFirst - fed;
                    recipe[sample] = steppedSecond - supplied;
                    host[sample] = (fed - start) + (supplied - verifiedFirst) + (end - verifiedSecond);
                    whole[sample] = end - start;
                }
            }
        } finally {
            equal(destroy(handle), 0, "mixed factory destroyed");
        }
        System.out.printf("mixed_factory dormant=%d active=%d warmup_cycles=%d measured_cycles=%d ticks_per_cycle=2%n",
                dormant, active, warmup, cycles);
        printStats("mixed_transport_fluid_jni_step_export", transport);
        printStats("mixed_recipe_jni_step_export", recipe);
        printStats("mixed_host_actions_per_cycle", host);
        printStats("mixed_full_two_tick_cycle", whole);
    }

    public static void main(String[] args) {
        System.out.println("java_runtime=" + System.getProperty("java.runtime.version"));
        tests();
        adapterTests();
        factoryTests();
        backpressureTests();
        System.out.println("s5_tests=PASS (adapter and kernel batches, corrupt inputs, capacity, sequence, backpressure, invalid handles/ticks)");
        frameBenchmark("scenario_A", gen -> writeTopology(0, NODES, gen), NODES);
        frameBenchmark("scenario_B", NativeBridge::writePosesAndPaletteDiffs, 4);
        perNodeBenchmark();
        equal(transfer(logicOutput(2)), 1, "logic output next generation");
        System.out.println("scenario_A nodes=2000 per_node_frames_bytes=64000 batch_bytes=16024");
        System.out.println("scenario_B contraptions=4 static_blocks=1024 static_resend_bytes=4096 pose_plus_palette_bytes=424");
        System.out.println("ring_slots=4 slot_capacity=65536 footprint_bytes=262144");
        settledBenchmark();
        factoryBenchmark();
    }
}
