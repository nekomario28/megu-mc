/**
 * Per-update state of one sculk charge cursor (`SculkSpreader$ChargeCursor.update`).
 *
 * Takes only primitives and JDK types: the game's classes live in the bundler's child
 * loader, so the logger must never name one. Each line carries the spreader round
 * (`SculkSpreader.updateCursors` call) and the cursor's index inside that round, because a
 * cursor that reaches charge 0 is dropped from the list: without those two numbers the two
 * traces cannot be aligned past the first dead cursor. `facings` is reduced to the face mask
 * over Direction's declaration order (down, up, north, south, west, east), which is the same
 * index the port's own mask uses; `-1` means the set is null (the spreader's SAME_POSITION
 * variant), `0` an empty set (the all-types variant).
 */
public class SculkLog {
    static int round = -1;
    static int idx = 0;

    /**
     * Writes trace lines to our own file instead of `System.out`.
     *
     * The game routes stdout through log4j, and the console appender does not preserve a
     * thread's line order (measured: in `Worker-Main-5`'s lines the sculk windows hold none of
     * the placement's draws, while the same thread's stream holds 41208 of them - so anything
     * that reads a window or a pass positionally off `run.log` is reading scrambled order).
     * A single lock plus a buffered writer keeps the true order; a shutdown hook flushes.
     */
    private static final Object LOCK = new Object();
    private static java.io.PrintWriter writer;

    private static java.io.PrintWriter writer() {
        if (writer == null) {
            String path = System.getProperty("sculktrace", "sculk-trace.txt");
            try {
                writer = new java.io.PrintWriter(new java.io.BufferedWriter(
                        new java.io.FileWriter(path, false), 1 << 16));
            } catch (java.io.IOException e) {
                throw new RuntimeException(e);
            }
            Runtime.getRuntime().addShutdownHook(new Thread() {
                @Override public void run() {
                    synchronized (LOCK) {
                        writer.flush();
                        writer.close();
                    }
                }
            });
        }
        return writer;
    }

    /**
     * A line from another logger, so it lands in the same order-preserving file.
     *
     * `CarveLog` writes through `System.out`, i.e. log4j, whose console appender does not keep a
     * thread's line order (measured: a sculk window holding none of its own draws while the same
     * thread's stream held 41208 of them). Any trace whose *sequence* is read has to come through
     * here - `BIOMEF` is one: comparing the candidate *order* against the port's is the next
     * measurement for the draw-25 divergence, and it is unanswerable from `run.log`.
     */
    public static void line(String text) {
        out(text);
    }

    private static void out(String line) {
        if (Boolean.getBoolean("sculkquiet")) return;
        synchronized (LOCK) {
            // The thread is part of the record: the lock keeps lines whole, but placements run on
            // many worker threads, so the file's global order is not any one thread's program
            // order. Reading a single window off the file mixed one thread's sculk placement with
            // another's feature placements (measured: 28 consecutive `SPLACE ecw` lines inside
            // what looked like one sculk window). With the tag a reader gets per-thread order.
            writer().println("[w=" + Thread.currentThread().getName() + "] " + line);
        }
    }

    /** `SculkSpreader.updateCursors` entry: one round, cursor index starts over. */
    public static void round() {
        round++;
        idx = 0;
    }

    /**
     * Entry marker for one sculk call site, so the native log names each step.
     *
     * `cursor` prints the index it is about to advance past, so the current cursor's index is
     * `idx - 1` here; printing `idx` would label every marker as the next cursor's.
     */
    public static void mark(String what) {
        if (Boolean.getBoolean("sculkquiet")) return;
        out("SCMARK " + what + " r=" + round + " i=" + (idx - 1));
    }

    /** `MultifaceSpreader.spreadAll` entry: the number of spread types its config carries. */
    public static void spreadall(int types) {
        if (Boolean.getBoolean("sculkquiet")) return;
        out("SSPREADALL types=" + types + " r=" + round + " i=" + (idx - 1));
    }

