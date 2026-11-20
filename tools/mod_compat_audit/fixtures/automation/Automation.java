package auditfixture.automation;

import net.minecraft.core.BlockPos;
import net.minecraft.core.Direction;
import net.minecraft.world.Container;
import net.minecraft.world.level.Level;
import net.minecraft.world.level.LevelAccessor;
import net.minecraft.world.level.block.Block;
import net.minecraft.world.level.block.entity.BlockEntity;
import net.minecraft.world.level.block.entity.BlockEntityTicker;
import net.minecraft.world.level.block.entity.BlockEntityType;
import net.minecraft.world.level.block.state.BlockState;
import net.neoforged.fml.common.Mod;
import net.neoforged.neoforge.capabilities.Capabilities;
import net.neoforged.neoforge.capabilities.RegisterCapabilitiesEvent;
import net.neoforged.neoforge.items.IItemHandler;

@Mod("audit_automation")
public final class Automation {
    public static BlockEntityTicker<BlockEntity> ticker(LevelAccessor scheduler, Block block) {
        return (world, pos, state, entity) -> {
            scheduler.scheduleTick(pos, block, 2);
            entity.setChanged();
        };
    }

    public static void tick(BlockEntityTicker<BlockEntity> ticker, Level level, BlockPos pos,
                            BlockState state, BlockEntity entity) {
        ticker.tick(level, pos, state, entity);
    }

    public static void register(RegisterCapabilitiesEvent event, BlockEntityType<BlockEntity> type,
                                IItemHandler handler) {
        event.registerBlockEntity(Capabilities.ItemHandler.BLOCK, type, (entity, side) -> handler);
    }

    public static void transfer(Level world, BlockPos pos, BlockState state, BlockEntity entity,
                                Container source, Direction face, boolean simulate) {
        var handler = Capabilities.ItemHandler.BLOCK.getCapability(world, pos, state, entity, face);
        if (handler != null && handler.getSlots() > 0) {
            var remainder = handler.insertItem(0, source.getItem(0), simulate);
            if (!simulate) {
                source.setItem(0, remainder);
            }
        }
    }
}
