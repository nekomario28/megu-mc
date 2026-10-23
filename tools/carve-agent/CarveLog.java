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

    /** InSquarePlacement: the (x,z) it drew, before any filter sees it. */
    public static void insq(int x, int z) {
        String p = System.getProperty("insqchunk");
        if (p == null) return;
        String[] a = p.split(",");
        if (a.length != 2) return;
        if ((x >> 4) != Integer.parseInt(a[0]) || (z >> 4) != Integer.parseInt(a[1])) return;
        if (!overworld()) return;
        int[] f = FEAT.get();
        System.out.println("INSQ " + x + " " + z + " feature=" + f[0] + "," + f[1]);
    }

    /** BiomeFilter verdicts: `-Dbiomefchunk=cx,cz` limits the log to one chunk. */
    /**
     * What the biome filter actually saw, via reflection so no game type has to
     * be named at the instrumentation site (a hand-rolled interface chain through
     * the obfuscated hierarchy is easy to get wrong and fails verification).
     * `ctx.d().t(pos)` is `level.getBiome(pos)`; `ctx.e()` is the top feature.
     */
    public static void biomef3(Object ctx, Object pos, boolean ok) {
        if (System.getProperty("biomefwhy") == null) return;
        int x = 0, y = 0, z = 0;
        try {
            x = (Integer) call(pos, "u");
            y = (Integer) call(pos, "v");
            z = (Integer) call(pos, "w");
        } catch (Throwable t) {
            // positions are always available; a miss just leaves the coords at 0
        }
        String biome = "?", top = "?";
        try {
            Object level = callIn(ctx, new String[] {"dds", "dcz"}, "d", new Object[0]);
            // `dds.t(jd)` is `WorldGenLevel.getBiome(BlockPos)`; several unrelated
            // one-letter methods exist on the hierarchy, so the declaring class is
            // pinned by name (obfuscated, hence the string).
            Object holder = callIn(level, new String[] {"dds", "dcz"}, "t", new Object[] {pos});
            biome = String.valueOf(holder);
        } catch (Throwable t) {
            biome = "err:" + t;
        }
        try {
            top = String.valueOf(call(ctx, "e"));
        } catch (Throwable t) {
            top = "err:" + t;
        }
        System.out.println("BIOMEF3 " + x + " " + y + " " + z + " " + ok
                + " biome=" + biome + " top=" + top);
    }

    /** Invoke the first no-arg (or single-arg) method with that name, whatever its arity. */
    /** Like `call`, but only accepts a method declared by one of `owners`. */
    static Object callIn(Object o, String[] owners, String name, Object[] args) throws Exception {
        java.util.ArrayDeque<Class<?>> q = new java.util.ArrayDeque<>();
        java.util.HashSet<Class<?>> seen = new java.util.HashSet<>();
        q.add(o.getClass());
        while (!q.isEmpty()) {
            Class<?> c = q.poll();
            if (c == null || !seen.add(c)) continue;
            boolean owner = false;
            for (String w : owners) {
                if (c.getName().equals(w) || c.getName().endsWith("." + w)) owner = true;
            }
            if (owner) {
                for (java.lang.reflect.Method m : c.getDeclaredMethods()) {
                    if (!m.getName().equals(name) || m.getParameterCount() != args.length) continue;
                    if (args.length == 1 && args[0] != null
                            && !m.getParameterTypes()[0].isInstance(args[0])) continue;
                    m.setAccessible(true);
                    return m.invoke(o, args);
                }
            }
            q.add(c.getSuperclass());
            for (Class<?> i : c.getInterfaces()) q.add(i);
        }
        throw new NoSuchMethodException(name + " on " + java.util.Arrays.toString(owners));
    }

    static Object call(Object o, String name, Class<?>[] sig, Object... args) throws Exception {
        // the method may be declared on an interface several levels up, so walk
        // both the superclass chain and every interface it implements
        java.util.ArrayDeque<Class<?>> q = new java.util.ArrayDeque<>();
        java.util.HashSet<Class<?>> seen = new java.util.HashSet<>();
        q.add(o.getClass());
        while (!q.isEmpty()) {
            Class<?> c = q.poll();
            if (c == null || !seen.add(c)) continue;
            for (java.lang.reflect.Method m : c.getDeclaredMethods()) {
                if (!m.getName().equals(name) || m.getParameterCount() != args.length) continue;
                // several obfuscated classes reuse one-letter names; the first
                // candidate that accepts the argument is the one we want, and the
                // parameter type is checked here rather than guessed at the call site
                if (args.length == 1 && !m.getParameterTypes()[0].isInstance(args[0])) continue;
                m.setAccessible(true);
                try {
                    return m.invoke(o, args);
                } catch (java.lang.reflect.InvocationTargetException ite) {
                    if (ite.getCause() instanceof NullPointerException) continue;
                    throw ite;
                } catch (Throwable t) {
                    throw (Exception) (t instanceof Exception ? t : new Exception(t));
                }
            }
            q.add(c.getSuperclass());
            for (Class<?> i : c.getInterfaces()) q.add(i);
        }
        throw new NoSuchMethodException(name);
    }

    static Object call(Object o, String name) throws Exception {
        return call(o, name, new Class<?>[0]);
    }

    static Class<?> typeOf(Object o) {
        return o.getClass();
    }

    static final ThreadLocal<int[]> QB = new ThreadLocal<int[]>() {
        @Override protected int[] initialValue() { return new int[] { 0 }; }
    };

    /** `WorldGenRegion.getNoiseBiome(qx, qy, qz)` for one quart cell. */
    public static void qbiome(int qx, int qy, int qz, Object holder) {
        String p = System.getProperty("qbiome");
        if (p == null) return;
        String[] a = p.split(",");
        if (a.length == 1 && a[0].equals("all")) {
            if (QB.get()[0]++ < 24) {
                System.out.println("QBIOME " + qx + " " + qy + " " + qz + " -> " + holder);
            }
            return;
        }
        if (a.length != 3) return;
        if (qx != Integer.parseInt(a[0]) || qy != Integer.parseInt(a[1])
                || qz != Integer.parseInt(a[2])) {
            return;
        }
        System.out.println("QBIOME " + qx + " " + qy + " " + qz + " -> " + holder);
    }

    public static void biomef(int x, int y, int z, boolean ok) {
        String p = System.getProperty("biomefchunk");
        if (p == null) return;
        String[] a = p.split(",");
        if (a.length != 2) return;
        if ((x >> 4) != Integer.parseInt(a[0]) || (z >> 4) != Integer.parseInt(a[1])) return;
        if (!overworld()) return;
        int[] f = FEAT.get();
        System.out.println("BIOMEF " + x + " " + y + " " + z + " " + ok + " feature=" + f[0] + "," + f[1]);
    }

    /** `FeatureSorter.buildFeaturesPerStep`'s biome list, in iteration order. */
    public static void biomes(Object list) {
        if (System.getProperty("biomelist") == null) return;
        System.out.println("BIOMELIST " + list);
    }

    /** Set by ChunkGenerator.applyBiomeDecoration's min build height. */
    static final ThreadLocal<int[]> DIM = new ThreadLocal<int[]>() {
        @Override protected int[] initialValue() { return new int[] { Integer.MIN_VALUE }; }
    };

    /** -64 marks the overworld; every per-chunk log is gated on it. */
    static boolean overworld() {
        String p = System.getProperty("dim");
        if (p == null) return true;
        return DIM.get()[0] == Integer.parseInt(p);
    }

    public static void dim(int minY) {
        DIM.get()[0] = minY;
    }

    static final ThreadLocal<int[]> FEAT = new ThreadLocal<int[]>() {
        @Override protected int[] initialValue() { return new int[] { -1, -1 }; }
    };

    static final ThreadLocal<int[]> CHUNK = new ThreadLocal<int[]>() {
        @Override protected int[] initialValue() { return new int[] { Integer.MIN_VALUE, 0, 0 }; }
    };

    /** setDecorationSeed(seed, x, z): remember the chunk and the level seed. */
    public static void deco(long seed, int x, int z) {
        CHUNK.get()[0] = x >> 4;
        CHUNK.get()[1] = z >> 4;
        // the nether/end decorate with their own seed; -Ddecoseed=0 keeps the
        // overworld only
        String want = System.getProperty("decoseed");
        CHUNK.get()[2] = (want == null || want.equals(Long.toString(seed))) ? 1 : 0;
    }

    /** setFeatureSeed(seed, i, step): log the (i, step) pair for the target chunk. */
    public static void fseed(long seed, int i, int step) {
        // Attribution must be right for *every* chunk, not just the logged one:
        // a tree in a neighbouring chunk is otherwise reported with whatever
        // feature was last logged (that is how "feature=0,10" showed up on trees).
        FEAT.get()[0] = i;
        FEAT.get()[1] = step;
        String p = System.getProperty("fseedchunk");
        if (p == null) return;
        String[] a = p.split(",");
        if (a.length != 2) return;
        int[] c = CHUNK.get();
        if (c[0] != Integer.parseInt(a[0]) || c[1] != Integer.parseInt(a[1])) return;
        if (c[2] != 1 || !overworld()) return;
        System.out.println("FSEED " + seed + " " + i + " " + step);
        String arm = System.getProperty("fseedarm");
        if (arm != null && arm.equals(i + "," + step)) {
            RD.get()[0] = 1;
            RD.get()[1] = 0;
        }
    }

    /** OreFeature origins: `-Dorechunk=cx,cz` limits the log to one chunk. */
    public static void ore(int x, int y, int z, int size) {
        String p = System.getProperty("orechunk");
        if (p == null) return;
        String[] a = p.split(",");
        if (a.length != 2) return;
        // the trunk may sit in a neighbour while its canopy spills into the
        // chunk of interest, so accept a one-chunk radius
        int dx = (x >> 4) - Integer.parseInt(a[0]);
        int dz = (z >> 4) - Integer.parseInt(a[1]);
        if (dx < -1 || dx > 1 || dz < -1 || dz > 1) return;
        if (!overworld()) return;
        int[] f = FEAT.get();
        System.out.println("ORE " + x + " " + y + " " + z + " size=" + size
                + " feature=" + f[0] + "," + f[1]);
    }

    /** TreeFeature.place origins: `-Dtreechunk=cx,cz` limits the log to one chunk. */
    public static void tree(int x, int y, int z) {
        String p = System.getProperty("treechunk");
        if (p == null) return;
        String[] a = p.split(",");
        if (a.length != 2) return;
        // the trunk may sit in a neighbour while its canopy spills into the
        // chunk of interest, so accept a one-chunk radius
        int dx = (x >> 4) - Integer.parseInt(a[0]);
        int dz = (z >> 4) - Integer.parseInt(a[1]);
        if (dx < -1 || dx > 1 || dz < -1 || dz > 1) return;
        if (!overworld()) return;
        int[] f = FEAT.get();
        System.out.println("TREE " + x + " " + y + " " + z + " feature=" + f[0] + "," + f[1]);
    }

    /** Gate for -Dveindump=x,y,z: which cells the vein sampler should report. */
    public static boolean vwant(int x, int y, int z) {
        String p = System.getProperty("veindump");
        return p != null && p.equals(x + "," + y + "," + z);
    }

    /** `LegacyRandomSource.nextInt(bound)`: the bound and the value it returned. */
    public static void rbound(long bound, long val) {
        int[] st = RD.get();
        if (st[0] != 1) return;
        int i = st[1]++;
        if (i >= 24) { st[0] = -1; return; }
        System.out.println("RBOUND " + i + " bound=" + bound + " -> " + val);
    }

    /** `nextInt()` (no bound). */
    public static void rdrawN(int value) {
        int[] st = RD.get();
        if (st[0] != 1) return;
        int i = st[1]++;
        if (i >= 200) { st[0] = -1; return; }
        System.out.println("RDRAW " + i + " nextInt()=" + value);
    }

    /** `nextInt(bound)` with the bound in the label, as the port prints it. */
    public static void rdrawI(int bound, int value) {
        int[] st = RD.get();
        if (st[0] != 1) return;
        int i = st[1]++;
        if (i >= 200) { st[0] = -1; return; }
        System.out.println("RDRAW " + i + " nextInt(" + bound + ")=" + value);
    }

    /** A typed `nextFloat` draw (a float parameter keeps the value intact; the
     *  long-typed logger flattens small floats to 0). */
    public static void rdrawF(String label, float value) {
        int[] st = RD.get();
        if (st[0] != 1) return;
        int i = st[1]++;
        if (i >= 200) { st[0] = -1; return; }
        System.out.println("RDRAW " + i + " " + label + "=" + value);
    }

    /** Bound (0 = nextInt()) and value of one Xoroshiro draw. */
    public static void rdraw2(long bound, long val) {
        int[] st = RD.get();
        if (st[0] != 1) return;
        int i = st[1]++;
        if (i >= 16) { st[0] = -1; return; }
        System.out.println("XDRAW " + i + " bound=" + bound + " -> " + val);
    }

    /** The three density functions OreVeinifier samples for one cell. */
    public static void vdump(int x, int y, int z, double t, double r, double g) {
        System.out.println("VDUMP " + x + " " + y + " " + z + " toggle=" + t + " ridged=" + r + " gap=" + g);
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
