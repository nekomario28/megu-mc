package net.mcspine.probe;

import java.io.InputStream;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.ArrayList;
import java.util.List;
import java.util.Arrays;
import java.util.jar.JarEntry;
import java.util.jar.JarFile;
import org.objectweb.asm.ClassReader;
import org.objectweb.asm.Opcodes;
import org.objectweb.asm.Type;
import org.objectweb.asm.tree.AbstractInsnNode;
import org.objectweb.asm.tree.AnnotationNode;
import org.objectweb.asm.tree.ClassNode;
import org.objectweb.asm.tree.FieldInsnNode;
import org.objectweb.asm.tree.FieldNode;
import org.objectweb.asm.tree.InvokeDynamicInsnNode;
import org.objectweb.asm.tree.LdcInsnNode;
import org.objectweb.asm.tree.MethodInsnNode;
import org.objectweb.asm.tree.MethodNode;
import org.objectweb.asm.tree.MultiANewArrayInsnNode;
import org.objectweb.asm.tree.TypeInsnNode;

/** Emits deterministic JSON class references and annotations for the Python policy layer. */
public final class FabricShimCheck {
    private static String quote(String value) {
        return "\"" + value.replace("\\", "\\\\").replace("\"", "\\\"")
            .replace("\n", "\\n").replace("\r", "\\r") + "\"";
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

    private static void emit(JarFile jar, JarEntry entry) throws Exception {
        ClassNode c = new ClassNode(Opcodes.ASM9);
        try (InputStream in = jar.getInputStream(entry)) { new ClassReader(in).accept(c, 0); }
        List<String> refs = new ArrayList<>(), annotations = new ArrayList<>();
        refs.add(c.superName);
        if (c.interfaces != null) refs.addAll(c.interfaces);
        addAnnotations(c.visibleAnnotations, annotations, refs);
        addAnnotations(c.invisibleAnnotations, annotations, refs);
        for (FieldNode f : c.fields) {
            type(f.desc, refs); addAnnotations(f.visibleAnnotations, annotations, refs);
            addAnnotations(f.invisibleAnnotations, annotations, refs);
        }
        for (MethodNode m : c.methods) {
            type(m.desc, refs); addAnnotations(m.visibleAnnotations, annotations, refs);
            addAnnotations(m.invisibleAnnotations, annotations, refs);
            for (AbstractInsnNode insn : m.instructions) {
                if (insn instanceof MethodInsnNode call) {
                    refs.add(call.owner); type(call.desc, refs);
                    annotations.add("MEMBER|" + call.owner + "|" + call.name + "|" + call.desc);
                } else if (insn instanceof FieldInsnNode field) {
                    refs.add(field.owner); type(field.desc, refs);
                    annotations.add("FIELD|" + field.owner + "|" + field.name + "|" + field.desc);
                } else if (insn instanceof TypeInsnNode t) refs.add(t.desc);
                else if (insn instanceof MultiANewArrayInsnNode a) type(a.desc, refs);
                else if (insn instanceof InvokeDynamicInsnNode d) {
                    type(d.desc, refs); refs.add(d.bsm.getOwner()); type(d.bsm.getDesc(), refs);
                    for (Object arg : d.bsmArgs) if (arg instanceof Type t) type(t.getDescriptor(), refs);
                } else if (insn instanceof LdcInsnNode ldc && ldc.cst instanceof Type t) type(t.getDescriptor(), refs);
            }
        }
        refs.removeIf(v -> v == null);
        refs = refs.stream().distinct().sorted().toList(); annotations = annotations.stream().distinct().sorted().toList();
        boolean client = annotations.stream().anyMatch(a -> a.contains("Lnet/fabricmc/api/Environment;") && a.contains("CLIENT"));
        System.out.print("{\"class\":" + quote(c.name) + ",\"client\":" + client + ",\"refs\":[");
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
