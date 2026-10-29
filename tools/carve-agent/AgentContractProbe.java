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
        System.setProperty("sculktrace", trace.toString());
        System.setProperty("sculkdrawpass", "0,0");
        Map<String, byte[]> classes = new HashMap<>();
        for (String name : new String[]{"dds", "duz", "ayw", "ece"}) {
            classes.put(name, type(name).toByteArray());
        }
        ClassWriter pos = type("jd");
        zeroMethod(pos, "u", "()I");
        zeroMethod(pos, "w", "()I");
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
        ClassLoader loader = new ClassLoader(AgentContractProbe.class.getClassLoader()) {
            @Override protected Class<?> findClass(String name) throws ClassNotFoundException {
                byte[] bytes = classes.get(name);
                if (bytes == null) throw new ClassNotFoundException(name);
                return defineClass(name, bytes, 0, bytes.length);
            }
        };
        Class<?> world = loader.loadClass("dds"), generator = loader.loadClass("duz");
        Class<?> random = loader.loadClass("ayw"), position = loader.loadClass("jd");
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
        // Production flushes at shutdown; inspect the actual buffered output now.
        var writer = SculkLog.class.getDeclaredField("writer");
        writer.setAccessible(true);
        ((java.io.PrintWriter) writer.get(null)).flush();
        String output = Files.readString(trace);
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
    }
}
