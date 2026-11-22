package net.minecraft.world.level;

import java.util.HashMap;
import java.util.List;
import java.util.Map;
import net.megu.neoforge.DirectBatchBridge;
import net.megu.neoforge.MutationBuffer;
import net.minecraft.core.BlockPos;
import net.minecraft.core.registries.Registries;
import net.minecraft.resources.ResourceKey;
import net.minecraft.resources.ResourceLocation;
import net.minecraft.world.level.block.Block;
import net.minecraft.world.level.block.entity.BlockEntity;
import net.minecraft.world.level.block.state.BlockState;
import net.neoforged.neoforge.capabilities.BlockCapability;

/** Immutable input snapshot for mods. Writes are conditional host mutations. */
public class Level implements LevelAccessor {
    public static final ResourceKey<Level> OVERWORLD = dimensionKey("overworld");
    public static final ResourceKey<Level> NETHER = dimensionKey("the_nether");
    public static final ResourceKey<Level> END = dimensionKey("the_end");
    // Rust DimId slot order, independent of the protocol's dimension-type IDs.
    private static final List<ResourceKey<Level>> DIMENSIONS = List.of(OVERWORLD, NETHER, END);

    private final int dim;
    private final DirectBatchBridge.TickContext tickContext;
    private final Map<BlockPos, BlockState> blocks = new HashMap<>();

    public Level() { this(0); }
    public Level(int dimension) { this(dimension, null); }

    public Level(int dimension, DirectBatchBridge.TickContext tickContext) {
        this.dim = dimension;
        this.tickContext = tickContext;
    }

    private static ResourceKey<Level> dimensionKey(String path) {
        return ResourceKey.create(Registries.DIMENSION, ResourceLocation.withDefaultNamespace(path));
    }

    public ResourceKey<Level> dimension() {
        if (dim < 0 || dim >= DIMENSIONS.size()) throw new IllegalStateException("Level has no supported host dimension");
        return DIMENSIONS.get(dim);
    }

    public static boolean supportsDimension(ResourceLocation location) {
        return DIMENSIONS.stream().anyMatch(key -> key.location().equals(location));
    }

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

    @Override public void scheduleTick(BlockPos pos, Block block, int delay) {
        DirectBatchBridge.scheduleBlockTick(tickContext, this, pos, block, delay);
    }
}
