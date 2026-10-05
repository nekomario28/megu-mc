package net.mcspine.probe;

import java.nio.file.Path;
import java.util.Collection;
import java.util.List;
import java.util.Set;
import net.fabricmc.loader.impl.game.GameProvider;
import net.fabricmc.loader.impl.game.patch.GameTransformer;
import net.fabricmc.loader.impl.launch.FabricLauncher;
import net.fabricmc.loader.impl.metadata.BuiltinModMetadata;
import net.fabricmc.loader.impl.util.Arguments;

/** Diagnostic game provider. Does not advertise Minecraft or Fabric API compatibility. */
public final class SpineGameProvider implements GameProvider {
    private final Arguments arguments = new Arguments();
    private final GameTransformer transformer = new GameTransformer();
    private Path gameJar;
    private Path directory;

    public String getGameId() { return "mcspine_probe"; }
    public String getGameName() { return "mc-spine loader probe"; }
    public String getRawGameVersion() { return "0.0.0"; }
    public String getNormalizedGameVersion() { return "0.0.0"; }
    public Collection<BuiltinMod> getBuiltinMods() {
        return List.of(new BuiltinMod(List.of(gameJar),
            new BuiltinModMetadata.Builder(getGameId(), getRawGameVersion()).build()));
    }
    public String getEntrypoint() { return "net.mcspine.probe.ProbeMain"; }
    public Path getLaunchDirectory() { return directory; }
    public boolean requiresUrlClassLoader() { return false; }
    public Set<BuiltinTransform> getBuiltinTransforms(String className) { return Set.of(); }
    public boolean isEnabled() { return true; }
    public boolean locateGame(FabricLauncher launcher, String[] args) {
        arguments.parse(args);
        gameJar = Path.of(System.getProperty("mcspine.probe.gameJar"));
        directory = Path.of(System.getProperty("mcspine.probe.gameDir"));
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
