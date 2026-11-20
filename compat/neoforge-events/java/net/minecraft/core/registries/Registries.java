package net.minecraft.core.registries;

import net.minecraft.core.Registry;
import net.minecraft.resources.ResourceKey;
import net.minecraft.resources.ResourceLocation;
import net.minecraft.world.item.Item;
import net.minecraft.world.level.Level;
import net.minecraft.world.level.block.Block;

public final class Registries {
    public static final ResourceKey<Registry<Block>> BLOCK =
        ResourceKey.createRegistryKey(ResourceLocation.withDefaultNamespace("block"));
    public static final ResourceKey<Registry<Item>> ITEM =
        ResourceKey.createRegistryKey(ResourceLocation.withDefaultNamespace("item"));
    public static final ResourceKey<Registry<Level>> DIMENSION =
        ResourceKey.createRegistryKey(ResourceLocation.withDefaultNamespace("dimension"));

    private Registries() { }
}
