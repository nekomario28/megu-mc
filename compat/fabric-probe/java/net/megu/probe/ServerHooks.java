package net.megu.probe;
import net.fabricmc.api.ModInitializer;
import net.fabricmc.loader.impl.FabricLoaderImpl;
import net.minecraft.block.*;
import net.minecraft.entity.Entity;
import net.minecraft.util.math.BlockPos;
import net.minecraft.world.World;
/** Normal server's supported, already-eligible farmland conversion. No Java world storage. */
public final class ServerHooks {
    public static void initialize() {
        var loader = FabricLoaderImpl.INSTANCE;
        loader.prepareModInit(loader.getGameDir(), null);
        loader.invokeEntrypoints("main", ModInitializer.class, ModInitializer::onInitialize);
        // Force required Mixins to validate before normal server listeners are opened.
        new FarmlandBlock();
    }
    public static void land(int x, int y, int z, float distance) {
        new FarmlandBlock().onLandedUpon(new World(), new BlockState(),
            new BlockPos(x,y,z), new Entity(), distance);
    }
}
