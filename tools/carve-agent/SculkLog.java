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

    private static void out(String line) {
        if (Boolean.getBoolean("sculkquiet")) return;
        synchronized (LOCK) {
            writer().println(line);
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
        // Thread-local, not a single flag: placements run on many worker threads at once, and a
        // shared flag is flipped by every other thread's `place` call (measured: sculk windows
        // came out with 0 draws while one window collected 12932 of another thread's).
        if ("edi".equals(featureClass)) ARMED.set(Boolean.TRUE); else ARMED.remove();
        if (Boolean.getBoolean("sculkquiet")) return;
        out("SPLACE " + featureClass + " " + cx + " " + cz);
    }

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
        out("SPATCH " + x + " " + y + " " + z);
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
