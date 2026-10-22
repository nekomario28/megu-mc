import java.lang.reflect.*;
import java.util.*;

/**
 * Run vanilla's CaveWorldCarver against an all-stone fake chunk and record every
 * cell it writes.
 *
 * Why a probe: a port's tunnel path is decided by a chain of float draws
 * (`nextFloat`, `nextInt`, `Mth.sin/cos`, `skip`). Reading the bytecode gets the
 * draws right; only running vanilla says whether the *geometry* is right. With an
 * all-stone world the recorded set is exactly the carver's contribution, so a port
 * can be compared cell for cell without a whole worldgen behind it.
 *
 *   javac -cp sj:<server libraries> -d classes CarverProbe.java
 *   java  -cp sj:<server libraries>:classes CarverProbe <seed> <tx> <tz> <ox> <oz>
 */
public class CarverProbe {
    static final List<int[]> cells = new ArrayList<>();
    static sun.misc.Unsafe U;
    static dtc STONE;
    static dtc CAVE_AIR;

    static void initUnsafe() throws Exception {
        Field f = sun.misc.Unsafe.class.getDeclaredField("theUnsafe");
        f.setAccessible(true);
        U = (sun.misc.Unsafe) f.get(null);
    }

    static void setInt(Object o, Class<?> owner, String name, int v) throws Exception {
        Field f = owner.getDeclaredField(name);
        U.putInt(o, U.objectFieldOffset(f), v);
    }

    /** Blocks.STONE / Blocks.CAVE_AIR by registry name, without guessing the obf fields. */
    static dtc block(String name) throws Exception {
        for (Field f : dga.class.getDeclaredFields()) {
            if (f.getType() != dfy.class) continue;
            Object b = f.get(null);
            if (b.toString().endsWith(":" + name + "}")) return ((dfy) b).o();
        }
        throw new IllegalStateException("no block " + name);
    }

    /** -Dhoriz=1.0 pins a provider to a constant; otherwise it is uniform(min,max). */
    static bpu fp(String key, float lo, float hi) {
        String v = System.getProperty(key);
        return v == null ? bqb.b(lo, hi) : bps.a(Float.parseFloat(v));
    }

    public static class LogMask extends dux {
        int gets = 0;
        LogMask() { super(384, -64); }
        @Override public void a(int x, int y, int z) {
            if (LEG) System.out.println("SET " + x + "," + y + "," + z);
            super.a(x, y, z);
        }
        @Override public boolean b(int x, int y, int z) {
            gets++;
            if (LEG) System.out.println("GET " + x + "," + y + "," + z);
            return super.b(x, y, z);
        }
    }

    static boolean LEG = System.getProperty("leg") != null;

    public static class FakeChunk extends duy {
        dcd pos;
        FakeChunk() { super(null, null, null, null, 0L, null, null); }
        int getPosCalls = 0;
        public dcd f() {
            getPosCalls++;
            if (LEG && getPosCalls <= 6) {
                StackTraceElement[] st = Thread.currentThread().getStackTrace();
                System.out.println("GETPOS from " + st[2] + " / " + st[3]);
            }
            return pos;
        }
        int reads = 0;
        public dtc a_(jd p) { reads++; return STONE; }
        public dtc a(jd p, dtc st, boolean moved) {
            cells.add(new int[]{p.u(), p.v(), p.w(), st == CAVE_AIR ? 0 : st == CAVE_AIR ? 0 : (st.toString().contains("lava") ? 2 : 1)});
            return null;
        }
        public void e(jd p) {}
        public duy.a q() { return null; }
        public eyw<epd> p() { return null; }
        public eyw<dfy> o() { return null; }
        public dvj b(int i) { return null; }
        public dvz j() { return null; }
        public void d(jd p) {}
        public ub a(jd p, jo.a f) { return null; }
        public epe b_(jd p) { return null; }
        public dqh c_(jd p) { return null; }
        public void a(dqh k) {}
        public void a(bsr s) {}
    }

