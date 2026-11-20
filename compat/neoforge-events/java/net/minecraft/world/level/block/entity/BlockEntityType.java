package net.minecraft.world.level.block.entity;

import java.util.Objects;
import net.minecraft.resources.ResourceLocation;

/** Identifies a bridge-backed block entity by the registered S2 block name. */
public final class BlockEntityType<T extends BlockEntity> {
    private final ResourceLocation id;

    public BlockEntityType(ResourceLocation id) {
        this.id = Objects.requireNonNull(id);
    }

    public ResourceLocation id() { return id; }
}
