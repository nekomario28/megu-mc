package net.mcspine.neoforge;
import net.neoforged.neoforge.common.NeoForge;
import net.neoforged.neoforge.event.level.BlockEvent.FarmlandTrampleEvent;
import net.minecraft.world.level.Level;
import net.minecraft.world.level.block.state.BlockState;
import net.minecraft.world.entity.Entity;
import net.minecraft.core.BlockPos;
import net.mcspine.probe.Host;
/** Only this original mod's cancellation contract; arguments are opaque API shapes. */
public final class ServerHooks {
    public static void land(int x, int y, int z, float distance) {
        var event = new FarmlandTrampleEvent(new Level(), new BlockPos(x,y,z),
            new BlockState(), distance, new Entity());
        NeoForge.EVENT_BUS.post(event);
        if (!event.isCanceled()) Host.setBlock(x,y,z,10);
    }
}
