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
        System.out.println("SCMARK " + what + " r=" + round + " i=" + (idx - 1));
    }

    /** `MultifaceSpreader.spreadAll` entry: the number of spread types its config carries. */
    public static void spreadall(int types) {
        if (Boolean.getBoolean("sculkquiet")) return;
        System.out.println("SSPREADALL types=" + types + " r=" + round + " i=" + (idx - 1));
    }

    /** `SculkPatchFeature.place` entry: the patch's origin, so a run's trace splits by patch. */
    public static void patch(int x, int y, int z) {
        if (Boolean.getBoolean("sculkquiet")) return;
        System.out.println("SPATCH " + x + " " + y + " " + z);
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
        System.out.println("SCURSOR r=" + round + " i=" + (idx++) + " " + x + " " + y + " " + z
                + " " + charge + " " + decayDelay + " " + updateDelay + " " + mask);
    }
}
