package net.megu.probe;

import net.fabricmc.api.ModInitializer;
import net.fabricmc.loader.impl.FabricLoaderImpl;

public final class ProbeMain {
    public static boolean changed;
    public static void run() {
        var loader = FabricLoaderImpl.INSTANCE;
        loader.prepareModInit(loader.getGameDir(), null);
        loader.invokeEntrypoints("main", ModInitializer.class, ModInitializer::onInitialize);
        if (Host.getBlock(8, 70, 8) != 1) {
            throw new IllegalStateException("Mod initializer did not write the Rust world");
        }
        changed = new ProbeGame().replaceBlock(2);
    }
}
