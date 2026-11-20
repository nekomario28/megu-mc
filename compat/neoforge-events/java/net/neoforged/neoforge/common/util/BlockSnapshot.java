package net.neoforged.neoforge.common.util;

import net.minecraft.core.BlockPos;
import net.minecraft.world.level.Level;
import net.minecraft.world.level.block.state.BlockState;

/** Input-only facade for the 1.21.1 EntityPlaceEvent constructor. */
public final class BlockSnapshot {
    private final Level level;
    private final BlockPos pos;
    private final BlockState state;
    private final BlockState currentState;

    public BlockSnapshot(Level level, BlockPos pos, BlockState replacedState, BlockState placedState) {
        this.level = level;
        this.pos = pos;
        this.state = replacedState;
        this.currentState = placedState;
    }

    public Level getLevel() { return level; }
    public BlockPos getPos() { return pos; }
    public BlockState getState() { return state; }
    public BlockState getCurrentState() { return currentState; }
}
