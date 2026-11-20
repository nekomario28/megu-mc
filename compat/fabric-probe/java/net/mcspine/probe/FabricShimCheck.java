package net.mcspine.probe;

import java.io.InputStream;
import java.util.ArrayList;
import java.util.List;
import java.util.Arrays;
import java.util.jar.JarEntry;
import java.util.jar.JarFile;
import org.objectweb.asm.ClassReader;
import org.objectweb.asm.ConstantDynamic;
import org.objectweb.asm.Handle;
import org.objectweb.asm.Opcodes;
import org.objectweb.asm.Type;
import org.objectweb.asm.signature.SignatureReader;
import org.objectweb.asm.signature.SignatureVisitor;
import org.objectweb.asm.tree.AbstractInsnNode;
import org.objectweb.asm.tree.AnnotationNode;
import org.objectweb.asm.tree.ClassNode;
import org.objectweb.asm.tree.FieldInsnNode;
import org.objectweb.asm.tree.FieldNode;
import org.objectweb.asm.tree.FrameNode;
import org.objectweb.asm.tree.InvokeDynamicInsnNode;
import org.objectweb.asm.tree.LdcInsnNode;
import org.objectweb.asm.tree.MethodInsnNode;
import org.objectweb.asm.tree.MethodNode;
import org.objectweb.asm.tree.MultiANewArrayInsnNode;
import org.objectweb.asm.tree.RecordComponentNode;
import org.objectweb.asm.tree.TypeInsnNode;

/** Emits deterministic JSON class references and annotations for the Python policy layer. */
public final class FabricShimCheck {
    private static String quote(String value) {
        var out = new StringBuilder("\"");
        for (char c : value.toCharArray()) {
            if (c == '"' || c == '\\') out.append('\\').append(c);
            else if (c < 32) out.append(String.format("\\u%04x", (int) c));
            else out.append(c);
        }
        return out.append('"').toString();
    }

    private static void ref(String name, List<String> refs) {
        if (name.startsWith("[")) type(name, refs);
        else refs.add(name);
    }

    private static void type(String descriptor, List<String> refs) {
        if (descriptor == null) return;
        try {
            Type t = Type.getType(descriptor);
            if (t.getSort() == Type.METHOD) {
                type(t.getReturnType().getDescriptor(), refs);
                for (Type arg : t.getArgumentTypes()) type(arg.getDescriptor(), refs);
            } else if (t.getSort() == Type.ARRAY) type(t.getElementType().getDescriptor(), refs);
            else if (t.getSort() == Type.OBJECT) refs.add(t.getInternalName());
        } catch (IllegalArgumentException ignored) { }
    }

    private static void annotation(AnnotationNode node, List<String> annotations, List<String> refs) {
        annotations.add(node.desc + " " + stable(node.values));
        type(node.desc, refs);
        if (node.values == null) return;
        for (int i = 0; i < node.values.size(); i += 2) {
            String key = String.valueOf(node.values.get(i));
            Object value = node.values.get(i + 1);
            value(value, annotations, refs);
            if (node.desc.equals("Lorg/spongepowered/asm/mixin/Mixin;") && key.equals("targets")) {
                if (value instanceof String name) refs.add(name.replace('.', '/'));
                else if (value instanceof List<?> names) for (Object name : names)
                    if (name instanceof String s) refs.add(s.replace('.', '/'));
            }
        }
    }

    private static String stable(Object value) {
        if (value == null) return "null";
        if (value instanceof String[] pair) return Arrays.toString(pair);
        if (value instanceof AnnotationNode a) return a.desc + " " + stable(a.values);
        if (value instanceof List<?> list) {
            List<String> parts = new ArrayList<>();
            for (Object item : list) parts.add(stable(item));
            return parts.toString();
        }
        return String.valueOf(value);
    }

    private static void value(Object value, List<String> annotations, List<String> refs) {
        if (value instanceof Type t) type(t.getDescriptor(), refs);
        else if (value instanceof AnnotationNode a) annotation(a, annotations, refs);
        else if (value instanceof List<?> list) for (Object item : list) value(item, annotations, refs);
        else if (value instanceof String[] pair && pair.length == 2) annotations.add(pair[0] + "=" + pair[1]);
    }

    private static void addAnnotations(List<AnnotationNode> nodes, List<String> annotations, List<String> refs) {
        if (nodes != null) for (AnnotationNode node : nodes) annotation(node, annotations, refs);
    }

    private static boolean client(List<AnnotationNode> nodes) {
        return nodes != null && nodes.stream().anyMatch(a ->
            a.desc.equals("Lnet/fabricmc/api/Environment;") && a.values != null
                && a.values.stream().anyMatch(v -> v instanceof String[] pair
                    && Arrays.equals(pair, new String[]{"Lnet/fabricmc/api/EnvType;", "CLIENT"})));
    }

    private static SignatureVisitor signatureTypes(List<String> refs) {
        return new SignatureVisitor(Opcodes.ASM9) {
            private String owner;

            @Override public void visitClassType(String name) {
                owner = name;
                refs.add(owner);
            }
            @Override public void visitInnerClassType(String name) {
                visitClassType(owner + "$" + name);
            }
            @Override public SignatureVisitor visitTypeArgument(char wildcard) {
                return signatureTypes(refs);
            }
        };
    }

