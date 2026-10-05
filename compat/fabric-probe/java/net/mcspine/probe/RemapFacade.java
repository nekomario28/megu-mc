package net.mcspine.probe;
import java.nio.file.*;
import java.util.Properties;
import org.objectweb.asm.*;
import org.objectweb.asm.commons.*;
/** Remaps only the host facade; never transforms or rewrites the supplied mod JAR. */
public final class RemapFacade {
    public static void main(String[] args) throws Exception {
        var mappings = new Properties();
        try (var input = Files.newInputStream(Path.of(args[0]))) { mappings.load(input); }
        var remapper = new Remapper(Opcodes.ASM9) {
            public String map(String name) { return mappings.getProperty("class." + name, name); }
            public String mapMethodName(String owner, String name, String descriptor) {
                return mappings.getProperty("method." + owner + "." + name + descriptor, name);
            }
        };
        Path source = Path.of(args[1]), output = Path.of(args[2]);
        try (var paths = Files.walk(source)) {
            for (Path path : paths.filter(p -> p.toString().endsWith(".class")).toList()) {
                var reader = new ClassReader(Files.readAllBytes(path));
                var writer = new ClassWriter(0);
                reader.accept(new ClassRemapper(writer, remapper), 0);
                Path destination = output.resolve(remapper.map(reader.getClassName()) + ".class");
                Files.createDirectories(destination.getParent());
                Files.write(destination, writer.toByteArray());
            }
        }
    }
}
