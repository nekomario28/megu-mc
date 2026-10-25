// Reads per-block-state facts and the overworld/nether biome search trees out of the running
// vanilla 1.21.1 server jar. Nothing here is copied from another implementation: every number
// is what the game itself answers once its registries are bootstrapped.
//
// The 1.21.1 server jar is obfuscated. The class and member names below are written in Fabric's
// Yarn names (CC0) and translated to the jar's own names at run time through Fabric's published
// intermediary + Yarn mapping files, which the caller supplies (see gen_vanilla_data.sh). The
// jar is never modified and nothing from Mojang is downloaded by this tool.
//
//   java Extract.java <out-dir> <intermediary.tiny> <yarn.tiny> <classpath-entry>...
//   (a leading `--candidates` prints extra candidate predicates for diagnosis)
import java.io.*;
import java.lang.reflect.*;
import java.net.*;
import java.nio.charset.StandardCharsets;
import java.nio.file.*;
import java.util.*;
import java.util.function.Function;

public class Extract {
    // ---- mapping lookup -------------------------------------------------------------------
    static final Map<String, String> yarnToObf = new HashMap<>();      // yarn class -> obf class
    static final Map<String, String> obfToInter = new HashMap<>();     // obf class -> intermediary class
    static final Map<String, String> interToYarn = new HashMap<>();    // intermediary class -> yarn class
    static final Map<String, List<String[]>> members = new HashMap<>(); // obf class -> {kind,obfName,obfDesc,interName}
    static final Map<String, String> yarnMember = new HashMap<>();     // interClass.interName -> yarn name
    static ClassLoader loader;

    static void readTiny(Path p, boolean inter) throws IOException {
        String cur = null;       // key class (obf for inter file, intermediary for yarn file)
        String curInter = null;
        for (String l : Files.readAllLines(p, StandardCharsets.UTF_8)) {
            if (l.startsWith("c\t")) {
                String[] f = l.split("\t");
                if (inter) {
                    cur = f[1];
                    curInter = f[2];
                    obfToInter.put(f[1], f[2]);
                } else {
                    cur = f[1];
                    interToYarn.put(f[1], f[2]);
                }
            } else if (l.startsWith("\t") && cur != null) {
                String[] f = l.substring(1).split("\t");
                if (!f[0].equals("m") && !f[0].equals("f")) continue;
                if (inter) {
                    members.computeIfAbsent(cur, k -> new ArrayList<>()).add(new String[] {f[0], f[2], f[1], f[3]});
                } else {
                    yarnMember.put(cur + "." + f[2], f[3]);
                }
            }
        }
    }

    static String yarnNameOf(String obfClass, String interName) {
        String ic = obfToInter.get(obfClass);
        String y = ic == null ? null : yarnMember.get(ic + "." + interName);
        return y != null ? y : interName;
    }

    static Class<?> cls(String yarn) throws Exception {
        String obf = yarnToObf.get(yarn);
        if (obf == null) throw new IllegalStateException("no mapping for class " + yarn);
        return Class.forName(obf.replace('/', '.'), false, loader);
    }

    static Class<?> descClass(String d) throws Exception {
        switch (d) {
            case "Z": return boolean.class;
            case "I": return int.class;
            case "J": return long.class;
            case "F": return float.class;
            case "D": return double.class;
            case "B": return byte.class;
            case "S": return short.class;
            case "C": return char.class;
            case "V": return void.class;
        }
        if (d.startsWith("[")) return Array.newInstance(descClass(d.substring(1)), 0).getClass();
        return Class.forName(d.substring(1, d.length() - 1).replace('/', '.'), false, loader);
    }

    static List<Class<?>> parseParams(String desc) throws Exception {
        List<Class<?>> out = new ArrayList<>();
        int i = 1;
        while (desc.charAt(i) != ')') {
            int s = i;
            while (desc.charAt(i) == '[') i++;
            if (desc.charAt(i) == 'L') i = desc.indexOf(';', i);
            i++;
            out.add(descClass(desc.substring(s, i)));
        }
        return out;
    }

