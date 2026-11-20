package net.minecraft.world.level.block.entity;

import net.minecraft.world.level.Level;

/** Base type for the bridge's activation-scoped block entity views. */
public abstract class BlockEntity {
    protected BlockEntity() { }

    public abstract Level getLevel();
}
