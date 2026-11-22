package net.minecraft.registry;

import net.megu.probe.RegistryStore;

public final class Registries {
    private Registries() {}

    public static final DefaultedRegistry<Object> BLOCK = RegistryStore.BLOCK;
    public static final DefaultedRegistry<Object> ITEM = RegistryStore.ITEM;
}
