import java.nio.file.Files;
import java.nio.file.Path;
import java.util.HashMap;
import java.util.Map;
import org.objectweb.asm.ClassWriter;
import org.objectweb.asm.MethodVisitor;
import org.objectweb.asm.Opcodes;

/** Execute a transformed ConfiguredFeature return inside an armed placement.
 * Minimal game-shaped classes keep this independent of Minecraft downloads.
 */
public final class AgentContractProbe implements Opcodes {
    private static ClassWriter type(String name) {
        ClassWriter writer = new ClassWriter(ClassWriter.COMPUTE_FRAMES | ClassWriter.COMPUTE_MAXS);
        writer.visit(V17, ACC_PUBLIC, name, null, "java/lang/Object", null);
        MethodVisitor init = writer.visitMethod(ACC_PUBLIC, "<init>", "()V", null, null);
        init.visitCode();
        init.visitVarInsn(ALOAD, 0);
        init.visitMethodInsn(INVOKESPECIAL, "java/lang/Object", "<init>", "()V", false);
        init.visitInsn(RETURN);
        init.visitMaxs(0, 0);
        init.visitEnd();
        return writer;
    }

    private static void zeroMethod(ClassWriter writer, String name, String signature) {
        MethodVisitor method = writer.visitMethod(ACC_PUBLIC, name, signature, null, null);
        method.visitCode();
        method.visitInsn(ICONST_0);
        method.visitInsn(IRETURN);
        method.visitMaxs(0, 0);
        method.visitEnd();
    }