    static Method method(Class<?> c, String yarnName, Class<?>... params) throws Exception {
        for (Class<?> k = c; k != null; k = k.getSuperclass()) {
            Method m = declared(k, yarnName, params);
            if (m != null) return m;
            for (Class<?> i : k.getInterfaces()) {
                m = declared(i, yarnName, params);
                if (m != null) return m;
            }
        }
        throw new NoSuchMethodException(c.getName() + "#" + yarnName);
    }

    static Method declared(Class<?> k, String yarnName, Class<?>[] params) throws Exception {
        List<String[]> ms = members.get(k.getName().replace('.', '/'));
        if (ms == null) return null;
        for (String[] m : ms) {
            if (!m[0].equals("m") || !yarnNameOf(k.getName().replace('.', '/'), m[3]).equals(yarnName)) continue;
            if (!parseParams(m[2]).equals(Arrays.asList(params))) continue;
            Method r = k.getDeclaredMethod(m[1], params);
            r.setAccessible(true);
            return r;
        }
        return null;
    }

    static Field field(Class<?> k, String yarnName) throws Exception {
        List<String[]> ms = members.get(k.getName().replace('.', '/'));
        if (ms != null) {
            for (String[] m : ms) {
                if (m[0].equals("f") && yarnNameOf(k.getName().replace('.', '/'), m[3]).equals(yarnName)) {
                    Field f = k.getDeclaredField(m[1]);
                    f.setAccessible(true);
                    return f;
                }
            }
        }
        throw new NoSuchFieldException(k.getName() + "#" + yarnName);
    }

    // ---- extraction -----------------------------------------------------------------------
    static Object enumConst(Class<?> e, String name) {
        for (Object o : e.getEnumConstants()) if (((Enum<?>) o).name().equals(name)) return o;
        throw new IllegalStateException(name);
    }

