import java.io.File;
import java.lang.instrument.*;
import java.security.ProtectionDomain;
import java.util.jar.JarFile;
import org.objectweb.asm.*;

/** Appends CarveLog to the bootstrap search path and rewrites applyCarvers. */
public class Agent {
    /** Frames must be recomputed after inserting branches; keep merges as Object. */
    static ClassWriter writer(ClassReader cr) {
        return new ClassWriter(cr, ClassWriter.COMPUTE_FRAMES | ClassWriter.COMPUTE_MAXS) {
            @Override protected String getCommonSuperClass(String a, String b) {
                return "java/lang/Object";
            }
        };
    }

    public static void premain(String args, Instrumentation inst) {
        try {
            File self = new File(Agent.class.getProtectionDomain().getCodeSource().getLocation().toURI());
            File dir = self.getParentFile();
            File log = new File(dir, "carvelog.jar");
            inst.appendToBootstrapClassLoaderSearch(new JarFile(log));
            System.err.println("CARVEAGENT logger=" + log);
        } catch (Throwable t) {
            System.err.println("CARVEAGENT bootstrap failed: " + t);
        }
        inst.addTransformer(new Transformer());
    }

    /** `WorldCarver.getCarveState(ctx, cfg, pos, aquifer)` -> log pos + state id. */
    static byte[] instrumentCarveState(byte[] buf) {
        try {
            ClassReader cr = new ClassReader(buf);
            ClassWriter cw = writer(cr);
            cr.accept(new ClassVisitor(Opcodes.ASM9, cw) {
                @Override public MethodVisitor visitMethod(int acc, String mn, String md,
                                                           String sig, String[] ex) {
                    MethodVisitor mv = super.visitMethod(acc, mn, md, sig, ex);
                    if (!mn.equals("a") || !md.equals("(Leaz;Leax;Ljd;Ldyk;)Ldtc;")) return mv;
                    return new MethodVisitor(Opcodes.ASM9, mv) {
                        @Override public void visitInsn(int op) {
                            if (op == Opcodes.ARETURN) {
                                Label isNull = new Label(), join = new Label();
                                super.visitVarInsn(Opcodes.ASTORE, 40);
                                super.visitVarInsn(Opcodes.ALOAD, 40);
                                super.visitJumpInsn(Opcodes.IFNULL, isNull);
                                super.visitFieldInsn(Opcodes.GETSTATIC, "dfy", "q", "Ljs;");
                                super.visitVarInsn(Opcodes.ALOAD, 40);
                                super.visitMethodInsn(Opcodes.INVOKEVIRTUAL, "js", "a",
                                    "(Ljava/lang/Object;)I", false);
                                super.visitJumpInsn(Opcodes.GOTO, join);
                                super.visitLabel(isNull);
                                super.visitInsn(Opcodes.ICONST_M1);
                                super.visitLabel(join);
                                super.visitVarInsn(Opcodes.ISTORE, 41);
                                super.visitVarInsn(Opcodes.ALOAD, 3);
                                super.visitMethodInsn(Opcodes.INVOKEVIRTUAL, "jd", "u", "()I", false);
                                super.visitVarInsn(Opcodes.ALOAD, 3);
                                super.visitMethodInsn(Opcodes.INVOKEVIRTUAL, "jd", "v", "()I", false);
                                super.visitVarInsn(Opcodes.ALOAD, 3);
                                super.visitMethodInsn(Opcodes.INVOKEVIRTUAL, "jd", "w", "()I", false);
                                super.visitVarInsn(Opcodes.ILOAD, 41);
                                super.visitMethodInsn(Opcodes.INVOKESTATIC, "CarveLog", "state",
                                    "(IIII)V", false);
                                super.visitVarInsn(Opcodes.ALOAD, 40);
                            }
                            super.visitInsn(op);
                        }
                    };
                }
            }, 0);
            return cw.toByteArray();
        } catch (Throwable t) {
            System.err.println("CARVEAGENT carveState transform failed: " + t);
            return null;
        }
    }

