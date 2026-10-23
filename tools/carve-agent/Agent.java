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

    static boolean hasTunnel(byte[] buf) {
        for (String d : CARVE_TUNNEL_DESCS) {
            if (new String(buf, java.nio.charset.StandardCharsets.ISO_8859_1).contains(d)) return true;
        }
        return false;
    }

    static final String[] CARVE_TUNNEL_DESCS = {
        // C is a generic parameter: erased as `eax` on WorldCarver, `eba` on
        // CaveWorldCarver. The tunnel lives on the subclass.
        "(Leaz;Leba;Lduy;Ljava/util/function/Function;JLdyk;DDDDDFFFIIDLdux;Lebe$a;)V",
        "(Leaz;Leax;Lduy;Ljava/util/function/Function;JLdyk;DDDDDFFFIIDLdux;Lebe$a;)V",
    };

    /** `WorldCarver.carveTunnel`: log seed, start, yaw/pitch and length. */
    static byte[] instrumentTunnel(byte[] buf) {
        try {
            ClassReader cr = new ClassReader(buf);
            ClassWriter cw = writer(cr);
            cr.accept(new ClassVisitor(Opcodes.ASM9, cw) {
                @Override public MethodVisitor visitMethod(int acc, String mn, String md,
                                                           String sig, String[] ex) {
                    MethodVisitor mv = super.visitMethod(acc, mn, md, sig, ex);
                    if (!mn.equals("a") || !java.util.Arrays.asList(CARVE_TUNNEL_DESCS).contains(md)) return mv;
                    return new MethodVisitor(Opcodes.ASM9, mv) {
                        @Override public void visitCode() {
                            super.visitCode();
                            // seed=5-6, x/y/z=8..17, yaw/pitch=18/19, count=22
                            super.visitVarInsn(Opcodes.LLOAD, 5);
                            super.visitVarInsn(Opcodes.DLOAD, 8);
                            super.visitVarInsn(Opcodes.DLOAD, 10);
                            super.visitVarInsn(Opcodes.DLOAD, 12);
                            super.visitVarInsn(Opcodes.FLOAD, 19);   // yaw
                            super.visitVarInsn(Opcodes.FLOAD, 20);   // pitch
                            super.visitVarInsn(Opcodes.ILOAD, 22);
                            super.visitMethodInsn(Opcodes.INVOKESTATIC, "CarveLog", "tunnel",
                                "(JDDDFFI)V", false);
                        }
                    };
                }
            }, 0);
            return cw.toByteArray();
        } catch (Throwable t) {
            System.err.println("CARVEAGENT tunnel transform failed: " + t);
            return null;
        }
    }

    /** `CaveWorldCarver.carve(...)`: track which source chunk the writes belong to. */
    static byte[] instrumentCarve(byte[] buf) {
        try {
            ClassReader cr = new ClassReader(buf);
            ClassWriter cw = writer(cr);
            cr.accept(new ClassVisitor(Opcodes.ASM9, cw) {
                @Override public MethodVisitor visitMethod(int acc, String mn, String md,
                                                           String sig, String[] ex) {
                    MethodVisitor mv = super.visitMethod(acc, mn, md, sig, ex);
                    if (!mn.equals("a") || !md.equals(
                            "(Leaz;Leba;Lduy;Ljava/util/function/Function;Layw;Ldyk;Ldcd;Ldux;)Z")) return mv;
                    return new MethodVisitor(Opcodes.ASM9, mv) {
                        @Override public void visitCode() {
                            super.visitCode();
                            super.visitVarInsn(Opcodes.ALOAD, 7);
                            super.visitFieldInsn(Opcodes.GETFIELD, "dcd", "e", "I");
                            super.visitVarInsn(Opcodes.ALOAD, 7);
                            super.visitFieldInsn(Opcodes.GETFIELD, "dcd", "f", "I");
                            super.visitMethodInsn(Opcodes.INVOKESTATIC, "CarveLog", "enter", "(II)V", false);
                        }
                        @Override public void visitInsn(int op) {
                            if (op == Opcodes.ARETURN) {
                                super.visitMethodInsn(Opcodes.INVOKESTATIC, "CarveLog", "exit", "()V", false);
                            }
                            super.visitInsn(op);
                        }
                    };
                }
            }, 0);
            return cw.toByteArray();
        } catch (Throwable t) {
            System.err.println("CARVEAGENT carve transform failed: " + t);
            return null;
        }
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

    /** Logs the LCG state that c(JII) (setLargeFeatureSeed) leaves behind. */
    static byte[] instrumentRSeed(byte[] buf) {
        try {
            ClassReader cr = new ClassReader(buf);
            ClassWriter cw = writer(cr);
            cr.accept(new ClassVisitor(Opcodes.ASM9, cw) {
                @Override public MethodVisitor visitMethod(int acc, String mn, String md,
                                                           String sig, String[] ex) {
                    MethodVisitor mv = super.visitMethod(acc, mn, md, sig, ex);
                    if (!mn.equals("c") || !md.equals("(JII)V")) return drawHook(mv, mn, md);
                    return new MethodVisitor(Opcodes.ASM9, mv) {
                        @Override public void visitCode() {
                            super.visitCode();
                            super.visitVarInsn(Opcodes.ILOAD, 3);
                            super.visitVarInsn(Opcodes.ILOAD, 4);
                            super.visitVarInsn(Opcodes.LLOAD, 1);
                            super.visitMethodInsn(Opcodes.INVOKESTATIC, "CarveLog", "rseedIn", "(IIJ)V", false);
                        }
                        @Override public void visitInsn(int op) {
                            if (op == Opcodes.RETURN) {
                                super.visitVarInsn(Opcodes.ILOAD, 3);
                                super.visitVarInsn(Opcodes.ILOAD, 4);
                                super.visitVarInsn(Opcodes.ALOAD, 0);
                                super.visitMethodInsn(Opcodes.INVOKESTATIC, "CarveLog", "rseedOut",
                                                      "(IILjava/lang/Object;)V", false);
                            }
                            super.visitInsn(op);
                        }
                    };
                }
            }, 0);
            return cw.toByteArray();
        } catch (Throwable t) {
            System.err.println("CARVEAGENT rseed failed: " + t);
            return null;
        }
    }

    /** dyz.c(int) is next(bits), the single funnel behind every dyn draw. */
    static byte[] instrumentBits(byte[] buf) {
        try {
            ClassReader cr = new ClassReader(buf);
            ClassWriter cw = writer(cr);
            cr.accept(new ClassVisitor(Opcodes.ASM9, cw) {
                @Override public MethodVisitor visitMethod(int acc, String mn, String md,
                                                           String sig, String[] ex) {
                    MethodVisitor mv = super.visitMethod(acc, mn, md, sig, ex);
                    if (mn.equals("a") && md.equals("(I)I")) {
                        return new MethodVisitor(Opcodes.ASM9, mv) {
                            @Override public void visitInsn(int op) {
                                if (op != Opcodes.IRETURN) { super.visitInsn(op); return; }
                                super.visitVarInsn(Opcodes.ISTORE, 40);
                                super.visitVarInsn(Opcodes.ILOAD, 1);
                                super.visitInsn(Opcodes.I2L);
                                super.visitVarInsn(Opcodes.ILOAD, 40);
                                super.visitInsn(Opcodes.I2L);
                                super.visitMethodInsn(Opcodes.INVOKESTATIC, "CarveLog", "rbound",
                                                      "(JJ)V", false);
                                super.visitVarInsn(Opcodes.ILOAD, 40);
                                super.visitInsn(op);
                            }
                        };
                    }
                    if (!mn.equals("c") || !md.equals("(I)I")) return mv;
                    return new MethodVisitor(Opcodes.ASM9, mv) {
                        @Override public void visitInsn(int op) {
                            if (op != Opcodes.IRETURN) { super.visitInsn(op); return; }
                            super.visitVarInsn(Opcodes.ISTORE, 40);
                            super.visitVarInsn(Opcodes.ILOAD, 1);
                            super.visitInsn(Opcodes.I2L);
                            super.visitVarInsn(Opcodes.ILOAD, 40);
                            super.visitInsn(Opcodes.I2L);
                            super.visitMethodInsn(Opcodes.INVOKESTATIC, "CarveLog", "rbits",
                                                  "(JJ)V", false);
                            super.visitVarInsn(Opcodes.ILOAD, 40);
                            super.visitInsn(op);
                        }
                    };
                }
            }, 0);
            return cw.toByteArray();
        } catch (Throwable t) {
            System.err.println("CARVEAGENT bits failed: " + t);
            return null;
        }
    }

    /** XoroshiroRandomSource: the typed draws (it has no `c(int)` funnel). */
    static byte[] instrumentXorDraws(byte[] buf) {
        try {
            ClassReader cr = new ClassReader(buf);
            ClassWriter cw = writer(cr);
            cr.accept(new ClassVisitor(Opcodes.ASM9, cw) {
                @Override public MethodVisitor visitMethod(int acc, String mn, String md,
                                                           String sig, String[] ex) {
                    MethodVisitor mv = super.visitMethod(acc, mn, md, sig, ex);
                    if (mn.equals("a") && md.equals("(I)I")) return drawLog(mv, 4, true);
                    if (mn.equals("f") && md.equals("()I")) return drawLog(mv, 0, false);
                    return mv;
                }
            }, 0);
            return cw.toByteArray();
        } catch (Throwable t) {
            System.err.println("CARVEAGENT xor draws failed: " + t);
            return null;
        }
    }

    /** Wrap IRETURN: log (bound, value) then return it. */
    static MethodVisitor drawLog(MethodVisitor mv, int argSlot, boolean hasArg) {
        return new MethodVisitor(Opcodes.ASM9, mv) {
            @Override public void visitInsn(int op) {
                if (op != Opcodes.IRETURN) { super.visitInsn(op); return; }
                super.visitVarInsn(Opcodes.ISTORE, 40);
                if (hasArg) super.visitVarInsn(Opcodes.ILOAD, argSlot);
                else super.visitInsn(Opcodes.ICONST_0);
                super.visitInsn(Opcodes.I2L);
                super.visitVarInsn(Opcodes.ILOAD, 40);
                super.visitInsn(Opcodes.I2L);
                super.visitMethodInsn(Opcodes.INVOKESTATIC, "CarveLog", "rdraw2", "(JJ)V", false);
                super.visitVarInsn(Opcodes.ILOAD, 40);
                super.visitInsn(op);
            }
        };
    }

    /** setDecorationSeed / setFeatureSeed: log the per-feature seeding of a chunk. */
    static byte[] instrumentSeeds(byte[] buf) {
        try {
            ClassReader cr = new ClassReader(buf);
            ClassWriter cw = writer(cr);
            cr.accept(new ClassVisitor(Opcodes.ASM9, cw) {
                @Override public MethodVisitor visitMethod(int acc, String mn, String md,
                                                           String sig, String[] ex) {
                    MethodVisitor mv = super.visitMethod(acc, mn, md, sig, ex);
                    if (mn.equals("a") && md.equals("(JII)J")) {
                        return new MethodVisitor(Opcodes.ASM9, mv) {
                            @Override public void visitCode() {
                                super.visitCode();
                                super.visitVarInsn(Opcodes.LLOAD, 1);
                                super.visitVarInsn(Opcodes.ILOAD, 3);
                                super.visitVarInsn(Opcodes.ILOAD, 4);
                                super.visitMethodInsn(Opcodes.INVOKESTATIC, "CarveLog", "deco", "(JII)V", false);
                            }
                        };
                    }
                    if (mn.equals("b") && md.equals("(JII)V")) {
                        return new MethodVisitor(Opcodes.ASM9, mv) {
                            @Override public void visitCode() {
                                super.visitCode();
                                super.visitVarInsn(Opcodes.LLOAD, 1);
                                super.visitVarInsn(Opcodes.ILOAD, 3);
                                super.visitVarInsn(Opcodes.ILOAD, 4);
                                super.visitMethodInsn(Opcodes.INVOKESTATIC, "CarveLog", "fseed", "(JII)V", false);
                            }
                        };
                    }
                    return mv;
                }
            }, 0);
            return cw.toByteArray();
        } catch (Throwable t) {
            System.err.println("CARVEAGENT seeds failed: " + t);
            return null;
        }
    }

    /**
     * `RandomSource`'s default draws (`nextInt(bound)`, `nextFloat`) live on the
     * interface, not on the implementing class, so they can only be seen here.
     */
    static byte[] instrumentDynDraws(byte[] buf) {
        try {
            ClassReader cr = new ClassReader(buf);
            ClassWriter cw = writer(cr);
            cr.accept(new ClassVisitor(Opcodes.ASM9, cw) {
                @Override public MethodVisitor visitMethod(int acc, String mn, String md,
                                                           String sig, String[] ex) {
                    MethodVisitor mv = super.visitMethod(acc, mn, md, sig, ex);
                    boolean bound = mn.equals("a") && md.equals("(I)I");
                    boolean plain = mn.equals("f") && md.equals("()I");
                    boolean flt = mn.equals("i") && md.equals("()F");
                    if (!bound && !plain && !flt) return mv;
                    final int n = bound ? 2 : 1; // args pushed before the value
                    return new MethodVisitor(Opcodes.ASM9, mv) {
                        @Override public void visitInsn(int op) {
                            boolean ret = bound || plain ? op == Opcodes.IRETURN : op == Opcodes.FRETURN;
                            if (!ret) { super.visitInsn(op); return; }
                            super.visitVarInsn(plain || bound ? Opcodes.ISTORE : Opcodes.FSTORE, 40);
                            super.visitLdcInsn(bound ? "nextInt" : (plain ? "nextInt()" : "nextFloat"));
                            if (bound) { super.visitVarInsn(Opcodes.ILOAD, 1); super.visitInsn(Opcodes.I2L); }
                            else super.visitLdcInsn(Long.valueOf(0L));
                            super.visitVarInsn(plain || bound ? Opcodes.ILOAD : Opcodes.FLOAD, 40);
                            if (flt) super.visitInsn(Opcodes.F2L);
                            else if (plain || bound) super.visitInsn(Opcodes.I2L);
                            super.visitMethodInsn(Opcodes.INVOKESTATIC, "CarveLog", "rdraw",
                                                  "(Ljava/lang/String;JJ)V", false);
                            super.visitVarInsn(plain || bound ? Opcodes.ILOAD : Opcodes.FLOAD, 40);
                            super.visitInsn(op);
                        }
                    };
                }
            }, 0);
            return cw.toByteArray();
        } catch (Throwable t) {
            System.err.println("CARVEAGENT dyn draws failed: " + t);
            return null;
        }
    }

    /** InSquarePlacement: log the drawn (x, z) for every candidate. */
    static byte[] instrumentInSquare(byte[] buf) {
        try {
            ClassReader cr = new ClassReader(buf);
            ClassWriter cw = writer(cr);
            cr.accept(new ClassVisitor(Opcodes.ASM9, cw) {
                @Override public MethodVisitor visitMethod(int acc, String mn, String md,
                                                           String sig, String[] ex) {
                    MethodVisitor mv = super.visitMethod(acc, mn, md, sig, ex);
                    if (!mn.equals("a_") || !md.equals("(Leiw;Layw;Ljd;)Ljava/util/stream/Stream;")) {
                        return mv;
                    }
                    return new MethodVisitor(Opcodes.ASM9, mv) {
                        @Override public void visitInsn(int op) {
                            if (op != Opcodes.ARETURN) { super.visitInsn(op); return; }
                            // locals: 0=this, 1=eiw, 2=ayw, 3=jd, 4=blockX, 5=blockZ
                            super.visitVarInsn(Opcodes.ILOAD, 4);
                            super.visitVarInsn(Opcodes.ILOAD, 5);
                            super.visitMethodInsn(Opcodes.INVOKESTATIC, "CarveLog", "insq",
                                                  "(II)V", false);
                            super.visitInsn(op);
                        }
                    };
                }
            }, 0);
            return cw.toByteArray();
        } catch (Throwable t) {
            System.err.println("CARVEAGENT insquare failed: " + t);
            return null;
        }
    }

    /** WorldGenRegion.getNoiseBiome(int,int,int): the quart biome actually read. */
    static byte[] instrumentNoiseBiome(byte[] buf) {
        try {
            ClassReader cr = new ClassReader(buf);
            ClassWriter cw = writer(cr);
            cr.accept(new ClassVisitor(Opcodes.ASM9, cw) {
                @Override public MethodVisitor visitMethod(int acc, String mn, String md,
                                                           String sig, String[] ex) {
                    MethodVisitor mv = super.visitMethod(acc, mn, md, sig, ex);
                    if (!mn.equals("a") || !md.equals("(III)Ljm;")) return mv;
                    return new MethodVisitor(Opcodes.ASM9, mv) {
                        @Override public void visitInsn(int op) {
                            if (op != Opcodes.ARETURN) { super.visitInsn(op); return; }
                            // holder is the return value: stash it, push (qx,qy,qz),
                            // then reload it as the 4th argument
                            super.visitVarInsn(Opcodes.ASTORE, 40);
                            super.visitVarInsn(Opcodes.ILOAD, 1);
                            super.visitVarInsn(Opcodes.ILOAD, 2);
                            super.visitVarInsn(Opcodes.ILOAD, 3);
                            super.visitVarInsn(Opcodes.ALOAD, 40);
                            super.visitMethodInsn(Opcodes.INVOKESTATIC, "CarveLog", "qbiome",
                                                  "(IIILjava/lang/Object;)V", false);
                            super.visitVarInsn(Opcodes.ALOAD, 40);
                            super.visitInsn(op);
                        }
                    };
                }
            }, 0);
            return cw.toByteArray();
        } catch (Throwable t) {
            System.err.println("CARVEAGENT noisebiome failed: " + t);
            return null;
        }
    }

    /** BiomeFilter: log every candidate position and whether the biome accepts it. */
    static byte[] instrumentBiomeFilter(byte[] buf) {
        try {
            ClassReader cr = new ClassReader(buf);
            ClassWriter cw = writer(cr);
            cr.accept(new ClassVisitor(Opcodes.ASM9, cw) {
                @Override public MethodVisitor visitMethod(int acc, String mn, String md,
                                                           String sig, String[] ex) {
                    MethodVisitor mv = super.visitMethod(acc, mn, md, sig, ex);
                    if (!mn.equals("a") || !md.equals("(Leiw;Layw;Ljd;)Z")) return mv;
                    return new MethodVisitor(Opcodes.ASM9, mv) {
                        @Override public void visitInsn(int op) {
                            if (op != Opcodes.IRETURN) { super.visitInsn(op); return; }
                            super.visitVarInsn(Opcodes.ISTORE, 40);
                            super.visitVarInsn(Opcodes.ALOAD, 3);
                            for (String g : new String[] {"u", "v", "w"}) {
                                super.visitVarInsn(Opcodes.ALOAD, 3);
                                super.visitMethodInsn(Opcodes.INVOKEVIRTUAL, "kh", g, "()I", false);
                            }
                            super.visitVarInsn(Opcodes.ALOAD, 1);
                            super.visitVarInsn(Opcodes.ALOAD, 3);
                            super.visitVarInsn(Opcodes.ILOAD, 40);
                            super.visitMethodInsn(Opcodes.INVOKESTATIC, "CarveLog", "biomef3",
                                                  "(Ljava/lang/Object;Ljava/lang/Object;Z)V", false);
                            super.visitVarInsn(Opcodes.ILOAD, 40);
                            super.visitMethodInsn(Opcodes.INVOKESTATIC, "CarveLog", "biomef",
                                                  "(IIIZ)V", false);
                            super.visitVarInsn(Opcodes.ILOAD, 40);
                            super.visitInsn(op);
                        }
                    };
                }
            }, 0);
            return cw.toByteArray();
        } catch (Throwable t) {
            System.err.println("CARVEAGENT biomefilter failed: " + t);
            return null;
        }
    }

    /** FeatureSorter.buildFeaturesPerStep: log the biome iteration order. */
    static byte[] instrumentBiomes(byte[] buf) {
        try {
            ClassReader cr = new ClassReader(buf);
            ClassWriter cw = writer(cr);
            cr.accept(new ClassVisitor(Opcodes.ASM9, cw) {
                @Override public MethodVisitor visitMethod(int acc, String mn, String md,
                                                           String sig, String[] ex) {
                    MethodVisitor mv = super.visitMethod(acc, mn, md, sig, ex);
                    if (!mn.equals("a") || !md.startsWith("(Ljava/util/List;Ljava/util/function/Function;Z)")) {
                        return mv;
                    }
                    return new MethodVisitor(Opcodes.ASM9, mv) {
                        @Override public void visitCode() {
                            super.visitCode();
                            super.visitVarInsn(Opcodes.ALOAD, 0);
                            super.visitMethodInsn(Opcodes.INVOKESTATIC, "CarveLog", "biomes",
                                                  "(Ljava/lang/Object;)V", false);
                        }
                    };
                }
            }, 0);
            return cw.toByteArray();
        } catch (Throwable t) {
            System.err.println("CARVEAGENT biomes failed: " + t);
            return null;
        }
    }

    /**
     * ChunkGenerator.applyBiomeDecoration: remember which dimension is decorating.
     * The nether and the end decorate the same chunk coordinates with the same
     * level seed, so every per-chunk log needs this discriminator
     * (LevelHeightAccessor.getMinBuildHeight is -64 only in the overworld).
     */
    static byte[] instrumentDimension(byte[] buf) {
        try {
            ClassReader cr = new ClassReader(buf);
            ClassWriter cw = writer(cr);
            cr.accept(new ClassVisitor(Opcodes.ASM9, cw) {
                @Override public MethodVisitor visitMethod(int acc, String mn, String md,
                                                           String sig, String[] ex) {
                    MethodVisitor mv = super.visitMethod(acc, mn, md, sig, ex);
                    if (!mn.equals("a") || !md.startsWith("(Ldds;Lduy;")) return mv;
                    return new MethodVisitor(Opcodes.ASM9, mv) {
                        @Override public void visitCode() {
                            super.visitCode();
                            super.visitVarInsn(Opcodes.ALOAD, 1);
                            super.visitMethodInsn(Opcodes.INVOKEINTERFACE, "dcy", "I_", "()I", true);
                            super.visitMethodInsn(Opcodes.INVOKESTATIC, "CarveLog", "dim", "(I)V", false);
                        }
                    };
                }
            }, 0);
            return cw.toByteArray();
        } catch (Throwable t) {
            System.err.println("CARVEAGENT dimension failed: " + t);
            return null;
        }
    }

    /** TreeFeature.place: log the trunk origin of every tree in a chunk. */
    static byte[] instrumentTree(byte[] buf) {
        try {
            ClassReader cr = new ClassReader(buf);
            ClassWriter cw = writer(cr);
            cr.accept(new ClassVisitor(Opcodes.ASM9, cw) {
                @Override public MethodVisitor visitMethod(int acc, String mn, String md,
                                                           String sig, String[] ex) {
                    MethodVisitor mv = super.visitMethod(acc, mn, md, sig, ex);
                    if (!mn.equals("a") || !md.equals("(Lecg;)Z")) return mv;
                    return new MethodVisitor(Opcodes.ASM9, mv) {
                        @Override public void visitCode() {
                            super.visitCode();
                            for (String g : new String[] {"u", "v", "w"}) {
                                super.visitVarInsn(Opcodes.ALOAD, 1);
                                super.visitMethodInsn(Opcodes.INVOKEVIRTUAL, "ecg", "e", "()Ljd;", false);
                                super.visitMethodInsn(Opcodes.INVOKEVIRTUAL, "kh", g, "()I", false);
                            }
                            super.visitMethodInsn(Opcodes.INVOKESTATIC, "CarveLog", "tree", "(III)V", false);
                        }
                    };
                }
            }, 0);
            return cw.toByteArray();
        } catch (Throwable t) {
            System.err.println("CARVEAGENT tree failed: " + t);
            return null;
        }
    }

    /** OreFeature.place: log the origin and size of every blob in a chunk. */
    static byte[] instrumentOre(byte[] buf) {
        try {
            ClassReader cr = new ClassReader(buf);
            ClassWriter cw = writer(cr);
            cr.accept(new ClassVisitor(Opcodes.ASM9, cw) {
                @Override public MethodVisitor visitMethod(int acc, String mn, String md,
                                                           String sig, String[] ex) {
                    MethodVisitor mv = super.visitMethod(acc, mn, md, sig, ex);
                    if (!mn.equals("a") || !md.equals("(Lecg;)Z")) return mv;
                    return new MethodVisitor(Opcodes.ASM9, mv) {
                        @Override public void visitCode() {
                            super.visitCode();
                            for (String g : new String[] {"u", "v", "w"}) {
                                super.visitVarInsn(Opcodes.ALOAD, 1);
                                super.visitMethodInsn(Opcodes.INVOKEVIRTUAL, "ecg", "e", "()Ljd;", false);
                                super.visitMethodInsn(Opcodes.INVOKEVIRTUAL, "kh", g, "()I", false);
                            }
                            super.visitVarInsn(Opcodes.ALOAD, 1);
                            super.visitMethodInsn(Opcodes.INVOKEVIRTUAL, "ecg", "f", "()Leei;", false);
                            super.visitTypeInsn(Opcodes.CHECKCAST, "eeq");
                            super.visitFieldInsn(Opcodes.GETFIELD, "eeq", "c", "I");
                            super.visitMethodInsn(Opcodes.INVOKESTATIC, "CarveLog", "ore", "(IIII)V", false);
                        }
                    };
                }
            }, 0);
            return cw.toByteArray();
        } catch (Throwable t) {
            System.err.println("CARVEAGENT ore failed: " + t);
            return null;
        }
    }

    /**
     * OreVeinifier: log the three density-function values the sampler reads.
     * The synthetic lambda is `a(dyr toggle, dtc terrain, dzl factory,
     * dyr ridged, dyr gap, dyr$b ctx)`.
     */
    static byte[] instrumentVein(byte[] buf) {
        try {
            ClassReader cr = new ClassReader(buf);
            ClassWriter cw = writer(cr);
            cr.accept(new ClassVisitor(Opcodes.ASM9, cw) {
                @Override public MethodVisitor visitMethod(int acc, String mn, String md,
                                                           String sig, String[] ex) {
                    MethodVisitor mv = super.visitMethod(acc, mn, md, sig, ex);
                    if (!mn.equals("a") || !md.equals("(Ldyr;Ldtc;Ldzl;Ldyr;Ldyr;Ldyr$b;)Ldtc;")) {
                        return mv;
                    }
                    return new MethodVisitor(Opcodes.ASM9, mv) {
                        @Override public void visitCode() {
                            super.visitCode();
                            Label skip = new Label();
                            // ctx.a() / b() / c() are blockX / blockY / blockZ
                            super.visitVarInsn(Opcodes.ALOAD, 5);
                            super.visitMethodInsn(Opcodes.INVOKEINTERFACE, "dyr$b", "a", "()I", true);
                            super.visitVarInsn(Opcodes.ALOAD, 5);
                            super.visitMethodInsn(Opcodes.INVOKEINTERFACE, "dyr$b", "b", "()I", true);
                            super.visitVarInsn(Opcodes.ALOAD, 5);
                            super.visitMethodInsn(Opcodes.INVOKEINTERFACE, "dyr$b", "c", "()I", true);
                            super.visitMethodInsn(Opcodes.INVOKESTATIC, "CarveLog", "vwant", "(III)Z", false);
                            super.visitJumpInsn(Opcodes.IFEQ, skip);
                            // vdump(IIIDDD): x,y,z then toggle,ridged,gap
                            super.visitVarInsn(Opcodes.ALOAD, 5);
                            super.visitMethodInsn(Opcodes.INVOKEINTERFACE, "dyr$b", "a", "()I", true);
                            super.visitVarInsn(Opcodes.ALOAD, 5);
                            super.visitMethodInsn(Opcodes.INVOKEINTERFACE, "dyr$b", "b", "()I", true);
                            super.visitVarInsn(Opcodes.ALOAD, 5);
                            super.visitMethodInsn(Opcodes.INVOKEINTERFACE, "dyr$b", "c", "()I", true);
                            for (int slot : new int[] {0, 3, 4}) {
                                super.visitVarInsn(Opcodes.ALOAD, slot);
                                super.visitVarInsn(Opcodes.ALOAD, 5);
                                super.visitMethodInsn(Opcodes.INVOKEINTERFACE, "dyr", "a",
                                                      "(Ldyr$b;)D", true);
                            }
                            super.visitMethodInsn(Opcodes.INVOKESTATIC, "CarveLog", "vdump",
                                                  "(IIIDDD)V", false);
                            super.visitLabel(skip);
                        }
                    };
                }
            }, 0);
            return cw.toByteArray();
        } catch (Throwable t) {
            System.err.println("CARVEAGENT vein failed: " + t);
            return null;
        }
    }

    /** Logs the first draws after a reseed, so the server's stream is readable. */
    static MethodVisitor drawHook(MethodVisitor mv, String mn, String md) {
        final boolean isInt = mn.equals("f") && md.equals("()I");
        final boolean isFloat = mn.equals("i") && md.equals("()F");
        final boolean isLong = mn.equals("g") && md.equals("()J");
        final boolean isBound = mn.equals("a") && md.equals("(I)I");
        if (!isInt && !isFloat && !isLong && !isBound) return mv;
        final int slot = 40;
        final String label = isInt ? "nextInt" : isFloat ? "nextFloat" : isLong ? "nextLong" : "nextInt(b)";
        return new MethodVisitor(Opcodes.ASM9, mv) {
            @Override public void visitInsn(int op) {
                boolean isReturn = (isInt || isBound) ? op == Opcodes.IRETURN
                    : isFloat ? op == Opcodes.FRETURN : op == Opcodes.LRETURN;
                if (!isReturn) { super.visitInsn(op); return; }
                if (isInt || isBound) {
                    super.visitVarInsn(Opcodes.ISTORE, slot);
                } else if (isFloat) {
                    super.visitVarInsn(Opcodes.FSTORE, slot);
                } else {
                    super.visitVarInsn(Opcodes.LSTORE, slot);
                }
                super.visitLdcInsn(label);
                if (isBound) { super.visitVarInsn(Opcodes.ILOAD, 1); super.visitInsn(Opcodes.I2L); }
                else { super.visitLdcInsn(Long.valueOf(-1L)); }
                if (isInt || isBound) { super.visitVarInsn(Opcodes.ILOAD, slot); super.visitInsn(Opcodes.I2L); }
                else if (isFloat) { super.visitVarInsn(Opcodes.FLOAD, slot); super.visitInsn(Opcodes.F2L); }
                else { super.visitVarInsn(Opcodes.LLOAD, slot); }
                super.visitMethodInsn(Opcodes.INVOKESTATIC, "CarveLog", "rdraw",
                                      "(Ljava/lang/String;JJ)V", false);
                if (isLong) super.visitVarInsn(Opcodes.LLOAD, slot);
                else if (isFloat) super.visitVarInsn(Opcodes.FLOAD, slot);
                else super.visitVarInsn(Opcodes.ILOAD, slot);
                super.visitInsn(op);
            }
        };
    }

    static class Transformer implements ClassFileTransformer {
        @Override public byte[] transform(ClassLoader l, String name, Class<?> c,
                                          ProtectionDomain pd, byte[] buf) {
            if ("ebb".equals(name)) {
                byte[] b = instrumentCarve(buf);
                return instrumentTunnel(b != null ? b : buf);
            }
            if ("ebe".equals(name) && buf != null && hasTunnel(buf)) return instrumentTunnel(buf);
            if ("ebe".equals(name)) return instrumentCarveState(buf);
            if ("dzi".equals(name)) return instrumentVein(buf);
            if ("ecz".equals(name)) return instrumentOre(buf);
            if ("edq".equals(name)) return instrumentTree(buf);
            if ("duz".equals(name)) return instrumentDimension(buf);
            if ("deg".equals(name)) return instrumentBiomes(buf);
            if ("eii".equals(name)) return instrumentBiomeFilter(buf);
            if ("arb".equals(name)) return instrumentNoiseBiome(buf);
            if ("eis".equals(name)) return instrumentInSquare(buf);
            if ("dyn".equals(name)) return instrumentDynDraws(buf);
            if ("dzx".equals(name)) {
                byte[] b = instrumentSeeds(buf);
                return instrumentRSeed(b != null ? b : buf);
            }
            if ("dyz".equals(name)) return instrumentBits(buf);

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
                            @Override public void visitCode() {
                                super.visitCode();
                                // local 12 is set later; the target ChunkPos is arg 6 (local 7)
                                super.visitVarInsn(Opcodes.ALOAD, 7);
                                super.visitMethodInsn(Opcodes.INVOKEVIRTUAL, "duy", "f", "()Ldcd;", false);
                                super.visitInsn(Opcodes.DUP);
                                super.visitFieldInsn(Opcodes.GETFIELD, "dcd", "e", "I");
                                super.visitInsn(Opcodes.SWAP);
                                super.visitFieldInsn(Opcodes.GETFIELD, "dcd", "f", "I");
                                super.visitMethodInsn(Opcodes.INVOKESTATIC, "CarveLog", "target", "(II)V", false);
                            }
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
