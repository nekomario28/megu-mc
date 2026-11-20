package net.minecraft.util.hit;

import net.minecraft.util.math.BlockPos;

public final class BlockHitResult {
    private final BlockPos pos;

    public BlockHitResult(BlockPos pos) { this.pos = pos; }

    public BlockPos getBlockPos() { return pos; }
}
