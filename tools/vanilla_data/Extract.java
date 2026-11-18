// Reads per-block-state facts and the overworld/nether biome search trees out of the running
// vanilla 1.21.1 server jar. Nothing here is copied from another implementation: every number
// is what the game itself answers once its registries are bootstrapped.
//
// The 1.21.1 server jar is obfuscated. The class and member names below are written in Fabric's
// Yarn names (CC0) and translated to the jar's own names at run time through Fabric's published
// intermediary + Yarn mapping files, which the caller supplies (see gen_vanilla_data.sh). The
// jar is never modified and nothing from Mojang is downloaded by this tool.
//
//   java Extract.java [--candidates] [--queries FILE] <out-dir> <intermediary.tiny> <yarn.tiny> <classpath-entry>...
//   `--candidates` prints extra candidate predicates for diagnosis.
//   `--queries FILE` answers the queries in FILE (same line format as vanilla_vectors.tsv, without
//   the answer) instead of the generated ones, to check some other table of points against the game.
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
        Path queriesFile = null;
        while (a[ai].startsWith("--")) {
            if (a[ai].equals("--candidates")) { candidates = true; ai++; }
            else if (a[ai].equals("--queries")) { queriesFile = Paths.get(a[ai + 1]); ai += 2; }
            else throw new IllegalArgumentException(a[ai]);
        }
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
        // The collision shape is a per-state box list, not a scalar: `getCollisionShape` answers
        // an empty VoxelShape for air, a plant or a torch and several boxes for a fence, a stair
        // or a lit candle. `getBoundingBoxes` is the same decomposition `VoxelShape.toAabbs`
        // gives, but reads the already-cached box list instead of walking the voxel planes.
        Class<?> shapeC = cls("net/minecraft/util/shape/VoxelShape");
        Class<?> boxC = cls("net/minecraft/util/math/Box");
        Method mCollision = method(stateC, "getCollisionShape", viewC, posC);
        Method mBoxes = method(shapeC, "getBoundingBoxes");
        Field[] boxField = {
            field(boxC, "minX"), field(boxC, "minY"), field(boxC, "minZ"),
            field(boxC, "maxX"), field(boxC, "maxY"), field(boxC, "maxZ"),
        };
        Method mRawId = method(blockC, "getRawIdFromState", cls("net/minecraft/block/BlockState"));
        Class<?> regC = cls("net/minecraft/registry/Registry");
        Object blockReg = field(cls("net/minecraft/registry/Registries"), "BLOCK").get(null);
        Method mGetId = method(regC, "getId", Object.class);
        Object stateIds = field(blockC, "STATE_IDS").get(null);

        StringBuilder names = new StringBuilder();
        List<int[]> rows = new ArrayList<>();
        List<String> blockOf = new ArrayList<>();
        List<String> shapesOf = new ArrayList<>();
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
            // One token per box, `minX,minY,minZ,maxX,maxY,maxZ`, comma-joined; an empty
            // collision shape is an empty field. `Double.toString` round-trips through
            // `float()` in the writer, so the AABBs survive without a rounding decision here.
            StringBuilder shape = new StringBuilder();
            for (Object box : (List<?>) mBoxes.invoke(mCollision.invoke(st, view, pos))) {
                for (int k = 0; k < 6; k++) {
                    if (k > 0) shape.append(',');
                    shape.append(Double.toString((Double) boxField[k].get(box)));
                }
                shape.append('\t');
            }
            if (shape.length() > 0) shape.setLength(shape.length() - 1);
            shapesOf.add(shape.toString());
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
        // one line per state id: id then one `minX,minY,minZ,maxX,maxY,maxZ` field per box
        try (PrintWriter w = new PrintWriter(Files.newBufferedWriter(out.resolve("state_shapes.tsv")))) {
            for (int i = 0; i < shapesOf.size(); i++) {
                w.println(shapesOf.get(i).isEmpty() ? Integer.toString(i) : i + "\t" + shapesOf.get(i));
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

        Vectors.write(out.resolve(queriesFile == null ? "vanilla_vectors.tsv" : "queries_answered.tsv"), queriesFile,
                entriesOf[0], mEntryGet, previous, pc, leafBox, leafBiome);
        // Tie-heavy reference answers (biome_probe_ties.tsv). The first probe set is mostly
        // unambiguous points; this one is built from the leaves' own range edges so that many
        // points sit at the same distance from leaves of different biomes. There the answer is
        // decided by the order in which the game scans a branch's children, so this is the set
        // that pins the scan order. Same cleared-seed lookup and ambiguity flag as above.
        Random rt = new Random(20260);
        int tieKept = 0, tieTotal = 0;
        try (PrintWriter w = new PrintWriter(Files.newBufferedWriter(out.resolve("biome_probe_ties.tsv")))) {
            while (tieKept < 2500) {
                long[] box = leafBox.get(rt.nextInt(leafBox.size()));
                long[] v = new long[6];
                for (int k = 0; k < 6; k++) {
                    long lo = box[2 * k], hi = box[2 * k + 1];
                    switch (rt.nextInt(6)) {
                        case 0: v[k] = lo; break;
                        case 1: v[k] = hi; break;
                        case 2: v[k] = lo - 1 - rt.nextInt(300); break;
                        case 3: v[k] = hi + 1 + rt.nextInt(300); break;
                        default: v[k] = lo + (long) (rt.nextDouble() * (hi - lo + 1)); break;
                    }
                }
                previous.remove();
                Object pt = pc.newInstance(v[0], v[1], v[2], v[3], v[4], v[5]);
                String b = String.valueOf(mEntryGet.invoke(entriesOf[0], pt));
                long best = Long.MAX_VALUE;
                Set<String> atBest = new TreeSet<>();
                for (int l = 0; l < leafBox.size(); l++) {
                    long d = 0;
                    long[] bx = leafBox.get(l);
                    for (int k = 0; k < 7; k++) {
                        long t = k < 6 ? v[k] : 0;
                        long dd = t < bx[2 * k] ? bx[2 * k] - t : (t > bx[2 * k + 1] ? t - bx[2 * k + 1] : 0);
                        d += dd * dd;
                    }
                    if (d < best) { best = d; atBest.clear(); }
                    if (d == best) atBest.add(leafBiome.get(l));
                }
                if (!atBest.contains(b)) throw new IllegalStateException("game answer is not a nearest leaf: " + b);
                boolean amb = atBest.size() > 1;
                tieTotal++;
                if (!amb && rt.nextInt(10) != 0) continue;   // keep every tie, a tenth of the rest
                w.println(v[0] + "\t" + v[1] + "\t" + v[2] + "\t" + v[3] + "\t" + v[4] + "\t" + v[5] + "\t" + b + "\t" + (amb ? 1 : 0));
                tieKept++;
            }
        }

        // ---- bounded integer draws --------------------------------------------------------
        // random_next_int.tsv: for each (generator, seed, bound), the first `draws` calls of
        // nextInt(bound) on one fresh generator, folded into an FNV-1a hash of the results,
        // then the generator's next nextLong() (which shows how many raw words the draws used).
        //   kind: xoroshiro = Xoroshiro128PlusPlusRandom(seed)
        //         worldgen  = ChunkRandom(Xoroshiro128PlusPlusRandom(seed))
        //         checked   = CheckedRandom(seed)  (the classic java.util.Random algorithm)
        Class<?> randC = cls("net/minecraft/util/math/random/Random");
        Class<?> xoC = cls("net/minecraft/util/math/random/Xoroshiro128PlusPlusRandom");
        Class<?> chunkRC = cls("net/minecraft/util/math/random/ChunkRandom");
        Class<?> checkedC = cls("net/minecraft/util/math/random/CheckedRandom");
        Constructor<?> xoCtor = xoC.getDeclaredConstructor(long.class);
        Constructor<?> chunkCtor = chunkRC.getDeclaredConstructor(randC);
        Constructor<?> checkedCtor = checkedC.getDeclaredConstructor(long.class);
        xoCtor.setAccessible(true); chunkCtor.setAccessible(true); checkedCtor.setAccessible(true);
        Method mNextInt = method(randC, "nextInt", int.class);
        Method mNextLong = method(randC, "nextLong");
        int[] bounds = {1, 2, 3, 5, 6, 7, 10, 12, 16, 24, 100, 1000, 12345, 65537, 1 << 20, (1 << 20) + 1,
            100000007, 1 << 28, (1 << 28) + 1, 0x15555555, 0x2AAAAAAA, 0x3FFFFFFF, (1 << 30) + 1, 0x55555555,
            0x5FFFFFFF, 0x60000000, 0x6AAAAAAA, 0x7000_0001, 0x7FFFFFF0, 0x7FFFFFFE, 0x7FFFFFFF};
        String[] kinds = {"xoroshiro", "worldgen", "checked"};
        try (PrintWriter w = new PrintWriter(Files.newBufferedWriter(out.resolve("random_next_int.tsv")))) {
            for (String kind : kinds) {
                int nseeds = kind.equals("xoroshiro") ? 24 : 6;
                for (int si = 0; si < nseeds; si++) {
                    long seed = si == 0 ? 0 : (si < 8 ? si : new Random(900 + si).nextLong());
                    for (int bound : bounds) {
                        Object r = kind.equals("xoroshiro") ? xoCtor.newInstance(seed)
                            : kind.equals("worldgen") ? chunkCtor.newInstance(xoCtor.newInstance(seed))
                            : checkedCtor.newInstance(seed);
                        int draws = 256;
                        long h = 0xcbf29ce484222325L;
                        for (int i = 0; i < draws; i++) {
                            int x = (Integer) mNextInt.invoke(r, bound);
                            for (int by = 0; by < 4; by++) { h ^= (x >>> (8 * by)) & 0xff; h *= 0x100000001b3L; }
                        }
                        long after = (Long) mNextLong.invoke(r);
                        w.println(kind + "\t" + seed + "\t" + bound + "\t" + draws + "\t" + Long.toUnsignedString(h) + "\t" + after);
                    }
                }
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

// Reference answers from the game for tests: random-number streams, the multi-noise biome
// lookup, the overworld climate density functions and the aquifer's private helpers. Every line
// is `kind <TAB> arguments... <TAB> | <TAB> answers...`; the inputs are drawn here from a fixed
// seed (or read from `--queries`), the answers come from the game's own classes.
class Vectors {
    static final String[] KIND = {"air", "water", "lava"};
    static Class<?> stateC, blockC, flC, implC, aqC, posC, dfC, cnsC, settingsC, ncC;
    static Object lookup, settings, noiseParams, blockReg, beardifier, noBlend;
    static Method mGetId, mBlockOf, mDefState, mSample, mValue, mGetWrapper, mGetOrThrow;
    static Object[] kindState = new Object[3];
    static Constructor<?> posCtor, flCtor, cnsCtor, mutCtor;
    static Field fFlY, fFlState, fAquifer;
    static final Map<Long, Object> noiseConfigs = new HashMap<>();
    static final Map<String, Object[]> aquifers = new HashMap<>();

    static void init() throws Exception {
        stateC = Extract.cls("net/minecraft/block/BlockState");
        blockC = Extract.cls("net/minecraft/block/Block");
        aqC = Extract.cls("net/minecraft/world/gen/chunk/AquiferSampler");
        flC = Extract.cls("net/minecraft/world/gen/chunk/AquiferSampler$FluidLevel");
        implC = Extract.cls("net/minecraft/world/gen/chunk/AquiferSampler$Impl");
        dfC = Extract.cls("net/minecraft/world/gen/densityfunction/DensityFunction");
        posC = Extract.cls("net/minecraft/world/gen/densityfunction/DensityFunction$NoisePos");
        cnsC = Extract.cls("net/minecraft/world/gen/chunk/ChunkNoiseSampler");
        settingsC = Extract.cls("net/minecraft/world/gen/chunk/ChunkGeneratorSettings");
        ncC = Extract.cls("net/minecraft/world/gen/noise/NoiseConfig");
        Class<?> keyC = Extract.cls("net/minecraft/registry/RegistryKey");
        Class<?> regKeys = Extract.cls("net/minecraft/registry/RegistryKeys");
        Class<?> wrapC = Extract.cls("net/minecraft/registry/RegistryWrapper$WrapperLookup");
        Class<?> entryLookupC = Extract.cls("net/minecraft/registry/RegistryEntryLookup");
        Class<?> entryC = Extract.cls("net/minecraft/registry/entry/RegistryEntry");

        lookup = Extract.method(Extract.cls("net/minecraft/registry/BuiltinRegistries"), "createWrapperLookup").invoke(null);
        mGetWrapper = Extract.method(wrapC, "getWrapperOrThrow", keyC);
        mGetOrThrow = Extract.method(entryLookupC, "getOrThrow", keyC);
        mValue = Extract.method(entryC, "value");
        Object settingsLookup = mGetWrapper.invoke(lookup, Extract.field(regKeys, "CHUNK_GENERATOR_SETTINGS").get(null));
        noiseParams = mGetWrapper.invoke(lookup, Extract.field(regKeys, "NOISE_PARAMETERS").get(null));
        Object overworldKey = Extract.field(settingsC, "OVERWORLD").get(null);
        settings = mValue.invoke(mGetOrThrow.invoke(settingsLookup, overworldKey));

        Class<?> regC = Extract.cls("net/minecraft/registry/Registry");
        blockReg = Extract.field(Extract.cls("net/minecraft/registry/Registries"), "BLOCK").get(null);
        mGetId = Extract.method(regC, "getId", Object.class);
        mBlockOf = Extract.method(Extract.cls("net/minecraft/block/AbstractBlock$AbstractBlockState"), "getBlock");
        mDefState = Extract.method(blockC, "getDefaultState");
        Class<?> blocksC = Extract.cls("net/minecraft/block/Blocks");
        String[] bn = {"AIR", "WATER", "LAVA"};
        for (int i = 0; i < 3; i++) kindState[i] = mDefState.invoke(Extract.field(blocksC, bn[i]).get(null));

        Class<?> posImpl = Extract.cls("net/minecraft/world/gen/densityfunction/DensityFunction$UnblendedNoisePos");
        posCtor = posImpl.getDeclaredConstructor(int.class, int.class, int.class);
        posCtor.setAccessible(true);
        flCtor = flC.getDeclaredConstructor(int.class, stateC);
        flCtor.setAccessible(true);
        fFlY = Extract.field(flC, "y");
        fFlState = Extract.field(flC, "state");
        Class<?> beardC = Extract.cls("net/minecraft/world/gen/densityfunction/DensityFunctionTypes$Beardifier");
        beardifier = Extract.field(beardC, "INSTANCE").get(null);
        noBlend = Extract.method(Extract.cls("net/minecraft/world/gen/chunk/Blender"), "getNoBlending").invoke(null);
        for (Constructor<?> c : cnsC.getDeclaredConstructors()) if (c.getParameterCount() == 9) cnsCtor = c;
        cnsCtor.setAccessible(true);
        fAquifer = Extract.field(cnsC, "aquiferSampler");
        Class<?> mutC = Class.forName("org.apache.commons.lang3.mutable.MutableDouble", false, Extract.loader);
        mutCtor = mutC.getConstructor(double.class);
        mSample = Extract.method(dfC, "sample", posC);
    }

    static Object noiseConfig(long seed) throws Exception {
        Object nc = noiseConfigs.get(seed);
        if (nc == null) {
            nc = Extract.method(ncC, "create", settingsC, Extract.cls("net/minecraft/registry/RegistryEntryLookup"), long.class)
                    .invoke(null, settings, noiseParams, seed);
            noiseConfigs.put(seed, nc);
        }
        return nc;
    }

    static Object pos(int x, int y, int z) throws Exception { return posCtor.newInstance(x, y, z); }

    static Object level(int y, int kind) throws Exception { return flCtor.newInstance(y, kindState[kind]); }

    static int kindOf(Object state) throws Exception {
        String id = String.valueOf(mGetId.invoke(blockReg, mBlockOf.invoke(state)));
        for (int i = 0; i < 3; i++) if (id.equals("minecraft:" + KIND[i])) return i;
        throw new IllegalStateException("fluid answer is neither air, water nor lava: " + id);
    }

    // the chunk's own aquifer, built the way the game builds it (inside its ChunkNoiseSampler)
    static Object aquifer(long seed, int cx, int cz) throws Exception {
        String key = seed + "/" + cx + "/" + cz;
        Object[] have = aquifers.get(key);
        if (have == null) {
            Object nc = noiseConfig(seed);
            Object shape = Extract.method(settingsC, "generationShapeConfig").invoke(settings);
            int cell = (Integer) Extract.method(shape.getClass(), "horizontalCellBlockCount").invoke(shape);
            Object fluidSampler = Extract.method(Extract.cls("net/minecraft/world/gen/chunk/NoiseChunkGenerator"),
                    "createFluidLevelSampler", settingsC).invoke(null, settings);
            Object cns = cnsCtor.newInstance(16 / cell, nc, cx * 16, cz * 16, shape, beardifier, settings, fluidSampler, noBlend);
            have = new Object[] {fAquifer.get(cns), cns};
            aquifers.put(key, have);
        }
        return have[0];
    }

    static String num(double d) { return Double.toString(d); }

    static String answer(String[] q, Object entries, Method mEntryGet, ThreadLocal<Object> previous,
                         Constructor<?> pointCtor, List<long[]> leafBox, List<String> leafBiome) throws Exception {
        switch (q[0]) {
            case "xoro": return xoro(Long.parseLong(q[1]));
            case "biome": {
                long seed = Long.parseLong(q[1]);
                int qx = Integer.parseInt(q[2]), qy = Integer.parseInt(q[3]), qz = Integer.parseInt(q[4]);
                Object sampler = Extract.method(ncC, "getMultiNoiseSampler").invoke(noiseConfig(seed));
                Object pt = Extract.method(sampler.getClass(), "sample", int.class, int.class, int.class).invoke(sampler, qx, qy, qz);
                long[] v = (long[]) Extract.method(pt.getClass(), "getNoiseValueList").invoke(pt);
                previous.remove();
                String b = String.valueOf(mEntryGet.invoke(entries, pointCtor.newInstance(v[0], v[1], v[2], v[3], v[4], v[5])));
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
                return v[0] + "\t" + v[1] + "\t" + v[2] + "\t" + v[3] + "\t" + v[4] + "\t" + v[5] + "\t" + b + "\t" + (atBest.size() > 1 ? 1 : 0);
            }
            case "density": {
                Object router = Extract.method(ncC, "getNoiseRouter").invoke(noiseConfig(Long.parseLong(q[1])));
                Object df = Extract.method(router.getClass(), q[2]).invoke(router);
                return num((Double) mSample.invoke(df, pos(Integer.parseInt(q[3]), Integer.parseInt(q[4]), Integer.parseInt(q[5]))));
            }
        }
        // the aquifer helpers: seed, chunk x, chunk z, then the call's own arguments
        long seed = Long.parseLong(q[1]);
        Object aq = aquifer(seed, Integer.parseInt(q[2]), Integer.parseInt(q[3]));
        int[] n = new int[q.length];                       // integer arguments (others stay 0, read from q)
        for (int i = 4; i < q.length; i++) {
            try { n[i] = Integer.parseInt(q[i]); } catch (NumberFormatException e) { /* a boolean or a double */ }
        }
        Class<?> I = int.class;
        switch (q[0]) {
            case "aq_noise_based_fluid_level":
                return String.valueOf(Extract.method(implC, "getNoiseBasedFluidLevel", I, I, I, I).invoke(aq, n[4], n[5], n[6], n[7]));
            case "aq_fluid_block_y":
                return String.valueOf(Extract.method(implC, "getFluidBlockY", I, I, I, flC, I, boolean.class)
                        .invoke(aq, n[4], n[5], n[6], level(n[7], n[8]), n[9], Boolean.parseBoolean(q[10])));
            case "aq_fluid_block_kind":
                return String.valueOf(kindOf(Extract.method(implC, "getFluidBlockState", I, I, I, flC, I)
                        .invoke(aq, n[4], n[5], n[6], level(n[7], n[8]), n[9])));
            case "aq_fluid_level": {
                Object fl = Extract.method(implC, "getFluidLevel", I, I, I).invoke(aq, n[4], n[5], n[6]);
                return fFlY.get(fl) + "\t" + kindOf(fFlState.get(fl));
            }
            case "aq_calculate_density": {
                Object barrier = mutCtor.newInstance(Double.NaN);
                Object r = Extract.method(implC, "calculateDensity", posC, barrier.getClass(), flC, flC)
                        .invoke(aq, pos(n[4], n[5], n[6]), barrier, level(n[7], n[8]), level(n[9], n[10]));
                return num((Double) r);
            }
            case "aq_apply": {
                Object st = Extract.method(aqC, "apply", posC, double.class)
                        .invoke(aq, pos(n[4], n[5], n[6]), Double.parseDouble(q[7]));
                return st == null ? "none" : KIND[kindOf(st)];
            }
        }
        throw new IllegalArgumentException("unknown query " + q[0]);
    }

    // one fixed script of draws against a fresh generator, mirrored one-to-one by the test
    static String xoro(long seed) throws Exception {
        Class<?> rc = Extract.cls("net/minecraft/util/math/random/Xoroshiro128PlusPlusRandom");
        Class<?> randC = Extract.cls("net/minecraft/util/math/random/Random");
        Class<?> splitC = Extract.cls("net/minecraft/util/math/random/RandomSplitter");
        Constructor<?> c = rc.getDeclaredConstructor(long.class);
        c.setAccessible(true);
        Object r = c.newInstance(seed);
        Method nl = Extract.method(randC, "nextLong");
        List<String> o = new ArrayList<>();
        o.add(String.valueOf(nl.invoke(r)));
        o.add(String.valueOf(nl.invoke(r)));
        o.add(String.valueOf(Extract.method(randC, "nextInt").invoke(r)));
        o.add(String.valueOf(Extract.method(randC, "nextInt", int.class).invoke(r, 100)));
        o.add(String.valueOf(Extract.method(randC, "nextInt", int.class).invoke(r, 1000003)));
        o.add(num((Double) Extract.method(randC, "nextDouble").invoke(r)));
        o.add((Boolean) Extract.method(randC, "nextBoolean").invoke(r) ? "1" : "0");
        Object child = Extract.method(randC, "split").invoke(r);
        o.add(String.valueOf(nl.invoke(child)));
        Object sp = Extract.method(randC, "nextSplitter").invoke(r);
        o.add(String.valueOf(nl.invoke(Extract.method(splitC, "split", String.class).invoke(sp, "minecraft:test_stream"))));
        o.add(String.valueOf(nl.invoke(Extract.method(splitC, "split", int.class, int.class, int.class).invoke(sp, -37, 64, 1021))));
        return String.join("\t", o);
    }

    static List<String> generate() {
        List<String> q = new ArrayList<>();
        Random rnd = new Random(20261025L);
        long[] seeds = {0L, 1211L, 20260101L, -7046029254386353131L, Long.MAX_VALUE};
        for (long s : seeds) q.add("xoro\t" + s);
        // multi-noise lookups: quart coordinates (x, y, z) over a few thousand blocks
        long[] biomeSeeds = {1211L, 20260101L, -7046029254386353131L};
        for (long s : biomeSeeds)
            for (int i = 0; i < 60; i++)
                q.add("biome\t" + s + "\t" + (rnd.nextInt(8001) - 4000) + "\t" + (rnd.nextInt(80) - 16) + "\t" + (rnd.nextInt(8001) - 4000));
        // the climate functions the biome source reads, at block coordinates
        String[] names = {"continents", "erosion", "depth", "ridges"};
        for (long s : new long[] {0L, 20260101L})
            for (String nm : names)
                for (int i = 0; i < 24; i++)
                    q.add("density\t" + s + "\t" + nm + "\t" + (rnd.nextInt(6001) - 3000) + "\t" + (rnd.nextInt(256) - 64) + "\t" + (rnd.nextInt(6001) - 3000));
        // the aquifer's helpers in one chunk, queried at positions around it
        long seed = 1211L;
        int cx = -3, cz = 5;
        String ctx = seed + "\t" + cx + "\t" + cz + "\t";
        for (int i = 0; i < 160; i++) {
            int x = rnd.nextInt(241) - 120, y = rnd.nextInt(192) - 64, z = rnd.nextInt(241) - 120;
            q.add("aq_noise_based_fluid_level\t" + ctx + x + "\t" + y + "\t" + z + "\t" + (rnd.nextInt(161) - 32));
            int defY = rnd.nextBoolean() ? 63 : rnd.nextInt(60) - 54;
            q.add("aq_fluid_block_y\t" + ctx + x + "\t" + y + "\t" + z + "\t" + defY + "\t1\t" + (rnd.nextInt(160) - 32) + "\t" + rnd.nextBoolean());
            q.add("aq_fluid_block_kind\t" + ctx + x + "\t" + y + "\t" + z + "\t" + defY + "\t1\t" + (rnd.nextInt(140) - 70));
            q.add("aq_fluid_level\t" + ctx + x + "\t" + y + "\t" + z);
            q.add("aq_calculate_density\t" + ctx + x + "\t" + y + "\t" + z + "\t" + (rnd.nextInt(140) - 70) + "\t1\t" + (rnd.nextInt(140) - 70) + "\t1");
        }
        // aquifer.apply for positions in the chunk itself (the game indexes its caches by chunk)
        double[] dens = {-0.45, -0.2, -0.05, -0.01, 0.0, 0.004, 0.05, 0.15};
        for (int i = 0; i < 200; i++)
            q.add("aq_apply\t" + ctx + (cx * 16 + rnd.nextInt(16)) + "\t" + (rnd.nextInt(192) - 64) + "\t" + (cz * 16 + rnd.nextInt(16)) + "\t" + dens[rnd.nextInt(dens.length)]);
        return q;
    }

    static void write(Path out, Path queriesFile, Object entries, Method mEntryGet, ThreadLocal<Object> previous,
                      Constructor<?> pointCtor, List<long[]> leafBox, List<String> leafBiome) throws Exception {
        init();
        List<String> qs = queriesFile == null ? generate() : Files.readAllLines(queriesFile);
        try (PrintWriter w = new PrintWriter(Files.newBufferedWriter(out))) {
            for (String line : qs) {
                if (line.isBlank()) continue;
                String[] q = line.split("\t");
                w.println(line + "\t|\t" + answer(q, entries, mEntryGet, previous, pointCtor, leafBox, leafBiome));
            }
        }
    }
}
