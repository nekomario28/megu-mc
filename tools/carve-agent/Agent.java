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
        inst.addTransformer(new SculkTransformer());
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
                    boolean dbl = mn.equals("j") && md.equals("()D");
                    if (!bound && !plain && !flt && !dbl) return mv;
                    return new MethodVisitor(Opcodes.ASM9, mv) {
                        @Override public void visitInsn(int op) {
                            boolean ret = (bound || plain) ? op == Opcodes.IRETURN
                                : (flt ? op == Opcodes.FRETURN : op == Opcodes.DRETURN);
                            if (!ret) { super.visitInsn(op); return; }
                            if (flt || dbl) {
                                super.visitVarInsn(dbl ? Opcodes.DSTORE : Opcodes.FSTORE, 40);
                                super.visitLdcInsn(dbl ? "nextDouble" : "nextFloat");
                                super.visitVarInsn(dbl ? Opcodes.DLOAD : Opcodes.FLOAD, 40);
                                super.visitMethodInsn(Opcodes.INVOKESTATIC, "CarveLog",
                                                      dbl ? "rdrawD" : "rdrawF",
                                                      dbl ? "(Ljava/lang/String;D)V"
                                                          : "(Ljava/lang/String;F)V", false);
                                super.visitVarInsn(dbl ? Opcodes.DLOAD : Opcodes.FLOAD, 40);
                                super.visitInsn(op);
                                return;
                            }
                            super.visitVarInsn(Opcodes.ISTORE, 40);
                            // label carries the bound: nextInt(16) / nextInt()
                            if (bound) {
                                super.visitVarInsn(Opcodes.ILOAD, 1);
                                super.visitVarInsn(Opcodes.ILOAD, 40);
                                super.visitMethodInsn(Opcodes.INVOKESTATIC, "CarveLog", "rdrawI",
                                                      "(II)V", false);
                            } else {
                                super.visitVarInsn(Opcodes.ILOAD, 40);
                                super.visitMethodInsn(Opcodes.INVOKESTATIC, "CarveLog", "rdrawN",
                                                      "(I)V", false);
                            }
                            super.visitVarInsn(Opcodes.ILOAD, 40);
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
                            // decoration order: the chunk being decorated is arg 2
                            super.visitVarInsn(Opcodes.ALOAD, 2);
                            // ChunkAccess (`duy`) is an abstract class, not an
                            // interface: INVOKEINTERFACE here throws
                            // IncompatibleClassChangeError inside chunk generation.
                            super.visitMethodInsn(Opcodes.INVOKEVIRTUAL, "duy", "f", "()Ldcd;", false);
                            super.visitInsn(Opcodes.DUP);
                            super.visitFieldInsn(Opcodes.GETFIELD, "dcd", "e", "I");
                            super.visitInsn(Opcodes.SWAP);
                            super.visitFieldInsn(Opcodes.GETFIELD, "dcd", "f", "I");
                            super.visitMethodInsn(Opcodes.INVOKESTATIC, "CarveLog", "decoOrder",
                                                  "(II)V", false);
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

    /** Feature.place origins; the named logger applies its own chunk filter. */
    static byte[] instrumentFeatureOrigin(byte[] buf, String logger) {
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
                            super.visitMethodInsn(Opcodes.INVOKESTATIC, "CarveLog", logger, "(III)V", false);
                        }
                        @Override public void visitMethodInsn(int op, String owner, String name, String desc, boolean itf) {
                            boolean read = logger.equals("geode") && owner.equals("dds") && name.equals("a_") && desc.equals("(Ljd;)Ldtc;");
                            if (read) {
                                super.visitVarInsn(Opcodes.ASTORE, 200);
                                super.visitVarInsn(Opcodes.ALOAD, 200);
                            }
                            super.visitMethodInsn(op, owner, name, desc, itf);
                            if (read) {
                                super.visitVarInsn(Opcodes.ASTORE, 201);
                                for (String g : new String[] {"u", "v", "w"}) {
                                    super.visitVarInsn(Opcodes.ALOAD, 200);
                                    super.visitMethodInsn(Opcodes.INVOKEVIRTUAL, "kh", g, "()I", false);
                                }
                                super.visitVarInsn(Opcodes.ALOAD, 201);
                                super.visitMethodInsn(Opcodes.INVOKESTATIC, "dfy", "i", "(Ldtc;)I", false);
                                super.visitMethodInsn(Opcodes.INVOKESTATIC, "CarveLog", "geodeRead", "(IIII)V", false);
                                super.visitVarInsn(Opcodes.ALOAD, 201);
                            }
                        }
                    };
                }
            }, 0);
            return cw.toByteArray();
        } catch (Throwable t) {
            System.err.println("CARVEAGENT " + logger + " origin failed: " + t);
            return null;
        }
    }

    /** SeagrassFeature's actual block reads, preserving the operand stack. */
    static byte[] instrumentSeagrass(byte[] buf) {
        try {
            ClassReader cr = new ClassReader(buf);
            ClassWriter cw = writer(cr);
            cr.accept(new ClassVisitor(Opcodes.ASM9,cw) {
                @Override public MethodVisitor visitMethod(int acc,String mn,String md,String sig,String[] ex) {
                    MethodVisitor mv=super.visitMethod(acc,mn,md,sig,ex);
                    if(!mn.equals("a") || !md.equals("(Lecg;)Z")) return mv;
                    return new MethodVisitor(Opcodes.ASM9,mv) {
                        @Override public void visitMethodInsn(int op,String owner,String name,String desc,boolean itf) {
                            boolean read=owner.equals("dds") && name.equals("a_") && desc.equals("(Ljd;)Ldtc;");
                            if(read) {super.visitInsn(Opcodes.DUP);super.visitVarInsn(Opcodes.ASTORE,40);}
                            super.visitMethodInsn(op,owner,name,desc,itf);
                            if(read) {
                                super.visitInsn(Opcodes.DUP);super.visitVarInsn(Opcodes.ALOAD,40);super.visitInsn(Opcodes.SWAP);
                                super.visitMethodInsn(Opcodes.INVOKESTATIC,"CarveLog","seagrassRead","(Ljava/lang/Object;Ljava/lang/Object;)V",false);
                            }
                        }
                    };
                }
            },0);
            return cw.toByteArray();
        } catch(Throwable t) {System.err.println("CARVEAGENT seagrass failed: "+t);return null;}
    }

    /** OreFeature.place origins and canPlaceOre decisions (no changed game inputs). */
    static byte[] instrumentOre(byte[] buf) {
        try {
            ClassReader cr = new ClassReader(buf);
            ClassWriter cw = writer(cr);
            cr.accept(new ClassVisitor(Opcodes.ASM9, cw) {
                @Override public MethodVisitor visitMethod(int acc, String mn, String md,
                                                           String sig, String[] ex) {
                    MethodVisitor mv = super.visitMethod(acc, mn, md, sig, ex);
                    if (mn.equals("a") && md.equals("(Ldtc;Ljava/util/function/Function;Layw;Leeq;Leeq$a;Ljd$a;)Z")) {
                        return new MethodVisitor(Opcodes.ASM9, mv) {
                            @Override public void visitInsn(int opcode) {
                                if (opcode == Opcodes.IRETURN) {
                                    super.visitVarInsn(Opcodes.ISTORE, 8);
                                    for (String g : new String[] {"u", "v", "w"}) {
                                        super.visitVarInsn(Opcodes.ALOAD, 5);
                                        super.visitMethodInsn(Opcodes.INVOKEVIRTUAL, "kh", g, "()I", false);
                                    }
                                    super.visitVarInsn(Opcodes.ALOAD, 0);
                                    super.visitMethodInsn(Opcodes.INVOKESTATIC, "dfy", "i", "(Ldtc;)I", false);
                                    super.visitVarInsn(Opcodes.ILOAD, 8);
                                    super.visitMethodInsn(Opcodes.INVOKESTATIC, "CarveLog", "oreTest", "(IIIIZ)V", false);
                                    super.visitVarInsn(Opcodes.ILOAD, 8);
                                }
                                super.visitInsn(opcode);
                            }
                        };
                    }
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
                        @Override public void visitMethodInsn(int op, String owner, String name, String desc, boolean itf) {
                            super.visitMethodInsn(op, owner, name, desc, itf);
                            if (owner.equals("dds") && name.equals("a") && desc.equals("(Ldyy$a;II)I")) {
                                super.visitVarInsn(Opcodes.ISTORE, 40);
                                super.visitVarInsn(Opcodes.ILOAD, 27);
                                super.visitVarInsn(Opcodes.ILOAD, 28);
                                super.visitVarInsn(Opcodes.ILOAD, 40);
                                super.visitMethodInsn(Opcodes.INVOKESTATIC, "CarveLog", "oreHeight", "(III)V", false);
                                super.visitVarInsn(Opcodes.ILOAD, 40);
                            }
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

    /** WorldGenRegion writes to one named cell, including the writer chunk. */
    static byte[] instrumentWrites(byte[] buf,String callback) {
        ClassReader cr = new ClassReader(buf);
        ClassWriter cw = writer(cr);
        cr.accept(new ClassVisitor(Opcodes.ASM9, cw) {
            @Override public MethodVisitor visitMethod(int acc, String mn, String md, String sig, String[] ex) {
                MethodVisitor mv = super.visitMethod(acc, mn, md, sig, ex);
                if (!mn.equals("a") || !md.equals("(Ljd;Ldtc;II)Z")) return mv;
                return new MethodVisitor(Opcodes.ASM9, mv) {
                    @Override public void visitInsn(int opcode) {
                        if (opcode == Opcodes.IRETURN) {
                            super.visitVarInsn(Opcodes.ISTORE, 30);
                            for (String g : new String[] {"u", "v", "w"}) {
                                super.visitVarInsn(Opcodes.ALOAD, 1);
                                super.visitMethodInsn(Opcodes.INVOKEVIRTUAL, "kh", g, "()I", false);
                            }
                            super.visitVarInsn(Opcodes.ALOAD, 2);
                            super.visitMethodInsn(Opcodes.INVOKESTATIC, "dfy", "i", "(Ldtc;)I", false);
                            if(callback.equals("runtimeCell")) {
                                super.visitVarInsn(Opcodes.ALOAD,0);
                                super.visitMethodInsn(Opcodes.INVOKEVIRTUAL,"dcw","I_","()I",false);
                            }
                            super.visitVarInsn(Opcodes.ILOAD, 30);
                            super.visitMethodInsn(Opcodes.INVOKESTATIC, "CarveLog",callback,
                                callback.equals("runtimeCell") ? "(IIIIIZ)V" : "(IIIIZ)V",false);
                            super.visitVarInsn(Opcodes.ILOAD, 30);
                        }
                        super.visitInsn(opcode);
                    }
                };
            }
        }, ClassReader.EXPAND_FRAMES);
        return cw.toByteArray();
    }

    /** MushroomBlock.canSurvive: preserve and log the actual raw brightness. */
    static byte[] instrumentMushroomLight(byte[] buf) {
        ClassReader cr = new ClassReader(buf);
        ClassWriter cw = writer(cr);
        cr.accept(new ClassVisitor(Opcodes.ASM9, cw) {
            @Override public MethodVisitor visitMethod(int acc, String mn, String md, String sig, String[] ex) {
                MethodVisitor mv = super.visitMethod(acc, mn, md, sig, ex);
                if (!mn.equals("a") || !md.equals("(Ldtc;Ldcz;Ljd;)Z")) return mv;
                return new MethodVisitor(Opcodes.ASM9, mv) {
                    @Override public void visitMethodInsn(int op, String owner, String name, String desc, boolean itf) {
                        super.visitMethodInsn(op, owner, name, desc, itf);
                        if (owner.equals("dcz") && name.equals("b") && desc.equals("(Ljd;I)I")) {
                            super.visitVarInsn(Opcodes.ISTORE, 200);
                            for (String g : new String[] {"u", "v", "w"}) {
                                super.visitVarInsn(Opcodes.ALOAD, 3);
                                super.visitMethodInsn(Opcodes.INVOKEVIRTUAL, "kh", g, "()I", false);
                            }
                            super.visitVarInsn(Opcodes.ILOAD, 200);
                            super.visitMethodInsn(Opcodes.INVOKESTATIC, "CarveLog", "mushroomLight", "(IIII)V", false);
                            super.visitVarInsn(Opcodes.ILOAD, 200);
                        }
                    }
                };
            }
        }, ClassReader.EXPAND_FRAMES);
        return cw.toByteArray();
    }

    /** Accepted proto/container entries and LevelTicks drain order. */
    static byte[] instrumentTicks(byte[] buf, String owner) {
        ClassReader cr = new ClassReader(buf);
        ClassWriter cw = writer(cr);
        cr.accept(new ClassVisitor(Opcodes.ASM9, cw) {
            @Override public MethodVisitor visitMethod(int acc, String mn, String md, String sig, String[] ex) {
                MethodVisitor mv = super.visitMethod(acc, mn, md, sig, ex);
                boolean proto = owner.equals("eyr") && mn.equals("a") && md.equals("(Leys;)V");
                boolean chunk = owner.equals("eyo") && mn.equals("b") && md.equals("(Leyt;)V");
                boolean drain = owner.equals("eyq") && mn.equals("c") && md.equals("(Leyt;)V");
                boolean run = owner.equals("eyq") && mn.equals("a") && md.equals("(Ljava/util/function/BiConsumer;)V");
                if (!(proto || chunk || drain || run)) return mv;
                return new MethodVisitor(Opcodes.ASM9, mv) {
                    @Override public void visitCode() {
                        super.visitCode();
                        if (chunk || drain) {
                            super.visitLdcInsn(chunk ? "chunk-add" : "drain");
                            super.visitVarInsn(Opcodes.ALOAD, 1);
                            super.visitMethodInsn(Opcodes.INVOKESTATIC, "CarveLog", "scheduled", "(Ljava/lang/String;Ljava/lang/Object;)V", false);
                        }
                    }
                    @Override public void visitMethodInsn(int op, String target, String name, String desc, boolean itf) {
                        if (proto && op == Opcodes.INVOKEINTERFACE && target.equals("java/util/List")
                                && name.equals("add") && desc.equals("(Ljava/lang/Object;)Z")) {
                            super.visitInsn(Opcodes.DUP);
                            super.visitLdcInsn("proto-add");
                            super.visitInsn(Opcodes.SWAP);
                            super.visitMethodInsn(Opcodes.INVOKESTATIC, "CarveLog", "scheduled", "(Ljava/lang/String;Ljava/lang/Object;)V", false);
                        }
                        if (run && op == Opcodes.INVOKEINTERFACE && target.equals("java/util/function/BiConsumer")
                                && name.equals("accept") && desc.equals("(Ljava/lang/Object;Ljava/lang/Object;)V")) {
                            super.visitLdcInsn("run");
                            super.visitVarInsn(Opcodes.ALOAD, 2);
                            super.visitMethodInsn(Opcodes.INVOKESTATIC, "CarveLog", "scheduled", "(Ljava/lang/String;Ljava/lang/Object;)V", false);
                        }
                        super.visitMethodInsn(op, target, name, desc, itf);
                    }
                };
            }
        }, ClassReader.EXPAND_FRAMES);
        return cw.toByteArray();
    }

    static class Transformer implements ClassFileTransformer {
        @Override public byte[] transform(ClassLoader l, String name, Class<?> c,
                                          ProtectionDomain pd, byte[] buf) {
            if (System.getProperty("tickbox") != null && ("eyr".equals(name) || "eyo".equals(name) || "eyq".equals(name))) return instrumentTicks(buf, name);
            if ("ebb".equals(name)) {
                byte[] b = instrumentCarve(buf);
                return instrumentTunnel(b != null ? b : buf);
            }
            if ("ebe".equals(name) && buf != null && hasTunnel(buf)) return instrumentTunnel(buf);
            if ("ebe".equals(name)) return instrumentCarveState(buf);
            if ("dzi".equals(name)) return instrumentVein(buf);
            if ("ecz".equals(name)) return instrumentOre(buf);
            if ("edk".equals(name) && System.getProperty("seagrasschunk") != null) return instrumentSeagrass(buf);
            if ("arb".equals(name) && (System.getProperty("featurecell") != null || System.getProperty("featurebox") != null)) return instrumentWrites(buf,"featureCell");
            if ("dcw".equals(name) && (System.getProperty("runtimecell") != null || System.getProperty("runtimebox") != null)) return instrumentWrites(buf,"runtimeCell");
            if ("dla".equals(name) && System.getProperty("mushroomchunk") != null) return instrumentMushroomLight(buf);
            if ("edq".equals(name)) return instrumentFeatureOrigin(buf, "tree");
            if ("edc".equals(name) && System.getProperty("patchchunk") != null) return instrumentFeatureOrigin(buf, "patch");
            if ("eck".equals(name) && System.getProperty("geodechunk") != null) return instrumentFeatureOrigin(buf, "geode");
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

/**
 * `SculkSpreader$ChargeCursor.update` (`dmu$a.a(dcx,jd,ayw,dmu,boolean)`): log the cursor's
 * position, charge, delays and facing mask at entry.
 *
 * Entry is the comparable point: the same fields are what the port's `SPC_CURSOR_TRACE`
 * prints before its own update body runs, and a cursor's trajectory is then two index-aligned
 * lists rather than two cell sets. The hook only reads fields and calls `jd.u()/v()/w()`, so
 * it draws no random and cannot change the spread's decisions.
 */
class SculkTransformer implements ClassFileTransformer {
    @Override public byte[] transform(ClassLoader loader, String name, Class<?> beingRedefined,
            ProtectionDomain domain, byte[] buf) {
        if ("dmu".equals(name)) {
            try {
                ClassReader cr = new ClassReader(buf);
                ClassWriter cw = new ClassWriter(cr, ClassWriter.COMPUTE_FRAMES | ClassWriter.COMPUTE_MAXS) {
                    @Override protected String getCommonSuperClass(String a, String b) {
                        return "java/lang/Object";
                    }
                };
                cr.accept(new ClassVisitor(Opcodes.ASM9, cw) {
                    @Override public MethodVisitor visitMethod(int acc, String mn, String md,
                            String sig, String[] ex) {
                        MethodVisitor mv = super.visitMethod(acc, mn, md, sig, ex);
                        if (!mn.equals("a") || !md.equals("(Ldcx;Ljd;Layw;Z)V")) return mv;
                        return new MethodVisitor(Opcodes.ASM9, mv) {
                            @Override public void visitCode() {
                                super.visitCode();
                                super.visitMethodInsn(Opcodes.INVOKESTATIC, "SculkLog", "round", "()V", false);
                            }
                        };
                    }
                }, 0);
                System.err.println("SCULKAGENT updateCursors hook installed");
                return cw.toByteArray();
            } catch (Throwable t) {
                System.err.println("SCULKAGENT updateCursors transform failed: " + t);
                return null;
            }
        }
        if ("dmv".equals(name) || "dmq".equals(name)) {
            try {
                ClassReader cr = new ClassReader(buf);
                ClassWriter cw = new ClassWriter(cr, ClassWriter.COMPUTE_FRAMES | ClassWriter.COMPUTE_MAXS) {
                    @Override protected String getCommonSuperClass(String a, String b) {
                        return "java/lang/Object";
                    }
                };
                final String owner = name;
                cr.accept(new ClassVisitor(Opcodes.ASM9, cw) {
                    @Override public MethodVisitor visitMethod(int acc, String mn, String md,
                            String sig, String[] ex) {
                        MethodVisitor mv = super.visitMethod(acc, mn, md, sig, ex);
                        String what = null;
                        if (owner.equals("dmv") && mn.equals("a")
                                && md.equals("(Ldmu$a;Ldcx;Ljd;Layw;Ldmu;Z)I")) {
                            what = "one-charge-vein";
                        } else if (owner.equals("dmv") && mn.equals("a")
                                && md.equals("(Ldmu;Ldcx;Ljd;Layw;)Z")) {
                            what = "convert";
                        } else if (owner.equals("dmv") && mn.equals("a")
                                && md.equals("(Ldcx;Ljd;Ldtc;Ljava/util/Collection;)Z")) {
                            what = "attempt-vein";
                        } else if (owner.equals("dmq") && mn.equals("a")
                                && md.equals("(Ldmu$a;Ldcx;Ljd;Layw;Ldmu;Z)I")) {
                            what = "one-charge-sculk";
                        }
                        if (what == null) return mv;
                        final String tag = what;
                        return new MethodVisitor(Opcodes.ASM9, mv) {
                            @Override public void visitCode() {
                                super.visitCode();
                                super.visitLdcInsn(tag);
                                super.visitMethodInsn(Opcodes.INVOKESTATIC, "SculkLog", "mark",
                                        "(Ljava/lang/String;)V", false);
                            }
                        };
                    }
                }, 0);
                System.err.println("SCULKAGENT " + name + " markers installed");
                return cw.toByteArray();
            } catch (Throwable t) {
                System.err.println("SCULKAGENT " + name + " marker transform failed: " + t);
                return null;
            }
        }
        if ("dkz".equals(name)) {
            try {
                ClassReader cr = new ClassReader(buf);
                ClassWriter cw = new ClassWriter(cr, ClassWriter.COMPUTE_FRAMES | ClassWriter.COMPUTE_MAXS) {
                    @Override protected String getCommonSuperClass(String a, String b) {
                        return "java/lang/Object";
                    }
                };
                cr.accept(new ClassVisitor(Opcodes.ASM9, cw) {
                    @Override public MethodVisitor visitMethod(int acc, String mn, String md,
                            String sig, String[] ex) {
                        MethodVisitor mv = super.visitMethod(acc, mn, md, sig, ex);
                        if (!mn.equals("a") || !md.equals("(Ldtc;Ldcx;Ljd;Z)J")) return mv;
                        return new MethodVisitor(Opcodes.ASM9, mv) {
                            @Override public void visitCode() {
                                super.visitCode();
                                // `this.b.a().length`: the config's own spread-type array, read in
                                // the instrumented class because the implementation is
                                // package-private and a bootstrap logger cannot reflect into it.
                                super.visitVarInsn(Opcodes.ALOAD, 0);
                                super.visitFieldInsn(Opcodes.GETFIELD, "dkz", "b", "Ldkz$b;");
                                super.visitMethodInsn(Opcodes.INVOKEINTERFACE, "dkz$b", "a",
                                        "()[Ldkz$e;", true);
                                super.visitInsn(Opcodes.ARRAYLENGTH);
                                super.visitMethodInsn(Opcodes.INVOKESTATIC, "SculkLog", "spreadall",
                                        "(I)V", false);
                            }
                        };
                    }
                }, 0);
                System.err.println("SCULKAGENT dkz spreadAll hook installed");
                return cw.toByteArray();
            } catch (Throwable t) {
                System.err.println("SCULKAGENT dkz spreadAll hook failed: " + t);
                return null;
            }
        }
        if ("ad".equals(name)) {
            // `Util.shuffledCopy(ObjectArrayList|Stream, RandomSource)`: log the list size and the
            // caller class at entry, so a shuffle's bound can be traced back to the list that owns
            // it (a `nextInt(49)` is a 50-element list, and the caller names it).
            try {
                ClassReader cr = new ClassReader(buf);
                ClassWriter cw = new ClassWriter(cr, ClassWriter.COMPUTE_FRAMES | ClassWriter.COMPUTE_MAXS) {
                    @Override protected String getCommonSuperClass(String a, String b) {
                        return "java/lang/Object";
                    }
                };
                cr.accept(new ClassVisitor(Opcodes.ASM9, cw) {
                    @Override public MethodVisitor visitMethod(int acc, String mn, String md,
                            String sig, String[] ex) {
                        MethodVisitor mv = super.visitMethod(acc, mn, md, sig, ex);
                        boolean list = mn.equals("a")
                                && md.equals("(Lit/unimi/dsi/fastutil/objects/ObjectArrayList;Layw;)Ljava/util/List;");
                        boolean stream = mn.equals("a")
                                && md.equals("(Ljava/util/stream/Stream;Layw;)Ljava/util/List;");
                        if (!list && !stream) return mv;
                        return new MethodVisitor(Opcodes.ASM9, mv) {
                            @Override public void visitCode() {
                                super.visitCode();
                                if (list) {
                                    super.visitVarInsn(Opcodes.ALOAD, 0);
                                    super.visitMethodInsn(Opcodes.INVOKEVIRTUAL,
                                            "it/unimi/dsi/fastutil/objects/ObjectArrayList", "size", "()I", false);
                                } else {
                                    super.visitInsn(Opcodes.ICONST_M1);
                                }
                                super.visitTypeInsn(Opcodes.NEW, "java/lang/Throwable");
                                super.visitInsn(Opcodes.DUP);
                                super.visitMethodInsn(Opcodes.INVOKESPECIAL, "java/lang/Throwable",
                                        "<init>", "()V", false);
                                super.visitMethodInsn(Opcodes.INVOKEVIRTUAL, "java/lang/Throwable",
                                        "getStackTrace", "()[Ljava/lang/StackTraceElement;", false);
                                super.visitInsn(Opcodes.ICONST_1);
                                super.visitInsn(Opcodes.AALOAD);
                                super.visitMethodInsn(Opcodes.INVOKEVIRTUAL, "java/lang/StackTraceElement",
                                        "getClassName", "()Ljava/lang/String;", false);
                                super.visitMethodInsn(Opcodes.INVOKESTATIC, "SculkLog", "shuffle",
                                        "(ILjava/lang/String;)V", false);
                            }
                        };
                    }
                }, 0);
                System.err.println("SCULKAGENT ad shuffledCopy hook installed");
                return cw.toByteArray();
            } catch (Throwable t) {
                System.err.println("SCULKAGENT ad hook failed: " + t);
                return null;
            }
        }
        if ("eiv".equals(name)) {
            // `eiv` is `PlacedFeature` (`net.minecraft.world.level.levelgen.placement.PlacedFeature`).
            // Its `b(Ldds;Lduz;Layw;Ljd;)Z` is `placeWithBiomeCheck` - the placement's outermost
            // entry, so the *position draws* the placement modifiers make happen inside the armed
            // window. Arming on `ConfiguredFeature.place` (`ebq`) cannot see them: the modifiers
            // run before that method is reached (measured: those windows held no position draw at
            // all - SPLACE -> SRNG -> SPATCH with nothing between).
            try {
                ClassReader cr = new ClassReader(buf);
                ClassWriter cw = new ClassWriter(cr, ClassWriter.COMPUTE_FRAMES | ClassWriter.COMPUTE_MAXS) {
                    @Override protected String getCommonSuperClass(String a, String b) {
                        return "java/lang/Object";
                    }
                };
                cr.accept(new ClassVisitor(Opcodes.ASM9, cw) {
                    @Override public MethodVisitor visitMethod(int acc, String mn, String md,
                            String sig, String[] ex) {
                        MethodVisitor mv = super.visitMethod(acc, mn, md, sig, ex);
                        if (!mn.equals("b") || !md.equals("(Ldds;Lduz;Layw;Ljd;)Z")) return mv;
                        return new MethodVisitor(Opcodes.ASM9, mv) {
                            @Override public void visitInsn(int op) {
                                if (op == Opcodes.IRETURN) {
                                    super.visitMethodInsn(Opcodes.INVOKESTATIC, "SculkLog",
                                            "windowEnd", "()V", false);
                                    super.visitMethodInsn(Opcodes.INVOKESTATIC, "SculkLog",
                                            "disarm", "()V", false);
                                }
                                super.visitInsn(op);
                            }

                            @Override public void visitCode() {
                                super.visitCode();
                                super.visitMethodInsn(Opcodes.INVOKESTATIC, "SculkLog",
                                        "armPlacement", "()V", false);
                                super.visitMethodInsn(Opcodes.INVOKESTATIC, "SculkLog",
                                        "windowStart", "()V", false);
                            }
                        };
                    }
                }, 0);
                System.err.println("SCULKAGENT eiv placement-window hook installed");
                return cw.toByteArray();
            } catch (Throwable t) {
                System.err.println("SCULKAGENT eiv hook failed: " + t);
                return null;
            }
        }
        if ("dyn".equals(name) || "dzz".equals(name) || "dzx".equals(name) || "dyz".equals(name)
                || "dzr".equals(name) || "dzo".equals(name)) {
            // The placement RNG's draws, logged only while a sculk patch is being placed (the
            // `armed` flag is set by the `ebq` hook). `(value, kind)` argument order so the value
            // can be DUP-ed before the call and survive the return.
            String[][] ints = {
                // class, method, descriptor, kind
                // `dyn` (BitRandomSource) is the mixin interface the legacy sources inherit their
                // bounded/derived draws from - the placement's `nextInt(bound)` is here, which is
                // why hooking only the concrete classes recorded `next(bits)` (the raw generator,
                // full-range values) and missed the positions entirely.
                {"dyn", "a", "(I)I", "nextInt(bound)@dyn"},
                {"dyn", "f", "()I", "nextInt()@dyn"},
                {"dyn", "h", "()Z", "nextBoolean@dyn"},
                {"dzr", "c", "(I)I", "next(bits)@dzr"},
                {"dzo", "c", "(I)I", "next(bits)@dzo"},
                {"dzz", "a", "(I)I", "nextInt(bound)@dzz"},
                {"dzz", "f", "()I", "nextInt()@dzz"},
                {"dzx", "c", "(I)I", "next(bits)@dzx"},
                {"dzx", "l", "()I", "nextInt()@dzx"},
                {"dyz", "c", "(I)I", "next(bits)@dyz"},
            };
            String[][] longs = {
                {"dzz", "g", "()J", "nextLong()@dzz"},
                {"dyn", "g", "()J", "nextLong()@dyn"},
            };
            String[][] doubles = {
                {"dyn", "j", "()D", "nextDouble()@dyn"},
                {"dzz", "j", "()D", "nextDouble()@dzz"}, {"dzz", "k", "()D", "gaussian@dzz"},
                {"dyz", "k", "()D", "nextDouble()@dyz"},
                {"dzr", "k", "()D", "nextDouble()@dzr"},
                {"dzo", "k", "()D", "nextDouble()@dzo"}};
            String[][] floats = {
                {"dzz", "i", "()F", "nextFloat()@dzz"},
                {"dyn", "i", "()F", "nextFloat()@dyn"},
            };
            try {
                ClassReader cr = new ClassReader(buf);
                final int[] hooked = {0};
                ClassWriter cw = new ClassWriter(cr, ClassWriter.COMPUTE_FRAMES | ClassWriter.COMPUTE_MAXS) {
                    @Override protected String getCommonSuperClass(String a, String b) {
                        return "java/lang/Object";
                    }
                };
                cr.accept(new ClassVisitor(Opcodes.ASM9, cw) {
                    @Override public MethodVisitor visitMethod(int acc, String mn, String md,
                            String sig, String[] ex) {
                        MethodVisitor mv = super.visitMethod(acc, mn, md, sig, ex);
                        String kind = null, logger = null, desc = null;
                        int opcode = 0, localOp = 0;
                        boolean bounded = false;
                        for (String[] row : ints) {
                            if (row[0].equals(name) && row[1].equals(mn) && row[2].equals(md)) {
                                kind = row[3];
                                // `(I)I` is `nextInt(bound)`: log the bound too, so the value can
                                // be matched against the port's `nextInt(<bound>)=<value>`.
                                bounded = md.equals("(I)I") && !row[3].contains("bits");
                                logger = bounded ? "drawIBound" : "drawI";
                                desc = bounded ? "(IILjava/lang/String;)V" : "(ILjava/lang/String;)V";
                                opcode = Opcodes.IRETURN;
                                localOp = Opcodes.ILOAD;
                            }
                        }
                        for (String[] row : longs) {
                            if (row[0].equals(name) && row[1].equals(mn) && row[2].equals(md)) {
                                kind = row[3];
                                logger = "drawJ";
                                desc = "(JLjava/lang/String;)V";
                                opcode = Opcodes.LRETURN;
                            }
                        }
                        for (String[] row : doubles) {
                            if (row[0].equals(name) && row[1].equals(mn) && row[2].equals(md)) {
                                kind = row[3];
                                logger = "drawD";
                                desc = "(DLjava/lang/String;)V";
                                opcode = Opcodes.DRETURN;
                            }
                        }
                        for (String[] row : floats) {
                            if (row[0].equals(name) && row[1].equals(mn) && row[2].equals(md)) {
                                kind = row[3];
                                logger = "drawF";
                                desc = "(FLjava/lang/String;)V";
                                opcode = Opcodes.FRETURN;
                            }
                        }
                        if (kind == null) return mv;
                        hooked[0]++;
                        final String k = kind, lg = logger, dsc = desc;
                        final int ret = opcode;
                        final boolean bd = bounded;
                        return new MethodVisitor(Opcodes.ASM9, mv) {
                            @Override public void visitInsn(int op) {
                                if (op == ret) {
                                    // a long/double needs DUP2 (DUP only covers a category-1
                                    // value, measured: `dyz.k()D @7: dup` failed verification)
                                    super.visitInsn(ret == Opcodes.LRETURN || ret == Opcodes.DRETURN
                                            ? Opcodes.DUP2 : Opcodes.DUP);
                                    if (bd) {
                                        super.visitVarInsn(Opcodes.ILOAD, 1);
                                    }
                                    super.visitLdcInsn(k);
                                    super.visitMethodInsn(Opcodes.INVOKESTATIC, "SculkLog", lg, dsc,
                                            false);
                                }
                                super.visitInsn(op);
                            }
                        };
                    }
                }, 0);
                System.err.println("SCULKAGENT rng draw hook installed for " + name
                        + (hooked[0] == 0 ? " (no method matched)" : " (" + hooked[0] + " methods)"));
                return cw.toByteArray();
            } catch (Throwable t) {
                System.err.println("SCULKAGENT rng draw hook failed for " + name + ": " + t);
                return null;
            }
        }
        if ("duz".equals(name)) {
            try {
                ClassReader cr = new ClassReader(buf);
                ClassWriter cw = new ClassWriter(cr, ClassWriter.COMPUTE_FRAMES | ClassWriter.COMPUTE_MAXS) {
                    @Override protected String getCommonSuperClass(String a, String b) {
                        return "java/lang/Object";
                    }
                };
                cr.accept(new ClassVisitor(Opcodes.ASM9, cw) {
                    @Override public MethodVisitor visitMethod(int acc, String mn, String md,
                            String sig, String[] ex) {
                        MethodVisitor mv = super.visitMethod(acc, mn, md, sig, ex);
                        // `ChunkGenerator.applyBiomeDecoration(WorldGenLevel, ChunkAccess,
                        // StructureManager)`: log the decorated chunk, so the placement lines of
                        // a pass can be told apart from the 8 neighbouring passes that log the
                        // same biome sources.
                        if (!mn.equals("a") || !md.equals("(Ldds;Lduy;Lddq;)V")) return mv;
                        return new MethodVisitor(Opcodes.ASM9, mv) {
                            @Override public void visitCode() {
                                super.visitCode();
                                super.visitVarInsn(Opcodes.ALOAD, 2);
                                super.visitMethodInsn(Opcodes.INVOKEVIRTUAL, "duy", "f", "()Ldcd;",
                                        false);
                                super.visitInsn(Opcodes.DUP);
                                super.visitFieldInsn(Opcodes.GETFIELD, "dcd", "e", "I");
                                super.visitInsn(Opcodes.SWAP);
                                super.visitFieldInsn(Opcodes.GETFIELD, "dcd", "f", "I");
                                super.visitMethodInsn(Opcodes.INVOKESTATIC, "SculkLog", "pass",
                                        "(II)V", false);
                            }
                        };
                    }
                }, 0);
                System.err.println("SCULKAGENT duz decoration-pass hook installed");
                return cw.toByteArray();
            } catch (Throwable t) {
                System.err.println("SCULKAGENT duz decoration-pass hook failed: " + t);
                return null;
            }
        }
        if ("ebq".equals(name)) {
            try {
                ClassReader cr = new ClassReader(buf);
                ClassWriter cw = new ClassWriter(cr, ClassWriter.COMPUTE_FRAMES | ClassWriter.COMPUTE_MAXS) {
                    @Override protected String getCommonSuperClass(String a, String b) {
                        return "java/lang/Object";
                    }
                };
                cr.accept(new ClassVisitor(Opcodes.ASM9, cw) {
                    @Override public MethodVisitor visitMethod(int acc, String mn, String md,
                            String sig, String[] ex) {
                        MethodVisitor mv = super.visitMethod(acc, mn, md, sig, ex);
                        // `PlacedFeature.placeWithBiomeCheck(ServerLevelAccessor, ChunkGenerator,
                        // RandomSource, BlockPos)`: log the feature class and the chunk it is
                        // attempted in, at entry, one line per attempt.
                        if (!mn.equals("a") || !md.equals("(Ldds;Lduz;Layw;Ljd;)Z")) return mv;
                        return new MethodVisitor(Opcodes.ASM9, mv) {
                            @Override public void visitInsn(int op) {
                                // the method returns boolean, so its return opcode is IRETURN
                                if (op == Opcodes.IRETURN) {
                                    super.visitMethodInsn(Opcodes.INVOKESTATIC, "SculkLog",
                                            "disarm", "()V", false);
                                }
                                super.visitInsn(op);
                            }

                            @Override public void visitCode() {
                                super.visitCode();
                                super.visitVarInsn(Opcodes.ALOAD, 0);
                                super.visitMethodInsn(Opcodes.INVOKEVIRTUAL, "ebq", "b", "()Lece;",
                                        false);
                                super.visitMethodInsn(Opcodes.INVOKEVIRTUAL, "java/lang/Object",
                                        "getClass", "()Ljava/lang/Class;", false);
                                super.visitMethodInsn(Opcodes.INVOKEVIRTUAL, "java/lang/Class",
                                        "getName", "()Ljava/lang/String;", false);
                                super.visitVarInsn(Opcodes.ALOAD, 4);
                                super.visitMethodInsn(Opcodes.INVOKEVIRTUAL, "jd", "u", "()I", false);
                                super.visitInsn(Opcodes.ICONST_4);
                                super.visitInsn(Opcodes.ISHR);
                                super.visitVarInsn(Opcodes.ALOAD, 4);
                                super.visitMethodInsn(Opcodes.INVOKEVIRTUAL, "jd", "w", "()I", false);
                                super.visitInsn(Opcodes.ICONST_4);
                                super.visitInsn(Opcodes.ISHR);
                                super.visitMethodInsn(Opcodes.INVOKESTATIC, "SculkLog", "place",
                                        "(Ljava/lang/String;II)V", false);
                                // the RNG instance the placement was handed, so the class to hook
                                // is a measurement rather than a guess
                                super.visitVarInsn(Opcodes.ALOAD, 3);
                                super.visitMethodInsn(Opcodes.INVOKEVIRTUAL, "java/lang/Object",
                                        "getClass", "()Ljava/lang/Class;", false);
                                super.visitMethodInsn(Opcodes.INVOKEVIRTUAL, "java/lang/Class",
                                        "getName", "()Ljava/lang/String;", false);
                                super.visitMethodInsn(Opcodes.INVOKESTATIC, "SculkLog", "rngClass",
                                        "(Ljava/lang/String;)V", false);
                            }
                        };
                    }
                }, 0);
                System.err.println("SCULKAGENT ebq placement hook installed");
                return cw.toByteArray();
            } catch (Throwable t) {
                System.err.println("SCULKAGENT ebq placement hook failed: " + t);
                return null;
            }
        }
        if ("edi".equals(name)) {
            try {
                ClassReader cr = new ClassReader(buf);
                ClassWriter cw = new ClassWriter(cr, ClassWriter.COMPUTE_FRAMES | ClassWriter.COMPUTE_MAXS) {
                    @Override protected String getCommonSuperClass(String a, String b) {
                        return "java/lang/Object";
                    }
                };
                cr.accept(new ClassVisitor(Opcodes.ASM9, cw) {
                    @Override public MethodVisitor visitMethod(int acc, String mn, String md,
                            String sig, String[] ex) {
                        MethodVisitor mv = super.visitMethod(acc, mn, md, sig, ex);
                        // `SculkPatchFeature.place(FeaturePlaceContext)`: log the origin
                        if (!mn.equals("a") || !md.equals("(Lecg;)Z")) return mv;
                        return new MethodVisitor(Opcodes.ASM9, mv) {
                            @Override public void visitCode() {
                                super.visitCode();
                                super.visitVarInsn(Opcodes.ALOAD, 1);
                                super.visitMethodInsn(Opcodes.INVOKEVIRTUAL, "ecg", "e", "()Ljd;", false);
                                super.visitInsn(Opcodes.DUP);
                                super.visitMethodInsn(Opcodes.INVOKEVIRTUAL, "jd", "u", "()I", false);
                                super.visitInsn(Opcodes.SWAP);
                                super.visitInsn(Opcodes.DUP);
                                super.visitMethodInsn(Opcodes.INVOKEVIRTUAL, "jd", "v", "()I", false);
                                super.visitInsn(Opcodes.SWAP);
                                super.visitMethodInsn(Opcodes.INVOKEVIRTUAL, "jd", "w", "()I", false);
                                super.visitMethodInsn(Opcodes.INVOKESTATIC, "SculkLog", "patch",
                                        "(III)V", false);
                            }
                        };
                    }
                }, 0);
                System.err.println("SCULKAGENT edi patch hook installed");
                return cw.toByteArray();
            } catch (Throwable t) {
                System.err.println("SCULKAGENT edi patch hook failed: " + t);
                return null;
            }
        }
        if (!"dmu$a".equals(name)) return null;
        try {
            ClassReader cr = new ClassReader(buf);
            ClassWriter cw = new ClassWriter(cr, ClassWriter.COMPUTE_FRAMES | ClassWriter.COMPUTE_MAXS) {
                @Override protected String getCommonSuperClass(String a, String b) {
                    return "java/lang/Object";
                }
            };
            cr.accept(new ClassVisitor(Opcodes.ASM9, cw) {
                @Override public MethodVisitor visitMethod(int acc, String mn, String md,
                        String sig, String[] ex) {
                    MethodVisitor mv = super.visitMethod(acc, mn, md, sig, ex);
                    if (!mn.equals("a") || !md.equals("(Ldcx;Ljd;Layw;Ldmu;Z)V")) return mv;
                    return new MethodVisitor(Opcodes.ASM9, mv) {
                        @Override public void visitCode() {
                            super.visitCode();
                            for (String getter : new String[]{"u", "v", "w"}) {
                                super.visitVarInsn(Opcodes.ALOAD, 0);
                                super.visitFieldInsn(Opcodes.GETFIELD, "dmu$a", "d", "Ljd;");
                                super.visitMethodInsn(Opcodes.INVOKEVIRTUAL, "jd", getter, "()I", false);
                            }
                            super.visitVarInsn(Opcodes.ALOAD, 0);
                            super.visitFieldInsn(Opcodes.GETFIELD, "dmu$a", "e", "I");
                            super.visitVarInsn(Opcodes.ALOAD, 0);
                            super.visitFieldInsn(Opcodes.GETFIELD, "dmu$a", "g", "I");
                            super.visitVarInsn(Opcodes.ALOAD, 0);
                            super.visitFieldInsn(Opcodes.GETFIELD, "dmu$a", "f", "I");
                            super.visitVarInsn(Opcodes.ALOAD, 0);
                            super.visitFieldInsn(Opcodes.GETFIELD, "dmu$a", "h", "Ljava/util/Set;");
                            super.visitMethodInsn(Opcodes.INVOKESTATIC, "SculkLog", "cursor",
                                    "(IIIIIILjava/util/Set;)V", false);
                        }
                    };
                }
            }, 0);
            System.err.println("SCULKAGENT cursor hook installed");
            return cw.toByteArray();
        } catch (Throwable t) {
            System.err.println("SCULKAGENT transform failed: " + t);
            return null;
        }
    }
}
