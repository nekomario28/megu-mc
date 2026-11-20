package net.minecraft.world.level;

import net.minecraft.resources.ResourceKey;

public abstract class Level implements LevelAccessor {
    public static final ResourceKey<Level> OVERWORLD = null;
    public abstract ResourceKey<Level> dimension();
}
