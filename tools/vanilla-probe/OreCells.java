import java.lang.reflect.*;
import java.util.*;

/** Run vanilla's OreFeature and record the cells it writes (fake level + section). */
public class OreCells {
    static final List<int[]> cells = new ArrayList<>();
    static int draws = 0;
    static int lastIdx = 0;
    static sun.misc.Unsafe U;

    static void initUnsafe() throws Exception {
        Field f = sun.misc.Unsafe.class.getDeclaredField("theUnsafe");
        f.setAccessible(true);
        U = (sun.misc.Unsafe) f.get(null);
    }

    static int[] xyz(Object pos) throws Exception {
        Class<?> c = pos.getClass();
        return new int[]{(int) c.getMethod("u").invoke(pos),
                         (int) c.getMethod("v").invoke(pos),
                         (int) c.getMethod("w").invoke(pos)};
    }

    public static class FakeSection extends dvj {
        int idx, cx, cz;
        public FakeSection() { super((jz<ddw>) null); this.idx = 0; }

        public dtc a(int lx, int ly, int lz) { return null; }
        public dtc a(int lx, int ly, int lz, dtc st) { return a(lx, ly, lz, st, false); }
        public dtc a(int lx, int ly, int lz, dtc st, boolean moved) {
            cells.add(new int[]{cx * 16 + lx, (idx << 4) - 64 + ly, cz * 16 + lz});
            return null;
        }
        public void a() {}
        public void b() {}
    }

    public static class FakeChunk extends duy {
        int cx, cz;
        FakeSection section;
        java.util.Map<Integer, FakeSection> byIdx;
        FakeChunk() { super(null, null, null, null, 0L, null, null); }
        public dvj b(int idx) {
            lastIdx = idx;
            return byIdx.computeIfAbsent(idx, k -> {
                try {
                    FakeSection s = (FakeSection) U.allocateInstance(FakeSection.class);
                    s.idx = k; s.cx = cx; s.cz = cz;
                    return s;
                } catch (Exception e) { throw new RuntimeException(e); }
            });
        }
        public dtc a(jd pos, dtc st, boolean moved) { return null; }
        public void a(dqh k) {}
        public void a(bsr s) {}
        public dvz j() { return null; }
        public void d(jd pos) {}
        public ub a(jd pos, jo.a flags) { return null; }
        public eyw<dfy> o() { return null; }
        public eyw<epd> p() { return null; }
        public duy.a q() { return null; }
        public epe b_(jd pos) { return null; }
        public dtc a_(jd pos) { return null; }
        public dqh c_(jd pos) { return null; }
    }

    public static void main(String[] args) throws Exception {
        initUnsafe();
        ab.a();
        akt.a();
        long seed = Long.parseLong(args[0]);
        int px = Integer.parseInt(args[1]), py = Integer.parseInt(args[2]), pz = Integer.parseInt(args[3]);
        int size = Integer.parseInt(args[4]);

        FakeSection section = (FakeSection) U.allocateInstance(FakeSection.class);
        java.util.Map<Long, FakeChunk> chunks = new java.util.HashMap<>();

        enn rule = new enn() {
            public boolean a(dtc state, ayw random) { return true; }
            protected eno<?> a() { return null; }
        };
        eeq config = new eeq(List.of(eeq.a(rule, (dtc) null)), size, 0.0f);
        ecz feature = new ecz(eeq.a);

        dzz real = new dzz(seed);
        ayw random = (ayw) Proxy.newProxyInstance(OreCells.class.getClassLoader(),
                new Class<?>[]{ayw.class}, (proxy, method, margs) -> {
                    Object r;
                    try { r = method.invoke(real, margs); }
                    catch (InvocationTargetException e) { throw e.getCause(); }
                    String n = method.getName();
                    if (n.equals("g") || n.equals("i") || n.equals("j") || n.equals("f") || n.equals("a")) draws++;
                    return r;
                });

        dds level = (dds) Proxy.newProxyInstance(OreCells.class.getClassLoader(),
                new Class<?>[]{dds.class}, (proxy, method, margs) -> {
                    String n = method.getName();
                    Class<?>[] pt = method.getParameterTypes();
                    if (n.equals("toString")) return "fake";
                    if (n.equals("hashCode")) return 1;
                    if (n.equals("equals")) return proxy == margs[0];
                    if (n.equals("f_")) return true;             // hasChunkAt
                    if (n.equals("a") && pt.length == 2 && pt[0] == int.class && pt[1] == int.class
                            && method.getReturnType() == duy.class) {
                        int cxx = (int) margs[0], czz = (int) margs[1];
                        return chunks.computeIfAbsent(((long) cxx << 32) ^ (czz & 0xFFFFFFFFL), k -> {
                            try {
                                FakeChunk fc = (FakeChunk) U.allocateInstance(FakeChunk.class);
                                fc.cx = cxx; fc.cz = czz; fc.byIdx = new java.util.HashMap<>();
                                return fc;
                            } catch (Exception e) { throw new RuntimeException(e); }
                        });
                    }
                    if (n.equals("e") && pt.length == 1 && pt[0] == int.class) return (int)(margs[0]) + 64 >> 4;   // section index
                    if (n.equals("an") && pt.length == 0 && method.getReturnType() == int.class) return 24;
                    if (n.equals("d") && pt.length == 1 && method.getReturnType() == boolean.class) return false;
                    if (n.equals("a") && pt.length == 3 && pt[1] == int.class && pt[2] == int.class
                            && method.getReturnType() == int.class) return 320;   // getHeight
                    Class<?> rt = method.getReturnType();
                    if (rt == boolean.class) return false;
                    if (rt == int.class) return 0;
                    if (rt == long.class) return 0L;
                    if (rt == float.class) return 0f;
                    if (rt == double.class) return 0d;
                    return null;
                });

        ecg<eeq> ctx = new ecg<>(Optional.empty(), level, null, random, new jd(px, py, pz), config);
        boolean ok = feature.a(ctx);
        System.out.println("placed=" + ok + " cells=" + cells.size() + " draws=" + draws);
        Set<String> uniq = new TreeSet<>();
        for (int[] c : cells) uniq.add(((px & ~15) + c[0]) + " " + c[1] + " " + ((pz & ~15) + c[2]));
        for (String s : uniq) System.out.println("CELL " + s);
    }
}
