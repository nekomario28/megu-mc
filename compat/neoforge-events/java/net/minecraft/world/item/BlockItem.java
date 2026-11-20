package net.minecraft.world.item;

import java.util.Objects;
import net.minecraft.world.level.block.Block;

public class BlockItem extends Item {
    private final Block block;

    public BlockItem(Block block, Properties properties) {
        super(properties);
        this.block = Objects.requireNonNull(block);
    }

    public Block getBlock() { return block; }
}