    public static void main(String[] args) throws Exception {
        initUnsafe();
        ab.a();
        akt.a();
        STONE = block("stone");
        CAVE_AIR = block("cave_air");
        long seed = Long.parseLong(args[0]);
        int tx = Integer.parseInt(args[1]), tz = Integer.parseInt(args[2]);

        jq<dfy> tag = (jq<dfy>) Proxy.newProxyInstance(CarverProbe.class.getClassLoader(),
                new Class<?>[]{jq.class}, (p, m, a) -> {
                    if (m.getName().equals("a") && m.getParameterCount() == 1) return true;
                    Class<?> rt = m.getReturnType();
                    if (rt == boolean.class) return false;
                    if (rt == int.class) return 0;
                    return null;
                });

        boolean canyon = System.getProperty("canyon") != null;
        boolean extra = System.getProperty("extra") != null;
        String yc = System.getProperty("yconst");
        ehx yp = yc == null
                ? (canyon ? ehv.a(dzs.a(10), dzs.a(67), 1)
                          : (extra ? ehv.a(dzs.b(8), dzs.a(47), 1)
                                   : ehv.a(dzs.b(8), dzs.a(180), 1)))
                : ehw.a(dzs.a(Integer.parseInt(yc)));
        eax cfg;
        if (canyon) {
            // canyon.json: yScale 3.0, vertical_rotation uniform(-0.125, 0.125),
            // shape = distance uniform(0.75, 1.0), thickness trapezoid(0, 6, 2),
            // width_smoothness 3, horizontal_radius uniform(0.75, 1.0),
            // vertical_radius default 1.0 / centre 0.0
            cfg = new eav(0.01f, yp, bps.a(3.0f), dzs.b(8), eay.a, tag,
                    bqb.b(-0.125f, 0.125f),
                    new eav.a(bqb.b(0.75f, 1.0f),
                              bqa.a(0f, 6f, 2f),
                              3,
                              bqb.b(0.75f, 1.0f),
                              1.0f, 0.0f));
        } else {
            cfg = new eba(extra ? 0.07f : 0.15f, yp, fp("yScale", 0.1f, 0.9f), dzs.b(8), eay.a, tag,
                    fp("horiz", 0.7f, 1.4f),
                    fp("vert", 0.8f, 1.3f),
                    fp("floor", -1.0f, -0.4f));
        }


        if (System.getProperty("dumpcfg") != null) {
            eaz c0 = (eaz) U.allocateInstance(eaz.class);
            setInt(c0, dzv.class, "a", -64);
            setInt(c0, dzv.class, "b", 384);
            System.out.println("above_bottom 8 resolves to " + dzs.b(8).a(c0));
            System.out.println("absolute 180 resolves to " + dzs.a(180).a(c0));
            System.out.println("canyon y min (absolute 10) = " + dzs.a(10).a(c0));
            ayw r0 = new dzx(new dyz(0L));
            System.out.println("y sample 1 = " + cfg.e.a(r0, c0));
            System.out.println("y sample 2 = " + cfg.e.a(r0, c0));
            System.out.println("y sample 3 = " + cfg.e.a(r0, c0));
            return;
        }

        eaz ctx = (eaz) U.allocateInstance(eaz.class);
        setInt(ctx, dzv.class, "a", -64);
        setInt(ctx, dzv.class, "b", 384);
        System.out.println("ctx minY=" + ctx.a() + " depth=" + ctx.b());

        FakeChunk chunk = (FakeChunk) U.allocateInstance(FakeChunk.class);
        chunk.pos = new dcd(tx, tz);

        dyk aquifer = (dyk) Proxy.newProxyInstance(CarverProbe.class.getClassLoader(),
                new Class<?>[]{dyk.class}, (p, m, a) -> {
                    if (m.getName().equals("a") && m.getParameterCount() == 2) return CAVE_AIR;
                    Class<?> rt = m.getReturnType();
                    if (rt == boolean.class) return false;
                    return null;
                });

        dux mask = new LogMask();
        ebe carver;
        if (canyon) {
            carver = new eaw(eav.a) {
                @Override
                public boolean a(eaz c, eav g, duy ch, java.util.function.Function b, ayw r,
                                 dyk q, dcd p, dux m) {
                    boolean res = super.a(c, g, ch, b, r, q, p, m);
                    int bits = 0;
                    for (long w : m.a()) bits += Long.bitCount(w);
                    System.out.println("carve " + p + " -> " + res + " maskBits=" + bits);
                    return res;
                }
            };
        } else {
        carver = new ebb(eba.a) {
            @Override
            public boolean a(eaz c, eba g, duy ch, java.util.function.Function b, ayw r,
                             dyk q, dcd p, dux m) {
                boolean res = super.a(c, g, ch, b, r, q, p, m);
                int bits = 0;
                for (long w : m.a()) bits += Long.bitCount(w);
                System.out.println("carve " + p + " -> " + res + " maskBits=" + bits);
                return res;
            }
        };
        }
        // the whole -8..8 neighbourhood, as applyCarvers walks it, one seed per
        // origin and one shared mask for the target chunk
        int starts = 0;
        for (int dx = -8; dx <= 8; dx++) {
            for (int dz = -8; dz <= 8; dz++) {
                int cxo = tx + dx, czo = tz + dz;
                String only = System.getProperty("only");
                if (only != null && !only.equals(cxo + "," + czo)) continue;
                dzx real = new dzx(new dyz(Long.getLong("iseed", 0L)));
                real.c(seed + Integer.getInteger("k", 0), cxo, czo);
                final int[] n = {0};
                final int[] firstOrigin = {-1};
                ayw random = (ayw) Proxy.newProxyInstance(CarverProbe.class.getClassLoader(),
                        new Class<?>[]{ayw.class}, (p, m, a) -> {
                            Object r;
                            try { r = m.invoke(real, a); }
                            catch (InvocationTargetException e) { throw e.getCause(); }
                            if (LEG && (only != null || firstOrigin[0] == cxo * 1000 + czo) && n[0] < 60) {
                                StringBuilder sb = new StringBuilder("DRAW o=" + cxo + "," + czo + " " + m.getName() + "(");
                                if (a != null) for (Object o : a) sb.append(o).append(",");
                                System.out.println(sb + ") -> " + r);
                                n[0]++;
                            }
                            return r;
                        });
                boolean st = carver.a(cfg, random);
                if (LEG) System.out.println("START " + cxo + " " + czo + " " + st);
                if (st) {
                    starts++;
                    if (LEG && firstOrigin[0] == -1) firstOrigin[0] = cxo * 1000 + czo;
                    carver.a(ctx, cfg, chunk, null, random, aquifer, new dcd(cxo, czo), mask);
                }
            }
        }
        System.out.println("starts=" + starts + " target=" + tx + "," + tz);
        if (LEG) {
            for (int i = 0; i < Math.min(40, cells.size()); i++) {
                int[] c = cells.get(i);
                System.out.println("ORDER " + c[0] + " " + c[1] + " " + c[2]);
            }
        }
        Set<String> uniq = new TreeSet<>();
        for (int[] c : cells) uniq.add(c[0] + " " + c[1] + " " + c[2] + " " + c[3]);
        long[] bits = mask.a();
        int set = 0;
        for (long w : bits) set += Long.bitCount(w);
        System.out.println("cells=" + uniq.size() + " writes=" + cells.size() + " maskBits=" + set
                + " maskGets=" + ((LogMask) mask).gets + " getPos=" + chunk.getPosCalls);
        for (String s : uniq) System.out.println("CELL " + s);
    }
}