    /** `ChunkGenerator.applyBiomeDecoration` entry: the chunk whose decoration pass this is.
     *
     * One pass covers the 3x3 biome sources around its chunk (vanilla's `ChunkPos.rangeClosed`),
     * so the `SPLACE` lines of a chunk are logged under nine different base positions and only
     * the pass marker says which decoration pass they belong to.
     */
    public static void pass(int cx, int cz) {
        // also recorded on the thread, because a placement window carries no coordinates of its own
        // and two instruments (`BIOMEF` and `SPWINDOW`) have to be joinable to be reconciled
        PASS.get()[0] = cx;
        PASS.get()[1] = cz;
        if (Boolean.getBoolean("sculkquiet")) return;
        out("SPASS " + cx + " " + cz);
    }

    /** `PlacedFeature.placeWithBiomeCheck` entry: which feature is attempted, in which chunk.
     *
     * One line per attempt, in attempt order, so the *placement pipeline* can be compared with
     * the port's `SATTEMPT` trace. The feature arrives as its class name (the logger lives in the
     * bootstrap loader and cannot reference a game class); the chunk is derived from the base
     * position the chunk generator passes in.
     */
    public static void place(String featureClass, int cx, int cz) {
        // Arming here (and disarming at every return of the same method) is what keeps the draw
        // trace small: only the draws made while a sculk patch is being placed are logged.
        // Arming is *not* done here. It was, and that produced a second mechanism competing with
        // `armPlacement` (which arms at `PlacedFeature.placeWithBiomeCheck`, where the position
        // draws are made): a non-sculk feature placed inside an armed window flipped the flag off
        // and the draws stopped mid-window while the non-draw markers continued - a trace that
        // looks like "nothing draws here" rather than "the instrument stopped looking". One place
        // arms, one place disarms.
        if (Boolean.getBoolean("sculkquiet")) return;
        out("SPLACE " + featureClass + " " + cx + " " + cz);
    }

    /**
     * `Util.shuffledCopy(list, random)` - logged with the list's size and its caller.
     *
     * `Util.shuffle` is Fisher-Yates (`nextInt(i+1)` for i = n-1..1), so a shuffle of an n-element
     * list draws `nextInt(n)`, `nextInt(n-1)`, ... The bound alone therefore fingerprints the list
     * length: a `(49)` draw is a 50-element list. Reading which class asks for a 50-element shuffle
     * out of bytecode meant walking callers by hand; one line here answers it, and the caller class
     * comes from a stack trace because the shuffle call itself carries no clue.
     *
     * Gated on the placement window (`armed`) so the volume stays to what is being investigated.
     */
    public static void shuffle(int size, String caller) {
        if (armed()) out("SSHUFFLE size=" + size + " caller=" + caller);
    }

    /**
     * Entry of `PlacedFeature.placeWithBiomeCheck` - the point *before* the placement modifiers.
     *
     * Measured why this is the right arm point: the modifiers (which draw the position) run in
     * `PlacedFeature.placeWithBiomeCheck` -> `placeWithContext`, *before* it calls
     * `ConfiguredFeature.place`. Arming on `ConfiguredFeature.place` produced sculk windows with
     * no position draw in them at all (SPLACE -> SRNG -> SPATCH, nothing between), and `ebq` -
     * which this agent originally labelled `PlacedFeature` - is `ConfiguredFeature`.
     */
    public static void armPlacement() {
        ARMED.set(Boolean.TRUE);
    }

    /** The placement window's boundaries, so the trace carries the unit a port run covers.
     *
     * A port `FSEED_ARM=cx,cz,gi,step` run logs one *placement*: the modifier draws for every
     * candidate plus every accepted position's feature call. The agent only marked
     * `ConfiguredFeature.place` (`SPLACE`), which fires once per accepted position, so segmenting a
     * thread at `SPLACE` produced ~20 windows per placement with ~35 draws each while the port's
     * window held 769 - a comparison between different units, not a difference in behaviour.
     */
    public static void windowStart() {
        WIN.get()[1]++;                    // depth: placements nest
        if (WIN.get()[1] == 1) {
            WIN.get()[0] = SERIAL.incrementAndGet();
        }
        out("SPWINDOW start s=" + WIN.get()[0] + " d=" + WIN.get()[1]
                + " pass=" + PASS.get()[0] + "," + PASS.get()[1]);
    }