    public static void main(String[] a) throws Exception {
        int ai = 0;
        boolean candidates = false;
        if (a[ai].equals("--candidates")) { candidates = true; ai++; }
        Path out = Paths.get(a[ai++]);
        readTiny(Paths.get(a[ai++]), true);
        readTiny(Paths.get(a[ai++]), false);
        // yarn class name -> obf class name
        for (Map.Entry<String, String> e : obfToInter.entrySet()) {
            String y = interToYarn.get(e.getValue());
            if (y != null) yarnToObf.put(y, e.getKey());
        }
        List<URL> urls = new ArrayList<>();
        for (; ai < a.length; ai++) urls.add(Paths.get(a[ai]).toUri().toURL());
        loader = new URLClassLoader(urls.toArray(new URL[0]), ClassLoader.getPlatformClassLoader());
        Thread.currentThread().setContextClassLoader(loader);

        method(cls("net/minecraft/SharedConstants"), "createGameVersion").invoke(null);
        method(cls("net/minecraft/Bootstrap"), "initialize").invoke(null);

        Class<?> blockC = cls("net/minecraft/block/Block");
        Class<?> stateC = cls("net/minecraft/block/AbstractBlock$AbstractBlockState");
        Class<?> viewC = cls("net/minecraft/world/BlockView");
        Class<?> posC = cls("net/minecraft/util/math/BlockPos");
        Class<?> dirC = cls("net/minecraft/util/math/Direction");
        Class<?> sstC = cls("net/minecraft/block/SideShapeType");
        Object view = field(cls("net/minecraft/world/EmptyBlockView"), "INSTANCE").get(null);
        Object pos = field(posC, "ORIGIN").get(null);
        Object[] dirs = dirC.getEnumConstants();           // DOWN, UP, NORTH, SOUTH, WEST, EAST
        Object[] sst = {enumConst(sstC, "FULL"), enumConst(sstC, "CENTER"), enumConst(sstC, "RIGID")};

        Method mLum = method(stateC, "getLuminance");
        Method mOpa = method(stateC, "getOpacity", viewC, posC);
        Method mAir = method(stateC, "isAir");
        Method mRep = method(stateC, "isReplaceable");
        Method mLiq = method(stateC, "isLiquid");
        Method mSol = method(stateC, "isSolid");
        Method mSide = method(stateC, "isSideSolid", viewC, posC, dirC, sstC);
        Method mFull = method(stateC, "isSideSolidFullSquare", viewC, posC, dirC);
        Method mBlock = method(stateC, "getBlock");
        Method mOpaque = method(stateC, "isOpaque");
        Method mSolidBlock = method(stateC, "isSolidBlock", viewC, posC);
        Method mFullCube = method(stateC, "isFullCube", viewC, posC);
        Method mFluid = method(stateC, "getFluidState");
        Method mFluidEmpty = method(cls("net/minecraft/fluid/FluidState"), "isEmpty");
        Method mRawId = method(blockC, "getRawIdFromState", cls("net/minecraft/block/BlockState"));
        Class<?> regC = cls("net/minecraft/registry/Registry");
        Object blockReg = field(cls("net/minecraft/registry/Registries"), "BLOCK").get(null);
        Method mGetId = method(regC, "getId", Object.class);
        Object stateIds = field(blockC, "STATE_IDS").get(null);

        StringBuilder names = new StringBuilder();
        List<int[]> rows = new ArrayList<>();
        List<String> blockOf = new ArrayList<>();
        int id = 0;
        for (Object st : (Iterable<?>) stateIds) {
            int raw = (Integer) mRawId.invoke(null, st);
            if (raw != id) throw new IllegalStateException("state id order: " + raw + " != " + id);
            String name = String.valueOf(mGetId.invoke(blockReg, mBlock.invoke(st)));
            blockOf.add(name);
            int lum = (Integer) mLum.invoke(st);
            int opa = (Integer) mOpa.invoke(st, view, pos);
            int flags = 0;
            if ((Boolean) mAir.invoke(st)) flags |= 1;
            if ((Boolean) mRep.invoke(st)) flags |= 2;
            if ((Boolean) mLiq.invoke(st)) flags |= 4;
            if ((Boolean) mSol.invoke(st)) flags |= 8;
            if (!(Boolean) mFluidEmpty.invoke(mFluid.invoke(st))) flags |= 16;
            int sides = 0;
            for (int d = 0; d < 6; d++) if ((Boolean) mSide.invoke(st, view, pos, dirs[d], sst[0])) sides |= 1 << d;
            int[] r = new int[candidates ? 10 : 4];
            r[0] = lum; r[1] = opa; r[2] = flags; r[3] = sides;
            if (candidates) {
                int sc = 0, sr = 0, sf = 0;
                for (int d = 0; d < 6; d++) {
                    if ((Boolean) mSide.invoke(st, view, pos, dirs[d], sst[1])) sc |= 1 << d;
                    if ((Boolean) mSide.invoke(st, view, pos, dirs[d], sst[2])) sr |= 1 << d;
                    if ((Boolean) mFull.invoke(st, view, pos, dirs[d])) sf |= 1 << d;
                }
                r[4] = sc; r[5] = sr; r[6] = sf;
                r[7] = (Boolean) mOpaque.invoke(st) ? 1 : 0;
                r[8] = (Boolean) mSolidBlock.invoke(st, view, pos) ? 1 : 0;
                r[9] = (Boolean) mFullCube.invoke(st, view, pos) ? 1 : 0;
            }
            rows.add(r);
            id++;
        }
        // one line per state id: id lum opacity flags sides [candidates...]
        try (PrintWriter w = new PrintWriter(Files.newBufferedWriter(out.resolve("state_facts.tsv")))) {
            for (int i = 0; i < rows.size(); i++) {
                StringBuilder sb = new StringBuilder();
                sb.append(i).append('\t').append(blockOf.get(i));
                for (int v : rows.get(i)) sb.append('\t').append(v);
                w.println(sb);
            }
        }

        // ---- biome search trees ---------------------------------------------------------
        Class<?> presetC = cls("net/minecraft/world/biome/source/MultiNoiseBiomeSourceParameterList$Preset");
        Class<?> bsfC = cls("net/minecraft/world/biome/source/MultiNoiseBiomeSourceParameterList$Preset$BiomeSourceFunction");
        Class<?> entriesC = cls("net/minecraft/world/biome/source/util/MultiNoiseUtil$Entries");
        Class<?> treeC = cls("net/minecraft/world/biome/source/util/MultiNoiseUtil$SearchTree");
        Class<?> nodeC = cls("net/minecraft/world/biome/source/util/MultiNoiseUtil$SearchTree$TreeNode");
        Class<?> branchC = cls("net/minecraft/world/biome/source/util/MultiNoiseUtil$SearchTree$TreeBranchNode");
        Class<?> leafC = cls("net/minecraft/world/biome/source/util/MultiNoiseUtil$SearchTree$TreeLeafNode");
        Class<?> rangeC = cls("net/minecraft/world/biome/source/util/MultiNoiseUtil$ParameterRange");
        Class<?> keyC = cls("net/minecraft/registry/RegistryKey");
        Class<?> idC = cls("net/minecraft/util/Identifier");
        Class<?> pointC = cls("net/minecraft/world/biome/source/util/MultiNoiseUtil$NoiseValuePoint");
        Method mKeyVal = method(keyC, "getValue");
        Method mBsf = method(presetC, "biomeSourceFunction");
        Method mApply = null;
        for (Method m : bsfC.getMethods()) if (m.getParameterCount() == 1 && m.getParameterTypes()[0] == Function.class) mApply = m;
        mApply.setAccessible(true);
        Method mEntryGet = method(entriesC, "getValue", pointC);
        Field fTree = field(entriesC, "tree");
        Field fFirst = field(treeC, "firstNode");
        Field fParams = field(nodeC, "parameters");
        Field fSub = field(branchC, "subTree");
        Field fValue = field(leafC, "value");
        Field fPrev = field(treeC, "previousResultNode");
        Field fMin = field(rangeC, "min");
        Field fMax = field(rangeC, "max");
        Function<Object, Object> toName = k -> {
            try { return String.valueOf(mKeyVal.invoke(k)); } catch (Exception e) { throw new RuntimeException(e); }
        };
        StringBuilder js = new StringBuilder("{");
        Object[] entriesOf = new Object[2];
        String[] presets = {"OVERWORLD", "NETHER"};
        for (int p = 0; p < 2; p++) {
            Object preset = field(presetC, presets[p]).get(null);
            Object entries = mApply.invoke(mBsf.invoke(preset), toName);
            entriesOf[p] = entries;
            Object root = fFirst.get(fTree.get(entries));
            if (p > 0) js.append(',');
            js.append('"').append(presets[p].toLowerCase()).append("\":");
            node(js, root, branchC, leafC, fParams, fSub, fValue, fMin, fMax);
        }
        js.append("}\n");
        Files.writeString(out.resolve("biome_tree.json"), js.toString());

        // Reference answers: points looked up through the game's own tree. The game seeds each
        // search with the previous result (a thread-local), which decides ties between leaves at
        // equal distance, so the seed is cleared before every lookup. A point is also marked
        // `ambiguous` when a brute-force scan finds two different biomes at the same minimum
        // distance: only those points depend on tree order rather than on the data.
        Constructor<?> pc = null;
        for (Constructor<?> c : pointC.getDeclaredConstructors()) if (c.getParameterCount() == 6) pc = c;
        if (pc == null) throw new IllegalStateException("NoiseValuePoint constructor");
        pc.setAccessible(true);
        @SuppressWarnings("unchecked")
        ThreadLocal<Object> previous = (ThreadLocal<Object>) fPrev.get(fTree.get(entriesOf[0]));
        List<long[]> leafBox = new ArrayList<>();
        List<String> leafBiome = new ArrayList<>();
        collect(fFirst.get(fTree.get(entriesOf[0])), branchC, leafC, fParams, fSub, fValue, fMin, fMax, leafBox, leafBiome);
        Random rnd = new Random(1211);
        try (PrintWriter w = new PrintWriter(Files.newBufferedWriter(out.resolve("biome_probe.tsv")))) {
            int n = 6000;
            for (int i = 0; i < n; i++) {
                long[] v = new long[6];
                boolean wide = (i & 1) == 0;
                for (int k = 0; k < 6; k++) {
                    int lo = -12000, hi = 12000;
                    if (k == 4) { lo = -2000; hi = 12000; }            // depth
                    long x = lo + (long) (rnd.nextDouble() * (hi - lo));
                    if (!wide) x = Math.round(x / 500.0) * 500;           // grid points land on range edges
                    v[k] = x;
                }
                previous.remove();
                Object pt = pc.newInstance(v[0], v[1], v[2], v[3], v[4], v[5]);
                String b = String.valueOf(mEntryGet.invoke(entriesOf[0], pt));
                long best = Long.MAX_VALUE;
                Set<String> atBest = new TreeSet<>();
                for (int l = 0; l < leafBox.size(); l++) {
                    long d = 0;
                    long[] box = leafBox.get(l);
                    for (int k = 0; k < 7; k++) {
                        long t = k < 6 ? v[k] : 0;
                        long dd = t < box[2 * k] ? box[2 * k] - t : (t > box[2 * k + 1] ? t - box[2 * k + 1] : 0);
                        d += dd * dd;
                    }
                    if (d < best) { best = d; atBest.clear(); }
                    if (d == best) atBest.add(leafBiome.get(l));
                }
                if (!atBest.contains(b)) throw new IllegalStateException("game answer is not a nearest leaf: " + b);
                w.println(v[0] + "\t" + v[1] + "\t" + v[2] + "\t" + v[3] + "\t" + v[4] + "\t" + v[5] + "\t" + b + "\t" + (atBest.size() > 1 ? 1 : 0));
            }
        }
    }

