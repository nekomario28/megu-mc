package net.minecraft.core.registries;

import net.megu.probe.RegistryStore;
import net.minecraft.core.DefaultedRegistry;

public final class BuiltInRegistries {
    private BuiltInRegistries() {}

    public static final DefaultedRegistry<Object> BLOCK = RegistryStore.BLOCK;
    public static final DefaultedRegistry<Object> ITEM = RegistryStore.ITEM;
}
