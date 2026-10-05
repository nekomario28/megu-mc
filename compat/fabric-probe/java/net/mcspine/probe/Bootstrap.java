package net.mcspine.probe;

import net.fabricmc.api.EnvType;
import net.fabricmc.loader.impl.launch.knot.Knot;

/** System-loader entry; native methods are registered on the actual target-loader class. */
public final class Bootstrap {
    private static Knot knot;
    private static ClassLoader target;

    public static Class<?> initialize(String gameJar, String gameDirectory, boolean trample) throws ClassNotFoundException {
        if (trample) {
            for (String key : System.getProperties().stringPropertyNames()) {
                if (key.startsWith("fabric.")) throw new IllegalStateException("Unsupported Fabric override in scoped contract: " + key);
            }
        }
        System.setProperty("mcspine.probe.gameJar", gameJar);
        System.setProperty("mcspine.probe.gameDir", gameDirectory);
        System.setProperty("mcspine.probe.trample", Boolean.toString(trample));
        knot = new Knot(EnvType.SERVER);
        target = knot.init(new String[0]);
        return target.loadClass("net.mcspine.probe.Host");
    }
    public static boolean run() throws ReflectiveOperationException {
        var provider = net.fabricmc.loader.impl.FabricLoaderImpl.INSTANCE.getGameProvider();
        provider.launch(target);
        return target.loadClass(provider.getEntrypoint()).getField("changed").getBoolean(null);
    }
}
