package net.megu.probe;

import java.nio.file.Path;
import java.util.Collection;
import java.util.List;
import java.util.Set;
import net.fabricmc.loader.impl.game.GameProvider;
import net.fabricmc.loader.impl.game.patch.GameTransformer;
import net.fabricmc.loader.impl.launch.FabricLauncher;
import net.fabricmc.loader.impl.metadata.BuiltinModMetadata;
import net.fabricmc.loader.impl.util.Arguments;

/** Foundation mode has no Minecraft dependency; trample mode exposes only a pinned mod contract. */
public final class SpineGameProvider implements GameProvider {
    private final Arguments arguments = new Arguments();
    private final GameTransformer transformer = new GameTransformer();
    private Path gameJar;
    private Path directory;

    private boolean trample;
    public String getGameId() { return "megu_probe"; }
    public String getGameName() { return "megu loader probe"; }
    public String getRawGameVersion() { return "0.0.0"; }
    public String getNormalizedGameVersion() { return "0.0.0"; }
    public Collection<BuiltinMod> getBuiltinMods() {
        var own = new BuiltinMod(List.of(gameJar), new BuiltinModMetadata.Builder(getGameId(), getRawGameVersion()).build());
        if (!trample) return List.of(own);
        return List.of(own, new BuiltinMod(List.of(gameJar), new BuiltinModMetadata.Builder("minecraft", "1.21.1").build()));
    }
    public String getEntrypoint() { return trample ? "net.megu.probe.TrampleMain" : "net.megu.probe.ProbeMain"; }
    public Path getLaunchDirectory() { return directory; }
    public boolean requiresUrlClassLoader() { return false; }
    public Set<BuiltinTransform> getBuiltinTransforms(String className) { return Set.of(); }
    public boolean isEnabled() { return true; }
    public boolean locateGame(FabricLauncher launcher, String[] args) {
        arguments.parse(args);
        gameJar = Path.of(System.getProperty("megu.probe.gameJar"));
        directory = Path.of(System.getProperty("megu.probe.gameDir"));
        trample = Boolean.getBoolean("megu.probe.trample");
        if (trample) {
            // Minecraft dependency is provided only in this pinned, isolated contract mode.
            try { java.nio.file.Files.createDirectories(directory.resolve("mods")); }
            catch (java.io.IOException exception) { throw new IllegalStateException("Invalid scoped mods directory", exception); }
            try (var mods = java.nio.file.Files.list(directory.resolve("mods"))) {
                for (Path path : mods.toList()) {
                    if (!java.nio.file.Files.isRegularFile(path)) throw new IllegalStateException("Unsupported mod entry");
                    String hash = java.util.HexFormat.of().formatHex(java.security.MessageDigest.getInstance("SHA-256").digest(java.nio.file.Files.readAllBytes(path)));
                    if (!hash.equals("ccd48d96239f485846ced57968e73c6040df4a66ca6ea02cb180a29a5cff7a3f")) throw new IllegalStateException("Unsupported mod hash in scoped Minecraft contract");
                }
            } catch (java.io.IOException | java.security.NoSuchAlgorithmException exception) { throw new IllegalStateException(exception); }
        }
        return true;
    }
    public void initialize(FabricLauncher launcher) {
        transformer.locateEntrypoints(launcher, List.of(gameJar));
        // Keep the probe's classes eligible for Knot's actual Mixin transformation.
        launcher.setValidParentClassPath(launcher.getClassPath().stream()
            .filter(path -> !path.equals(gameJar)).toList());
    }
    public GameTransformer getEntrypointTransformer() { return transformer; }
    public void unlockClassPath(FabricLauncher launcher) { launcher.addToClassPath(gameJar); }
    public void launch(ClassLoader loader) {
        try {
            loader.loadClass(getEntrypoint()).getMethod("run").invoke(null);
        } catch (ReflectiveOperationException exception) {
            throw new IllegalStateException("Probe entrypoint failed", exception);
        }
    }
    public Arguments getArguments() { return arguments; }
    public String[] getLaunchArguments(boolean sanitize) { return arguments.toArray(); }
}