    static void collect(Object n, Class<?> branchC, Class<?> leafC, Field fParams, Field fSub, Field fValue,
                        Field fMin, Field fMax, List<long[]> boxes, List<String> names) throws Exception {
        if (branchC.isInstance(n)) {
            for (Object c : (Object[]) fSub.get(n)) collect(c, branchC, leafC, fParams, fSub, fValue, fMin, fMax, boxes, names);
        } else {
            Object[] ps = (Object[]) fParams.get(n);
            long[] box = new long[14];
            for (int i = 0; i < 7; i++) { box[2 * i] = (Long) fMin.get(ps[i]); box[2 * i + 1] = (Long) fMax.get(ps[i]); }
            boxes.add(box);
            names.add(String.valueOf(fValue.get(n)));
        }
    }

    static void node(StringBuilder js, Object n, Class<?> branchC, Class<?> leafC, Field fParams, Field fSub,
                     Field fValue, Field fMin, Field fMax) throws Exception {
        Object[] ps = (Object[]) fParams.get(n);
        js.append("{\"parameters\":[");
        for (int i = 0; i < ps.length; i++) {
            if (i > 0) js.append(',');
            js.append("{\"min\":").append(fMin.get(ps[i])).append(",\"max\":").append(fMax.get(ps[i])).append('}');
        }
        js.append("],");
        if (branchC.isInstance(n)) {
            js.append("\"_type\":\"branch\",\"subTree\":[");
            Object[] sub = (Object[]) fSub.get(n);
            for (int i = 0; i < sub.length; i++) {
                if (i > 0) js.append(',');
                node(js, sub[i], branchC, leafC, fParams, fSub, fValue, fMin, fMax);
            }
            js.append("]}");
        } else {
            js.append("\"_type\":\"leaf\",\"biome\":\"").append(fValue.get(n)).append("\"}");
        }
    }
}
