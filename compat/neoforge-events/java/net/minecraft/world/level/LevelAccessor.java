package net.minecraft.world.level;

import net.minecraft.core.BlockPos;
import net.minecraft.world.level.block.Block;

public interface LevelAccessor {
    void scheduleTick(BlockPos pos, Block block, int delay);
}