    static class Transformer implements ClassFileTransformer {
        @Override public byte[] transform(ClassLoader l, String name, Class<?> c,
                                          ProtectionDomain pd, byte[] buf) {
            if ("ebe".equals(name)) return instrumentCarveState(buf);
            if (!"dzb".equals(name)) return null;
            try {
                ClassReader cr = new ClassReader(buf);
                ClassWriter cw = writer(cr);
                cr.accept(new ClassVisitor(Opcodes.ASM9, cw) {
                    @Override public MethodVisitor visitMethod(int acc, String mn, String md,
                                                               String sig, String[] ex) {
                        MethodVisitor mv = super.visitMethod(acc, mn, md, sig, ex);
                        if (!mn.equals("a") || !md.startsWith("(Larb;JLdzm;")) return mv;
                        return new MethodVisitor(Opcodes.ASM9, mv) {
                            @Override public void visitMethodInsn(int op, String owner, String n,
                                                                 String d, boolean itf) {
                                if (owner.equals("ebc") && n.equals("a") && d.equals("(Layw;)Z")) {
                                    super.visitVarInsn(Opcodes.ALOAD, 12);
                                    super.visitFieldInsn(Opcodes.GETFIELD, "dcd", "e", "I");
                                    super.visitVarInsn(Opcodes.ALOAD, 12);
                                    super.visitFieldInsn(Opcodes.GETFIELD, "dcd", "f", "I");
                                    super.visitVarInsn(Opcodes.ALOAD, 19);
                                    super.visitFieldInsn(Opcodes.GETFIELD, "dcd", "e", "I");
                                    super.visitVarInsn(Opcodes.ALOAD, 19);
                                    super.visitFieldInsn(Opcodes.GETFIELD, "dcd", "f", "I");
                                    super.visitVarInsn(Opcodes.ILOAD, 23);
                                    super.visitVarInsn(Opcodes.LLOAD, 2);
                                    super.visitVarInsn(Opcodes.ILOAD, 23);
                                    super.visitInsn(Opcodes.I2L);
                                    super.visitInsn(Opcodes.LADD);
                                    super.visitMethodInsn(Opcodes.INVOKESTATIC, "CarveLog", "pre",
                                        "(IIIIIJ)V", false);
                                    super.visitMethodInsn(op, owner, n, d, itf);
                                    super.visitVarInsn(Opcodes.ISTORE, 40);
                                    super.visitVarInsn(Opcodes.ALOAD, 12);
                                    super.visitFieldInsn(Opcodes.GETFIELD, "dcd", "e", "I");
                                    super.visitVarInsn(Opcodes.ALOAD, 12);
                                    super.visitFieldInsn(Opcodes.GETFIELD, "dcd", "f", "I");
                                    super.visitVarInsn(Opcodes.ALOAD, 19);
                                    super.visitFieldInsn(Opcodes.GETFIELD, "dcd", "e", "I");
                                    super.visitVarInsn(Opcodes.ALOAD, 19);
                                    super.visitFieldInsn(Opcodes.GETFIELD, "dcd", "f", "I");
                                    super.visitVarInsn(Opcodes.ILOAD, 23);
                                    super.visitVarInsn(Opcodes.ILOAD, 40);
                                    super.visitMethodInsn(Opcodes.INVOKESTATIC, "CarveLog", "post",
                                        "(IIIIIZ)V", false);
                                    super.visitVarInsn(Opcodes.ILOAD, 40);
                                    return;
                                }
                                super.visitMethodInsn(op, owner, n, d, itf);
                            }
                        };
                    }
                }, 0);
                return cw.toByteArray();
            } catch (Throwable t) {
                System.err.println("CARVEAGENT transform failed: " + t);
                return null;
            }
        }
    }
}