    private static void signature(String sig, boolean field, List<String> refs) {
        if (sig == null) return;
        var reader = new SignatureReader(sig);
        if (field) reader.acceptType(signatureTypes(refs));
        else reader.accept(signatureTypes(refs));
    }

    private static void member(boolean field, String owner, String name, String desc,
                               List<String> annotations, List<String> refs) {
        ref(owner, refs);
        type(desc, refs);
        annotations.add((field ? "FIELD|" : "MEMBER|") + owner + "|" + name + "|" + desc);
    }

    private static void constant(Object value, List<String> annotations, List<String> refs) {
        if (value instanceof Type t) type(t.getDescriptor(), refs);
        else if (value instanceof Handle h) {
            member(h.getTag() <= Opcodes.H_PUTSTATIC, h.getOwner(), h.getName(), h.getDesc(), annotations, refs);
        } else if (value instanceof ConstantDynamic d) {
            type(d.getDescriptor(), refs);
            constant(d.getBootstrapMethod(), annotations, refs);
            for (int i = 0; i < d.getBootstrapMethodArgumentCount(); i++) {
                constant(d.getBootstrapMethodArgument(i), annotations, refs);
            }
        }
    }

    private static void emit(JarFile jar, JarEntry entry) throws Exception {
        ClassNode c = new ClassNode(Opcodes.ASM9);
        try (InputStream in = jar.getInputStream(entry)) { new ClassReader(in).accept(c, 0); }
        List<String> refs = new ArrayList<>(), annotations = new ArrayList<>();
        if (c.superName != null) refs.add(c.superName);
        refs.addAll(c.interfaces);
        signature(c.signature, false, refs);
        boolean client = client(c.visibleAnnotations) || client(c.invisibleAnnotations);
        boolean clientMember = false;
        addAnnotations(c.visibleAnnotations, annotations, refs);
        addAnnotations(c.invisibleAnnotations, annotations, refs);
        if (c.recordComponents != null) for (RecordComponentNode component : c.recordComponents) {
            type(component.descriptor, refs);
            signature(component.signature, true, refs);
            addAnnotations(component.visibleAnnotations, annotations, refs);
            addAnnotations(component.invisibleAnnotations, annotations, refs);
        }
        for (FieldNode f : c.fields) {
            type(f.desc, refs);
            signature(f.signature, true, refs);
            clientMember |= client(f.visibleAnnotations) || client(f.invisibleAnnotations);
            addAnnotations(f.visibleAnnotations, annotations, refs);
            addAnnotations(f.invisibleAnnotations, annotations, refs);
        }
        for (MethodNode m : c.methods) {
            type(m.desc, refs);
            signature(m.signature, false, refs);
            clientMember |= client(m.visibleAnnotations) || client(m.invisibleAnnotations);
            addAnnotations(m.visibleAnnotations, annotations, refs);
            addAnnotations(m.invisibleAnnotations, annotations, refs);
            refs.addAll(m.exceptions);
            m.tryCatchBlocks.stream().map(t -> t.type).filter(t -> t != null).forEach(refs::add);
            for (AbstractInsnNode insn : m.instructions) {
                if (insn instanceof MethodInsnNode call) {
                    member(false, call.owner, call.name, call.desc, annotations, refs);
                } else if (insn instanceof FieldInsnNode field) {
                    member(true, field.owner, field.name, field.desc, annotations, refs);
                } else if (insn instanceof TypeInsnNode t) ref(t.desc, refs);
                else if (insn instanceof MultiANewArrayInsnNode a) type(a.desc, refs);
                else if (insn instanceof InvokeDynamicInsnNode d) {
                    type(d.desc, refs);
                    constant(d.bsm, annotations, refs);
                    for (Object arg : d.bsmArgs) constant(arg, annotations, refs);
                } else if (insn instanceof LdcInsnNode ldc) constant(ldc.cst, annotations, refs);
                else if (insn instanceof FrameNode frame) {
                    if (frame.local != null) for (Object local : frame.local)
                        if (local instanceof String name) ref(name, refs);
                    if (frame.stack != null) for (Object stack : frame.stack)
                        if (stack instanceof String name) ref(name, refs);
                }
            }
        }
        refs = refs.stream().distinct().sorted().toList();
        annotations = annotations.stream().distinct().sorted().toList();
        System.out.print("{\"class\":" + quote(c.name) + ",\"entry\":" + quote(entry.getName())
            + ",\"version\":" + c.version + ",\"client\":" + client + ",\"client_member\":" + clientMember + ",\"refs\":[");
        for (int i=0;i<refs.size();i++) { if(i>0) System.out.print(","); System.out.print(quote(refs.get(i))); }
        System.out.print("],\"annotations\":[");
        for (int i=0;i<annotations.size();i++) { if(i>0) System.out.print(","); System.out.print(quote(annotations.get(i))); }
        System.out.println("]}");
    }

    public static void main(String[] args) throws Exception {
        for (String filename : args) {
            try (JarFile jar = new JarFile(filename)) {
                var entries = jar.stream().filter(e -> !e.isDirectory() && e.getName().endsWith(".class"))
                    .sorted(java.util.Comparator.comparing(JarEntry::getName)).toList();
                for (JarEntry entry : entries) emit(jar, entry);
            }
        }
    }
}
