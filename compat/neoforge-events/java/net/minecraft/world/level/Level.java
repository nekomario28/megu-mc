package net.minecraft.world.level;

import java.util.HashMap;
import java.util.Map;
import net.mcspine.neoforge.MutationBuffer;
import net.minecraft.core.BlockPos;
import net.minecraft.world.level.block.entity.BlockEntity;
import net.minecraft.world.level.block.state.BlockState;
import net.neoforged.neoforge.capabilities.BlockCapability;

/** Immutable input snapshot for mods. Writes are conditional host mutations. */
public class Level implements LevelAccessor {
    private final int dim;
    private final Map<BlockPos, BlockState> blocks = new HashMap<>();

    public Level() { this(0); }
    public Level(int dimension) { this.dim = dimension; }

    public BlockState getBlockState(BlockPos pos) {
        return blocks.getOrDefault(pos, new BlockState());
    }

    /** Populate an input snapshot from the Rust owner before dispatch. */
    public void snapshotBlock(BlockPos pos, BlockState state) { blocks.put(pos, state); }

    public <T, C> T getCapability(BlockCapability<T, C> capability, BlockPos pos, C context) {
        return getCapability(capability, pos, null, null, context);
    }

    public <T, C> T getCapability(BlockCapability<T, C> capability, BlockPos pos, BlockState state,
            BlockEntity blockEntity, C context) {
        return capability.getCapability(this, pos, state, blockEntity, context);
    }

    public boolean setBlock(BlockPos pos, BlockState state, int flags) {
        MutationBuffer.append(new MutationBuffer.SetBlockIf(dim, pos.x(), pos.y(), pos.z(),
            state.stateId(), getBlockState(pos).stateId()));
        return true;
    }
}
