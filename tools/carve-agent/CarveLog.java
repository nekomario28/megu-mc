/** Bootstrap-visible logger for the instrumented carver hooks (no game types). */
public class CarveLog {
    static final ThreadLocal<int[]> TGT = new ThreadLocal<int[]>() {
        @Override protected int[] initialValue() { return new int[]{Integer.MIN_VALUE, Integer.MIN_VALUE}; }
    };
    public static void target(int tx, int tz) { TGT.set(new int[]{tx, tz}); }
    static final ThreadLocal<int[]> SRC = new ThreadLocal<int[]>() {
        @Override protected int[] initialValue() { return new int[]{Integer.MIN_VALUE, Integer.MIN_VALUE}; }
    };
    public static void enter(int sx, int sz) { SRC.set(new int[]{sx, sz}); }
    public static void exit() { SRC.set(new int[]{Integer.MIN_VALUE, Integer.MIN_VALUE}); }
    public static void pre(int tx, int tz, int sx, int sz, int k, long seed) {
        System.out.println("APRE t=" + tx + "," + tz + " s=" + sx + "," + sz + " k=" + k + " seed=" + seed);
    }
    public static void post(int tx, int tz, int sx, int sz, int k, boolean start) {
        System.out.println("APOST t=" + tx + "," + tz + " s=" + sx + "," + sz + " k=" + k + " start=" + start);
    }
    /** Only log the reseed for the chunk named by -Drseed=<cx>,<cz>. */
    static boolean rseedOn(int cx, int cz) {
        String p = System.getProperty("rseed");
        return p != null && p.equals(cx + "," + cz);
    }

    /** Per-thread draw counter, armed by c(JII) for the chunk named by -Drseed. */
    static final ThreadLocal<int[]> RD = new ThreadLocal<int[]>() {
        @Override protected int[] initialValue() { return new int[] { -1, 0 }; }
    };

    static void arm(int cx, int cz) {
        String p = System.getProperty("rseed");
        RD.get()[0] = (p != null && p.equals(cx + "," + cz)) ? 1 : -1;
        RD.get()[1] = 0;
    }

    /** dyz.c(int) is the single funnel every dyn draw goes through. */
    public static void rbits(long bits, long val) {
        int[] st = RD.get();
        if (st[0] != 1) return;
        int i = st[1]++;
        if (i >= 16) { st[0] = -1; return; }
        System.out.println("RBITS " + i + " bits=" + bits + " -> " + val);
    }

    public static void rdraw(String m, long arg, long val) {
        int[] st = RD.get();
        if (st[0] != 1) return;
        int i = st[1]++;
        if (i >= 16) { st[0] = -1; return; }
        System.out.println("RDRAW " + i + " " + m + "(" + arg + ")=" + val);
    }

    public static void rseedIn(int cx, int cz, long arg) {
        if (!rseedOn(cx, cz)) return;
        System.out.println("RSIN s=" + cx + "," + cz + " arg=" + arg);
    }

    public static void rseedOut(int cx, int cz, Object src) {
        if (!rseedOn(cx, cz)) return;
        try {
            Class<?> k = src.getClass();
            java.lang.reflect.Field f = null;
            while (k != null && f == null) {
                try { f = k.getDeclaredField("h"); }
                catch (NoSuchFieldException e) { k = k.getSuperclass(); }
            }
            f.setAccessible(true);
            Object v = f.get(src);
            long lcg = (Long) v.getClass().getMethod("get").invoke(v);
            System.out.println("RSOUT s=" + cx + "," + cz + " lcg=" + lcg);
            arm(cx, cz);
        } catch (Throwable t) {
            System.out.println("RSOUT s=" + cx + "," + cz + " err=" + t);
        }
    }

    public static void tunnel(long seed, double x, double y, double z, float yaw, float pitch, int count) {
        int[] s = SRC.get();
        System.out.println("ATUN s=" + s[0] + "," + s[1] + " seed=" + seed
            + " x=" + (long) Math.floor(x) + " y=" + (long) Math.floor(y) + " z=" + (long) Math.floor(z)
            + " yaw=" + yaw + " pitch=" + pitch + " count=" + count);
    }
    public static void state(int x, int y, int z, int id) {
        int[] s = SRC.get();
        int[] t = TGT.get();
        System.out.println("ASTATE t=" + t[0] + "," + t[1] + " s=" + s[0] + "," + s[1]
            + " " + x + " " + y + " " + z + " " + id);
    }
}
