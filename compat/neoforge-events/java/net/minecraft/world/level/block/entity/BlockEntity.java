package net.minecraft.world.level.block.entity;

import net.minecraft.world.level.Level;

/** Base type for the bridge's activation-scoped block entity views. */
public abstract class BlockEntity {
    private boolean dirty;

    protected BlockEntity() { }

    public abstract Level getLevel();

    public void setChanged() {
        checkActivation();
        dirty = true;
    }

    protected final boolean isDirty() {
        checkActivation();
        return dirty;
    }

    protected void checkActivation() {
        throw new IllegalStateException("Block entity has no active bridge view");
    }
}
