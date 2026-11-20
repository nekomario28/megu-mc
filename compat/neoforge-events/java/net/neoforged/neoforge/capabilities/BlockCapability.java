package net.neoforged.neoforge.capabilities;

import net.mcspine.neoforge.S4CapabilityBridge;
import net.minecraft.core.BlockPos;
import net.minecraft.world.level.Level;
import net.minecraft.world.level.block.entity.BlockEntity;
import net.minecraft.world.level.block.state.BlockState;

public final class BlockCapability<T, C> {
    BlockCapability() { }

    public T getCapability(Level level, BlockPos pos, BlockState state, BlockEntity blockEntity, C context) {
        return S4CapabilityBridge.lookup(this, level, pos, state, blockEntity, context);
    }
}
