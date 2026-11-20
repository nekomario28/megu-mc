package net.minecraft.network;

import net.minecraft.core.Registry;
import net.minecraft.resources.ResourceKey;
import net.minecraft.resources.ResourceLocation;

public class FriendlyByteBuf {
    public native FriendlyByteBuf writeResourceLocation(ResourceLocation id);
    public native <T> ResourceKey<T> readResourceKey(ResourceKey<? extends Registry<T>> registry);
}