    public static void windowEnd() {
        int depth = WIN.get()[1]--;
        out("SPWINDOW end s=" + WIN.get()[0] + " d=" + depth);
    }

    /** The window a `BIOMEF`/`SPATCH` line belongs to, so two instruments can be joined.
     *
     * This exists because two of them disagreed: `BIOMEF` counted 256 candidates for one chunk's
     * sculk placement while `SPWINDOW`-delimited windows held at most 381 draws, and no 768-draw
     * window existed in that pass at all. A contradiction between instruments is not settled by
     * believing one - it is settled by giving both the same identifier and counting again.
     */
    public static int windowSerial() {
        return WIN.get()[0];
    }

    private static final java.util.concurrent.atomic.AtomicInteger SERIAL =
            new java.util.concurrent.atomic.AtomicInteger();
    private static final ThreadLocal<int[]> WIN = ThreadLocal.withInitial(() -> new int[]{0, 0});
    private static final ThreadLocal<int[]> PASS = ThreadLocal.withInitial(() -> new int[]{0, 0});


    /** The class of the `RandomSource` a placement was handed. */
    public static void rngClass(String name) {
        if (Boolean.getBoolean("sculkquiet")) return;
        out("SRNG " + name);
    }

    /** Called before every `return` of `PlacedFeature.placeWithBiomeCheck`. */
    public static void disarm() {
        ARMED.remove();
    }

    private static final ThreadLocal<Boolean> ARMED = new ThreadLocal<>();

    private static boolean armed() {
        return Boolean.TRUE.equals(ARMED.get());
    }

    public static void drawI(int value, String kind) {
        if (armed()) out("SDRAW " + kind + " " + value);
    }

    /**
     * A bounded draw, with the bound it was given.
     *
     * The bound is what makes the native trace joinable against the port's: the port logs
     * `nextInt(16)=15` (bound and value), and without the bound the native side is a bare stream
     * of integers that can only be compared positionally. Argument order keeps the value DUP-ed
     * on the stack so it survives the call.
     */
    public static void drawIBound(int value, int bound, String kind) {
        if (!armed()) return;
        out("SDRAW " + kind + "(" + bound + ") " + value);
        // A bound that is neither a placement modifier's nor a known shuffle gives no clue on its
        // own, and walking callers out of bytecode by hand is how three hypotheses died already.
        // For those, one stack trace names the caller at the moment it happens.
        if (bound != 16 && bound != 321) {
            StackTraceElement[] st = new Throwable().getStackTrace();
            StringBuilder sb = new StringBuilder("STRACE bound=").append(bound).append(" value=")
                    .append(value);
            for (int i = 1; i < st.length && i <= 3; i++) {
                sb.append(" | ").append(st[i].getClassName()).append('.').append(st[i].getMethodName());
            }
            out(sb.toString());
        }
    }

    public static void drawJ(long value, String kind) {
        if (armed()) out("SDRAW " + kind + " " + value);
    }

    public static void drawD(double value, String kind) {
        if (armed()) out("SDRAW " + kind + " " + value);
    }

    public static void drawF(float value, String kind) {
        if (armed()) out("SDRAW " + kind + " " + value);
    }

    /** `SculkPatchFeature.place` entry: the patch's origin, so a run's trace splits by patch. */
    public static void patch(int x, int y, int z) {
        if (Boolean.getBoolean("sculkquiet")) return;
        out("SPATCH " + x + " " + y + " " + z + " s=" + windowSerial());
    }

    public static void cursor(int x, int y, int z, int charge, int decayDelay, int updateDelay,
                              java.util.Set<?> facings) {
        if (Boolean.getBoolean("sculkquiet")) return;
        int mask = -1;
        if (facings != null) {
            mask = 0;
            for (Object o : facings) {
                if (o instanceof Enum<?> e) mask |= 1 << e.ordinal();
            }
        }
        out("SCURSOR r=" + round + " i=" + (idx++) + " " + x + " " + y + " " + z
                + " " + charge + " " + decayDelay + " " + updateDelay + " " + mask);
    }
}
