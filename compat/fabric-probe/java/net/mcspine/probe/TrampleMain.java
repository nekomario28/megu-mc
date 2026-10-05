package net.mcspine.probe;
import net.fabricmc.api.ModInitializer;
import net.fabricmc.loader.impl.FabricLoaderImpl;
import net.minecraft.block.*;
import net.minecraft.entity.Entity;
import net.minecraft.util.math.BlockPos;
import net.minecraft.world.World;
/** Runs a controlled boundary operation against Rust-owned storage. */
public final class TrampleMain {
    public static boolean changed;
    public static void run() {
        var loader = FabricLoaderImpl.INSTANCE;
        loader.prepareModInit(loader.getGameDir(), null);
        loader.invokeEntrypoints("main", ModInitializer.class, ModInitializer::onInitialize);
        if (Host.getBlock(8, 70, 8) != 4286) { throw new IllegalStateException("Native farmland initialization failed"); }
        new FarmlandBlock().onLandedUpon(new World(), new BlockState(), new BlockPos(8,70,8), new Entity(), 10);
        changed = Host.getBlock(8,70,8) != 4286;
    }
}
