package net.mcspine.neoforge;
import java.nio.file.*;
import java.net.URLClassLoader;
import net.neoforged.fml.common.Mod;
import net.neoforged.neoforge.common.NeoForge;
/** Pinned event adapter discovery, not the full FML loader/bootstrap. */
public final class Bootstrap {
    static Object instance;
    private static URLClassLoader modLoader;
    public static Class<?> initialize(String directory) throws Exception {
        Path mods = Path.of(directory).resolve("mods");
        Files.createDirectories(mods);
        try (var paths = Files.list(mods)) {
            var entries = paths.toList();
            if (entries.size() > 1) throw new IllegalStateException("Only one pinned NeoForge mod is supported");
            for (Path path : entries) {
                if (!Files.isRegularFile(path)) throw new IllegalStateException("Unsupported NeoForge mod entry");
                String hash = java.util.HexFormat.of().formatHex(java.security.MessageDigest.getInstance("SHA-256").digest(Files.readAllBytes(path)));
                if (!hash.equals("90d3318ce049d2e684b08d12ab6e35410a9d8e159afc43757102ab127c028acf"))
                    throw new IllegalStateException("Unsupported NeoForge mod hash in scoped event contract");
                modLoader = new URLClassLoader(new java.net.URL[]{path.toUri().toURL()}, Bootstrap.class.getClassLoader());
                Class<?> mod = modLoader.loadClass("com.seedprotect.SeedProtectMod");
                Mod annotation = mod.getAnnotation(Mod.class);
                if (annotation == null || !annotation.value().equals("seedprotect")) throw new IllegalStateException("Pinned NeoForge Mod annotation mismatch");
                instance = mod.getConstructor().newInstance();
            }
        }
        // The actual NeoForge class creates a shutdown game bus. Registration precedes start.
        NeoForge.EVENT_BUS.start();
        return net.mcspine.probe.Host.class;
    }
    public static Class<?> initializeServer() { return ServerHooks.class; }
}