    public static void main(String[] args) throws Exception {
        Path trace = Path.of(args[0]);
        Path popOrder = trace.resolveSibling("pop-order.tsv");
        System.setProperty("sculktrace", trace.toString());
        System.setProperty("sculkdrawpass", "0,0");
        System.setProperty("dim", "-64");
        System.setProperty("popchunk", "1");
        System.setProperty("poporder", popOrder.toString());
        Map<String, byte[]> classes = new HashMap<>();
        for (String name : new String[]{"ayw", "ece", "ddq"}) {
            classes.put(name, type(name).toByteArray());
        }
        // dcy (LevelHeightAccessor) interface with I_() -> minBuildHeight, implemented by dds
        ClassWriter dcy = new ClassWriter(ClassWriter.COMPUTE_FRAMES | ClassWriter.COMPUTE_MAXS);
        dcy.visit(V17, ACC_PUBLIC | ACC_INTERFACE | ACC_ABSTRACT, "dcy", null, "java/lang/Object", null);
        dcy.visitMethod(ACC_PUBLIC | ACC_ABSTRACT, "I_", "()I", null, null).visitEnd();
        dcy.visitEnd();
        classes.put("dcy", dcy.toByteArray());

        ClassWriter dds = new ClassWriter(ClassWriter.COMPUTE_FRAMES | ClassWriter.COMPUTE_MAXS);
        dds.visit(V17, ACC_PUBLIC, "dds", null, "java/lang/Object", new String[]{"dcy"});
        MethodVisitor ddsInit = dds.visitMethod(ACC_PUBLIC, "<init>", "()V", null, null);
        ddsInit.visitCode();
        ddsInit.visitVarInsn(ALOAD, 0);
        ddsInit.visitMethodInsn(INVOKESPECIAL, "java/lang/Object", "<init>", "()V", false);
        ddsInit.visitInsn(RETURN);
        ddsInit.visitMaxs(0, 0);
        ddsInit.visitEnd();
        dds.visitField(ACC_PUBLIC, "minY", "I", null, null).visitEnd();
        MethodVisitor getMinY = dds.visitMethod(ACC_PUBLIC, "I_", "()I", null, null);
        getMinY.visitCode();
        getMinY.visitVarInsn(ALOAD, 0);
        getMinY.visitFieldInsn(GETFIELD, "dds", "minY", "I");
        getMinY.visitInsn(IRETURN);
        getMinY.visitMaxs(0, 0);
        getMinY.visitEnd();
        classes.put("dds", dds.toByteArray());

        ClassWriter dcd = type("dcd");
        dcd.visitField(ACC_PUBLIC, "e", "I", null, null).visitEnd();
        dcd.visitField(ACC_PUBLIC, "f", "I", null, null).visitEnd();
        classes.put("dcd", dcd.toByteArray());

        ClassWriter duy = type("duy");
        duy.visitField(ACC_PUBLIC, "pos", "Ldcd;", null, null).visitEnd();
        MethodVisitor getPos = duy.visitMethod(ACC_PUBLIC, "f", "()Ldcd;", null, null);
        getPos.visitCode();
        getPos.visitVarInsn(ALOAD, 0);
        getPos.visitFieldInsn(GETFIELD, "duy", "pos", "Ldcd;");
        getPos.visitInsn(ARETURN);
        getPos.visitMaxs(0, 0);
        getPos.visitEnd();
        classes.put("duy", duy.toByteArray());

        ClassWriter rawDuz = type("duz");
        MethodVisitor applyDeco = rawDuz.visitMethod(ACC_PUBLIC, "a", "(Ldds;Lduy;Lddq;)V", null, null);
        applyDeco.visitCode();
        applyDeco.visitInsn(RETURN);
        applyDeco.visitMaxs(0, 0);
        applyDeco.visitEnd();
        byte[] duzTransformed = new SculkTransformer().transform(
                null, "duz", null, null, rawDuz.toByteArray());
        if (duzTransformed == null) throw new AssertionError("ChunkGenerator duz hook absent");
        classes.put("duz", duzTransformed);

        ClassWriter pos = type("jd");
        // Distinct coordinates per axis: the mark hook reads `u()`, `v()` and `w()` reflectively,
        // and a transposed pair (y from w, z from v) is invisible when every axis is zero.
        for (String[] g : new String[][]{{"u", "82"}, {"v", "69"}, {"w", "-500"}}) {
            MethodVisitor axis = pos.visitMethod(ACC_PUBLIC, g[0], "()I", null, null);
            axis.visitCode();
            axis.visitLdcInsn(Integer.valueOf(g[1]));
            axis.visitInsn(IRETURN);
            axis.visitMaxs(0, 0);
            axis.visitEnd();
        }
        classes.put("jd", pos.toByteArray());
        ClassWriter feature = type("ebq");
        MethodVisitor value = feature.visitMethod(ACC_PUBLIC, "b", "()Lece;", null, null);
        value.visitCode();
        value.visitTypeInsn(NEW, "ece");
        value.visitInsn(DUP);
        value.visitMethodInsn(INVOKESPECIAL, "ece", "<init>", "()V", false);
        value.visitInsn(ARETURN);
        value.visitMaxs(0, 0);
        value.visitEnd();
        zeroMethod(feature, "a", "(Ldds;Lduz;Layw;Ljd;)Z");
        byte[] transformed = new SculkTransformer().transform(
                null, "ebq", null, null, feature.toByteArray());
        if (transformed == null) throw new AssertionError("ConfiguredFeature hook absent");
        classes.put("ebq", transformed);

        // `ChunkAccess.markPosForPostprocessing`: execute the transformed ProtoChunk body and
        // require that the hook not only exists but fires and decodes the cell. A hook whose
        // marker strings are present but whose transform never matches looks identical to a run
        // that filed no mark - the failure mode this whole lane exists to rule out.
        System.setProperty("marktrace", "all");
        ClassWriter dvs = new ClassWriter(ClassWriter.COMPUTE_FRAMES | ClassWriter.COMPUTE_MAXS);
        dvs.visit(V17, ACC_PUBLIC, "dvs", null, "duy", null);
        MethodVisitor dvsInit = dvs.visitMethod(ACC_PUBLIC, "<init>", "()V", null, null);
        dvsInit.visitCode();
        dvsInit.visitVarInsn(ALOAD, 0);
        dvsInit.visitMethodInsn(INVOKESPECIAL, "duy", "<init>", "()V", false);
        dvsInit.visitInsn(RETURN);
        dvsInit.visitMaxs(0, 0);
        dvsInit.visitEnd();
        MethodVisitor markBody = dvs.visitMethod(ACC_PUBLIC, "e", "(Ljd;)V", null, null);
        markBody.visitCode();
        markBody.visitInsn(RETURN);
        markBody.visitMaxs(0, 0);
        markBody.visitEnd();
        byte[] dvsTransformed = new Agent.Transformer().transform(
                null, "dvs", null, null, dvs.toByteArray());
        if (dvsTransformed == null) throw new AssertionError("ProtoChunk mark hook absent");
        classes.put("dvs", dvsTransformed);
        System.clearProperty("marktrace");
        ClassLoader loader = new ClassLoader(AgentContractProbe.class.getClassLoader()) {
            @Override protected Class<?> findClass(String name) throws ClassNotFoundException {
                byte[] bytes = classes.get(name);
                if (bytes == null) throw new ClassNotFoundException(name);
                return defineClass(name, bytes, 0, bytes.length);
            }
        };
        Class<?> world = loader.loadClass("dds"), generator = loader.loadClass("duz");
        Class<?> random = loader.loadClass("ayw"), position = loader.loadClass("jd");
        Class<?> chunkAccess = loader.loadClass("duy"), chunkPos = loader.loadClass("dcd");
        Class<?> structMgr = loader.loadClass("ddq");
        Class<?> configured = loader.loadClass("ebq");

        Object instance = configured.getConstructor().newInstance();
        SculkLog.armPlacement();
        SculkLog.windowStart();
        configured.getMethod("a", world, generator, random, position).invoke(instance,
                null, null, random.getConstructor().newInstance(), position.getConstructor().newInstance());
        SculkLog.drawI(42, "after-configured-return");
        SculkLog.armPlacement();
        SculkLog.windowStart();
        SculkLog.windowEnd();
        SculkLog.disarm();
        SculkLog.drawI(44, "after-nested-placement-return");
        SculkLog.windowEnd();
        SculkLog.disarm();
        SculkLog.drawI(43, "after-placement-return");

        Object genInstance = generator.getConstructor().newInstance();
        Object netherWorld = world.getConstructor().newInstance();
        world.getField("minY").setInt(netherWorld, 0);
        Object netherPos = chunkPos.getConstructor().newInstance();
        chunkPos.getField("e").setInt(netherPos, 9);
        chunkPos.getField("f").setInt(netherPos, 9);
        Object netherChunk = chunkAccess.getConstructor().newInstance();
        chunkAccess.getField("pos").set(netherChunk, netherPos);
        generator.getMethod("a", world, chunkAccess, structMgr).invoke(genInstance, netherWorld, netherChunk, null);

        Object owWorld = world.getConstructor().newInstance();
        world.getField("minY").setInt(owWorld, -64);
        Object owPos = chunkPos.getConstructor().newInstance();
        chunkPos.getField("e").setInt(owPos, 3);
        chunkPos.getField("f").setInt(owPos, -2);
        Object owChunk = chunkAccess.getConstructor().newInstance();
        chunkAccess.getField("pos").set(owChunk, owPos);
        generator.getMethod("a", world, chunkAccess, structMgr).invoke(genInstance, owWorld, owChunk, null);

        System.setProperty("sculkquiet", "true");
        Object owPos2 = chunkPos.getConstructor().newInstance();
        chunkPos.getField("e").setInt(owPos2, 3);
        chunkPos.getField("f").setInt(owPos2, -1);
        Object owChunk2 = chunkAccess.getConstructor().newInstance();
        chunkAccess.getField("pos").set(owChunk2, owPos2);
        generator.getMethod("a", world, chunkAccess, structMgr).invoke(genInstance, owWorld, owChunk2, null);
        System.clearProperty("sculkquiet");

        // Production flushes at shutdown; inspect the actual buffered output now.
        var writer = SculkLog.class.getDeclaredField("writer");
        writer.setAccessible(true);
        ((java.io.PrintWriter) writer.get(null)).flush();
        var popWriterField = SculkLog.class.getDeclaredField("popWriter");
        popWriterField.setAccessible(true);
        ((java.io.PrintWriter) popWriterField.get(null)).flush();
        String output = Files.readString(trace);
        String popOutput = Files.readString(popOrder);
        if (output.contains("9 9") || popOutput.contains("9\t9")) {
            throw new AssertionError("dimension filter failed to suppress minY=0 pass");
        }
        if (!output.contains("SPASS 3 -2") || !output.contains("POP_CHUNK 3 -2")) {
            throw new AssertionError("duz hook missing SPASS/POP_CHUNK for minY=-64");
        }
        if (!output.contains("POP_CHUNK 3 -1") || output.contains("SPASS 3 -1")) {
            throw new AssertionError("POP_CHUNK must still log under sculkquiet=true while SPASS is suppressed");
        }
        if (!popOutput.equals("3\t-2\n3\t-1\n")) {
            throw new AssertionError("unexpected poporder TSV: " + popOutput);
        }
        if (!output.contains("SDRAW after-configured-return 42")) {
            throw new AssertionError("ConfiguredFeature return disarmed the outer placement");
        }
        if (!output.contains("SDRAW after-nested-placement-return 44")) {
            throw new AssertionError("nested placement return disarmed the outer placement");
        }
        if (output.contains("SDRAW after-placement-return")) {
            throw new AssertionError("placement return failed to disarm");
        }
        System.out.println("agent placement lifetime: PASS");

        // --- mark hook: fired, and decoded u/v/w onto the right axes ---
        System.setProperty("marktrace", "all");
        Class<?> protoChunk = loader.loadClass("dvs");
        Object markChunk = protoChunk.getConstructor().newInstance();
        Object markChunkPos = chunkPos.getConstructor().newInstance();
        chunkPos.getField("e").setInt(markChunkPos, 5);
        chunkPos.getField("f").setInt(markChunkPos, -32);
        chunkAccess.getField("pos").set(markChunk, markChunkPos);
        protoChunk.getMethod("e", position).invoke(markChunk, position.getConstructor().newInstance());
        long markCalls = (Long) CarveLog.class.getDeclaredField("MARK_CALLS").get(null);
        @SuppressWarnings("unchecked")
        java.util.Set<String> markSeen = ((java.util.Map<String, Integer>)
                CarveLog.class.getDeclaredField("MARK_SEEN").get(null)).keySet();
        System.clearProperty("marktrace");
        if (markCalls < 1) throw new AssertionError("ProtoChunk mark hook never fired");
        String wantMark = "filed|5,-32|82,69,-500|";
        boolean decoded = false;
        for (String key : markSeen) if (key.startsWith(wantMark)) decoded = true;
        if (!decoded) throw new AssertionError("mark hook decoded the wrong cell: " + markSeen);
        System.out.println("agent mark insertion: PASS calls=" + markCalls);
    }
}
